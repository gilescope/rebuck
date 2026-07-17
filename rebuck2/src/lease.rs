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
    /// death we can only infer from silence. Also `None` once `done` is set —
    /// a result is not a lease and cannot expire out from under a claimant.
    expires: Option<Instant>,
    waiters: Vec<oneshot::Sender<Outcome>>,
    /// The key's canonical result, once a leader has published one.
    ///
    /// Set => this entry is no longer a lease but an ANSWER, and every later
    /// claimant adopts it instead of building. Only a SUCCESS becomes canonical:
    /// a failure leaves the key open, or one machine's transient OOM would be
    /// cached for the whole grid.
    ///
    /// This is bounded and cheap: what is retained is the published descriptor
    /// chain (a few hundred bytes), never the layer. It is the same
    /// one-answer-per-key table a single buildkitd keeps for a single build.
    done: Option<Vec<u8>>,
}

/// The result of asking for a key.
pub enum Claim {
    /// You own it. Build it, then call [`Leases::release`]. If you are a peer,
    /// heartbeat every [`HEARTBEAT`] until you do.
    Leader,
    /// A peer owns it. Await this for their result.
    Follower(oneshot::Receiver<Outcome>),
    /// Already built, and here it is. Adopt it — do NOT build your own.
    ///
    /// The key's canonical answer: whoever got here first. Building it again
    /// would produce a SECOND answer (an `apt-get update` alone guarantees the
    /// bytes differ), leaving the grid with two results for one key and an
    /// artifact stitched from both. First writer wins.
    Done(Vec<u8>),
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
    /// Unique ids for out-of-process (HTTP) holders — see [`Leases::claim_http`].
    next_http: std::sync::atomic::AtomicU64,
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
            next_http: std::sync::atomic::AtomicU64::new(0),
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
                // Already answered. Adopt it — first writer wins. Checked BEFORE
                // expiry: a result never expires, and re-leading a key that has
                // an answer is precisely how the grid ends up with two.
                if let Some(bytes) = &e.done {
                    self.merged
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Claim::Done(bytes.clone());
                }
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
                        done: None,
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
        let waiters = match &outcome {
            // A success is the key's ANSWER. Keep it: the next claimant adopts it
            // rather than building a second, different one (principle 3).
            Outcome::Done(bytes) => {
                let e = map.get_mut(key).expect("checked above");
                e.done = Some(bytes.clone());
                e.expires = None; // a result is not a lease; it cannot expire
                e.holder = Holder::Local; // nobody holds it now; it is just an answer
                std::mem::take(&mut e.waiters)
            }
            // A failure answers nothing. Drop the entry so the next claimant
            // leads and builds it — otherwise one machine's transient OOM is
            // cached for the whole fleet.
            _ => map.remove(key).expect("checked above").waiters,
        };
        drop(map);
        for w in waiters {
            let _ = w.send(outcome.clone());
        }
    }

    /// Claim on behalf of an out-of-process client (a buildkitd over HTTP).
    ///
    /// NOT `claim_local`: a local holder is drop-guarded, which is only safe when
    /// the holder is a future in THIS process. An HTTP claim returns immediately
    /// and nothing holds it — so a client that crashes mid-build would wedge the
    /// key FOREVER and block its followers forever. That is the exact hang this
    /// module exists to prevent.
    ///
    /// So an HTTP holder is a peer like any other: it gets a TTL and a unique
    /// id. It must heartbeat to keep the lease, and echo the id to release it —
    /// otherwise a zombie could publish over its successor.
    pub fn claim_http(&self, key: &str) -> (Claim, String) {
        let id = format!(
            "http-{}",
            self.next_http
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        (self.claim(key, Holder::Peer(id.clone())), id)
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

    /// Give up a peer's claim without a result: it failed, or was cancelled. The
    /// followers are freed to rebuild rather than waiting out the TTL for a
    /// result that is never coming.
    pub fn abandon_peer(&self, key: &str, endpoint: &str) {
        let mut map = self.inner.lock().unwrap();
        if map
            .get(key)
            .is_some_and(|e| e.holder == Holder::Peer(endpoint.to_string()))
        {
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

    /// Leases actually HELD — entries someone is building right now.
    ///
    /// Not the same as [`len`], and the difference is the point: an entry that
    /// has published its result is no longer a lease but the key's canonical
    /// answer, retained so a late claimant adopts it instead of building a
    /// second one. A leak is a lease nobody will ever release; a retained
    /// answer is the feature.
    #[allow(dead_code)] // assertions
    pub fn held(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.done.is_none())
            .count()
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
    }

    /// Principle 3: ONE canonical result per key, first writer wins.
    ///
    /// A claimant that turns up after the leader has already released must NOT
    /// be handed a fresh lease and told to build. It built the same key, so it
    /// would produce a SECOND, different result -- `apt-get update` alone
    /// guarantees the bytes differ -- and the grid would hold two answers to one
    /// question. Half the artifact built against one, half the other: exactly
    /// what the grid exists to prevent.
    ///
    /// Measured on +examples-1 before this: 14 of 14 keys agreed and only 7
    /// merged. The other 7 were BOTH leaders -- they never overlapped in time,
    /// so both built, and both kept their own bytes.
    #[tokio::test]
    async fn a_late_claimant_adopts_the_canonical_result_rather_than_rebuilding() {
        let l = Leases::default();
        assert!(leader(l.claim_local("k")));
        l.release("k", None, Outcome::Done(b"the-one-true-layer".to_vec()));

        // The leader is long gone. This claimant is not racing anyone.
        match l.claim_local("k") {
            Claim::Done(bytes) => assert_eq!(bytes, b"the-one-true-layer".to_vec()),
            Claim::Leader => {
                panic!("a late claimant must ADOPT the canonical result, not build a second one")
            }
            Claim::Follower(_) => panic!("nobody is building it; there is nothing to wait for"),
        }
    }

    /// A FAILED build must not become canonical. The key has no answer, so the
    /// next claimant has to build it -- otherwise one machine's transient
    /// failure is cached for the whole grid.
    #[tokio::test]
    async fn a_failure_is_not_canonical() {
        let l = Leases::default();
        assert!(leader(l.claim_local("k")));
        l.release("k", None, Outcome::Failed("oom".into()));
        assert!(
            leader(l.claim_local("k")),
            "a failure must leave the key open for the next claimant to build"
        );
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
