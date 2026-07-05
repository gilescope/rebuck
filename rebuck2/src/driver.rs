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
    pub scratch: std::path::PathBuf,
}

struct WorkerConn {
    id: u64,
    tx: mpsc::UnboundedSender<D2W>,
    /// Jobs sent and not yet answered (running + prefetched).
    inflight: Arc<AtomicU32>,
    slots: u32,
    #[allow(dead_code)]
    os: String,
}

/// An action in flight: who to answer, what to run, where it's running.
struct Job {
    tx: oneshot::Sender<Result<re::ActionResult, String>>,
    action: Dig,
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
    /// Jobs awaiting assignment — workers pull via bounded outstanding
    /// counts rather than having work pinned to them at arrival.
    queue: Mutex<std::collections::VecDeque<u64>>,
    /// Decentralized mode: blob hash -> producing worker's endpoint id.
    providers: Mutex<HashMap<String, String>>,
    /// Bloom gossip: worker endpoint id -> summary of its store.
    blooms: Mutex<HashMap<String, mesh::Bloom>>,
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
            queue: Mutex::new(std::collections::VecDeque::new()),
            providers: Mutex::new(HashMap::new()),
            blooms: Mutex::new(HashMap::new()),
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
                        queue.push_front(*id);
                    }
                }
            }
            self.pump().await;
        }
        read_result
    }

    pub async fn pending_jobs(&self) -> usize {
        self.jobs.lock().await.len()
    }

    /// Blob presence, counting provider-indexed blobs as present.
    pub async fn has_blob(&self, d: &Dig) -> bool {
        if self.store.has(d).await {
            return true;
        }
        self.providers.lock().await.contains_key(&d.hash)
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

        if workers.is_empty() {
            if !self.cfg.local_exec {
                return; // hold everything queued until a worker joins
            }
            while let Some(job_id) = queue.pop_front() {
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

        while !queue.is_empty() {
            let n = (queue.len() / (workers.len() * 4)).clamp(1, 16) as u32;
            let Some(w) = workers
                .iter()
                .filter(|w| w.inflight.load(Ordering::Relaxed) < w.slots + n)
                .min_by_key(|w| w.inflight.load(Ordering::Relaxed))
            else {
                break; // every pipeline full — completions re-pump
            };
            let job_id = queue.pop_front().expect("checked non-empty");
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
                // Dying worker: put the job back; its disconnect path re-pumps.
                w.inflight.fetch_sub(1, Ordering::Relaxed);
                job.worker = 0;
                job.started = None;
                queue.push_front(job_id);
                break;
            }
        }

        // Tail speculation: nothing queued, RUN capacity idle -> race stragglers.
        if queue.is_empty() {
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

        let job_id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.jobs.lock().await.insert(
            job_id,
            Job {
                tx,
                action: action_digest.clone(),
                worker: 0,
                attempts: 0,
                started: None,
                speculated: false,
            },
        );
        self.queue.lock().await.push_back(job_id);
        self.pump().await;

        let action_result = rx
            .await
            .context("job dropped without completion")?
            .map_err(|e| anyhow::anyhow!("execution failed: {e}"))?;

        // Recover do_not_cache from the Action we already hold in CAS.
        let do_not_cache = match self.store.get(action_digest).await? {
            Some(bytes) => re::Action::decode(bytes.as_slice())
                .map(|a| a.do_not_cache)
                .unwrap_or(false),
            None => false,
        };
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
        self.store.link_out(d, dest).await
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
                scratch: std::env::temp_dir(),
            },
        )
    }

    /// Fake worker: drains Runs from its channel, completes them after a
    /// beat, and re-pumps — exactly what handle_worker's reader does.
    async fn fake_worker(d: &Arc<Driver>, id: u64, slots: u32) -> Arc<AtomicU32> {
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
            os: "test".into(),
        });
        handle
    }

    /// Pull-model invariant: a worker's outstanding count never exceeds
    /// slots + max prefetch, and every job completes exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pump_bounds_outstanding_and_completes_all() {
        let d = test_driver(false);
        let inf1 = fake_worker(&d, 1, 2).await;
        let inf2 = fake_worker(&d, 2, 2).await;

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
}
