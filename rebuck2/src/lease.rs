//! Cross-machine single-flight: never build the same thing twice at once.
//!
//! A single buildkitd already does this inside itself — its solver edge-merges
//! concurrent identical vertices — and that is precisely why an Earthly
//! Satellite felt good: many builds hitting *one* daemon get build-once for
//! free. The moment you have N ephemeral daemons (a CI matrix) each has its own
//! solver and the property is lost. This module restores it across the mesh.
//!
//! The design is BuildBuddy's "action merging", which Bazel RE has run in
//! production since 2022, and which the container-build world does not have:
//! Depot, Dagger and Nix all reuse cache *after the fact* and let two concurrent
//! builds of the same thing both run.
//!
//! ```text
//!   claim(key) ── vacant ──▶ Leader   build it, heartbeat, then release()
//!              └─ held ────▶ Follower await the leader's result
//! ```
//!
//! **The failure that matters is not duplicate work, it is a hang.** A holder
//! can die at any moment — a runner is evicted, a process is OOM-killed, a
//! client disconnects — and its followers must never wait forever for a result
//! nobody is computing. So:
//!
//! - A remote holder carries a **deadline**. It heartbeats; if it stops, the
//!   lease expires, waiters are told to [`Outcome::Retry`], and one of them
//!   becomes the new leader. Losing the lease costs a rebuild; losing the
//!   waiters costs the build.
//! - A local holder needs no deadline: it is a future in this process, and
//!   [`LeaseGuard`]'s `Drop` reaps it even on cancellation or panic.
//!
//! Deliberately absent (v1): hedged execution — duplicating a straggling leader
//! and taking whichever finishes first. It trades the very work this module
//! exists to save, so it should be driven by measurements rather than instinct.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// How long a remote holder's claim survives without a heartbeat.
///
/// This is the DETECTION LATENCY for a hard death. A leader killed with -9 (an
/// evicted runner, an OOM kill) sends no QUIC close frame, so the driver cannot
/// learn of it by disconnect — only by silence. Its followers therefore wait up
/// to this long before re-electing. Bounded, never a hang, but not free.
///
/// Generous by default: evicting a LIVE leader early is worse than waiting a
/// little longer on a dead one, because it duplicates the very work this module
/// exists to save. Tunable (`--lease-ttl-secs`) for tests and for fleets whose
/// jobs are all short.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(90);

/// Heartbeat interval a remote holder should use. Comfortably inside
/// [`DEFAULT_LEASE_TTL`] so a single dropped packet does not evict a working leader.
#[allow(dead_code)] // the cadence for worker::heartbeat, whose caller is the buildkit fork
pub const HEARTBEAT: Duration = Duration::from_secs(20);

/// How often the reaper looks for silent holders. Must be well under the TTL,
/// or the TTL is not the detection latency — the sum is.
pub const REAP_EVERY: Duration = Duration::from_secs(5);

/// What a follower is eventually told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The leader finished. These are the payload's encoded result bytes.
    Done(Vec<u8>),
    /// The leader failed. Every follower gets the same failure: it is the same
    /// job, so it is the same answer.
    Failed(String),
    /// The leader vanished (died, or was cancelled) without publishing. Claim
    /// again — one of the waiters will become the new leader.
    Retry,
}

/// Who is holding a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Holder {
    /// A task in this process. Drop-guarded, so it needs no deadline.
    Local,
    /// A worker across the mesh, identified by endpoint. Can die silently, so
    /// it must heartbeat.
    Peer(String),
}

struct Entry {
    holder: Holder,
    /// `None` for a local holder: its guard reaps it. `Some` for a peer, whose
    /// death we can only infer from silence.
    expires: Option<Instant>,
    waiters: Vec<oneshot::Sender<Outcome>>,
}

/// The result of asking for a key.
pub enum Claim {
    /// You own it. Build it, then call [`Leases::release`]. If you are a peer,
    /// heartbeat every [`HEARTBEAT`] until you do.
    Leader,
    /// A peer owns it. Await this for their result.
    Follower(oneshot::Receiver<Outcome>),
}

