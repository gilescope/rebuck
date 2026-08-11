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
    /// gRPC address of a buildkitd this worker may drive, and the loopback
    /// mirror to publish results into. Both absent = this worker declines
    /// every subtree offered to it, which is the correct answer rather than
    /// a degraded one: the requester then builds it itself, exactly as it
    /// would have without dispatch at all.
    pub buildkit_addr: Option<String>,
    pub registry_addr: Option<String>,
    /// Serve a registry HERE, backed by the fleet.
    ///
    /// The piece that lets a worker live on its own machine. Its buildkitd
    /// speaks OCI and cannot speak the mesh, so it needs a registry it can
    /// reach; with one on localhost, a miss is answered by whichever machine
    /// built the layer instead of by an HTTP host every daemon must route to.
    pub registry_bind: Option<String>,
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
    /// "The party is over": if this path appears while we are still
    /// dialling, the driver has finished and there is nothing left to
    /// join. Something outside the process creates it (in CI, a poller
    /// watching for the driver's end-of-lap marker) — a worker that
    /// arrives late otherwise burns its whole `connect_wait` on a driver
    /// that exited half an hour ago, and reds its job doing it.
    pub give_up_file: Option<std::path::PathBuf>,
}

/// What the connect-retry loop should do after a failed dial.
#[derive(Debug, PartialEq, Eq)]
enum Retry {
    Again,
    PartyOver,
    Fatal,
}

