//! Synthetic REAPI benchmark against a live driver: seeds AC entries
//! (a configurable slice deliberately "poisoned" - tree proto present,
//! interior file absent, the zombie-row shape from the 2026-07 hetero
//! saga) then fires concurrent GetActionResult load and reports latency
//! percentiles per verdict. Answers "what does serving a hit/refusal
//! cost?" in seconds on a laptop instead of a 40-minute CI lap.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use prost::Message;
use re::action_cache_client::ActionCacheClient;
use re::content_addressable_storage_client::ContentAddressableStorageClient;

use crate::store::sha256_hex;

pub struct BenchCfg {
    pub grpc: String,
    /// Write withheld interior blobs as a cas/ layout here (a worker's
    /// store volume): the fleet then holds what the driver lacks, and
    /// "poisoned" lookups exercise the MESH validation path instead of
    /// a driver-local miss.
    pub plant_dir: Option<std::path::PathBuf>,
    pub entries: usize,
    /// 0-100: percentage of entries whose tree interior file is withheld.
    pub poisoned_pct: usize,
    pub concurrency: usize,
    pub rounds: usize,
}

fn digest_of(bytes: &[u8]) -> re::Digest {
    re::Digest {
        hash: sha256_hex(bytes),
        size_bytes: bytes.len() as i64,
    }
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

pub async fn run(cfg: BenchCfg) -> Result<()> {
    let channel = tonic::transport::Endpoint::from_shared(cfg.grpc.clone())?
        .connect()
        .await
        .with_context(|| format!("connect {}", cfg.grpc))?;
    let mut cas = ContentAddressableStorageClient::new(channel.clone());
    let mut ac = ActionCacheClient::new(channel.clone());

    // ---- Seed ------------------------------------------------------
    println!(
        "[bench] seeding {} entries ({}% poisoned) ...",
        cfg.entries, cfg.poisoned_pct
    );
    let seed_start = Instant::now();
    let mut keys: Vec<(String, bool)> = Vec::with_capacity(cfg.entries);
    for i in 0..cfg.entries {
        let poisoned = (i * 100 / cfg.entries.max(1)) < cfg.poisoned_pct;
        // Interior file unique per entry; ~1KB, dirs-argsfile sized.
        let interior = format!("-Ldependency={i}\n").repeat(64).into_bytes();
        let interior_dig = digest_of(&interior);
        let tree = re::Tree {
            root: Some(re::Directory {
                files: vec![re::FileNode {
                    name: "dirs".into(),
                    digest: Some(interior_dig.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            children: vec![],
        };
        let tree_bytes = tree.encode_to_vec();
        let tree_dig = digest_of(&tree_bytes);
        let mut reqs = vec![re::batch_update_blobs_request::Request {
            digest: Some(tree_dig.clone()),
            data: tree_bytes,
            compressor: 0,
        }];
        if !poisoned {
            reqs.push(re::batch_update_blobs_request::Request {
                digest: Some(interior_dig),
                data: interior,
                compressor: 0,
            });
        } else if let Some(dir) = &cfg.plant_dir {
            let sub = dir.join("cas").join(&interior_dig.hash[..2]);
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join(&interior_dig.hash), &interior)?;
        }
        cas.batch_update_blobs(re::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: reqs,
            digest_function: 0,
        })
        .await?;
        let result = re::ActionResult {
            output_directories: vec![re::OutputDirectory {
                path: "out".into(),
                tree_digest: Some(tree_dig),
                is_topologically_sorted: false,
                root_directory_digest: None,
            }],
            ..Default::default()
        };
        // Deterministic fake action key per entry.
        let key = sha256_hex(format!("bench-action-{i}").as_bytes());
        ac.update_action_result(re::UpdateActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(re::Digest {
                hash: key.clone(),
                size_bytes: 1,
            }),
            action_result: Some(result),
            results_cache_policy: None,
            digest_function: 0,
        })
        .await?;
        keys.push((key, poisoned));
    }
    println!(
        "[bench] seeded in {:.1}s",
        seed_start.elapsed().as_secs_f64()
    );

    // ---- Fire ------------------------------------------------------
    let keys = Arc::new(keys);
    let fire_start = Instant::now();
    let mut tasks = Vec::new();
    for t in 0..cfg.concurrency {
        let keys = keys.clone();
        let channel = channel.clone();
        let rounds = cfg.rounds;
        tasks.push(tokio::spawn(async move {
            let mut ac = ActionCacheClient::new(channel);
            // (hit_us, refused_us) per this task
            let mut hits: Vec<u128> = Vec::new();
            let mut refused: Vec<u128> = Vec::new();
            for r in 0..rounds {
                for (i, (key, poisoned)) in keys.iter().enumerate() {
                    // Stripe the space across tasks.
                    if i % 7 != (t + r) % 7 {
                        continue;
                    }
                    let started = Instant::now();
                    let res = ac
                        .get_action_result(re::GetActionResultRequest {
                            instance_name: String::new(),
                            action_digest: Some(re::Digest {
                                hash: key.clone(),
                                size_bytes: 1,
                            }),
                            inline_stdout: false,
                            inline_stderr: false,
                            inline_output_files: vec![],
                            digest_function: 0,
                        })
                        .await;
                    let us = started.elapsed().as_micros();
                    match (res.is_ok(), poisoned) {
                        (true, _) => hits.push(us),
                        (false, true) => refused.push(us),
                        (false, false) => eprintln!("[bench] UNEXPECTED miss on clean {key}"),
                    }
                }
            }
            (hits, refused)
        }));
    }
    let mut hits: Vec<u128> = Vec::new();
    let mut refused: Vec<u128> = Vec::new();
    for t in tasks {
        let (h, r) = t.await?;
        hits.extend(h);
        refused.extend(r);
    }
    let wall = fire_start.elapsed().as_secs_f64();
    hits.sort_unstable();
    refused.sort_unstable();
    let total = hits.len() + refused.len();
    println!(
        "[bench] fired {total} lookups in {wall:.2}s = {:.0} rps (concurrency {})",
        total as f64 / wall,
        cfg.concurrency
    );
    for (name, v) in [("hit", &hits), ("refused", &refused)] {
        if v.is_empty() {
            continue;
        }
        println!(
            "[bench] {name:8} n={:<7} p50={}us p90={}us p99={}us max={}us",
            v.len(),
            percentile(v, 0.50),
            percentile(v, 0.90),
            percentile(v, 0.99),
            percentile(v, 0.99999),
        );
    }
    Ok(())
}

/// In-process fleet bench (no Docker): brings up a driver + N workers over
/// the real loopback iroh mesh, PRE-PLACES each action's heavy input on
/// exactly one worker (asymmetric data), fires Execute for all of them,
/// and reports wall time + bytes the driver had to relay across the mesh.
/// Run with locality on vs off to price moving-task-to-data.
pub struct FleetCfg {
    pub workers: usize,
    pub actions: usize,
    pub rlib_kb: usize,
    pub locality: bool,
}

pub async fn fleet(cfg: FleetCfg) -> Result<()> {
    use crate::{driver::Driver, driver::DriverCfg, mesh::Dig, store::Store, worker};
    use std::time::Duration;

    let root = tempfile::tempdir()?.keep();
    let session = format!("bench-fleet-{}", cfg.workers);
    let addr_file = root.join("addr.json");

    let dstore = Arc::new(Store::new(root.join("driver"))?);
    let driver = Driver::new(
        dstore.clone(),
        DriverCfg {
            session: session.clone(),
            min_workers: cfg.workers,
            local_exec: false,
            decentralized: false,
            hardlinks: true,
            cache_failures: false,
            locality: cfg.locality,
            addr_file: Some(addr_file.clone()),
            finalize_file: None,
            scratch: root.join("driver-exec"),
        },
    );
    {
        let d = driver.clone();
        tokio::spawn(async move { d.serve_mesh().await });
    }
    for _ in 0..100 {
        if addr_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Build the action set and PRE-PLACE each heavy rlib on one worker's
    // on-disk store BEFORE the workers boot, so their FIRST bloom (sent
    // immediately on connect) already announces the placement. The driver
    // holds only the tree/command/action protos - never the rlib bytes,
    // so a mis-routed job MUST cross the mesh to fetch.
    let rlib = vec![0xABu8; cfg.rlib_kb * 1024];
    let mut actions: Vec<Dig> = Vec::new();
    let wpaths: Vec<_> = (0..cfg.workers)
        .map(|w| root.join(format!("w{w}")))
        .collect();
    let wstores: Vec<Arc<Store>> = wpaths
        .iter()
        .map(|p| Store::new(p.clone()).map(Arc::new))
        .collect::<Result<_>>()?;
    for i in 0..cfg.actions {
        let mut bytes = rlib.clone();
        bytes.extend_from_slice(&(i as u64).to_le_bytes());
        let rlib_dig = digest_of(&bytes);
        let owner = i % cfg.workers;
        let rlib_d: crate::mesh::Dig = (&rlib_dig).into();
        wstores[owner].put(Some(&rlib_d), &bytes).await?;
        let dir = re::Directory {
            files: vec![re::FileNode {
                name: format!("lib{i}.rlib"),
                digest: Some(rlib_dig),
                is_executable: false,
                node_properties: None,
            }],
            ..Default::default()
        };
        let dir_dig = dstore.put(None, &dir.encode_to_vec()).await?;
        let cmd = re::Command {
            arguments: vec![if cfg!(windows) { "cmd" } else { "true" }.into()],
            output_paths: vec![],
            ..Default::default()
        };
        let cmd_dig = dstore.put(None, &cmd.encode_to_vec()).await?;
        let action = re::Action {
            command_digest: Some(cmd_dig.to_proto()),
            input_root_digest: Some(dir_dig.to_proto()),
            ..Default::default()
        };
        actions.push(dstore.put(None, &action.encode_to_vec()).await?);
    }
    // Now boot the workers over their pre-seeded stores.
    for (w, wstore) in wstores.iter().enumerate() {
        let wstore = wstore.clone();
        let cfgw = worker::WorkerCfg {
            session: session.clone(),
            slots: 8,
            scratch: root.join(format!("w{w}-exec")),
            connect_wait: Duration::from_secs(30),
            driver_addr_file: Some(addr_file.clone()),
            hardlinks: true,
            preloaded_shard: None,
        };
        std::fs::create_dir_all(&cfgw.scratch)?;
        tokio::spawn(async move { worker::run(wstore, cfgw).await });
    }
    for _ in 0..200 {
        if driver.worker_count().await >= cfg.workers {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // First bloom lands right after join; give it a beat to reach the driver.
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!(
        "[fleet] {} workers joined + announced",
        driver.worker_count().await
    );

    let start = std::time::Instant::now();
    let mut tasks = Vec::new();
    for a in &actions {
        let d = driver.clone();
        let a = a.clone();
        tasks.push(tokio::spawn(async move { d.execute(&a).await.map(|_| ()) }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await?.is_ok() {
            ok += 1;
        }
    }
    // --- Hot-CAS lever: client-download latency, relayed vs driver-local.
    // Place small metadata blobs on ONE worker; time get_blob first-touch
    // (relay from worker, then cached) vs second-touch (driver-local) -
    // the exact win of seeding the driver's metadata locally.
    {
        let mut metas: Vec<crate::mesh::Dig> = Vec::new();
        for i in 0..500usize {
            let b = format!("-Ldependency={i}-rmeta\n").repeat(32).into_bytes();
            let dg = digest_of(&b);
            let d: crate::mesh::Dig = (&dg).into();
            wstores[0].put(Some(&d), &b).await?;
            metas.push(d);
        }
        tokio::time::sleep(Duration::from_millis(1500)).await; // bloom
        let t0 = std::time::Instant::now();
        for m in &metas {
            let _ = driver.get_blob(m).await;
        }
        let relay = t0.elapsed().as_secs_f64();
        let t1 = std::time::Instant::now();
        for m in &metas {
            let _ = driver.get_blob(m).await;
        }
        let local = t1.elapsed().as_secs_f64();
        println!(
            "[fleet] metadata reads (500): relay {:.3}s ({:.1}/s) vs driver-local {:.3}s ({:.1}/s) = {:.0}x",
            relay,
            metas.len() as f64 / relay,
            local,
            metas.len() as f64 / local.max(0.0001),
            relay / local.max(0.0001),
        );
    }

    let wall = start.elapsed().as_secs_f64();
    let served: u64 = wstores
        .iter()
        .map(|s| s.read_bytes.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    // Every action's data lives on exactly one worker, so perfect routing
    // moves ZERO bytes across the mesh; the non-locality baseline pays
    // ~(1 - 1/workers) x total (a job lands off-data that fraction of the
    // time and must fetch).
    println!(
        "[fleet] locality={:5} workers={} actions={} rlib={}KB -> {ok} ok in {wall:.2}s, mesh-served {:.1} MB (ideal 0)",
        cfg.locality,
        cfg.workers,
        cfg.actions,
        cfg.rlib_kb,
        served as f64 / (1024.0 * 1024.0),
    );
    Ok(())
}
