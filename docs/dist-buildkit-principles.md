# dist-buildkit — principles

Why the design is the shape it is: claims about what the PRODUCT must be, each
paid for, most by being wrong first.

Deliberately not here: how to test it, how to debug it, how to avoid fooling
yourself. Those are working practice, equally true of any project, and live
where they are used — the header comments of the rigs in `rebuck2/tests/`,
which record the specific ways each one lied.

Companion to [dispatch-plan.md](dispatch-plan.md) — what we are building now:
**a buildkitd that distributes one build across a fleet**, for any buildkit
client. These are claims about that product. Where one names an earthly
target, it is naming an example of a unit every frontend declares — see §10
and §15.

These principles were paid for by the DEDUPLICATION line of work (single-flight,
consolidation, the mesh mirror). That line is measured and complete; its plan,
measurements and handover live on `giles-single-buildkit-with-dist` and PR #7.
They are ported here unchanged because they are claims about the PRODUCT, and
the product did not change when the mechanism did. Where a principle names
single-flight specifically, read it as "the coordination mechanism, whichever it
currently is" — §2's argument survives the move from dedup to dispatch, and §5
and §6 constrain dispatch more tightly than they ever constrained dedup.

## 1. The grid must behave as ONE machine

The north star. N buildkitds building one logical build must produce what one
buildkitd would have produced. Everything below follows from this.

On one machine, `RUN apt-get update` executes once and every downstream vertex
sees the same apt state. On a grid without coordination, machine A gets the apt
state from 10:00 and machine B from 10:05, and the final artifact is stitched
from both — a build **no single machine would ever produce**. That is not a
slower build, it is a different one.

## 2. Single-flight is a CONSISTENCY mechanism, not an optimisation

It reads like a performance feature — "don't build the same thing twice" — and
measuring it that way produces the wrong conclusion. Measured, on an idle
2-worker fleet:

| face | verdict |
| ---- | ------- |
| latency | **loses** ~`T_xfer` for a follower that arrives with the leader |
| throughput | wins — one build instead of N |
| **consistency** | **required** — it is what makes (1) true |

We nearly shipped a "skip the lease when the fleet is idle" gate on the strength
of the first two rows. It would have bought a little latency by making the grid
non-deterministic. **A correctness mechanism cannot be gated on load.**

## 3. One canonical result per key — first writer wins

If we accidentally build the same key twice (a race, or a fail-open), the LATER
result is discarded and the earlier, published one is adopted. The build is
already paid for; keeping it would leave the grid multi-valued for one key, which
is exactly (1) violated.

Liveness never requires keeping *your own* bytes — only *some* bytes.

> Implemented: a released success becomes the key's canonical answer, and every
> later claimant adopts it (`Claim::Done`). Only a success is canonical — a
> failure drops the entry, or one machine's transient OOM would be cached for
> the fleet. Measured on `+examples-1`: led 15, merged 30 across a solo run plus
> two concurrent instances — each vertex built exactly once, grid-wide.

## 4. Identity is what BUILDKIT matches on — not what we can compute

A lease key must be content-addressed AND machine-stable. Buildkit hands us
several keys per dep and they are not equivalent:

- **fast key** — the dep's own cache key (output >= 0). What buildkit actually
  matches on. Content-addressed for an image; `random:` for a local source,
  where it is per-run noise.
- **slow key** — a contenthash of the dep's RESULT (output `-1`). Present only
  when `ContentBasedHash` is set.

Rule: **use the fast key when it identifies the dep; fall back to the slow key
only when the fast key is `random:`; refuse when neither identifies anything.**

The slow key is a FALLBACK, not an extra ingredient. `RUN apt-get update` on a
fixed base is a cache hit on a second local run even though apt fetched different
bytes — the key is `f(base, command)` and never hashes the output. Mixing the
contenthash in as well poisons a key that already agrees across machines, over
bytes buildkit itself ignores.

