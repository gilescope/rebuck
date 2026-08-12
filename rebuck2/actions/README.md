# rebuck2 composite actions

Reusable choreography for a distributed buck2 build on GitHub Actions:
one **driver** job owning the buck2 invocation, N **worker** jobs lending
CPU over the iroh mesh. The actions carry all the driver/worker plumbing
so a workflow only declares fleet shape and build targets.

| action          | role                                                  |
| --------------- | ----------------------------------------------------- |
| `driver`        | Install, seed store, start, addr, quorum, buckconfig  |
| `driver-finish` | Finalize shards, re-snapshot, stats, logs, stop       |
| `worker`        | Install, restore shard, serve, publish shard          |
| `setup`         | Standalone `rebuck2` install (driver/worker embed it) |
| `buck2`         | Standalone pinned buck2 install (driver embeds it)    |
| `runtime-env`   | Export ACTIONS_RUNTIME_TOKEN + RESULTS_URL to the job |
| `bank-restore`  | Seed the store from the CI bank (AC rows, CAS range)  |
| `bank-publish`  | Bank + upload this node's new blobs, rows and spill   |

A distributed build is two `uses:` lines: `driver` and `worker` install
buck2 + the engine themselves. Pin everything to the **same full sha**:
the engine install defaults to `github.action_ref`, so one sha pins
engine + choreography together and the warm binary cache stays honest.

## Example

```yaml
jobs:
  driver:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      actions: write   # store snapshot save
    steps:
      - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4
      # pinned rustc (dtolnay/rust-toolchain master)
      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9
        with: { toolchain: "1.92.0" }
      - uses: gilescope/rebuck/rebuck2/actions/driver@<sha>
        with:
          session: mesh-${{ github.run_id }}
          buck2-version: "2026-06-01"
          min-workers: "1"          # quorum BELOW fleet size: one no-show
          co-worker-slots: "3"      # must not scrap the run
          execution-platforms: root//platforms:re-exec
      - run: buck2 build //...
      - uses: gilescope/rebuck/rebuck2/actions/driver-finish@<sha>
        if: always()

  worker:
    continue-on-error: true   # the driver job is the verdict
    strategy:
      fail-fast: false
      matrix: { n: [1, 2, 3] }
    runs-on: ubuntu-latest
    permissions:
      contents: read
      actions: read   # driver-addr artifact fetch
    steps:
      # pinned rustc (dtolnay/rust-toolchain master)
      - uses: dtolnay/rust-toolchain@3c5f7ea28cd621ae0bf5283f0e981fb97b8a7af9
        with: { toolchain: "1.92.0" }
      - uses: gilescope/rebuck/rebuck2/actions/worker@<sha>
        with:
          session: mesh-${{ github.run_id }}
```

## Sharded CAS persistence (big trees)

For multi-GB stores the monolith snapshot doesn't fit the actions cache.
Instead: the driver snapshots the **AC only** and each worker carries a
**CAS shard** as a run artifact (artifacts don't LRU-evict; a 10GB cache
evicted whole shard sets within an hour):

```yaml
      # driver
      - uses: gilescope/rebuck/rebuck2/actions/driver@<sha>
        with:
          session: mesh-${{ github.run_id }}
          min-workers: "2"
          no-local-exec: "true"
          finalize: "true"                    # assign shards at teardown
          snapshot-key-prefix: rebuck2-ac-
          snapshot-subdirs: ac

      # worker <n> preloads shard <n-1> and publishes whichever shard the
      # driver assigns it at finalize. Timing defaults suit long sweeps;
      # short-lap fleets are clipped by the job's timeout-minutes anyway.
      - uses: gilescope/rebuck/rebuck2/actions/worker@<sha>
        with:
          session: mesh-${{ github.run_id }}
          shard: ${{ matrix.shard }}
```

Shard artifact names (`cas-shard-N`) are content-addressed slices, so
separate fleets sharing a repo (e.g. a single-OS one and a mixed-OS one)
cross-pollinate through them, while their ACs stay per-fleet via
`snapshot-key-prefix`.

## Autoscaled workers (optional)

Static worker jobs idle on warm laps (everything is an AC hit). Instead
of a fixed matrix, drop the worker jobs and let the driver summon them:
the driver's heartbeat logs `pending_jobs=N`; a background loop next to
the build greps it and `gh workflow run`s a workers workflow with a
count sized to the queue. Warm laps never spawn a worker (give the
driver `co-worker-slots` for the trickle); cold laps summon the fleet
within a heartbeat. Summoned workers run in their own workflow run, so
they pass `addr-run-id: <the driver's run id>` to find the addr
artifact. The dispatching job needs `actions: write`.

