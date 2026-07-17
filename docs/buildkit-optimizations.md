# buildkit payload — optimisations

Found while building P1/P2, not yet done. Ordered by expected value.

**(0) supersedes (1).** Both aim at the same thing — get the coordinator off the
data path — but (0) does it by being buildkit's content store rather than a
registry it pushes to, which makes the transfer step vanish instead of
optimising it.

Companion to [buildkit-plan.md](buildkit-plan.md) (the design) and
[optimizations.md](optimizations.md) (the buck2 engine's perf journey, which is
where several of these ideas were already proven).

## 0. Be buildkit's CONTENT STORE, not a registry it pushes to

**The architectural one.** Everything below (1) is a workaround for solving this
at the wrong level.

The problem with the HTTP-registry design: a layer lives inside buildkitd's
containerd content store, which rebuck cannot reach — so the leader has to PUSH
it out and every follower has to PULL it back, and the coordinator sits on the
critical path for every byte.

But buildkit's content store is an INTERFACE, and we fork buildkit:

```text
worker/runc/runc.go:95      local.NewStore(root/content)     <- a content.Store
snapshot/containerd/content.go:14
                            NewContentStore(store content.Store, ns string)
```

Substitute a mesh-backed `content.Store` and the push/pull disappears entirely:

```text
leader   commits a layer -> content store -> which IS the mesh    (no push)
follower reads  a layer  <- content store <- nearest holder       (no pull)
```