Both halves of this were learned by getting them wrong: first by inheriting
`random:` (single-flight was INERT for every build with a `COPY`), then by
unioning the slow key in (vertices whose fast keys already matched still would
not merge).

## 5. Fail open, never fail wrong

No cross-machine identity => no lease => build locally, exactly as unmodified
buildkit would. Duplicate work is always correct. A wrong layer never is, and a
stall is worse than the duplicate work we set out to prevent.

Corollary: prefer keys that are OVER-specific to keys that are under-specific. An
over-specific key is useless (never merges); an under-specific key hands a
follower someone else's layer.

## 6. The coordinator is never on the data path

Layers travel leader -> follower, peer to peer. The coordinator arbitrates the
lease and nothing else. The test for this is deliberately blunt and hard to fake:
after the build, look at what is on the driver's DISK. If it is out of the data
path, the layer is simply not there. (Measured: 0 MiB.)

## 7. Determinism is the ceiling

A vertex's mergeability is bounded by its inputs' reproducibility — but the bound
is far looser than it first appears, and we twice mistook our own bugs for it.

Merging needs two things, and we learned them one failure apart:

1. **The KEY must agree.** It is `f(op, deps)` and does not hash the output
   (principle 4), so a vertex is key-mergeable whenever buildkit itself would
   call it a cache hit — including `RUN apt-get update`, whose output is wildly
   non-deterministic and whose key is perfectly stable. Measured on
   `+examples-1`: **14 of 14 keys agree**, cache mounts and unpinned apt
   included.
2. **The ADOPTION must be sound**: the published layer must BE the whole
   result. A cache-mounted vertex fails this — bazel keeps its real output tree
   in the mount and leaves only a symlink in the layer, so a follower adopting
   it gets a dangling result (measured: `readlink -f ./bazel-out` -> nothing).
   Key agreement is necessary, not sufficient. Such vertices are excluded from
   the lease (`hasCacheMount`) until mounts are fleet-shared (P3).

The identity bound proper is narrower still: an input whose IDENTITY differs
across machines cannot merge. In practice that means a local source with no
content key at all — we refuse the lease there (principle 5) rather than invent
one.

Twice we blamed reproducibility for what was our own key derivation:
`random:` inheritance, then unioning the slow key in. Both times the vertices
were mergeable all along. **Before concluding "this build is too
non-deterministic to merge", check that the key derivation is not the thing
diverging** — the pre-hash string says which, in one run.

Where a build IS non-reproducible, note the tension with (2): the lease is then
carrying MORE, not less. It is what makes one machine's result canonical for the
whole grid, and so supplies the consistency the build itself lacks. The less
reproducible the build, the more the lease is carrying.

## 8. Resolution happens ONCE — the grid must agree what it built ON

Principle 1 says the grid must produce what one machine would. Single-flight
only ever delivered half of that: it makes each vertex build once, and says
nothing about the base it built on.

Every machine resolved every tag for itself. Measured on the stem, per daemon,
per run: **7 resolutions**, of references like `alpine:3.22`, `alpine:3.13`,
`alpine:3.19`, `golang:1.21-alpine3.19` — all MUTABLE. Docker Official Images
are republished under the same tag for CVE rebuilds, so a tag that moves
mid-run leaves part of the fleet on the old base and part on the new. That is
principle 1 violated, by exactly the mechanism its own apt example describes.

It degrades doubly, and the second half is nastier than the first: differing
digests yield differing lease keys, so the merge rate collapses to zero —
indistinguishable from an unreachable coordinator, which is a failure we have
already spent ten days misreading once.

So resolution is coordinated like execution: first machine to ask publishes the
digest, the rest adopt it. Measured: `resolve_merged=6` — six times a machine
took the fleet's answer instead of asking the registry.

Two corollaries, both learned the hard way:

- **Coordinate the seam that FIRES.** We wired `ResolveImageConfig`, which for
  this workload is called **0 times**; the live seam is `resolveSourceMetadata`
  at 7. Instrumentation settled in one run what inference had got wrong.
- **Adopt without reconstructing.** A follower resolves the PINNED reference
  down the ordinary path rather than rebuilding a serialised response.
  Correctness stays with code that already works; only the digest travels.

## 9. The origin registry is a fallback, not a data path

The sibling of principle 6. The coordinator is off the layer path; the upstream
registry should be too.

Agreeing a digest still leaves N machines fetching the same bytes from Docker
Hub. earthbuild already pays for this and says so in its own Earthfile: *"The
inner buildkit requires Docker hub creds to prevent rate-limiting issues"* — and
because `earthly-entrypoint.sh` starts a buildkitd inside every test container,
a CI run makes those requests from ~480 daemons rather than 12.

Buying credentials is a workaround for the symptom. The fix is to fetch once
into the fleet and serve peer to peer, which the mesh is already equipped for:
the driver keeps a bloom per peer, `HasMany` confirms what blooms only route.

The bound is honest: **blooms may only ever lie in the safe direction**. A
claimed holder must still be confirmed before anyone waits on it — an
unconfirmed false positive is a stall, and a stall on a hot path is worse than
the request it was avoiding.

And it is a rate-limit and determinism argument, NOT a speed one. Whether a peer
beats a CDN is unmeasured; every speed prediction this project has made from
first principles has been wrong. Fetch-once is worth doing because the fleet
should not be N customers of someone else's quota — if it is also faster, that
is a result to measure, not a premise to assume.

## 10. Hand over TREES, not vertices

The unit of work one machine asks another for is a subtree — in earthbuild
terms, a target. Never a single vertex.

In the built system the unit is a whole gateway `Solve`, which is a graph and
therefore satisfies this comfortably. Cutting a Solve into smaller subtrees is
untested rather than rejected: everything measured routes Solves whole.

A vertex's inputs are usually larger than its work. Measured on two shards, over
half of every exec vertex is milliseconds of it — 58% of group3's 214 and 54% of
group5's 521 are `echo`, `test`, `diff`, `mkdir`. Handing one of those to a peer
means shipping its input snapshot, running for 5ms, shipping the result back,
then shipping the same inputs out again for the vertex that depends on it. The
transfer is the work.

Send the subtree and the arithmetic inverts:

| | per vertex | per subtree |
| ---------------- | ---------------------- | ----------------------- |
| inputs | once per vertex | **once** |
| intermediates | cross the wire, twice | **never leave the peer** |
| paid for | everything | **the boundary only** |

And the boundary does not need inventing. An earthly target already is one: a
chain of vertices with one output and a declared frontier, written by the author,
connected by `BUILD` edges, and already what the lease key keys on. A
partitioning heuristic here would be us re-deriving, worse, a boundary the
Earthfile states outright.

**Every frontend declares one, and buildkit already carries it.**
`OpMetadata.ProgressGroup` is buildkit's own grouping of vertices into logical
units — an earthly target, a Dockerfile stage, a dagger step — and it is on
the wire for any client. So "use the author's boundary" is not an earthly
convenience; it is available wherever the graph is. A client also hands over
one `LLBBridge.Solve` per such unit, which is a boundary delivered rather than
inferred.

The same error has a smaller twin in the coordination protocol: `claim` is one
round trip per vertex, and one pair run recorded `led=828` — over half of them
for vertices cheaper than the round trip that asked about them. Per-vertex is the
wrong granularity for talking about work as well as for moving it.

Three consequences, all conservative:

- **Exclusions propagate upward.** One `LOCALLY`, one cache mount, one secret,
  one privileged exec anywhere in the subtree excludes the WHOLE subtree. A
  partially-dispatchable tree is not dispatchable.
- **Platform is the union of the subtree's constraints.** One linux-only vertex
  pins the tree.
