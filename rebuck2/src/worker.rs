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
use tokio::sync::{Mutex, Semaphore};

use crate::mesh::{self, BlobReq, BlobResp, Dig, D2W, W2D};
use crate::payload::Payload;
use crate::store::Store;

pub struct WorkerCfg {
    pub session: String,
    pub slots: usize,
    pub scratch: std::path::PathBuf,
    pub connect_wait: Duration,
    /// Path to a JSON EndpointAddr for the driver (CI run artifact).
    /// Dialing by explicit addr sidesteps n0 discovery; the session-derived
    /// id remains the fallback when the file is absent or stale.
    pub driver_addr_file: Option<std::path::PathBuf>,
    /// Hardlink inputs from the store into exec dirs (default). Off for
    /// filesystems/tools where shared inodes are problematic.
    pub hardlinks: bool,
    /// CI shard this worker restored before joining; finalize hands it
    /// the same shard back (see W2D::Hello::preloaded_shard).
    pub preloaded_shard: Option<u8>,
    /// Serve an OCI registry on this port for a buildkitd sharing the box.
    /// This is what keeps layers OFF the coordinator: the daemon pushes to
    /// loopback and pulls from whichever peer built the thing.
    pub registry_port: Option<u16>,
    /// Bind address for that registry. Loopback by default and deliberately —
    /// it has no auth.
    pub registry_bind: std::net::IpAddr,
}

