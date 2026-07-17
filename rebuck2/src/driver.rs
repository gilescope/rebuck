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
    /// Quorum by RANGE, not head-count: wait until this many DISTINCT
    /// preloaded shards are held by joined workers before dispatching.
    /// 0 = off. Head-count quorum starts the build while the slowest
    /// range owners are still seeding; the earliest-scheduled actions
    /// (proc-macros, build scripts) then get their AC results refused
    /// as unservable - the referenced blobs ARE banked, their range is
    /// just dark during the join window - and re-execute every lap
    /// (run 29596537112: 283 misses, all join-race).
    pub require_shards: usize,
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
    /// Name-independent caching: consult/populate the canonical action
    /// cache (see norm.rs) so identical work under different labels shares
    /// results. ON by default (--no-name-independent disables): a canonical
    /// hit requires identical normalized command + argsfile content + source
    /// tree, misses degrade silently, and the one behaviour change - twin
    /// labels now get identical symbol hashes, so linking both into ONE
    /// binary fails loudly instead of "working" via label-salted metadata -
    /// is a shape dependency resolvers do not produce.
    pub name_independent: bool,
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
    /// Cached result whose referenced blobs are all fetchable. Boxed:
    /// ActionResult is ~528 bytes and the other variants carry nothing.
    Hit(Box<re::ActionResult>),
    /// Entry exists but at least one referenced blob is gone (evicted CAS):
    /// callers must report a miss so the client re-executes and re-uploads.
    Unservable,
    Miss,
}