- **Failure granularity is the subtree.** It fails as a unit and re-runs as a
  unit, which is the price of not paying for its interior.

## 11. A tree subdivides at its narrowest declared seam

Principle 10 says the unit is a tree. The obvious objection is that a tree can be
too big -- `+earthly` is most of a shard, and handing it over whole leaves
nothing to balance.

It subdivides, and not by a heuristic: an Earthfile already DECLARES several
seams, and they differ in the only thing that matters -- how much has to cross
the wire for the piece to be built elsewhere.

| seam | what the frontier is | width |
| ------------------------- | --------------------------------- | ----- |
| `FROM <registry image>` | a digest any machine can fetch | **~free** -- the mesh already serves it (P4b) |
| `COPY +target/artifact` | one named artifact | narrow |
| `RUN --mount=from=...` | one mounted path | narrow |
| `FROM +target` / `BUILD +target` | a whole snapshot or image | wide |
| mid-chain, between two RUNs | a whole snapshot, AND a target cut in half | widest, and undeclared |

So the rule is **cut at the narrowest declared seam available**, not "at the next
`BUILD` edge". A chain rooted at `FROM alpine:3.24.1` is the best possible
handover: its entire frontier is a public digest, so a peer needs nothing from
us at all. A `COPY +deps/lockfile` boundary is next best -- one artifact crosses,
not a rootfs.

Read off the LLB, which is where a generic client's seams actually are: a
`SourceOp` whose identifier is `docker-image://` IS `FROM <registry image>` —
its frontier is a digest any machine can pull. A `local://` source is the
build context, which lives on the invoking machine and is `LOCALLY` in all
but name. So the table is computable without reading an Earthfile, and
`dispatch::analyse` computes it.

The word DECLARED is load-bearing. The moment we cut somewhere the Earthfile does
not name, we are inventing a boundary and have re-introduced the partitioning
heuristic principle 10 rejects -- and paying a full snapshot for the privilege.

Exclusions and platform still propagate upward within each piece: a child
containing `LOCALLY` is undispatchable, and its parent is undispatchable AS A
WHOLE, but the parent's OTHER children remain free to travel.

Corollary: seam width is a property we can MEASURE, not merely rank. Preferring
the narrowest available frontier is a scheduling input the timing store can
learn, the same way it learns duration.

## 12. Finishing beats starting -- worker-to-worker work has priority

When worker A subdivides its tree and hands a branch to worker B, **A is
blocked on B**. That work has a machine waiting on it. New work from the driver
does not.

So: a worker takes worker-to-worker work first, and pushes back on the driver.
Refusal IS the backpressure -- a driver that cannot place work has learned the
fleet is saturated, without needing a metric to tell it.

Three reasons this is a principle and not a tuning knob:

- **Completions set makespan, starts do not.** A fleet that always accepts new
  work converges on every machine being 90% through something and nothing
  finishing. The queue looks busy and the build does not progress.
- **A part-built subtree holds state**: materialised inputs, intermediate
  snapshots, a warm daemon. Interleaving a second tree either evicts that state
  or doubles the footprint, and both are worse than waiting.
- **Without it, subdivision makes things worse rather than better.** If B
  prefers fresh driver work, A stalls while holding everything it has built --
  so the very mechanism meant to improve balance produces a fleet of blocked
  machines sitting on warm state. Subdivision without backpressure is a
  regression.

Corollary: the driver must be able to be told "no". A dispatch protocol where
the coordinator assigns rather than offers cannot express this, and would have
to rediscover it as a load metric -- later, and worse.

## 13. Estimates are COARSE on purpose -- that is what makes them survive

The timing store is keyed on the target and the args that reach it. Never on the
cache key, and never on content. That is deliberate, and it is the opposite of
what every other key in this system wants.

