# rebuck as a distributed BuildKit — plan

Extend the fleet (mesh + CAS + lifecycle) to a second payload: earthbuild
talking to rebuck, rebuck driving a fleet of buildkitd workers. Along the way,
split the codebase along the seam that makes a third payload cheap.

Companion to [re-engine-plan.md](re-engine-plan.md) (buck2/REAPI, shipped).

## Verdict

A 3-judge design panel scored three rival architectures. **Registry-only won
3-0** against the shim. The roadmap reflects that: build P0 + P1, benchmark
against a plain ghcr.io remote cache at steady state, and treat P2 as a data
decision — not a foregone one.

Adversarial review raised 21 objections, **2 rated fatal**, both against P2
(see [P2](#p2--buildkitd-shim)). Neither is fatal to P1.

**Why now.** Earthly Technologies shut down Earthly Cloud + Satellites on
**2025-07-16** and ended active maintenance of the OSS project, endorsing a
community fork ([EarthBuild/earthbuild][eb], v0.8.17, active). Their own
migration advice to Satellite users was *"roll out your own remote
BuildKit"* ([blog][shut]). That is the demand this plan serves.

[eb]: https://github.com/EarthBuild/earthbuild
[shut]: https://earthly.dev/blog/shutting-down-earthfiles-cloud/

## The finding that shapes everything

**BuildKit has no dispatchable unit of work.** Bazel RE gives us
`Action = {command, input_root_digest, platform}` — hermetic,
content-addressed, self-describing; 32 bytes on the wire, any worker can run
it. An LLB `ExecOp` has none of it:

- inputs are containerd snapshot chains (local overlayfs), not Merkle trees
- `--mount=type=cache` is mutable node-local state, deliberately excluded from
  cache keys — `CacheMap` *zeroes the mount ID before hashing*
  (`solver/llbsolver/ops/exec.go:127-130`); by design, not oversight
- `MountType_SECRET`/`MountType_SSH` call back to the client session mid-exec
- network is unrestricted; `RUN apt-get update` is expected non-deterministic

`worker.Worker` is a Go interface resolved in-process. No wire protocol exists
for buildkitd A to hand a vertex to buildkitd B:
[moby/buildkit#62](https://github.com/moby/buildkit/issues/62) ("RFC:
Distributed BuildKit", 2017) was closed unimplemented.

### Gap or verdict? Verdict

- **Dagger did not distribute LLB — they escaped it.** Project Theseus
  (2024-25) ripped out BuildKit's solver and replaced it with an e-graph
  caching engine. Their answer to LLB's non-distributability was to leave.
- **Depot/Namespace/Blacksmith** run *multiple builds* in parallel per
  instance (BuildKit's native pipeline parallelism), never *one build* across
  instances. This is the most common confusion in the landscape.
- **`buildx` multi-node** shards by platform only, never one platform's DAG.
- **Earthly Satellites already did target-level dispatch** across buildkitd
  instances — the exact granularity P2 proposes. P2 is therefore *not novel*;
  it is Satellites, P2P-native. Known-working pattern, but nobody went below
  target granularity, and that boundary is where the evidence stops.

### What is dispatchable

1. **Earthly holds the scheduler, not buildkitd.** It uses `gateway.v0` and
   issues `LLBBridge.Solve` per subgraph from its own Go code.
2. **An LLB `Definition` is a self-contained DAG** rooted at `SourceOp`s,
   content-addressed by op digest — dispatchable to any worker that re-solves
   it.

Re-solving is only cheap if workers share a cache. **The shared cache is the
product; the scheduler is a footnote.** We already have a fleet-wide
content-addressed store with P2P discovery and GH-cache persistence.

### Our CAS is already an OCI registry

`cas/<hh>/<sha256hex>` + sha256 digests + a tag namespace is exactly what OCI
distribution v2 serves. BuildKit's only cross-instance state transfer is
`--cache-to/--cache-from type=registry`. Point a buildkitd at a localhost
registry fronting the mesh and it thinks it is talking to a boring registry
while its layers travel P2P over iroh.

## Correction to the previous draft

The first draft said workers run **stock** `moby/buildkit`. **That is wrong.**
earthbuild sets `ExporterEarthly = "earthly"` on every Solve
(`buildkit/client/exporters.go:9`); stock buildkitd answers *"exporter earthly
could not be found"*. Workers must run **EarthBuild/buildkit2** (last upstream
merge 2026-04-08, near-current; the stale `earthly/buildkit` repo is a
different thing).

"No BuildKit fork" is therefore scoped to **rebuck itself**. The workers run
the community fork. Consequently the P2 protocol surface must be generated
from **buildkit2's** protos, not upstream's — the original method table was
derived from upstream and was missing three RPCs that are each independently
fatal.

## Sequencing

| phase | what | weeks | gate |
| ----- | ---- | ----- | ---- |
| P0 | payload/fleet trait split | 1-2 | always |
| P1 | mesh OCI registry facade | ~3 | always |
| — | benchmark vs ghcr.io at **steady state** | — | P2 go/no-go |
| P2 | LLBBridge shim + session relay | 10-14 | positive P1 delta only |

## P0 — payload/fleet split

The fleet does not care what a job is. Only four things reach into `re::`:

| leak | today | fix |
| ---- | ----- | --- |
| `Driver::execute` | takes `action_digest: &Dig` | takes an opaque spec |
| `Driver::validated_ac_get` | decodes `re::ActionResult` | payload hook for digests |
| `D2W::Run { action: Dig }` | assumes the spec IS a CAS blob | `Run { spec: Vec<u8> }` |
| `PlatKey::from` | reads `re::Platform` properties | payload hook |

Everything else — queue, blooms, providers, `peer_conns`, `affinity_owner`,
`memo_servable`, `finalized_shards` — is already payload-blind.

### Traits

```rust
/// Scheduling metadata extracted from an opaque job spec.
/// The fleet never decodes the spec itself.
pub trait JobSpec: Send + Sync {
    fn platform(&self) -> PlatKey;

    /// Pin all jobs sharing this key to ONE worker, ONE directory.
    /// buck2: the `__<crate>__/` output prefix — rustc folds env!-read
    ///   values into the SVH, so pipelined twins in different dirs are
    ///   link-incompatible (E0460).
    /// buildkit: the Earthly target — type=cache mounts are node-local,
    ///   so a migrated target loses its warm cargo/npm registry.
    /// Same mechanism, same reason: some state will not travel.
    fn affinity(&self) -> Option<String>;

    /// Heaviest inputs, for delay-scheduled locality. Blooms lie safely.
    fn locality_hints(&self) -> Vec<Dig>;

    fn encode(&self) -> Vec<u8>;
}

/// Driver-side: the protocol the build tool dials.
#[async_trait]
pub trait Frontend: Send + Sync + 'static {
    async fn serve(self: Arc<Self>, fleet: Arc<Fleet>, addr: SocketAddr)
        -> Result<()>;

    /// Blobs a cached result references. The fleet refuses a cache hit
    /// whose blobs it cannot produce — the validated-AC invariant is
    /// generic and load-bearing (17k blob-less hits -> 34k client extract
    /// failures; optimizations.md). Only the decoding is payload-specific.
    fn result_digests(&self, result: &[u8]) -> Vec<Dig>;
}

/// Worker-side: run one opaque spec against the mesh CAS.
#[async_trait]
pub trait Executor: Send + Sync + 'static {
    /// Stand up per-worker sidecars before serving.
    /// buildkit: the localhost mesh registry + buildkitd. buck2: nothing.
    async fn setup(&self, blobs: Arc<dyn Blobs>) -> Result<()> { Ok(()) }

    async fn run(&self, blobs: &dyn Blobs, spec: &[u8], scratch: &Path)
        -> Result<Vec<u8>>;
}
```

`Fleet` is today's `Driver` minus REAPI. Exposes `submit(spec)`, `blobs()`,
`store()`, `worker_count()`.

```text
src/fleet/            mesh.rs store.rs driver.rs(-REAPI) worker.rs(-exec)
src/payload/reapi/    rpc.rs exec.rs        # buck2 today; bazel is a config
src/payload/buildkit/ registry.rs shim.rs bkexec.rs
```

Not over-abstracting: bazel speaks REAPI — a *config* of the reapi payload,
not a third one (it does want `GetTree`, compressed-blobs and real
`WaitExecution`, all `UNIMPLEMENTED` today). Two families is enough to find
the seam; three is enough to draw it wrong.

## P1 — mesh OCI registry

An OCI distribution v2 facade over the existing CAS. earthbuild already speaks
`type=registry` (`builder/solver.go`: `CacheOptionsEntry{Type: "registry"}`).
No shim, no LLB parsing, no earthbuild changes. Use `mode=max`
(`--max-remote-cache`) to export intermediate vertex layers; the wire surface
is identical to `mode=min`, only the blob count differs.

### Wire surface — six routes, not sixty

Verified against containerd's `remotes/docker` (what BuildKit delegates to):

| route | methods | notes |
| ----- | ------- | ----- |
| `/v2/<name>/manifests/<ref>` | HEAD, GET | Tag namespace. Serve OCI ImageManifest and Index. **Pass manifest annotations through verbatim** — including `containerimage.inlinecache`, which buildkit2 preserves and upstream dropped; strip it and `--use-inline-cache` breaks *silently*. |
| `/v2/<name>/manifests/<ref>` | PUT | HEAD existence check precedes every PUT. |
| `/v2/<name>/blobs/<digest>` | HEAD, GET | CAS lookup. **Accurate `Content-Length` is mandatory**: the importer enforces `maxBlobSize = 1<<20` on the config blob (`cache/remotecache/import.go:133-135`) and rejects on mismatch, silently. |
| `/v2/<name>/blobs/uploads/` | POST | Return `Location`. If `?mount=&from=` present return **202**, not 404 — a 404 errors the client rather than falling back (`pusher.go:210-229`). |
| `<Location>` | PUT `?digest=` | Full body, verify digest, store. |

Not needed, each verified from source:

- **No PATCH.** Chunked upload is `// TODO` at `pusher.go:280`, unreachable.
  Uploads are strictly POST-then-PUT.
- **No Range GET.** Client falls back to serial fetch on
  `errContentRangeIgnored`.
- **No `/v2/` ping** (auth is reactive-401), **no auth**, **no TLS**:
  containerd's `MatchLocalhost` forces plain HTTP for `127.0.0.1`/`localhost`
  (`util/resolver/resolver.go:38-41`). No `buildkitd.toml` stanza at all.
- **No Referrers API.** `FetchReferrers` exists on `dockerFetcher` but the
  cache importer never calls it (zero references).

Config blob mediaType is `application/vnd.buildkit.cacheconfig.v0`. Layer
blobs are opaque; compression lives in the manifest, not `Content-Encoding`.

**Scope: ~200-400 lines of Rust (axum).** Reference implementations exist but
are *not* dependencies: `ferro-oci-server` v1.0.0 (35 downloads) and
`buildkit-client`/`bkit` v0.1.4 (349 downloads) — one-person projects. Crib
the route shapes; do not take the dep. ⚠︎ conf 0.8 on their fitness.

### Done when

Two runners, same Earthfile; runner B hits cache on layers runner A produced,
blobs demonstrably over iroh, store survives via `actions/cache`.

**Baseline is ghcr.io `type=registry`, not no-cache**, at steady state (>=3
consecutive runs, source-changing branch, stable lockfile). Be honest about
what we are selling: punch-soak mean is 70.6 MB/s (spread 16-254) vs ghcr.io
~100-150 MB/s — **the raw throughput advantage can invert under concurrent
egress**. The real differentiators are GH-cache pre-seed overlapping the
worker-join window, and no external auth dependency.

## P2 — buildkitd shim

> Gate: build only on a positive P1 delta. Panel voted against it 3-0.
> ⚠︎ conf 0.75.

### The two fatal findings

1. **The method table was wrong in three places, each fatal.** Drafted from
   upstream protos, missing: `Ping` (session cannot start —
   `grpcclient/client.go:53`), `Return` (unconditional deferred call at
   `:206`; its absence kills *every* build at teardown), and `Export`
   (fork-only; every WAIT/END block and SAVE IMAGE —
   `wait_block.go:280,404`). Regenerate from buildkit2's protos.
2. **The session relay table omitted `pullping.PullPing`.** It is called
   *synchronously inside* buildkit2's exporter
   (`exporter/earthlyoutputs/export.go:598`) and blocks before export returns.
   Without relay, **every SAVE IMAGE silently produces no output.** It is a
   *worker -> shim -> client* callback, the opposite direction to FileSync, so
   the "slurp context into CAS once" trick does not cover it — N workers means
   N relay channels multiplexed through one client session. Also missing:
   `localhost.Localhost` (all LOCALLY targets fail) and `socketforward.Socket`.

Also correcting: `NewContainer`/`ExecProcess`/`ReleaseContainer` were listed as
required. earthbuild never calls them from the build path (only from a logging
wrapper) — stub them `UNIMPLEMENTED`.

### Session transport — use a Go sidecar

`Control.Session` carries raw h2 frames in `BytesMessage.Data`;
`grpchijack` wraps the stream as a `net.Conn`. Rust's tonic wants
`AsyncRead + AsyncWrite`, not `Streaming<BytesMessage>`.

Prior art exists — `arcboxlabs/buildkit-client` does h2c-over-gRPC-stream in
Rust with `h2` 0.4 directly, including DiffCopy/Auth/Secrets — but it is a
0.1.x with 349 downloads. **Recommendation: a Go sidecar** wrapping buildkit2's
own `grpchijack` (~400 lines of established Go), exposing a thin internal gRPC
API to the Rust fleet. Hand-rolling it in Rust is ~2-3 weeks of novel async
with no in-repo precedent, and it must track a fork that is actively extending
its proto surface. The fleet, CAS, mesh and dispatch stay Rust; the sidecar
owns only the churn-prone protocol edge.

Also non-trivial: `DiffCopy` is ~500 lines of bespoke protocol (depth-first
sorted STAT listings, Go FileMode translation, REQ/DATA/FIN flow control), not
proto wrapping. The previous draft treated context fan-in as a footnote.

## Cache mounts — the structural risk

**The hard constraint:** `GetRemotes()` exists only on `immutableRef`
(`cache/refs.go:68`). Cache mounts are `MutableRef` with `NoCommit: true`
(`container.go:237`), released after exec — never committed, never reachable
by the registry export pipeline. No `ExportCacheMount` exists anywhere.

[moby/buildkit#1512](https://github.com/moby/buildkit/issues/1512) is the live
tracker: **open**, milestone `v0.future`, 62 comments, no PR. (The previous
draft cited #1474 as "open since 2019". It is **closed**, superseded by
\#1512 — cite that instead.)

**Why it bites in CI, not just on dep-update days:** `COPY src/` invalidates
the layer, so `RUN cargo build` misses the *layer* cache on every source
change. The cargo-registry CACHE mount is what saves 1-5 min on those runs.
Layer cache helps re-runs; cache mounts help every commit. And on ephemeral
runners `/var/lib/buildkit` dies at job teardown — **sticky affinity fixes
intra-run duplication but recovers nothing across runs.**

| option | mechanism | status |
| ------ | --------- | ------ |
| (a) sticky affinity | `JobSpec::affinity` = Earthly target | in P2 already; intra-run only; required regardless |
| (b) copy-out/copy-in | earthbuild **already has this**: `CACHE --persist` (`earthfile2llb/converter.go:3453-3476`) emits a copy-out ExecOp into an `ImmutableRef`, which *is* `GetRemotes()`-traversable and shippable over the mesh. No fork. | pursue first if (a) is insufficient; ~8 min overhead on large caches, deduped by chainID |
| (c) proxy snapshotter | buildkitd has a first-class `proxySnapshotterPath` seam (`cmd/buildkitd/main_oci_worker.go:359-387`) routing Prepare/Commit/Mounts to an external gRPC socket. `Commit()` is the push-to-CAS hook. **No fork.** | right long-term answer; ~500 LoC Go (no Rust crate for the containerd snapshots proto). Clipper.dev runs this in production (claims 4.2x) |

That (c) needs no fork is the single most useful thing the research turned up.
Scope it after P2 data, not before.

## Risks

| risk | severity | mitigation |
| ---- | -------- | ---------- |
| `type=cache` cross-run cold start | **high** — hits every source-change run on ephemeral runners; could invert P2 vs one persistent buildkitd | (a) always; (b)/(c) for cross-run; gate on benchmark |
| `pullping` reverse relay fan-out | **high** — no in-repo precedent; N channels through one session | ~2.5 wk explicit scope; Go sidecar owns it |
| P2P slower than ghcr.io at steady state | medium — 70.6 MB/s mean vs ~100-150 | P1 benchmark answers this *before* P2 starts |
| Economics invert (data >> compute) | medium — `apt-get` layers are data-bound | `cargo`/`npm` targets are compute-bound; benchmark partitions the cases |
| buildkit2 proto churn | low-med — fork actively extends proto surface | pin worker images + shim protos to one buildkit2 SHA; CI-diff protos on bump |
| **GH cache budget contention** | medium | the 10 GB quota is **per-repo across all caches** — OCI layer shards compete directly with the existing buck2 shards. Two payloads, one budget. |

**The bar we must clear is embarrassingly low.** Earthly's remote cache is one
flag: `--remote-cache=ghcr.io/org/repo:cache`, plus three lines of workflow.
If P1 cannot beat that at steady state, the honest move is to stop.

## Non-goals

- **Forking BuildKit *in rebuck*.** Workers run EarthBuild/buildkit2.
- **Sharding one LLB DAG below target granularity.** Snapshot chains make
  vertex-level dispatch a rewrite of buildkitd's executor with worse fidelity.
  Nobody has done it; Dagger left LLB rather than try.
- **Translating LLB -> REAPI actions** (the "reuse `exec.rs` wholesale" idea).
  Three independently fatal causes: dropping cache mounts makes every
  Rust/Go/Node build *slower than no rebuck at all*; output paths are unknowable
  (`RUN cargo build` emits unknown files into `target/`), so it needs a
  reimplementation of `runcexecutor.go`; and non-hermetic REAPI caching is
  semantically wrong for `apt-get` layers. Judged 18-33 weeks to reach a worse
  result.
- **Windows buildkit workers.** Windows buildkitd is WCOW-only/experimental and
  has zero overlap with the MSVC-Rust-via-buck2 sweep; containerd is not even
  preinstalled on `windows-latest`. The two payloads do not share a worker pool.
- Chunked PATCH upload, Referrers API, Range GET in the P1 registry — none are
  used by the BuildKit cache path.
