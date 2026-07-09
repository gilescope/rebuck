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