A cache key must be EXACT: under-specific and a follower gets someone else's
layer (principle 5). An estimate must be STABLE: it is consulted to decide
scheduling order, how deep to subdivide, and what is worth dispatching -- and
being wrong by 20% costs a slightly worse schedule, while having no entry at all
costs no schedule.

Outside earthly the same key exists under a different name:
`OpMetadata.ProgressGroup`, or the `llb.customname` description buildkit
renders in progress output. Both are coarse, both are stable across commits,
and both are on the wire — so the estimate generalises without becoming a
cache key by accident.

Key the estimate on content and it is perfect and useless: every commit
invalidates every sample, and a build system whose input changes constantly
would carry a permanently cold statistics table. Key it on the target and it is
approximate and durable -- `+deps` takes about as long as it did last week
whether or not a source file moved, because what dominates its duration is what
it DOES, not which bytes it did it to.

So: precision and robustness are in tension here, and we choose robustness. The
one lesson to carry is that **the same identity must not serve both jobs.**
Reusing the cache key as the statistics key looks like tidy engineering and
produces a table that is empty exactly when it is needed.

Two consequences:

- **Estimates may only feed decisions where being wrong is CHEAP.** Ordering,
  subdivision depth, dispatch selection: all recoverable. Never correctness,
  never a cache-hit decision, never an exclusion.
- **The first build of anything has no statistics, and that must be survivable
  rather than special-cased.** Fall back to not subdividing and to structural
  order; the table fills as a side effect of the run, and run two is already
  informed. A cold-start path that has to be maintained separately is a second
  scheduler nobody tests.

## 14. Bank by GENERATION, not by size

Most of what a build produces dies young: it is rebuilt next commit and the
banked copy is never read. A minority survives for months -- base images,
dependency trees, toolchains, the stem. Banking both costs the same upload and
returns wildly different value.

So promote by SURVIVAL, exactly as a generational collector does. Track, per
coarse key (principle 13), how many consecutive builds produced the same content
digest. Below the tenuring threshold -- two or three generations -- do not bank
it at all; it will be invalid before anything reads it, and every byte spent on
it is spent twice: once uploading, once evicting.

    changes every commit   -> never bank. Upload cost, zero hit rate.
    stable 2-3 builds      -> tenure it. High hit rate per byte.
    stable for months      -> the stem, the base images. The whole prize.

This is a different question to `bank/`'s existing compaction policy, which
decides WHEN to repack what is already banked. This decides what is admitted at
all, and it is the cheaper lever: nothing beats not uploading.

The statistic is a sibling of the duration one and wants the same coarse key for
the same reason -- content-keyed stability is a contradiction, since the key
changes exactly when the content does. Key on the target and ask "did this
target's output digest change between runs", which is answerable and stable.

Corollary: a cheap way to be wrong is to bank by SIZE, on the reasoning that big
things are expensive to rebuild. Size is uncorrelated with survival. A 2 GB image
layer rebuilt every commit is worth less than a 40 MB toolchain that has not
moved since March.

## 15. The client must not have to change

The product is a buildkitd. A user points `BUILDKIT_HOST` at it and gets a
fleet; that is the whole interface. Not a plugin, not a patched earthly, not
a fork of buildx, not an SDK.

This is a product claim and not a convenience, because it decides what we are
allowed to build. Any design that needs a change in the client — a flag, a
hook, an agreed side-channel, a cooperating frontend — has stopped being a
distributed buildkit and become a feature of whichever client agreed to it.
That is the position the dedup line was in, and it is why the earthbuild
version of this could only ever help earthbuild.

Three consequences:

- **The wire is the contract, and it is not ours.** `Control` and `LLBBridge`
  are versioned by moby. We serve them; we do not extend them. A capability
  we wish existed is a capability we do without, or upstream.
- **We only see what a client already sends.** Measured: a client-built graph
  arrives at `LLBBridge.Solve`, and a frontend requested BY NAME sends no
  graph at all because it runs inside the daemon. The second case is not a
  gap to close — there is nothing on the wire to distribute, and that build
  was always going to run on one machine.
