# Dispatch - the build plan

What to build, in what order, and what proves each step. Design rationale is in
[dispatch-plan.md](dispatch-plan.md); product claims are in
[dist-buildkit-principles.md](dist-buildkit-principles.md).

## The numbers this plan is built on

Per-group wall-clock, each measured COLD in its own rig invocation, because
`SOLO=1` only restarts daemons in the pair phase -- so a sweep of N targets
leaves the daemon warm for targets 2..N and every number after the first
measures something else. Both columns are the same twelve groups.

| group | cold | warm |    delta |
| ----- | ---: | ---: | -------: |
| 1     | 321s | 361s |          |
| 3     | 210s | 226s |          |
| 4     | 435s | 312s |          |
| 6     | 189s |  91s |  **+98** |
| 7     | 369s | 227s |     +142 |
| 8     | 195s |  94s | **+101** |
| 9     | 414s | 300s |          |
| 10    | 203s |  90s | **+113** |
| 11    | 222s | 110s |          |
| 2, 5  | FAIL | 142s |          |

The three bolded rows are the finding: groups whose own work is ~90s cost ~190s
cold. **The delta is the shared stem, ~100s, and every group pays it.** That
matches the independent measurement of the stem at 32 vertices / 94s.

So of roughly 3,590 runner-seconds for a twelve-group run, about **1,176s (33%)
is the same stem built twelve times.**

## Which lever buys what

Let `V` = total variable work (~2,414s), `S` = stem (~98s), `N` = runners.

| | makespan | runner-seconds |
| ------------------ | -------------------- | -------------- |
| today | ~550s (the worst group) | 3,590 |
| perfect rebalance | `S + V/12` = **~300s** | 3,590 (unchanged) |
| coalesce + dispatch | `S + V/N` = **~300s** at N=12 | **2,512** |
| both | ~300s | 2,512 |

**Rebalancing and coalescing produce the SAME makespan.** Twelve groups each
building the stem do it in parallel, and parallel duplicate work is free in
wall-clock -- principle 2, again, from a new direction.

They differ in what else they buy:

- **Rebalancing is a latency fix**: 550s -> 300s, free, no new mechanism.
- **Coalescing is a COST fix**: 3,590 -> 2,512 runner-seconds, a third of the
  compute, and it only scales past 12 workers.

Neither subsumes the other, and rebalancing is not a stepping stone to
coalescing -- it is worth doing on its own and worth undoing later.

## Milestones

### M0 - trustworthy per-group costs (in flight)

Twelve cold runs, one rig invocation each. Nine landed; groups 2 and 5 FAIL cold
having passed warm, which is a finding rather than noise and blocks nothing else.

- **Done when**: twelve cold numbers, and the two cold-only failures explained.
- **Note**: `copy-tilde-test` fails cold on `output_does_not_contain` -- another
  member of the class where a warm cache changes what a test means.

### M1 - rebalance the twelve groups

Bin-pack tests into twelve groups of equal expected cost, using M0's group totals
and the per-target proxy ONLY to split within large groups (it correlates
r=0.734 overall and is 10x wrong on groups with few targets, so it is a splitter
of last resort, not a cost model).

- **Done when**: cold makespan drops from ~550s toward ~300s, measured the same
  way as M0.
- **Cost**: an Earthfile change and one measurement round. No new mechanism.
- **The packer is built** -- `rebuck2 bank timings plan <table> <bins> <target>...`,
  longest-processing-time-first, deterministic across machines so two runners
  derive the same plan rather than disagreeing about which group each is
  building. A target with no samples costs the MEDIAN of those that have them,
  and the plan reports how many were placed that way, because a plan that is
  mostly guesses should be read as one. What M1 now waits on is not the
  mechanism but the SAMPLES -- see M2.

### M2 - the timing store

Record per-vertex `Started`/`Completed` from buildkit's status stream (already
streamed -- `client/graph.go`; no fork change), keyed by target + the build args
that reach it, banked via `bank/`'s existing key/value machinery.

