//! rebuck2 — ad-hoc distributed Remote Execution for buck2, over iroh.
//!
//! One binary, two roles:
//!   rebuck2 driver  — beside buck2: serves REAPI on localhost, coordinates
//!                     workers over the iroh mesh.
//!   rebuck2 worker  — anywhere: joins the mesh, executes actions.
//!
//! Rendezvous needs no service: both sides derive the driver's iroh key from
//! `--session` (default $GITHUB_RUN_ID), see mesh.rs.

mod bank;
mod bench;
mod driver;
mod exec;
mod github;
mod mesh;
mod norm;
mod rpc;
mod store;
mod worker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

fn usage() -> ! {
    eprintln!(
        "usage: rebuck2 driver [--grpc-port N] [--require-shards N] [--store DIR] [--session S] [--locality] \
         [--min-workers N] [--no-local-exec] [--decentralized-cas] [--no-hardlinks] [--no-reflink] [--cache-failures] [--no-name-independent]\n       \
         rebuck2 worker [--store DIR] [--session S] [--slots N] [--preloaded-shard N] [--connect-wait-secs N] [--no-hardlinks] [--no-reflink]\n       \
         rebuck2 verify-store --store DIR\n       \
         rebuck2 bench [--grpc URL] [--entries N] [--poisoned-pct P] [--plant-dir DIR] [--concurrency C] [--rounds R]"
    );
    std::process::exit(2)
}

struct Args(Vec<String>);

impl Args {
    fn opt(&mut self, name: &str) -> Option<String> {
        let i = self.0.iter().position(|a| a == name)?;
        if i + 1 >= self.0.len() {
            usage()
        }
        self.0.remove(i);
        Some(self.0.remove(i))
    }
    fn flag(&mut self, name: &str) -> bool {
        let i = self.0.iter().position(|a| a == name);
        if let Some(i) = i {
            self.0.remove(i);
            true
        } else {
            false
        }
    }
    fn done(self) {
        if let Some(a) = self.0.first() {
            eprintln!("unknown argument: {a}");
            usage()
        }
    }
}

fn default_store(role: &str) -> std::path::PathBuf {
    dirs_home().join(".cache").join("rebuck2").join(role)
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(Into::into)
        .unwrap_or_else(|| ".".into())
}