/// Every CAS digest a cached result commits the server to delivering.
/// Zero-size blobs are implicit in RE and skipped.
/// One-line host memory/disk readout for the heartbeat. Best-effort and
/// linux-oriented (the driver runs on ubuntu in CI); other hosts report
/// what they can. Never fails - a vitals gap must not cost a heartbeat.
fn host_vitals(store_root: &std::path::Path) -> String {
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            let field = |name: &str| {
                s.lines()
                    .find(|l| l.starts_with(name))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|kb| kb / 1024)
            };
            Some(format!(
                "mem avail {}MB of {}MB, swap free {}MB",
                field("MemAvailable:")?,
                field("MemTotal:")?,
                field("SwapFree:").unwrap_or(0),
            ))
        })
        .unwrap_or_else(|| "mem ?".to_owned());
    // Top memory consumers: WHICH process balloons is the whole question
    // (buck2 daemon vs dice fork vs rebuck2 driver vs client commands) and
    // zero profiling has been done - box totals alone can't attribute.
    let top = std::process::Command::new("ps")
        .args(["-eo", "rss=,comm=", "--sort=-rss"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .take(5)
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let rss_kb: u64 = it.next()?.parse().ok()?;
                    let comm = it.next()?;
                    Some(format!("{comm} {}MB", rss_kb / 1024))
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ps ?".to_owned());
    let disk = std::process::Command::new("df")
        .arg("-Pm")
        .arg(store_root)
        .output()
        .ok()
        .and_then(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            let l = out.lines().nth(1)?;
            let avail = l.split_whitespace().nth(3)?;
            Some(format!("disk avail {avail}MB at store"))
        })
        .unwrap_or_else(|| "disk ?".to_owned());
    format!("{mem}; {disk}; top: {top}")
}

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
    /// Canonical (name-independent) action-cache memo: normalized key ->
    /// encoded normalized ActionResult. Read-mostly hot path beside the
    /// on-disk acn/ namespace (see norm.rs and store::acn_get).
    memo_canonical: tokio::sync::RwLock<HashMap<String, Vec<u8>>>,
    /// Distinct shards acked (redundant assignment: a shard is banked
    /// when ANY of its assignees uploads - union-sync makes both copies
    /// complete, so whichever wins is a full artifact).
    finalized_shards: Mutex<std::collections::BTreeSet<u8>>,
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
            memo_canonical: tokio::sync::RwLock::new(HashMap::new()),
            finalized_shards: Mutex::new(std::collections::BTreeSet::new()),
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
        // D2W::Ping). 20s beat, 90s worker patience. Every third beat
        // carries the driver's memory/disk vitals: workers print them, so
        // when the driver box dies (OOM killed the runner agent on
        // 29232220897) its final vitals survive in every worker's log.
        {
            let this = self.clone();
            let store_root = self.store.root_dir();
            tokio::spawn(async move {
                let mut beat = 0u64;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                    beat += 1;
                    let vitals = if beat.is_multiple_of(3) {
                        Some(host_vitals(&store_root))
                    } else {
                        None
                    };
                    for w in this.workers.lock().await.iter() {
                        let _ = w.tx.send(D2W::Ping {
                            vitals: vitals.clone(),
                        });
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
                    W2D::Hello { .. } => bail!("unexpected second Hello"),
                }
            }
        }
        .await;

        self.workers.lock().await.retain(|w| w.id != worker_id);
        // A departed worker may have been the sole holder behind memoized
        // servable verdicts - revalidate everything from here on.
        self.memo_servable.write().await.clear();
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

    /// Approximate heap held by the driver's big in-memory maps, for the
    /// stats heartbeat: zero memory profiling existed when the driver box
    /// OOM'd (run 29232220897), and these maps are the engine's only
    /// unbounded growth. Counts + estimated MB, cheap enough per minute.
    pub async fn mem_summary(&self) -> String {
        let providers = self.providers.lock().await;
        // hash String (64) + endpoint String (64) + HashMap slot overhead.
        let prov_mb = providers.len() * (64 + 64 + 96) / (1024 * 1024);
        let prov_n = providers.len();
        drop(providers);
        let memo_s = self.memo_servable.read().await;
        let memo_s_mb = memo_s.values().map(|v| v.len() + 128).sum::<usize>() / (1024 * 1024);
        let memo_s_n = memo_s.len();
        drop(memo_s);
        let memo_c = self.memo_canonical.read().await;
        let memo_c_mb = memo_c.values().map(|v| v.len() + 128).sum::<usize>() / (1024 * 1024);
        let memo_c_n = memo_c.len();
        drop(memo_c);
        let blooms_mb = self
            .blooms
            .lock()
            .await
            .values()
            .map(|b| b.bits.len())
            .sum::<usize>()
            / (1024 * 1024);
        format!(
            "prov {prov_n} (~{prov_mb}MB) memoS {memo_s_n} (~{memo_s_mb}MB) memoC {memo_c_n} (~{memo_c_mb}MB) blooms ~{blooms_mb}MB"
        )
    }

    pub async fn pending_jobs(&self) -> usize {
        self.jobs.lock().await.len()
    }

    /// Per-platform queued-work summary for the stats heartbeat, e.g.
    /// "windows/x86_64:12 macos/aarch64:340" ("-" when nothing queued).
    /// A bucket whose oldest job has waited >120s gets its age appended
    /// (":12 oldest 340s!") AND a dedicated warning line naming the job,
    /// its platform, and its affinity owner - a starved-but-live queue
    /// previously looked exactly like an idle one, and one unroutable job
    /// wedged a whole lap invisibly (run 29202575268, affinity deadlock).
    pub async fn queue_summary(&self) -> String {
        let queue = self.queue.lock().await;
        let jobs = self.jobs.lock().await;
        let owners = self.affinity_owner.lock().await;
        let mut parts: Vec<String> = Vec::new();
        for (k, q) in queue.iter().filter(|(_, q)| !q.is_empty()) {
            let os = if k.os.is_empty() { "*" } else { &k.os };
            let arch = if k.arch.is_empty() { "*" } else { &k.arch };
            let oldest = q
                .iter()
                .filter_map(|id| jobs.get(id).map(|j| (*id, j)))
                .max_by_key(|(_, j)| j.submitted.elapsed());
            match oldest {
                Some((id, j)) if j.submitted.elapsed().as_secs() > 120 => {
                    let age = j.submitted.elapsed().as_secs();
                    parts.push(format!("{os}/{arch}:{} oldest {age}s!", q.len()));
                    let owner = j
                        .affinity
                        .and_then(|a| owners.get(&a))
                        .map(|w| format!("worker {w}"))
                        .unwrap_or_else(|| "-".to_owned());
                    println!(
                        "[driver] STARVED job {id} ({}) queued {age}s on {os}/{arch}, affinity owner {owner}",
                        j.action.hash
                    );
                }
                _ => parts.push(format!("{os}/{arch}:{}", q.len())),
            }
        }
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
        // Deterministic range-owner fallback for hintless digests: bloom
        // gossip lags joins, so freshly-seeded banked blobs can have no
        // claimant at validation time - graph-root results then read as
        // unservable and re-execute every lap (runs 29596537112..
        // 29613987710: the persistent-108 class survived shard-coverage
        // gating for exactly this reason). The shard map is not gossip:
        // first hex nibble / 2 names the owner, and its store was seeded
        // before it joined. Same exact HasMany verification either way.
        let shard_owner: HashMap<u8, String> = {
            let w = self.workers.lock().await;
            let mut m = HashMap::new();
            for h in w.iter() {
                if let Some(p) = h.preloaded_shard {
                    m.entry(p).or_insert_with(|| h.endpoint.clone());
                }
            }
            m
        };
        {
            let providers = self.providers.lock().await;
            let blooms = self.blooms.lock().await;
            for (i, d) in digs.iter().enumerate() {
                if self.store.has(d).await {
                    have[i] = true;
                    continue;
                }
                let peer = providers
                    .get(&d.hash)
                    .cloned()
                    .or_else(|| {
                        blooms
                            .iter()
                            .find(|(_, b)| b.contains(&d.hash))
                            .map(|(e, _)| e.clone())
                    })
                    .or_else(|| {
                        let nib = u8::from_str_radix(d.hash.get(..1)?, 16).ok()?;
                        shard_owner.get(&(nib / 2)).cloned()
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
            // Discriminate probe FAILURE (routing/transport - confirmed is
            // None) from an honest "no": the hunt for the persistent-108
            // died in this blind spot twice (banked + owner-held blobs
            // still read unservable, runs 29613987710/29615666747).
            if confirmed.is_none() {
                let n = self.unservable_logged.fetch_add(1, Ordering::Relaxed);
                if n < 20 {
                    println!(
                        "[driver] hasmany PROBE FAILED to {peer} ({} digs, first {})",
                        idxs.len(),
                        idxs.first().map(|&i| digs[i].hash.as_str()).unwrap_or("?")
                    );
                }
            }
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
        // Second chance for denials: hints outrank the range owner, and a
        // bloom FALSE POSITIVE is deterministic per digest - the same
        // wrong worker gets asked every lap, honestly says no, and the
        // owner is never consulted. That was the persistent-108 class
        // (runs 29596537112..29617187131), immune to every join-window
        // gate because it is not a race at all.
        let denied: Vec<usize> = (0..digs.len()).filter(|&i| !have[i]).collect();
        if !denied.is_empty() && !shard_owner.is_empty() {
            let mut retry: HashMap<String, Vec<usize>> = HashMap::new();
            for &i in &denied {
                if let Some(owner) = u8::from_str_radix(&digs[i].hash[..1], 16)
                    .ok()
                    .and_then(|nib| shard_owner.get(&(nib / 2)))
                {
                    retry.entry(owner.clone()).or_default().push(i);
                }
            }
            let verdicts = futures::future::join_all(retry.into_iter().map(|(peer, idxs)| {
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
            let mut providers = self.providers.lock().await;
            for (peer, idxs, confirmed) in verdicts {
                for (k, &i) in idxs.iter().enumerate() {
                    if confirmed
                        .as_ref()
                        .and_then(|v| v.get(k))
                        .copied()
                        .unwrap_or(false)
                    {
                        have[i] = true;
                        providers.insert(digs[i].hash.clone(), peer.clone());
                    }
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
            if let Ok(mut result) = re::ActionResult::decode(cached.as_slice()) {
                crate::norm::ensure_execution_metadata(&mut result);
                return AcLookup::Hit(Box::new(result));
            }
        }
        let Some(bytes) = self.store.ac_get(hash).await else {
            return AcLookup::Miss;
        };
        if let Some(at) = self.memo_unservable.lock().await.get(hash) {
            if at.elapsed() < std::time::Duration::from_secs(120) {
                return AcLookup::Unservable;
            }
        }
        let Ok(mut result) = re::ActionResult::decode(bytes.as_slice()) else {
            // Corrupt entry: re-execution overwrites it. Safer than serving.
            return AcLookup::Miss;
        };
        // Serve-time heal: pre-fix canon-hits were re-cached under the
        // requesting digest WITHOUT execution_metadata (rewrite_result
        // used to strip it) and then banked - buck2's client hard-rejects
        // such rows. Every digest-keyed serve flows through here.
        crate::norm::ensure_execution_metadata(&mut result);
        let mut digs = result_digests(&result);
        // Top-level digests prove a directory output's Tree PROTO exists,
        // not its contents: reader 29010597531 lost 5,390 actions to
        // interior files of validated directory outputs that existed
        // nowhere. Expand each tree (small, cached after first fetch) and
        // demand its files and child Directory protos too.
        // Tree fetches in parallel — scrub_ac funnels tens of thousands of
        // entries through here 32-wide, so an inner per-directory await
        // multiplies. Decode/expand stays sequential on the results.
        let tree_blobs = futures::future::join_all(
            result
                .output_directories
                .iter()
                .filter_map(|od| od.tree_digest.as_ref())
                .map(|td| {
                    let tdig: Dig = td.into();
                    async move { self.get_blob(&tdig).await }
                }),
        )
        .await;
        for (ti, fetched) in tree_blobs.into_iter().enumerate() {
            let Ok(Some(tree_bytes)) = fetched else {
                // The tree arms were a sampling blind spot: run
                // 29611514770 refused 484 lookups with ZERO samples -
                // every one came through here. Name the missing tree.
                let n = self.unservable_logged.fetch_add(1, Ordering::Relaxed);
                if n < 20 {
                    let td = result
                        .output_directories
                        .get(ti)
                        .and_then(|od| od.tree_digest.as_ref())
                        .map(|d| format!("{}/{}", d.hash, d.size_bytes))
                        .unwrap_or_default();
                    println!(
                        "[driver] unservable sample {n}: action {hash} TREE blob missing {td} (dir {})",
                        result
                            .output_directories
                            .get(ti)
                            .map(|od| od.path.as_str())
                            .unwrap_or("?")
                    );
                }
                self.memo_unservable
                    .lock()
                    .await
                    .insert(hash.to_string(), std::time::Instant::now());
                return AcLookup::Unservable;
            };
            let Ok(tree) = re::Tree::decode(tree_bytes.as_slice()) else {
                let n = self.unservable_logged.fetch_add(1, Ordering::Relaxed);
                if n < 20 {
                    println!("[driver] unservable sample {n}: action {hash} TREE decode failed");
                }
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
        self.memo_servable
            .write()
            .await
            .insert(hash.to_string(), Arc::new(bytes));
        AcLookup::Hit(Box::new(result))
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
    async fn locality_pref(&self, input_root: &Dig) -> Option<u64> {
        let bytes = self.get_blob(input_root).await.ok()??;
        let dir = re::Directory::decode(bytes.as_slice()).ok()?;
        // Top-K heaviest files decide; small files follow cheaply anyway.
        let mut files: Vec<(&str, i64)> = dir
            .files
            .iter()
            .filter_map(|f| f.digest.as_ref().map(|d| (d.hash.as_str(), d.size_bytes)))
            .collect();
        files.sort_by_key(|(_, s)| -*s);
        files.truncate(8);
        if files.is_empty() {
            return None;
        }
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
        if self.ensure_blob_local(d).await? {
            return self.store.get(d).await;
        }
        Ok(None)
    }

    /// Read-through presence: make `d` locally available (streaming fetch
    /// into the store), without ever holding the blob in memory. The serve
    /// paths pair this with `Store::copy_out` so large blobs relay at
    /// O(chunk); `get_blob` keeps the Vec API for small proto reads.
    pub async fn ensure_blob_local(&self, d: &Dig) -> Result<bool> {
        if self.store.has(d).await {
            return Ok(true);
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
                    None if !claimed_but_failed => return Ok(false),
                    None => break,
                }
            }
            candidates.dedup();
            let mut denied = 0usize;
            let total = candidates.len();
            for endpoint in candidates {
                match self.fetch_blob_from(&endpoint, d).await {
                    Ok(true) => {
                        self.providers.lock().await.insert(d.hash.clone(), endpoint);
                        return Ok(true);
                    }
                    Ok(false) => {
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
        Ok(false)
    }

    /// One blob fetch from one peer, streamed straight into the store: two
    /// attempts (retry once on a fresh connection if the pooled one went
    /// stale). Ok(false) = peer answered Missing; Err = peer unreachable/
    /// protocol error. Callers treat both as "not from this peer", never
    /// as a global verdict.
    async fn fetch_blob_from(&self, endpoint: &str, d: &Dig) -> Result<bool> {
        for attempt in 0..2 {
            let conn = self.peer_conn(endpoint).await?;
            let res: Result<bool> = async {
                let (mut send, mut recv) = conn.open_bi().await?;
                mesh::send_frame(&mut send, &BlobReq::Get(d.clone())).await?;
                send.finish()?;
                match mesh::recv_frame::<BlobResp>(&mut recv)
                    .await?
                    .context("provider closed blob stream")?
                {
                    BlobResp::Found { size } => {
                        let expect = Dig {
                            hash: d.hash.clone(),
                            size: size as i64,
                        };
                        self.store.put_stream(Some(&expect), &mut recv).await?;
                        Ok(true)
                    }
                    BlobResp::Missing => Ok(false),
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

    /// Fetch a blob by walking the input-root Directory tree to `rel_path`.
    /// Used by canonical-key computation to read @-argsfile content.
    async fn fetch_blob_by_rel_path(
        &self,
        input_root: &Dig,
        rel_path: &str,
    ) -> Result<Option<Vec<u8>>> {
        let mut dir_dig = input_root.clone();
        let mut parts = rel_path.split('/').peekable();
        loop {
            let Some(part) = parts.next() else {
                return Ok(None);
            };
            let Some(bytes) = self.get_blob(&dir_dig).await? else {
                return Ok(None);
            };
            let dir = re::Directory::decode(bytes.as_slice()).context("decode Directory")?;
            if parts.peek().is_none() {
                let Some(f) = dir.files.iter().find(|f| f.name == part) else {
                    return Ok(None);
                };
                let Some(d) = &f.digest else { return Ok(None) };
                return self.get_blob(&d.into()).await;
            }
            let Some(sub) = dir.directories.iter().find(|d| d.name == part) else {
                return Ok(None);
            };
            let Some(d) = &sub.digest else {
                return Ok(None);
            };
            dir_dig = d.into();
        }
    }

    /// Content hash of the input tree: sorted (rel-path, blob-hash) pairs
    /// over every file EXCEPT the argsfiles actually reachable from the
    /// command's @-references (those are normalized separately — their raw
    /// bytes carry label tokens). Exact-path exclusion, not extension
    /// matching: a crate shipping a data file named `*.args` must still
    /// reach the key, or two actions differing only in it would share.
    async fn source_content_hash(
        &self,
        input_root: &Dig,
        exclude: &std::collections::HashSet<String>,
    ) -> Result<[u8; 32]> {
        use sha2::{Digest as _, Sha256};
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut stack: Vec<(Dig, String)> = vec![(input_root.clone(), String::new())];
        while let Some((dig, prefix)) = stack.pop() {
            let Some(bytes) = self.get_blob(&dig).await? else {
                anyhow::bail!("input tree dir {} unavailable", dig.hash);
            };
            let dir = re::Directory::decode(bytes.as_slice()).context("decode Directory")?;
            for f in &dir.files {
                if exclude.contains(&format!("{prefix}{}", f.name)) {
                    continue;
                }
                if let Some(d) = &f.digest {
                    pairs.push((format!("{prefix}{}", f.name), d.hash.clone()));
                }
            }
            for s in &dir.symlinks {
                pairs.push((format!("{prefix}{}", s.name), format!("->{}", s.target)));
            }
            for d in &dir.directories {
                if let Some(dd) = &d.digest {
                    stack.push((dd.into(), format!("{prefix}{}/", d.name)));
                }
            }
        }
        pairs.sort();
        let mut h = Sha256::new();
        for (p, hash) in &pairs {
            h.update(p.as_bytes());
            h.update([0u8]);
            h.update(hash.as_bytes());
            h.update([0u8]);
        }
        Ok(h.finalize().into())
    }

    /// Canonical (name-independent) key for an action: normalized Command,
    /// normalized content of every @-argsfile reachable from the arguments
    /// (one indirection level: argsfiles referencing further @-files are
    /// followed once), and the source-input content hash.
    async fn compute_canonical_key(&self, cmd: &re::Command, input_root: &Dig) -> Result<String> {
        let norm_cmd = crate::norm::normalize_command(cmd);
        let mut argsfiles: Vec<Vec<u8>> = Vec::new();
        let mut reachable: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = cmd
            .arguments
            .iter()
            .filter_map(|a| a.strip_prefix('@').map(str::to_owned))
            .collect();
        let mut depth = 0;
        while !queue.is_empty() && depth < 2 {
            let mut next = Vec::new();
            for rel in queue.drain(..) {
                let Some(bytes) = self.fetch_blob_by_rel_path(input_root, &rel).await? else {
                    anyhow::bail!("argsfile {rel} not reachable in input tree");
                };
                for line in String::from_utf8_lossy(&bytes).split('\n') {
                    if let Some(r) = line.trim().strip_prefix('@') {
                        next.push(r.to_owned());
                    }
                }
                argsfiles.push(crate::norm::normalize_argsfile(&bytes));
                reachable.insert(rel);
            }
            queue = next;
            depth += 1;
        }
        let src = self.source_content_hash(input_root, &reachable).await?;
        Ok(crate::norm::canonical_key_from_parts(
            &norm_cmd, &argsfiles, &src,
        ))
    }

    /// Probe the canonical cache for `cmd`. On a hit whose blobs are still
    /// fetchable, returns the result rewritten to this action's declared
    /// output paths. `Ok(None)` = miss (caller executes and then calls
    /// [`Self::canonical_put`] with the fresh result).
    async fn canonical_probe(
        self: &Arc<Self>,
        key: &str,
        cmd: &re::Command,
    ) -> Result<Option<re::ActionResult>> {
        let cached = {
            let memo = self.memo_canonical.read().await;
            memo.get(key).cloned()
        };
        let cached = match cached {
            Some(b) => Some(b),
            None => self.store.acn_get(key).await,
        };
        let Some(bytes) = cached else { return Ok(None) };
        let Ok(canonical) = re::ActionResult::decode(bytes.as_slice()) else {
            return Ok(None); // corrupt row: re-execution overwrites it
        };
        let rewritten = crate::norm::rewrite_result(canonical, cmd);
        // Same honesty gate as the digest-keyed AC: never serve a result
        // whose referenced blobs the fleet can't deliver.
        let digs = result_digests(&rewritten);
        if !digs.is_empty() && self.has_blobs(&digs).await.iter().any(|p| !p) {
            return Ok(None);
        }
        println!("[driver] canon-hit {key}");
        Ok(Some(rewritten))
    }

    async fn canonical_put(&self, key: String, result: &re::ActionResult) {
        let normalized = crate::norm::normalize_result(result);
        let bytes = normalized.encode_to_vec();
        if let Err(e) = self.store.acn_put(&key, &bytes).await {
            eprintln!("[driver] canon-put {key} failed: {e:#}");
            return;
        }
        println!("[driver] canon-put {key}");
        self.memo_canonical.write().await.insert(key, bytes);
    }

    /// Barrier: block until the agreed pool has formed once. A latch, not a
    /// level check — late joiners always add capacity, and a shrinking pool
    /// never re-blocks dispatch.
    pub async fn await_pool_formed(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.pool_formed.load(Relaxed) {
            return;
        }
        loop {
            let (n, shards) = {
                let w = self.workers.lock().await;
                let shards: std::collections::BTreeSet<u8> =
                    w.iter().filter_map(|h| h.preloaded_shard).collect();
                (w.len(), shards.len())
            };
            if n >= self.cfg.min_workers && shards >= self.cfg.require_shards {
                break;
            }
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
        let (plat, do_not_cache, affinity, input_root, canon_cmd) = match self
            .get_blob(action_digest)
            .await?
        {
            Some(bytes) => match re::Action::decode(bytes.as_slice()) {
                Ok(action) => {
                    let mut plat = PlatKey::from_properties(action.platform.as_ref());
                    let mut affinity_key: Option<String> = None;
                    let mut canon_cmd: Option<re::Command> = None;
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
                                    // pre-v2.1 clients (buck2 included) put
                                    // platform on Command, not Action.
                                    #[allow(deprecated)]
                                    {
                                        plat = PlatKey::from_properties(cmd.platform.as_ref());
                                    }
                                }
                                affinity_key = crate_affinity_key(&cmd);
                                if self.cfg.name_independent && crate::norm::is_rustc_action(&cmd) {
                                    canon_cmd = Some(cmd);
                                }
                            }
                        }
                    }
                    // Fall back to the input root when no crate prefix is
                    // recognisable — same-input actions still colocate.
                    // PLATFORM-SCOPED: the E0460 twin-pinning rationale only
                    // applies within one platform (twins share a config), and
                    // an unscoped key deadlocks under single-daemon sweeps -
                    // content-based output paths carry no config hash, so all
                    // legs' rustc_cfg shared one key, its owner was another
                    // OS's worker, and the job starved forever (run
                    // 29202575268: whole fleet idle behind one queued job).
                    let affinity = affinity_key
                        .or_else(|| action.input_root_digest.as_ref().map(|d| d.hash.clone()))
                        .map(|key| {
                            use std::hash::{Hash, Hasher};
                            let mut h = std::collections::hash_map::DefaultHasher::new();
                            key.hash(&mut h);
                            plat.hash(&mut h);
                            h.finish()
                        });
                    (
                        plat,
                        action.do_not_cache,
                        affinity,
                        action.input_root_digest,
                        canon_cmd,
                    )
                }
                Err(_) => (PlatKey::default(), false, None, None, None),
            },
            None => (PlatKey::default(), false, None, None, None),
        };

        // Name-independent probe: identical work under a different label
        // may already have a canonical result. Failures here degrade to a
        // plain miss - the canonical layer must never fail an action.
        let mut canon_key: Option<String> = None;
        if let (Some(cmd), Some(root)) = (&canon_cmd, &input_root) {
            let root: Dig = root.into();
            match self.compute_canonical_key(cmd, &root).await {
                Ok(key) => {
                    if let Ok(Some(result)) = self.canonical_probe(&key, cmd).await {
                        return Ok(exec::Outcome {
                            action_result: result,
                            do_not_cache,
                        });
                    }
                    canon_key = Some(key);
                }
                Err(e) => {
                    // Diagnosable but non-fatal: unsupported shapes just miss.
                    eprintln!("[driver] canon-key skip: {e:#}");
                }
            }
        }
        let locality = if self.cfg.locality {
            match &input_root {
                Some(d) => self.locality_pref(&d.into()).await,
                None => None,
            }
        } else {
            None
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
            .entry(plat)
            .or_default()
            .push_back(job_id);
        self.pump().await;

        let action_result = rx
            .await
            .context("job dropped without completion")?
            .map_err(|e| anyhow::anyhow!("execution failed: {e}"))?;

        // Populate the canonical cache with the fresh result so the next
        // differently-named twin hits. Successes only: failures are not
        // name-independent facts (they may be env-transient).
        if let Some(key) = canon_key {
            if action_result.exit_code == 0 && !do_not_cache {
                self.canonical_put(key, &action_result).await;
            }
        }

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
        // link_out_exec guarantees the exec bit on BOTH paths - including
        // normalizing a mode-stripped shared store inode (bank-seeded
        // blobs staged 0o100644 and died with EACCES, run 29524645875).
        self.store.link_out_exec(d, dest, is_executable).await?;
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
                match driver.ensure_blob_local(&d).await {
                    Ok(true) => {
                        mesh::send_frame(
                            &mut send,
                            &BlobResp::Found {
                                size: d.size as u64,
                            },
                        )
                        .await?;
                        driver.store.copy_out(&d, &mut send).await?;
                    }
                    Ok(false) => mesh::send_frame(&mut send, &BlobResp::Missing).await?,
                    Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
                }
            }
        }
        BlobReq::Put(d) => match driver.store.put_stream(Some(&d), &mut recv).await {
            Ok(_) => mesh::send_frame(&mut send, &BlobResp::PutOk).await?,
            Err(e) => mesh::send_frame(&mut send, &BlobResp::Err(format!("{e:#}"))).await?,
        },
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
                match driver.ensure_blob_local(d).await {
                    Ok(true) => {
                        mesh::send_frame(
                            &mut send,
                            &BlobResp::Found {
                                size: d.size as u64,
                            },
                        )
                        .await?;
                        driver.store.copy_out(d, &mut send).await?;
                    }
                    Ok(false) => mesh::send_frame(&mut send, &BlobResp::Missing).await?,
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

    fn test_driver(local_exec: bool) -> Arc<Driver> {
        test_driver_min(local_exec, 0)
    }

    fn test_driver_with(f: impl FnOnce(&mut DriverCfg)) -> Arc<Driver> {
        let dir = tempfile::tempdir().unwrap().keep();
        let mut cfg = DriverCfg {
            session: "test".into(),
            min_workers: 0,
            require_shards: 0,
            local_exec: false,
            decentralized: false,
            hardlinks: true,
            addr_file: None,
            finalize_file: None,
            cache_failures: false,
            locality: false,
            prefetch_metadata: false,
            name_independent: true,
            scratch: std::env::temp_dir(),
        };
        f(&mut cfg);
        Driver::new(Arc::new(Store::new(dir).unwrap()), cfg)
    }

    fn test_driver_min(local_exec: bool, min_workers: usize) -> Arc<Driver> {
        let dir = tempfile::tempdir().unwrap().keep();
        Driver::new(
            Arc::new(Store::new(dir).unwrap()),
            DriverCfg {
                session: "test".into(),
                min_workers,
                require_shards: 0,
                local_exec,
                decentralized: false,
                hardlinks: true,
                addr_file: None,
                finalize_file: None,
                cache_failures: false,
                locality: false,
                prefetch_metadata: false,
                name_independent: false,
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
    /// The caching dream end-to-end at driver level: two actions compiling
    /// identical source under DIFFERENT labels compute one canonical key;
    /// the first execution's canonical_put lets the second label
    /// canonical_probe a hit whose paths are its own and whose blob
    /// digests are the original's.
    #[tokio::test]
    async fn name_independent_twins_share_one_canonical_row() {
        let d = test_driver_with(|c| c.name_independent = true);

        // Shared source file + per-label argsfiles in per-label trees.
        let src = b"pub fn f() {}".to_vec();
        let src_dig = d.store.put(None, &src).await.unwrap();
        let mk_tree = |args: Vec<u8>| {
            let store = d.store.clone();
            let src_dig = src_dig.clone();
            async move {
                let args_dig = store.put(None, &args).await.unwrap();
                let dir = re::Directory {
                    files: vec![
                        re::FileNode {
                            name: "lib.rs".into(),
                            digest: Some(src_dig.to_proto()),
                            ..Default::default()
                        },
                        re::FileNode {
                            name: "cmd.args".into(),
                            digest: Some(args_dig.to_proto()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                };
                let bytes = dir.encode_to_vec();
                store.put(None, &bytes).await.unwrap()
            }
        };
        let main_out = "buck-out/s/art/p/__adler-1__/a1b2c3d4e5f60718/lib.rmeta";
        let snap_out = "buck-out/s/art/p/snapshots/2024-11/__adler-1__/ffee001122334455/lib.rmeta";
        let root_main = mk_tree(
            format!("-Cmetadata=fixups//third-party:adler-1#aa\n{main_out}\n").into_bytes(),
        )
        .await;
        let root_snap = mk_tree(
            format!("-Cmetadata=fixups//third-party/snapshots/2024-11:adler-1#bb\n{snap_out}\n")
                .into_bytes(),
        )
        .await;
        let mk_cmd = |out: &str| re::Command {
            arguments: vec![
                "python3".into(),
                "rustc_action.py".into(),
                "@cmd.args".into(),
            ],
            output_paths: vec![out.to_owned()],
            ..Default::default()
        };
        let cmd_main = mk_cmd(main_out);
        let cmd_snap = mk_cmd(snap_out);

        // Different labels, one canonical key.
        let k1 = d
            .compute_canonical_key(&cmd_main, &root_main)
            .await
            .unwrap();
        let k2 = d
            .compute_canonical_key(&cmd_snap, &root_snap)
            .await
            .unwrap();
        assert_eq!(k1, k2, "labels must not reach the canonical key");

        // First build populates; the twin hits with ITS paths, SAME digest.
        let rmeta = d.store.put(None, b"compiled bytes").await.unwrap();
        let result = re::ActionResult {
            output_files: vec![re::OutputFile {
                path: main_out.into(),
                digest: Some(rmeta.to_proto()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(d.canonical_probe(&k1, &cmd_main).await.unwrap().is_none());
        d.canonical_put(k1, &result).await;
        let hit = d
            .canonical_probe(&k2, &cmd_snap)
            .await
            .unwrap()
            .expect("twin must hit");
        assert_eq!(hit.output_files[0].path, snap_out);
        assert_eq!(
            hit.output_files[0].digest.as_ref().unwrap().hash,
            rmeta.hash
        );

        // Divergent source = different key: the dream never lies.
        let src2 = d.store.put(None, b"pub fn f() { panic!() }").await.unwrap();
        let dir2 = re::Directory {
            files: vec![
                re::FileNode {
                    name: "lib.rs".into(),
                    digest: Some(src2.to_proto()),
                    ..Default::default()
                },
                re::FileNode {
                    name: "cmd.args".into(),
                    digest: Some(
                        d.store
                            .put(
                                None,
                                format!("-Cmetadata=x:adler-1#cc\n{snap_out}\n").as_bytes(),
                            )
                            .await
                            .unwrap()
                            .to_proto(),
                    ),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let root2 = d.store.put(None, &dir2.encode_to_vec()).await.unwrap();
        let k3 = d.compute_canonical_key(&cmd_snap, &root2).await.unwrap();
        assert_ne!(k2, k3, "source divergence must change the key");
    }

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
                    d2.complete(job, Ok(re::ActionResult::default())).await;
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