**Keyed COARSELY on purpose** (principle 13). Not on the cache key, not on
content: a content-keyed table is perfect and useless, because every commit
empties it. `+deps` takes about as long as it did last week whether or not a
source file moved. We are choosing robustness over precision, and the estimate
only ever feeds decisions where being wrong is cheap.

- **Also record STABILITY**, not only duration: how many consecutive builds
  produced the same output digest for this key. That is what decides admission
  to the bank (principle 14) -- and it wants the same coarse key, because
  content-keyed stability is a contradiction: the key changes exactly when the
  content does.
- **Done when**: a second run can answer "how long does `+deps` take with these
  args" with a median and a p90, and "has `+deps` changed in the last three
  builds".
- **Why before M3-M5**: rebalancing, longest-first scheduling, the cheap-bloom
  and dispatch selection currently guess separately. This is the one instrument
  all four need, and M1 is presently blocked on not having it.

**Built** (`rebuck2/src/bank/timings.rs`): the coarse key, both statistics,
tenure, the bin-packer, and banking across runs. `bank timings record | stats |
plan | prune | tenured | merge | restore | publish`.

Three decisions worth carrying, because each was a fork in the road:

- **The banked unit is an OBSERVATION, not an aggregate** -- one row per
  (key, run), with medians, p90s and stability computed at read time. That is
  what lets a delta replay the way `bank/dice.rs`'s does: idempotent,
  order-independent, first writer wins. Two runners adding to the same mean is
  not order-independent, and the tidier-looking design is the broken one.
- **The whole table travels, not a delta.** A few thousand lines after pruning
  to eight samples a key, so one role's artifact bootstraps a cold machine.
  `dice.rs` deltas because it is millions of rows.
- **Fail open at every step**: an unreadable table, an artifact that will not
  download, a digest that is not a digest -- each costs an estimate and never a
  build. An estimate may only feed decisions where being wrong is cheap
  (principle 13), and that has to include being absent.

**The ingest needs no fork, and no earthbuild change either.** This plan said
to record per-vertex `Started`/`Completed` from buildkit's status stream. It
turns out `earthly --logstream-debug-file=X` already writes protojson deltas
whose `TargetManifest` carries `canonicalName`, `overrideArgs`, both stamps and
`dependsOn` -- and `overrideArgs` is already the `k=v` form the coarse key
takes. Per-target is also COARSER than per-vertex, which is what principle 13
wanted in the first place. `bank timings ingest <table> <run> <log>`.

**Spans NEST, and it would have poisoned every estimate.** Measured on a real
three-target build: `+test` 2995ms *contains* `+build` 2945ms *contains*
`+deps` 469ms, so the spans of a 2995s build sum to 6409ms. A bin-packer fed
those believes three targets' work where there is one target's. Samples carry
SELF time -- span less what its dependencies were occupying. This was found by
capturing a real run rather than reasoning about the format, which is the same
lesson as "coordinate the seam that FIRES" (principle 8): one run settled what
inference had wrong.

**Stability comes from CACHEDNESS**, since no output digest is reported. A
target whose EXEC commands were all served from cache did not change, so an
identity is synthesised from that: cached keeps the one it had, uncached gets a
fresh one, and the existing stability count works unchanged.

Not "all commands cached" -- measured, the structural ones (`FROM +base`,
`SAVE ARTIFACT`) report uncached on an identical rerun while the `RUN` beside
them reports cached. That predicate is never true, and a predicate that is
never true is not a signal.

Tested in all three directions rather than argued:

| case | reports | truth | cost |
| --------------------- | -------- | --------- | ------------- |
| unchanged rerun | cached | unchanged | correct |
| source edited | uncached | changed | correct |
| unchanged, COLD cache | uncached | unchanged | a lost tenure |

