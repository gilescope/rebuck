//! Worker: join the mesh, pull jobs off the control stream, execute, push
//! outputs back. Blob reads check the local store first — inputs shared
//! across actions (toolchains, common deps) transfer once per worker.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use iroh::endpoint::Connection;
use iroh::Endpoint;
use prost::Message;
use tokio::sync::{Mutex, Semaphore};

use crate::exec;
use crate::mesh::{self, BlobReq, BlobResp, Dig, D2W, W2D};
use crate::store::Store;

pub struct WorkerCfg {
    pub session: String,
    pub slots: usize,
    pub scratch: std::path::PathBuf,
    pub connect_wait: Duration,
}

pub async fn run(store: Arc<Store>, cfg: WorkerCfg) -> Result<()> {
    let ep = Endpoint::builder(iroh::endpoint::presets::N0)
        .bind()
        .await?;
    let target = mesh::driver_id(&cfg.session);
    println!(
        "[worker] endpoint_id={} driver={target} session={}",
        ep.id(),
        cfg.session
    );

    let conn = {
        let deadline = Instant::now() + cfg.connect_wait;
        loop {
            match ep.connect(target, mesh::ALPN).await {
                Ok(c) => break c,
                Err(e) if Instant::now() < deadline => {
                    println!("[worker] connect retry: {e}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Err(e) => return Err(e).context("driver never became reachable"),
            }
        }
    };
    println!("[worker] connected");

    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    mesh::send_frame(
        &mut ctrl_send,
        &W2D::Hello {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            slots: cfg.slots as u32,
        },
    )
    .await?;

    let ctrl_send = Arc::new(Mutex::new(ctrl_send));
    let slots = Arc::new(Semaphore::new(cfg.slots));
    let blobs = Arc::new(RemoteBlobs {
        conn: conn.clone(),
        store: store.clone(),
    });

    loop {
        let Some(msg) = mesh::recv_frame::<D2W>(&mut ctrl_recv).await? else {
            println!("[worker] driver closed control stream — done");
            return Ok(());
        };
        let D2W::Run { job, action } = msg;
        let blobs = blobs.clone();
        let ctrl = ctrl_send.clone();
        let scratch = cfg.scratch.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let _permit = slots.acquire_owned().await.expect("semaphore open");
            let reply = match exec::run_action(blobs.as_ref(), &action, &scratch).await {
                Ok(outcome) => W2D::Done {
                    job,
                    action_result: outcome.action_result.encode_to_vec(),
                },
                Err(e) => W2D::Failed {
                    job,
                    msg: format!("{e:#}"),
                },
            };
            if let Err(e) = mesh::send_frame(&mut *ctrl.lock().await, &reply).await {
                eprintln!("[worker] failed to send result for job {job}: {e:#}");
            }
        });
    }
}

/// Blobs fetched from / pushed to the driver, with local store as cache.
struct RemoteBlobs {
    conn: Connection,
    store: Arc<Store>,
}

#[async_trait::async_trait]
impl exec::Blobs for RemoteBlobs {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
        if let Some(bytes) = self.store.get(d).await? {
            return Ok(bytes);
        }
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Get(d.clone())).await?;
        send.finish()?;
        let resp: BlobResp = mesh::recv_frame(&mut recv)
            .await?
            .context("driver closed blob stream")?;
        match resp {
            BlobResp::Found { size } => {
                let bytes = mesh::recv_raw(&mut recv, size).await?;
                self.store.put(Some(d), &bytes).await?;
                Ok(bytes)
            }
            BlobResp::Missing => bail!("driver CAS missing blob {}/{}", d.hash, d.size),
            other => bail!("unexpected blob response: {other:?}"),
        }
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Dig> {
        let d = self.store.put(None, &bytes).await?;
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Put(d.clone())).await?;
        send.write_all(&bytes).await?;
        send.finish()?;
        let resp: BlobResp = mesh::recv_frame(&mut recv)
            .await?
            .context("driver closed blob stream")?;
        match resp {
            BlobResp::PutOk => Ok(d),
            other => bail!("blob put rejected: {other:?}"),
        }
    }
}
