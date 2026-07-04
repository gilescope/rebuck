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
    pub scratch: std::path::PathBuf,
}

struct WorkerConn {
    id: u64,
    tx: mpsc::UnboundedSender<D2W>,
    inflight: Arc<AtomicU32>,
    #[allow(dead_code)]
    os: String,
}

pub struct Driver {
    pub store: Arc<Store>,
    cfg: DriverCfg,
    jobs: Mutex<HashMap<u64, oneshot::Sender<Result<re::ActionResult, String>>>>,
    workers: Mutex<Vec<WorkerConn>>,
    worker_arrived: tokio::sync::Notify,
    next_job: AtomicU64,
    next_worker: AtomicU64,
    local_slots: Semaphore,
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
        })
    }

    /// Bind the iroh endpoint and accept workers forever.
    pub async fn serve_mesh(self: &Arc<Self>) -> Result<()> {
        let ep = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(mesh::secret(&self.cfg.session, "driver"))
            .alpns(vec![mesh::ALPN.to_vec()])
            .bind()
            .await?;
        println!(
            "[driver] mesh endpoint_id={} session={}",
            ep.id(),
            self.cfg.session
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
        println!("[driver] worker {worker_id} joined: {os}/{arch} slots={slots}");

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
        let mut ctrl_send = ctrl_send;
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if mesh::send_frame(&mut ctrl_send, &msg).await.is_err() {
                    break;
                }
            }
        });

        // blob streams: each request on its own bi-stream
        let blob_conn = conn.clone();
        let blob_store = self.store.clone();
        let blobs = tokio::spawn(async move {
            while let Ok((send, recv)) = blob_conn.accept_bi().await {
                let store = blob_store.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_blob_stream(store, send, recv).await {
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
                    W2D::Done { job, action_result } => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        let result = re::ActionResult::decode(action_result.as_slice())
                            .map_err(|e| format!("bad ActionResult from worker: {e}"));
                        if let Some(tx) = self.jobs.lock().await.remove(&job) {
                            let _ = tx.send(result.map_err(|e| e.to_string()));
                        }
                    }
                    W2D::Failed { job, msg } => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        if let Some(tx) = self.jobs.lock().await.remove(&job) {
                            let _ = tx.send(Err(msg));
                        }
                    }
                    W2D::Hello { .. } => bail!("unexpected second Hello"),
                }
            }
        }
        .await;

        // v0: jobs in flight on a dropped worker fail; rescheduling is roadmap #4.
        self.workers.lock().await.retain(|w| w.id != worker_id);
        println!("[driver] worker {worker_id} left");
        writer.abort();
        blobs.abort();
        read_result
    }

    /// Execute an action: dispatch to a worker, or run locally as fallback.
    pub async fn execute(self: &Arc<Self>, action_digest: &Dig) -> Result<exec::Outcome> {
        // Barrier: don't start work until the agreed pool has formed.
        while self.workers.lock().await.len() < self.cfg.min_workers {
            self.worker_arrived.notified().await;
        }

        let dispatched = {
            let workers = self.workers.lock().await;
            match workers
                .iter()
                .min_by_key(|w| w.inflight.load(Ordering::Relaxed))
            {
                Some(w) => {
                    let job = self.next_job.fetch_add(1, Ordering::Relaxed);
                    let (tx, rx) = oneshot::channel();
                    self.jobs.lock().await.insert(job, tx);
                    w.inflight.fetch_add(1, Ordering::Relaxed);
                    println!(
                        "[driver] job {job} -> worker {} ({})",
                        w.id, action_digest.hash
                    );
                    w.tx.send(D2W::Run {
                        job,
                        action: action_digest.clone(),
                    })
                    .map_err(|_| anyhow::anyhow!("worker channel closed"))?;
                    Some(rx)
                }
                None => None,
            }
        };

        let action_result = match dispatched {
            Some(rx) => rx
                .await
                .context("worker dropped mid-job")?
                .map_err(|e| anyhow::anyhow!("remote execution failed: {e}"))?,
            None if self.cfg.local_exec => {
                let _permit = self.local_slots.acquire().await?;
                let blobs = StoreBlobs {
                    store: self.store.clone(),
                };
                return exec::run_action(&blobs, action_digest, &self.cfg.scratch).await;
            }
            None => bail!("no workers connected and local execution disabled"),
        };

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
    store: Arc<Store>,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let Some(req) = mesh::recv_frame::<BlobReq>(&mut recv).await? else {
        return Ok(());
    };
    match req {
        BlobReq::Get(d) => match store.get(&d).await {
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
        },
        BlobReq::Put(d) => {
            let bytes = mesh::recv_raw(&mut recv, d.size as u64).await?;
            match store.put(Some(&d), &bytes).await {
                Ok(_) => mesh::send_frame(&mut send, &BlobResp::PutOk).await?,
                Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
            }
        }
    }
    send.finish()?;
    Ok(())
}
