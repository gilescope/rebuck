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
