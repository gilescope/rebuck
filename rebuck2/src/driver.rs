//! Driver: owns the job table, accepts workers over iroh, dispatches actions,
//! and serves worker blob requests from the local store.
//!
//! Scheduling v0: least-inflight worker wins; if no worker is connected (and
//! none is required) the action runs locally in-process — which makes a
//! driver with no workers behave like rebuck-with-execution, a strict
//! superset of cache-only mode.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use iroh::endpoint::Connection;
use iroh::Endpoint;
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};

use crate::lease;
use crate::mesh::{self, BlobReq, BlobResp, Dig, D2W, W2D};
use crate::payload::Payload;
use crate::store::Store;

pub struct DriverCfg {
    pub session: String,
    pub min_workers: usize,
    pub local_exec: bool,
    /// Outputs stay on producing workers; the driver keeps digest -> worker
    /// index and redirects fetches. Trades worker-loss resilience for
    /// driver disk/egress. See docs/re-engine-plan.md roadmap #7.
    pub decentralized: bool,
    /// Hardlink inputs from the store in local-fallback execution.
    pub hardlinks: bool,
    /// Cache non-zero exit codes in the AC too. A compile failure is as
    /// deterministic as a success for a given action digest — any toolchain
    /// or source change makes a new digest and retries for real. Kills the
    /// re-run-every-known-failure tax on warm sweeps. Infra failures are
    /// never cached regardless.
    pub cache_failures: bool,
    /// When this file appears, assign snapshot shards to the fleet
    /// (Finalize), await their Finalized replies, then write
    /// `<file>.done` for the workflow to proceed on.
    pub finalize_file: Option<std::path::PathBuf>,
    /// Locality-aware dispatch: prefer the worker whose bloom already
    /// claims a job's heaviest inputs - move the task to the data, not
    /// GiBs of rlibs to the task. Soft preference with a short patience
    /// window (delay scheduling); blooms only lie in the safe direction
    /// (a false positive costs the fetch we'd have done anyway).
    pub locality: bool,
    /// Eagerly pull the fleet's small (<256KB) metadata blobs into the
    /// driver store once the pool forms, so buck2's client downloads are
    /// driver-LOCAL (immune to worker mesh-latency variance - the linux-36m
    /// slow-leg root cause) instead of relayed per-blob at build time.
    pub prefetch_metadata: bool,
    /// Write the driver's full EndpointAddr (id + relay) here once bound.
    /// CI publishes it as a run artifact so workers can dial directly -
    /// n0 discovery becomes a fallback instead of a single point of
    /// failure (observed: regional discovery outages stranding workers
    /// for their whole 30-minute window).
    pub addr_file: Option<std::path::PathBuf>,
    pub scratch: std::path::PathBuf,
    /// How long a remote lease holder may go silent before it is presumed dead
    /// and its followers re-elect. This is the detection latency for a hard
    /// kill (no QUIC close frame), so it is also the worst-case stall a
    /// follower can suffer. See [`crate::lease`].
    pub lease_ttl: std::time::Duration,
}

/// Outcome of a validated AC lookup ([`Driver::validated_ac_get`]).
pub enum AcLookup {
    /// Cached result whose referenced blobs are all fetchable. The payload's
    /// own encoding — the fleet ships these bytes without ever reading them.
    Hit(Arc<Vec<u8>>),
    /// Entry exists but at least one referenced blob is gone (evicted CAS):
    /// callers must report a miss so the client re-executes and re-uploads.
    Unservable,
    Miss,
}

struct WorkerConn {
    id: u64,
    tx: mpsc::UnboundedSender<D2W>,
    /// Jobs sent and not yet answered (running + prefetched).
    inflight: Arc<AtomicU32>,
    slots: u32,
    os: String,
    arch: String,
    /// Mesh endpoint id, for direct blob probes (gossip-independent).
    endpoint: String,
    /// CI shard the worker restored before joining; finalize is sticky to
    /// it (a worker packs the range its store is rich in).
    preloaded_shard: Option<u8>,
}

/// What platform a job demands. Empty string = no constraint on that axis
/// (matches any worker). The payload decides how a spec maps onto one.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct PlatKey {
    pub os: String,
    pub arch: String,
}

impl PlatKey {
    fn admits(&self, os: &str, arch: &str) -> bool {
        (self.os.is_empty() || self.os == os) && (self.arch.is_empty() || self.arch == arch)
    }

    /// Queue keys a worker may pull from, most specific first.
    fn pull_order(os: &str, arch: &str) -> [PlatKey; 4] {
        [
            PlatKey {
                os: os.into(),
                arch: arch.into(),
            },
            PlatKey {
                os: os.into(),
                arch: String::new(),
            },
            PlatKey {
                os: String::new(),
                arch: arch.into(),
            },
            PlatKey::default(),
        ]
    }
}

/// An action in flight: who to answer, what to run, where it's running.
struct Job {
    /// The payload's encoded result. Opaque to the fleet.
    tx: oneshot::Sender<Result<Vec<u8>, String>>,
    action: Dig,
    /// Platform this action demands (empty axes = any).
    plat: PlatKey,
    /// Current assignee (worker id; 0 = driver-local/queued).
    worker: u64,
    attempts: u32,
    started: Option<std::time::Instant>,
    /// Tail speculation: raced on a second worker, first result wins.
    speculated: bool,
    /// Input-root affinity: actions sharing an input root run on the SAME
    /// worker. A crate's pipelined metadata compile and its rlib compile
    /// share an input root, and rustc's crate hash is only provably stable
    /// within one machine — split across machines the pair diverged and
    /// every downstream link died with E0460 (gooseberry PR#23 takes 5-8).
    /// Locality is the free side benefit.
    affinity: Option<u64>,
    /// Soft data-locality preference: worker id whose bloom claims this
    /// job's heaviest inputs. Honoured while `submitted` is younger than
    /// LOCALITY_PATIENCE, then anyone may take the job (delay scheduling).
    locality: Option<u64>,
    submitted: std::time::Instant,
}

/// How long a job waits for its data-local worker before running anywhere.
const LOCALITY_PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);

const MAX_ATTEMPTS: u32 = 3;
/// A running job must be at least this old before the tail races it on a
/// second worker — otherwise every action in a small build runs twice.
const SPECULATE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// Frees a local leader's followers if the leader is dropped before it publishes
/// (client disconnect, cancellation, panic). Without this they wait on a result
/// nobody is computing — trading duplicate work for a hang, which is the worse
/// bug of the two.
struct LeaderGuard<'a> {
    leases: &'a lease::Leases,
    key: &'a str,
    armed: bool,
    done: bool,
}

impl LeaderGuard<'_> {
    fn disarm(mut self) {
        self.done = true;
    }
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if self.armed && !self.done {
            self.leases.abandon_local(self.key);
        }
    }
}

pub struct Driver {
    pub store: Arc<Store>,
    /// The only thing here that can read a job spec or a result. Every proto
    /// decode in the system is on the far side of this. See [`crate::payload`].
    payload: Arc<dyn Payload>,
    cfg: DriverCfg,
    jobs: Mutex<HashMap<u64, Job>>,
    workers: Mutex<Vec<WorkerConn>>,
    worker_arrived: tokio::sync::Notify,
    /// Latched once the pool first reaches `min_workers` — the barrier must
    /// not re-arm when workers are lost mid-run (a CI fleet cannot refill;
    /// re-blocking would hang the build until the job timeout).
    pool_formed: std::sync::atomic::AtomicBool,
    /// Latch so eager metadata prefetch fires once.
    prefetch_started: std::sync::atomic::AtomicBool,
    next_job: AtomicU64,
    next_worker: AtomicU64,
    local_slots: Semaphore,
    /// Jobs awaiting assignment, bucketed by demanded platform — workers
    /// pull from their matching buckets via bounded outstanding counts.
    queue: Mutex<HashMap<PlatKey, std::collections::VecDeque<u64>>>,
    /// Decentralized mode: blob hash -> producing worker's endpoint id.
    providers: Mutex<HashMap<String, String>>,
    /// Diagnostic: how many unservable samples we have logged.
    unservable_logged: AtomicU64,
    /// Session-scope validation memo. Serving a hit costs a full
    /// transitive validation (tree fetches + fleet HasMany batches); a
    /// warm lap makes ~59k lookups over ~20k unique entries, so verdicts
    /// are memoized. Servable verdicts clear when ANY worker disconnects
    /// (a holder may have left); unservable verdicts expire on a short
    /// TTL (blobs may arrive) - staleness there is over-conservative,
    /// never dishonest.
    /// Validated-servable AC entries -> their encoded ActionResult bytes.
    /// Read-mostly under RwLock: a warm hit is served from memory with no
    /// disk read, no revalidation, and concurrent readers (~110k lookups
    /// per hetero lap otherwise serialized on one Mutex + one file read
    /// each). Cleared when a worker disconnects (a holder may have left).
    memo_servable: tokio::sync::RwLock<HashMap<String, Arc<Vec<u8>>>>,
    memo_unservable: Mutex<HashMap<String, std::time::Instant>>,
    /// Bloom gossip: worker endpoint id -> summary of its store.
    blooms: Mutex<HashMap<String, mesh::Bloom>>,
    /// One QUIC connection per peer, multiplexed bi-streams. Per-call dials
    /// melt the endpoint under FindMissing storms (reader 28929862924: three
    /// warm daemons probing ~50k digests against an AC-only store spawned
    /// thousands of concurrent handshakes; the tonic h2 streams starved and
    /// every leg died with BrokenPipe).
    peer_conns: Mutex<HashMap<String, Connection>>,
    /// Input-root hash -> owning worker id ([`Job::affinity`]). Locked
    /// after `queue` everywhere.
    affinity_owner: Mutex<HashMap<u64, u64>>,
    /// Bounds concurrent mesh fetches/probes so a warm-start burst cannot
    /// exhaust sockets even with pooled connections.
    mesh_fetches: Semaphore,
    /// Cache outcome accounting for the stats heartbeat: AC hits that were
    /// successes, AC hits that were cached failures, and executions forced
    /// by do_not_cache actions (the prelude's diag wrappers).
    /// Distinct shards acked (redundant assignment: a shard is banked
    /// when ANY of its assignees uploads - union-sync makes both copies
    /// complete, so whichever wins is a full artifact).
    finalized_shards: Mutex<std::collections::BTreeSet<u8>>,
    /// Single-flight index: action hash -> waiters on the leader's result.
    /// Two concurrent Execute calls for one action execute it ONCE; the
    /// second attaches. BuildKit gets this for free inside one daemon
    /// (solver edge-merge) and loses it across daemons — the same property,
    /// here, is what stops a CI matrix compiling the identical action on
    /// every machine at once. See docs/buildkit-plan.md P2.
    ///
    /// Cross-machine single-flight. Local claims today (one driver = one
    /// coordinator for buck2); the mesh arm lets a worker's buildkitd claim
    /// too. See [`crate::lease`].
    leases: lease::Leases,
    pub ac_hit_ok: AtomicU64,
    pub ac_hit_fail: AtomicU64,
    pub dnc_exec: AtomicU64,
    /// Mesh endpoint, for read-through fetches from providers.
    mesh_ep: tokio::sync::OnceCell<Endpoint>,
}

impl Driver {
    pub fn new(store: Arc<Store>, cfg: DriverCfg, payload: Arc<dyn Payload>) -> Arc<Self> {
        let lease_ttl = cfg.lease_ttl;
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Arc::new(Self {
            store,
            payload,
            cfg,
            jobs: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            worker_arrived: tokio::sync::Notify::new(),
            prefetch_started: std::sync::atomic::AtomicBool::new(false),
            pool_formed: std::sync::atomic::AtomicBool::new(false),
            next_job: AtomicU64::new(1),
            next_worker: AtomicU64::new(1),
            local_slots: Semaphore::new(cores),
            queue: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
            unservable_logged: AtomicU64::new(0),
            memo_servable: tokio::sync::RwLock::new(HashMap::new()),
            memo_unservable: Mutex::new(HashMap::new()),
            blooms: Mutex::new(HashMap::new()),
            peer_conns: Mutex::new(HashMap::new()),
            affinity_owner: Mutex::new(HashMap::new()),
            mesh_fetches: Semaphore::new(64),
            finalized_shards: Mutex::new(std::collections::BTreeSet::new()),
            leases: lease::Leases::with_ttl(lease_ttl),
            ac_hit_ok: AtomicU64::new(0),
            ac_hit_fail: AtomicU64::new(0),
            dnc_exec: AtomicU64::new(0),
            mesh_ep: tokio::sync::OnceCell::new(),
        })
    }

