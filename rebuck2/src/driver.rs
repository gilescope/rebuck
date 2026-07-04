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
    pub scratch: std::path::PathBuf,
}

struct WorkerConn {
    id: u64,
    tx: mpsc::UnboundedSender<D2W>,
    inflight: Arc<AtomicU32>,
    #[allow(dead_code)]
    os: String,
}

/// An action in flight: who to answer, what to run, where it's running.
struct Job {
    tx: oneshot::Sender<Result<re::ActionResult, String>>,
    action: Dig,
    /// Current assignee (worker id; 0 = driver-local).
    worker: u64,
    attempts: u32,
}

const MAX_ATTEMPTS: u32 = 3;

pub struct Driver {
    pub store: Arc<Store>,
    cfg: DriverCfg,
    jobs: Mutex<HashMap<u64, Job>>,
    workers: Mutex<Vec<WorkerConn>>,
    worker_arrived: tokio::sync::Notify,
    next_job: AtomicU64,
    next_worker: AtomicU64,
    local_slots: Semaphore,
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
            os,
        });
        self.worker_arrived.notify_waiters();

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
                    }
                    W2D::Failed { job, msg } => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        self.complete(job, Err(msg)).await;
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
            for id in orphans {
                self.dispatch(id).await;
            }
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

    /// Assign a queued job to the least-loaded worker, or the driver-local
    /// executor when there are no workers, or fail it out of attempts/options.
    async fn dispatch(self: &Arc<Self>, job_id: u64) {
        let workers = self.workers.lock().await;
        let mut jobs = self.jobs.lock().await;
        let Some(job) = jobs.get_mut(&job_id) else {
            return; // completed while we raced
        };
        if job.attempts >= MAX_ATTEMPTS {
            let job = jobs.remove(&job_id).expect("just found it");
            let _ = job
                .tx
                .send(Err(format!("gave up after {MAX_ATTEMPTS} attempts")));
            return;
        }
        job.attempts += 1;
        if let Some(w) = workers
            .iter()
            .min_by_key(|w| w.inflight.load(Ordering::Relaxed))
        {
            job.worker = w.id;
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
                .is_ok()
            {
                return;
            }
            // Channel already closed — the disconnect path will requeue us.
            w.inflight.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        if self.cfg.local_exec {
            job.worker = 0;
            let action = job.action.clone();
            drop(jobs);
            drop(workers);
            println!("[driver] job {job_id} -> local ({})", action.hash);
            let this = self.clone();
            tokio::spawn(async move {
                let _permit = this.local_slots.acquire().await.expect("semaphore open");
                let blobs = StoreBlobs {
                    store: this.store.clone(),
                };
                let result = exec::run_action(&blobs, &action, &this.cfg.scratch)
                    .await
                    .map(|o| o.action_result)
                    .map_err(|e| format!("{e:#}"));
                this.complete(job_id, result).await;
            });
            return;
        }
        let job = jobs.remove(&job_id).expect("just found it");
        let _ = job.tx.send(Err(
            "no workers connected and local execution disabled".into()
        ));
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
            },
        );
        self.dispatch(job_id).await;

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
