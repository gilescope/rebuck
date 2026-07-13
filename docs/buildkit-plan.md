# rebuck as a distributed BuildKit — plan

Reuse the fleet (mesh + CAS + lifecycle) to give earthbuild what Earthly
Satellites gave, but over free GitHub-Actions cache instead of a paid cloud.
Along the way, split the codebase along the seam that makes a third payload
cheap.

Companion to [re-engine-plan.md](re-engine-plan.md) (buck2/REAPI, shipped).

## North star

**Satellites via free GitHub caching.** Earthly Satellites were never
intra-build target sharding — `earthly --sat X +t` ran the *whole* build on
one remote buildkitd. What you paid for was the satellite's **persistent warm
disk**: layer cache *and* `type=cache` mounts surviving across builds. Decompose
it:

| what a Satellite gave | our free-GH equivalent | cost |
| -------------------------------- | ---------------------------------- | ---- |
| warm layer cache (persistent disk) | **P1** mesh OCI registry | ~3 wk, no fork |
| warm `type=cache` mounts (disk) | **P2** mesh-backed snapshotter | fork buildkit |
| a remote buildkitd | a buildkitd on a runner | none |

The warm cache — not distribution — is the product. rebuck supplies it from
the iroh mesh + the `actions/cache` shard machinery already built for buck2.

## What changed since v2

Two constraints the panel decided under are now lifted, and one version fact
landed:

- **We own both forks.** earthbuild is bumped on branch `giles-update-latest-buildkit`
  ([EarthBuild/earthbuild#442](https://github.com/EarthBuild/earthbuild/pull/442));
  the buildkit fork is [EarthBuild/buildkit](https://github.com/EarthBuild/buildkit).
  "No fork" is no longer a hard rule — the rule is now **minimise fork
  surface**, spend it only where it kills a load-bearing risk.
- **That deletes the shim.** v2's P2 impersonated buildkitd
  (`Control`+`LLBBridge`+`Session`, the reverse-gRPC hijack, the `pullping`
  relay) *because* we couldn't touch either side. The panel voted registry-only
  3-0 largely to escape that swamp. With fork freedom the swamp is irrelevant:
  we don't impersonate buildkitd, we run a real one and point earthbuild at it.
  **Both v2 fatal objections evaporate** — they were about the shim.
- **The buildkit bump is a containerd v1 -> v2 jump.** #442 pins buildkit
  v0.30.0 / containerd v2.2.5 / grpc 1.80 (base `main` is still v0.31.1 /
  containerd v1). The buildkit semver nominally dips; the real move is
  containerd **v2**, which is the API the P2 snapshotter targets.

## The finding that still holds

**An LLB `ExecOp` is not dispatchable to an arbitrary worker, and forking does
not change that.** Inputs are containerd snapshot chains (local overlayfs), not
Merkle trees; `MountType_SECRET`/`SSH` call back to the client mid-exec;
network is unrestricted. Forking buildkit does not make snapshots
content-addressed. So intra-build DAG sharding stays a rewrite of buildkitd's
executor — [moby/buildkit#62](https://github.com/moby/buildkit/issues/62)
("RFC: Distributed BuildKit", closed unimplemented), and Dagger's answer
(Project Theseus, 2024-25) was to *replace* the LLB solver, not distribute it.

This is why we mirror Satellites (one buildkitd per build) rather than beat
them: nobody sharded one LLB graph, and the reason is structural, not
lack-of-effort.

## Distribution model — one buildkitd per invocation

Verified in the earthbuild source: `cmd/earthly/subcmd/build_cmd.go:364`
constructs a single `bkClient`, passed as one `Builder.BkClient` field
(`:551`); there is no client collection anywhere in non-test code, and
`BuildkitAddress` is a single string. A satellite name just resolved to that
one address. **One build -> one buildkitd, always.**

So distribution comes from the CI matrix (N runners, each its own earthbuild +
buildkitd, sharded by target at the workflow level — the sweep workflows
already do this for buck2), and *sharing* comes from the mesh cache. This is
exactly the registry-only design the panel picked, and it needs no shim.

Intra-build target routing (one earthbuild fanning targets across N daemons) is
an **optional future** earthbuild change — thread a per-target client selector
through the single `BkClient` at `build_cmd.go:364`. Not needed to match
Satellites; scope only if a real workload wants it.

## Sequencing

| phase | what | fork | weeks |
| ----- | ---- | ---- | ----- |
| P0 | payload/fleet trait split | none | 1-2 |
| P1 | mesh OCI registry facade | none | ~3 |
| — | benchmark vs ghcr.io at **steady state** | — | — |
| P2 | mesh-backed cache mounts | buildkit | 4-8 |

P1 ships alone and is the whole layer-cache half of Satellites. P2 is what
turns "registry cache" into "Satellites"; gate it on P1 numbers.

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
src/payload/buildkit/ registry.rs snapshotter.rs
```

For the buildkit payload the `Frontend` is the OCI registry (P1) and the
`Executor::setup` launches the local buildkitd + the mesh registry; there is
no per-action `run` dispatch (earthbuild owns the build graph). bazel remains a
*config* of the reapi payload, not a third one.

## P1 — mesh OCI registry

An OCI distribution v2 facade over the existing CAS. earthbuild already speaks
`type=registry` (`builder/solver.go`: `CacheOptionsEntry{Type: "registry"}`).
No shim, no LLB parsing, no earthbuild changes. Use `mode=max` to export
intermediate vertex layers; the wire surface is identical to `mode=min`.

### Wire surface — six routes

Verified against containerd's `remotes/docker` (what buildkit delegates to):

| route | methods | notes |
| ----- | ------- | ----- |
| `/v2/<name>/manifests/<ref>` | HEAD, GET | Tag namespace. Serve OCI ImageManifest and Index. **Pass annotations through verbatim** — including `containerimage.inlinecache`, which the fork preserves and upstream dropped; strip it and `--use-inline-cache` breaks *silently*. |
| `/v2/<name>/manifests/<ref>` | PUT | HEAD existence check precedes every PUT. |
| `/v2/<name>/blobs/<digest>` | HEAD, GET | CAS lookup. **Accurate `Content-Length` is mandatory**: the importer enforces `maxBlobSize = 1<<20` on the config blob (`cache/remotecache/import.go:133-135`) and rejects on mismatch, silently. |
| `/v2/<name>/blobs/uploads/` | POST | Return `Location`. If `?mount=&from=` present return **202**, not 404 (`pusher.go:210-229`). |
| `<Location>` | PUT `?digest=` | Full body, verify digest, store. |

Not needed, each verified from source: no PATCH (chunked upload is `// TODO`,
`pusher.go:280`), no Range GET (client falls back to serial), no `/v2/` ping,
no auth, no TLS (`MatchLocalhost` forces plain HTTP for `127.0.0.1`), no
Referrers API (cache importer never calls it). Config blob mediaType is
`application/vnd.buildkit.cacheconfig.v0`; do not reject unknown mediaTypes.

**Scope: ~200-400 lines of Rust (axum).** `ferro-oci-server` v1.0.0 (35
downloads) and `buildkit-client`/`bkit` v0.1.4 (349 downloads) are one-person
projects — crib the route shapes, do not take the dep. ⚠︎ conf 0.8.

### Done when

Two runners, same Earthfile; runner B hits cache on layers runner A produced,
blobs demonstrably over iroh, store survives via `actions/cache`.

**Baseline is ghcr.io `type=registry`, not no-cache**, at steady state (>=3
consecutive runs, source-changing branch, stable lockfile). Be honest: punch
mean is 70.6 MB/s (16-254 spread) vs ghcr.io ~100-150; the raw-throughput edge
can invert under concurrent egress. The real differentiators are GH-cache
pre-seed overlapping the worker-join window, and no external auth dependency.
**The bar is one flag** (`--remote-cache=ghcr.io/org/repo:cache`). If P1 cannot
beat that at steady state, stop.

## P2 — mesh-backed cache mounts (the Satellites feature)

> Gate on a positive P1 delta. This is where the buildkit fork earns its keep.
> ⚠︎ conf 0.75.

Cache mounts are the half of Satellites a registry cache cannot carry. **The
hard constraint:** `GetRemotes()` exists only on `immutableRef`
(`cache/refs.go:68`); cache mounts are `MutableRef` with `NoCommit: true`
(`container.go:237`), released after exec, never reachable by the export
pipeline. `CacheMap` zeroes the mount ID before hashing
(`solver/llbsolver/ops/exec.go:127-130`) — exclusion is by design.
[moby/buildkit#1512](https://github.com/moby/buildkit/issues/1512) is open,
milestone `v0.future`, no PR. (#1474 is its closed predecessor — cite #1512.)

**Why it bites every commit, not just dep-update days:** `COPY src/`
invalidates the layer, so `RUN cargo build` misses the *layer* cache on every
source change; the cargo-registry CACHE mount is what saves 1-5 min on those
runs. And on ephemeral runners `/var/lib/buildkit` dies at teardown, so every
job starts cold. This is the gap P1 alone leaves.

Options, least fork surface first:

| option | mechanism | fork | verdict |
| ------ | --------- | ---- | ------- |
| (a) sticky affinity | `JobSpec::affinity` = target | none | intra-run only; required regardless, never sufficient alone |
| (b) copy-out/in | earthbuild **already ships** `CACHE --persist` (`earthfile2llb/converter.go:3453-3476`): a copy-out ExecOp writes mount contents to an `ImmutableRef`, which *is* `GetRemotes()`-traversable and mesh-shippable | none | try first; ~8 min on big caches, deduped by chainID |
| (c) proxy snapshotter | buildkitd's `proxySnapshotterPath` seam (`cmd/buildkitd/main_oci_worker.go`) routes Prepare/Commit/Mounts to an external gRPC socket. `Commit()` on a dirty mount is the push-to-mesh hook; `Prepare()` the lazy-pull. **Plugin seam, not a core patch** — survives rebases. | plugin only | right long-term answer; ~500 LoC Go over containerd **v2** snapshots proto (no Rust crate). Clipper.dev runs this in prod |
| (d) native export | patch the fork to commit+export cache mounts (implement #1512 for our backend) | core patch | last resort; a permanent carry against upstream |

Decision order (a) always, then (b) [no fork, exists], then (c) [plugin seam],
(d) only if (c) is too slow. The prime risk becomes the headline feature.

Note: (c) needs a small Go sidecar (the snapshots gRPC service). That is the
*only* Go in the design — the fleet, CAS, mesh, dispatch and P1 registry stay
Rust. Unlike v2's shim sidecar, it speaks a stable containerd proto, not the
churn-prone earthbuild session surface.

## Version pinning

Worker buildkitd, the P2 snapshotter proto, and any shim protos must track
**one EarthBuild/buildkit rev** (go.mod `replace` on the earthbuild branch
pins it). Protos live in `EarthBuild/buildkit/frontend/gateway/pb/` and
`EarthBuild/buildkit/session/{filesync,auth,secrets,pullping,localhost,socketforward}`.
CI-diff the protos on every fork bump. Gateway + session protos were additive
across v0.29-v0.31, so the risk is the fork's own additions, not upstream drift.

## Risks

| risk | severity | mitigation |
| ---- | -------- | ---------- |
| cache-mount transfer complexity | **high** — the core value; (c) is novel Go over containerd v2 | (b) first (already exists, no fork); (c) only if (b) too slow |
| P2P slower than ghcr.io at steady state | medium | P1 benchmark answers this *before* P2 |
| economics invert (data >> compute) | medium — `apt-get` layers are data-bound | `cargo`/`npm` targets are compute-bound; benchmark partitions cases |
| fork maintenance drag | medium — we now carry earthbuild + buildkit forks | prefer plugin seam (c) over core patch (d); pin one rev; CI-diff protos |
| GH cache budget contention | medium | the 10 GB quota is **per-repo across all caches** — OCI layer shards compete with buck2 shards |

## Non-goals

- **The buildkitd shim** (v2's P2). Deleted. We run a real buildkitd and point
  earthbuild at it; no `Control`/`LLBBridge`/`Session` impersonation, no
  `pullping` relay, no reverse-gRPC hijack. The two v2 fatal objections were
  all about the shim.
- **Intra-build DAG sharding below target granularity.** Snapshot chains make
  it a buildkitd-executor rewrite; forking does not help. Nobody has done it.
- **Intra-build multi-host target routing.** Possible (an earthbuild change at
  `build_cmd.go:364`) but unnecessary to match Satellites; defer until a
  workload demands it.
- **Translating LLB -> REAPI actions** (reuse `exec.rs` wholesale). Three
  independently fatal causes: dropping cache mounts makes every Rust/Go/Node
  build slower than no rebuck; output paths are unknowable (`RUN cargo build`
  emits unknown files into `target/`); non-hermetic REAPI caching is wrong for
  `apt-get` layers. ~18-33 wk to a worse result.
- **Windows/macOS buildkit workers.** Windows buildkitd is WCOW-only and shares
  no pool with the MSVC-Rust-via-buck2 sweep; buildkitd does not run on macOS.
- Chunked PATCH, Referrers, Range GET in the P1 registry — unused by the cache
  path.
