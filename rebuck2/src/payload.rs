//! The payload seam: what the fleet must ask of a build system it cannot read.
//!
//! The fleet — mesh, CAS, queue, blooms, providers, shards, requeue — schedules
//! and replicates **opaque blobs**. Only a [`Payload`] knows what those blobs
//! mean. Everything REAPI-shaped (`Action`, `Command`, `ActionResult`,
//! `Directory`, `Tree`) lives behind this trait, so a second payload (BuildKit)
//! plugs in without the fleet learning a new vocabulary.
//!
//! **The invariant:** the fleet never decodes a spec or a result. Reaching for
//! a proto type in fleet code means the hook belongs here instead.
//!
//! Not three payloads, two families: bazel speaks REAPI, so it is a *config* of
//! [`reapi`], not a third implementation. Two data points are enough to find a
//! seam; three are enough to draw it in the wrong place.

use std::path::Path;

use anyhow::Result;

use crate::driver::PlatKey;
use crate::mesh::Dig;
use crate::store::Blobs;

pub mod reapi;

/// Everything the fleet needs in order to *schedule* a job whose spec it cannot
/// read. Produced by [`Payload::inspect`].
#[derive(Default, Debug, Clone)]
pub struct JobMeta {
    /// Which workers may run this. An empty axis means "any".
    pub plat: PlatKey,
    /// Never cache, and never merge in-flight duplicates. REAPI is explicit
    /// that requests for such a job may not be coalesced, and buck2's prelude
    /// uses them for diagnostic wrappers that want a genuine re-run.
    pub do_not_cache: bool,
    /// Pin every job sharing this key to ONE worker, in ONE directory.
    ///
    /// buck2: a crate's pipelined twins (rustc folds `env!`-read paths into the
    /// SVH, so twins in different dirs are link-incompatible — E0460).
    /// buildkit: an Earthly target (its `type=cache` mount is node-local, so a
    /// migrated target loses its warm cargo/npm registry).
    /// Same mechanism, same reason: some state will not travel.
    pub affinity: Option<u64>,
    /// Root of the job's input tree, for delay-scheduled locality (move the
    /// task to the data). `None` disables the preference for this job.
    pub input_root: Option<Dig>,
}

/// A finished job: the payload's own encoding of its result, and whether the
/// fleet may cache it. The fleet stores and ships these bytes without ever
/// looking inside.
pub struct Done {
    pub result: Vec<u8>,
    pub do_not_cache: bool,
}

/// The contract between the generic fleet and a specific build system.
///
/// Every method is handed a [`Blobs`] rather than reaching for a store: a
/// payload running on a worker resolves blobs over the mesh, and one running on
/// the driver resolves them locally, and neither should care which.
#[async_trait::async_trait]
pub trait Payload: Send + Sync + 'static {
    /// Read a job spec (itself a CAS blob) into scheduling metadata.
    ///
    /// Must not fail the job: a spec the payload cannot parse yields
    /// `JobMeta::default()` (any platform, no affinity), because a scheduling
    /// hint is not worth failing a build over — the executor will produce the
    /// real error.
    async fn inspect(&self, spec: &Dig, blobs: &dyn Blobs) -> JobMeta;

    /// Every blob a cached result references, **transitively** (a directory
    /// output names a tree proto, whose interior files must exist too).
    ///
    /// This backs the validated-AC invariant: the fleet refuses to serve a
    /// cache hit whose blobs it cannot produce. That invariant is generic and
    /// load-bearing — serving blob-less hits cost 17k actions and 34k client
    /// extract failures (see docs/optimizations.md) — but only the *decoding*
    /// is payload-specific.
    async fn referenced_digests(&self, result: &[u8], blobs: &dyn Blobs) -> Result<Vec<Dig>>;

    /// The heaviest inputs under `root`, as `(hash, size)`, for locality
    /// dispatch. Cheap and best-effort: an empty answer just means "no
    /// preference".
    async fn heavy_inputs(&self, root: &Dig, blobs: &dyn Blobs) -> Vec<(String, i64)>;

    /// Run one job to completion, in-process. Used by the worker and by the
    /// driver's local fallback; the only difference between them is which
    /// `Blobs` they hand over.
    async fn execute(&self, blobs: &dyn Blobs, spec: &Dig, scratch: &Path) -> Result<Done>;
}

#[cfg(test)]
mod tests {
    /// The seam, enforced. The fleet schedules and replicates opaque blobs; the
    /// moment a fleet module names a proto type, a second payload stops being
    /// possible and the abstraction is a comment rather than a boundary.
    ///
    /// A source-level assertion because Rust cannot express it in the type
    /// system without splitting crates — which is the stronger fix, and what
    /// this test is a stand-in for.
    #[test]
    fn the_fleet_never_speaks_reapi() {
        // Everything generic: mesh + CAS + scheduling + the OCI facade.
        const FLEET: [&str; 5] = [
            "src/mesh.rs",
            "src/store.rs",
            "src/driver.rs",
            "src/worker.rs",
            "src/registry.rs",
        ];
        for file in FLEET {
            let src = std::fs::read_to_string(file).expect(file);
            // Tests may speak REAPI — they exercise the fleet THROUGH the reapi
            // payload. Production code may not.
            let prod = src.split("#[cfg(test)]").next().unwrap_or_default();
            for (i, line) in prod.lines().enumerate() {
                let l = line.trim_start();
                if l.starts_with("//") || l.starts_with("///") {
                    continue;
                }
                assert!(
                    !l.contains("bazel_remote_apis"),
                    "{file}:{}: the fleet must not import REAPI — put the decode \
                     behind a `Payload` hook instead:\n  {line}",
                    i + 1
                );
            }
        }
    }
}