The third row is the fresh-runner case, and it UNDER-tenures: we fail to bank
something we could have. The dangerous direction -- claiming unchanged when it
moved -- needs buildkit to report a cache hit on different inputs, which is
principle 7's determinism bound and is already accepted everywhere else here.
**The proxy lies only in the safe direction**, which is the blooms rule applied
to a new question.

So M2's done-when is met, on real logs: three laps with two untouched tenures
`+deps`; one edit resets stability to 1 and withdraws it.

Wiring into the bank actions stays deliberately undone until a real CI lap has
produced a table worth banking -- the dependency runs ingest -> samples ->
wiring, not the reverse.

### M2.5 - port the dedup delta onto trunk (measured, and it is the blocker)

M3 and M4 both extend `rebuck2/src/lease.rs`, which exists only on
`giles-single-buildkit-with-dist`. That branch forked at `423e21a` (2026-07-12)
and has 74 commits since -- but it never took what TRUNK gained in the same
period, which includes the entire `bank/` module. So the three lines have three
different module sets:

| | has |
| ------------------- | ---------------------------------- |
| `origin/main` | `bank/` |
| `giles-dispatch` | `bank/` + the timing store |
| dedup branch | `lease.rs`, `registry.rs`, `payload/` |

Merged in a scratch worktree to size it rather than guess:

- **24 conflict hunks in 4 files** (`driver.rs` 10, `worker.rs` 5, `main.rs` 4,
  `Cargo.lock` 5) between the dedup branch and TRUNK -- with none of the
  dispatch work involved. This debt is pre-existing and grows with every
  commit to either line.
- **The dispatch work adds ONE hunk** to that, in this file. The timing store
  is new files, and new files carry cleanly exactly as the plan predicted.
- The conflicts are not union-able: the dedup branch MOVED code (e.g.
  `result_digests` from `driver.rs` to `payload/reapi.rs`), so a side that
  looks deleted is relocated, and a naive union duplicates it.
- **`Cargo.toml` auto-merges into an INVALID file** -- no conflict marker, two
  `reqwest` keys, one 0.12 and one 0.13. `cargo metadata` catches it; a merge
  that only compiles the resolved conflicts does not. Resolution is 0.13 with
  `stream` unioned in, which the mirror needs.

So the plan's step 1 is its own PR against trunk, and it should happen before
M3 rather than alongside it. Sequencing it after M2 rather than before cost
nothing: the timing store never needed the lease.

### M3 - batched, mostly-local coordination

A gossiped bloom of published lease keys, plus a non-blocking batch query;
blocking `claim` only where we intend to follow.

- **Done when**: a cold pair run shows the same `merged` count with an order of
  magnitude fewer lease round trips (today: `led=828`, one per vertex).
- **Risk**: a stale bloom loses sharing rather than breaking it. Size and gossip
  cadence deliberately; do not inherit the blob-bloom's parameters.

### M4 - `D2W::Lead { subtree, frontier }`

The only genuinely new protocol. A peer is handed a subtree's spec and the
descriptor chains of its frontier, builds it, and publishes -- the requester's
existing `adoptLeaderResult` path is unchanged.

- **Unit is a target** (principle 10). Exclusions propagate upward: one
  `LOCALLY`, cache mount, secret or privileged exec anywhere excludes the whole
  subtree. Platform is the union of the subtree's constraints.
- **Principle 6 rules out the obvious implementation**: the frontier and the
  result must travel peer-to-peer. The driver arbitrates and carries nothing.
- **The driver OFFERS; it does not assign** (principle 12). A worker must be
  able to refuse, because refusal is the backpressure -- and a protocol where
  the coordinator assigns cannot express it and would have to rediscover it as a
  load metric, later and worse.
- **Worker-to-worker beats driver-dispatched.** A subdivided branch has a
  machine blocked on it; new driver work does not. Without this, subdivision is
  a REGRESSION: workers sit on warm state waiting for peers who took fresh work
  instead.
- **Done when**: a single earthly build whose vertices demonstrably executed on
  machines that did not invoke it, and the driver's disk stays flat.

### M5 - coalesce CI to one build

