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
| P2.1 | buildkit identity in the lease key | buildkit ~2 LoC | days |
| P4 | **source-op coordination (resolve + pull)** | buildkit | 3-5 |

P1 + P2 are the headline (warm cache + build-once). P3 is the remaining
Satellite warmth (`type=cache` mounts); gate on P1/P2 numbers.

**P2 is built and measured, and the wall-clock case did not land** -- see
[use-x86-as-fast-build-runners-notes.md](use-x86-as-fast-build-runners-notes.md).
It coordinates correctly across real machines and adopts the entire shared
prefix, but on earthbuild's test suite that prefix is I/O-bound, so adopting it
costs about what building it costs. P2.1 and P4 come out of that measurement and
are now the live work.

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

**That bar was cleared by tests that could not fail.** Every e2e used a RUN whose
only input was a base image (`RUN … /dev/urandom … sleep`), and single-flight was
INERT for any vertex downstream of a `COPY` — the local source's session-scoped
key is stamped `random:` by buildkit, and the lease key inherited it. Four green
tests, zero merges on a real build. The counter now exists for real
(`/_rebuck/stats` → `led`/`merged`/`abandoned`); before, nothing surfaced it and
the e2e asserted on markers alone.

Measured on earthbuild's own `+examples-1` (`rebuck2/tests/load-earthbuild-examples.sh`),
two instances, both cold:

| | |
| ---------------------- | ------------------------------------------------ |
| exec vertices claimed by both | 14 |
| lease keys **agree** | **10** |
| keys differ | 4 — and none is our bug |

The 4: one content-hashes a **cache mount** (machine-local mutable state — that
is P3 below); the rest content-hash a rootfs produced upstream by an unpinned
`apt-get update`, i.e. genuinely different bytes.

**So the real bar is: single-flight's merge rate is bounded by the build's
REPRODUCIBILITY, not by our key derivation.** A byte-reproducible target merges
everything (`./examples/go+build`: 2/2). A target with cache mounts or an
unpinned apt cannot, and no amount of work here changes that — only P3, or the
build becoming reproducible, does. Measure `merged/(led+merged)` against what the
workload can actually achieve, not against 100%.

## P3 — mesh-backed cache mounts

> The remaining Satellite warmth. Gate on P1/P2. ⚠︎ 0.75.
>
> **This note has now been wrong in both directions; the third version is
> grounded in a failure.** v1 said cache mounts POISON single-flight (keys
> diverge). False — that was our own slow-key union bug; with the slow key as a
> fallback, 14/14 keys agree. v2 then said cache-mounted vertices "merge like
> any other". ALSO false, and dangerously so: the keys agree but the ADOPTION is
> unsound. A cache-mounted vertex's published layer is not its whole result —
> `examples/bazel+build` (`CACHE /root/.cache/bazel`) keeps bazel's real output
> tree in the mount and leaves only a symlink in the layer. A follower adopting
> that layer gets a dangling symlink: measured, instance 2's
> `readlink -f ./bazel-out` returned nothing and the build failed. `cpp` merging
> happily was luck — its mount held only intermediates.
>
> So cache-mounted execs are now EXCLUDED from single-flight entirely
> (`ExecOp.hasCacheMount`, fail open, build locally): fail open, never fail
> wrong. Which restores P3's claim on the merge rate after all, by the third
> route: until mounts are fleet-shared, cache-mounted vertices cannot merge
> SOUNDLY, so P3 buys their warmth AND their mergeability.

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

## P2.1 — the lease key must name the buildkit that built it

`LeaseKey` is a function of graph content alone: op digest, dep chain, selector,
output index. It carries no builder identity, so two daemons at different
commits compute the SAME key and adopt each other's results silently.

Harmless on a homogeneous fleet, and earthbuild is homogeneous *today* only by
accident: `earthly-entrypoint.sh` starts a buildkitd inside every test container
(~480 per CI run), and its image is built from `./buildkitd+buildkitd`, so inner
and outer are the same commit. Pin a released buildkitd for the inner daemon —
the obvious thing to do — and two versions share one keyspace with no error.

Do the cheap half first: mix the buildkit commit into the key, so versions
partition into cohorts and never merge across. The ambitious half (carry version
as lease metadata, let adoption decide compatibility) needs a policy for what
"compatible" means when LLB semantics change, and is unsound without the cheap
half in place first.

**Fail loudly.** A version-partitioned refusal must be visible. A fleet whose
merge rate is zero because two daemons disagree looks exactly like a fleet whose
coordinator is unreachable, and that ambiguity has already cost ten days once.

### Done when — P2.1

Two daemons at different commits, same graph, produce different lease keys and
merge nothing; the refusal appears in `/_rebuck/stats` rather than only as an
absence of merges.