    /// Bind the iroh endpoint and accept workers forever.
    pub async fn serve_mesh(self: &Arc<Self>) -> Result<()> {
        let ep = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(mesh::secret(&self.cfg.session, "driver"))
            .alpns(vec![mesh::ALPN.to_vec()])
            .bind()
            .await?;
        if let Some(path) = &self.cfg.addr_file {
            // Give the relay handshake a beat so the addr carries a relay
            // URL; workers can dial it without any discovery lookup.
            let mut addr = ep.addr();
            for _ in 0..50 {
                if addr.relay_urls().next().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                addr = ep.addr();
            }
            let json = serde_json::to_string(&addr)?;
            tokio::fs::write(path, &json).await?;
            println!("[driver] addr written to {}", path.display());
        }
        if let Some(sig) = self.cfg.finalize_file.clone() {
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    if tokio::fs::metadata(&sig).await.is_ok() {
                        // Scrub dead AC rows BEFORE banking so the snapshot
                        // is clean and next lap skips their revalidation.
                        let (scanned, deleted) = this.scrub_ac().await;
                        println!("[driver] ac scrub: {deleted}/{scanned} unservable rows deleted");
                        let shards_needed = this.finalize_shards(8).await;
                        let told = shards_needed as u64;
                        println!("[driver] finalize signalled: told {told} workers");
                        // Acks land within seconds when they land at all;
                        // a lost ack (observed: 6/8, 2 never arrived) must
                        // cost ~2min, not a 15min deadline - stragglers
                        // degrade to partial save by design.
                        // Warm laps SKIP unchanged shards -> ack in
                        // seconds regardless, so a generous deadline costs
                        // nothing there; it only spends time on TRANSITION
                        // laps that actually re-pack (scrub/re-exec changed
                        // the era) - where a complete 8/8 bank is worth it
                        // (a partial bank re-poisons the next era). 45s cut
                        // a re-packing lap to 4/8; 180s lets it finish.
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(180);
                        while this.finalized_shards.lock().await.len() < shards_needed
                            && std::time::Instant::now() < deadline
                        {
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        }
                        // APPEND .done - with_extension REPLACES the last
                        // extension ("finalize.signal" -> "finalize.done")
                        // and the CI poll for finalize.signal.done burned
                        // its full 1000s cap on EVERY lap (~16.7min/lap,
                        // in every decomposition as "finalize 16m40s").
                        let banked = this.finalized_shards.lock().await.len();
                        let done = std::path::PathBuf::from(format!("{}.done", sig.display()));
                        let _ = tokio::fs::write(&done, format!("{banked}")).await;
                        println!(
                            "[driver] finalize complete: {banked}/{shards_needed} shards banked"
                        );
                        // Everyone still connected (unassigned workers,
                        // lost-ack packers) exits NOW instead of idling
                        // until their CI timeout cap: the driver's own
                        // teardown is a SIGTERM, which sends no QUIC close.
                        let n = {
                            let workers = this.workers.lock().await;
                            for w in workers.iter() {
                                let _ = w.tx.send(D2W::Exit);
                            }
                            workers.len()
                        };
                        println!("[driver] exit broadcast: told {n} remaining workers");
                        return;
                    }
                }
            });
        }

        // Liveness heartbeat: event-driven gossip goes silent on idle legs,
        // and a worker can't tell a quiet driver from a dead one (see
        // D2W::Ping). 20s beat, 90s worker patience.
        //
        // The same beat reaps dead lease holders. `evict_peer` already handles
        // a clean disconnect; this is the backstop for the messier deaths — a
        // hung process, a severed network, a runner yanked mid-action — where
        // silence is the only signal we get. Without it a follower waits on a
        // job nobody is building.
        {
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                    for w in this.workers.lock().await.iter() {
                        let _ = w.tx.send(D2W::Ping);
                    }
                }
            });
        }

        // Reaper, on its own faster beat. A leader killed with -9 sends no QUIC
        // close, so `evict_peer` never fires for it; silence is the only signal
        // and this is what listens for it.
        {
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(lease::REAP_EVERY).await;
                    let reaped = this.reap_leases();
                    if reaped > 0 {
                        println!("[driver] reaped {reaped} lease(s) from silent holders");
                    }
                }
            });
        }

        let _ = self.mesh_ep.set(ep.clone());
        {
            // Speculation needs a clock, not just completion events.
            let this = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    this.pump().await;
                }
            });
        }
        println!(
            "[driver] mesh endpoint_id={} session={} decentralized={}",
            ep.id(),
            self.cfg.session,
            self.cfg.decentralized
        );
        loop {
            let Some(incoming) = ep.accept().await else {
                bail!("mesh endpoint closed");
            };
            let this = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        if let Err(e) = this.handle_worker(conn).await {
                            eprintln!("[driver] worker connection ended: {e:#}");
                        }
                    }
                    Err(e) => eprintln!("[driver] incoming failed: {e:#}"),
                }
            });
        }
    }

    async fn handle_worker(self: &Arc<Self>, conn: Connection) -> Result<()> {
        // Control stream is the first bi-stream the peer opens.
        let (ctrl_send, mut ctrl_recv) = conn.accept_bi().await?;
        let hello: W2D = mesh::recv_frame(&mut ctrl_recv)
            .await?
            .context("peer hung up before Hello")?;

        // A bare client (a lease claimant, a blob reader) is not a worker: it
        // takes no jobs and joins no pool. Serve its streams and let it go —
        // enrolling it would schedule work it cannot do.
        if matches!(hello, W2D::ClientHello) {
            let endpoint = conn.remote_id().to_string();
            while let Ok((send, recv)) = conn.accept_bi().await {
                let driver = self.clone();
                let peer = endpoint.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_blob_stream(driver, peer, send, recv).await {
                        eprintln!("[driver] client stream error: {e:#}");
                    }
                });
            }
            // The client is gone. Anything it was leading is now ownerless —
            // free its followers immediately rather than making them wait out
            // the full TTL to discover nobody is building their job.
            let freed = self.leases.evict_peer(&endpoint);
            if freed > 0 {
                println!("[driver] client left holding {freed} lease(s) — followers re-elect");
            }
            return Ok(());
        }

        let W2D::Hello {
            os,
            arch,
            slots,
            preloaded_shard,
        } = hello
        else {
            bail!("expected Hello, got {hello:?}");
        };
        let worker_id = self.next_worker.fetch_add(1, Ordering::Relaxed);
        let endpoint = conn.remote_id().to_string();
        println!("[driver] worker {worker_id} joined: {os}/{arch} slots={slots} ep={endpoint}");

        // Mode handshake before anything else flows.
        let mut ctrl_send = ctrl_send;
        mesh::send_frame(
            &mut ctrl_send,
            &D2W::Welcome {
                decentralized: self.cfg.decentralized,
            },
        )
        .await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
        let inflight = Arc::new(AtomicU32::new(0));
        self.workers.lock().await.push(WorkerConn {
            id: worker_id,
            tx,
            inflight: inflight.clone(),
            slots,
            os,
            arch,
            endpoint: endpoint.clone(),
            preloaded_shard,
        });
        self.worker_arrived.notify_waiters();
        self.pump().await;

        // Eager metadata prefetch: once the pool is up, warm the driver's
        // hot-CAS from the fleet in the background so client downloads are
        // local, not relayed at build time.
        if self.cfg.prefetch_metadata
            && self.workers.lock().await.len() >= self.cfg.min_workers.max(1)
            && !self.prefetch_started.swap(true, Ordering::Relaxed)
        {
            let this = self.clone();
            tokio::spawn(async move { this.eager_prefetch_metadata().await });
        }

        // writer: job dispatches -> control stream
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if mesh::send_frame(&mut ctrl_send, &msg).await.is_err() {
                    break;
                }
            }
        });

        // blob streams: each request on its own bi-stream
        let blob_conn = conn.clone();
        let blob_driver = self.clone();
        let blob_ep = endpoint.clone();
        let blobs = tokio::spawn(async move {
            while let Ok((send, recv)) = blob_conn.accept_bi().await {
                let driver = blob_driver.clone();
                let peer = blob_ep.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_blob_stream(driver, peer, send, recv).await {
                        eprintln!("[driver] blob stream error: {e:#}");
                    }
                });
            }
        });

        // reader: results back from the worker
        let read_result: Result<()> = async {
            loop {
                let Some(msg) = mesh::recv_frame::<W2D>(&mut ctrl_recv).await? else {
                    return Ok(()); // clean disconnect
                };
                match msg {
                    W2D::Done {
                        job,
                        action_result,
                        stored,
                    } => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        if self.cfg.decentralized && !stored.is_empty() {
                            let mut providers = self.providers.lock().await;
                            for hash in stored {
                                providers.insert(hash, endpoint.clone());
                            }
                        }
                        // Opaque to the fleet: the payload encoded it, the
                        // payload's client will decode it.
                        self.complete(job, Ok(action_result)).await;
                        self.pump().await;
                    }
                    W2D::Failed { job, msg } => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        self.complete(job, Err(msg)).await;
                        self.pump().await;
                    }
                    W2D::Finalized { shard } => {
                        println!("[driver] worker {worker_id} finalized shard {shard}");
                        self.finalized_shards.lock().await.insert(shard);
                    }
                    W2D::Holdings { bloom } => {
                        self.blooms.lock().await.insert(endpoint.clone(), bloom);
                        // Rebroadcast the full picture to everyone.
                        let peers: Vec<(String, mesh::Bloom)> = self
                            .blooms
                            .lock()
                            .await
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        for w in self.workers.lock().await.iter() {
                            let _ = w.tx.send(D2W::Blooms {
                                peers: peers.clone(),
                            });
                        }
                    }
                    W2D::Hello { .. } | W2D::ClientHello => {
                        bail!("unexpected second Hello on a worker stream")
                    }
                }
            }
        }
        .await;

        self.workers.lock().await.retain(|w| w.id != worker_id);
        // A departed worker may have been the sole holder behind memoized
        // servable verdicts - revalidate everything from here on.
        self.memo_servable.write().await.clear();
        self.blooms.lock().await.remove(&endpoint);
        // It is also not coming back to heartbeat. Free anyone waiting on a
        // lease it held NOW, rather than making them wait out the full TTL to
        // discover that nobody is building their job.
        let freed = self.leases.evict_peer(&endpoint);
        if freed > 0 {
            println!(
                "[driver] worker {worker_id} left holding {freed} lease(s) — followers re-elect"
            );
        }
        println!("[driver] worker {worker_id} left");
        writer.abort();
        blobs.abort();

        // Requeue whatever the departed worker still owed us.
        let orphans: Vec<u64> = self
            .jobs
            .lock()
            .await
            .iter()
            .filter(|(_, j)| j.worker == worker_id)
            .map(|(id, _)| *id)
            .collect();
        if !orphans.is_empty() {
            println!(
                "[driver] requeueing {} job(s) from worker {worker_id}",
                orphans.len()
            );
            {
                let mut jobs = self.jobs.lock().await;
                let mut queue = self.queue.lock().await;
                for id in orphans.iter().rev() {
                    if let Some(job) = jobs.get_mut(id) {
                        job.worker = 0;
                        job.started = None;
                        queue.entry(job.plat.clone()).or_default().push_front(*id);
                    }
                }
            }
            self.pump().await;
        }
        read_result
    }

    /// A driver with a throwaway store and no workers — the registry tests need
    /// one, and they live in another module.
    #[cfg(test)]
    pub fn for_test() -> Arc<Self> {
        let dir = tempfile::tempdir().unwrap().keep();
        Driver::new(
            Arc::new(Store::new(dir).unwrap()),
            DriverCfg {
                session: "test".into(),
                min_workers: 0,
                local_exec: false,
                decentralized: false,
                hardlinks: true,
                addr_file: None,
                finalize_file: None,
                cache_failures: false,
                locality: false,
                prefetch_metadata: false,
                scratch: std::env::temp_dir(),
                lease_ttl: lease::DEFAULT_LEASE_TTL,
            },
            Arc::new(crate::payload::reapi::Reapi),
        )
    }

    /// Executions elided by single-flight — followers that attached to a
    /// leader instead of rebuilding. The whole point, counted.
    pub fn sf_merged(&self) -> u64 {
        self.leases.merged.load(Ordering::Relaxed)
    }

    /// Ask some OTHER worker to take a copy of `hash`, so it does not live on a
    /// single machine. Best-effort and off the critical path: a failure costs
    /// durability, never correctness.
    async fn replicate(&self, hash: &str, producer: &str) {
        let workers = self.workers.lock().await;
        // Deterministic pick from the hash: spreads replicas evenly, and two
        // announces of the same blob choose the same second holder rather than
        // scattering copies.
        let others: Vec<&WorkerConn> = workers.iter().filter(|w| w.endpoint != producer).collect();
        if others.is_empty() {
            return; // a one-worker fleet has nowhere to put a second copy
        }
        let pick =
            usize::from_str_radix(&hash[..4.min(hash.len())], 16).unwrap_or(0) % others.len();
        let _ = others[pick].tx.send(D2W::Replicate {
            digest: hash.to_string(),
        });
    }

    /// The fleet's lease table — cross-machine single-flight.
    pub fn lease_table(&self) -> &lease::Leases {
        &self.leases
    }

    /// Expire leases whose holder has gone quiet, and free their waiters.
    pub fn reap_leases(&self) -> usize {
        self.leases.reap()
    }

    pub fn cache_failures(&self) -> bool {
        self.cfg.cache_failures
    }

    pub async fn pending_jobs(&self) -> usize {
        self.jobs.lock().await.len()
    }

    /// Per-platform queued-work summary for the stats heartbeat, e.g.
    /// "windows/x86_64:12 macos/aarch64:340" ("-" when nothing queued).
    pub async fn queue_summary(&self) -> String {
        let queue = self.queue.lock().await;
        let mut parts: Vec<String> = queue
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(k, q)| {
                let os = if k.os.is_empty() { "*" } else { &k.os };
                let arch = if k.arch.is_empty() { "*" } else { &k.arch };
                format!("{os}/{arch}:{}", q.len())
            })
            .collect();
        parts.sort();
        if parts.is_empty() {
            "-".to_owned()
        } else {
            parts.join(" ")
        }
    }

    /// Batch presence over the whole mesh: local store + provider index
    /// first, then bloom-routed exact `HasMany` verification against
    /// workers (blooms route, never testify). Confirmed holders land in
    /// the providers map so later fetches redirect straight to them.
    /// This is what lets shard-seeded worker stores count as "present"
    /// in buck2's FindMissingBlobs without the driver holding the bytes.
    /// Pooled peer connection: reuse the live one, dial on first use or
    /// after the old one died. Callers that hit a stream-open error should
    /// `drop_peer_conn` and retry once — a stale handle looks healthy until
    /// the first stream touches it.
    async fn peer_conn(&self, peer: &str) -> Result<Connection> {
        if let Some(c) = self.peer_conns.lock().await.get(peer) {
            if c.close_reason().is_none() {
                return Ok(c.clone());
            }
        }
        let ep = self.mesh_ep.get().context("mesh endpoint not up")?;
        let id: iroh::EndpointId = peer
            .parse()
            .map_err(|_| anyhow::anyhow!("bad peer endpoint {peer:?}"))?;
        let conn = ep.connect(id, mesh::ALPN).await?;
        self.peer_conns
            .lock()
            .await
            .insert(peer.to_string(), conn.clone());
        Ok(conn)
    }

    async fn drop_peer_conn(&self, peer: &str) {
        self.peer_conns.lock().await.remove(peer);
    }

    /// One BlobReq round-trip on a pooled connection, one retry on a fresh
    /// connection if the pooled one turned out stale.
    async fn peer_request(&self, peer: &str, req: &BlobReq) -> Result<BlobResp> {
        for attempt in 0..2 {
            let conn = self.peer_conn(peer).await?;
            let res = async {
                let (mut send, mut recv) = conn.open_bi().await?;
                mesh::send_frame(&mut send, req).await?;
                send.finish()?;
                mesh::recv_frame::<BlobResp>(&mut recv)
                    .await?
                    .context("peer closed blob stream")
            }
            .await;
            match res {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt == 0 => {
                    self.drop_peer_conn(peer).await;
                    let _ = e;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop returns on second attempt")
    }

    pub async fn has_blobs(self: &Arc<Self>, digs: &[Dig]) -> Vec<bool> {
        let mut have = vec![false; digs.len()];
        // Peer to ask per unknown digest: the provider-index entry first,
        // else the first bloom claimant. The index is a routing HINT, not
        // a presence oracle - a worker's LRU can evict a blob minutes
        // after announcing it, and testifying on the bare entry turned
        // 3,650 stale hints into hard exec failures per lap (healing4/5).
        // Both sources get the same exact HasMany verification.
        let mut by_peer: HashMap<String, Vec<usize>> = HashMap::new();
        {
            let providers = self.providers.lock().await;
            let blooms = self.blooms.lock().await;
            for (i, d) in digs.iter().enumerate() {
                if self.store.has(d).await {
                    have[i] = true;
                    continue;
                }
                let peer = providers.get(&d.hash).cloned().or_else(|| {
                    blooms
                        .iter()
                        .find(|(_, b)| b.contains(&d.hash))
                        .map(|(e, _)| e.clone())
                });
                if let Some(p) = peer {
                    by_peer.entry(p).or_default().push(i);
                }
            }
        }
        // All peers concurrently — this sits under buck2's FindMissingBlobs,
        // and sequential per-peer round-trips scale with fleet size (same
        // lesson probe_workers already carries). Permit INSIDE each future:
        // holding one across the fan-out would starve the fleet on big
        // batches. Merges stay sequential after the RTTs.
        let verdicts = futures::future::join_all(by_peer.into_iter().map(|(peer, idxs)| {
            let batch: Vec<Dig> = idxs.iter().map(|&i| digs[i].clone()).collect();
            async move {
                let _permit = self.mesh_fetches.acquire().await;
                let confirmed = match self.peer_request(&peer, &BlobReq::HasMany(batch)).await {
                    Ok(BlobResp::HaveMany(v)) => Some(v),
                    _ => None,
                };
                (peer, idxs, confirmed)
            }
        }))
        .await;
        for (peer, idxs, confirmed) in verdicts {
            let mut providers = self.providers.lock().await;
            for (k, &i) in idxs.iter().enumerate() {
                let ok = confirmed
                    .as_ref()
                    .and_then(|v| v.get(k))
                    .copied()
                    .unwrap_or(false);
                if ok {
                    have[i] = true;
                    providers.insert(digs[i].hash.clone(), peer.clone());
                } else if providers.get(&digs[i].hash) == Some(&peer) {
                    // Unproven: evict so the next lookup rediscovers
                    // honestly instead of re-trusting the stale entry.
                    providers.remove(&digs[i].hash);
                }
            }
        }
        have
    }

    /// AC lookup that only returns results the CAS can actually deliver.
    /// BOTH doors — the GetActionResult endpoint and Execute's short-circuit
    /// — must go through here: an unvalidated Execute door served 17k
    /// blob-less results after cache eviction (writer 28935304124, 34,208
    /// extract_artifacts failures).
    pub async fn validated_ac_get(self: &Arc<Self>, hash: &str) -> AcLookup {
        // Fast path: a validated-servable entry is served from memory -
        // no disk read, no revalidation, concurrent readers.
        if let Some(cached) = self.memo_servable.read().await.get(hash).cloned() {
            return AcLookup::Hit(cached);
        }
        let Some(bytes) = self.store.ac_get(hash).await else {
            return AcLookup::Miss;
        };
        if let Some(at) = self.memo_unservable.lock().await.get(hash) {
            if at.elapsed() < std::time::Duration::from_secs(120) {
                return AcLookup::Unservable;
            }
        }
        // The payload names every blob this result commits us to delivering -
        // transitively, because a directory output's top-level digest proves
        // only that its TREE PROTO exists, not its contents (reader
        // 29010597531 lost 5,390 actions to interior files of "validated"
        // directory outputs that existed nowhere).
        //
        // An error here means "cannot vouch for this result" - a corrupt entry
        // OR a tree we could not fetch. Both are Unservable rather than Miss:
        // the client re-executes either way, and refusing to serve is the safe
        // direction. `note_ac_written` clears the verdict once it is rewritten.
        let blobs = MeshBlobs(self.clone());
        let digs = match self.payload.referenced_digests(&bytes, &blobs).await {
            Ok(d) => d,
            Err(e) => {
                let n = self.unservable_logged.fetch_add(1, Ordering::Relaxed);
                if n < 20 {
                    println!("[driver] unservable sample {n}: action {hash}: {e:#}");
                }
                self.memo_unservable
                    .lock()
                    .await
                    .insert(hash.to_string(), std::time::Instant::now());
                return AcLookup::Unservable;
            }
        };
        if !digs.is_empty() {
            let have = self.has_blobs(&digs).await;
            if let Some(i) = have.iter().position(|p| !p) {
                // Sample the unservable class: ~23k entries stayed
                // unservable across laps even after fleet-union banking
                // (shard sizes unmoved - the blobs exist NOWHERE). Name
                // them so the next fix targets the right reference class.
                let n = self.unservable_logged.fetch_add(1, Ordering::Relaxed);
                if n < 20 {
                    println!(
                        "[driver] unservable sample {n}: action {hash} missing blob {}/{} (of {} referenced)",
                        digs[i].hash,
                        digs[i].size,
                        digs.len()
                    );
                }
                self.memo_unservable
                    .lock()
                    .await
                    .insert(hash.to_string(), std::time::Instant::now());
                return AcLookup::Unservable;
            }
        }
        let bytes = Arc::new(bytes);
        self.memo_servable
            .write()
            .await
            .insert(hash.to_string(), bytes.clone());
        AcLookup::Hit(bytes)
    }

    /// A fresh result was written for this key: any cached unservable
    /// verdict is obsolete.
    pub async fn note_ac_written(&self, hash: &str) {
        self.memo_unservable.lock().await.remove(hash);
        self.memo_servable.write().await.remove(hash);
    }

    /// Direct HasMany probe of every connected worker for one digest.
    /// Returns the first endpoint that testifies, seeding the provider
    /// index so the next lookup is O(1).
    async fn probe_workers(&self, d: &Dig) -> Option<String> {
        let endpoints: Vec<String> = self
            .workers
            .lock()
            .await
            .iter()
            .map(|w| w.endpoint.clone())
            .filter(|e| !e.is_empty())
            .collect();
        // All peers concurrently: a healing run probes for thousands of
        // genuinely-new blobs, and 11 sequential round-trips per miss
        // multiplied out to hours (run 28962751323). One fan-out, first
        // yes wins; unanimous no answers in one RTT.
        let probes = endpoints.into_iter().map(|ep| async move {
            let hit = matches!(
                self.peer_request(&ep, &BlobReq::HasMany(vec![d.clone()])).await,
                Ok(BlobResp::HaveMany(v)) if v.first().copied().unwrap_or(false)
            );
            hit.then_some(ep)
        });
        let found = futures::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .next();
        if let Some(ep) = &found {
            self.providers
                .lock()
                .await
                .insert(d.hash.clone(), ep.clone());
        }
        found
    }

    /// Data-locality preference: the worker whose bloom claims the most
    /// BYTES of this input root's top-level files. Blooms are already
    /// gossiped to the driver, so scoring is in-memory bit-tests - the
    /// who-has-what oracle costs nothing extra. Returns None when no
    /// worker claims anything (cold data: any worker is equally far).
    async fn locality_pref(self: &Arc<Self>, input_root: &Dig) -> Option<u64> {
        let blobs = MeshBlobs(self.clone());
        let files = self.payload.heavy_inputs(input_root, &blobs).await;
        if files.is_empty() {
            return None;
        }
        let files: Vec<(&str, i64)> = files.iter().map(|(h, s)| (h.as_str(), *s)).collect();
        let blooms = self.blooms.lock().await;
        let workers = self.workers.lock().await;
        let mut best: Option<(u64, i64)> = None;
        for w in workers.iter() {
            let Some(bloom) = blooms.get(&w.endpoint) else {
                continue;
            };
            let score: i64 = files
                .iter()
                .filter(|(h, _)| bloom.contains(h))
                .map(|(_, s)| *s)
                .sum();
            if score > 0 && best.map(|(_, b)| score > b).unwrap_or(true) {
                best = Some((w.id, score));
            }
        }
        best.map(|(id, _)| id)
    }

    /// Read-through get: local store first, then fetch from the fleet and
    /// cache. Used by the gRPC surface (buck2's reads) and the mesh serve
    /// arm (workers' exec inputs).
    ///
    /// Candidate order: provider-index hint, then every bloom claimant,
    /// then one exact fan-out probe. A single peer's "Missing" is a
    /// routing miss (stale index, bloom false positive, LRU eviction) -
    /// NOT an answer: trusting it turned 3,650 fetches per lap into hard
    /// action failures (healing4/5). Failed hints are evicted so
    /// rediscovery stays honest.
    pub async fn get_blob(&self, d: &Dig) -> Result<Option<Vec<u8>>> {
        self.get_blob_inner(&d.hash, Some(d.size)).await
    }

    /// Read-through fetch by hash alone — the OCI path.
    ///
    /// REAPI digests always carry a size; an OCI digest never does. The size is
    /// not needed to FIND a blob (store, blooms, provider index and the mesh
    /// `Get` are all hash-keyed) — only to verify what came back. So a hash-only
    /// caller verifies by hash, and the size check degenerates rather than
    /// being faked: passing a zero size would make the cache-back's
    /// `put(Some(d))` reject every non-empty blob as a digest mismatch.
    pub async fn get_blob_by_hash(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.get_blob_inner(hash, None).await
    }

    async fn get_blob_inner(&self, hash: &str, size: Option<i64>) -> Result<Option<Vec<u8>>> {
        let d = &Dig {
            hash: hash.to_string(),
            size: size.unwrap_or_default(),
        };
        if let Some(bytes) = self.store.get_by_hash(hash).await? {
            return Ok(Some(bytes));
        }
        let _permit = self.mesh_fetches.acquire().await;
        // Retry rounds with backoff, candidates rebuilt fresh each round: a
        // saturated holder's transient fetch error must NOT become Missing.
        // Reader 29007342337 lost 3,212 actions to exactly that - the sole
        // holder of a hot shard range erroring under 25k-fetch load, the
        // one-shot chain concluding Missing, buck2 failing the action hard.
        let mut claimed_but_failed = false;
        for round in 0..4u32 {
            if round > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(round))).await;
            }
            let mut candidates: Vec<String> = Vec::new();
            if let Some(e) = self.providers.lock().await.get(&d.hash).cloned() {
                candidates.push(e);
            }
            {
                let blooms = self.blooms.lock().await;
                candidates.extend(
                    blooms
                        .iter()
                        .filter(|(_, b)| b.contains(&d.hash))
                        .map(|(e, _)| e.clone()),
                );
            }
            if candidates.is_empty() {
                // Gossip is 30s-periodic and a dice-warm client outruns it:
                // exact fan-out probe (also reseeds the provider index).
                match self.probe_workers(d).await {
                    Some(e) => candidates.push(e),
                    // Nobody claims it and nobody failed us: honest Missing.
                    None if !claimed_but_failed => return Ok(None),
                    None => break,
                }
            }
            candidates.dedup();
            let mut denied = 0usize;
            let total = candidates.len();
            for endpoint in candidates {
                match self.fetch_blob_from(&endpoint, d).await {
                    Ok(Some(bytes)) => {
                        // Verify by hash always; by size only when the caller
                        // knew one. An OCI caller does not, so the size check
                        // degenerates instead of rejecting every blob.
                        let expect = Dig {
                            hash: d.hash.clone(),
                            size: size.unwrap_or(bytes.len() as i64),
                        };
                        self.store.put(Some(&expect), &bytes).await?;
                        self.providers.lock().await.insert(d.hash.clone(), endpoint);
                        return Ok(Some(bytes));
                    }
                    Ok(None) => {
                        // Peer explicitly lacks it (bloom false positive or
                        // eviction): drop the stale hint, count the denial.
                        denied += 1;
                        let mut providers = self.providers.lock().await;
                        if providers.get(&d.hash) == Some(&endpoint) {
                            providers.remove(&d.hash);
                        }
                    }
                    Err(e) => {
                        claimed_but_failed = true;
                        println!(
                            "[driver] blob {} fetch from {endpoint} failed (round {round}): {e:#}",
                            d.hash
                        );
                        self.drop_peer_conn(&endpoint).await;
                    }
                }
            }
            if denied == total {
                // Every claimant explicitly denied: not transient, stop.
                break;
            }
        }
        if claimed_but_failed {
            // A holder exists but would not serve: this is an INFRA error,
            // retryable at the job layer (another worker, another route) -
            // never Missing, which clients treat as a hard verdict.
            bail!(
                "blob {}/{} is held by a peer but unfetchable after retries",
                d.hash,
                d.size
            );
        }
        Ok(None)
    }

    /// One blob fetch from one peer: two attempts (retry once on a fresh
    /// connection if the pooled one went stale). Ok(None) = peer answered
    /// Missing; Err = peer unreachable/protocol error. Callers treat both
    /// as "not from this peer", never as a global verdict.
    async fn fetch_blob_from(&self, endpoint: &str, d: &Dig) -> Result<Option<Vec<u8>>> {
        for attempt in 0..2 {
            let conn = self.peer_conn(endpoint).await?;
            let res: Result<Option<Vec<u8>>> = async {
                let (mut send, mut recv) = conn.open_bi().await?;
                mesh::send_frame(&mut send, &BlobReq::Get(d.clone())).await?;
                send.finish()?;
                match mesh::recv_frame::<BlobResp>(&mut recv)
                    .await?
                    .context("provider closed blob stream")?
                {
                    BlobResp::Found { size } => {
                        let bytes = mesh::recv_raw(&mut recv, size).await?;
                        Ok(Some(bytes))
                    }
                    BlobResp::Missing => Ok(None),
                    other => bail!("provider {endpoint} for {}: {other:?}", d.hash),
                }
            }
            .await;
            match res {
                Ok(x) => return Ok(x),
                Err(_) if attempt == 0 => self.drop_peer_conn(endpoint).await,
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop returns on second attempt")
    }

    /// One GetMany round-trip to a worker: pull `digs` into the local store.
    /// Returns the digests NOT obtained. Mirror of the worker's helper —
    /// `peer_request` can't carry the multi-frame reply. No redirect
    /// handling: workers' GetMany arm serves store-only.
    async fn fetch_many_from(&self, peer: &str, digs: &[Dig]) -> Result<Vec<Dig>> {
        let conn = self.peer_conn(peer).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::GetMany(digs.to_vec())).await?;
        send.finish()?;
        let mut unfetched = Vec::new();
        for d in digs {
            match mesh::recv_frame::<BlobResp>(&mut recv)
                .await?
                .context("holder closed mid-batch")?
            {
                BlobResp::Found { size } => {
                    let bytes = mesh::recv_raw(&mut recv, size).await?;
                    self.store.put(Some(d), &bytes).await?;
                }
                _ => unfetched.push(d.clone()),
            }
        }
        Ok(unfetched)
    }

    pub async fn worker_count(&self) -> usize {
        self.workers.lock().await.len()
    }

    /// Post-build: assign snapshot shards 0..of round-robin across the
    /// connected fleet and tell each worker to sync + save its shard.
    /// Returns how many workers were told (each shard covered when the
    /// fleet is >= `of`; extras double up for redundancy).
    /// Pull every small (<256KB) blob the fleet holds into the driver
    /// store, in parallel. Metadata (rmetas, dirs argsfiles) is small and
    /// is exactly what buck2 clients download to compute pipelined keys;
    /// having it driver-local turns those downloads from per-blob mesh
    /// relays (whose latency swings with worker network placement - the
    /// 36-minute linux leg) into local-disk reads. One startup burst that
    /// overlaps buck2's analysis phase.
    async fn eager_prefetch_metadata(self: &Arc<Self>) {
        const OF: u8 = 8;
        const SMALL: i64 = 256 * 1024;
        // Union each shard range across the fleet, keep the small ones we
        // do not already hold.
        let mut want: std::collections::BTreeMap<String, Dig> = Default::default();
        let peers: Vec<String> = self
            .workers
            .lock()
            .await
            .iter()
            .map(|w| w.endpoint.clone())
            .filter(|e| !e.is_empty())
            .collect();
        for shard in 0..OF {
            let lists = futures::future::join_all(peers.iter().map(|ep| {
                let this = self.clone();
                let ep = ep.clone();
                async move {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        this.peer_request(&ep, &BlobReq::ListShard { shard, of: OF }),
                    )
                    .await
                }
            }))
            .await;
            for l in lists {
                if let Ok(Ok(BlobResp::HashList(v))) = l {
                    for d in v {
                        if d.size > 0 && d.size <= SMALL && !self.store.has(&d).await {
                            want.entry(d.hash.clone()).or_insert(d);
                        }
                    }
                }
            }
        }
        let n = want.len();
        println!("[driver] eager prefetch: pulling {n} small metadata blobs from the fleet");
        // Group by bloom/provider claimant and pull chunked GetMany batches
        // per holder — request overhead per chunk, not per blob (this was
        // 48-wide but still one routed get_blob per blob). Unclaimed or
        // unfetched leftovers keep the per-blob path: get_blob's retry
        // rounds and fan-out probe are the honesty layer, not overhead.
        let mut by_peer: HashMap<String, Vec<Dig>> = HashMap::new();
        let mut unrouted: Vec<Dig> = Vec::new();
        {
            let providers = self.providers.lock().await;
            let blooms = self.blooms.lock().await;
            for d in want.into_values() {
                let peer = providers.get(&d.hash).cloned().or_else(|| {
                    blooms
                        .iter()
                        .find(|(_, b)| b.contains(&d.hash))
                        .map(|(e, _)| e.clone())
                });
                match peer {
                    Some(p) => by_peer.entry(p).or_default().push(d),
                    None => unrouted.push(d),
                }
            }
        }
        let got = Arc::new(AtomicU64::new(0));
        let leftovers = futures::future::join_all(by_peer.into_iter().map(|(peer, group)| {
            let got = got.clone();
            async move {
                let mut missed: Vec<Dig> = Vec::new();
                for chunk in group.chunks(512) {
                    let _permit = self.mesh_fetches.acquire().await;
                    match self.fetch_many_from(&peer, chunk).await {
                        Ok(rest) => {
                            got.fetch_add((chunk.len() - rest.len()) as u64, Ordering::Relaxed);
                            missed.extend(rest);
                        }
                        Err(_) => missed.extend_from_slice(chunk),
                    }
                }
                missed
            }
        }))
        .await;
        unrouted.extend(leftovers.into_iter().flatten());
        let sem = Arc::new(Semaphore::new(48));
        let tasks: Vec<_> = unrouted
            .into_iter()
            .map(|d| {
                let this = self.clone();
                let sem = sem.clone();
                let got = got.clone();
                tokio::spawn(async move {
                    let _p = sem.acquire().await.expect("sem open");
                    if matches!(this.get_blob(&d).await, Ok(Some(_))) {
                        got.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for t in tasks {
            let _ = t.await;
        }
        println!(
            "[driver] eager prefetch done: {}/{n} blobs now driver-local",
            got.load(Ordering::Relaxed)
        );
    }

    pub async fn finalize_shards(&self, of: u8) -> usize {
        // One shard, one worker: duplicate assignments produced duplicate
        // shard artifacts whose contents depended on each packer's sync
        // progress — the fetch-side "newest first" then picked one
        // arbitrarily (reader 28957851178 hit the holes).
        //
        // PRELOAD-STICKY: a worker packs the shard it restored - its store
        // is rich in exactly that range. Join-order round-robin repacked
        // ranges the assignee barely held, thinning the pool every lap
        // (reader 29007342337 published 47-92MB shards over healing6's
        // 452-500MB ones), and its first assignee was always the driver's
        // co-worker, whose store nothing ever packs - the eternally-absent
        // cas-shard-0. Workers without a preload are ineligible; a shard
        // with no eligible worker keeps its previous artifact.
        let workers = self.workers.lock().await;
        let mut assigned: Vec<Option<u64>> = vec![None; usize::from(of)];
        let mut taken: std::collections::BTreeSet<u64> = Default::default();
        for w in workers.iter() {
            if let Some(p) = w.preloaded_shard {
                let p = usize::from(p);
                if p < assigned.len() && assigned[p].is_none() {
                    assigned[p] = Some(w.id);
                    taken.insert(w.id);
                }
            }
        }
        for slot in assigned.iter_mut().filter(|s| s.is_none()) {
            if let Some(w) = workers
                .iter()
                .find(|w| w.preloaded_shard.is_some() && !taken.contains(&w.id))
            {
                *slot = Some(w.id);
                taken.insert(w.id);
            }
        }
        // Primary-only: workers run the whole build (39-42min) and do NOT
        // leave mid-lap, so a shard's holder is present at finalize - it
        // just needs TIME to pack+upload (the 180s deadline covers that).
        // Redundant backups were tried and reverted: they doubled the pack
        // work and published DUPLICATE artifacts (shard-1/2/4 twice)
        // without reliably improving coverage. On a warm era the pack SKIPS
        // (unchanged), so this is fast AND complete in steady state.
        let _ = &taken;
        let mut shards_assigned = 0;
        for (i, wid) in assigned.iter().enumerate() {
            let Some(wid) = wid else {
                println!("[driver] finalize: no eligible worker for shard {i} - previous artifact stands");
                continue;
            };
            if let Some(w) = workers.iter().find(|w| w.id == *wid) {
                if w.tx.send(D2W::Finalize { shard: i as u8, of }).is_ok() {
                    shards_assigned += 1;
                }
            }
        }
        shards_assigned
    }

    /// Delete AC entries whose referenced blobs are unservable across the
    /// fleet - dead rows from prior poisoned eras (13.5k on the 2026-07
    /// warm laps). buck2 ignores their refusal anyway (the consuming
    /// compile hits), so a scrubbed row becomes a clean fast Miss next
    /// lap instead of a repeated transitive-validation walk. Returns
    /// (scanned, deleted). Concurrency-bounded; validation is memoized.
    pub async fn scrub_ac(self: &Arc<Self>) -> (usize, usize) {
        let keys = self.store.ac_list();
        let scanned = keys.len();
        // Use the session validation memo: an entry proven servable this
        // lap needs no re-check (a warm lap validated 58k to delete 0 -
        // 120s of pure waste). Only entries NOT in the servable memo are
        // candidates - the unserved/suspect tail.
        let servable = self.memo_servable.read().await;
        let candidates: Vec<String> = keys
            .into_iter()
            .filter(|k| !servable.contains_key(k))
            .collect();
        drop(servable);
        let sem = Arc::new(Semaphore::new(32));
        let deleted = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for k in candidates {
            let this = self.clone();
            let sem = sem.clone();
            let deleted = deleted.clone();
            tasks.push(tokio::spawn(async move {
                let _p = sem.acquire().await.expect("sem open");
                if matches!(this.validated_ac_get(&k).await, AcLookup::Unservable) {
                    this.store.ac_delete(&k).await;
                    deleted.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
        (scanned, deleted.load(Ordering::Relaxed) as usize)
    }

    /// Resolve a job's oneshot and drop it from the table.
    async fn complete(&self, job_id: u64, result: Result<Vec<u8>, String>) {
        if let Some(job) = self.jobs.lock().await.remove(&job_id) {
            let _ = job.tx.send(result);
        }
    }

    /// Drain the queue into workers, pull-style: each worker holds at most
    /// `slots + n` jobs, where n adapts to queue depth — deep queue means big
    /// pipelines (round-trip amortisation), a draining queue tightens n to 1
    /// so the last placements are precise. With no workers, fall back to
    /// driver-local execution (when allowed). Idle tail capacity speculates:
    /// the oldest running job is raced on a free worker, first result wins.
    async fn pump(self: &Arc<Self>) {
        let workers = self.workers.lock().await;
        let mut jobs = self.jobs.lock().await;
        let mut queue = self.queue.lock().await;
        let host_os = std::env::consts::OS;
        let host_arch = std::env::consts::ARCH;

        if workers.is_empty() {
            if !self.cfg.local_exec {
                return; // hold everything queued until a worker joins
            }
            // Local fallback can only run host-compatible actions.
            let local_ids: Vec<u64> = PlatKey::pull_order(host_os, host_arch)
                .into_iter()
                .filter_map(|k| queue.remove(&k))
                .flatten()
                .collect();
            for job_id in local_ids {
                let Some(job) = jobs.get_mut(&job_id) else {
                    continue;
                };
                job.attempts += 1;
                job.worker = 0;
                job.started = Some(std::time::Instant::now());
                let action = job.action.clone();
                println!("[driver] job {job_id} -> local ({})", action.hash);
                let this = self.clone();
                tokio::spawn(async move {
                    let _permit = this.local_slots.acquire().await.expect("semaphore open");
                    let blobs = StoreBlobs {
                        store: this.store.clone(),
                        hardlinks: this.cfg.hardlinks,
                    };
                    let result = this
                        .payload
                        .execute(&blobs, &action, &this.cfg.scratch)
                        .await
                        .map(|d| d.result)
                        .map_err(|e| format!("{e:#}"));
                    this.complete(job_id, result).await;
                });
            }
            return;
        }

        let total_pending: usize = queue.values().map(|q| q.len()).sum();
        let n = (total_pending / (workers.len().max(1) * 4)).clamp(1, 16) as u32;
        let live_ids: std::collections::HashSet<u64> = workers.iter().map(|w| w.id).collect();
        let mut owners = self.affinity_owner.lock().await;
        // Each worker drains its matching buckets (most specific first)
        // while it has pipeline headroom. Per-platform buckets mean a full
        // windows pool never blocks an idle mac pool. Affinity jobs owned
        // by another LIVE worker are skipped (dead owners are usurped).
        loop {
            let mut assigned_any = false;
            for w in workers.iter() {
                while w.inflight.load(Ordering::Relaxed) < w.slots + n {
                    let job_id = PlatKey::pull_order(&w.os, &w.arch)
                        .into_iter()
                        .find_map(|k| {
                            let q = queue.get_mut(&k)?;
                            let pos = q.iter().position(|id| {
                                let Some(j) = jobs.get(id) else { return true };
                                // Soft data-locality: within the patience
                                // window only the preferred (live) worker
                                // takes the job; after it, anyone.
                                if let Some(pref) = j.locality {
                                    if pref != w.id
                                        && live_ids.contains(&pref)
                                        && j.submitted.elapsed() < LOCALITY_PATIENCE
                                    {
                                        return false;
                                    }
                                }
                                match j.affinity {
                                    None => true,
                                    Some(a) => match owners.get(&a) {
                                        Some(owner) => *owner == w.id || !live_ids.contains(owner),
                                        None => true,
                                    },
                                }
                            })?;
                            q.remove(pos)
                        });
                    let Some(job_id) = job_id else {
                        break; // nothing this worker can run
                    };
                    let Some(job) = jobs.get_mut(&job_id) else {
                        continue; // completed while queued
                    };
                    if job.attempts >= MAX_ATTEMPTS {
                        let job = jobs.remove(&job_id).expect("just found it");
                        let _ = job
                            .tx
                            .send(Err(format!("gave up after {MAX_ATTEMPTS} attempts")));
                        continue;
                    }
                    job.attempts += 1;
                    job.worker = w.id;
                    job.started = Some(std::time::Instant::now());
                    if let Some(a) = job.affinity {
                        owners.insert(a, w.id);
                    }
                    w.inflight.fetch_add(1, Ordering::Relaxed);
                    println!(
                        "[driver] job {job_id} -> worker {} ({})",
                        w.id, job.action.hash
                    );
                    if w.tx
                        .send(D2W::Run {
                            job: job_id,
                            action: job.action.clone(),
                        })
                        .is_err()
                    {
                        // Dying worker: put the job back; its disconnect
                        // path re-pumps.
                        w.inflight.fetch_sub(1, Ordering::Relaxed);
                        job.worker = 0;
                        job.started = None;
                        queue
                            .entry(job.plat.clone())
                            .or_default()
                            .push_front(job_id);
                        break;
                    }
                    assigned_any = true;
                }
            }
            if !assigned_any {
                break;
            }
        }

        // Tail speculation: nothing queued, RUN capacity idle -> race stragglers.
        if queue.values().all(|q| q.is_empty()) {
            for w in workers
                .iter()
                .filter(|w| w.inflight.load(Ordering::Relaxed) < w.slots)
            {
                let Some((&job_id, job)) = jobs
                    .iter_mut()
                    .filter(|(_, j)| {
                        j.started.is_some_and(|t| t.elapsed() >= SPECULATE_AFTER)
                            && !j.speculated
                            && j.worker != 0
                            && j.worker != w.id
                            // Affinity jobs never race on a second machine —
                            // a byte-different twin is exactly what affinity
                            // exists to prevent.
                            && j.affinity.is_none()
                            && j.plat.admits(&w.os, &w.arch)
                    })
                    .min_by_key(|(_, j)| j.started)
                else {
                    break;
                };
                job.speculated = true;
                w.inflight.fetch_add(1, Ordering::Relaxed);
                println!(
                    "[driver] job {job_id} speculated -> worker {} ({})",
                    w.id, job.action.hash
                );
                let _ = w.tx.send(D2W::Run {
                    job: job_id,
                    action: job.action.clone(),
                });
            }
        }
    }

    /// Barrier: block until the agreed pool has formed once. A latch, not a
    /// level check — late joiners always add capacity, and a shrinking pool
    /// never re-blocks dispatch.
    async fn await_pool_formed(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.pool_formed.load(Relaxed) {
            return;
        }
        while self.workers.lock().await.len() < self.cfg.min_workers {
            self.worker_arrived.notified().await;
        }
        self.pool_formed.store(true, Relaxed);
    }

    /// Run a job: queue it, dispatch (worker or local), await the result.
    /// Returns the payload's ENCODED result — the fleet never looks inside.
    pub async fn execute(self: &Arc<Self>, action_digest: &Dig) -> Result<crate::payload::Done> {
        self.await_pool_formed().await;

        // The payload reads the spec; the fleet only learns where it may run.
        // MeshBlobs, not the local store: with an AC-only-seeded driver the
        // spec's blobs live on worker shards, and a local-only read silently
        // degraded routing to PlatKey::default() - putting /bin/sh actions on
        // windows workers (reader 28957851178).
        let blobs = MeshBlobs(self.clone());
        let meta = self.payload.inspect(action_digest, &blobs).await;
        let (plat, do_not_cache, affinity, input_root) =
            (meta.plat, meta.do_not_cache, meta.affinity, meta.input_root);

        let locality = if self.cfg.locality {
            match &input_root {
                Some(d) => self.locality_pref(d).await,
                None => None,
            }
        } else {
            None
        };

        // Single-flight. `do_not_cache` jobs never merge: REAPI is explicit that
        // in-flight requests for such an Action may not be coalesced (the
        // prelude's diag wrappers want a genuine re-run).
        let merge = !do_not_cache;
        let key = &action_digest.hash;

        loop {
            if merge {
                if let lease::Claim::Follower(rx) = self.leases.claim_local(key) {
                    match rx.await {
                        Ok(lease::Outcome::Done(result)) => {
                            return Ok(crate::payload::Done {
                                result,
                                do_not_cache,
                            })
                        }
                        // The same job is the same failure for every waiter.
                        Ok(lease::Outcome::Failed(e)) => bail!("execution failed: {e}"),
                        // The leader vanished (cancelled, or its channel died).
                        // Go round again — one of us becomes the new leader
                        // rather than all of us waiting on a corpse.
                        Ok(lease::Outcome::Retry) | Err(_) => continue,
                    }
                }
            }

            // We lead. The guard frees our followers if we are dropped before
            // publishing (client disconnect, cancellation): trading duplicate
            // work for a hang would be a bad bargain.
            let guard = LeaderGuard {
                leases: &self.leases,
                key,
                armed: merge,
                done: false,
            };

            let job_id = self.next_job.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.jobs.lock().await.insert(
                job_id,
                Job {
                    tx,
                    action: action_digest.clone(),
                    plat: plat.clone(),
                    worker: 0,
                    attempts: 0,
                    started: None,
                    speculated: false,
                    affinity,
                    locality,
                    submitted: std::time::Instant::now(),
                },
            );
            self.queue
                .lock()
                .await
                .entry(plat.clone())
                .or_default()
                .push_back(job_id);
            self.pump().await;

            let outcome = rx
                .await
                .unwrap_or_else(|_| Err("job dropped without completion".to_string()));
            if merge {
                self.leases.release(
                    key,
                    None,
                    match &outcome {
                        Ok(r) => lease::Outcome::Done(r.clone()),
                        Err(e) => lease::Outcome::Failed(e.clone()),
                    },
                );
            }
            guard.disarm();

            let result = outcome.map_err(|e| anyhow::anyhow!("execution failed: {e}"))?;
            return Ok(crate::payload::Done {
                result,
                do_not_cache,
            });
        }
    }
}

/// Blobs resolved through the driver's read-through path: local store first,
/// then whichever peer holds them. What the payload gets when it needs to
/// follow a reference (a tree proto, a command) that may live anywhere on the
/// mesh — the fleet answers "where", the payload asks "what".
pub struct MeshBlobs(pub Arc<Driver>);

#[async_trait::async_trait]
impl crate::store::Blobs for MeshBlobs {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
        self.0
            .get_blob(d)
            .await?
            .with_context(|| format!("blob {} missing", d.hash))
    }
    async fn put(&self, bytes: Vec<u8>) -> Result<Dig> {
        self.0.store.put(None, &bytes).await
    }
}

/// Blobs backed directly by the driver's store (local fallback execution).
pub struct StoreBlobs {
    pub store: Arc<Store>,
    pub hardlinks: bool,
}

#[async_trait::async_trait]
impl crate::store::Blobs for StoreBlobs {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
        self.store
            .get(d)
            .await?
            .with_context(|| format!("blob {} missing", d.hash))
    }
    async fn put(&self, bytes: Vec<u8>) -> Result<Dig> {
        self.store.put(None, &bytes).await
    }
    async fn put_file(&self, path: &std::path::Path) -> Result<Dig> {
        let bytes = tokio::fs::read(path).await?;
        let d = Dig {
            hash: crate::store::sha256_hex(&bytes),
            size: bytes.len() as i64,
        };
        if self.hardlinks {
            self.store.adopt(&d, path).await?;
        } else {
            self.store.put(Some(&d), &bytes).await?;
        }
        Ok(d)
    }
    async fn materialize_file(
        &self,
        d: &Dig,
        dest: &std::path::Path,
        is_executable: bool,
    ) -> Result<()> {
        if !self.hardlinks || d.size == 0 {
            let bytes = self.get(d).await?;
            tokio::fs::write(dest, &bytes).await?;
            if is_executable {
                crate::store::set_exec(dest).await?;
            }
            return Ok(());
        }
        if self.store.link_out(d, dest).await? == crate::store::Materialized::Private
            && is_executable
        {
            crate::store::set_exec(dest).await?;
        }
        Ok(())
    }
}

/// `peer` is who is asking. A lease is OWNED by an endpoint: an anonymous claim
/// could not be heartbeated, evicted on disconnect, or defended against a
/// zombie's late release.
async fn serve_blob_stream(
    driver: Arc<Driver>,
    peer: String,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let Some(req) = mesh::recv_frame::<BlobReq>(&mut recv).await? else {
        return Ok(());
    };
    match req {
        BlobReq::Get(d) => {
            // Decentralized: point the asker at the producer instead of
            // relaying bytes through the driver's NIC.
            let redirect = if driver.cfg.decentralized {
                driver.providers.lock().await.get(&d.hash).cloned()
            } else {
                None
            };
            if let Some(endpoint) = redirect {
                mesh::send_frame(&mut send, &BlobResp::Provider { endpoint }).await?;
            } else {
                // get_blob, not store.get: with an AC-only-seeded driver a
                // worker's exec inputs often live on ANOTHER worker's shard;
                // a store-only serve returned Missing for 2,756 input
                // fetches (run 28959911677). Read-through relays and caches
                // locally, reconstituting the driver's hot set.
                match driver.get_blob(&d).await {
                    Ok(Some(bytes)) => {
                        mesh::send_frame(
                            &mut send,
                            &BlobResp::Found {
                                size: bytes.len() as u64,
                            },
                        )
                        .await?;
                        send.write_all(&bytes).await?;
                    }
                    Ok(None) => mesh::send_frame(&mut send, &BlobResp::Missing).await?,
                    Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
                }
            }
        }
        BlobReq::Put(d) => {
            let bytes = mesh::recv_raw(&mut recv, d.size as u64).await?;
            match driver.store.put(Some(&d), &bytes).await {
                Ok(_) => mesh::send_frame(&mut send, &BlobResp::PutOk).await?,
                Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
            }
        }
        BlobReq::HasMany(digs) => {
            let mut have = Vec::with_capacity(digs.len());
            for d in &digs {
                have.push(driver.store.has(d).await);
            }
            mesh::send_frame(&mut send, &BlobResp::HaveMany(have)).await?;
        }
        // Batched Get: one BlobResp frame per digest in request order, bytes
        // inline after each Found. Same per-item semantics as Get (Provider
        // redirect in decentralized mode, read-through get_blob otherwise);
        // workers issue chunks on parallel streams, so the read-through
        // relaying fans out across streams even though each stream is serial.
        BlobReq::Announce(digs) => {
            // Record the producer. A follower's fetch is then a redirect to the
            // machine that built it, not a relay through ours.
            {
                let mut providers = driver.providers.lock().await;
                for d in &digs {
                    providers.insert(d.hash.clone(), peer.clone());
                }
            }
            // Name a second holder. Until someone fetches it, this blob exists on
            // exactly ONE machine — and a worker that dies takes a downstream
            // action's INPUT with it, which no requeue can recover. Fire and
            // forget: nobody waits, and it never lands on the driver's disk.
            for d in &digs {
                driver.replicate(&d.hash, &peer).await;
            }
            mesh::send_frame(&mut send, &BlobResp::PutOk).await?;
        }

        // --- cross-machine single-flight (docs/buildkit-plan.md P2) ---
        BlobReq::Claim { key } => {
            match driver.leases.claim_peer(&key, &peer) {
                lease::Claim::Leader => {
                    mesh::send_frame(&mut send, &BlobResp::LeaseGranted).await?;
                }
                lease::Claim::Follower(rx) => {
                    // Hold the stream open and PUSH the outcome. The worker
                    // does not poll, and a leader's death reaches it as a
                    // LeaseRetry rather than as silence.
                    mesh::send_frame(&mut send, &BlobResp::LeaseHeld).await?;
                    let resp = match rx.await {
                        Ok(lease::Outcome::Done(r)) => BlobResp::LeaseDone(Ok(r)),
                        Ok(lease::Outcome::Failed(e)) => BlobResp::LeaseDone(Err(e)),
                        // Sender dropped == the lease was torn down. Same
                        // remedy: claim again.
                        Ok(lease::Outcome::Retry) | Err(_) => BlobResp::LeaseRetry,
                    };
                    mesh::send_frame(&mut send, &resp).await?;
                }
            }
        }
        BlobReq::Heartbeat { key } => {
            let alive = driver.leases.heartbeat(&key, &peer);
            mesh::send_frame(&mut send, &BlobResp::LeaseAlive(alive)).await?;
        }
        BlobReq::Release { key, result } => {
            let outcome = match result {
                Ok(r) => lease::Outcome::Done(r),
                Err(e) => lease::Outcome::Failed(e),
            };
            driver.leases.release(&key, Some(&peer), outcome);
            mesh::send_frame(&mut send, &BlobResp::PutOk).await?;
        }
        BlobReq::GetMany(digs) => {
            for d in &digs {
                let redirect = if driver.cfg.decentralized {
                    driver.providers.lock().await.get(&d.hash).cloned()
                } else {
                    None
                };
                if let Some(endpoint) = redirect {
                    mesh::send_frame(&mut send, &BlobResp::Provider { endpoint }).await?;
                    continue;
                }
                match driver.get_blob(d).await {
                    Ok(Some(bytes)) => {
                        mesh::send_frame(
                            &mut send,
                            &BlobResp::Found {
                                size: bytes.len() as u64,
                            },
                        )
                        .await?;
                        send.write_all(&bytes).await?;
                    }
                    Ok(None) => mesh::send_frame(&mut send, &BlobResp::Missing).await?,
                    Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
                }
            }
        }
        BlobReq::ListShard { shard, of } => {
            // Union across the FLEET, not just this store: the driver holds
            // only what it relayed, and blobs built on other workers
            // in-range otherwise never reach the banked shard - the
            // structural plateau (the same ~23.7k unservable entries
            // re-executed on every 76-minute lap). A peer that fails to
            // answer is skipped: partial union still beats local-only.
            let mut by_hash: std::collections::BTreeMap<String, i64> = driver
                .store
                .list_shard(shard, of)
                .into_iter()
                .map(|d| (d.hash, d.size))
                .collect();
            let peers: Vec<String> = driver
                .workers
                .lock()
                .await
                .iter()
                .map(|w| w.endpoint.clone())
                .filter(|e| !e.is_empty())
                .collect();
            let lists = futures::future::join_all(peers.into_iter().map(|ep| {
                let driver = driver.clone();
                async move {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        driver.peer_request(&ep, &BlobReq::ListShard { shard, of }),
                    )
                    .await
                }
            }))
            .await;
            for l in lists {
                if let Ok(Ok(BlobResp::HashList(v))) = l {
                    for d in v {
                        by_hash.entry(d.hash).or_insert(d.size);
                    }
                }
            }
            let digs: Vec<Dig> = by_hash
                .into_iter()
                .map(|(hash, size)| Dig { hash, size })
                .collect();
            mesh::send_frame(&mut send, &BlobResp::HashList(digs)).await?;
        }
    }
    send.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // The fleet itself no longer speaks REAPI; these tests do, because they
    // exercise it THROUGH the reapi payload.
    use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
    use prost::Message;

    fn test_driver(local_exec: bool) -> Arc<Driver> {
        test_driver_min(local_exec, 0)
    }

    fn test_driver_with(f: impl FnOnce(&mut DriverCfg)) -> Arc<Driver> {
        let dir = tempfile::tempdir().unwrap().keep();
        let mut cfg = DriverCfg {
            session: "test".into(),
            min_workers: 0,
            local_exec: false,
            decentralized: false,
            hardlinks: true,
            addr_file: None,
            finalize_file: None,
            cache_failures: false,
            locality: false,
            prefetch_metadata: false,
            scratch: std::env::temp_dir(),
            lease_ttl: lease::DEFAULT_LEASE_TTL,
        };
        f(&mut cfg);
        Driver::new(
            Arc::new(Store::new(dir).unwrap()),
            cfg,
            Arc::new(crate::payload::reapi::Reapi),
        )
    }

    fn test_driver_min(local_exec: bool, min_workers: usize) -> Arc<Driver> {
        let dir = tempfile::tempdir().unwrap().keep();
        Driver::new(
            Arc::new(Store::new(dir).unwrap()),
            DriverCfg {
                session: "test".into(),
                min_workers,
                local_exec,
                decentralized: false,
                hardlinks: true,
                addr_file: None,
                finalize_file: None,
                cache_failures: false,
                locality: false,
                prefetch_metadata: false,
                scratch: std::env::temp_dir(),
                lease_ttl: lease::DEFAULT_LEASE_TTL,
            },
            Arc::new(crate::payload::reapi::Reapi),
        )
    }

    /// Fake worker: drains Runs from its channel, completes them after a
    /// beat, and re-pumps — exactly what handle_worker's reader does.
    async fn fake_worker(d: &Arc<Driver>, id: u64, slots: u32, os: &str) -> Arc<AtomicU32> {
        let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
        let inflight = Arc::new(AtomicU32::new(0));
        let d2 = d.clone();
        let inf = inflight.clone();
        tokio::spawn(async move {
            while let Some(D2W::Run { job, .. }) = rx.recv().await {
                let d3 = d2.clone();
                let inf = inf.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    inf.fetch_sub(1, Ordering::Relaxed);
                    d3.complete(job, Ok(re::ActionResult::default().encode_to_vec()))
                        .await;
                    d3.pump().await;
                });
            }
        });
        let handle = inflight.clone();
        d.workers.lock().await.push(WorkerConn {
            id,
            tx,
            inflight,
            slots,
            os: os.into(),
            arch: "test_arch".into(),
            endpoint: String::new(),
            preloaded_shard: None,
        });
        handle
    }

    /// Like `fake_worker`, but counts the Runs it was dispatched — the
    /// execution counter a single-flight assertion needs.
    async fn counting_worker(d: &Arc<Driver>, id: u64, slots: u32, os: &str) -> Arc<AtomicU32> {
        let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
        let runs = Arc::new(AtomicU32::new(0));
        let inflight = Arc::new(AtomicU32::new(0));
        let d2 = d.clone();
        let inf = inflight.clone();
        let cnt = runs.clone();
        tokio::spawn(async move {
            while let Some(D2W::Run { job, .. }) = rx.recv().await {
                cnt.fetch_add(1, Ordering::Relaxed);
                let d3 = d2.clone();
                let inf = inf.clone();
                tokio::spawn(async move {
                    // Long enough that a second execute() for the same
                    // action is genuinely concurrent with the first.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    inf.fetch_sub(1, Ordering::Relaxed);
                    d3.complete(job, Ok(re::ActionResult::default().encode_to_vec()))
                        .await;
                    d3.pump().await;
                });
            }
        });
        d.workers.lock().await.push(WorkerConn {
            id,
            tx,
            inflight,
            slots,
            os: os.into(),
            arch: "test_arch".into(),
            endpoint: String::new(),
            preloaded_shard: None,
        });
        runs
    }

    fn dig(hex: char) -> Dig {
        Dig {
            hash: std::iter::repeat_n(hex, 64).collect(),
            size: 1,
        }
    }

    /// Single-flight: two concurrent Execute calls for the SAME action must
    /// execute it ONCE — the second attaches to the first's result. Without
    /// this, a CI matrix (or two racing PRs) burns the identical compile on
    /// every machine at once. This is the in-process half of the distributed
    /// lease (docs/buildkit-plan.md P2); BuildKit gets the same property for
    /// free inside one daemon and loses it across daemons.
    #[tokio::test]
    async fn concurrent_identical_actions_execute_once() {
        let d = test_driver(false);
        let runs = counting_worker(&d, 1, 4, "test").await;
        let a = dig('a');

        let (d1, d2, a1, a2) = (d.clone(), d.clone(), a.clone(), a.clone());
        let h1 = tokio::spawn(async move { d1.execute(&a1).await });
        let h2 = tokio::spawn(async move { d2.execute(&a2).await });
        h1.await.unwrap().expect("leader failed");
        h2.await.unwrap().expect("follower failed");

        assert_eq!(
            runs.load(Ordering::Relaxed),
            1,
            "identical concurrent actions must execute once, not twice"
        );
    }

    /// An abandoned leader (client disconnect / cancellation) must not strand
    /// its followers. Dropping the leader's future drops the waiters' senders;
    /// each follower's recv fails, it loops, and one of them becomes the new
    /// leader. Trading duplicate work for a hang would be a bad bargain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn abandoned_leader_re_elects_rather_than_hanging() {
        let d = test_driver(false);
        let runs = counting_worker(&d, 1, 4, "test").await;
        let a = dig('c');

        // Leader claims, then is cancelled mid-flight.
        let (d1, a1) = (d.clone(), a.clone());
        let leader = tokio::spawn(async move { d1.execute(&a1).await });
        // Let it claim the slot and dispatch.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (d2, a2) = (d.clone(), a.clone());
        let follower = tokio::spawn(async move { d2.execute(&a2).await });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        leader.abort();

        // The follower must still complete — re-elected as leader.
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), follower)
            .await
            .expect("follower stranded by an abandoned leader");
        got.unwrap().expect("re-elected follower must complete");
        assert!(runs.load(Ordering::Relaxed) >= 1);
        assert!(d.leases.is_empty(), "the abandoned lease leaked");
    }

    /// `do_not_cache` actions must never merge — REAPI is explicit that
    /// in-flight requests for such an Action may not be coalesced. buck2's
    /// prelude uses them for diagnostic wrappers that want a real re-run.
    #[tokio::test]
    async fn do_not_cache_actions_never_merge() {
        let d = test_driver(false);
        let runs = counting_worker(&d, 1, 4, "test").await;

        let action = re::Action {
            do_not_cache: true,
            ..Default::default()
        };
        let mut bytes = Vec::new();
        action.encode(&mut bytes).unwrap();
        let a = d.store.put(None, &bytes).await.unwrap();

        let (d1, d2, a1, a2) = (d.clone(), d.clone(), a.clone(), a.clone());
        let h1 = tokio::spawn(async move { d1.execute(&a1).await });
        let h2 = tokio::spawn(async move { d2.execute(&a2).await });
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();

        assert_eq!(
            runs.load(Ordering::Relaxed),
            2,
            "do_not_cache actions must both execute"
        );
    }

    /// Decentralized mode's one real hole, closed. A blob lives on exactly ONE
    /// machine between being produced and first being wanted; a worker that dies
    /// in that window takes a downstream action's INPUT with it, and no requeue
    /// can recover an input. So the driver names a SECOND holder the instant a
    /// blob is announced — off the critical path, and never onto its own disk
    /// (which was the ceiling that made decentralized mode necessary at all).
    #[tokio::test]
    async fn an_announced_blob_is_replicated_to_a_second_worker() {
        let d = test_driver(false);
        let (produced_by, other) = ("worker-a", "worker-b");
        let told = replicating_worker(&d, 1, produced_by).await;
        let told_other = replicating_worker(&d, 2, other).await;

        d.replicate(&"a".repeat(64), produced_by).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(
            told.lock().await.is_empty(),
            "the producer already has it — replicating to itself is not a second copy"
        );
        assert_eq!(
            told_other.lock().await.len(),
            1,
            "some OTHER worker must be asked to take a copy"
        );
    }

    /// A one-worker fleet has nowhere to put a second copy. It must not ask the
    /// producer to re-fetch its own blob.
    #[tokio::test]
    async fn a_lone_worker_is_not_asked_to_replicate_to_itself() {
        let d = test_driver(false);
        let told = replicating_worker(&d, 1, "only-worker").await;
        d.replicate(&"a".repeat(64), "only-worker").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(told.lock().await.is_empty());
    }

    /// A worker that records the Replicate orders it was given.
    async fn replicating_worker(
        d: &Arc<Driver>,
        id: u64,
        endpoint: &str,
    ) -> Arc<Mutex<Vec<String>>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let D2W::Replicate { digest } = msg {
                    sink.lock().await.push(digest);
                }
            }
        });
        d.workers.lock().await.push(WorkerConn {
            id,
            tx,
            inflight: Arc::new(AtomicU32::new(0)),
            slots: 1,
            os: "test".into(),
            arch: "test_arch".into(),
            endpoint: endpoint.into(),
            preloaded_shard: None,
        });
        got
    }

    /// Distinct actions must NOT be merged — the coalescing is keyed on the
    /// action digest, not on "something is already running".
    #[tokio::test]
    async fn distinct_actions_still_both_execute() {
        let d = test_driver(false);
        let runs = counting_worker(&d, 1, 4, "test").await;

        let (d1, d2) = (d.clone(), d.clone());
        let (a, b) = (dig('a'), dig('b'));
        let h1 = tokio::spawn(async move { d1.execute(&a).await });
        let h2 = tokio::spawn(async move { d2.execute(&b).await });
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();

        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }

    /// The join barrier is a latch: once the pool has formed, losing a
    /// worker mid-run must NOT re-arm it (a CI fleet cannot refill — every
    /// new execute() would block until the job timeout).
    #[tokio::test]
    async fn pool_barrier_latches_once_formed() {
        let d = test_driver_min(false, 2);
        fake_worker(&d, 1, 2, "test").await;
        fake_worker(&d, 2, 2, "test").await;
        d.await_pool_formed().await; // forms at 2/2
        d.workers.lock().await.retain(|w| w.id != 1); // straggler dies
        tokio::time::timeout(std::time::Duration::from_millis(200), d.await_pool_formed())
            .await
            .expect("barrier re-armed after worker loss - latch regressed");
    }

    /// Actions sharing an input root must execute on the SAME worker: a
    /// crate's pipelined metadata compile and its rlib compile share one
    /// input root, and splitting the pair across machines diverged their
    /// crate hashes (E0460 at every downstream link).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn affinity_pins_same_input_root_to_one_worker() {
        let d = test_driver(false);
        let log: Arc<std::sync::Mutex<Vec<(u64, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        for wid in [1u64, 2] {
            let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
            let inflight = Arc::new(AtomicU32::new(0));
            let d2 = d.clone();
            let log2 = log.clone();
            let inf = inflight.clone();
            tokio::spawn(async move {
                while let Some(D2W::Run { job, action }) = rx.recv().await {
                    log2.lock().unwrap().push((wid, action.hash.clone()));
                    let d3 = d2.clone();
                    let inf = inf.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        inf.fetch_sub(1, Ordering::Relaxed);
                        d3.complete(job, Ok(re::ActionResult::default().encode_to_vec()))
                            .await;
                        d3.pump().await;
                    });
                }
            });
            d.workers.lock().await.push(WorkerConn {
                id: wid,
                tx,
                inflight,
                slots: 1,
                os: "test".into(),
                arch: "test_arch".into(),
                endpoint: String::new(),
                preloaded_shard: None,
            });
        }

        // Two distinct actions (different salts) sharing one input root.
        let root = re::Digest {
            hash: "c".repeat(64),
            size_bytes: 1,
        };
        let mut digs = Vec::new();
        for salt in 0u8..12 {
            let action = re::Action {
                input_root_digest: Some(root.clone()),
                salt: vec![salt],
                ..Default::default()
            };
            let dig = d.store.put(None, &action.encode_to_vec()).await.unwrap();
            digs.push(dig);
        }
        let runs: Vec<_> = digs
            .iter()
            .map(|dig| {
                let d = d.clone();
                let dig = dig.clone();
                tokio::spawn(async move { d.execute(&dig).await })
            })
            .collect();
        for r in runs {
            r.await.unwrap().expect("job must complete");
        }
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 12, "all actions executed: {log:?}");
        let owners: std::collections::HashSet<u64> = log.iter().map(|(w, _)| *w).collect();
        assert_eq!(
            owners.len(),
            1,
            "same input root must pin to one worker: {log:?}"
        );
    }

    /// The provider index is a routing HINT, not a presence oracle: a
    /// worker's 10GB LRU can evict a blob minutes after announcing it.
    /// healing4/5: has_blobs testified on the bare index entry, exec-time
    /// get_blob then trusted the same stale entry and turned one peer's
    /// "Missing" into a hard action failure - 3,650 times per lap.
    #[tokio::test]
    async fn stale_provider_entry_is_a_hint_not_truth() {
        let d = test_driver(false);
        let dig = Dig {
            hash: "ab".repeat(32),
            size: 3,
        };
        d.providers
            .lock()
            .await
            .insert(dig.hash.clone(), "unreachable-peer".into());
        // Validation must verify the entry (unreachable peer = unproven)
        // and evict the failed hint so rediscovery stays honest.
        assert_eq!(d.has_blobs(std::slice::from_ref(&dig)).await, vec![false]);
        assert!(!d.providers.lock().await.contains_key(&dig.hash));
        // The serve path must classify a claimed-but-unfetchable blob as an
        // INFRA error (retryable at the job layer), never Ok(None): reader
        // 29007342337 lost 3,212 actions to transient fetch failures being
        // reported as Missing. The hint is kept - it may recover.
        d.providers
            .lock()
            .await
            .insert(dig.hash.clone(), "unreachable-peer".into());
        assert!(d.get_blob(&dig).await.is_err());
        assert!(d.providers.lock().await.contains_key(&dig.hash));
    }

    /// Shallow validation proved the Tree PROTO exists, not its contents:
    /// reader 29010597531 lost 5,390 actions to interior files of
    /// validated directory outputs that no longer existed anywhere.
    #[tokio::test]
    async fn ac_validation_expands_directory_trees() {
        let d = test_driver(false);
        // A tree whose root directory references one file we never store.
        let file_hash = crate::store::sha256_hex(b"1234567");
        let tree = re::Tree {
            root: Some(re::Directory {
                files: vec![re::FileNode {
                    name: "gone.rlib".into(),
                    digest: Some(re::Digest {
                        hash: file_hash.clone(),
                        size_bytes: 7,
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            children: vec![],
        };
        let tree_dig = d.store.put(None, &tree.encode_to_vec()).await.unwrap();
        let result = re::ActionResult {
            output_directories: vec![re::OutputDirectory {
                path: "outdir".into(),
                tree_digest: Some(tree_dig.to_proto()),
                is_topologically_sorted: false,
                root_directory_digest: None,
            }],
            ..Default::default()
        };
        let key = "b".repeat(64);
        d.store.ac_put(&key, &result.encode_to_vec()).await.unwrap();
        assert!(
            matches!(d.validated_ac_get(&key).await, AcLookup::Unservable),
            "tree proto present but interior file absent must be Unservable"
        );
        // Store the interior file; the unservable verdict is memoized
        // until the entry is rewritten (the real-world invalidation:
        // re-execution re-puts the result), then it becomes servable.
        let f = Dig {
            hash: file_hash,
            size: 7,
        };
        d.store.put(Some(&f), b"1234567").await.unwrap();
        d.note_ac_written(&key).await;
        assert!(matches!(d.validated_ac_get(&key).await, AcLookup::Hit(_)));
    }

    /// Finalize is preload-sticky: a worker packs the range its store is
    /// rich in, and preload-less workers (the driver's co-worker) are
    /// never assigned - nothing packs their store, so an assignment there
    /// silently loses the shard (the eternally-absent cas-shard-0).
    #[tokio::test]
    async fn finalize_is_preload_sticky_and_skips_ineligible() {
        let d = test_driver(false);
        let mut rxs = Vec::new();
        for (id, preload) in [(1u64, None), (2, Some(2u8)), (3, Some(0u8))] {
            let (tx, rx) = mpsc::unbounded_channel::<D2W>();
            d.workers.lock().await.push(WorkerConn {
                id,
                tx,
                inflight: Arc::new(AtomicU32::new(0)),
                slots: 1,
                os: "linux".into(),
                arch: "test_arch".into(),
                endpoint: String::new(),
                preloaded_shard: preload,
            });
            rxs.push((id, rx));
        }
        // 3 shards, 2 eligible workers: sticky shards 0 and 2 assigned,
        // shard 1 has no unassigned eligible worker and is dropped.
        assert_eq!(d.finalize_shards(3).await, 2);
        for (id, rx) in &mut rxs {
            let mut got = Vec::new();
            while let Ok(msg) = rx.try_recv() {
                if let D2W::Finalize { shard, .. } = msg {
                    got.push(shard);
                }
            }
            match id {
                1 => assert!(got.is_empty(), "co-worker must not be assigned"),
                2 => assert_eq!(got, vec![2], "sticky to its preload"),
                3 => assert_eq!(got, vec![0], "sticky to its preload"),
                _ => unreachable!(),
            }
        }
    }

    /// Validation verdicts are session-memoized: ~59k lookups over ~20k
    /// unique entries per lap, each costing tree fetches + fleet HasMany.
    /// Servable memos survive blob loss until a worker disconnects (the
    /// invalidation event); unservable memos expire on TTL/rewrite.
    #[tokio::test]
    async fn validation_verdicts_are_memoized_per_session() {
        let d = test_driver(false);
        let blob = d.store.put(None, b"abc").await.unwrap();
        let result = re::ActionResult {
            output_files: vec![re::OutputFile {
                path: "out".into(),
                digest: Some(blob.to_proto()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let key = "c".repeat(64);
        d.store.ac_put(&key, &result.encode_to_vec()).await.unwrap();
        assert!(matches!(d.validated_ac_get(&key).await, AcLookup::Hit(_)));
        // Remove the blob behind the memo: still a Hit (memoized verdict),
        // proving the second lookup skipped revalidation.
        let p = d.store.cas_path_for_test(&blob.hash);
        std::fs::remove_file(&p).unwrap();
        assert!(matches!(d.validated_ac_get(&key).await, AcLookup::Hit(_)));
        // Worker-disconnect invalidation: verdicts revalidate -> Unservable.
        d.memo_servable.write().await.clear();
        assert!(matches!(
            d.validated_ac_get(&key).await,
            AcLookup::Unservable
        ));
        // ...and the unservable verdict is memoized until the entry is
        // rewritten (note_ac_written), after which blobs restored = Hit.
        d.store.put(Some(&blob), b"abc").await.unwrap();
        assert!(matches!(
            d.validated_ac_get(&key).await,
            AcLookup::Unservable
        ));
        d.note_ac_written(&key).await;
        assert!(matches!(d.validated_ac_get(&key).await, AcLookup::Hit(_)));
    }

    /// Locality routing: a job whose heaviest input a worker already
    /// holds (per its bloom) is dispatched to THAT worker - moving the
    /// task to the data instead of GiBs of rlibs to the task.
    #[tokio::test]
    async fn locality_prefers_the_worker_holding_the_inputs() {
        let d = test_driver_with(|cfg| cfg.locality = true);
        // Input tree: one 1MB file. Worker B's bloom claims it; A's doesn't.
        let file_hash = crate::store::sha256_hex(b"big-rlib-bytes");
        let dir = re::Directory {
            files: vec![re::FileNode {
                name: "libbig.rlib".into(),
                digest: Some(re::Digest {
                    hash: file_hash.clone(),
                    size_bytes: 1_000_000,
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let dir_dig = d.store.put(None, &dir.encode_to_vec()).await.unwrap();
        let cmd = re::Command {
            arguments: vec!["true".into()],
            ..Default::default()
        };
        let cmd_dig = d.store.put(None, &cmd.encode_to_vec()).await.unwrap();
        let action = re::Action {
            command_digest: Some(cmd_dig.to_proto()),
            input_root_digest: Some(dir_dig.to_proto()),
            ..Default::default()
        };
        let action_dig = d.store.put(None, &action.encode_to_vec()).await.unwrap();

        let mut bloom_b = crate::mesh::Bloom::with_capacity(64);
        bloom_b.insert(&file_hash);
        d.blooms.lock().await.insert("epB".into(), bloom_b);

        let ran = Arc::new(Mutex::new(Vec::<&str>::new()));
        for (id, ep) in [(1u64, "epA"), (2, "epB")] {
            let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
            let inflight = Arc::new(AtomicU32::new(0));
            let d2 = Arc::clone(&d);
            let inf = inflight.clone();
            let ran2 = ran.clone();
            tokio::spawn(async move {
                while let Some(D2W::Run { job, .. }) = rx.recv().await {
                    ran2.lock().await.push(ep);
                    inf.fetch_sub(1, Ordering::Relaxed);
                    d2.complete(job, Ok(re::ActionResult::default().encode_to_vec()))
                        .await;
                    d2.pump().await;
                }
            });
            d.workers.lock().await.push(WorkerConn {
                id,
                tx,
                inflight,
                slots: 4,
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                endpoint: ep.into(),
                preloaded_shard: None,
            });
        }
        d.execute(&action_dig).await.unwrap();
        assert_eq!(*ran.lock().await, vec!["epB"], "job must go to the data");
    }

    /// scrub_ac deletes only unservable rows, keeps servable ones.
    #[tokio::test]
    async fn scrub_ac_deletes_only_dead_rows() {
        let d = test_driver(false);
        // Servable: references a blob present in the store.
        let blob = d.store.put(None, b"live").await.unwrap();
        let live = re::ActionResult {
            output_files: vec![re::OutputFile {
                path: "o".into(),
                digest: Some(blob.to_proto()),
                ..Default::default()
            }],
            ..Default::default()
        };
        d.store
            .ac_put(&"a".repeat(64), &live.encode_to_vec())
            .await
            .unwrap();
        // Dead: references a blob nobody holds.
        let dead = re::ActionResult {
            output_files: vec![re::OutputFile {
                path: "o".into(),
                digest: Some(re::Digest {
                    hash: "ff".repeat(32),
                    size_bytes: 9,
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        d.store
            .ac_put(&"b".repeat(64), &dead.encode_to_vec())
            .await
            .unwrap();

        let (scanned, deleted) = d.scrub_ac().await;
        assert_eq!(scanned, 2);
        assert_eq!(deleted, 1);
        assert!(d.store.ac_get(&"a".repeat(64)).await.is_some(), "live kept");
        assert!(d.store.ac_get(&"b".repeat(64)).await.is_none(), "dead gone");
    }

    /// Pull-model invariant: a worker's outstanding count never exceeds
    /// slots + max prefetch, and every job completes exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pump_bounds_outstanding_and_completes_all() {
        let d = test_driver(false);
        let inf1 = fake_worker(&d, 1, 2, "test").await;
        let inf2 = fake_worker(&d, 2, 2, "test").await;

        let watchdog = {
            let (inf1, inf2) = (inf1.clone(), inf2.clone());
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let s2 = stop.clone();
            let h = tokio::spawn(async move {
                let mut max = 0;
                while !s2.load(Ordering::Relaxed) {
                    max = max
                        .max(inf1.load(Ordering::Relaxed))
                        .max(inf2.load(Ordering::Relaxed));
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                max
            });
            (stop, h)
        };

        let runs: Vec<_> = (0..100)
            .map(|i| {
                let d = d.clone();
                tokio::spawn(async move {
                    d.execute(&Dig {
                        hash: format!("{i:064}"),
                        size: 1,
                    })
                    .await
                })
            })
            .collect();
        for r in runs {
            // Actions "succeed" with an empty result; do_not_cache lookup
            // tolerates the digest being absent from the store.
            r.await.unwrap().expect("job must complete");
        }
        watchdog.0.store(true, Ordering::Relaxed);
        let max_seen = watchdog.1.await.unwrap();
        assert!(
            max_seen <= 2 + 16,
            "outstanding exceeded slots+max_prefetch: {max_seen}"
        );
    }

    /// Like fake_worker, but records every action hash it runs.
    async fn fake_worker_logged(
        d: &Arc<Driver>,
        id: u64,
        slots: u32,
        os: &str,
        log: Arc<Mutex<Vec<(u64, String)>>>,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel::<D2W>();
        let inflight = Arc::new(AtomicU32::new(0));
        let d2 = d.clone();
        let inf = inflight.clone();
        tokio::spawn(async move {
            while let Some(D2W::Run { job, action }) = rx.recv().await {
                log.lock().await.push((id, action.hash.clone()));
                let d3 = d2.clone();
                let inf = inf.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    inf.fetch_sub(1, Ordering::Relaxed);
                    d3.complete(job, Ok(re::ActionResult::default().encode_to_vec()))
                        .await;
                    d3.pump().await;
                });
            }
        });
        d.workers.lock().await.push(WorkerConn {
            id,
            tx,
            inflight,
            slots,
            os: os.into(),
            arch: "test_arch".into(),
            endpoint: String::new(),
            preloaded_shard: None,
        });
    }

    /// Store an Action demanding `os` and return its digest.
    /// `salt` keeps otherwise-identical actions distinct — REAPI's own field
    /// for the purpose. Without it every action for a given os hashes the
    /// same, and single-flight (correctly) collapses them into one execution,
    /// which is not what a routing test means to measure.
    async fn platformed_action(d: &Arc<Driver>, os: &str, salt: u32) -> Dig {
        let action = re::Action {
            platform: Some(re::Platform {
                properties: vec![re::platform::Property {
                    name: "OSFamily".into(),
                    value: os.into(),
                }],
            }),
            salt: salt.to_le_bytes().to_vec(),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        action.encode(&mut bytes).unwrap();
        d.store.put(None, &bytes).await.unwrap()
    }

    /// Platform routing: os-tagged jobs only land on matching workers;
    /// untagged jobs land anywhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn jobs_route_to_matching_platform_workers() {
        let d = test_driver(false);
        let log = Arc::new(Mutex::new(Vec::new()));
        fake_worker_logged(&d, 1, 4, "windows", log.clone()).await;
        fake_worker_logged(&d, 2, 4, "macos", log.clone()).await;

        let mut runs = Vec::new();
        let mut want: HashMap<String, &str> = HashMap::new();
        for i in 0..20u32 {
            let os = if i % 2 == 0 { "windows" } else { "macos" };
            let dig = platformed_action(&d, os, i).await;
            want.insert(dig.hash.clone(), os);
            let d2 = d.clone();
            runs.push(tokio::spawn(async move { d2.execute(&dig).await }));
        }
        for r in runs {
            r.await.unwrap().expect("job must complete");
        }

        let log = log.lock().await;
        assert_eq!(log.len(), 20, "every job dispatched exactly once");
        for (worker, hash) in log.iter() {
            let expect = match want[hash] {
                "windows" => 1,
                _ => 2,
            };
            assert_eq!(
                *worker, expect,
                "action for {} landed on worker {worker}",
                want[hash]
            );
        }
    }
}