`+test-no-qemu` already BUILDs all twelve groups; no repo reorganisation. One
driver job owning the invocation, N worker jobs lending CPU, via main's existing
`rebuck2/actions` choreography.

- **Done when**: runner-seconds drop toward `S + V`, i.e. ~2,512 against today's
  ~3,590, with makespan no worse.
- **Report utilisation PER PLATFORM**: a 60%-utilised heterogeneous fleet may be
  100% on linux and 0% elsewhere.

## Why the critical path is not the binding constraint (but measure it anyway)

The fleet is expected to be WORK-bound, not depth-bound: the suite's total work
is far larger than the fleet's capacity, so makespan is set by throughput and
batch efficiency rather than by the longest chain. That is the case for
dispatch, and it is why per-vertex handover (principle 10) and per-vertex
coordination were both the wrong granularity.

It is still worth computing the critical path ONCE, from the graph plus M2's
timings, because it answers a question nothing else does: **the N at which
adding workers stops paying.** Below that N the fleet is work-bound and batch
efficiency dominates; above it, we are buying runners to wait on a chain.

Also if we know the critical path, then that helps make sure that we get that priority
scheduled.

**Both are now one command**: `bank timings critical <logstream-file>` prints
the chain and the saturation N. It weights by SELF time, because a parent that
only waits on its child is not on the critical path in any sense that matters,
and ties break on the names so two machines derive the SAME chain -- a
prioritisation the fleet disagrees about is worse than none.

On the sample build it reports a pure chain: 2995ms of 2995ms work on the
critical path, saturating at ONE runner. That is the shape worth watching for
in the real suite -- where a group is a chain, no amount of fleet helps it, and
the lever is subdividing the chain (M4) rather than adding runners.

## Bank the stem first, and compare against it

Before M4/M5, the honest competitor to dispatch is: **bank the stem and keep
twelve shards.** If the stem is stable across builds -- and by inspection it
should be, it is the toolchain and the base images -- principle 14 tenures it,
run two restores it, and the 1,176s of duplicated stem largely evaporates
WITHOUT any dispatch at all.

That would capture most of the compute saving for a fraction of the work. M2's
stability statistic answers whether it holds; if it does, M4 and M5 must justify
themselves on what is left, which is a much harder bar and the right one.

**That question is now ASKABLE**, which it was not when this was written: run
the suite three times with `bank timings ingest`, then `bank timings tenured`.
If the stem's targets tenure and the leaves do not, the case for banking over
dispatching is made in one command, on data, before a line of M4 is written.
Note the fresh-runner caveat above -- a cold cache under-reports tenure, so the
three laps want the bank warm, or the answer is pessimistic rather than wrong.

Corollary from the same principle: do not bank what changes every commit. The
upload is paid, the hit rate is zero, and the eviction is paid again.

## Open, and honest about it

- **How deep to subdivide, and where.** Principles 11 and 12 answer the "a
  target is too big" objection: cut at the narrowest DECLARED seam -- a chain
  rooted at `FROM <registry image>` needs nothing from us at all, a
  `COPY +t/artifact` boundary crosses one artifact, and `BUILD +t` crosses a
  whole snapshot -- and prioritise worker-to-worker work so a subdividing worker
  is never left blocked on a peer that took fresh driver work instead. What is
  NOT settled is when to STOP: one level deeper is always available, and past
  some depth the frontier costs more than the work. Both seam width and work are
  measurable, so M2's timing store answers it rather than a constant -- and
  because that store is coarse by design (principle 13) it keeps answering
  across commits instead of emptying on every source change. The first build of
  anything has no statistics: fall back to not subdividing, and let run two be
  informed.
- **Two groups fail cold and pass warm.** Same class as the `secrets` and
  `aws-flag` false passes: a warm cache changes what a test means. Expect more
  of these as coalescing makes everything warm.
- **`V` is an estimate.** It is `cold - 98s` for nine groups and the warm number
  for the other three. M2 replaces the estimate with data.
