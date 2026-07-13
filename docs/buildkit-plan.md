# rebuck as a distributed BuildKit — plan

Give a fleet of ephemeral earthbuild/buildkitd runners the two things a single
persistent buildkitd gives for free — a shared warm layer cache, and
**build-once** (never compile the same thing on two machines at once) — over
free GitHub-Actions cache instead of a paid cloud. Along the way, split the
codebase along the seam that makes a third payload cheap.

Companion to [re-engine-plan.md](re-engine-plan.md) (buck2/REAPI, shipped).

## North star

**Recreate, across N ephemeral daemons, the single-flight that one persistent
daemon gives for free.** Two wants, one root cause:

| want | when it bites | mechanism |
| ------------------------------------ | ----------------------- | --------- |
| built layers reused elsewhere | build B *after* A | P1 mesh registry (after-the-fact) |
| **don't build the same thing twice, concurrently** | A and B *at once* | P2 distributed single-flight (in-flight) |

The root cause of both: **a single buildkitd already single-flights** — its
solver edge-merges concurrent identical vertices (`Solver.actives` on the
vertex digest, `jobs.go:44`; edge merge on the fast key, `scheduler.go:309`;
`flightcontrol.Group` in `sharedOp`, `jobs.go:956`). Many builds hitting *one*
daemon get build-once for free; that is *why Earthly Satellites felt good*. The
moment you have N independent daemons (a CI matrix), each has its own solver and
the property is lost. rebuck's job is to restore it across the mesh.

We cannot just keep one big daemon: no persistent host is a stated non-goal
(everything ephemeral on GH runners). The lease is the price of ephemerality.

## The gap is real (confirmed)

The container-build world has **no** cross-machine build dedup:

