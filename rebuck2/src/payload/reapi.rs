//! The REAPI payload: buck2 (and bazel, which is a config of this, not a
//! payload of its own).
//!
//! Every `Action`/`Command`/`ActionResult`/`Directory`/`Tree` decode in the
//! system lives here. The fleet hands over opaque bytes and a [`Blobs`]; this
//! module is the only thing that knows they are protobufs.

use std::path::Path;

use anyhow::Result;
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use prost::Message;

use crate::driver::PlatKey;
use crate::exec::{self, crate_affinity_key};
use crate::mesh::Dig;
use crate::payload::{Done, JobMeta, Payload};
use crate::store::Blobs;

/// Parse REAPI platform properties (`Command.platform` / `Action.platform`).
/// Recognised keys, case-insensitively: `OSFamily`/`os`, `Arch`/`architecture`.
/// Values are matched verbatim against a worker's `std::env::consts` strings
/// ("windows"/"linux"/"macos", "x86_64"/"aarch64").
fn plat_from_properties(platform: Option<&re::Platform>) -> PlatKey {
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

/// Digests an `ActionResult` names directly (outputs, stdout, stderr). Not
/// transitive: a directory output names a *tree proto*, whose interior files
/// this does not reach. See [`Reapi::referenced_digests`] for why that matters.
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

fn hash_of(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// `Dig` is the fleet's own digest; converting it to and from REAPI's is
/// payload business, not the mesh's. Both live here so that `mesh.rs` need not
/// know a protobuf exists.
impl From<&re::Digest> for Dig {
    fn from(d: &re::Digest) -> Self {
        Self {
            hash: d.hash.clone(),
            size: d.size_bytes,
        }
    }
}

impl Dig {
    pub fn to_proto(&self) -> re::Digest {
        re::Digest {
            hash: self.hash.clone(),
            size_bytes: self.size,
        }
    }
}

pub struct Reapi;

#[async_trait::async_trait]
impl Payload for Reapi {
    async fn inspect(&self, spec: &Dig, blobs: &dyn Blobs) -> JobMeta {
        // A spec we cannot read is not a build failure — it is a missing
        // scheduling *hint*. Default meta lets any worker take it and the
        // executor produce the real error.
        let Ok(bytes) = blobs.get(spec).await else {
            return JobMeta::default();
        };
        let Ok(action) = re::Action::decode(bytes.as_slice()) else {
            return JobMeta::default();
        };

        let mut plat = plat_from_properties(action.platform.as_ref());
        let mut affinity_key: Option<String> = None;
        if let Some(cd) = &action.command_digest {
            if let Ok(cmd_bytes) = blobs.get(&cd.into()).await {
                if let Ok(cmd) = re::Command::decode(cmd_bytes.as_slice()) {
                    if plat == PlatKey::default() {
                        // buck2 (and every pre-v2.1 client) puts platform on
                        // Command, not Action. Honour both, Command losing.
                        #[allow(deprecated)]
                        {
                            plat = plat_from_properties(cmd.platform.as_ref());
                        }
                    }
                    affinity_key = crate_affinity_key(&cmd);
                }
            }
        }

        JobMeta {
            plat,
            do_not_cache: action.do_not_cache,
            // Fall back to the input root when no crate prefix is legible —
            // same-input actions still colocate.
            affinity: affinity_key
                .or_else(|| action.input_root_digest.as_ref().map(|d| d.hash.clone()))
                .map(|k| hash_of(&k)),
            input_root: action.input_root_digest.as_ref().map(Into::into),
        }
    }

    /// Transitive: a directory output's top-level digest proves its *tree
    /// proto* exists, not its contents. Reader 29010597531 lost 5,390 actions
    /// to interior files of "validated" directory outputs that existed nowhere.
    /// Expand every tree and demand its files and child `Directory` protos too.
    async fn referenced_digests(&self, result: &[u8], blobs: &dyn Blobs) -> Result<Vec<Dig>> {
        let result = re::ActionResult::decode(result)?;
        let mut digs = result_digests(&result);

        // Trees in parallel: scrub_ac funnels tens of thousands of entries
        // through here 32-wide, so an inner per-directory await multiplies.
        // Decode and expansion stay sequential on the results.
        let fetched = futures::future::join_all(
            result
                .output_directories
                .iter()
                .filter_map(|od| od.tree_digest.as_ref())
                .map(|td| {
                    let tdig: Dig = td.into();
                    async move { blobs.get(&tdig).await }
                }),
        )
        .await;

        for tree_bytes in fetched {
            let tree = re::Tree::decode(tree_bytes?.as_slice())?;
            for dir in tree.root.iter().chain(tree.children.iter()) {
                for f in &dir.files {
                    if let Some(d) = &f.digest {
                        if d.size_bytes > 0 {
                            digs.push(d.into());
                        }
                    }
                }
            }
            // Child `Directory` protos are themselves separate CAS blobs, fetched
            // by digest during materialization. The tree embeds copies, so their
            // digests are computable locally — but they must still EXIST in the
            // CAS, and dropping them from the check would quietly make validation
            // less strict than the bug it was written for.
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
        Ok(digs)
    }

    async fn heavy_inputs(&self, root: &Dig, blobs: &dyn Blobs) -> Vec<(String, i64)> {
        let Ok(bytes) = blobs.get(root).await else {
            return Vec::new();
        };
        let Ok(dir) = re::Directory::decode(bytes.as_slice()) else {
            return Vec::new();
        };
        let mut files: Vec<(String, i64)> = dir
            .files
            .iter()
            .filter_map(|f| f.digest.as_ref().map(|d| (d.hash.clone(), d.size_bytes)))
            .collect();
        // Top-K heaviest decide; small files follow cheaply anyway.
        files.sort_by_key(|(_, s)| -*s);
        files.truncate(8);
        files
    }

    async fn execute(&self, blobs: &dyn Blobs, spec: &Dig, scratch: &Path) -> Result<Done> {
        let outcome = exec::run_action(blobs, spec, scratch).await?;
        Ok(Done {
            result: outcome.action_result.encode_to_vec(),
            do_not_cache: outcome.do_not_cache,
        })
    }
}
