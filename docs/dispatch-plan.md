# A distributed BuildKit - one build, many machines

**The product is a buildkitd that distributes a single build across a fleet.**
Point any buildkit client at it and the client does not change: not earthly,
not buildx, not buildctl, not dagger. They already speak the wire; we serve
it.

That framing is newer than most of this document, and it is a widening rather
than a pivot. Everything below was written for earthbuild's CI, which remains
the first consumer and the source of every measurement here. But nothing in
the mechanism is earthly-shaped -- it is written in terms of `pb.Op`, because
that is what a build actually is -- and the constraint that kept it
earthly-shaped turned out not to exist.

Successor to the dedup plan (`buildkit-plan.md`, on
`giles-single-buildkit-with-dist` / PR #7). Its phases P1-P4b are
**deduplication**: N independent builds that avoid redoing each other's work.
They are built and measured.

This plan is the alternative, not the continuation.

The product claims both lines answer to are in
[dist-buildkit-principles.md](dist-buildkit-principles.md). Three of them bind
dispatch harder than they bound dedup: the grid must behave as ONE machine (§1),
fail open never fail wrong (§5), and the coordinator is never on the data path
(§6) -- which for dispatch means a subtree's inputs and results travel
peer-to-peer, never through the driver. A fourth arrived with the reframing:
the client must not have to change (§15).

## Why this is possible, and why nobody has done it

Two facts, and the second was measured rather than assumed.

**Nobody ships a distributed buildkit.** The remote-buildkit vendors -- Depot,
Blacksmith, Namespace, BuildJet -- all sell ONE BIG REMOTE DAEMON. That is a
faster machine, not a fleet, and it is the shape the next section explains: a
buildkit solver has global knowledge of exactly one build, so N daemons cannot
queue across each other. The gap in the market and the structural fact are the
same fact.

**The whole graph is visible on one connection.** A buildkit client drives its
build through `LLBBridge`, created with `NewLLBBridgeClient(c.conn)` -- the
same connection it speaks Control on, as a second service. So the `Definition`
lands in our hands with no change to the client and no cooperation from the
frontend.

That was got wrong once, expensively enough to record: `Control.Solve` arrives
with NO definition, and the obvious conclusion -- that the graph is somewhere
we cannot reach -- is false. It is on the other service. Three client shapes,
and only the middle one is invisible:

| client | where the LLB is |
| ------------------------------ | ------------------------------------ |
| raw LLB | `Control.Solve`, definition set |
| frontend by NAME | nowhere -- it runs INSIDE the daemon |
| client-built LLB (earthly) | `LLBBridge.Solve` |

The invisible case costs nothing: if no graph crosses the wire there is no
graph to dispatch, and that build was always going to run on one machine.

## The GOAL is settled, the MECHANISM is not

Everything below this section describes one way to distribute a build:
subdivide the graph, offer subtrees to peers, publish results peer-to-peer.
It is built and it works (M4). It is not established that it is the right
one, and this document previously read as though it were.

The candidates, and what would decide between them:

| mechanism | unit of work | biggest doubt |
| ------------------- | ------------------- | ------------------------------ |
| **A.** subtree dispatch | a closure we choose | we pick the cuts, so we can pick badly |
| **B.** gateway-Solve routing | one `LLBBridge.Solve` | enough Solves? balanced? |
| **C.** N solvers, one lease table | a vertex, claimed | principle 2: coordination is not latency |
| **D.** shared remote cache | nothing, after the fact | measured: buys cost, not latency |

A needs frontier analysis, offers and a subdivision depth. B needs routing
and a shared cache. C is the dedup line and is already built. D is what the
market already sells.

**B deserves more attention than it has had**, and it only became visible
today: a client drives its build through MANY `LLBBridge.Solve` calls -- for
earthly, roughly one per target. Those are units the author already declared,
handed to us with no analysis at all. If a real build makes enough of them,
and they are not wildly unbalanced, B gets most of A's benefit for a fraction
of A's machinery: no seam classification, no subdivision depth, no offer
protocol.

**So the first measurement is not "how many free-frontier cuts are there".**
That question only matters if A is the answer, and asking it first is how you
measure the wrong thing convincingly. The mechanism-neutral question is:

> **What does a real build actually look like on the wire?**

Number of gateway Solves, ops per Solve, wall-clock per Solve, platform
spread, source schemes, how much overlaps between Solves. That
characterisation prices A, B and C at once, and the proxy that can collect it
already exists.

## The first thing it should be good at

**Native multi-arch.** `buildx` builds other architectures by emulation, which
is slow, or by per-arch builders the user wires up and maintains. A fleet with
real arm64 and amd64 machines does it natively, and principle 10's platform
union is already the mechanism -- one linux/arm64 vertex pins its subtree to a
machine that is actually linux/arm64.

This is worth naming as the first target because it is a pain people already
have, it needs no subdivision heuristics to pay off, and it is the case where
"behaves as ONE machine" (§1) is most obviously worth more than a faster
single machine.

## Why dedup could not win the thing we kept asking it for

**On independent runners with spare capacity, duplicated work is free in
wall-clock.** Twelve runners each building the 94s stem *in parallel* costs 94s.
Twelve runners coordinating means one builds it and eleven BLOCK, then proceed:
the same wall-clock at best, worse by `T_xfer` at the margin.

So dedup buys cost and rate limits, and cannot buy latency. Measured,
repeatedly:
consolidation is *not slower*; single-flight on an idle box is *not faster*
(373s coordinated vs 342s uncoordinated, inside a ~10% run-to-run spread).

Both remain worth having. P4b took 100 origin requests to 25 across four
runners, which is a rate-limit fix rather than a speed one. But the latency
question needs a different mechanism.

## The one structural fact

**A queue needs one scheduler with global knowledge of readiness.** BuildKit's
solver already is exactly that -- per build. So a fine-grained queue exists only
if there is ONE build. With N independent solvers you can dedup between them and
never queue across them, because nobody knows what is ready.

The twelve `+test-no-qemu-groupN` shards exist *because* buildkit could not
distribute one build. `+test-no-qemu` already BUILDs all twelve and needs no
repo reorganisation. Give buildkit dispatch and the sharding is vestigial.

## The unit is a SUBTREE, and it already has a name

Dispatching one vertex at a time is absurd: ship the inputs, run 5ms, ship the
result back, ship them out again for the dependent vertex. Over half of every
shard is milliseconds of work (measured: 58% of group3's 214 exec vertices, 54%
of group5's 521 are `echo`/`test`/`diff`/`mkdir`).

Send a **subtree** instead and the arithmetic inverts:

- its external inputs transfer ONCE
- its intermediates are born and consumed on the peer and never cross the wire
- only its boundary is paid for

An **earthly target** is already that subtree: a chain of vertices with one
output and a declared frontier. No graph partitioning, no heuristic boundary
selection -- use the boundary the Earthfile author already wrote, that `BUILD`
edges already connect, and that the lease key already keys on.

**And the LLB says the same thing without the Earthfile.** That mattered more
than expected once the client stopped being assumed to be earthly:

- `Op.inputs` is the graph, so a subtree is reachability from any op -- no
  partitioning heuristic, just a closure.
- The subtree's frontier is exactly its SOURCE ops, and their identifiers say
  what a handover costs. `docker-image://` is a digest any machine can pull;
  `local://` is the build context, which lives on the invoking machine and is
  `LOCALLY` in all but name.
- `OpMetadata.ProgressGroup` is buildkit's OWN grouping of vertices into
  logical units -- an earthly target, a Dockerfile stage, a dagger step. It is
  the frontend-agnostic name for the thing this section is about, and it is
  already on the wire.

So "use the boundary the author declared" survives the generalisation intact:
every frontend declares one, and buildkit already carries it.

Fewer, larger units also make each decision affordable to get right, which is
the third argument against a cost model (below).

## What already exists

| piece | where | state |
| ------------------------- | ------------------------------- | ----- |
| serialisable work spec | `pb.ExecOp` - args, env, mounts, network, security | protobuf already |
| receive a peer's result | `adoptLeaderResult` -> `worker.FromRemote` | built (P2) |
| move layers peer-to-peer | mesh registry + blooms | built (P1/P4b) |
| fleet choreography | `rebuck2/actions`: one driver job, N worker jobs | built, stress-tested |
| persistence across runs | `bank/` - artifact pool, 8 ranges, primary-owner writes | built, stress-tested |

The transport is done. The missing piece is **dispatch**: one protocol message
saying *"lead on someone else's behalf"*, carrying a subtree's spec and the
descriptor chains of its frontier.

## Coordination must be batched, and mostly local

`claim` is one HTTP POST per key, and it LONG-POLLS for followers. Group1's pair
run recorded `led=828`: 828 round trips, over half of them for vertices cheaper
than the round trip.

Three levers, composing:

1. **A gossiped bloom of published keys.** Not present -> definitely not
   published -> build it, ZERO network operations. Present -> maybe -> one
   batched query. Most vertices in a cold run are published by nobody, so the
   common case costs nothing. `D2W::Blooms` already gossips; `mesh::Bloom`
   already exists and is already documented as lying "only in the safe
   direction".
2. **Split the protocol.** A non-blocking BATCH query per wave (`published /
   leading / free`), then blocking `claim` only for the few we intend to follow.
   One request cannot hold open on behalf of twelve keys where three are
   followers.
3. **Dispatch by subtree**, so there are far fewer decisions to coordinate.

## No cost model - a learned bloom instead

A cost model is only needed if a wrong decision is HARMFUL, and it is harmful
only because the requester blocks. Two things remove the harm:

- **Stall-triggered offers.** Anything still running after N seconds is by
  definition not a 5ms `echo`. Self-calibrating, and simpler than a threshold
  table.
- **A learned cheap-bloom.** Execute, record the duration, and add the key below
  threshold. False positive = "cheap when it isn't" = we do the work ourselves,
  which is what we were going to do. False negative is impossible. Learned from
  data, not declared as patterns -- the failure mode of a declared model is that
  a wrong threshold changes behaviour silently.

Deferred, not cancelled: at full utilisation, hedged work displaces real work.
That is also when we would have the data to build a cost model properly.

**Instrument first**, and the instrument mostly exists. BuildKit already
timestamps every vertex and streams it to the client -- `client/graph.go`'s
`Vertex` carries `Started`/`Completed`, which is how earthly renders progress.
What has no timings is the DEBUG LOG, which emits `creating` lines and no
completions; that is why "trivial vs minutes" above is inferred from command
text rather than measured. Recording what is already streamed needs no fork
change.

## A timing store, because a threshold is not a distribution

A single "is this cheap" bit is the weakest thing that could be learned. What is
actually wanted is **how long a target takes, as a distribution, subsetted by
the args it was given** -- because args change what a target does
(`--mode=0004` and `--mode=0777` are different work under one name).

    key    = target ref + the build args that reach it
    value  = duration samples (count, median, p90)

Populated from the status stream, banked with everything else. `bank/dice.rs`
already banks a key/value store as deterministic, replayable, order-independent
text deltas -- a timings table is that same shape, so persistence is a reuse
rather than a build.

It then feeds four things that currently each guess separately:

- **Rebalancing.** The reason step 0 stalled: a static proxy correlated only
  r=0.734 with measured group times and was 10x wrong on small groups, and the
  measured times were themselves confounded by warm/cold ordering. A timing
  store is the durable fix for both.
- **Scheduling.** Longest-processing-time-first is the standard makespan
  heuristic and needs exactly this data.
- **The cheap-bloom.** A learned threshold becomes a learned distribution;
  "p90 under 50ms" is a far better predicate than one observed sample.
- **Dispatch.** Which subtree is worth shipping, when we eventually want that
  question answered rather than deferred.

## The fleet is heterogeneous

Runners are not interchangeable. earthbuild's own CI already spans linux, macOS
and windows (`giles-mac-worker-timeout`; a windows worker once burned 5h dead),
and dispatch must respect that or it will route work to a machine that cannot
run it.

The wire already carries what is needed: `pb.Op` has both `Platform` and
`WorkerConstraints`, per op. And main's driver already routes REAPI actions by
their demanded platform for buck2 -- so the fleet-side concept exists and this
is
extending it to buildkit rather than inventing it.

Three consequences:

- **A subtree's platform is the union of its vertices' constraints.** One
  linux-only vertex pins the whole subtree, the same way one `LOCALLY` excludes
  it entirely.
- **Emulation is a trap, not a fallback.** binfmt lets a linux/amd64 worker run
  linux/arm64 slowly (`tonistiigi/binfmt` is already in earthbuild's graph).
  Prefer native; treat emulated capacity as a last resort rather than as
  capacity, or the queue will happily hand arm64 work to an amd64 box and call
  it scheduled.
- **Idle mac and windows runners are not spare capacity for linux work.** A
  heterogeneous fleet's utilisation must be reported PER PLATFORM, or a fleet
  that looks 60% utilised may be 100% on linux and 0% elsewhere.

## Sequencing

Written when mechanism A was assumed. Kept because most of it is
mechanism-neutral, and marked with what is actually true now.

| | state |
| --- | ------- |
| 0. rebalance the twelve groups | not done; free latency, needs an Earthfile change |
| 1. port the dedup delta | superseded -- pinched, not merged |
| 2. timing store, banked | **built** -- ingest, banking, critical path |
| 3. published-key bloom + batch query | **built** |
| 4. `D2W::Lead { subtree, frontier }` | **built** -- mechanism A specifically |
| 5. coalesce CI to `+test-no-qemu` | not done |
| 6. report utilisation, per platform | not done |

**What comes next is not item 5.** It is characterising a real build on the
wire, because items 4 and 5 both assume mechanism A and that assumption is
now the open question rather than the plan.

## Excluded from dispatch by construction

Propagates to the WHOLE subtree -- one excluded vertex excludes its subtree,
conservatively:

- `LOCALLY` - means *this* machine, definitionally
- cache mounts - already excluded from single-flight; adoption is unsound
  (`ExecOp.hasCacheMount`, measured: a follower got a dangling symlink)
- `Secretenv` - shipping the spec ships the secret reference
- `security=insecure` - granting a peer privileged exec is a trust decision, not
  a scheduling one

## Non-goals

- **Normalisation** (a `norm.rs` analogue). buck2 injects the target label into
  an otherwise-identical command, so normalising removes contamination that was
    never semantic. A buildkit `ExecOp` has NO label -- measured twice: cross-
  shard
  sharing is 10 commands with a ceiling of 41, and every within-shard
  near-collapse is explained by `span`, which is display metadata already absent
  from the cache key. buildkit already dedups these.
- **A fourth `Executor` implementation.** Wrong seam: it is handed host mount
  paths, not content digests, so shipping from there means tarring a mounted
  rootfs and losing every bit of layer sharing.
- **A second persistence mechanism.** `bank/` exists.
- **More dedup measurement.** Its ceiling is understood.

## Invariants this cost us to learn

- **A `should_fail` test whose failure depends on something ABSENT from the
  cache key cannot be served from cache and still mean anything.** Secrets and
  credentials are deliberately outside buildkit's cache key, so a shared daemon
  turned two negative tests green. 88 `should_fail` call sites exist; two were
  reached. This class announces nothing when it breaks.
- **An instrument that counts log lines is not counting daemons.** `grep -c` on
  `running under pid=` overcounted ~8x once `--verbose` changed how much inner
  output was echoed. Sample the machine (`pgrep -c buildkitd` at peak), not the
  log.
- **A bare `Canceled` is a real failure and earthly discards its reason** unless
  `--verbose` is set. Twelve CI jobs died that way with the cause unrecoverable.
- **Blooms must lie in the safe direction, and which direction that is depends
  on the question.** For holdings, a false positive costs a confirmation. For
  cheapness, it costs local execution. Both are safe; state which one before
  reusing a bloom for a new question.
