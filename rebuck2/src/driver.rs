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

use crate::exec;
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
    /// Write the driver's full EndpointAddr (id + relay) here once bound.
    /// CI publishes it as a run artifact so workers can dial directly -
    /// n0 discovery becomes a fallback instead of a single point of
    /// failure (observed: regional discovery outages stranding workers
    /// for their whole 30-minute window).
    pub addr_file: Option<std::path::PathBuf>,
    pub scratch: std::path::PathBuf,
}

struct WorkerConn {
    id: u64,
    tx: mpsc::UnboundedSender<D2W>,
    /// Jobs sent and not yet answered (running + prefetched).
    inflight: Arc<AtomicU32>,
    slots: u32,
    os: String,
    arch: String,
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
    next_job: AtomicU64,
    next_worker: AtomicU64,
    local_slots: Semaphore,
    /// Jobs awaiting assignment, bucketed by demanded platform — workers
    /// pull from their matching buckets via bounded outstanding counts.
    queue: Mutex<HashMap<PlatKey, std::collections::VecDeque<u64>>>,
    /// Decentralized mode: blob hash -> producing worker's endpoint id.
    providers: Mutex<HashMap<String, String>>,
    /// Bloom gossip: worker endpoint id -> summary of its store.
    blooms: Mutex<HashMap<String, mesh::Bloom>>,
    /// Cache outcome accounting for the stats heartbeat: AC hits that were
    /// successes, AC hits that were cached failures, and executions forced
    /// by do_not_cache actions (the prelude's diag wrappers).
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
            next_job: AtomicU64::new(1),
            next_worker: AtomicU64::new(1),
            local_slots: Semaphore::new(cores),
            queue: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
            blooms: Mutex::new(HashMap::new()),
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
        let W2D::Hello { os, arch, slots } = hello else {
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
    pub async fn has_blobs(self: &Arc<Self>, digs: &[Dig]) -> Vec<bool> {
        let mut have = vec![false; digs.len()];
        let mut unknown: Vec<usize> = Vec::new();
        {
            let providers = self.providers.lock().await;
            for (i, d) in digs.iter().enumerate() {
                if self.store.has(d).await || providers.contains_key(&d.hash) {
                    have[i] = true;
                } else {
                    unknown.push(i);
                }
            }
        }
        if unknown.is_empty() {
            return have;
        }
        // Group the unknowns by the first bloom that claims them.
        let mut by_peer: HashMap<String, Vec<usize>> = HashMap::new();
        {
            let blooms = self.blooms.lock().await;
            for &i in &unknown {
                if let Some((ep, _)) = blooms.iter().find(|(_, b)| b.contains(&digs[i].hash)) {
                    by_peer.entry(ep.clone()).or_default().push(i);
                }
            }
        }
        let Some(ep) = self.mesh_ep.get() else {
            return have;
        };
        for (peer, idxs) in by_peer {
            let batch: Vec<Dig> = idxs.iter().map(|&i| digs[i].clone()).collect();
            let confirmed = async {
                let id: iroh::EndpointId = peer.parse().ok()?;
                let conn = ep.connect(id, mesh::ALPN).await.ok()?;
                let (mut send, mut recv) = conn.open_bi().await.ok()?;
                mesh::send_frame(&mut send, &BlobReq::HasMany(batch))
                    .await
                    .ok()?;
                send.finish().ok()?;
                match mesh::recv_frame::<BlobResp>(&mut recv).await.ok()?? {
                    BlobResp::HaveMany(v) => Some(v),
                    _ => None,
                }
            }
            .await;
            if let Some(v) = confirmed {
                let mut providers = self.providers.lock().await;
                for (k, &i) in idxs.iter().enumerate() {
                    if v.get(k).copied().unwrap_or(false) {
                        have[i] = true;
                        providers.insert(digs[i].hash.clone(), peer.clone());
                    }
                }
            }
        }
        have
    }

    /// Read-through get: local store first, then fetch from the producing
    /// worker and cache. Used by the gRPC surface (buck2's reads).
    pub async fn get_blob(&self, d: &Dig) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.store.get(d).await? {
            return Ok(Some(bytes));
        }
        let endpoint = match self.providers.lock().await.get(&d.hash).cloned() {
            Some(e) => e,
            // Index miss: any peer whose bloom claims the blob (FP -> None).
            None => {
                let blooms = self.blooms.lock().await;
                match blooms
                    .iter()
                    .find(|(_, b)| b.contains(&d.hash))
                    .map(|(e, _)| e.clone())
                {
                    Some(e) => e,
                    None => return Ok(None),
                }
            }
        };
        let Some(ep) = self.mesh_ep.get() else {
            return Ok(None);
        };
        let id: iroh::EndpointId = endpoint
            .parse()
            .map_err(|_| anyhow::anyhow!("bad provider endpoint {endpoint:?}"))?;
        let conn = ep.connect(id, mesh::ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Get(d.clone())).await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("provider closed blob stream")?
        {
            BlobResp::Found { size } => {
                let bytes = mesh::recv_raw(&mut recv, size).await?;
                self.store.put(Some(d), &bytes).await?;
                Ok(Some(bytes))
            }
            BlobResp::Missing => Ok(None),
            other => bail!("provider {endpoint} for {}: {other:?}", d.hash),
        }
    }

