# Dispatch - the build plan

What to build, in what order, and what proves each step. Design rationale is in
[dispatch-plan.md](dispatch-plan.md); product claims are in
[dist-buildkit-principles.md](dist-buildkit-principles.md).

## The numbers this plan is built on

Per-group wall-clock, each measured COLD in its own rig invocation, because
`SOLO=1` only restarts daemons in the pair phase -- so a sweep of N targets
leaves the daemon warm for targets 2..N and every number after the first
measures something else. Both columns are the same twelve groups.

| group | cold | warm | delta |
| ----- | ---: | ---: | ----: |
| 1     | 321s | 361s |       |
| 3     | 210s | 226s |       |
| 4     | 435s | 312s |       |
| 6     | 189s |  91s | **+98** |
| 7     | 369s | 227s | +142  |
| 8     | 195s |  94s | **+101** |
| 9     | 414s | 300s |       |
| 10    | 203s |  90s | **+113** |
| 11    | 222s | 110s |       |
| 2, 5  | FAIL | 142s |       |

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

### M2 - the timing store

Record per-vertex `Started`/`Completed` from buildkit's status stream (already
streamed -- `client/graph.go`; no fork change), keyed by target + the build args
that reach it, banked via `bank/`'s existing key/value machinery.

**Keyed COARSELY on purpose** (principle 13). Not on the cache key, not on
content: a content-keyed table is perfect and useless, because every commit
empties it. `+deps` takes about as long as it did last week whether or not a
source file moved. We are choosing robustness over precision, and the estimate
only ever feeds decisions where being wrong is cheap.

- **Done when**: a second run can answer "how long does `+deps` take with these
  args" with a median and a p90.
- **Why before M3-M5**: rebalancing, longest-first scheduling, the cheap-bloom
  and dispatch selection currently guess separately. This is the one instrument
  all four need, and M1 is presently blocked on not having it.

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