## P4 — source-op coordination (resolve + pull)

The lever P2's measurements point at. `SingleFlightKey` is consumed in exactly
one place — `solver/llbsolver/ops/exec.go:420` — so **ExecOps are coordinated
and source ops are not**. Every machine resolves and pulls every base image
independently.

That is where the duplicated I/O actually is:

- ~18 registry pulls per earthbuild CI job, uncoordinated
- ~480 inner buildkitds per run, each pulling independently. earthbuild already
  papers over this with Docker Hub mirror credentials — `Earthfile:564` says so
  outright: "The inner buildkit requires Docker hub creds to prevent
  rate-limiting issues"
- the 32 ExecOps we *do* coordinate are cheap by comparison

It is also a correctness fix, not only a performance one. Docker Official Images
are republished under the same tag for CVE rebuilds, so a tag that moves
mid-run leaves half a fleet on the old base and half on the new — section 1 of
[dist-buildkit-principles.md](dist-buildkit-principles.md) violated, silently.
Worse, the differing digests produce differing lease keys, so the merge rate
collapses to zero and looks like a broken coordinator.

Two mechanisms, and the first is most of the value:

1. **Lease the RESOLUTION.** First machine to resolve a reference publishes the
   digest; the rest adopt it. Makes the fleet single-valued about what it built
   on, independently of whether the tag moves. Cheap, and it fixes the
   consistency hole whether or not the pull is ever shared.
2. **Lease the PULL.** One machine fetches the layers and serves them to peers
   over the mesh, as it already does for ExecOp outputs. The transfer path
   exists and is proven — 368 MiB moved peer-to-peer with the driver store flat
   at 1 MiB.

### Done when — P4

Two runners building targets that share a base image resolve it to the same
digest via the coordinator, and the layers cross the mesh once rather than being
pulled twice from the registry. Measured, not asserted: registry egress drops,
`serves` rises, and a tag republished mid-run does not split the fleet.

**Do not accept "merges went up" as evidence.** P2's history is a catalogue of
plausible numbers from broken rigs: an unreachable agent, a shared working tree,
a stale pin, a masked pipeline. Every claim here needs the arms distinguishable
and the failure mode loud.

## P4b — serve image layers from the mesh, not the registry

P4a agrees a digest per reference. Every machine still *fetches* it, so N
machines make N registry requests for the same bytes. earthbuild already pays
for that: `Earthfile:564` -- "The inner buildkit requires Docker hub creds to
prevent rate-limiting issues" -- and every test container starts its own
buildkitd, so a CI run makes those requests hundreds of times over.

The parts already exist and were built for a different reason:

| piece              | where             | what it does today                |
| ------------------ | ----------------- | --------------------------------- |
| per-peer holdings  | `driver.rs:227`   | `blooms: HashMap<String, Bloom>`  |
| locality dispatch  | `driver.rs:43`    | prefers the worker holding inputs |
| exact confirmation | `mesh.rs:172`     | `HasMany` confirms what blooms route |
| OCI registry       | `registry.rs:753` | agent serves `/v2/{*path}`        |

So the fleet can already answer "who holds this digest?" It simply never asks on
behalf of an image pull, and the agent has no upstream path: a blob it does not
hold is a plain miss.

Shape: make the agent a **pull-through mirror**, and point buildkitd's registry
mirror at it.

1. blob requested; agent holds it -> serve
2. not held -> ask the driver whose bloom claims it, confirm with `HasMany`,
   fetch peer-to-peer
3. nobody holds it -> fetch upstream ONCE, store, serve

Turns N x M registry requests into 1 x M, which is the rate-limit fix rather
than a workaround for it.

Two properties are load-bearing:

- **Blooms are probabilistic and must never be trusted alone.** They lie only in
  the safe direction (false positives, never false negatives -- see
  `bloom_no_false_negatives_and_sane_fp`), so a claimed holder must still be
  confirmed with `HasMany` before anyone waits on it. A false positive that is
  not confirmed is a stall, not a wrong answer.
- **Fail open to upstream, always.** A mirror that fails a pull when the mesh is
  unavailable is strictly worse than no mirror. Same rule as every other part of
  this system, and the one it has broken before.

### Done when — P4b

Two machines building from the same base image produce ONE upstream registry
request between them, measured at the agent rather than inferred: `serves` rises,
upstream fetches do not. And with the mesh killed mid-run, both still build.

**Not a throughput claim yet.** Whether serving a layer from a peer beats
fetching it from a CDN is unmeasured and not obvious -- Docker Hub is fast and
close. The case for P4b is rate limits and determinism first; speed is a
hypothesis to test, not a premise.
