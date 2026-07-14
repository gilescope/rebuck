# buildkit payload — optimisations

Found while building P1/P2, not yet done. Ordered by expected value.

Companion to [buildkit-plan.md](buildkit-plan.md) (the design) and
[optimizations.md](optimizations.md) (the buck2 engine's perf journey, which is
where several of these ideas were already proven).

## 1. Layers should travel P2P, not through the coordinator

**The one that matters.** Today a leader PUSHES its layer to the coordinator's
registry and every follower PULLS it back down. The coordinator is on the
critical path for every byte, so a fleet of N workers funnels N-1 downloads
through one box's NIC — exactly the centralisation the mesh exists to avoid.

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

## 7. Base-image layers are pushed needlessly

The descriptor chain a leader publishes includes the base image's layers, which
every worker already pulled from Docker Hub. `hasBlob` catches this after the
first build, but the first leader still uploads a whole alpine/ubuntu.

Cheap fix: skip descriptors whose layers came from a `SourceOp` (they are
already addressable by their original registry), and let the follower pull those
from where it got them the first time.

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

## Measured, not guessed

None of the above has a number against it yet. The order is by *expected* value
and could be wrong. Before doing any of them, the thing to build is the
instrumentation:

- leases claimed / merged / abandoned (the driver already counts `sf_merged`)
- time a follower spent BLOCKED vs what it would have spent building
- bytes pushed by leaders vs bytes pulled by followers (is the coordinator
  actually a bottleneck, or is this all theory?)

"Do not optimise this until a profile says so" applies to every line above.