fn default_session() -> String {
    std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| "local".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        usage()
    }
    let role = argv.remove(0);
    let mut args = Args(argv);
    match role.as_str() {
        // Deterministic file munging for the CI store bank - no engine, no
        // mesh, no store handle. Takes its own argv so the bank verbs keep
        // the flat shape their callers already use.
        "bank" => bank::run(&args.0).await,
        "driver" => run_driver(args).await,
        "bench" => {
            let cfg = bench::BenchCfg {
                grpc: args
                    .opt("--grpc")
                    .unwrap_or_else(|| "http://127.0.0.1:9092".into()),
                plant_dir: args.opt("--plant-dir").map(Into::into),
                entries: args
                    .opt("--entries")
                    .map(|s| s.parse().expect("--entries: number"))
                    .unwrap_or(2000),
                poisoned_pct: args
                    .opt("--poisoned-pct")
                    .map(|s| s.parse().expect("--poisoned-pct: 0-100"))
                    .unwrap_or(20),
                concurrency: args
                    .opt("--concurrency")
                    .map(|s| s.parse().expect("--concurrency: number"))
                    .unwrap_or(16),
                rounds: args
                    .opt("--rounds")
                    .map(|s| s.parse().expect("--rounds: number"))
                    .unwrap_or(3),
            };
            args.done();
            bench::run(cfg).await
        }
        "bench-fleet" => {
            let cfg = bench::FleetCfg {
                workers: args
                    .opt("--workers")
                    .map(|s| s.parse().expect("--workers"))
                    .unwrap_or(4),
                actions: args
                    .opt("--actions")
                    .map(|s| s.parse().expect("--actions"))
                    .unwrap_or(200),
                rlib_kb: args
                    .opt("--rlib-kb")
                    .map(|s| s.parse().expect("--rlib-kb"))
                    .unwrap_or(512),
                locality: args.flag("--locality"),
                prefetch: args.flag("--prefetch"),
            };
            let assert = args.flag("--assert");
            args.done();
            let m = bench::fleet(cfg).await?;
            if assert {
                // CI perf gate: fail loudly on a metrics regression.
                anyhow::ensure!(m.ok > 0, "no actions completed");
                anyhow::ensure!(
                    m.meta_local_per_s > m.meta_relay_per_s * 2.0,
                    "driver-local reads not beating relay: local={:.0}/s relay={:.0}/s",
                    m.meta_local_per_s,
                    m.meta_relay_per_s
                );
                println!("[fleet] ASSERT OK");
            }
            Ok(())
        }
        "verify-store" => {
            let dir: std::path::PathBuf = args
                .opt("--store")
                .map(Into::into)
                .unwrap_or_else(|| usage());
            args.done();
            let (ok, bad) = store::verify_cas(&dir)?;
            println!("verify-store: {ok} verified, {bad} rejected");
            if bad > 0 {
                eprintln!(
                    "verify-store: WARNING - rejected blobs suggest a poisoned or corrupt shard artifact"
                );
            }
            Ok(())
        }
        "worker" => {
            let store_root: std::path::PathBuf = args
                .opt("--store")
                .map(Into::into)
                .unwrap_or_else(|| default_store("worker"));
            let store = Arc::new(store::Store::new(store_root.clone())?);
            if args.flag("--no-reflink") {
                store.disable_clone();
            }
            let cfg = worker::WorkerCfg {
                session: args.opt("--session").unwrap_or_else(default_session),
                slots: args
                    .opt("--slots")
                    .map(|s| s.parse().expect("--slots: number"))
                    .unwrap_or_else(|| {
                        std::thread::available_parallelism()
                            .map(|n| n.get())
                            .unwrap_or(2)
                    }),
                // Same volume as the store — hardlinks die of EXDEV when
                // /tmp is tmpfs (ubuntu >= 24.10).
                scratch: store_root.join("exec"),
                driver_addr_file: args.opt("--driver-addr-file").map(Into::into),
                connect_wait: Duration::from_secs(
                    args.opt("--connect-wait-secs")
                        .map(|s| s.parse().expect("--connect-wait-secs: number"))
                        .unwrap_or(600),
                ),
                hardlinks: !args.flag("--no-hardlinks"),
                preloaded_shard: args
                    .opt("--preloaded-shard")
                    .map(|s| s.parse().expect("--preloaded-shard: number")),
                give_up_file: args.opt("--give-up-file").map(Into::into),
            };
            args.done();
            std::fs::create_dir_all(&cfg.scratch)?;
            worker::run(store, cfg).await
        }
        _ => usage(),
    }
}