pub async fn run(store: Arc<Store>, cfg: WorkerCfg, payload: Arc<dyn Payload>) -> Result<()> {
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
            // Prefer the published addr (no discovery dependency); fall
            // back to the session-derived id via n0 discovery.
            let attempt = match &cfg.driver_addr_file {
                Some(path) => match tokio::fs::read_to_string(path).await {
                    Ok(json) => match serde_json::from_str::<iroh::EndpointAddr>(&json) {
                        Ok(addr) => ep.connect(addr, mesh::ALPN).await,
                        Err(e) => {
                            println!("[worker] bad driver addr file ({e}); using discovery");
                            ep.connect(target, mesh::ALPN).await
                        }
                    },
                    Err(_) => ep.connect(target, mesh::ALPN).await,
                },
                None => ep.connect(target, mesh::ALPN).await,
            };
            match attempt {
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
            preloaded_shard: cfg.preloaded_shard,
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

    // The local agent: a buildkitd on this box talks only to us.
    if let Some(port) = cfg.registry_port {
        let agent = Arc::new(Agent {
            store: store.clone(),
            blobs: blobs.clone(),
            conn: conn.clone(),
        });
        let bind = cfg.registry_bind;
        tokio::spawn(async move {
            if let Err(e) = crate::registry::serve((bind, port).into(), agent).await {
                eprintln!("[agent] registry died: {e:#}");
            }
        });
    }

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

    // Bloom gossip: advertise what this store holds — immediately on
    // connect (a dice-warm client starts fetching within seconds; shard
    // seeds must be visible before the first FindMissing), then every 30s
    // when changed. Peers use it to fetch hot blobs from caches instead of
    // one producer.
    {
        let store = store.clone();
        let ctrl = ctrl_send.clone();
        tokio::spawn(async move {
            let mut last_n = usize::MAX;
            loop {
                let hashes = store.list_hashes();
                if hashes.len() != last_n {
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
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }

    loop {
        // Liveness watchdog: the driver pings every 20s; a long silence
        // means it died without a QUIC close (SIGTERM/crash) and this
        // worker would otherwise idle until the CI timeout cap.
        let msg = match tokio::time::timeout(
            Duration::from_secs(90),
            mesh::recv_frame::<D2W>(&mut ctrl_recv),
        )
        .await
        {
            Err(_) => {
                println!("[worker] no driver traffic for 90s — assuming driver gone, exiting");
                return Ok(());
            }
            Ok(frame) => match frame? {
                Some(msg) => msg,
                None => {
                    println!("[worker] driver closed control stream — done");
                    return Ok(());
                }
            },
        };
        let (job, action) = match msg {
            D2W::Run { job, action } => (job, action),
            D2W::Ping => continue,
            D2W::Exit => {
                println!("[worker] driver said exit — done");
                return Ok(());
            }
            D2W::Blooms { peers } => {
                let mut map = peer_blooms.lock().await;
                map.clear();
                map.extend(peers);
                continue;
            }
            D2W::Welcome { .. } => continue,
            D2W::Finalize { shard, of } => {
                println!("[worker] finalize: syncing snapshot shard {shard}/{of}");
                if let Err(e) = sync_shard(&store, &conn, shard, of, &cfg.scratch).await {
                    eprintln!("[worker] shard sync failed (partial save): {e:#}");
                }
                // The workflow's save step reads this to key the cache entry.
                let id_path = cfg
                    .scratch
                    .parent()
                    .unwrap_or(&cfg.scratch)
                    .join("shard.id");
                // Trailing newline matters: `read` in the CI teardown returns
                // rc=1 at EOF-without-newline, and bash -e killed the pack
                // step on every worker (shards were never saved).
                let _ = tokio::fs::write(&id_path, format!("{shard} {of}\n")).await;
                let _ =
                    mesh::send_frame(&mut *ctrl_send.lock().await, &W2D::Finalized { shard }).await;
                println!("[worker] finalized shard {shard} — exiting");
                return Ok(());
            }
        };
        let blobs = blobs.clone();
        let ctrl = ctrl_send.clone();
        let scratch = cfg.scratch.clone();
        let slots = slots.clone();
        let payload = payload.clone();
        tokio::spawn(async move {
            let _permit = slots.acquire_owned().await.expect("semaphore open");
            let tracking = TrackingBlobs {
                inner: blobs,
                stored: Mutex::new(Vec::new()),
            };
            let reply = match payload.execute(&tracking, &action, &scratch).await {
                Ok(done) => W2D::Done {
                    job,
                    action_result: done.result,
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
        BlobReq::HasMany(digs) => {
            let mut have = Vec::with_capacity(digs.len());
            for d in &digs {
                have.push(store.has(d).await);
            }
            mesh::send_frame(&mut send, &BlobResp::HaveMany(have)).await?;
        }
        // One BlobResp frame per digest in request order, bytes inline after
        // each Found. get-then-reply per item: no Found promise can outlive
        // an LRU eviction between a batched presence check and the read.
        BlobReq::GetMany(digs) => {
            for d in &digs {
                match store.get(d).await {
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
            // Finalize union sync: the driver aggregates every worker's
            // range list so banked shards cover the FLEET's holdings.
            let digs = store.list_shard(shard, of);
            mesh::send_frame(&mut send, &BlobResp::HashList(digs)).await?;
        }
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
impl crate::store::Blobs for TrackingBlobs {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
        self.inner.get(d).await
    }
    // Not the trait's no-op default: dropping this delegation silently
    // reverts staging to one round-trip per blob.
    async fn prefetch(&self, digs: &[Dig]) -> Result<()> {
        self.inner.prefetch(digs).await
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
pub struct RemoteBlobs {
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

/// Claim a key over the mesh from a standalone process — the shape a forked
/// buildkitd takes: dial the driver, ask, act. Exposed as `rebuck2 claim` so
/// the mesh lease path is exercisable (and debuggable) without a fork existing.
///
/// Prints `leader` and exits 0 if you must build it; prints the leader's result
/// to stdout and exits 0 if someone else did; exits 75 (EX_TEMPFAIL) to say
/// "re-claim" if the leader vanished.
pub async fn claim_once(session: &str, key: &str) -> Result<i32> {
    use tokio::io::AsyncReadExt;

    let ep = Endpoint::builder(iroh::endpoint::presets::N0)
        .alpns(vec![mesh::ALPN.to_vec()])
        .bind()
        .await?;
    let conn = ep.connect(mesh::driver_id(session), mesh::ALPN).await?;
    // Announce as a client, not a worker: we want a lease, not a job.
    let (mut ctrl, _ctrl_recv) = conn.open_bi().await?;
    mesh::send_frame(&mut ctrl, &W2D::ClientHello).await?;

    match claim(&conn, key).await? {
        Claimed::Leader => {
            println!("leader");
            // HOLD the lease while "building". A lease lives only as long as
            // its holder's connection does — drop it and the driver rightly
            // hands the job to someone else. A real builder keeps its
            // connection open for exactly this reason; here stdin stands in
            // for the build, and what it yields is the result.
            let mut result = Vec::new();
            tokio::io::stdin().read_to_end(&mut result).await?;
            release_lease(&conn, key, Ok(result)).await?;
            Ok(0)
        }
        Claimed::Done(result) => {
            use std::io::Write;
            std::io::stdout().write_all(&result)?;
            Ok(0)
        }
        Claimed::Failed(e) => {
            eprintln!("leader failed: {e}");
            Ok(1)
        }
        Claimed::Retry => {
            eprintln!("leader vanished; re-claim");
            Ok(75) // EX_TEMPFAIL
        }
    }
}

/// What a claimant is told (see [`crate::lease`]).
pub enum Claimed {
    /// You build it. Heartbeat until you publish, or the driver will assume you
    /// died and hand the job to someone else.
    Leader,
    /// A peer built it (or is about to). These are its result bytes.
    Done(Vec<u8>),
    /// The leader failed. Same job, same failure.
    Failed(String),
    /// The leader vanished without publishing. Claim again.
    Retry,
}

/// A worker's local agent: an OCI registry backed by THIS worker's store and the
/// mesh, with leases forwarded to the driver.
///
/// This is what puts the coordinator off the data path. A buildkitd talks only to
/// its own agent on loopback:
///
/// ```text
///   leader   pushes a layer -> its OWN agent (loopback, ~free)
///   follower pulls  a layer -> its own agent -> mesh -> the LEADER's agent
/// ```
///
/// So the bytes go straight from the machine that built them to the machine that
/// needs them. Measured amplification through the central coordinator was ~1.0x
/// per follower — i.e. (N-1)x at fleet scale, all through one NIC. Here it is
/// zero: the driver never sees the layer at all.
pub struct Agent {
    pub store: Arc<Store>,
    pub blobs: Arc<RemoteBlobs>,
    pub conn: Connection,
}

#[async_trait::async_trait]
impl crate::registry::RegistryStore for Agent {
    async fn blob_size(&self, hash: &str) -> Option<u64> {
        if let Some(n) = self.store.size_of(hash).await {
            return Some(n);
        }
        // Be honest about FLEET-wide presence, not just our own disk, or the
        // buildkitd re-pushes layers a peer already holds.
        match self.blobs.get_by_hash(hash).await {
            Ok(Some(b)) => Some(b.len() as u64),
            _ => None,
        }
    }
    async fn blob_get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.blobs.get_by_hash(hash).await
    }
    async fn blob_put(&self, bytes: &[u8]) -> Result<String> {
        Ok(self.store.put(None, bytes).await?.hash)
    }
    async fn upload_begin(&self) -> Result<crate::store::Upload> {
        self.store.begin_upload().await
    }
    async fn upload_finish(
        &self,
        up: crate::store::Upload,
        expected: Option<&str>,
    ) -> Result<String> {
        // Lands in OUR store — nothing is uploaded anywhere.
        let hash = self.store.finish_upload(up, expected).await?;
        // Tell the driver at once. Bloom gossip would get there in ~30s, but a
        // follower blocked on the lease asks in ~0s, and until the driver knows
        // who holds this it would RELAY the bytes through its own NIC — taking
        // the centralised path at exactly the moment it costs most.
        let announce = Dig {
            hash: hash.clone(),
            size: 0,
        };
        if let Err(e) = announce_blob(&self.conn, &announce).await {
            eprintln!("[agent] announce {hash} failed ({e:#}); peers will find it by gossip");
        }
        Ok(hash)
    }
    async fn tag_get(&self, key: &str) -> Option<String> {
        self.store.tag_get(key).await
    }
    async fn tag_put(&self, key: &str, manifest_hash: &str) -> Result<()> {
        self.store.tag_put(key, manifest_hash).await
    }

    // The lease is the DRIVER's — it is the fleet's single coordinator. A worker
    // agent only forwards. (Only the lease is centralised; the layers are not.)
    async fn lease_claim(&self, key: &str) -> Option<crate::registry::Claimed> {
        match claim(&self.conn, key).await {
            // The mesh holder is US (this worker's endpoint): the driver already
            // ties the lease to our connection, so our liveness IS the heartbeat
            // and a dead worker is evicted the moment it disconnects.
            Ok(Claimed::Leader) => Some(crate::registry::Claimed::Leader {
                holder: String::new(),
            }),
            Ok(Claimed::Done(r)) => Some(crate::registry::Claimed::Done(r)),
            Ok(Claimed::Failed(_)) | Ok(Claimed::Retry) => Some(crate::registry::Claimed::Retry),
            // The driver is unreachable. Tell the buildkitd to build it: a fleet
            // that loses its coordinator must degrade to plain BuildKit, never
            // stall.
            Err(e) => {
                eprintln!("[agent] lease claim failed ({e:#}); telling buildkitd to build it");
                Some(crate::registry::Claimed::Leader {
                    holder: String::new(),
                })
            }
        }
    }
    async fn lease_heartbeat(&self, key: &str, _holder: &str) -> bool {
        heartbeat(&self.conn, key).await.unwrap_or(false)
    }
    async fn lease_release(&self, key: &str, _holder: &str, result: Vec<u8>) {
        let _ = release_lease(&self.conn, key, Ok(result)).await;
    }
    async fn lease_abandon(&self, key: &str, _holder: &str) {
        let _ = release_lease(&self.conn, key, Err("leader abandoned".into())).await;
    }
}

/// Tell the driver we hold a blob, so it can redirect peers to us instead of
/// relaying their bytes.
async fn announce_blob(conn: &Connection, d: &Dig) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    mesh::send_frame(&mut send, &BlobReq::Announce(vec![d.clone()])).await?;
    send.finish()?;
    let _ = mesh::recv_frame::<BlobResp>(&mut recv).await?;
    Ok(())
}

/// Cross-machine single-flight, worker side.
///
/// A buildkitd fork calls this before executing a vertex, keyed on its fast
/// cache key: the first daemon to ask builds, the rest wait and reuse. That is
/// the property one buildkitd gets free from its own solver, and which N
/// ephemeral daemons otherwise lose. See docs/buildkit-plan.md P2.
pub async fn claim(conn: &iroh::endpoint::Connection, key: &str) -> Result<Claimed> {
    let (mut send, mut recv) = conn.open_bi().await?;
    mesh::send_frame(
        &mut send,
        &BlobReq::Claim {
            key: key.to_string(),
        },
    )
    .await?;
    send.finish()?;
    match mesh::recv_frame::<BlobResp>(&mut recv)
        .await?
        .context("driver closed the lease stream")?
    {
        BlobResp::LeaseGranted => Ok(Claimed::Leader),
        // The driver keeps this stream open and PUSHES the outcome, so a
        // leader's death arrives as LeaseRetry rather than as silence. A closed
        // stream means the driver itself went away: retry, never hang.
        BlobResp::LeaseHeld => match mesh::recv_frame::<BlobResp>(&mut recv).await? {
            Some(BlobResp::LeaseDone(Ok(r))) => Ok(Claimed::Done(r)),
            Some(BlobResp::LeaseDone(Err(e))) => Ok(Claimed::Failed(e)),
            _ => Ok(Claimed::Retry),
        },
        other => bail!("unexpected reply to Claim: {other:?}"),
    }
}

/// Tell the driver we are still working. `false` means we no longer hold the
/// lease (we were reaped as silent, and someone else owns the job now) — stop,
/// and re-claim.
///
/// Unused in-tree: a leader that finishes inside [`crate::lease::LEASE_TTL`]
/// never needs to heartbeat, and `rebuck2 claim` is one-shot. The consumer is
/// the forked buildkitd, whose vertices can outrun the TTL. Kept (and kept
/// tested via `Leases::heartbeat`) because adding it later means discovering
/// the need through 90-second timeouts in CI.
#[allow(dead_code)]
pub async fn heartbeat(conn: &iroh::endpoint::Connection, key: &str) -> Result<bool> {
    let (mut send, mut recv) = conn.open_bi().await?;
    mesh::send_frame(
        &mut send,
        &BlobReq::Heartbeat {
            key: key.to_string(),
        },
    )
    .await?;
    send.finish()?;
    match mesh::recv_frame::<BlobResp>(&mut recv).await? {
        Some(BlobResp::LeaseAlive(alive)) => Ok(alive),
        _ => Ok(false),
    }
}

/// Publish our result and wake every follower.
pub async fn release_lease(
    conn: &iroh::endpoint::Connection,
    key: &str,
    result: Result<Vec<u8>, String>,
) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    mesh::send_frame(
        &mut send,
        &BlobReq::Release {
            key: key.to_string(),
            result,
        },
    )
    .await?;
    send.finish()?;
    let _ = mesh::recv_frame::<BlobResp>(&mut recv).await?;
    Ok(())
}

impl RemoteBlobs {
    /// Blob by hash alone — the OCI path (a REAPI digest carries a size, an OCI
    /// digest never does). Same chain as `get`: local store, then whichever peer
    /// the blooms claim holds it, then the driver. That chain IS the P2P story —
    /// a follower's layer comes from the machine that built it, not from the
    /// coordinator's NIC.
    ///
    /// Verifies by hash: the size is learned from what came back, rather than
    /// faked as zero, which would make `put(Some(d))` reject every non-empty blob.
    pub async fn get_by_hash(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.store.get_by_hash(hash).await? {
            return Ok(Some(bytes));
        }
        // The wire is hash-keyed (a peer reads cas/<hh>/<hash>), so a zero size
        // is fine ON THE WIRE. It is the local cache-back that must not lie:
        // put(Some(d)) verifies size, so a zero would reject every non-empty
        // blob. Verify by hash and take the size from what actually arrived.
        let probe = Dig {
            hash: hash.to_string(),
            size: 0,
        };
        let bytes = match self.fetch_over_mesh(&probe).await {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let landed = Dig {
            hash: hash.to_string(),
            size: bytes.len() as i64,
        };
        self.store.put(Some(&landed), &bytes).await?;
        Ok(Some(bytes))
    }

    /// One pass of the peer/driver chain, WITHOUT the local cache-back — the
    /// caller decides how to verify (REAPI knows the size; OCI does not).
    async fn fetch_over_mesh(&self, d: &Dig) -> Result<Vec<u8>> {
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
                use std::sync::atomic::Ordering::Relaxed;
                self.hits_peer.fetch_add(1, Relaxed);
                return Ok(bytes);
            }
        }
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Get(d.clone())).await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("driver closed blob stream")?
        {
            BlobResp::Found { size } => {
                use std::sync::atomic::Ordering::Relaxed;
                self.hits_driver.fetch_add(1, Relaxed);
                Ok(mesh::recv_raw(&mut recv, size).await?)
            }
            // The driver points at whoever built it: fetch direct, peer to peer.
            BlobResp::Provider { endpoint } => {
                use std::sync::atomic::Ordering::Relaxed;
                let bytes = self.fetch_from(&endpoint, d).await?;
                self.hits_peer.fetch_add(1, Relaxed);
                Ok(bytes)
            }
            BlobResp::Missing => bail!("blob {} is nowhere on the mesh", d.hash),
            other => bail!("unexpected blob response: {other:?}"),
        }
    }

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

    /// One GetMany round-trip: pull `digs` from `endpoint` (None = the
    /// driver) into the local store. Returns the digests NOT obtained
    /// (missing on the holder, or redirected and the redirect also failed)
    /// — callers decide the next hop. Driver `Provider` redirects are
    /// followed with one further batched hop per provider.
    async fn fetch_many_from(&self, endpoint: Option<&str>, digs: &[Dig]) -> Result<Vec<Dig>> {
        use std::sync::atomic::Ordering::Relaxed;
        let conn = match endpoint {
            Some(ep) => {
                let id: iroh::EndpointId = ep
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad provider endpoint {ep:?}"))?;
                self.ep.connect(id, mesh::ALPN).await?
            }
            None => self.conn.clone(),
        };
        let (mut send, mut recv) = conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::GetMany(digs.to_vec())).await?;
        send.finish()?;
        let mut unfetched: Vec<Dig> = Vec::new();
        let mut redirects: HashMap<String, Vec<Dig>> = HashMap::new();
        for d in digs {
            match mesh::recv_frame::<BlobResp>(&mut recv)
                .await?
                .context("holder closed mid-batch")?
            {
                BlobResp::Found { size } => {
                    let bytes = mesh::recv_raw(&mut recv, size).await?;
                    self.store.put(Some(d), &bytes).await?;
                    if endpoint.is_some() {
                        self.hits_peer.fetch_add(1, Relaxed);
                    } else {
                        self.hits_driver.fetch_add(1, Relaxed);
                    }
                }
                BlobResp::Provider { endpoint } => {
                    redirects.entry(endpoint).or_default().push(d.clone());
                }
                _ => unfetched.push(d.clone()),
            }
        }
        for (ep, group) in redirects {
            match Box::pin(self.fetch_many_from(Some(&ep), &group)).await {
                Ok(rest) => unfetched.extend(rest),
                Err(_) => unfetched.extend(group),
            }
        }
        Ok(unfetched)
    }
}

#[async_trait::async_trait]
impl crate::store::Blobs for RemoteBlobs {
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

    /// Batched warm-up for materialize: group missing digests by bloom-
    /// claimed holder (same deterministic pick as `get`), one GetMany per
    /// group concurrently, driver fallback in concurrent chunks. Best
    /// effort by contract — whatever stays missing is refetched (and
    /// properly diagnosed) by the per-blob `get` path.
    async fn prefetch(&self, digs: &[Dig]) -> Result<()> {
        let mut missing: Vec<Dig> = Vec::new();
        let mut dedup: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for d in digs {
            if dedup.insert(&d.hash) && !self.store.has(d).await {
                missing.push(d.clone());
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let mut by_peer: HashMap<String, Vec<Dig>> = HashMap::new();
        let mut for_driver: Vec<Dig> = Vec::new();
        {
            let peers = self.peers.lock().await;
            for d in missing {
                let candidates: Vec<&String> = peers
                    .iter()
                    .filter(|(id, b)| **id != self.my_id && b.contains(&d.hash))
                    .map(|(id, _)| id)
                    .collect();
                if candidates.is_empty() {
                    for_driver.push(d);
                } else {
                    let pick =
                        usize::from_str_radix(&d.hash[..4], 16).unwrap_or(0) % candidates.len();
                    by_peer.entry(candidates[pick].clone()).or_default().push(d);
                }
            }
        }
        // Peer groups in parallel; a failed group falls through to the
        // driver (read-through there re-heals the hot set, same as `get`).
        let groups = futures::future::join_all(by_peer.iter().map(|(peer, group)| async move {
            match self.fetch_many_from(Some(peer), group).await {
                Ok(rest) => rest,
                Err(_) => group.clone(),
            }
        }))
        .await;
        for_driver.extend(groups.into_iter().flatten());
        // Chunked so one stream never serializes tens of thousands of blobs,
        // concurrent so the driver's read-through fans out too.
        use futures::StreamExt;
        futures::stream::iter(for_driver.chunks(512))
            .for_each_concurrent(4, |chunk| async move {
                let _ = self.fetch_many_from(None, chunk).await;
            })
            .await;
        Ok(())
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
        if !self.store.has(d).await {
            // Pulls into the local store as a side effect.
            let _ = self.get(d).await?;
        }
        // Linked = shared 0o555 inode, exec included; Private = ours to chmod.
        if self.store.link_out(d, dest).await? == crate::store::Materialized::Private
            && is_executable
        {
            crate::store::set_exec(dest).await?;
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

/// Make this worker's store a complete replica of snapshot shard
/// `shard`/`of`: list the driver's shard hashes, fetch what's missing.
/// Bounded concurrency; the post-build window is otherwise idle.
async fn sync_shard(
    store: &Arc<Store>,
    conn: &Connection,
    shard: u8,
    of: u8,
    _scratch: &std::path::Path,
) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    mesh::send_frame(&mut send, &BlobReq::ListShard { shard, of }).await?;
    send.finish()?;
    let digs = match mesh::recv_frame::<BlobResp>(&mut recv)
        .await?
        .context("driver closed shard list stream")?
    {
        BlobResp::HashList(v) => v,
        other => bail!("unexpected shard list response: {other:?}"),
    };
    let mut missing = Vec::new();
    for d in digs {
        if !store.has(&d).await {
            missing.push(d);
        }
    }
    println!(
        "[worker] shard {shard}: fetching {} missing blobs",
        missing.len()
    );
    // Chunked GetMany: 24 per-blob streams still cost a stream open and a
    // request round-trip PER BLOB through one driver conn — during the
    // finalize window, times every worker at once. One request per chunk
    // streams the bytes back-to-back; 4 chunks concurrent keeps the
    // driver's read-through fanned out (its GetMany arm is serial per
    // stream). Per-item Missing/Err skipped: shard save is best-effort.
    use futures::StreamExt;
    futures::stream::iter(missing.chunks(512))
        .for_each_concurrent(4, |chunk| {
            let store = store.clone();
            let conn = conn.clone();
            async move {
                let fetch = async {
                    let (mut send, mut recv) = conn.open_bi().await?;
                    mesh::send_frame(&mut send, &BlobReq::GetMany(chunk.to_vec())).await?;
                    send.finish()?;
                    for d in chunk {
                        if let BlobResp::Found { size } = mesh::recv_frame::<BlobResp>(&mut recv)
                            .await?
                            .context("driver closed mid-batch")?
                        {
                            let bytes = mesh::recv_raw(&mut recv, size).await?;
                            store.put(Some(d), &bytes).await?;
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                };
                if let Err(e) = fetch.await {
                    eprintln!("[worker] shard {shard}: chunk fetch failed (partial): {e:#}");
                }
            }
        })
        .await;
    Ok(())
}
