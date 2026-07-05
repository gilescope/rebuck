//! Worker: join the mesh, pull jobs off the control stream, execute, push
//! outputs back. Blob reads check the local store first — inputs shared
//! across actions (toolchains, common deps) transfer once per worker.
//!
//! Decentralized mode (driver's Welcome says so): outputs stay in the local
//! store instead of uploading; every worker serves `Get`s from its store to
//! any peer, and misses can be redirected to the producing worker
//! (`BlobResp::Provider`). Trade-off: a dead worker takes its blobs with it.

use std::collections::HashMap;
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
    /// Hardlink inputs from the store into exec dirs (default). Off for
    /// filesystems/tools where shared inodes are problematic.
    pub hardlinks: bool,
}

pub async fn run(store: Arc<Store>, cfg: WorkerCfg) -> Result<()> {
    let ep = Endpoint::builder(iroh::endpoint::presets::N0)
        .alpns(vec![mesh::ALPN.to_vec()])
        .bind()
        .await?;
    let target = mesh::driver_id(&cfg.session);
    println!(
        "[worker] endpoint_id={} driver={target} session={}",
        ep.id(),
        cfg.session
    );

    // Serve blobs to any peer (driver read-through, sibling workers).
    {
        let ep = ep.clone();
        let store = store.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let store = store.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    while let Ok((send, recv)) = conn.accept_bi().await {
                        let store = store.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_get(store, send, recv).await {
                                eprintln!("[worker] blob serve error: {e:#}");
                            }
                        });
                    }
                });
            }
        });
    }

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

    // First frame back is the mode handshake.
    let decentralized = match mesh::recv_frame::<D2W>(&mut ctrl_recv).await? {
        Some(D2W::Welcome { decentralized }) => decentralized,
        other => bail!("expected Welcome after Hello, got {other:?}"),
    };
    if decentralized {
        println!("[worker] decentralized CAS: outputs stay local, serving peers");
    }

    let ctrl_send = Arc::new(Mutex::new(ctrl_send));
    let slots = Arc::new(Semaphore::new(cfg.slots));
    let peer_blooms: Arc<Mutex<HashMap<String, mesh::Bloom>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let blobs = Arc::new(RemoteBlobs {
        conn: conn.clone(),
        ep: ep.clone(),
        store: store.clone(),
        upload: !decentralized,
        hardlinks: cfg.hardlinks,
        peers: peer_blooms.clone(),
        my_id: ep.id().to_string(),
        hits_local: std::sync::atomic::AtomicU64::new(0),
        hits_peer: std::sync::atomic::AtomicU64::new(0),
        hits_driver: std::sync::atomic::AtomicU64::new(0),
    });

    // Fetch-source stats: one line a minute (when changed) makes peer-serving
    // measurable rather than a matter of faith.
    {
        let blobs = blobs.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let mut last = (0, 0, 0);
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let now = (
                    blobs.hits_local.load(Relaxed),
                    blobs.hits_peer.load(Relaxed),
                    blobs.hits_driver.load(Relaxed),
                );
                if now != last {
                    println!(
                        "[cas] fetches: local={} peer={} driver={}",
                        now.0, now.1, now.2
                    );
                    last = now;
                }
            }
        });
    }

    // Bloom gossip: advertise what this store holds, every 30s when changed.
    // Peers use it to fetch hot blobs from caches instead of one producer.
    {
        let store = store.clone();
        let ctrl = ctrl_send.clone();
        tokio::spawn(async move {
            let mut last_n = usize::MAX;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let hashes = store.list_hashes();
                if hashes.len() == last_n {
                    continue;
                }
                last_n = hashes.len();
                let mut bloom = mesh::Bloom::with_capacity(hashes.len());
                for h in &hashes {
                    bloom.insert(h);
                }
                if mesh::send_frame(&mut *ctrl.lock().await, &W2D::Holdings { bloom })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    loop {
        let Some(msg) = mesh::recv_frame::<D2W>(&mut ctrl_recv).await? else {
            println!("[worker] driver closed control stream — done");
            return Ok(());
        };
        let (job, action) = match msg {
            D2W::Run { job, action } => (job, action),
            D2W::Blooms { peers } => {
                let mut map = peer_blooms.lock().await;
                map.clear();
                map.extend(peers);
                continue;
            }
            D2W::Welcome { .. } => continue,
        };
        let blobs = blobs.clone();
        let ctrl = ctrl_send.clone();
        let scratch = cfg.scratch.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let _permit = slots.acquire_owned().await.expect("semaphore open");
            let tracking = TrackingBlobs {
                inner: blobs,
                stored: Mutex::new(Vec::new()),
            };
            let reply = match exec::run_action(&tracking, &action, &scratch).await {
                Ok(outcome) => W2D::Done {
                    job,
                    action_result: outcome.action_result.encode_to_vec(),
                    stored: tracking.stored.into_inner(),
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

async fn serve_get(
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
        other => {
            mesh::send_frame(
                &mut send,
                &BlobResp::Err(format!("unsupported here: {other:?}")),
            )
            .await?
        }
    }
    send.finish()?;
    Ok(())
}

/// Records which blobs an action persisted — the driver's provider index.
struct TrackingBlobs {
    inner: Arc<RemoteBlobs>,
    stored: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl exec::Blobs for TrackingBlobs {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
        self.inner.get(d).await
    }
    async fn put(&self, bytes: Vec<u8>) -> Result<Dig> {
        let d = self.inner.put(bytes).await?;
        self.stored.lock().await.push(d.hash.clone());
        Ok(d)
    }
    async fn materialize_file(
        &self,
        d: &Dig,
        dest: &std::path::Path,
        is_executable: bool,
    ) -> Result<()> {
        self.inner.materialize_file(d, dest, is_executable).await
    }
    async fn put_file(&self, path: &std::path::Path) -> Result<Dig> {
        let d = self.inner.put_file(path).await?;
        self.stored.lock().await.push(d.hash.clone());
        Ok(d)
    }
}

/// Blobs fetched from the driver (or, on redirect, straight from the
/// producing worker), with the local store as cache.
struct RemoteBlobs {
    conn: Connection,
    ep: Endpoint,
    store: Arc<Store>,
    /// false in decentralized mode: outputs stay local, driver gets an index.
    upload: bool,
    hardlinks: bool,
    /// Gossiped peer holdings; consulted before asking the driver.
    peers: Arc<Mutex<HashMap<String, mesh::Bloom>>>,
    my_id: String,
    /// Where fetches were satisfied — settles "did peers actually serve?".
    hits_local: std::sync::atomic::AtomicU64,
    hits_peer: std::sync::atomic::AtomicU64,
    hits_driver: std::sync::atomic::AtomicU64,
}

impl RemoteBlobs {
    async fn upload_bytes(&self, d: &Dig, bytes: &[u8]) -> Result<()> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Put(d.clone())).await?;
        send.write_all(bytes).await?;
        send.finish()?;
        let resp: BlobResp = mesh::recv_frame(&mut recv)
            .await?
            .context("driver closed blob stream")?;
        match resp {
            BlobResp::PutOk => Ok(()),
            other => bail!("blob put rejected: {other:?}"),
        }
    }

    async fn fetch_from(&self, endpoint: &str, d: &Dig) -> Result<Vec<u8>> {
        let id: iroh::EndpointId = endpoint.parse().map_err(|_| {
            anyhow::anyhow!("bad provider endpoint {endpoint:?} for blob {}", d.hash)
        })?;
        let conn = self.ep.connect(id, mesh::ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Get(d.clone())).await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("provider closed blob stream")?
        {
            BlobResp::Found { size } => Ok(mesh::recv_raw(&mut recv, size).await?),
            other => bail!("provider {endpoint} for {}: {other:?}", d.hash),
        }
    }
}

#[async_trait::async_trait]
impl exec::Blobs for RemoteBlobs {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(bytes) = self.store.get(d).await? {
            self.hits_local.fetch_add(1, Relaxed);
            return Ok(bytes);
        }
        // Bloom-first: any peer cache claiming the blob beats a driver hop.
        // Deterministic pick from the hash spreads hot blobs across holders;
        // a false positive costs one refused Get and we fall through.
        let candidates: Vec<String> = {
            let peers = self.peers.lock().await;
            peers
                .iter()
                .filter(|(id, b)| **id != self.my_id && b.contains(&d.hash))
                .map(|(id, _)| id.clone())
                .collect()
        };
        if !candidates.is_empty() {
            let pick = usize::from_str_radix(&d.hash[..4], 16).unwrap_or(0) % candidates.len();
            if let Ok(bytes) = self.fetch_from(&candidates[pick], d).await {
                self.hits_peer.fetch_add(1, Relaxed);
                self.store.put(Some(d), &bytes).await?;
                return Ok(bytes);
            }
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
                self.hits_driver.fetch_add(1, Relaxed);
                self.store.put(Some(d), &bytes).await?;
                Ok(bytes)
            }
            BlobResp::Provider { endpoint } => {
                let bytes = self.fetch_from(&endpoint, d).await?;
                self.hits_peer.fetch_add(1, Relaxed);
                self.store.put(Some(d), &bytes).await?;
                Ok(bytes)
            }
            BlobResp::Missing => bail!("driver CAS missing blob {}/{}", d.hash, d.size),
            other => bail!("unexpected blob response: {other:?}"),
        }
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
        if !self.store.has(d).await {
            // Pulls into the local store as a side effect.
            let _ = self.get(d).await?;
        }
        // Linked = shared 0o555 inode, exec included; Private = ours to chmod.
        if self.store.link_out(d, dest).await? == crate::store::Materialized::Private
            && is_executable
        {
            exec::set_exec(dest).await?;
        }
        Ok(())
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Dig> {
        let d = self.store.put(None, &bytes).await?;
        if self.upload {
            self.upload_bytes(&d, &bytes).await?;
        }
        Ok(d)
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
        if self.upload {
            self.upload_bytes(&d, &bytes).await?;
        }
        Ok(d)
    }
}