P2P by construction, because rebuck's blob path already goes local store -> bloom
peers -> driver. No explicit transfer step, no coordinator in the data path, and
no double-storage (today a layer would live in BOTH buildkit's content store and
rebuck's CAS).

Scope, and what NOT to do:

| layer | content-addressed? | distributable? |
| ----- | ------------------ | -------------- |
| **content store** (blobs) | yes | **yes — it IS a CAS.** Do this. |
| snapshotter (overlayfs dirs) | no | No. The hard half — this is the `type=cache` problem (buildkit-plan P3). |
| full containerd (images/containers/tasks/leases) | — | Unnecessary. A huge API, none of it needed. |

**Do not pretend to be containerd.** The containerd *worker* wants images,
containers, tasks and leases. We need one interface: `content.Store` — Info,
Update, Walk, Delete, ReaderAt, Status, ListStatuses, Abort, Writer. ~200 lines
of Go, in-process, no gRPC.

The snapshotter stays local and DERIVES its snapshots by unpacking layers from
the content store, which is precisely why a distributed content store is
sufficient to share layers and why the snapshotter is not needed for it.

Bonus: everything buildkit reads or writes becomes fleet-shared automatically —
cache manifests, image configs, base layers — not just the single-flight path.

## 1. Layers should travel P2P, not through the coordinator — DONE

**Shipped and measured.** `rebuck2 worker --registry-port` gives each worker its
own agent; a buildkitd talks only to the agent on its own box.

```text
before (central registry)      after (agent per worker)
  driver store: 150 MiB          driver store:   0 MiB   <- never sees the layer
  amplification: 0.97x           worker 1 store: 154 MiB <- leader, loopback push
  => (N-1)x through one NIC      worker 2 store: 151 MiB <- follower, from the LEADER
```

The driver's store holding **0 MiB** is the proof: its read-through path CACHES
whatever it relays, so a single byte through the driver would be sitting on its
disk. Verified by `tests/e2e-buildkit-p2p.sh`.

Two things were needed beyond the agent itself:

- `--decentralized-cas`, so the driver REDIRECTS a fetch to whoever holds the
  blob instead of relaying (and caching) the bytes through itself.
- `BlobReq::Announce` — the agent tells the driver the instant a blob lands.
  Bloom gossip would get there in ~30s, but a follower blocked on a lease asks in
  ~0s, and until the driver knows who holds it, it relays: the centralised path
  at exactly the moment it costs most.

The original analysis, which still explains WHY:

**A leader PUSHES its layer to the
coordinator's registry and every follower PULLS it back down, so the coordinator
is on the critical path for every byte.

Measured by `e2e-buildkit-singleflight.sh` with a realistic 150 MB output layer
(what a cargo/npm build looks like to the layer store), 2 daemons:

```text
pushed by leaders:    2 blobs   153.95 MiB
pulled by followers:  1 blobs   150.05 MiB
amplification: 0.97x
```

~1.0x per follower, so **(N-1)x at fleet scale** — 8 workers put ~1 GB through
one NIC. This is the same centralisation that the buck2 side already fixed.

Note the trap: the FIRST version of this measurement used a 32-byte output layer
and reported `0.00x`, which would have said "no bottleneck, skip this". The
workload has to produce something before the number means anything.

The fleet already has the machinery, built for buck2 and unused by the buildkit
payload:

- `--decentralized-cas`: outputs stay on the producing worker; the driver keeps
  a `digest -> producer` index and redirects fetches.
- bloom gossip: workers advertise their holdings, so a hot layer is served by
  whichever peers already have it, not by its author.
- the provider index is *free* — the coordinator handed out the lease, so it
  already knows who built what.

So: the leader publishes descriptors WITHOUT uploading; the coordinator records
`descriptor -> leader endpoint`; a follower's `blobFetcher` asks the coordinator,
gets redirected, and pulls from the leader (or any peer that has it) over iroh.

Measured precedent: locality dispatch cut mesh traffic ~60x in CI on the buck2
side. Expect the same shape here — a base-image layer that N followers all want
should be served by N/2 of them, not by one.

Cost: `blobFetcher` learns to follow a redirect; the coordinator's OCI GET
answers `BlobResp::Provider` instead of bytes. Both already exist in the fleet.

## 2. The leader compresses on the critical path — MEASURED, it is real

`GetRemotes(createIfNeeded=true)` is what turns a freshly-executed snapshot into
a layer blob — i.e. it gzips the whole diff while the followers sit and wait. It
is on the leader's critical path *and* on every follower's — and worse than
either: `publish` runs in a defer inside `ExecOp.Exec`, so it blocks the
leader's OWN solve from seeing its own result.

Measured on `examples/bazel+image` (SFTIME, debug level):

```text
publishable (GetRemotes/compress) = 19.1s and 30.8s   on the two bazel layers
publish     (lease release+push)  < 1ms
adopt       (follower side)       = 4-22ms
```

30 seconds of compression before the solver learns the exec finished. On a
healthy disk that is latency; on a sick one it is plausibly a session-deadline
failure (see the bug note below).

Ideas, cheapest first:

- **zstd instead of gzip.** `compression.New(compression.Default)` is gzip.
  zstd is several times faster to compress at similar ratios, and BuildKit
  supports it natively (`compression.Zstd`). One-line change; measure.
- **Start compression eagerly.** The leader knows it is the leader the moment it
  claims. Nothing stops it compressing as the exec finishes rather than after.
- **Publish in two phases.** Descriptors first (so followers can start resolving
  and the lease is released), bytes second. Risky: a follower could fetch before
  the blob lands. Only worth it with (1), where the fetch is a redirect anyway.

## 3. Claim is a round-trip per vertex

Every ExecOp does an HTTP POST before it runs. On a wide graph that is hundreds
of serial round-trips against the coordinator, each one blocking a flightcontrol
goroutine.

- **Batch-claim the frontier.** The scheduler knows which edges are about to
  become executable; claiming them in one request would amortise the trip.
- **Keep-alive is already on** (`http.Client` reuses connections), so this is
  latency, not handshakes. Measure before building anything: if the coordinator
  is on localhost (it is — a rebuck agent per worker), the trip is sub-ms and
  this is a non-issue. Do NOT optimise this until a profile says so.

## 4. `hasBlob` is one HEAD per descriptor

A push of a 12-layer chain costs 12 HEADs before the first byte moves. The mesh
protocol already has `BlobReq::HasMany` (built for buck2's FindMissingBlobs) —
the OCI facade should expose a batch presence check and the pusher should use it.

Same shape as the buck2 fix that turned ~12 RTT-bound fetches/s into one
round-trip (see optimizations.md, `GetMany`).

## 5. LeaseKey is recomputed per exec

`LeaseKey` walks the dep DAG on every `edge.execOp`. It is memoized *within* a
call but not across, and the DAG is walked again for every vertex. For a deep
graph that is O(V*E) hashing.

Cache it on the `CacheKey` (it is content-addressed, so it can never go stale) —
`ck.leaseKey` alongside `ck.ids`. Cheap and obviously correct.

## 6. No hedging on a straggling leader

A leader that is slow (or wedged short of its TTL) blocks every follower for as
long as it takes. BuildBuddy hedges: past a threshold, dispatch a duplicate and
take whichever finishes first.

Deliberately omitted from v1 — it trades the very work the lease exists to save,
so it should be driven by a measurement (how often does a leader straggle?) and
not by instinct. Needs the per-vertex duration history we do not yet collect.

## 7. Base-image layers are pushed needlessly — measured, and it is NOISE

The descriptor chain a leader publishes includes the base image's layers, which
every worker already pulled from Docker Hub. I expected this to be worth fixing.

Measured: of 153.95 MiB pushed, the alpine base was **3.9 MiB — 2.5%**. And
`hasBlob` already dedupes it after the first build. For any build that actually
produces something, this is noise.

**Deprioritised on the evidence.** It is also riskier than it looks: a follower
whose base image is still LAZY has not materialised those blobs locally, so a
leader that skips pushing them would leave the follower unable to fetch them —
fail-open, rebuild, and single-flight silently does nothing. Not worth it.

## 8. Registry: no Range GET

Deliberate (BuildKit's cache path never asks). But lazy/estargz pulls DO, and
lazy pulling is exactly what makes a follower cheap — it should fetch only the
files a downstream op actually touches. If we ever want eStargz, Range lands
first.

## 9. Tags are driver-local

The registry runs beside the driver, so a per-worker registry (which (1) implies)
needs tags gossiped over the mesh. Blobs are content-addressed and need no
coordination; only the mutable tag namespace does. Small, but it blocks (1).

---

## 10. Does single-flight even PAY? — MEASURED, and the framing was wrong

The one I did not think to ask until the instrumentation answered it. Then I
answered it with a guess, and the numbers overturned that too.

**The guess** was that the payoff is a ratio: expensive-to-build + small-output
= win; cheap-to-build + large-output = lose. Same economics as the whole
distribution question (compute >> data, or don't bother).

**The measurement** (`rebuck2/tests/bench-singleflight-payoff.sh`) says the build
cost is irrelevant. Two machines, the second arriving `stagger` seconds after the
first; follower wall-clock:

| build | stagger | SF on | SF off | delta |
| ----- | ------- | ----- | ------ | ----- |
| 20s | 0s | 24.0s | 22.1s | **+1.9s** — SF LOSES |
| 20s | 10s | 14.1s | 21.0s | -6.9s |
| 20s | 20s | 4.1s | 21.1s | -17.0s |
| **40s** | 10s | 33.9s | 41.0s | **-7.1s** |

The follower's SF-on time fits `max(0, build - stagger) + T_xfer` (T_xfer ~= 4s
for a 50 MB layer). Subtract the control and **`build` cancels**:

```text
delta = T_xfer - stagger
```

Doubling the build time moved the stagger=10 delta by 0.2s. The build cost — the
thing the guess put at the centre — does not appear.

### What that means

**Single-flight is not a latency optimisation.** A follower that arrives WITH the
leader waits out the leader's whole build and then pays a transfer, where
building in parallel on an idle core would have been free: it loses ~T_xfer, and
the leader loses ~T_push on top. What the fleet gains is one build's worth of
CPU, and it pays on latency only iff `stagger > T_xfer`.

> **This section originally concluded "so it is a THROUGHPUT optimisation" and
> proposed gating the lease on fleet contention — skip it when idle, since
> parallel building is free. That is WRONG, and the numbers above cannot see why.**
>
> Single-flight is a **CONSISTENCY mechanism** (see
> [dist-buildkit-principles.md](dist-buildkit-principles.md), principles 1-3).
> On one machine `apt-get update` runs once and every downstream vertex sees the
> same apt state. On a grid that does not merge, machine A takes the state from
> 10:00 and machine B from 10:05, and the artifact is stitched from both — a
> build no single machine would produce. Gate the lease on idleness and you
> buy a little latency by making the grid non-deterministic.
>
> A correctness mechanism cannot be gated on load. The latency arithmetic below
> is real; it is simply not what decides whether the feature runs.

And no optimisation changes that. `T_xfer` is **irreducible**: the follower needs
the layer's bytes to unpack them, and lazy fetch (8) does not help because a
snapshot needs all of them. (0) removes the leader's push and the double-storage,
which is real, but it cannot make a concurrent follower faster than just
building.

### The levers: there are none — do not skip the lease

Every "skip the lease when X" this section proposed is dead, and they all die the
same death: skipping the lease means two machines build one key, and two builds
of one key produce two answers. The trigger does not matter.

- ~~**Skip for cheap vertices**~~ — build duration cancels out of the delta
  anyway, so it optimised the one variable measured to be irrelevant. Wrong twice
  over.
- ~~**Skip for huge outputs**~~ — `T_xfer` does scale with size, so this looked
  like the "right" lever. It is not: the big-output vertex is exactly the one you
  least want two divergent copies of.
- ~~**Gate on fleet contention**~~ — buys latency on an idle fleet by making the
  grid non-deterministic. See the correction above.

The lease is not an optimisation with a cost/benefit to tune. It is what makes
the grid one machine. The only honest lever left is **making the transfer
cheaper** — (0), which skips nothing and weakens no guarantee.

That also settles the question this section opened with. "Should single-flight be
unconditional?" Yes. Unconditionally.

### Still unmeasured

Throughput itself. Every number above comes from a 2-worker IDLE fleet — the case
where the feature looks worst. If its value is capacity, that is what to measure:
saturate the workers with queued work and compare builds completed/sec.

## Measured, not guessed

`/_rebuck/stats` now reports what actually crossed the coordinator (uploads,
serves, bytes each way) and the lease outcomes (led / merged / abandoned). The
e2e prints it.

Three of the guesses above have been overturned by looking:

- (1) **confirmed** at ~1.0x amplification per follower — but only once the
  workload produced a realistic layer. With a 32-byte output it measured `0.00x`
  and would have told me to skip it.
- (7) **demoted**: the base image was 2.5% of a realistic push, not the win I
  assumed.
- (10) **reframed**: build cost cancels; single-flight is a throughput
  optimisation. Its "skip cheap vertices" lever aimed at the wrong variable.

### The one that should have been caught first

Single-flight was **INERT for every real build** from the day it was written, and
four passing e2e tests said otherwise. A local source's cache key is
session-scoped, so buildkit stamps its digest `random:` — and `LeaseKey` walked
the dep chain and inherited it. Every vertex downstream of a `COPY` got a key no
other machine could compute. Measured on `./examples/go+build`: two instances,
four lease keys, **zero merges**.

It hid because **not one test had a `COPY`**. Every rig used
`RUN … /dev/urandom … sleep`, whose only input is a base image — the one shape
where the bug cannot bite. The random-marker trick that made those tests
"decisive" is exactly what kept local sources out of them: you cannot salt a
build with random content AND feed it real source files.

So the P2 "done when" bar was cleared by tests that could not fail. The fix is
one clause in `LeaseKey` (drop `random:` keys, use the content key beside them,
refuse when there is no content identity); the lesson is that a synthetic
workload agrees with whatever design produced it.
`rebuck2/tests/load-earthbuild-examples.sh` now runs earthbuild's own examples,
which is what found it in a single run.

Still unmeasured, and next:

- **throughput on a CONTENDED fleet** — the actual value proposition of (10), and
  the one thing an idle 2-worker rig cannot show
- claim latency per vertex (is (3) real, or is a localhost round-trip free?)

"Do not optimise this until a profile says so" applies to every line above,
including the ones I was most confident about. And: **prove it on a real graph,
or you have proved nothing.**

### The rig lies more than the product

Every one of these first presented as a product bug. Baseline-then-bisect
against a known-good reference before believing any of it:

- `merged=0` — a warm daemon cannot collide; the solo run had killed the race.
- apt `Network is unreachable` — no `NETWORK_MODE=cni`, so the worker fell back
  to host networking (no IPv6 route). Bisected by running the same rig with the
  STOCK buildkitd, which failed identically.
- an impossible log (one marker printed, another absent, same code path) —
  `KEEP=1` left a container up, and the readiness probe passed against the STALE
  daemon, so two runs silently tested the old binary.
- `Could not open file … (5: Input/output error)` — the host disk was full and
  the daemon died. The rigs now bin their CAS stores on exit; they were leaving
  ~1.8 GB per run.

And the instrumentation lesson: we could not SEE whether single-flight engaged —
the lease counters existed and nothing surfaced them, so the e2e asserted on
markers and passed while the feature did nothing. When two machines disagree the
digest tells you nothing; `LeaseKeyDebugString` (the pre-hash string) is what
names the diverging component. Both key bugs were found by diffing it.

---

## BUG (downgraded): the `$(...)` ARG-expansion failure does not reproduce

**Status: NOT currently reproducible.** After clearing the docker VM's disk
(it had been filling all day and later hit 100%), the identical run passes:
solo in 148s, pair merging 6/6. The bisection below was CONFOUNDED — each run
added ~8 GB to the VM, so "single-flight on" vs "off" was never the only
variable. The original 11-minute analyze phase, and the session deadline that
killed it, are both consistent with a starved disk.

Kept because the bisection METHOD was right and the table is honest data — it
just measured two variables while believing it measured one. The remaining
suspect with a number attached: `publishable` blocks the leader's own solve for
up to 30.8s on bazel-sized layers (see (2) above), which under disk pressure
could stretch past the gateway session deadline. If this failure returns, look
there first, and control the disk this time.

`examples/bazel+image` does:

```text
ARG jar = $(bazel cquery //:ProjectRunner_deploy.jar --output starlark ...)
```

A *non-constant* ARG: earthbuild RUNS the command and reads its **stdout** back
through the gateway session. With single-flight on, it fails:

```text
failed to expand ARG $(bazel cquery ...): non constant build arg read request:
DeadlineExceeded: no active session for jgxv7...: context deadline exceeded
```

Bisected, one variable at a time:

| setup | result |
| ----- | ------ |
| stock earthbuild, its own daemon | PASS |
| our rig's flags + STOCK buildkitd | PASS |
| **our binary, `BUILDKIT_SINGLEFLIGHT_URL` unset** | **PASS** |
| **our binary, single-flight ON** | **FAIL** |

So it is not the rig, and not the buildkit version bump. It is our code.

An earlier revision of this note blamed stdout: "single-flight adopts a layer,
and a layer carries no stdout". **That mechanism does not exist.** The code
(`earthfile2llb/converter.go:1015`) wraps the command so its output lands in a
FILE inside the layer (`withShellAndEnvVarsOutput(outputFile)`), solves it as an
ordinary vertex, and reads the file back with `ref.ReadFile`. An adopted layer
carries that file like any other. So the cause is genuinely unknown. Candidates,
all unverified, none to be trusted without an instrumented repro:

- the leader's `publish` (GetRemotes + push) runs before execOp returns, and a
  bazel-sized layer could hold the gateway request past the session deadline —
  the error IS a deadline: `no active session ... context deadline exceeded`
- the adopted ref is lazy, and `ReadFile` on it needs an unlazy path that our
  blobFetcher does not serve
- lease heartbeat/TTL interplay during the 11-minute bazel analysis

The bisection table above is solid; the mechanism is not. Reproduce with
logging before believing any of these.

This violates the fork's own contract, stated in `singleflight_test.go`:
"a fork that changes behaviour when you did not ask for it is a fork nobody will
merge". Here the behaviour changes when you DID ask for it, in a way that has
nothing to do with what you asked.

`examples-1` never used `$(...)`, which is why nothing caught this until the load
widened. Principle 8 again, one level up: a real graph is not one workload, it is
a distribution of them.