async fn run_driver(mut args: Args) -> Result<()> {
    let grpc_port: u16 = args
        .opt("--grpc-port")
        .map(|s| s.parse().expect("--grpc-port: port"))
        .unwrap_or(9092);
    let store_root: std::path::PathBuf = args
        .opt("--store")
        .map(Into::into)
        .unwrap_or_else(|| default_store("driver"));
    let store = Arc::new(store::Store::new(store_root.clone())?);
    if args.flag("--no-reflink") {
        store.disable_clone();
    }
    let scratch = store_root.join("exec");
    std::fs::create_dir_all(&scratch)?;
    let cfg = driver::DriverCfg {
        session: args.opt("--session").unwrap_or_else(default_session),
        min_workers: args
            .opt("--min-workers")
            .map(|s| s.parse().expect("--min-workers: number"))
            .unwrap_or(0),
        require_shards: args
            .opt("--require-shards")
            .map(|s| s.parse().expect("--require-shards: number"))
            .unwrap_or(0),
        local_exec: !args.flag("--no-local-exec"),
        decentralized: args.flag("--decentralized-cas"),
        hardlinks: !args.flag("--no-hardlinks"),
        cache_failures: args.flag("--cache-failures"),
        locality: args.flag("--locality"),
        prefetch_metadata: args.flag("--prefetch-metadata"),
        name_independent: !args.flag("--no-name-independent"),
        addr_file: args.opt("--addr-file").map(Into::into),
        finalize_file: args.opt("--finalize-file").map(Into::into),
        scratch,
    };
    args.done();

    let d = driver::Driver::new(store.clone(), cfg);

    let mesh = {
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = d.serve_mesh().await {
                eprintln!("[driver] mesh died: {e:#}");
            }
        })
    };

    let rpc_stats = Arc::new(rpc::RpcStats::default());

    // Once-a-minute heartbeat: egress saturation and disk pressure are the
    // driver's two failure horizons — make both visible in the job log.
    {
        let store = store.clone();
        let d = d.clone();
        let rs = rpc_stats.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
            let mut last_read = 0u64;
            let mut idle_mins = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let read = store.read_bytes.load(Relaxed);

                // Silence on the REAPI port is not idleness - it is the shape
                // of a build that will never start. Say so, with the two
                // diagnoses spelled out, rather than leaving a reader to infer
                // it from a row of zeroes (run 31157521034: four hours of
                // `ac_ok=0 ac_fail=0` that nobody could interpret).
                let conns = rs.conns.load(Relaxed);
                let asked = rs.ac_hits.load(Relaxed)
                    + rs.ac_misses.load(Relaxed)
                    + rs.blobs_read.load(Relaxed);
                if asked == 0 {
                    idle_mins += 1;
                    if idle_mins.is_multiple_of(5) {
                        if conns == 0 {
                            eprintln!(
                                "[driver] WARNING: {idle_mins}m serving and NOTHING has connected \
                                 to the REAPI port. buck2 is not dialling this address - check \
                                 .buckconfig.local [buck2_re_client] against the port above."
                            );
                        } else {
                            eprintln!(
                                "[driver] WARNING: {idle_mins}m serving, {conns} connection(s) \
                                 accepted but ZERO requests. buck2 reached us and is not asking - \
                                 check execution_platforms is in-graph, not passed via --config."
                            );
                        }
                    }
                } else {
                    idle_mins = 0;
                }

                println!(
                    "[stats] store={:.2} GiB served_total={:.2} GiB serve_rate={:.1} MiB/s conns={} pending_jobs={} workers={} ac_ok={} ac_fail={} dnc_exec={} queued[{}] mem[{}] grpc[ac {}/{}/{}u {:.2} GiB | casR {} {:.2} GiB | casW {:.2} GiB]",
                    gib(store.stored_bytes.load(Relaxed)),
                    gib(read),
                    (read - last_read) as f64 / (60.0 * 1024.0 * 1024.0),
                    conns,
                    d.pending_jobs().await,
                    d.worker_count().await,
                    d.ac_hit_ok.load(Relaxed),
                    d.ac_hit_fail.load(Relaxed),
                    d.dnc_exec.load(Relaxed),
                    d.queue_summary().await,
                    d.mem_summary().await,
                    rs.ac_hits.load(Relaxed),
                    rs.ac_misses.load(Relaxed),
                    rs.ac_unservable.load(Relaxed),
                    gib(rs.ac_bytes.load(Relaxed)),
                    rs.blobs_read.load(Relaxed),
                    gib(rs.blob_read_bytes.load(Relaxed)),
                    gib(rs.blob_write_bytes.load(Relaxed)),
                );
                last_read = read;
            }
        });
    }

    // Bind BEFORE announcing. The driver action gates the build legs on this
    // line, so printing it ahead of the bind makes it a claim rather than a
    // fact - and a leg released against an unbound port waits forever.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{grpc_port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding REAPI listener on {addr}"))?;
    // Count accepted connections so the heartbeat can tell "nobody dialled"
    // from "dialled, got nothing back" - the two are indistinguishable in
    // every other counter, and that ambiguity cost a four-hour lap.
    let incoming = rpc::counting_incoming(listener, rpc_stats.clone());
    let server = tokio::spawn(
        rpc::router(d.clone(), store.clone(), rpc_stats.clone()).serve_with_incoming(incoming),
    );

    // A bound socket is not a serving one. Prove a client can round-trip
    // before anything downstream is released against this address: the build
    // legs start on the line below, and a leg that dials a bound-but-silent
    // port waits forever rather than failing.
    rpc::self_check(addr).await?;
    println!("[driver] REAPI listening on grpc://{addr} (round-trip verified)");

    server
        .await
        .context("gRPC server task")?
        .context("gRPC server")?;

    mesh.abort();
    bail!("gRPC server exited")
}
