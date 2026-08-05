# rebuck

Make buck2 fast on GitHub Actions. buck2 without a Remote Execution backend has
no cross-run cache: a fresh CI daemon recompiles every action even if `buck-out`
survives (measured: 0% reuse).

This repo holds **two** answers to that, and they are separate products. Pick
one:

| | **rebuck** | **rebuck2** |
| --- | ------------------------------ | ----------------------------- |
| what | a cache | a cache **and** a build fleet |
| shape | one runner, sidecar on `localhost` | one driver job + N worker jobs |
| gets you | skip work you did before | skip it, and spread the rest |
| needs | one `uses:` line | a driver job and a worker matrix |
| persistence | `actions/cache` | the bank (artifacts, no LRU eviction) |
| start here | [Usage](#usage), below | [`rebuck2/`](rebuck2/README.md) |

**Use rebuck** if one runner can build your tree in an acceptable time and you
only want to stop repeating yourself. It is one line in a workflow and there is
nothing to operate.

**Use rebuck2** if a single runner is the bottleneck - a cold build that takes
hours, or a tree big enough that the 10 GB `actions/cache` budget stopped being
enough. It costs you a fleet to describe and a mesh to reason about.

They do not compose: rebuck2 is its own REAPI engine, so it replaces the
bazel-remote sidecar rather than sitting beside it.

---

## rebuck: persistent action cache

Persistent **buck2 action cache** on GitHub Actions — no remote-execution
service, no fork, within the free 10 GB cache budget.

rebuck runs a [bazel-remote](https://github.com/buchgr/bazel-remote) sidecar as
a cache-only REAPI server on `localhost`, wires buck2 to it, and persists its
blob store via `actions/cache`. Misses execute **locally** and upload; hits skip
the work entirely.

Measured on a 604-action Rust graph: cold build **137 s → 4 s** warm (100% cache
hits), cache footprint 169 MB.

### Usage

One-time, add an execution platform to your repo (buck2 needs the
`remote_cache_enabled` platform in-graph — it can't come from the action). Copy
[`examples/platforms/`](examples/platforms) to `platforms/` in your repo, and
add `.buckconfig.local` to `.gitignore`.

Then in your workflow, before any `buck` step:

```yaml
- uses: gilescope/install-buck2@latest
- uses: dtolnay/rust-toolchain@stable      # pin rustc — see Hermeticity
- uses: gilescope/rebuck@v1
- run: buck2 build //...
```

No buck-invocation changes needed: rebuck writes `.buckconfig.local`
(`execution_platforms` + `[buck2_re_client]`) and exports
`BUCK2_TEST_FORCE_CACHE_UPLOAD=1`. See [`examples/workflow.yml`](examples/workflow.yml).

### Inputs

| input | default | purpose |
| ---------------------- | ----------------------- | ----------------------------------------------- |
| `bazel-remote-version` | `2.6.1`                 | sidecar release (checksums pinned per version)   |
| `max-size-gb`          | `2`                     | on-disk LRU cap; keep the cache inside 10 GB     |
| `grpc-port`            | `9092`                  | REAPI gRPC port                                  |
| `http-port`            | `8080`                  | `/status` health + stats                         |
| `cache-dir`            | `~/.cache/rebuck` *(empty = this)* | blob store + `actions/cache` path     |
| `cache-key-prefix`     | `rebuck`                | `actions/cache` key prefix                       |
| `execution-platform`   | `root//platforms:re-cache` | the in-graph platform target                  |
| `write-buckconfig-local` | `true`                | inject buck config (false = DIY)                 |
| `buck-root`            | workspace root          | project root holding `.buckconfig`; the local one is written here |
| `force-cache-upload`   | `true`                  | export `BUCK2_TEST_FORCE_CACHE_UPLOAD=1`         |

One output: `grpc-address`, the `grpc://` address buck2 was pointed at — handy
if you set `write-buckconfig-local: false` and wire buck2 yourself.

### How it works

- **main**: restore `cache-dir` from `actions/cache` → download+checksum
  bazel-remote → start it detached → health-check `/status` → write
  `.buckconfig.local` → export the upload env.
- **post**: print stats → stop the sidecar (flush) → save `cache-dir` under a
  run-unique key (prefix restore-key gives partial hits; GHA LRU prunes).

#### Why these settings (each was a real blocker)

- **`execution_platforms` must be in a config file, not `--config`.** Passing it
  via `--config` is parser-scoped and never reaches action execution — the build
  silently uses the local-only default platform. rebuck writes `.buckconfig.local`.
- **`capabilities = false`.** The OSS gRPC client otherwise errors
  `Capabilities client: No address` against a cache-only server.
- **`BUCK2_TEST_FORCE_CACHE_UPLOAD=1`.** The prelude marks rustc compile actions
  `allow_cache_upload=False`; without the override their outputs (the bulk of the
  build) are never uploaded.
- **`remote_enabled=True` + cache-only server.** Needed to activate the
  `ActionCacheChecker`; remote *execution* attempts fall back to local because
  bazel-remote serves no Execution service.

#### Hermeticity (for real, not stale, hits)

buck2 action digests for this style of repo are cross-runner stable (paths are
project-relative). Two things to fix:

- **Pin rustc** (`rust-toolchain.toml` / `dtolnay/rust-toolchain`). `rustc` is
  PATH-resolved and not hashed into the digest, so a runner-image bump → same
  digest, different compiler → stale hits.
- **Don't wrap rustc with sccache** when using rebuck: `["sccache","rustc"]`
  changes the digest vs `["rustc"]`, and the action cache already covers
  compile + link (a superset of sccache).

### Not a fork

A buck2 fork could add an in-process disk cache, but it needs a trait over four
concrete `re_client` call sites (`ActionCacheChecker`, `CacheUploader`,
`CasDownloader`, the deferred materializer's `ReConnectionManager`) — ~600–900
LOC rebased against fast-moving upstream. A bazel-remote sidecar satisfies all
four at once with zero fork. rebuck is that.

---

## rebuck2: distributed builds

A cache-only sidecar still executes every miss on one runner. rebuck2 is a
REAPI engine that also *executes*: one **driver** job owns the buck2
invocation, N **worker** jobs lend CPU over an [iroh](https://iroh.computer)
mesh, and cache state persists as artifacts rather than `actions/cache`.

Full documentation: [`rebuck2/README.md`](rebuck2/README.md) for the engine,
[`rebuck2/actions/README.md`](rebuck2/actions/README.md) for the actions.

### Which action goes where

Every action lives under `gilescope/rebuck/rebuck2/actions/`. **Pin them all to
the same full 40-character sha** — the engine install defaults to
`github.action_ref`, so one sha pins the engine and the choreography together.
Two pins is the bug this design exists to avoid.

| action | job | when |
| --------------- | ------------ | -------------------------------- |
| `driver` | driver, first | always - installs buck2 + engine, seeds the store, waits for quorum, writes the buckconfig |
| `driver-finish` | driver, last (`if: always()`) | always - finalize, stats, logs, stop |
| `worker` | each worker | always - installs, serves, publishes its shard |
| `bank-restore` | both, before the build | you want warm laps; `mode: own` on workers, `all` on the node that reads the AC |
| `bank-publish` | both, last (`if: always()`) | pairs with `bank-restore` - banks and uploads what this node authored |
| `runtime-env` | any job needing artifact APIs | exports `ACTIONS_RUNTIME_TOKEN` + `RESULTS_URL` |
| `setup` | anywhere | you want the `rebuck2` binary alone; `driver`/`worker` already embed it |
| `buck2` | anywhere | you want a pinned buck2 alone; `driver` already embeds it |

The shortest useful fleet is two `uses:` lines - `driver` in one job, `worker`
in a matrix job - plus the same `session` string in both, derived from
`github.run_id`. Rendezvous is keyless: matching `session` is the whole
handshake.

```yaml
jobs:
  driver:
    runs-on: ubuntu-latest
    permissions: { contents: read, actions: write }
    steps:
      - uses: actions/checkout@<sha>
      - uses: dtolnay/rust-toolchain@<sha>
        with: { toolchain: "1.92.0" }        # pin rustc - see Hermeticity
      - uses: gilescope/rebuck/rebuck2/actions/driver@<sha>
        with:
          session: mesh-${{ github.run_id }}
          min-workers: "1"
      - run: buck2 build //...
      - uses: gilescope/rebuck/rebuck2/actions/driver-finish@<sha>
        if: always()

  worker:
    continue-on-error: true                  # the driver job is the verdict
    strategy:
      fail-fast: false
      matrix: { n: [1, 2, 3] }
    runs-on: ubuntu-latest
    permissions: { contents: read, actions: read }
    steps:
      - uses: dtolnay/rust-toolchain@<sha>
        with: { toolchain: "1.92.0" }
      - uses: gilescope/rebuck/rebuck2/actions/worker@<sha>
        with:
          session: mesh-${{ github.run_id }}
```

Add `bank-restore` / `bank-publish` to both jobs once that works — they are what
make the *next* lap warm, and they are the part worth reading
[the actions README](rebuck2/actions/README.md) for.

The Hermeticity note above applies here too, and harder: a fleet compiles on
several runners at once, so an unpinned toolchain means several compilers.

## License

MIT