    pub async fn worker_count(&self) -> usize {
        self.workers.lock().await.len()
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
        // Each worker drains its matching buckets (most specific first)
        // while it has pipeline headroom. Per-platform buckets mean a full
        // windows pool never blocks an idle mac pool.
        loop {
            let mut assigned_any = false;
            for w in workers.iter() {
                while w.inflight.load(Ordering::Relaxed) < w.slots + n {
                    let Some(job_id) = PlatKey::pull_order(&w.os, &w.arch)
                        .into_iter()
                        .find_map(|k| queue.get_mut(&k).and_then(|q| q.pop_front()))
                    else {
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

    /// Execute an action: queue it, dispatch (worker or local), await the result.
    pub async fn execute(self: &Arc<Self>, action_digest: &Dig) -> Result<exec::Outcome> {
        // Barrier: don't start work until the agreed pool has formed.
        while self.workers.lock().await.len() < self.cfg.min_workers {
            self.worker_arrived.notified().await;
        }

        // Route by the action's demanded platform (REAPI platform
        // properties live on the Command; Action.platform is the newer
        // spot — honour both, Command winning only if Action has none).
        let (plat, do_not_cache) = match self.store.get(action_digest).await? {
            Some(bytes) => match re::Action::decode(bytes.as_slice()) {
                Ok(action) => {
                    let mut plat = PlatKey::from_properties(action.platform.as_ref());
                    if plat == PlatKey::default() {
                        if let Some(cd) = &action.command_digest {
                            if let Ok(Some(cmd_bytes)) = self
                                .store
                                .get(&Dig {
                                    hash: cd.hash.clone(),
                                    size: cd.size_bytes,
                                })
                                .await
                            {
                                if let Ok(cmd) = re::Command::decode(cmd_bytes.as_slice()) {
                                    plat = PlatKey::from_properties(cmd.platform.as_ref());
                                }
                            }
                        }
                    }
                    (plat, action.do_not_cache)
                }
                Err(_) => (PlatKey::default(), false),
            },
            None => (PlatKey::default(), false),
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
        BlobReq::Get(d) => match driver.store.get(&d).await {
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
            Ok(None) => {
                // Decentralized: point the asker at the producer instead of
                // relaying bytes through the driver's NIC.
                let provider = driver.providers.lock().await.get(&d.hash).cloned();
                match provider {
                    Some(endpoint) => {
                        mesh::send_frame(&mut send, &BlobResp::Provider { endpoint }).await?
                    }
                    None => mesh::send_frame(&mut send, &BlobResp::Missing).await?,
                }
            }
            Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
        },
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
    }
    send.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_driver(local_exec: bool) -> Arc<Driver> {
        let dir = tempfile::tempdir().unwrap().keep();
        Driver::new(
            Arc::new(Store::new(dir).unwrap()),
            DriverCfg {
                session: "test".into(),
                min_workers: 0,
                local_exec,
                decentralized: false,
                hardlinks: true,
                addr_file: None,
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
        });
        handle
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
