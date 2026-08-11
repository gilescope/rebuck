//! rebuck2 — ad-hoc distributed Remote Execution for buck2, over iroh.
//!
//! One binary, two roles:
//!   rebuck2 driver  — beside buck2: serves REAPI on localhost, coordinates
//!                     workers over the iroh mesh.
//!   rebuck2 worker  — anywhere: joins the mesh, executes actions.
//!
//! Rendezvous needs no service: both sides derive the driver's iroh key from
//! `--session` (default $GITHUB_RUN_ID), see mesh.rs.

// A std guard held across an await is a deadlock waiting for a scheduler.
// Clippy cannot see the tokio equivalent - holding one is legal and often
// intended - and that is the one that bit: `job_terminal` held across an
// await by an `if let` scrutinee temporary cut routing from 94 solves to 8.
// This catches the half a machine can catch.
#![warn(clippy::await_holding_lock)]
#![warn(clippy::await_holding_refcell_ref)]

mod bank;
mod bench;
mod dispatch;
mod driver;
mod exec;
mod gateway;
mod github;
mod lease;
mod mech;
mod mesh;
mod norm;
mod proxy;
mod registry;
mod rpc;
mod solve;
mod store;
mod traces;
mod worker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use bazel_remote_apis::google::bytestream as bs;

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

/// Is the harvest's base image actually there to be pulled?
///
/// Checked before any solve, because the alternative is buildkit's version
/// of the same news: `failed to load cache key: <ref>: not found`, which
/// names the cache key it was computing and not the image it could not
/// resolve. Two of the eleven faults in this path presented that way.
///
/// The registry's pull-through covers BLOBS, not manifest tags, so a public
/// image has to be put there deliberately - `docker push` from the host, or
/// `mirror_image` through a peer with a session. This says so rather than
/// leaving it to be rediscovered.
async fn base_is_reachable(base: &str, registry: &str) {
    // A WARNING, never a refusal, and that took two goes to accept.
    //
    // The check exists because `failed to load cache key: <ref>: not found`
    // names the cache key and not the image. But it cannot tell ABSENT from
    // UNREACHABLE: the first version refused a Docker Hub reference that
    // plainly exists, and the second refused
    // `host.docker.internal:15099/...` - which the buildkit container
    // resolves perfectly well and the host running this check does not.
    //
    // Two false refusals of a working setup is enough. It says what it could
    // not confirm and gets out of the way; buildkit is the authority on
    // whether its own base resolves.
    if !base.starts_with(registry) {
        return;
    }
    match crate::solve::image_blobs(base).await {
        Some(b) if !b.is_empty() => {}
        _ => println!(
            "[check] cannot see {base} from here - it may still be fine, since the daemon \
             resolves names this process does not. If the solve below fails with `not \
             found`, that is why: the registry proxies blobs and not manifest tags, so a \
             public image is only there if it was put there."
        ),
    }
}

