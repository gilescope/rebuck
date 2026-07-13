# rebuck as a distributed BuildKit — plan

Extend the fleet (mesh + CAS + lifecycle) to a second payload: earthbuild
talking to rebuck, rebuck driving a fleet of stock buildkitds. Along the way,
split the codebase along the seam that makes a third payload (bazel, nix,
whatever) cheap.

Companion to [re-engine-plan.md](re-engine-plan.md) (the buck2/REAPI engine,
shipped). That document's fleet is this document's substrate.

## The finding that shapes everything

**BuildKit has no dispatchable unit of work.** Bazel RE gives us
`Action = {command, input_root_digest, platform}` — hermetic, content-addressed,
self-describing; 32 bytes on the wire and any worker can run it. That property
is the reason `driver.rs` exists at all.

An LLB `ExecOp` has none of it:

- inputs are **containerd snapshot chains** (local overlayfs entries), not
  Merkle trees;
- `--mount=type=cache` is mutable node-local state, *deliberately excluded from
  cache keys*;
- `MountType_SECRET` / `MountType_SSH` call back to the client's session
  mid-exec;
- network is unrestricted, so `RUN apt-get update` is expected to be
  non-deterministic.

`worker.Worker` is a Go interface resolved in-process. There is no wire protocol
for buildkitd A to hand a vertex to buildkitd B, and there never has been:
[moby/buildkit#62](https://github.com/moby/buildkit/issues/62) ("RFC: Distributed
BuildKit", 2017) was closed unimplemented, and the solver still carries the
comment *"the default worker for the temporary default non-distributed
use-case"*.

Nor does anyone do it. Depot, Namespace, Blacksmith, Earthly Satellites and
`buildx` multi-node are all **one build → one buildkitd**, merely remote,
persistent and on fast disk. `buildx` shards by *platform*, not by graph. That
unanimity is evidence, and this plan's job is to find out whether it's a gap or
a verdict (see [Risks](#risks)).

### So what *is* dispatchable

Two facts rescue it:

1. **Earthly holds the scheduler, not buildkitd.** It uses the `gateway.v0`
   callback and issues `LLBBridge.Solve` per subgraph from its own Go code.
2. **An LLB `Definition` is a self-contained DAG rooted at `SourceOp`s**,
   content-addressed by op digest. A definition therefore *is* dispatchable to
   any worker — the worker just re-solves it.

Re-solving is only cheap if the workers share a cache. **The shared cache is the
entire product; the scheduler is a footnote.** Which is convenient, because a
fleet-wide content-addressed store with P2P discovery and GH-cache persistence is
the thing we already have.

### And our CAS is already an OCI registry

`cas/<hh>/<sha256hex>` + sha256 digests + a tag namespace is *exactly* what OCI
distribution v2 serves. BuildKit's only supported cross-instance state transfer
is `--cache-to/--cache-from type=registry`: OCI artifacts, sha256-addressed
blobs, a `cache.v0` manifest encoding the cache-key DAG, imported via
`worker.FromRemote()` → `CacheMgr.GetByBlob()`.

Point a stock buildkitd at a *localhost* registry that is really a rebuck agent
fronting the mesh, and BuildKit thinks it's talking to a boring registry while
its layers travel P2P over iroh. **No BuildKit fork.**

```text
earthbuild ──Control+LLBBridge+Session──▶ rebuck driver (P2 shim)
                                            │  iroh mesh (unchanged)
                    ┌───────────────────────┼───────────────────────┐
              rebuck agent            rebuck agent            rebuck agent
              :5000 OCI  ◀── HTTP ──  :5000 OCI              :5000 OCI
                  │                       │                       │
              buildkitd               buildkitd               buildkitd
              (stock; --cache-to/from type=registry,ref=127.0.0.1:5000/c,mode=max)
```

## P0 — payload/fleet split

The fleet doesn't care what a job *is*. `Driver`'s ~40 fields — queue, blooms,
providers, peer_conns, affinity_owner, memo_servable, finalized_shards — are all
mechanics. Only two things reach into `re::`:

| leak | today | fix |
| ------------------------- | -------------------------------- | -------------------------- |
| `Driver::execute` | takes `action_digest: &Dig` | takes an opaque spec |
| `Driver::validated_ac_get` | decodes `re::ActionResult` | payload hook for digests |
| `D2W::Run { action: Dig }` | assumes the spec IS a CAS blob | `Run { spec: Vec<u8> }` |
| `PlatKey::from` | reads `re::Platform` properties | payload hook |

Everything else is already payload-blind. The split is smaller than it looks.

### The traits

```rust
/// Scheduling metadata the fleet needs, extracted from an opaque job spec.
/// The fleet never decodes the spec itself.
pub trait JobSpec: Send + Sync {
    /// Which workers may run this (os/arch buckets).
    fn platform(&self) -> PlatKey;

    /// Pin every job sharing this key to ONE worker, in ONE directory.
    /// buck2: the `__<crate>__/` output prefix — rustc tracks env!-read
    /// values into the SVH, so pipelined twins in different dirs hash
    /// differently and every downstream link dies with E0460.
    /// buildkit: the Earthly target — `type=cache` mounts are node-local
    /// mutable state, so a target that migrates loses its warm cargo/npm
    /// registry. Same mechanism, same reason: some state won't travel.
    fn affinity(&self) -> Option<String>;

    /// Heaviest inputs, for delay-scheduled locality (move the task to the
    /// data). Blooms only lie in the safe direction.
    fn locality_hints(&self) -> Vec<Dig>;

    fn encode(&self) -> Vec<u8>;
}

/// Driver-side payload: the protocol the build tool dials.
#[async_trait]
pub trait Frontend: Send + Sync + 'static {
    async fn serve(self: Arc<Self>, fleet: Arc<Fleet>, addr: SocketAddr) -> Result<()>;

    /// Blobs a cached result references. The fleet refuses to serve a cache
    /// hit whose blobs it cannot produce — the validated-AC invariant is
    /// generic and load-bearing (17k blob-less hits → 34k client extract
    /// failures, see optimizations.md); only the *decoding* is payload-specific.
    fn result_digests(&self, result: &[u8]) -> Vec<Dig>;
}

/// Worker-side payload: run one opaque spec against the mesh CAS.
#[async_trait]
pub trait Executor: Send + Sync + 'static {
    /// Stand up any per-worker sidecar before serving (buildkit: the
    /// localhost OCI registry + buildkitd; buck2: nothing).
    async fn setup(&self, blobs: Arc<dyn Blobs>) -> Result<()> { Ok(()) }

    async fn run(&self, blobs: &dyn Blobs, spec: &[u8], scratch: &Path)
        -> Result<Vec<u8>>;
}
```

`Fleet` is today's `Driver` minus the REAPI: mesh, store, queue, dispatch
(least-inflight + affinity + locality + tail speculation), blooms, providers,
requeue, shard finalize. It exposes `submit(spec) -> Result<Vec<u8>>`,
`blobs()`, `store()`, `worker_count()`.

### Module moves

```text
src/fleet/   mesh.rs store.rs driver.rs(-REAPI) worker.rs(-exec)  # generic
src/payload/reapi/    rpc.rs exec.rs            # buck2 today, bazel ~free
src/payload/buildkit/ registry.rs shim.rs bkexec.rs
```

**Do not over-abstract for a phantom third payload.** Bazel speaks the same
REAPI — it is a *config* of the reapi payload, not a third one (caveats: it wants
`GetTree`, compressed-blobs, real `WaitExecution`, all currently `UNIMPLEMENTED`).
The real axis is two families: **content-addressed actions** (buck2, bazel) vs
**layer graphs** (buildkit). Two is enough to find the seam; three is enough to
draw it wrong.

## P1 — mesh registry (ship this alone)

An OCI distribution v2 facade over the existing CAS. Stock buildkitds,
`--cache-to/--cache-from type=registry`. No shim, no LLB parsing, no earthbuild
changes.

- `GET/HEAD /v2/<name>/blobs/sha256:<hex>` → `Fleet::get_blob` (mesh fetch,
  bloom → provider index → driver store, exactly as today).
- `POST/PATCH/PUT /v2/<name>/blobs/uploads/` → `Store::put` (+ provider index in
  decentralized mode).
- `PUT/GET /v2/<name>/manifests/<ref>` → the tag namespace (today's `ac/`,
  generalised to a kv namespace).
- Cache blobs ride the existing shard/finalize machinery to `actions/cache`.

Delivers a fleet-wide P2P shared BuildKit cache with GH-cache persistence — which
is precisely what Depot and Namespace sell, minus the persistent box. It also
de-risks P2 by proving layer transfer over the mesh *before* any shim exists.

**Done when:** two runners, stock buildkitd each, same Earthfile; runner B's
build is a cache hit on layers runner A produced, blobs demonstrably over iroh
(not a registry), and the store survives a run via `actions/cache`.

## P2 — the buildkitd shim

rebuck driver serves what earthbuild dials, and routes each `LLBBridge.Solve` to
a worker.

Required surface (all of it genuinely needed):

| service | methods |
| ---------- | -------------------------------------------------------------- |
| `Control` | `Info`, `ListWorkers`, `Session`, `Solve`, `Status` |
| `LLBBridge` | `ResolveImageConfig`, `ResolveSourceMeta`, `Inputs`, `Solve`, `ReadFile`, `ReadDir`, `StatFile`, `Evaluate`, `Return`, `Warn`, `NewContainer`, `ReleaseContainer`, `ExecProcess` |
| session (client-side, we call *back*) | `FileSync.DiffCopy`, `Auth.Credentials`, `Secrets.GetSecret`, `SSHForward` |

`Control.Session` is the awkward one: the client runs an h2c gRPC **server** over
the bidi stream and buildkitd calls back into it. The shim must be both ends.
Fan-out fix: slurp the local context into the CAS once, then have the shim
*serve* `FileSync` toward the workers — context uploads once, not once per
worker.

Scheduling: sticky affinity by Earthly target (`JobSpec::affinity`), so
`type=cache` mounts stay warm on their worker.

**Done when:** `earthbuild +all` against the shim spreads independent targets
across ≥2 workers and beats a single fat buildkitd on the same total hardware.

## Risks

- **`type=cache` does not migrate.**
  [moby/buildkit#1474](https://github.com/moby/buildkit/issues/1474), open since
  2019. Earthly's `CACHE` directive, cargo/npm/go registries — node-local mutable
  state with no export path. Sticky affinity is a mitigation, not a fix. **The
  prime suspect for *why* nobody shards an LLB graph, and the thing most likely
  to sink P2.**
- **The economics invert.** A Bazel action is compile-one-file (compute ≫ data);
  a Docker layer is `apt-get install` (data ≫ compute). Distribution pays only
  when compute dominates. Earthly targets running `cargo build` / `npm test` do;
  `FROM ubuntu; RUN apt-get` does not. P1's numbers decide this before P2 costs
  anything.
- Snapshot chains still can't be handed worker-to-worker. We route *around* this
  (self-contained LLB defs + shared cache), not through it. A cold worker
  re-solves; the cache is what makes that free.
- Confidence: P1 high. **P2 ~0.75** — worth building only if P1 says
  cross-target parallelism beats one fat satellite.

## Non-goals

- Forking BuildKit. If a change needs a fork, it's out of scope by definition —
  the fork is what everyone else already did instead of solving this.
- Sharding a single LLB DAG below target granularity. The snapshot chain makes
  vertex-level dispatch a rewrite of buildkitd's executor with worse fidelity.
- Translating LLB → REAPI actions. Merkle-izing a full rootfs per exec, diffing
  `/` for outputs, and dropping cache mounts/secrets/network on the floor: strictly
  worse than the thing it replaces.
