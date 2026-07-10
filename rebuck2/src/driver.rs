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
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use iroh::endpoint::Connection;
use iroh::Endpoint;
use prost::Message;
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};

use crate::exec::{self, crate_affinity_key};
use crate::mesh::{self, BlobReq, BlobResp, Dig, D2W, W2D};
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
    /// Write the driver's full EndpointAddr (id + relay) here once bound.
    /// CI publishes it as a run artifact so workers can dial directly -
    /// n0 discovery becomes a fallback instead of a single point of
    /// failure (observed: regional discovery outages stranding workers
    /// for their whole 30-minute window).
    pub addr_file: Option<std::path::PathBuf>,
    pub scratch: std::path::PathBuf,
}

/// Outcome of a validated AC lookup ([`Driver::validated_ac_get`]).
pub enum AcLookup {
    /// Cached result whose referenced blobs are all fetchable.
    Hit(re::ActionResult),
    /// Entry exists but at least one referenced blob is gone (evicted CAS):
    /// callers must report a miss so the client re-executes and re-uploads.
    Unservable,
    Miss,
}

/// Every CAS digest a cached result commits the server to delivering.
/// Zero-size blobs are implicit in RE and skipped.
pub fn result_digests(r: &re::ActionResult) -> Vec<Dig> {
    let mut digs = Vec::new();
    let mut push = |d: &Option<re::Digest>| {
        if let Some(d) = d {
            if d.size_bytes > 0 {
                digs.push(Dig {
                    hash: d.hash.clone(),
                    size: d.size_bytes,
                });
            }
        }
    };
    for f in &r.output_files {
        push(&f.digest);
    }
    for t in &r.output_directories {
        push(&t.tree_digest);
    }
    push(&r.stdout_digest);
    push(&r.stderr_digest);
    digs
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

/// What platform an action demands, from its REAPI platform properties.
/// Empty string = no constraint on that axis (matches any worker).
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

    /// Parse from REAPI platform properties (Command.platform /
    /// Action.platform). Recognised keys (case-insensitive): OSFamily/os,
    /// Arch/arch. Values are matched verbatim against the worker's
    /// std::env::consts OS/ARCH strings ("windows"/"linux"/"macos",
    /// "x86_64"/"aarch64").
    fn from_properties(platform: Option<&re::Platform>) -> PlatKey {
        let mut key = PlatKey::default();
        if let Some(p) = platform {
            for prop in &p.properties {
                match prop.name.to_ascii_lowercase().as_str() {
                    "osfamily" | "os" => key.os = prop.value.to_ascii_lowercase(),
                    "arch" | "architecture" => key.arch = prop.value.to_ascii_lowercase(),
                    _ => {}
                }
            }
        }
        key
    }
}

/// An action in flight: who to answer, what to run, where it's running.
struct Job {
    tx: oneshot::Sender<Result<re::ActionResult, String>>,
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
}

const MAX_ATTEMPTS: u32 = 3;
/// A running job must be at least this old before the tail races it on a
/// second worker — otherwise every action in a small build runs twice.
const SPECULATE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Driver {
    pub store: Arc<Store>,
    cfg: DriverCfg,
    jobs: Mutex<HashMap<u64, Job>>,
    workers: Mutex<Vec<WorkerConn>>,
    worker_arrived: tokio::sync::Notify,
    /// Latched once the pool first reaches `min_workers` — the barrier must
    /// not re-arm when workers are lost mid-run (a CI fleet cannot refill;
    /// re-blocking would hang the build until the job timeout).
    pool_formed: std::sync::atomic::AtomicBool,
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
    memo_servable: Mutex<std::collections::HashSet<String>>,
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
    /// Workers that completed their post-build shard sync.
    pub finalized: AtomicU64,
    pub ac_hit_ok: AtomicU64,
    pub ac_hit_fail: AtomicU64,
    pub dnc_exec: AtomicU64,
    /// Mesh endpoint, for read-through fetches from providers.
    mesh_ep: tokio::sync::OnceCell<Endpoint>,
}

