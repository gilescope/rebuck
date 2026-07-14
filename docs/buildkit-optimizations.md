# buildkit payload — optimisations

Found while building P1/P2, not yet done. Ordered by expected value.

Companion to [buildkit-plan.md](buildkit-plan.md) (the design) and
[optimizations.md](optimizations.md) (the buck2 engine's perf journey, which is
where several of these ideas were already proven).

## 1. Layers should travel P2P, not through the coordinator

**The one that matters — and now measured.** A leader PUSHES its layer to the
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

## 2. The leader compresses on the critical path

`GetRemotes(createIfNeeded=true)` is what turns a freshly-executed snapshot into
a layer blob — i.e. it gzips the whole diff while the followers sit and wait. It
is on the leader's critical path *and* on every follower's.

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

## 10. Does single-flight even PAY? (the question the numbers raised)

The one I did not think to ask until the instrumentation answered it.

A follower avoided an 8-second build by downloading 150 MB. Whether that is a
win depends entirely on the ratio:

| the vertex | single-flight |
| ---------- | ------------- |
| expensive to build, small output (a compiled binary, a linked rlib) | **big win** |
| cheap to build, large output (unpacking an archive, generating assets) | **may LOSE** — the transfer costs more than the rebuild |

This is the same economics as the whole distribution question (compute >> data,
or don't bother), and it means single-flight should probably not be
unconditional. Options, none built:

- **Skip the lease for cheap vertices.** BuildKit does not know a vertex's cost
  up front, but the coordinator sees every build: a `key -> duration` history is
  free (the buck2 side already derives one from `ExecutedActionMetadata`).
- **Skip the lease for huge outputs.** Harder — the size is not known until the
  build finishes, and by then the followers are already waiting.
- **Make the transfer cheap instead** — i.e. do (1), and the question mostly
  goes away.

Which is another argument for (1) being first.

## Measured, not guessed

`/_rebuck/stats` now reports what actually crossed the coordinator (uploads,
serves, bytes each way) and the lease outcomes (led / merged / abandoned). The
e2e prints it.

Two of the guesses above have already been overturned by looking:

- (1) **confirmed** at ~1.0x amplification per follower — but only once the
  workload produced a realistic layer. With a 32-byte output it measured `0.00x`
  and would have told me to skip it.
- (7) **demoted**: the base image was 2.5% of a realistic push, not the win I
  assumed.

Still unmeasured, and next:

- time a follower spent BLOCKED vs what it would have spent building — the
  numerator of (10)
- claim latency per vertex (is (3) real, or is a localhost round-trip free?)

"Do not optimise this until a profile says so" applies to every line above,
including the ones I was most confident about.
