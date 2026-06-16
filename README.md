# rebuck

Persistent **buck2 action cache** on GitHub Actions — no remote-execution
service, no fork, within the free 10 GB cache budget.

buck2 without a Remote Execution backend has no cross-run cache: a fresh CI
daemon recompiles every action even if `buck-out` survives (measured: 0% reuse).
rebuck runs a [bazel-remote](https://github.com/buchgr/bazel-remote) sidecar as
a cache-only REAPI server on `localhost`, wires buck2 to it, and persists its
blob store via `actions/cache`. Misses execute **locally** and upload; hits skip
the work entirely.

Measured on a 604-action Rust graph: cold build **137 s → 4 s** warm (100% cache
hits), cache footprint 169 MB.

## Usage

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

## Inputs

| input | default | purpose |
| ---------------------- | ----------------------- | ----------------------------------------------- |
| `bazel-remote-version` | `2.6.1`                 | sidecar release (checksums pinned per version)   |
| `max-size-gb`          | `2`                     | on-disk LRU cap; keep the cache inside 10 GB     |
| `grpc-port`            | `9092`                  | REAPI gRPC port                                  |
| `http-port`            | `8080`                  | `/status` health + stats                         |
| `cache-dir`            | `~/.cache/rebuck`       | blob store + `actions/cache` path                |
| `cache-key-prefix`     | `rebuck`                | `actions/cache` key prefix                       |
| `execution-platform`   | `root//platforms:re-cache` | the in-graph platform target                  |
| `write-buckconfig-local` | `true`                | inject buck config (false = DIY)                 |
| `force-cache-upload`   | `true`                  | export `BUCK2_TEST_FORCE_CACHE_UPLOAD=1`         |

## How it works

- **main**: restore `cache-dir` from `actions/cache` → download+checksum
  bazel-remote → start it detached → health-check `/status` → write
  `.buckconfig.local` → export the upload env.
- **post**: print stats → stop the sidecar (flush) → save `cache-dir` under a
  run-unique key (prefix restore-key gives partial hits; GHA LRU prunes).

### Why these settings (each was a real blocker)

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

### Hermeticity (for real, not stale, hits)

buck2 action digests for this style of repo are cross-runner stable (paths are
project-relative). Two things to fix:

- **Pin rustc** (`rust-toolchain.toml` / `dtolnay/rust-toolchain`). `rustc` is
  PATH-resolved and not hashed into the digest, so a runner-image bump → same
  digest, different compiler → stale hits.
- **Don't wrap rustc with sccache** when using rebuck: `["sccache","rustc"]`
  changes the digest vs `["rustc"]`, and the action cache already covers
  compile + link (a superset of sccache).

## Not a fork

A buck2 fork could add an in-process disk cache, but it needs a trait over four
concrete `re_client` call sites (`ActionCacheChecker`, `CacheUploader`,
`CasDownloader`, the deferred materializer's `ReConnectionManager`) — ~600–900
LOC rebased against fast-moving upstream. A bazel-remote sidecar satisfies all
four at once with zero fork. rebuck is that.

## License

MIT