impl Driver {
    pub fn new(store: Arc<Store>, cfg: DriverCfg) -> Arc<Self> {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Arc::new(Self {
            store,
            cfg,
            jobs: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            worker_arrived: tokio::sync::Notify::new(),
            pool_formed: std::sync::atomic::AtomicBool::new(false),
            next_job: AtomicU64::new(1),
            next_worker: AtomicU64::new(1),
            local_slots: Semaphore::new(cores),
            queue: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
            unservable_logged: AtomicU64::new(0),
            memo_servable: Mutex::new(std::collections::HashSet::new()),
            memo_unservable: Mutex::new(HashMap::new()),
            blooms: Mutex::new(HashMap::new()),
            peer_conns: Mutex::new(HashMap::new()),
            affinity_owner: Mutex::new(HashMap::new()),
            mesh_fetches: Semaphore::new(64),
            finalized: AtomicU64::new(0),
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
                        let told = this.finalize_shards(8).await as u64;
                        println!("[driver] finalize signalled: told {told} workers");
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(900);
                        while this.finalized_count() < told && std::time::Instant::now() < deadline
                        {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        // APPEND .done - with_extension REPLACES the last
                        // extension ("finalize.signal" -> "finalize.done")
                        // and the CI poll for finalize.signal.done burned
                        // its full 1000s cap on EVERY lap (~16.7min/lap,
                        // in every decomposition as "finalize 16m40s").
                        let done = std::path::PathBuf::from(format!("{}.done", sig.display()));
                        let _ =
                            tokio::fs::write(&done, format!("{}", this.finalized_count())).await;
                        println!(
                            "[driver] finalize complete: {}/{told} workers",
                            this.finalized_count()
                        );
                        return;
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
        // Control stream is the first bi-stream the worker opens.
        let (ctrl_send, mut ctrl_recv) = conn.accept_bi().await?;
        let hello: W2D = mesh::recv_frame(&mut ctrl_recv)
            .await?
            .context("worker hung up before Hello")?;
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
        let blobs = tokio::spawn(async move {
            while let Ok((send, recv)) = blob_conn.accept_bi().await {
                let driver = blob_driver.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_blob_stream(driver, send, recv).await {
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
                        let result = re::ActionResult::decode(action_result.as_slice())
                            .map_err(|e| format!("bad ActionResult from worker: {e}"));
                        self.complete(job, result).await;
                        self.pump().await;
                    }
                    W2D::Failed { job, msg } => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        self.complete(job, Err(msg)).await;
                        self.pump().await;
                    }
                    W2D::Finalized { shard } => {
                        println!("[driver] worker {worker_id} finalized shard {shard}");
                        self.finalized.fetch_add(1, Ordering::Relaxed);
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
                    W2D::Hello { .. } => bail!("unexpected second Hello"),
                }
            }
        }
        .await;

        self.workers.lock().await.retain(|w| w.id != worker_id);
        // A departed worker may have been the sole holder behind memoized
        // servable verdicts - revalidate everything from here on.
        self.memo_servable.lock().await.clear();
        self.blooms.lock().await.remove(&endpoint);
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

    /// Blob presence, counting provider-indexed blobs as present.
    pub async fn has_blob(&self, d: &Dig) -> bool {
        if self.store.has(d).await {
            return true;
        }
        self.providers.lock().await.contains_key(&d.hash)
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
        for (peer, idxs) in by_peer {
            let batch: Vec<Dig> = idxs.iter().map(|&i| digs[i].clone()).collect();
            let _permit = self.mesh_fetches.acquire().await;
            let confirmed = match self.peer_request(&peer, &BlobReq::HasMany(batch)).await {
                Ok(BlobResp::HaveMany(v)) => Some(v),
                _ => None,
            };
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
        let Some(bytes) = self.store.ac_get(hash).await else {
            return AcLookup::Miss;
        };
        if self.memo_servable.lock().await.contains(hash) {
            if let Ok(result) = re::ActionResult::decode(bytes.as_slice()) {
                return AcLookup::Hit(result);
            }
        }
        if let Some(at) = self.memo_unservable.lock().await.get(hash) {
            if at.elapsed() < std::time::Duration::from_secs(120) {
                return AcLookup::Unservable;
            }
        }
        let Ok(result) = re::ActionResult::decode(bytes.as_slice()) else {
            // Corrupt entry: re-execution overwrites it. Safer than serving.
            return AcLookup::Miss;
        };
        let mut digs = result_digests(&result);
        // Top-level digests prove a directory output's Tree PROTO exists,
        // not its contents: reader 29010597531 lost 5,390 actions to
        // interior files of validated directory outputs that existed
        // nowhere. Expand each tree (small, cached after first fetch) and
        // demand its files and child Directory protos too.
        for od in &result.output_directories {
            let Some(td) = &od.tree_digest else { continue };
            let tdig: Dig = td.into();
            let Ok(Some(tree_bytes)) = self.get_blob(&tdig).await else {
                self.memo_unservable
                    .lock()
                    .await
                    .insert(hash.to_string(), std::time::Instant::now());
                return AcLookup::Unservable;
            };
            let Ok(tree) = re::Tree::decode(tree_bytes.as_slice()) else {
                self.memo_unservable
                    .lock()
                    .await
                    .insert(hash.to_string(), std::time::Instant::now());
                return AcLookup::Unservable;
            };
            for dir in tree.root.iter().chain(tree.children.iter()) {
                for f in &dir.files {
                    if let Some(d) = &f.digest {
                        if d.size_bytes > 0 {
                            digs.push(d.into());
                        }
                    }
                }
            }
            // Child Directory protos are separate CAS blobs referenced by
            // digest during materialization; their digests are computable
            // locally from the embedded copies.
            for child in &tree.children {
                let enc = child.encode_to_vec();
                if !enc.is_empty() {
                    digs.push(Dig {
                        hash: crate::store::sha256_hex(&enc),
                        size: enc.len() as i64,
                    });
                }
            }
        }
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
                        "[driver] unservable sample {n}: action {hash} missing blob {}/{} ({} outputs, {} dirs)",
                        digs[i].hash,
                        digs[i].size,
                        result.output_files.len(),
                        result.output_directories.len()
                    );
                }
                self.memo_unservable
                    .lock()
                    .await
                    .insert(hash.to_string(), std::time::Instant::now());
                return AcLookup::Unservable;
            }
        }
        self.memo_servable.lock().await.insert(hash.to_string());
        AcLookup::Hit(result)
    }

    /// A fresh result was written for this key: any cached unservable
    /// verdict is obsolete.
    pub async fn note_ac_written(&self, hash: &str) {
        self.memo_unservable.lock().await.remove(hash);
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
        if let Some(bytes) = self.store.get(d).await? {
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
                        self.store.put(Some(d), &bytes).await?;
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

    pub async fn worker_count(&self) -> usize {
        self.workers.lock().await.len()
    }

    /// Post-build: assign snapshot shards 0..of round-robin across the
    /// connected fleet and tell each worker to sync + save its shard.
    /// Returns how many workers were told (each shard covered when the
    /// fleet is >= `of`; extras double up for redundancy).
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
        let mut n = 0;
        for (i, wid) in assigned.iter().enumerate() {
            let Some(wid) = wid else {
                println!("[driver] finalize: no eligible worker for shard {i} - previous artifact stands");
                continue;
            };
            if let Some(w) = workers.iter().find(|w| w.id == *wid) {
                let _ = w.tx.send(D2W::Finalize { shard: i as u8, of });
                n += 1;
            }
        }
        n
    }

    pub fn finalized_count(&self) -> u64 {
        self.finalized.load(Ordering::Relaxed)
    }

    /// Resolve a job's oneshot and drop it from the table.
    async fn complete(&self, job_id: u64, result: Result<re::ActionResult, String>) {
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
                    let result = exec::run_action(&blobs, &action, &this.cfg.scratch)
                        .await
                        .map(|o| o.action_result)
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
                                match jobs.get(id).and_then(|j| j.affinity) {
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

    /// Execute an action: queue it, dispatch (worker or local), await the result.
    pub async fn execute(self: &Arc<Self>, action_digest: &Dig) -> Result<exec::Outcome> {
        self.await_pool_formed().await;

        // Route by the action's demanded platform (REAPI platform
        // properties live on the Command; Action.platform is the newer
        // spot — honour both, Command winning only if Action has none).
        // get_blob, not store.get: with an AC-only-seeded driver the
        // action/command blobs live on worker shards; a local-only read
        // silently degraded routing to PlatKey::default() and put /bin/sh
        // actions on windows workers (reader 28957851178).
        let (plat, do_not_cache, affinity) = match self.get_blob(action_digest).await? {
            Some(bytes) => match re::Action::decode(bytes.as_slice()) {
                Ok(action) => {
                    let mut plat = PlatKey::from_properties(action.platform.as_ref());
                    let mut affinity_key: Option<String> = None;
                    if let Some(cd) = &action.command_digest {
                        if let Ok(Some(cmd_bytes)) = self
                            .get_blob(&Dig {
                                hash: cd.hash.clone(),
                                size: cd.size_bytes,
                            })
                            .await
                        {
                            if let Ok(cmd) = re::Command::decode(cmd_bytes.as_slice()) {
                                if plat == PlatKey::default() {
                                    plat = PlatKey::from_properties(cmd.platform.as_ref());
                                }
                                affinity_key = crate_affinity_key(&cmd);
                            }
                        }
                    }
                    // Fall back to the input root when no crate prefix is
                    // recognisable — same-input actions still colocate.
                    let affinity = affinity_key
                        .or_else(|| action.input_root_digest.as_ref().map(|d| d.hash.clone()))
                        .map(|key| {
                            use std::hash::{Hash, Hasher};
                            let mut h = std::collections::hash_map::DefaultHasher::new();
                            key.hash(&mut h);
                            h.finish()
                        });
                    (plat, action.do_not_cache, affinity)
                }
                Err(_) => (PlatKey::default(), false, None),
            },
            None => (PlatKey::default(), false, None),
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
            },
        );
        self.queue
            .lock()
            .await
            .entry(plat)
            .or_default()
            .push_back(job_id);
        self.pump().await;

        let action_result = rx
            .await
            .context("job dropped without completion")?
            .map_err(|e| anyhow::anyhow!("execution failed: {e}"))?;

        Ok(exec::Outcome {
            action_result,
            do_not_cache,
        })
    }
}

/// Blobs backed directly by the driver's store (local fallback execution).
pub struct StoreBlobs {
    pub store: Arc<Store>,
    pub hardlinks: bool,
}

#[async_trait::async_trait]
impl exec::Blobs for StoreBlobs {
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
                exec::set_exec(dest).await?;
            }
            return Ok(());
        }
        if self.store.link_out(d, dest).await? == crate::store::Materialized::Private
            && is_executable
        {
            exec::set_exec(dest).await?;
        }
        Ok(())
    }
}

async fn serve_blob_stream(
    driver: Arc<Driver>,
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

    fn test_driver(local_exec: bool) -> Arc<Driver> {
        test_driver_min(local_exec, 0)
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
                scratch: std::env::temp_dir(),
            },
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
                    d3.complete(job, Ok(re::ActionResult::default())).await;
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
                        d3.complete(job, Ok(re::ActionResult::default())).await;
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
        assert_eq!(d.has_blobs(&[dig.clone()]).await, vec![false]);
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
        d.memo_servable.lock().await.clear();
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
                    d3.complete(job, Ok(re::ActionResult::default())).await;
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
    async fn platformed_action(d: &Arc<Driver>, os: &str) -> Dig {
        use prost::Message;
        let action = re::Action {
            platform: Some(re::Platform {
                properties: vec![re::platform::Property {
                    name: "OSFamily".into(),
                    value: os.into(),
                }],
            }),
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
            let dig = platformed_action(&d, os).await;
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