## Driver-side CAS shards (no-fleet or small-fleet lanes)

The monolith snapshot is all-or-nothing: one LRU eviction or era bump
and the lane is stone cold, even though the underlying model (per-action
AC, per-blob CAS) is perfectly incremental. `cas-shard-artifacts: "true"`
on `driver` makes the transport match the model: the CAS persists as 4
digest-range shard artifacts (artifacts do not LRU-evict and are not
branch-scoped), each shard skips its re-upload when unchanged, and
losing one costs a quarter of the warmth instead of all of it. Pair with
`snapshot-subdirs: "ac"` - the AC stays a small cache entry. Fleets with
long-lived workers can instead use `finalize:` + the worker action's
`shard:` preload (workers pack shards in parallel at teardown).

## Notes

- **Rendezvous** is keyless: pass the same `session` to driver and
  workers (derive it from `github.run_id`). The addr artifact lets
  workers dial the relay directly; n0 discovery is only a fallback.
- **Store location**: both `driver` and `worker` honour a mounted
  `$DEV_DRIVE` (windows ReFS Dev Drive) automatically; otherwise
  `~/.cache/rebuck2`. Override with `store:`.
- **Pinned rustc**: PATH-resolved toolchains are not part of action
  digests - warm cache hits are only honest if every run compiles with
  the same compiler. Pin the toolchain in the calling workflow.
- `driver-finish` reads state from `$GITHUB_ENV` exported by `driver`;
  they must run in the same job.

## The bank

Cache persistence is `rebuck2 bank` plus one action, not a directory of
shell. A consumer's restore is:

```yaml
      - uses: gilescope/rebuck/rebuck2/actions/bank-restore@<sha>
        with:
          role: ${{ runner.os }}-w${{ matrix.n }}
          mode: own          # 'all' on the node that reads the AC
          shard: ${{ matrix.owns }}
```

The same restore also brings back the coarse per-target timing table and
reports its path as `timings-table`. Feed the build's own log stream into
it and `bank-publish` banks it:

```yaml
      - id: bank
        uses: gilescope/rebuck/rebuck2/actions/bank-restore@<sha>
        with: { role: driver, mode: all }
      - run: earthly --logstream-debug-file=ls.json +test-no-qemu
      - run: rebuck2 bank timings ingest "$TABLE" "$GITHUB_RUN_NUMBER" ls.json
        env:
          TABLE: ${{ steps.bank.outputs.timings-table }}
```

Nothing about the timing table is load-bearing, and the wiring says so:
a cold or unreadable table logs a line and the lap proceeds. It does NOT
set `cold`, which gates real bank behaviour, and it never fails the job -
a build with no estimates schedules worse and produces the same bytes.

The store is hash-verified before the build sees it: a CAS filename IS
the sha256 of its content, and artifacts cross GitHub's branch-scoping
wall - any run, any branch, including fork PRs, shares one namespace.
That degrades a planted blob to a cold cache rather than code execution,
and it is why the container artifact's own provenance does not have to
be trusted. Set `verify: false` only when the hashing cost outweighs a
source you already trust.

`lineage` defaults to the branch (on a PR, the source branch) and
`parent-lineage` to the PR base, so a branch inherits the trunk's bank
read-only: its rows seed the store and join the diff base, while every
publish still goes to the branch's own manifest. A cold bank sets the
`cold` output rather than failing - a first lap on a new lineage is a
normal outcome.

The publish half is one `uses:` because the uploads belong to it:

```yaml
      - uses: gilescope/rebuck/rebuck2/actions/bank-publish@<sha>
        if: always()
        with:
          role: ${{ runner.os }}-w${{ matrix.n }}
          shard: ${{ matrix.owns }}
```

That single step banks the owned range, banks the AC rows this node
authored, spills the rest, and uploads all five artifacts in the order
that makes them self-verifying: each manifest upload is gated on its
container upload succeeding, so a manifest can never reference a
container that did not land. A death anywhere leaves the previous
generation as HEAD - stale by one lap, self-healing, and no other range
affected.

Store paths are resolved inside the action, per OS. That is deliberate:
the shell this replaces needed `cygpath` on windows, a
`timeout`/`gtimeout` fork on macOS, and `tr -d '\r'` after every
`jq.exe` call. A windows driver mostly just works now, and the caller
never learns why it wouldn't have.