- **Degrading has to be invisible.** A fleet with no peers, a graph we cannot
  cut, a subtree nobody will take: every one of these must produce the build
  an ordinary buildkitd would have produced, at ordinary speed. Principle 5
  said fail open; §15 says the client must not be able to tell.

And the tension this principle has to own rather than hide: earthbuild, the
first consumer, dispatches 1 solve in 12 and cannot do better without an
upstream patch — see [earthly-dispatch.md](earthly-dispatch.md). That looks
like the thing §15 forbids. The distinction it turns on is narrow and worth
stating: earthly SENDS its graph, so we distribute what a client already
sends, and the patch removes plumbing earthly attaches for a debugger that is
switched off. We need no cooperation, no flag and no side-channel; we need a
client to stop making every `RUN` undispatchable by accident. A reader should
still weigh that for themselves, because "it is really their bug" is what
every violation of this principle would say about itself.

Corollary, and it is the honest cost: **we inherit the whole Control surface**
— disk usage, prune, build history, cache import and export — whether or not
we distribute any of it. Being a convincing daemon is most of the work of
being one, and a client that hits an unimplemented method has been told, in
effect, to change.

## 16. Seed in pieces -- the first copy is the one that is one machine wide

A cascade forms on its own and works. Measured on one full
`+test-no-qemu`: worker 1 arrives first, finds nothing on any peer, and
pulls **75 blobs from the coordinator against 3 from peers**; worker 2,
arriving into a fleet that now holds something, gets **26 of 36 from
peers**. Nobody designed that and nothing needs to: a bloom filter per
peer is enough for the tree to build itself.

What does not spread is the SEED. One machine pulls the whole base off the
coordinator while the others wait for it to finish, because a peer cannot
serve what it has not got yet. The cascade is only as fast as its root,
and its root is one link.

So give every blob an owner, computed from its own digest:

    seeder_for(hash, peers)   // sorted peers, indexed by the digest's tail

Six workers then take six different sixths off the coordinator **at the
same time** and exchange the rest. In the time it used to take to seed one
machine, six each hold a sixth -- and from then on any machine can be
served from six sources at once rather than one.

The property that makes it work is that nobody coordinates. Every worker
computes the same owner from the same digest, so two of them never fetch
the same blob and no message is needed to arrange it. That requires the
function to be genuinely deterministic: sort the peer list first, or the
answer depends on whichever order a hash map happened to iterate in, and
the agreement it exists to provide is gone.

It also requires the distribution to be even, which is easy to get wrong
in a way that is invisible. The first version indexed by the digest's
REVERSED hex digits, which puts the least-variable characters in the low
bits; over 1200 hashes it sent almost everything to a single worker. A
seed-splitter that silently does not split is worse than none, because it
looks like it is working. Test the spread, not just the agreement.

Costs and limits, because they decide when this is worth it:

- It only pays when several machines want the same large blobs at once,
  which is exactly the base-image case and not much else.
- The serving side has to be willing to fetch what it does not hold -- and
  only from the ORIGIN, never from another peer, or two workers can wait
  on each other.
- It shortens transfer, not unpack. Every machine still decompresses and
  unpacks its own copy, so this is bounded by whatever fraction of the
  cost is on the wire.

## 17. The instrument is part of the system, and it lies too

Every mechanism here is judged by a measurement, so a wrong measurement is
worse than a wrong mechanism: it is a wrong mechanism that gets kept, or a
right one that gets thrown away. In one day of work on this branch the
instruments produced eight distinct false readings, and they fall into
four shapes worth naming.

**A failure that produces no evidence scores best.** earthly prints
`*failed*` once per target it ran and failed. A build that DIES prints
none of them, so the leg with zero failed targets was the one that
crashed, and it beat a leg that merely had a red test. The parity check
called it a win for the fleet. Judge on the exit code, and treat "no
evidence of failure" as a distinct outcome from "evidence of no failure".