- BuildKit vertex digests are "not between different invocations" — explicitly
  single-daemon ([solver design](https://crazymax.dev/buildkit/design/solver/)).
- Depot, Dagger, Nix/nixbuild.net all do *after-the-fact* cache reuse; two
  concurrent builds of the same thing both run. Depot even scales *out* under
  load (cache clones), destroying dedup.
- Bazel RE solved it in 2022: BuildBuddy "action merging" and NativeLink
  `AwaitedActionDb` — lease keyed on the action digest, heartbeat TTL, waiters
  attach via `WaitExecution`, hedged execution on stall, crashed holder → TTL
  expiry. That is the proven design; we port it to the mesh.

## The finding that still holds

An LLB `ExecOp` is not dispatchable to an arbitrary worker, and forking does
not change it (snapshot chains are local overlayfs, not Merkle trees). So we do
**not** shard one build across daemons —
[moby/buildkit#62](https://github.com/moby/buildkit/issues/62) closed
unimplemented; Dagger *replaced* the LLB solver rather than distribute it. We
mirror Satellites (one buildkitd per build) and add cross-daemon coordination,
not cross-daemon execution.

## What changed since v2

- **We own both forks.** earthbuild #442 (`giles-update-latest-buildkit`);
  buildkit fork [EarthBuild/buildkit](https://github.com/EarthBuild/buildkit).
  The rule is now **minimise fork surface**, not "no fork".
- **The shim is deleted.** v2's P2 impersonated buildkitd (reverse-gRPC hijack,
  `pullping` relay) only because we couldn't touch either side; the panel voted
  registry-only 3-0 to escape it. We run a real buildkitd and point earthbuild
  at it. Both v2 fatal objections were about the shim.
- **containerd v1 -> v2.** #442 pins buildkit v0.30.0 / containerd v2.2.5 /
  grpc 1.80 (the semver dips; the real move is containerd v2).

## Distribution model — one buildkitd per invocation

Verified: `cmd/earthly/subcmd/build_cmd.go:364` builds a single `bkClient`, one
`Builder.BkClient` field (`:551`), no client collection anywhere non-test,
`BuildkitAddress` is one string. **One build -> one buildkitd, always.** So
distribution is the CI matrix (as buck2 already shards), coordination is the
mesh. Intra-build multi-host routing is an optional future earthbuild change at
`build_cmd.go:364`; not needed here.

## Sequencing

| phase | what | fork | weeks |
| ----- | ---- | ---- | ----- |
| P0 | payload/fleet trait split (+ lease service) | none | 1-2 |
| P1 | mesh OCI registry facade | none | ~3 |
| P2 | **distributed single-flight** | buildkit ~60 LoC | 3-5 |
| P3 | mesh-backed cache mounts | buildkit (plugin) | 4-8 |

P1 + P2 are the headline (warm cache + build-once). P3 is the remaining
Satellite warmth (`type=cache` mounts); gate on P1/P2 numbers.

## P0 — payload/fleet split

Only four things reach into `re::`:

| leak | today | fix |
| ---- | ----- | --- |
| `Driver::execute` | takes `action_digest: &Dig` | takes an opaque spec |
| `Driver::validated_ac_get` | decodes `re::ActionResult` | payload hook for digests |
| `D2W::Run { action: Dig }` | assumes the spec IS a CAS blob | `Run { spec: Vec<u8> }` |
| `PlatKey::from` | reads `re::Platform` properties | payload hook |

Everything else — queue, blooms, providers, `peer_conns`, `affinity_owner`,
`memo_servable`, `finalized_shards` — is already payload-blind.

### Traits + the lease service

```rust
/// Scheduling metadata extracted from an opaque job spec.
pub trait JobSpec: Send + Sync {
    fn platform(&self) -> PlatKey;
    /// Pin jobs sharing this key to ONE worker, ONE dir.
    /// buck2: `__<crate>__/` prefix (SVH/E0460). buildkit: the target
    /// (node-local type=cache mounts). Same reason: some state won't travel.
    fn affinity(&self) -> Option<String>;
    fn locality_hints(&self) -> Vec<Dig>;
    fn encode(&self) -> Vec<u8>;
}

/// Driver-side: the protocol the build tool dials.
#[async_trait]
pub trait Frontend: Send + Sync + 'static {
    async fn serve(self: Arc<Self>, fleet: Arc<Fleet>, addr: SocketAddr) -> Result<()>;
    /// Blobs a cached result references. The fleet refuses a hit whose blobs
    /// it can't produce (validated-AC invariant; optimizations.md).
    fn result_digests(&self, result: &[u8]) -> Vec<Dig>;
}

/// Cross-daemon single-flight. The BuildBuddy pattern, mesh-native.
/// buck2 calls it in Driver::execute; the buildkit fork calls it (via the
/// localhost agent) before sharedOp.Exec.
#[async_trait]
pub trait LeaseService: Send + Sync {
    /// Claim `key`. Ok(Claimed) => YOU build it, then publish_result.
    /// Ok(Wait(rx)) => a peer holds it; await the result (a cache hit).
    async fn claim(&self, key: &str) -> Result<Lease>;   // heartbeat while held
    async fn publish(&self, key: &str, result: &[u8]) -> Result<()>;
}
```

The lease is a new **state** over the existing AC table: `key -> InProgress
{worker, deadline}` alongside `key -> Result`. TTL + heartbeat; a dead holder's
lease expires and the next claimant builds; hedged execution if a holder
over-runs (BuildBuddy §stall). `Fleet` owns it.

```text
src/fleet/            mesh.rs store.rs driver.rs(-REAPI) worker.rs lease.rs
src/payload/reapi/    rpc.rs exec.rs        # buck2 today; bazel is a config
src/payload/buildkit/ registry.rs snapshotter.rs
```

## P1 — mesh OCI registry

OCI distribution v2 over the existing CAS. earthbuild already speaks
`type=registry` (`builder/solver.go`). No shim, no earthbuild change. `mode=max`
to export intermediate vertices; wire surface identical to `mode=min`.

### Wire surface — six routes

Verified against containerd's `remotes/docker`:

| route | methods | notes |
| ----- | ------- | ----- |
| `/v2/<name>/manifests/<ref>` | HEAD, GET | Serve OCI ImageManifest + Index. **Pass annotations verbatim** — esp. `containerimage.inlinecache` (fork keeps it; strip it and `--use-inline-cache` breaks silently). |
| `/v2/<name>/manifests/<ref>` | PUT | HEAD existence check first. |
| `/v2/<name>/blobs/<digest>` | HEAD, GET | **Accurate `Content-Length` mandatory**: importer caps the config blob at `1<<20` and rejects on mismatch, silently (`import.go:133-135`). |
| `/v2/<name>/blobs/uploads/` | POST | `Location`; if `?mount=&from=` present return **202** not 404 (`pusher.go:210-229`). |
| `<Location>` | PUT `?digest=` | Full body, verify, store. |

Not needed (verified): no PATCH (`pusher.go:280` TODO), no Range GET, no `/v2/`
ping, no auth, no TLS (`MatchLocalhost` forces plain HTTP for `127.0.0.1`), no
Referrers. Config mediaType `application/vnd.buildkit.cacheconfig.v0`.

**Scope ~200-400 lines axum.** `ferro-oci-server` (35 dl), `buildkit-client`
(349 dl) are one-person projects — crib routes, not the dep. ⚠︎ 0.8.

### Done when — P1

Runner B hits cache on runner A's layers, blobs over iroh, store survives via
`actions/cache`. **Baseline is ghcr.io `type=registry` at steady state** (>=3
runs, source-changing branch). Honest: punch mean 70.6 MB/s vs ghcr.io
~100-150; the edge inverts under concurrent egress. The differentiators are
GH-cache pre-seed overlapping the join window + no external auth. The bar is one
flag (`--remote-cache=ghcr.io/...`); if P1 can't beat it at steady state, stop.

## P2 — distributed single-flight (the build-once feature)

> The concurrent-dedup half. P1 is the prerequisite (results publish to, and
> waiters fetch from, the mesh cache).

**Lease key = the fast cache key** (`currentIndexKey()`, `edge.go:263`) —
structurally what BuildKit already exports for `--cache-from`, so **stable and
identical across machines, no machine-local state** (confirmed). The slow
content key can't be used: it only exists *after* the dep op runs.

**Key stability != output determinism.** The key is deterministic even when the
output isn't (`RUN apt-get update`). So a non-hermetic step single-flights
correctly — and it *fixes* divergence: today two racers get two different
layers; under the lease they share one. Strictly better exactly where you'd
fear it's worse. Containerized steps have ~0 local state in the key; the one
carve-out is `LOCALLY` (runs on the host) — **pin to the caller, exclude from
lease and dispatch** (Earthly already treats it as uncacheable).

**Local-source key: prefer the git tree hash.** The one remaining wobble is the
local-context source key. BuildKit's `fsutil` checksum hashes content+path+mode
(not mtime) — *should* match across machines but can drift on line-endings,
symlink handling, or fs quirks. For a git-tracked context, the **git tree hash
is the same content key by construction** — it already covers names + mode
(incl. the exec bit) + recursive blob content and *ignores mtime/inode* — and is
precomputed, identical across machines, zero fs walk. Fast path: when the
context is a clean checkout, key the local source on `git rev-parse HEAD^{tree}`
(or a subtree hash) instead of re-walking. Caveats: (1) a dirty working tree
needs `git write-tree` or falls back to `fsutil`; (2) `.dockerignore`/`.earthlyignore`
must select the same set git tracks, else the hashes cover different files —
detect the mismatch and fall back. This is an optimisation + robustness belt on
P2's prerequisite, not a correctness dependency; the fast key is already stable
without it.

**Injection point** (three evaluated, only one works):

| candidate | verdict |
| --------- | ------- |
| blocking cache `Query()` | ✗ runs under the scheduler's `s.mu`; blocking deadlocks it |
| proxy-snapshotter `Prepare()` | ✗ snapshots get random UUIDs; two daemons can't agree a key pre-result |
| **`sharedOp.Exec()`** (`jobs.go:1194`) | ✅ runs in a `flightcontrol` goroutine, not under `s.mu`; safe to block |

The fork: before `op.Acquire()`, `claim(fast_key)` against the localhost rebuck
agent (-> driver over mesh). Claimed -> build, then publish. Wait -> skip exec,
import the peer's result via the existing remote-cache path for that key.
**~60 lines** across `jobs.go`/`types.go`/`llbsolver/solver.go`; no scheduler,
edge, storage, or snapshotter changes. (Wiring "skip + import peer result" may
add a little; ~60 is the agent's core estimate. ⚠︎ 0.8.)

**buck2 gets it for free.** `Driver::execute` (`driver.rs:1587`) mints a fresh
`job_id` per call — **no dedup today**. Route it through the same
`LeaseService` keyed on the action digest and concurrent identical actions
coalesce. Same component, both payloads; the BuildBuddy pattern, once.

### Done when — P2

Two concurrent runs of the same target across two runners execute the shared
vertices **once** (the second waits + imports), verified by an execution counter
on the mesh, build green.

## P3 — mesh-backed cache mounts

> The remaining Satellite warmth. Gate on P1/P2. ⚠︎ 0.75.

`type=cache` mounts (cargo/npm/go registries) are `MutableRef` with
`NoCommit: true` (`container.go:237`), never committed, unreachable by the
export pipeline; `GetRemotes()` is `immutableRef`-only (`cache/refs.go:68`).
[moby/buildkit#1512](https://github.com/moby/buildkit/issues/1512) open, no PR
(#1474 is its closed predecessor). They matter every commit: `COPY src/`
invalidates the layer so `RUN cargo build` misses the *layer* cache on every
source change; the CACHE mount is the 1-5 min saver. Ephemeral runners
cold-start every job.

Options, least fork first:

| option | mechanism | fork |
| ------ | --------- | ---- |
| (a) sticky affinity | `JobSpec::affinity` = target | none — intra-run only, always on |
| (b) copy-out/in | earthbuild **already ships** `CACHE --persist` (`converter.go:3453`): copy-out to an `ImmutableRef`, mesh-shippable | none — try first |
| (c) proxy snapshotter | `proxySnapshotterPath` seam: `Commit()` -> push to mesh, `Prepare()` -> lazy pull. **Plugin, not core patch** | ~500 LoC Go over containerd v2 snapshots proto (no Rust crate); Clipper.dev runs it in prod |
| (d) native export patch | commit+export mounts in the fork | core patch — last resort |

## Version pinning

Worker buildkitd, the P2 fork, and the P3 snapshotter proto track **one
EarthBuild/buildkit rev** (go.mod `replace` pins it). Gateway + session protos
were additive across v0.29-v0.31; risk is the fork's own additions. CI-diff
protos on bump.

## Risks

| risk | severity | mitigation |
| ---- | -------- | ---------- |
| P2 lease correctness (stale lease, dead holder, hedge storm) | **high** — distributed lock semantics | copy BuildBuddy exactly: TTL + heartbeat, hedged exec on stall, crashed holder -> expiry; single-writer per key |
| cache-mount transfer complexity (P3) | high | (b) first (no fork); (c) only if too slow |
| P2P slower than ghcr.io at steady state | medium | P1 benchmark answers before P2 |
| economics invert (data >> compute) | medium | `cargo`/`npm` compute-bound; benchmark partitions |
| fork maintenance drag (2 forks) | medium | prefer plugin seams; pin one rev; CI-diff protos |
| GH cache budget contention | medium | 10 GB is **per-repo across all caches** — OCI shards compete with buck2 shards |

## Non-goals

- **The buildkitd shim** (v2's P2). Deleted; real buildkitd, no impersonation.
- **Intra-build DAG sharding / multi-host routing.** Snapshot chains block the
  first; the second is a deferred earthbuild change, not needed to match
  Satellites.
- **Blocking the scheduler or the snapshotter for single-flight.** Both dead
  ends (`s.mu` deadlock; random snapshot UUIDs). The lease lives at
  `sharedOp.Exec` only.
- **Translating LLB -> REAPI actions.** Drops cache mounts (slower than no
  rebuck), output paths unknowable, non-hermetic caching wrong for `apt-get`.
- **Windows/macOS buildkit workers.** No pool overlap with the MSVC-buck2 sweep;
  no macOS buildkitd.