/// The note beats the clock in BOTH directions: arriving after the lap
/// ended is not a failure, so it must not surface as one even once the
/// connect deadline has also passed.
fn retry_verdict(give_up: Option<&std::path::Path>, deadline_passed: bool) -> Retry {
    if give_up.is_some_and(std::path::Path::exists) {
        Retry::PartyOver
    } else if deadline_passed {
        Retry::Fatal
    } else {
        Retry::Again
    }
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
                Err(e) => {
                    match retry_verdict(cfg.give_up_file.as_deref(), Instant::now() >= deadline) {
                        Retry::PartyOver => {
                            println!(
                                "[worker] driver finished before we joined — nothing to serve"
                            );
                            return Ok(());
                        }
                        Retry::Fatal => return Err(e).context("driver never became reachable"),
                        Retry::Again => {
                            println!("[worker] connect retry: {e}");
                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    }
                }
            }
        }
    };
    println!("[worker] connected");

    // What this worker can BUILD, which is its daemon's platform and not its
    // host's. On macOS the host is darwin/arm64 and the daemon is
    // linux/arm64, so a worker advertising the host is offered nothing at
    // all - the driver filters every linux graph out as WrongPlatform and
    // the fleet reports "took nothing" while looking perfectly healthy.
    //
    // Falls back to the host when there is no daemon to ask: such a worker
    // takes REAPI actions only, and those really are host-platform.
    let (os, arch) = match cfg.buildkit_addr.as_deref() {
        Some(bk) => crate::solve::daemon_platforms(bk)
            .await
            .first()
            .and_then(|p| p.split_once('/'))
            .map(|(o, a)| (o.to_owned(), a.to_owned()))
            .unwrap_or_else(|| (std::env::consts::OS.to_owned(), arch().to_owned())),
        None => (std::env::consts::OS.to_owned(), arch().to_owned()),
    };
    println!("[worker] offering {os}/{arch}");

    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    mesh::send_frame(
        &mut ctrl_send,
        &W2D::Hello {
            os: os.clone(),
            arch: arch.clone(),
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

    // A registry on this worker, backed by the fleet behind `blobs`.
    if let Some(bind) = cfg.registry_bind.clone() {
        let reg = crate::registry::MeshBacked::new(store.clone(), blobs.clone());
        match bind.parse() {
            Ok(addr) => {
                tokio::spawn(async move {
                    if let Err(e) = crate::registry::serve_with_upstream(addr, reg, None).await {
                        eprintln!("[worker] registry died: {e:#}");
                    }
                });
            }
            // Loud, not fatal: without it this worker's daemon has nowhere to
            // pull from and every lead will decline, which is correct but
            // reads as a fleet that mysteriously refuses everything.
            Err(e) => eprintln!("[worker] --registry-bind {bind:?} is not an address: {e}"),
        }
    }

    // Fetch-source stats: one line a minute (when changed) makes peer-serving
    // measurable rather than a matter of faith.
    {
        let blobs = blobs.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let mut last = (0, 0, 0);
            loop {
                // 15s, not 60. This only prints on CHANGE, so the cost of
                // a short interval is nothing, and the cost of a long one is
                // a 90-second build reporting no blob movement at all -
                // indistinguishable from a fleet where none happened.
                tokio::time::sleep(Duration::from_secs(15)).await;
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
            // Seeded ONCE from disk, then maintained by insertion. A bloom is
            // additive - inserting sets bits and never clears them - so it
            // never needs rebuilding from a directory walk. That walk is why
            // the tick was 30 seconds, and 30 seconds is longer than the
            // window in which a freshly-fetched share is worth anything.
            let mut held: Vec<String> = store.list_hashes();
            let mut bloom = mesh::Bloom::with_capacity(held.len().max(1024));
            for h in &held {
                bloom.insert(h);
            }
            let mut sized_for = held.len().max(1024);
            let mut dirty = true;
            loop {
                // Drain what the store has gained since the last pass.
                {
                    let (lock, _) = &*crate::store::GAINED;
                    if let Ok(mut g) = lock.lock() {
                        if g.resync {
                            // The cap was hit and records were dropped, so
                            // the in-memory filter can no longer be trusted
                            // to be complete. One walk, then incremental
                            // again - the old behaviour as a fallback rather
                            // than as the design.
                            g.resync = false;
                            g.hashes.clear();
                            drop(g);
                            held = store.list_hashes();
                            sized_for = held.len().max(1024);
                            bloom = mesh::Bloom::with_capacity(sized_for);
                            for h in &held {
                                bloom.insert(h);
                            }
                            dirty = true;
                        } else if !g.hashes.is_empty() {
                            for h in g.hashes.drain(..) {
                                bloom.insert(&h);
                                held.push(h);
                            }
                            dirty = true;
                        }
                    }
                }
                // RESIZE when the filter is past what it was sized for, or
                // its false-positive rate climbs and peers start being asked
                // for blobs they do not have. Sizing is 12 bits an element
                // rounded to a power of two, so this happens O(log N) times
                // across a whole run, not per tick.
                if held.len() > sized_for {
                    sized_for = held.len() * 2;
                    bloom = mesh::Bloom::with_capacity(sized_for);
                    for h in &held {
                        bloom.insert(h);
                    }
                }
                if dirty {
                    dirty = false;
                    if mesh::send_frame(
                        &mut *ctrl.lock().await,
                        &W2D::Holdings {
                            bloom: bloom.clone(),
                        },
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                // 30s WHEN IDLE, but woken the moment holdings change.
                //
                // The interval alone made the seed split fictional: a worker
                // fetches its share, and for up to thirty seconds no peer can
                // see it - which is longer than the whole fan-out window. So
                // every worker fell back to the driver anyway, and the split
                // cost N speculative fetches on top of the N-1 lazy ones it
                // was meant to replace. Strictly worse than not splitting.
                //
                // A share that nobody can see has not been shared.
                // Woken by a blob landing; the timer is only a safety net
                // now that nothing depends on it for latency.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    _ = crate::store::GAINED.1.notified() => {
                        // Coalesce a burst: a hundred blobs arriving in a
                        // second should be one gossip, not a hundred.
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
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
            // Answers to a subtree WE offered. Both are terminal, and both
            // are normal: a peer built it and named where, or nobody took
            // it and we build it ourselves - which is what we would have
            // done without dispatch at all.
            D2W::Placed { job, image_ref } => {
                println!("[worker] subtree {job} placed, pull from {image_ref}");
                continue;
            }
            // Fetch what will be wanted, before it is wanted.
            //
            // In the BACKGROUND and never blocking the control loop: this
            // worker must stay able to take a Lead while it prefetches, or
            // the mechanism costs exactly what it saves. Failures are
            // dropped - a blob that does not arrive now arrives lazily
            // later, which is what happens today.
            D2W::Prefetch { digests, peers } => {
                let n = digests.len();
                let blobs = blobs.clone();
                tokio::spawn(async move {
                    // Only this worker's share. Six workers pulling the same
                    // layers off the coordinator at once is the herd
                    // `seeder_for` exists to prevent - and a prefetch makes
                    // it arrive EARLIER, so it would hurt more than the lazy
                    // path it replaces.
                    let mine = blobs.my_share(digests, &peers).await;
                    let share = mine.len();
                    let mut got = 0usize;
                    for d in mine {
                        // `get` walks local, then peers by bloom, then the
                        // driver - the same path a lazy fetch takes, so a
                        // prefetch warms exactly what a build would have
                        // pulled and nothing else.
                        if exec::Blobs::get(&*blobs, &d).await.is_ok() {
                            got += 1;
                        }
                    }
                    println!("[worker] prefetched {got}/{share} of my share ({n} announced)");
                    // No explicit wake needed: the store fires on every
                    // blob it gains, so a fetched share announces itself.
                });
                continue;
            }
            D2W::Unplaced { job, why } => {
                println!("[worker] subtree {job} unplaced ({why}) - building it here");
                continue;
            }
            // An OFFER. Every refusal below is the protocol working, not a
            // failure: the requester builds it itself, exactly as it would
            // have without dispatch. Fail open, never fail wrong.
            D2W::Lead {
                job,
                subtree,
                frontier,
            } => {
                let reply = lead_reply(&cfg, &slots, job, &subtree, &frontier).await;
                let _ = mesh::send_frame(&mut *ctrl_send.lock().await, &reply).await;
                continue;
            }
            D2W::Ping { vitals } => {
                if let Some(v) = vitals {
                    println!("[driver-vitals] {v}");
                }
                continue;
            }
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
        let ac_store = store.clone();
        tokio::spawn(async move {
            let _permit = slots.acquire_owned().await.expect("semaphore open");
            let tracking = TrackingBlobs {
                inner: blobs,
                stored: Mutex::new(Vec::new()),
            };
            let reply = match exec::run_action(&tracking, &action, &scratch).await {
                Ok(outcome) => {
                    record_local_ac(&ac_store, &action.hash, &outcome).await;
                    W2D::Done {
                        job,
                        action_result: outcome.action_result.encode_to_vec(),
                        stored: tracking.stored.into_inner(),
                    }
                }
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
        // A registry asks by hash: buildkit's URL is the digest and the size
        // is what the reply is meant to supply. Served only from what this
        // worker holds - it does not walk the fleet on someone else's behalf.
        // A worker answers tag lookups from its own registry store. This
        // is what makes buildkit's registry cache usable across machines:
        // the cache ref is a tag, one worker exported it, and the others
        // have no way to find it otherwise.
        BlobReq::TagGet(key) => {
            let found = store.tag_get(&key).await;
            mesh::send_frame(&mut send, &BlobResp::Tag(found)).await?;
            send.finish().ok();
        }
        BlobReq::GetByHash(hash) => match store.get_by_hash(&hash).await {
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
            _ => mesh::send_frame(&mut send, &BlobResp::Missing).await?,
        },
        BlobReq::Get(d) => {
            if store.has(&d).await {
                mesh::send_frame(
                    &mut send,
                    &BlobResp::Found {
                        size: d.size as u64,
                    },
                )
                .await?;
                store.copy_out(&d, &mut send).await?;
            } else {
                mesh::send_frame(&mut send, &BlobResp::Missing).await?;
            }
        }
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
                if store.has(d).await {
                    mesh::send_frame(
                        &mut send,
                        &BlobResp::Found {
                            size: d.size as u64,
                        },
                    )
                    .await?;
                    store.copy_out(d, &mut send).await?;
                } else {
                    mesh::send_frame(&mut send, &BlobResp::Missing).await?;
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

/// Keep a local AC row for an action this worker just executed.
///
/// The worker never READS the AC — the driver is the only consumer — but
/// it is the node that authored the result, so it is the node that banks
/// it (`ci/ac-bank-plan.md`). Row and referenced blobs are then born AND
/// banked on the same box in the same lap: a torn publish loses both
/// together (honest miss) instead of banking a row whose outputs never
/// landed — the unservable class of writer 28935304124.
///
/// Cacheability is the strict subset of the driver's rule (rpc.rs): exit
/// 0 and not do_not_cache. `--cache-failures` dedupes failures WITHIN a
/// lap on the driver; a banked failure row replays forever.
/// Decide on an offered subtree and, if we take it, build it.
///
/// Split out of the control loop so the decision chain is readable in one
/// place: the checks run in the order `dispatch::consider` defines, and the
/// build only happens after all of them pass.
// No explicit wake needed: the store fires on every
// blob it gains, so a fetched share announces itself./// Which peer is responsible for pulling this blob from the driver first.
///
/// The seed is one machine wide today: the first worker to want the base
/// finds nothing on any peer and pulls all of it from the coordinator - 75
/// blobs against 3 from peers, measured - while the others wait. The cascade
/// behind that works (the next worker got 26 of 36 from peers); it is the
/// SEED that does not spread.
///
/// So each blob is assigned an owner by its own hash. Six workers then take
/// six different sixths off the coordinator at once and exchange the rest.
/// No coordination: every worker computes the same answer from the same
/// inputs, which is the only reason two of them do not fetch the same blob.
///
/// Sorted first, so the answer cannot depend on the order a peer map happens
/// to iterate in - that would defeat the agreement it exists to provide.
// Not yet wired. The requesting side is easy; the SERVING side is the part
// that matters and it needs a driver handle threaded into `serve_get`, which
// today deliberately holds only the store ("it does not walk the fleet on
// someone else's behalf"). Fetch-through to the DRIVER only is loop-free and
// is what the split needs - but warming attacks the same 192s chain more
// simply, so this waits on that result rather than both landing at once.
#[allow(dead_code)]
fn seeder_for(hash: &str, peers: &[String]) -> Option<String> {
    if peers.is_empty() {
        return None;
    }
    let mut sorted: Vec<&String> = peers.iter().collect();
    sorted.sort();
    // The digest's TRAILING hex digits, in order. The first version reversed
    // them, which puts the least-variable digits in the low bits - over 1200
    // synthetic hashes that sent almost everything to one worker, and the
    // spread test caught it.
    let tail = &hash[hash.len().saturating_sub(8)..];
    let n = u64::from_str_radix(tail, 16).unwrap_or(0);
    Some(sorted[(n % sorted.len() as u64) as usize].clone())
}

/// How a nested build should reach the daemon on THIS machine, if it should.
///
/// `None` leaves the graph alone, which means a nested earthly keeps dialling
/// whatever the coordinator forwarded. Off by default like every other
/// mechanism here: it changes the cache key of any RUN that carries the
/// variable, so a forwarded RUN stops merging across machines - a real cost
/// that has to be measured against the funnel it removes rather than assumed
/// smaller.
///
/// Not loopback. earthly's `IsLocal` treats 127.0.0.1 as "a buildkit I
/// manage" and tries to start its own container from an image that is not
/// published, so the nested build dies on `manifest unknown` before it
/// solves anything.
fn nested_host(enabled: bool, override_addr: Option<&str>, mine: Option<&str>) -> Option<String> {
    if !enabled {
        return None;
    }
    let addr = override_addr.or(mine)?;
    let addr = addr.strip_prefix("tcp://").unwrap_or(addr);
    let host = addr.split(':').next().unwrap_or(addr);
    (!host.is_empty() && host != "127.0.0.1" && host != "localhost" && host != "::1")
        .then(|| format!("tcp://{addr}"))
}

async fn lead_reply(
    cfg: &WorkerCfg,
    slots: &Arc<Semaphore>,
    job: u64,
    subtree: &[u8],
    frontier: &[Dig],
) -> W2D {
    use prost::Message;

    let decline = |why: String| W2D::Decline { job, why };

    // Configured to build at all? Absent daemon is a decline, not an error:
    // this worker simply lends CPU to REAPI actions and nothing else.
    let (Some(bk), Some(reg)) = (&cfg.buildkit_addr, &cfg.registry_addr) else {
        return decline("no buildkitd configured on this worker".into());
    };

    let Ok(def) = bollard_buildkit_proto::pb::Definition::decode(subtree) else {
        return decline("subtree is not a buildkit Definition".into());
    };

    // Re-check what the offerer already checked. One pass over the ops, and
    // a bug on their side cannot ship us a secret or a cache mount.
    let verdict = crate::dispatch::inspect(&def);
    let load = crate::dispatch::Load {
        slots: cfg.slots,
        peer: 0,
        driver: cfg.slots - slots.available_permits(),
    };
    // The same platform we advertised, not the host's: re-checking against
    // the host would refuse exactly the work we said we could take.
    let me = match cfg.buildkit_addr.as_deref() {
        Some(bk) => crate::solve::daemon_platforms(bk)
            .await
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{}/{}", std::env::consts::OS, arch())),
        None => format!("{}/{}", std::env::consts::OS, arch()),
    };
    // The fleet's policy, not this worker's opinion. Three components ask
    // this question - the gateway before offering, the driver before
    // choosing a peer, and here - and for a while they asked three different
    // versions of it: the first two agreed to route a cache-mount subtree
    // and the worker refused every offer of it.
    if let Err(why) = crate::dispatch::consider(load, &verdict, &me, crate::dispatch::policy()) {
        return decline(format!("{why:?}"));
    }

    // Hold a slot for the duration, so this worker's load is honest while
    // it builds and the next offer is declined rather than over-committed.
    let Ok(_permit) = slots.acquire().await else {
        return decline("worker shutting down".into());
    };
    println!(
        "[worker] leading subtree job {job}: {} ops, {} frontier blobs",
        verdict.ops,
        frontier.len()
    );
    // Where does a lead's time GO?
    //
    // 84 leads at ~24s each is most of the 383s by which six machines lose
    // to one, and three remedies have now been aimed at that number without
    // anyone knowing what is inside it. A lead is: the daemon fetching what
    // it needs (base, context, cache), then executing, then pushing the
    // result. Those have very different fixes and the report cannot tell
    // them apart.
    //
    // `fetched` is what THIS worker's registry served during the build -
    // the mirror hop, which is the difference between a worker and home.
    // The DISTRIBUTION is the discriminator, and it needs nothing but a
    // clock. If the first lead on a worker is slow and the rest are quick,
    // the cost is a cold cache paid once per machine. If every lead costs
    // the same, it is per-lead overhead - a mirror hop, or earthly's own
    // per-solve work paid 84 times instead of inline - and no amount of
    // cache seeding touches it.
    //
    // Three remedies have been aimed at this without anyone knowing which
    // shape it has.
    // FETCH versus EXECUTE, which is the split every remedy so far has been
    // chosen without.
    //
    // This worker's registry is in THIS process and serves its daemon's
    // pulls, so the bytes it hands out during a lead are exactly the mirror
    // hop - the inputs a worker must fetch where home reads its own content
    // store. Sampling the counter either side attributes them per lead.
    //
    // It is not a clean fetch/execute split: buildkit interleaves the two.
    // But bytes-per-lead beside duration-per-lead distinguishes "this lead
    // moved 400MB" from "this lead computed for 90 seconds", and those want
    // opposite fixes.
    let bytes_before = crate::registry::SERVED_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let t = std::time::Instant::now();
    // Point any nested earthly at THIS machine's daemon before handing the
    // graph over. earthly forwards its own BUILDKIT_HOST into every RUN, and
    // in a fleet that address is the coordinator's - so a nested build here
    // would dial back across the network and re-enter through one gateway.
    //
    // Only the worker can do this. The converter runs before placement and
    // cannot know which machine will execute the op, and earthly's own
    // machine-independent constant (tcp://buildkitsandbox:8372) resolves for
    // most execs but not for `--privileged --entrypoint` ones, where the
    // nested earthly dies on `could not connect to buildkit: timeout 1m0s`.
    let def = match nested_host(
        std::env::var("REBUCK2_LOCAL_NESTED").as_deref() == Ok("1"),
        std::env::var("REBUCK2_NESTED_HOST").ok().as_deref(),
        cfg.buildkit_addr.as_deref(),
    ) {
        Some(addr) => crate::dispatch::retarget_buildkit_host(&def, &addr),
        None => def,
    };
    let out = crate::solve::build_subtree(bk, reg, job, def).await;
    let moved =
        crate::registry::SERVED_BYTES.load(std::sync::atomic::Ordering::Relaxed) - bytes_before;
    println!(
        "[worker] job {job} took {}ms ({} ops, {} KiB fetched)",
        t.elapsed().as_millis(),
        verdict.ops,
        moved / 1024
    );
    match out {
        Ok(image_ref) => W2D::Led { job, image_ref },
        // A failed subtree is the requester's to rebuild. Reporting it as a
        // decline rather than swallowing it is what stops them waiting.
        Err(e) => {
            // Did the DAEMON survive the attempt?
            //
            // earthly's buildkit fork nil-derefs on the error path for
            // `no active sessions` (llbsolver.(*resultProxy).wrapError,
            // bridge.go:318) and takes the whole daemon with it. Measured:
            // three worker daemons dead inside a minute, while all three
            // workers stayed in the fleet advertising slots and accepting
            // leads they could no longer build.
            //
            // A worker with no daemon is not a slow worker, it is a hole
            // that silently eats every subtree the driver sends it. Leaving
            // is the honest move: the mesh connection drops, the driver
            // stops offering, and the requester builds at home.
            if crate::solve::daemon_platforms(bk).await.is_empty() {
                eprintln!(
                    "[worker] buildkit at {bk} is gone after job {job} - taking no more \
                     work. Last error: {e:#}"
                );
                // Close the slots. Every later offer then declines through
                // the `acquire` above, which is the mechanism that already
                // existed for a worker that cannot build.
                //
                // Closing rather than EXITING, deliberately: this worker's
                // registry still holds blobs the fleet may want, and a
                // process that leaves takes them with it. It stops building
                // and keeps serving.
                slots.close();
                return decline(format!("build failed and daemon died: {e:#}"));
            }
            decline(format!("build failed: {e:#}"))
        }
    }
}

/// `std::env::consts::ARCH` in the spelling buildkit platforms use.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

async fn record_local_ac(store: &Store, action_hash: &str, outcome: &exec::Outcome) {
    if outcome.do_not_cache || outcome.action_result.exit_code != 0 {
        return;
    }
    if let Err(e) = store
        .ac_put(action_hash, &outcome.action_result.encode_to_vec())
        .await
    {
        eprintln!("[worker] local AC row for {action_hash} not written: {e:#}");
    }
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
    /// Of these blobs, the ones THIS worker is responsible for seeding.
    ///
    /// Without this, a prefetch announced to six workers makes six workers
    /// pull the same layers off the coordinator at once - the thundering
    /// herd that `seeder_for` exists to prevent, arriving earlier and
    /// therefore hurting more. Each worker takes its own share; the rest
    /// reach it from peers, at six times the width, which is principle 16.
    async fn my_share(&self, digests: Vec<Dig>, fleet: &[String]) -> Vec<Dig> {
        // The DRIVER's list, not this worker's gossip. Computed locally the
        // shares do not partition: two workers with different peer sets both
        // claim some blobs and neither claims others, so the split leaves
        // gaps and duplicates simultaneously. Falls back to gossip only if
        // the driver sent nothing.
        let ids: Vec<String> = if fleet.is_empty() {
            let p = self.peers.lock().await;
            let mut v: Vec<String> = p.keys().cloned().collect();
            if !v.contains(&self.my_id) {
                v.push(self.my_id.clone());
            }
            v
        } else {
            fleet.to_vec()
        };
        // Alone, or before any gossip has arrived, "my share" is everything -
        // there is nobody to split with, and fetching nothing would make the
        // prefetch silently do nothing at exactly the moment it is most
        // needed.
        if ids.len() <= 1 {
            return digests;
        }
        digests
            .into_iter()
            .filter(|d| seeder_for(&d.hash, &ids).as_deref() == Some(self.my_id.as_str()))
            .collect()
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

    /// Streaming sibling of upload_bytes: file -> wire, O(chunk) memory.
    async fn upload_file(&self, d: &Dig, path: &std::path::Path) -> Result<()> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::Put(d.clone())).await?;
        let mut f = tokio::fs::File::open(path).await?;
        tokio::io::copy(&mut f, &mut send).await?;
        send.finish()?;
        let resp: BlobResp = mesh::recv_frame(&mut recv)
            .await?
            .context("driver closed blob stream")?;
        match resp {
            BlobResp::PutOk => Ok(()),
            other => bail!("blob put rejected: {other:?}"),
        }
    }

    /// `BlobReq::GetByHash` against one peer, dialled directly.
    async fn fetch_by_hash_from(&self, endpoint: &str, hash: &str) -> Result<Vec<u8>> {
        let id: iroh::EndpointId = endpoint
            .parse()
            .map_err(|_| anyhow::anyhow!("bad provider endpoint {endpoint:?} for {hash}"))?;
        let conn = self.ep.connect(id, mesh::ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::GetByHash(hash.to_owned())).await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("provider closed blob stream")?
        {
            BlobResp::Found { size } => Ok(mesh::recv_raw(&mut recv, size).await?),
            other => bail!("provider {endpoint} for {hash}: {other:?}"),
        }
    }

    /// Ask ONE peer to resolve a tag.
    ///
    /// A tag cannot be bloom-routed: a bloom filter answers "do you hold
    /// this content hash", and a tag is a name whose content is exactly what
    /// we are trying to learn. So this asks, rather than knowing where to.
    async fn tag_from(&self, endpoint: &str, key: &str) -> Result<Option<String>> {
        let id: iroh::EndpointId = endpoint
            .parse()
            .map_err(|_| anyhow::anyhow!("bad peer endpoint {endpoint:?} for tag {key}"))?;
        let conn = self.ep.connect(id, mesh::ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::TagGet(key.to_owned())).await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("peer closed tag stream")?
        {
            BlobResp::Tag(found) => Ok(found),
            // A peer too old to know TagGet answers Err. That is a miss,
            // not a fault: mixed-version fleets are the normal case during
            // a rollout.
            BlobResp::Err(_) => Ok(None),
            other => bail!("peer {endpoint} for tag {key}: {other:?}"),
        }
    }

    /// The same question to the driver, on the connection we already hold.
    async fn tag_driver(&self, key: &str) -> Result<Option<String>> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(&mut send, &BlobReq::TagGet(key.to_owned())).await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("driver closed tag stream")?
        {
            BlobResp::Tag(found) => Ok(found),
            BlobResp::Err(_) => Ok(None),
            other => bail!("driver for tag {key}: {other:?}"),
        }
    }

    /// The same question to the driver, on the connection we already hold.
    ///
    /// Names this worker, so the driver can answer `Provider` and point at a
    /// peer that has since acquired the blob. Its view of who holds what is
    /// fresher than ours: it collects every worker's bloom, and we act on the
    /// last copy it broadcast. On a cold fleet that difference is most of the
    /// coordinator's traffic - measured driver=47 against peer=5 per worker.
    async fn fetch_by_hash_driver(&self, hash: &str) -> Result<Vec<u8>> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        mesh::send_frame(
            &mut send,
            &BlobReq::GetByHashAs {
                hash: hash.to_owned(),
                me: self.my_id.clone(),
            },
        )
        .await?;
        send.finish()?;
        match mesh::recv_frame::<BlobResp>(&mut recv)
            .await?
            .context("driver closed blob stream")?
        {
            BlobResp::Found { size } => Ok(mesh::recv_raw(&mut recv, size).await?),
            // Sent to a peer instead. Try it, and fall back to asking the
            // driver for the BYTES if that fails.
            //
            // The fallback is not optional: a bloom lies in the "have it"
            // direction, so the driver can name a peer that does not have the
            // blob. Without this, one false positive turns a fetch into a
            // failed build - the trade for taking the coordinator off the
            // data path.
            BlobResp::Provider { endpoint } => {
                if let Ok(bytes) = self.fetch_by_hash_from(&endpoint, hash).await {
                    self.hits_peer
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(bytes);
                }
                let (mut send, mut recv) = self.conn.open_bi().await?;
                mesh::send_frame(&mut send, &BlobReq::GetByHash(hash.to_owned())).await?;
                send.finish()?;
                match mesh::recv_frame::<BlobResp>(&mut recv)
                    .await?
                    .context("driver closed blob stream on fallback")?
                {
                    BlobResp::Found { size } => Ok(mesh::recv_raw(&mut recv, size).await?),
                    other => bail!("driver fallback for {hash}: {other:?}"),
                }
            }
            other => bail!("driver for {hash}: {other:?}"),
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
                    let expect = Dig {
                        hash: d.hash.clone(),
                        size: size as i64,
                    };
                    self.store.put_stream(Some(&expect), &mut recv).await?;
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

/// The fleet, as a registry sees it.
///
/// `exec::Blobs` needs a `Dig` because REAPI always knows the size; a
/// registry only ever has the hash. Same walk - ours, then a peer the bloom
/// claims, then the driver - asked the one way a registry can ask.
#[async_trait::async_trait]
impl crate::registry::FleetBlobs for RemoteBlobs {
    async fn by_hash(&self, hash: &str) -> Option<Vec<u8>> {
        use std::sync::atomic::Ordering::Relaxed;
        if let Ok(Some(b)) = self.store.get_by_hash(hash).await {
            self.hits_local.fetch_add(1, Relaxed);
            return Some(b);
        }
        let candidates: Vec<String> = {
            let peers = self.peers.lock().await;
            peers
                .iter()
                .filter(|(id, b)| **id != self.my_id && b.contains(hash))
                .map(|(id, _)| id.clone())
                .collect()
        };
        for who in &candidates {
            if let Ok(bytes) = self.fetch_by_hash_from(who, hash).await {
                self.hits_peer.fetch_add(1, Relaxed);
                return Some(bytes);
            }
        }
        // The driver last. It holds what the gateway mirrored - bases and
        // published contexts - which a worker needs and no peer built.
        let bytes = self.fetch_by_hash_driver(hash).await.ok()?;
        self.hits_driver.fetch_add(1, Relaxed);
        Some(bytes)
    }

    async fn tag(&self, key: &str) -> Option<String> {
        // The DRIVER first, and this is the opposite order to `by_hash`.
        //
        // Blobs go peer-first because a bloom filter says which peer has
        // them and the driver should carry as little as possible. A tag has
        // no bloom, so peer-first means asking every worker in turn for
        // something most of them do not have - N dials to learn one string,
        // on the critical path of every cache lookup.
        //
        // The driver is one dial on a connection already open, and for the
        // cache ref specifically it is the likeliest holder anyway.
        if let Ok(Some(h)) = self.tag_driver(key).await {
            return Some(h);
        }
        let peers: Vec<String> = {
            let p = self.peers.lock().await;
            p.keys().filter(|id| **id != self.my_id).cloned().collect()
        };
        for who in &peers {
            if let Ok(Some(h)) = self.tag_from(who, key).await {
                return Some(h);
            }
        }
        None
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
                let expect = Dig {
                    hash: d.hash.clone(),
                    size: size as i64,
                };
                self.store.put_stream(Some(&expect), &mut recv).await?;
                self.hits_driver.fetch_add(1, Relaxed);
                self.store
                    .get(d)
                    .await?
                    .context("just-streamed blob missing")
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
                exec::set_exec(dest).await?;
            }
            return Ok(());
        }
        if !self.store.has(d).await {
            // Pulls into the local store as a side effect.
            let _ = self.get(d).await?;
        }
        // link_out_exec guarantees the exec bit on BOTH paths - including
        // normalizing a mode-stripped shared store inode (bank-seeded
        // blobs staged 0o100644 and died with EACCES, run 29524645875).
        self.store.link_out_exec(d, dest, is_executable).await?;
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
        // Streaming end to end: digest by chunked read, ingest by link or
        // stream, upload straight from the file. Reading whole outputs into
        // memory 64-wide was the ingestion half of the 2.4GB bench peak.
        let d = Store::hash_file(path).await?;
        if self.hardlinks {
            self.store.adopt(&d, path).await?;
        } else {
            let mut f = tokio::fs::File::open(path).await?;
            self.store.put_stream(Some(&d), &mut f).await?;
        }
        if self.upload {
            self.upload_file(&d, path).await?;
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
                            let expect = Dig {
                                hash: d.hash.clone(),
                                size: size as i64,
                            };
                            store.put_stream(Some(&expect), &mut recv).await?;
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

#[cfg(test)]
mod tests {

    #[test]
    fn a_share_is_a_share_and_alone_means_everything() {
        use super::seeder_for;

        // The six shares must PARTITION the set: every blob seeded by
        // exactly one worker. Miss one and it is never pre-positioned;
        // duplicate one and the herd is back, arriving earlier than the lazy
        // path it replaced and therefore hurting more.
        let peers: Vec<String> = (1..=6).map(|n| format!("w{n}")).collect();
        let blobs: Vec<String> = (0..600).map(|n| format!("{n:064x}")).collect();
        let mut seeded = std::collections::BTreeMap::new();
        for b in &blobs {
            let who = seeder_for(b, &peers).expect("a seeder");
            *seeded.entry(who).or_insert(0u32) += 1;
        }
        assert_eq!(
            seeded.values().sum::<u32>(),
            blobs.len() as u32,
            "every blob seeded exactly once"
        );
        assert_eq!(seeded.len(), 6, "and by every worker");

        // A fleet of one is not a fifth of a fleet. Before any gossip
        // arrives the peer list is empty, and splitting then would prefetch
        // nothing at exactly the moment it matters most.
        assert_eq!(
            seeder_for("abc", &["only".to_owned()]).as_deref(),
            Some("only")
        );
    }

    #[test]
    fn every_blob_has_one_agreed_seeder_and_the_load_spreads() {
        use super::seeder_for;

        // The seed is serial today: worker 1 arrives first, finds nothing on
        // any peer, and pulls the whole base off the coordinator - measured
        // at 75 blobs from the driver and 3 from peers - while five workers
        // wait for it. The cascade behind it works (worker 2 then got 26 of
        // 36 from peers); it is the seed that is one machine wide.
        //
        // So each blob gets a designated seeder, chosen from its own hash.
        // Six workers then pull six different sixths of the base off the
        // coordinator at once and exchange the rest.
        let peers: Vec<String> = (1..=6).map(|n| format!("w{n}")).collect();

        // AGREED, without anyone coordinating: every worker computes the
        // same seeder for the same blob, or two of them fetch it and the
        // split has bought nothing.
        let h = "abc123";
        let a = seeder_for(h, &peers);
        assert!(a.is_some());
        assert_eq!(a, seeder_for(h, &peers), "same answer twice");
        let shuffled: Vec<String> = peers.iter().rev().cloned().collect();
        assert_eq!(
            a,
            seeder_for(h, &shuffled),
            "order of the peer list must not matter"
        );

        // SPREAD. A thousand blobs over six workers should not pile up: the
        // point is six seeds at once, so no worker may take a large share.
        let mut hits = std::collections::BTreeMap::new();
        for n in 0..1200 {
            let who = seeder_for(&format!("{:064x}", n), &peers).expect("a seeder");
            *hits.entry(who).or_insert(0u32) += 1;
        }
        assert_eq!(hits.len(), 6, "every worker seeds something");
        let (lo, hi) = (
            *hits.values().min().expect("min"),
            *hits.values().max().expect("max"),
        );
        assert!(hi < lo * 2, "lopsided: {hits:?}");

        // Degenerate cases are not panics.
        assert_eq!(seeder_for("abc", &[]), None);
        assert_eq!(seeder_for("", &peers), seeder_for("", &peers));
    }

    #[test]
    fn a_nested_host_is_never_loopback_and_never_a_surprise() {
        use super::nested_host;

        // OFF unless asked for. Retargeting changes the cache key of any RUN
        // carrying the variable, so a forwarded RUN stops merging across
        // machines - a real cost that must not arrive by accident.
        assert_eq!(nested_host(false, None, Some("tcp://10.0.0.9:8372")), None);

        // The worker's own daemon, normalised to one spelling.
        assert_eq!(
            nested_host(true, None, Some("tcp://10.0.0.9:8372")).as_deref(),
            Some("tcp://10.0.0.9:8372")
        );
        assert_eq!(
            nested_host(true, None, Some("10.0.0.9:8372")).as_deref(),
            Some("tcp://10.0.0.9:8372")
        );

        // NEVER loopback. earthly's IsLocal treats 127.0.0.1 as "a buildkit I
        // manage" and starts its own container from an image that is not
        // published, so the nested build dies on `manifest unknown` before it
        // solves anything. That cost a baseline run to rediscover once.
        for local in ["tcp://127.0.0.1:8372", "localhost:8372", "tcp://::1:8372"] {
            assert_eq!(nested_host(true, None, Some(local)), None, "{local}");
        }

        // An explicit override wins - a worker's daemon address is how the
        // WORKER reaches it, which is not always how an exec can.
        assert_eq!(
            nested_host(
                true,
                Some("tcp://172.17.0.1:8372"),
                Some("tcp://127.0.0.1:8372")
            )
            .as_deref(),
            Some("tcp://172.17.0.1:8372")
        );
        // Nothing configured is nothing done.
        assert_eq!(nested_host(true, None, None), None);
    }

    use super::*;
    use bazel_remote_apis::build::bazel::remote::execution::v2 as re;

    fn outcome(exit_code: i32, do_not_cache: bool) -> exec::Outcome {
        exec::Outcome {
            action_result: re::ActionResult {
                exit_code,
                stdout_raw: b"hello".to_vec(),
                ..Default::default()
            },
            do_not_cache,
        }
    }

    #[test]
    fn party_over_beats_the_connect_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("party-over");

        // Nothing to say yet: keep dialling until the deadline.
        assert!(matches!(retry_verdict(Some(&note), false), Retry::Again));
        // Deadline with no note is the old behaviour: a hard error.
        assert!(matches!(retry_verdict(Some(&note), true), Retry::Fatal));

        std::fs::write(&note, b"").unwrap();
        // The note wins in BOTH directions - arriving after the lap ended
        // is not a failure, so it must not surface as one even when the
        // connect deadline has also passed.
        assert!(matches!(
            retry_verdict(Some(&note), false),
            Retry::PartyOver
        ));
        assert!(matches!(retry_verdict(Some(&note), true), Retry::PartyOver));
        // No note configured at all: deadline decides.
        assert!(matches!(retry_verdict(None, true), Retry::Fatal));
    }

    #[tokio::test]
    async fn banks_only_cacheable_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf()).unwrap();

        record_local_ac(&store, &"a".repeat(64), &outcome(0, false)).await;
        record_local_ac(&store, &"b".repeat(64), &outcome(1, false)).await;
        record_local_ac(&store, &"c".repeat(64), &outcome(0, true)).await;

        let banked = store.ac_get(&"a".repeat(64)).await.expect("row banked");
        assert_eq!(
            banked,
            outcome(0, false).action_result.encode_to_vec(),
            "banked bytes must be the encoded ActionResult"
        );
        assert!(
            store.ac_get(&"b".repeat(64)).await.is_none(),
            "failure rows must not reach the bank - the poison class"
        );
        assert!(
            store.ac_get(&"c".repeat(64)).await.is_none(),
            "do_not_cache rows must not reach the bank"
        );
    }
}