**An aggregate over a bimodal distribution describes neither mode.**
`+base` averaged 3.0s in one leg and 20.5s in the other, reported as a
6.7x tax. Its real shape is p50=0ms across 575 cache hits plus nine spans
of 191-282s that are targets BLOCKED on a dependency. The mean was
arithmetically correct and pointed at a fix that would have done nothing.
Print p50 and the tail, or print nothing.

**An instrument's own text is indistinguishable from its output.** CI
echoes each step's script, so the line that would print `not warmed`
greps exactly like the printing of it. Three iterations of "did the
warm-up run?" were answered by reading the script back as evidence. The
same shape: `git apply --check ... | head -5; echo rc=$?` reports HEAD's
status, and printed rc=0 for a patch that was already broken.

**A pattern that matches sometimes is worse than one that never matches.**
earthly right-aligns its target column to the longest target name in the
run, so `*failed*` lines are indented in some runs and not others. An
anchored pattern found them for weeks and then silently found none - which
is the died-check's signature, so a red test was reported as a crash.

The habits that actually caught these, in order of how often they worked:

- **Verify the instrument against a case where the answer is known.** The
  patch checker was proven by breaking a patch; the graph invariants by
  reversing op order and disabling pruning. Both found bugs in the
  checker rather than the code.
- **Keep the raw logs.** Every diagnosis that took a 20-minute round trip
  took it because the run had answered a question nobody had asked yet.
- **Distrust a clean number from a new instrument.** Each of the eight was
  caught because the previous one had taught that lesson, and the first
  few were not caught at all.
- **Refuse to ship a metric that contradicts a finding you trust.** Two
  definitions of "serial fraction" gave 0% and 25-44% where the hand
  reading says 71%. Neither shipped. A confident wrong number next to
  sound ones poisons all of them.

## 18. Pre-position assets against the work that is coming

Fetching on demand is correct and it is late. Correctness is why it
survives - a worker that pulls what it needs when it needs it is always
right - but "when it needs it" is the moment the work is due to start, so
the transfer is on the critical path by construction.

The timeline that makes the case, from one full `+test-no-qemu`:

| time                | phase                         | blobs a worker fetched |
| ------------------- | ----------------------------- | ---------------------- |
| join -> +284s       | the baseline leg runs         | **none at all**        |
| +2s into the fleet  | fleet leg starts              | 2                      |
| +32s                | still early                   | 16                     |
| +227s               | base chain nearly done        | 33                     |
| +302s               | fan-out starting              | 62                     |

Two idle windows, both wasted. The workers moved nothing for 284 seconds
while a machine they could see was building the very image they would need.
Then the bulk of the transfer landed exactly as the fan-out began - the
layer that finished minutes earlier had sat on one machine until somebody
asked.

So: when an asset becomes available and its consumer is still building,
send it. The build of layer N+1 is free time for distributing layer N, and
a chain three deep gives you that window twice.

What makes this safe to do speculatively:

- **Advisory, never required.** A prefetch that fails, arrives late, or is
  ignored leaves exactly the lazy pull that would have happened anyway. It
  can make a build faster and must not be able to make one wrong -
  principle 5 again, applied to bytes instead of work.
- **Fetch by the same path a build would.** Warm what would have been
  pulled, through local-then-peer-then-origin, or the prefetch populates
  something the real fetch does not consult.
- **Never block the taker.** Prefetching in the foreground would occupy the
  machine that a Lead is about to be offered to, which costs precisely what
  it saves.

And the limit worth stating, because it decides whether this is worth
building at all: pre-positioning shortens TRANSFER, not unpack. Every
machine still decompresses its own copy. It pays when the wire is on the
critical path and the window is genuinely idle - which here it is, for 284
seconds at a stretch.