/// The lease table. One per driver — it is the fleet's single coordinator, so
/// there is no consensus problem to solve, only a liveness one.
pub struct Leases {
    inner: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    /// Followers that attached rather than rebuilding. The whole point, counted.
    pub merged: std::sync::atomic::AtomicU64,
    /// Leaders elected — i.e. things that actually got built.
    pub led: std::sync::atomic::AtomicU64,
    /// Leaders that gave up (failed, cancelled, died). Their followers rebuilt,
    /// so this is the tax the mechanism levies when it goes wrong.
    pub abandoned: std::sync::atomic::AtomicU64,
}

impl Default for Leases {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_LEASE_TTL)
    }
}

impl Leases {
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
            merged: std::sync::atomic::AtomicU64::new(0),
            led: std::sync::atomic::AtomicU64::new(0),
            abandoned: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Claim `key` for a task in this process.
    pub fn claim_local(&self, key: &str) -> Claim {
        self.claim(key, Holder::Local)
    }

    /// Claim `key` on behalf of a worker across the mesh.
    pub fn claim_peer(&self, key: &str, endpoint: &str) -> Claim {
        self.claim(key, Holder::Peer(endpoint.to_string()))
    }

    fn claim(&self, key: &str, holder: Holder) -> Claim {
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(key) {
            Some(e) => {
                // A peer's lease that has gone quiet is not a lease. Take it —
                // the old holder's late Release is ignored (see `release`).
                if e.expires.is_some_and(|t| t <= Instant::now()) {
                    for w in std::mem::take(&mut e.waiters) {
                        let _ = w.send(Outcome::Retry);
                    }
                    e.holder = holder.clone();
                    e.expires = self.deadline(&holder);
                    self.led.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Claim::Leader;
                }
                let (tx, rx) = oneshot::channel();
                e.waiters.push(tx);
                self.merged
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Claim::Follower(rx)
            }
            None => {
                let expires = self.deadline(&holder);
                map.insert(
                    key.to_string(),
                    Entry {
                        expires,
                        holder,
                        waiters: Vec::new(),
                    },
                );
                self.led.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Claim::Leader
            }
        }
    }

    /// Extend a peer's claim. Ignored if the peer no longer holds the key —
    /// a heartbeat from a leader we already evicted must NOT resurrect it, or
    /// two leaders end up believing they own the same job.
    pub fn heartbeat(&self, key: &str, endpoint: &str) -> bool {
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(key) {
            Some(e) if e.holder == Holder::Peer(endpoint.to_string()) => {
                e.expires = Some(Instant::now() + self.ttl);
                true
            }
            _ => false,
        }
    }

    /// Publish a result and wake every follower.
    ///
    /// `by` must still hold the key. A leader that was evicted for silence and
    /// then finally finishes must not publish: its followers have already been
    /// told to retry, and one of them owns the key now. Its result would be
    /// correct but it is no longer the leader, and honouring it would let two
    /// writers race the same entry.
    pub fn release(&self, key: &str, by: Option<&str>, outcome: Outcome) {
        let mut map = self.inner.lock().unwrap();
        let Some(e) = map.get(key) else { return };
        let mine = match (by, &e.holder) {
            (None, Holder::Local) => true,
            (Some(ep), Holder::Peer(h)) => h == ep,
            _ => false,
        };
        if !mine {
            return;
        }
        let e = map.remove(key).expect("checked above");
        for w in e.waiters {
            let _ = w.send(outcome.clone());
        }
    }

