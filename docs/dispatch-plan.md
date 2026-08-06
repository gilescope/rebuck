# Dispatch - distributing one buildkit build, not deduplicating many

Successor to [buildkit-plan.md](buildkit-plan.md). That plan's phases (P1-P4b)
are **deduplication**: N independent builds that avoid redoing each other's
work. They are built and measured; see
[dist-buildkit-handover.md](dist-buildkit-handover.md).

This plan is the alternative, not the continuation.

## Why dedup could not win the thing we kept asking it for

**On independent runners with spare capacity, duplicated work is free in
wall-clock.** Twelve runners each building the 94s stem *in parallel* costs 94s.
Twelve runners coordinating means one builds it and eleven BLOCK, then proceed:
the same wall-clock at best, worse by `T_xfer` at the margin.

So dedup buys cost and rate limits, and cannot buy latency. Measured, repeatedly:
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
their demanded platform for buck2 -- so the fleet-side concept exists and this is
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

| | why now |
| --- | ------- |
| 0. **rebalance the twelve groups** | free, needs nothing; group4 is ~100x group11 and SETS makespan today |
| 1. port the dedup delta onto main | new files carry cleanly; ~1590 lines of hooks |
| 2. **timing store**, banked | rebalancing, scheduling, the bloom and dispatch all guess without it |
| 3. published-key bloom + batch query | kills per-vertex hops we already pay |
| 4. `D2W::Lead { subtree, frontier }` | the only genuinely new protocol |
| 5. coalesce CI to `+test-no-qemu` | one driver, N workers, existing actions |
| 6. report **utilisation, per platform** | a 60%-utilised fleet may be 100% linux and 0% elsewhere |

Step 0 is first on purpose: fixing the imbalance makes every later fleet number
honest instead of flattering. Step 2 before 3 and 4 because nothing currently
times a vertex, and a threshold picked without data is a guess wearing a number.

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
  never semantic. A buildkit `ExecOp` has NO label -- measured twice: cross-shard
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