/// One cache mount, out of a daemon and into the registry.
async fn harvest_one(
    bk: &str,
    registry: &str,
    base: &str,
    id: &str,
    dest: &str,
    input: Option<(Vec<u8>, String)>,
) -> anyhow::Result<()> {
    // WHICH PATH, per id. The count alone cannot say that `go-mod` was
    // observed and `go-build` was not, and a partial file is the likely
    // shape - a run only records the ids the graphs it saw actually mount.
    println!(
        "[harvest] {id}: {}",
        match &input {
            Some(_) => "using the input observed from a real graph",
            None => "RECONSTRUCTING earthly's input, which has been measured wrong",
        }
    );
    let def = dispatch::harvest_graph_with(base, id, dest, input);
    // Job 0: this is not a subtree and shares no numbering with one.
    let digest = solve::build_subtree(bk, registry, 0, def).await?;
    // PULLABLE, not the bare digest `build_subtree` answers with. A digest
    // names content and not a location, which is what lets a result travel;
    // a cache mount's input is an image reference and has to name somewhere,
    // and `docker-image://sha256:...` parses nowhere.
    let reference = solve::pullable(registry, &digest);
    // HOW MUCH came out, and it is not a nicety. A cache that was empty -
    // because the baseline used a different id, or never touched it -
    // harvests an empty layer, seeds nothing, and presents afterwards as
    // "seeding did not pay". Those are opposite findings.
    //
    // BYTES, not the blob count. The count read two for every id in the
    // first run that seeded successfully - config plus one layer - and two
    // is also what an EMPTY cache produces, so it could not tell a harvest
    // that worked from one that found nothing. The size can.
    match solve::image_blobs(&reference).await {
        Some(b) if !b.is_empty() => {
            let bytes: i64 = b.iter().map(|d| d.size).sum();
            println!(
                "[harvest] {id} at {dest} -> {reference}, {} blob(s), {:.1} MiB",
                b.len(),
                bytes as f64 / (1024.0 * 1024.0)
            );
            if bytes < 64 * 1024 {
                println!(
                    "[harvest] WARNING: {id} harvested under 64 KiB - that cache was \
                     effectively empty, so seeding it changes nothing. Which is a \
                     different finding from seeding not paying."
                );
            }
        }
        _ => println!(
            "[harvest] {id} at {dest} -> {reference}, but it names NO blobs - that cache \
             was empty, so seeding it will change nothing"
        ),
    }
    println!("REBUCK2_CACHE_SEEDS={id}={reference}");
    Ok(())
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
        // Stand in front of a buildkitd and report what could be
        // dispatched, forwarding everything unchanged. Point earthly here
        // instead of the daemon:
        //   rebuck2 buildkit-proxy --listen 127.0.0.1:1234 \
        //     --upstream http://127.0.0.1:1235
        // The peer-to-peer OCI mirror on its own, for a worker that lends a
        // buildkitd but runs no driver. Principle 6's mechanism needs
        // somewhere to publish to and pull from.
        "traces" => {
            // `rebuck2 traces <file.jsonl> [top]` - read what a run's spans
            // say, per leg. Committed rather than retyped: the analysis that
            // overturned the `+base` finding was a heredoc, and a heredoc is
            // not something the next run can be compared against.
            let path = args
                .opt("--file")
                .ok_or_else(|| anyhow::anyhow!("traces: --file <traces.jsonl>"))?;
            let top: usize = args.opt("--top").and_then(|t| t.parse().ok()).unwrap_or(12);
            let text = std::fs::read_to_string(&path)?;
            let legs = traces::parse(&text);
            if legs.is_empty() {
                anyhow::bail!("no spans in {path} - did the collector receive anything?");
            }
            traces::report(&legs, top);
            Ok(())
        }
        "harvest-cache" => {
            // `rebuck2 harvest-cache --bk <addr> --registry <host:port>
            //  --id go-mod --dest /go/pkg/mod [--base busybox:1]`
            //
            // Publish a warm cache mount as an image, and print the digest.
            // Feed it back as `REBUCK2_CACHE_SEEDS=go-mod=<digest>` and every
            // dispatched graph naming that id starts from it instead of from
            // nothing.
            //
            // A SUBCOMMAND rather than something the driver does on its own,
            // and deliberately. Harvesting is a solve that can fail in its
            // own ways - no shell in the base, an empty cache, a registry
            // that will not take a 400 MiB layer - and the measurement it
            // exists for is "does a seeded worker go faster", which needs
            // the seed to be a fixed input to the run rather than a thing
            // that may or may not have happened inside it.
            let bk = args.opt("--bk").unwrap_or_else(|| "127.0.0.1:8372".into());
            let registry = args
                .opt("--registry")
                .ok_or_else(|| anyhow::anyhow!("harvest-cache: --registry <host:port>"))?;
            // `--pairs id:path,id:path`, or a single `--id`/`--dest`. The
            // loop lives HERE and not in the workflow, because the shell
            // version of it could not be tested and was wrong: it split on
            // the first colon and rejected `id == path`, which is every
            // mount buildkit keys on its destination.
            let pairs = match args.opt("--pairs") {
                Some(raw) => dispatch::parse_seed_pairs(&raw),
                None => {
                    let id = args.opt("--id").ok_or_else(|| {
                        anyhow::anyhow!("harvest-cache: --pairs <id:path,...> or --id <cache id>")
                    })?;
                    let dest = args
                        .opt("--dest")
                        .ok_or_else(|| anyhow::anyhow!("harvest-cache: --dest <mount path>"))?;
                    vec![(id, dest)]
                }
            };
            if pairs.is_empty() {
                anyhow::bail!("harvest-cache: no usable id:path pair");
            }
            let base = args.opt("--base").unwrap_or_else(|| "busybox:1".into());
            // Best effort per pair, like the step it replaces: a cache that
            // cannot be harvested leaves that mount cold, which is what it
            // was anyway.
            base_is_reachable(&base, &registry).await;
            // WHAT IS ACTUALLY THERE, before harvesting anything.
            //
            // Four harvests came back at 0.0 MiB and the run could not say
            // whether the ids were wrong, the daemon was the wrong one, or
            // the baseline simply had not filled them. The daemon keeps its
            // own accounting and names each cache mount the way
            // `getRefCacheDir` builds the name, so it can be asked.
            let held = solve::cache_mounts(&bk).await;
            if held.is_empty() {
                println!("[harvest] {bk} reports NO cache mounts at all");
            } else {
                println!("[harvest] {bk} holds {} cache mount(s):", held.len());
                for (what, sz) in held.iter().take(12) {
                    println!("[harvest]   {:>9.1} MiB  {what}", *sz as f64 / 1048576.0);
                }
                // A SEEDED mount reads 0.0 MiB here however much it holds:
                // its size is the copy-on-write diff over the seed, not the
                // total. Observed locally - a mount seeded from a 30 MiB
                // image, read back successfully, reported 0.0. So this table
                // answers "was the cache filled by a build" and not "how
                // much can be read out of it".
            }
            // RESOLVE each asked-for id against what the daemon holds. A
            // mount with no `id=` in the Earthfile is keyed
            // `/run/cache/<per-target hash>/<target>`, so
            // `/root/.cache/golangci_lint` names nothing and the harvest
            // would read an empty directory it created itself.
            let ids_held = solve::cache_ids_held(&held);
            // THE REAL INPUTS, if a previous run left them. Reconstructing
            // earthly's cache-mount input was measured wrong - the digest
            // differs, so the key differs, so the harvest reads an empty
            // directory and reports a cold cache. These are the bytes the
            // proxy saw earthly send.
            let inputs = std::env::var("REBUCK2_CACHE_INPUTS_FILE")
                .ok()
                .map(|p| dispatch::expand_home(&p))
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| dispatch::decode_cache_inputs(&t))
                .unwrap_or_default();
            if inputs.is_empty() {
                println!(
                    "[harvest] no observed cache-mount inputs - falling back to a \
                     RECONSTRUCTION of earthly's, which has been measured wrong and \
                     fails silently as an empty harvest"
                );
            } else {
                println!("[harvest] {} observed cache-mount input(s)", inputs.len());
            }
            let mut failed = 0usize;
            for (id, dest) in &pairs {
                let id = match dispatch::resolve_cache_id(id, &ids_held) {
                    Some(found) => found,
                    None => {
                        println!(
                            "[harvest] no cache id {id:?} on this daemon - skipping. \
                             Harvesting it would read an empty directory and report a \
                             cold cache."
                        );
                        failed += 1;
                        continue;
                    }
                };
                let id = &id;
                let input = inputs.get(id).cloned();
                if let Err(e) = harvest_one(&bk, &registry, &base, id, dest, input).await {
                    println!("[harvest] {id} at {dest} failed: {e:#}");
                    failed += 1;
                }
            }
            if failed == pairs.len() {
                anyhow::bail!("harvest-cache: every pair failed");
            }
            Ok(())
        }
        "check-seeding" => {
            // `rebuck2 check-seeding --bk <addr> --registry <host:port>
            //  [--base <image>]`
            //
            // Can this daemon be handed a filled cache mount? Writes a
            // marker into cache A, harvests A, then reads the marker back
            // out of cache B seeded from that harvest.
            //
            // B is a DIFFERENT id, and that is the whole design: read it
            // back out of A and the probe passes by meeting A's own warm
            // mount, proving nothing. The ids carry the process id so a
            // second check on the same daemon does not inherit the first
            // one's mounts.
            //
            // Exists because eight separate faults stopped a seeding run,
            // none of them the mechanism, and each cost a twenty-five
            // minute fleet build to find. This answers "does seeding work
            // here" in about thirty seconds, and it is a fair question for
            // an operator to ask before a run depends on the answer.
            let bk = args.opt("--bk").unwrap_or_else(|| "127.0.0.1:8372".into());
            let registry = args
                .opt("--registry")
                .ok_or_else(|| anyhow::anyhow!("check-seeding: --registry <host:port>"))?;
            let base = args
                .opt("--base")
                .unwrap_or_else(|| format!("{registry}/library/busybox:1"));
            let n = std::process::id();
            let (src, dst) = (format!("seedcheck-src-{n}"), format!("seedcheck-dst-{n}"));
            let marker = "rebuck2-seed-marker";
            // `--fill-mb N` makes the round trip a MEASUREMENT as well as a
            // check. Principle 18's last clause says pre-positioning
            // shortens transfer and not unpack, and the open question is
            // whether a cache big enough to matter is still one it pays to
            // ship: `go-mod` runs to hundreds of megabytes and its miss path
            // is a download from a fast proxy, while `go-build`'s miss path
            // is CPU nothing can avoid.
            let fill: u64 = args
                .opt("--fill-mb")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let write_cmd = if fill > 0 {
                // Incompressible, so the layer is honest about its size. A
                // gigabyte of zeroes ships as almost nothing and would
                // flatter the mechanism enormously.
                format!(
                    "touch /c/{marker} && dd if=/dev/urandom of=/c/bulk bs=1M count={fill} \
                     2>/dev/null"
                )
            } else {
                format!("touch /c/{marker}")
            };

            base_is_reachable(&base, &registry).await;
            let t = std::time::Instant::now();
            println!("[check] writing {marker} into cache {src} ({fill} MiB of bulk)");
            solve::build_subtree(
                &bk,
                &registry,
                0,
                dispatch::cache_probe_graph(&base, &src, "/c", &write_cmd),
            )
            .await
            .map_err(|e| anyhow::anyhow!("could not write to a cache mount: {e:#}"))?;
            let t_write = t.elapsed();

            let t = std::time::Instant::now();
            println!("[check] harvesting {src}");
            let digest = solve::build_subtree(
                &bk,
                &registry,
                0,
                dispatch::harvest_graph(&base, &src, "/c"),
            )
            .await
            .map_err(|e| anyhow::anyhow!("could not harvest a cache mount: {e:#}"))?;
            let t_harvest = t.elapsed();
            let reference = solve::pullable(&registry, &digest);
            println!("[check] harvested {src} -> {reference}");

            // The graph the fleet would dispatch: an unseeded probe, put
            // through the same rewrite `make_portable` applies.
            let probe =
                dispatch::cache_probe_graph(&base, &dst, "/c", &format!("test -f /c/{marker}"));
            let seeds = std::collections::BTreeMap::from([(dst.clone(), reference.clone())]);
            let seeded = dispatch::seed_cache_mounts(&probe, &seeds);
            if seeded.def == probe.def {
                anyhow::bail!(
                    "seed_cache_mounts changed nothing - the mount was not seeded, so the \
                     read below would only prove that an empty cache is empty"
                );
            }

            println!("[check] reading {marker} back out of {dst}, seeded from {reference}");
            let t = std::time::Instant::now();
            match solve::build_subtree(&bk, &registry, 0, seeded).await {
                Ok(_) => {
                    let t_seed = t.elapsed();
                    println!("[check] SEEDING WORKS: a cold {dst} started from {src}'s contents");
                    // Three phases, and only the LAST is a cost a real build
                    // pays per machine. Writing and harvesting happen once,
                    // on one machine; seeding happens on every worker, and
                    // it is the number principle 18's caveat is about.
                    println!(
                        "[check] timings: write {}ms, harvest {}ms, seed+read {}ms{}",
                        t_write.as_millis(),
                        t_harvest.as_millis(),
                        t_seed.as_millis(),
                        if fill > 0 {
                            format!(" for {fill} MiB")
                        } else {
                            String::new()
                        }
                    );
                    Ok(())
                }
                // The probe's own `test -f` failing is the interesting
                // answer and not an error in the rig, so say which.
                Err(e) if dispatch::is_build_verdict(&format!("{e:#}")) => Err(anyhow::anyhow!(
                    "the marker was NOT there: buildkit accepted the graph and the seeded \
                     mount came up empty. {e:#}"
                )),
                Err(e) => Err(anyhow::anyhow!(
                    "the seeded probe could not run at all: {e:#}"
                )),
            }
        }
        "registry" => {
            let store_root: std::path::PathBuf = args
                .opt("--store")
                .map(Into::into)
                .unwrap_or_else(|| default_store("registry"));
            let store = Arc::new(store::Store::new(store_root)?);
            let bind = args
                .opt("--bind")
                .unwrap_or_else(|| "127.0.0.1:5000".into());
            // Pull-through when an origin is configured: a peer with no
            // credentials and no session can then fetch a public base image
            // through the mirror instead of Docker Hub. Principle 9 - the
            // origin registry is a fallback, not a data path.
            let upstream = registry::HttpUpstream::from_env()
                .map(|u| std::sync::Arc::new(u) as std::sync::Arc<dyn registry::Upstream>);
            registry::serve_with_upstream(bind.parse()?, store, upstream).await
        }
        "buildkit-proxy" => {
            let listen = args
                .opt("--listen")
                .unwrap_or_else(|| "127.0.0.1:1234".into());
            let upstream = args
                .opt("--upstream")
                .unwrap_or_else(|| "http://127.0.0.1:1235".into());
            // The proxy places through a driver, so it starts one and
            // serves the mesh: workers join THIS process, exactly as they
            // join a driver, and the gateway offers them work through the
            // same arbitration a subdividing worker gets.
            //
            // No `--peer`. A peer list here was a second way to find a
            // machine and a second way to move a layer, and the mesh is
            // older, tested to 19 workers, and keeps the coordinator off the
            // data path.
            let store_root: std::path::PathBuf = args
                .opt("--store")
                .map(Into::into)
                .unwrap_or_else(|| default_store("proxy"));
            let store = Arc::new(store::Store::new(store_root.clone())?);
            let scratch = store_root.join("exec");
            std::fs::create_dir_all(&scratch)?;
            let d = driver::Driver::new(
                store,
                driver::DriverCfg {
                    session: args.opt("--session").unwrap_or_else(default_session),
                    min_workers: 0,
                    require_shards: 0,
                    // The gateway is the only thing feeding this driver, and
                    // it builds its own share by falling through to the
                    // upstream daemon. REAPI local exec would be a second
                    // executor with nothing to run.
                    local_exec: false,
                    decentralized: args.flag("--decentralized-cas"),
                    hardlinks: true,
                    cache_failures: false,
                    locality: false,
                    prefetch_metadata: false,
                    name_independent: true,
                    addr_file: args.opt("--addr-file").map(Into::into),
                    finalize_file: None,
                    scratch,
                },
            );
            let registry_bind = args.opt("--registry-bind");
            args.done();
            {
                let d = d.clone();
                tokio::spawn(async move {
                    if let Err(e) = d.serve_mesh().await {
                        eprintln!("[proxy] mesh died: {e:#}");
                    }
                });
            }
            // The registry is served over the DRIVER, not over a bare
            // store, which is what makes it mesh-backed: a layer built on
            // one worker is served to another's buildkitd over iroh. A
            // separate `rebuck2 registry` process would be a third copy of
            // the same job and would hold no such index.
            if let Some(bind) = registry_bind {
                let up = registry::HttpUpstream::from_env()
                    .map(|u| Arc::new(u) as Arc<dyn registry::Upstream>);
                let addr: std::net::SocketAddr = bind.parse()?;
                // MeshBacked, not the bare driver. A subtree is built into
                // the BUILDER's store, so the coordinator serving the result
                // to its own daemon is serving something it does not have -
                // which is the whole reason a worker can be on another
                // machine.
                let reg = registry::MeshBacked::new(d.clone(), d.clone());
                tokio::spawn(async move {
                    if let Err(e) = registry::serve_with_upstream(addr, reg, up).await {
                        eprintln!("[proxy] registry died: {e:#}");
                    }
                });
            }
            proxy::serve(listen.parse()?, upstream, d).await
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
                buildkit_addr: args.opt("--buildkit-addr"),
                registry_addr: args.opt("--registry-addr"),
                registry_bind: args.opt("--registry-bind"),
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
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let read = store.read_bytes.load(Relaxed);
                println!(
                    "[stats] store={:.2} GiB served_total={:.2} GiB serve_rate={:.1} MiB/s pending_jobs={} workers={} ac_ok={} ac_fail={} dnc_exec={} queued[{}] mem[{}] grpc[ac {}/{}/{}u {:.2} GiB | casR {} {:.2} GiB | casW {:.2} GiB]",
                    gib(store.stored_bytes.load(Relaxed)),
                    gib(read),
                    (read - last_read) as f64 / (60.0 * 1024.0 * 1024.0),
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

    let addr = format!("127.0.0.1:{grpc_port}").parse()?;
    println!("[driver] REAPI listening on grpc://{addr}");
    let max = 256 * 1024 * 1024; // rustc rlibs can be chunky
    tonic::transport::Server::builder()
        .add_service(
            re::capabilities_server::CapabilitiesServer::new(rpc::Caps)
                .max_decoding_message_size(max),
        )
        .add_service(
            re::content_addressable_storage_server::ContentAddressableStorageServer::new(
                rpc::Cas {
                    driver: d.clone(),
                    stats: rpc_stats.clone(),
                },
            )
            .max_decoding_message_size(max),
        )
        .add_service(
            bs::byte_stream_server::ByteStreamServer::new(rpc::ByteStreamSvc {
                driver: d.clone(),
                stats: rpc_stats.clone(),
            })
            .max_decoding_message_size(max),
        )
        .add_service(
            re::action_cache_server::ActionCacheServer::new(rpc::Ac {
                store: store.clone(),
                stats: rpc_stats.clone(),
                driver: d.clone(),
            })
            .max_decoding_message_size(max),
        )
        .add_service(
            re::execution_server::ExecutionServer::new(rpc::Exec {
                driver: d.clone(),
                stats: rpc_stats.clone(),
            })
            .max_decoding_message_size(max),
        )
        .serve(addr)
        .await
        .context("gRPC server")?;

    mesh.abort();
    bail!("gRPC server exited")
}