    /// Whether a local claim is still ours. Local holders are drop-guarded and
    /// so need no heartbeat to stay alive; this exists so an HTTP caller (which
    /// has no Drop) can discover it was torn down rather than block forever.
    pub fn heartbeat_local(&self, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|e| e.holder == Holder::Local)
    }

    /// Drop a local claim without a result — the leader was cancelled. Waiters
    /// are told to retry so one of them takes over, rather than waiting on a
    /// result that will never come.
    pub fn abandon_local(&self, key: &str) {
        let mut map = self.inner.lock().unwrap();
        if map.get(key).is_some_and(|e| e.holder == Holder::Local) {
            self.abandoned
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let e = map.remove(key).expect("checked above");
            for w in e.waiters {
                let _ = w.send(Outcome::Retry);
            }
        }
    }

    /// Evict every peer lease held by `endpoint` — called when a worker
    /// disconnects. A dead worker is not coming back to heartbeat, and its
    /// followers should not wait out the full TTL to discover that.
    pub fn evict_peer(&self, endpoint: &str) -> usize {
        let mut map = self.inner.lock().unwrap();
        let dead: Vec<String> = map
            .iter()
            .filter(|(_, e)| e.holder == Holder::Peer(endpoint.to_string()))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &dead {
            if let Some(e) = map.remove(k) {
                for w in e.waiters {
                    let _ = w.send(Outcome::Retry);
                }
            }
        }
        dead.len()
    }

    /// Expire leases whose holder has gone quiet. Called on a timer: a holder
    /// that dies without a clean disconnect is only detectable by silence.
    pub fn reap(&self) -> usize {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();
        let stale: Vec<String> = map
            .iter()
            .filter(|(_, e)| e.expires.is_some_and(|t| t <= now))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            if let Some(e) = map.remove(k) {
                for w in e.waiters {
                    let _ = w.send(Outcome::Retry);
                }
            }
        }
        stale.len()
    }

    #[allow(dead_code)] // assertions; the table is otherwise self-managing
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn deadline(&self, h: &Holder) -> Option<Instant> {
        match h {
            // A local holder is drop-guarded: its death is observable, so it
            // needs no deadline.
            Holder::Local => None,
            Holder::Peer(_) => Some(Instant::now() + self.ttl),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leader(c: Claim) -> bool {
        matches!(c, Claim::Leader)
    }

    /// The point of the whole module: the first claimant builds, the rest wait.
    #[tokio::test]
    async fn first_claimant_leads_and_the_rest_follow() {
        let l = Leases::default();
        assert!(leader(l.claim_local("k")));

        let Claim::Follower(rx) = l.claim_local("k") else {
            panic!("second claim must follow, not lead");
        };
        l.release("k", None, Outcome::Done(b"built".to_vec()));
        assert_eq!(rx.await.unwrap(), Outcome::Done(b"built".to_vec()));
        assert!(l.is_empty(), "a released lease must not linger");
    }

    /// A failure is the same failure for every waiter — it is the same job.
    #[tokio::test]
    async fn a_leaders_failure_reaches_every_follower() {
        let l = Leases::default();
        assert!(leader(l.claim_local("k")));
        let (Claim::Follower(a), Claim::Follower(b)) = (l.claim_local("k"), l.claim_local("k"))
        else {
            panic!("expected followers");
        };
        l.release("k", None, Outcome::Failed("boom".into()));
        assert_eq!(a.await.unwrap(), Outcome::Failed("boom".into()));
        assert_eq!(b.await.unwrap(), Outcome::Failed("boom".into()));
    }

    /// THE failure mode that matters. A holder that dies must not strand its
    /// followers forever — they are told to retry, and one takes over.
    #[tokio::test]
    async fn a_dead_peer_frees_its_followers_rather_than_hanging_them() {
        let l = Leases::default();
        assert!(leader(l.claim_peer("k", "worker-a")));
        let Claim::Follower(rx) = l.claim_peer("k", "worker-b") else {
            panic!("expected a follower");
        };

        assert_eq!(l.evict_peer("worker-a"), 1, "the dead worker's lease");
        assert_eq!(rx.await.unwrap(), Outcome::Retry);
        // And the key is claimable again — someone must actually build it.
        assert!(leader(l.claim_peer("k", "worker-b")));
    }

    /// A silent holder is evicted by the reaper: a worker can die without ever
    /// sending a disconnect, and silence is the only signal we get.
    #[tokio::test]
    async fn a_silent_peer_is_reaped_and_its_waiters_retry() {
        let l = Leases::default();
        assert!(leader(l.claim_peer("k", "gone")));
        let Claim::Follower(rx) = l.claim_peer("k", "waiting") else {
            panic!("expected a follower");
        };

        // Force the deadline into the past — the wall-clock alternative is a
        // 90-second test.
        l.inner.lock().unwrap().get_mut("k").unwrap().expires =
            Some(Instant::now() - Duration::from_secs(1));

        assert_eq!(l.reap(), 1);
        assert_eq!(rx.await.unwrap(), Outcome::Retry);
        assert!(l.is_empty());
    }

    /// Heartbeating keeps a working leader alive. Without this, any job longer
    /// than the TTL would be perpetually re-elected and never finish.
    #[test]
    fn a_heartbeat_keeps_a_working_leader_alive() {
        let l = Leases::default();
        assert!(leader(l.claim_peer("k", "worker-a")));
        l.inner.lock().unwrap().get_mut("k").unwrap().expires =
            Some(Instant::now() - Duration::from_secs(1));

        assert!(l.heartbeat("k", "worker-a"), "the holder may extend");
        assert_eq!(l.reap(), 0, "a heartbeat must save it from the reaper");
    }

    /// A heartbeat from a leader we already evicted must NOT resurrect its
    /// claim — otherwise a zombie and its successor both believe they own the
    /// job, and both write the result.
    #[test]
    fn a_zombie_cannot_heartbeat_its_way_back() {
        let l = Leases::default();
        assert!(leader(l.claim_peer("k", "zombie")));
        assert_eq!(l.evict_peer("zombie"), 1);
        assert!(leader(l.claim_peer("k", "successor")));

        assert!(
            !l.heartbeat("k", "zombie"),
            "the evicted holder must not extend a lease it no longer owns"
        );
        assert!(l.heartbeat("k", "successor"), "the real holder still may");
    }

    /// Likewise a zombie's late result: correct, perhaps, but it is no longer
    /// the leader, and honouring it would let two writers race one entry.
    #[tokio::test]
    async fn a_zombies_late_result_is_ignored() {
        let l = Leases::default();
        assert!(leader(l.claim_peer("k", "zombie")));
        assert_eq!(l.evict_peer("zombie"), 1);
        assert!(leader(l.claim_peer("k", "successor")));
        let Claim::Follower(rx) = l.claim_peer("k", "waiter") else {
            panic!("expected a follower");
        };

        l.release("k", Some("zombie"), Outcome::Done(b"stale".to_vec()));
        l.release("k", Some("successor"), Outcome::Done(b"fresh".to_vec()));

        assert_eq!(
            rx.await.unwrap(),
            Outcome::Done(b"fresh".to_vec()),
            "the follower must get the CURRENT leader's result"
        );
    }

    /// An expired lease is taken over by the next claimant directly, without
    /// waiting for the reaper to run — the reaper is a backstop, not the path.
    #[tokio::test]
    async fn claiming_an_expired_lease_takes_it_over() {
        let l = Leases::default();
        assert!(leader(l.claim_peer("k", "dead")));
        let Claim::Follower(rx) = l.claim_peer("k", "waiter") else {
            panic!("expected a follower");
        };
        l.inner.lock().unwrap().get_mut("k").unwrap().expires =
            Some(Instant::now() - Duration::from_secs(1));

        assert!(
            leader(l.claim_peer("k", "newcomer")),
            "an expired lease is up for grabs"
        );
        assert_eq!(rx.await.unwrap(), Outcome::Retry, "old waiters are freed");
    }

    /// Distinct keys never interfere — the coalescing is keyed on the job, not
    /// on "something is already running".
    #[test]
    fn distinct_keys_are_independent() {
        let l = Leases::default();
        assert!(leader(l.claim_local("a")));
        assert!(leader(l.claim_local("b")));
        assert_eq!(l.len(), 2);
    }
}
