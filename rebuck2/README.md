# rebuck2

Ad-hoc distributed Remote Execution for buck2 — N GitHub-Actions runners (or
any machines) form a throwaway RE cluster for one build, over
[iroh](https://iroh.computer) P2P. Design + roadmap:
[docs/re-engine-plan.md](../docs/re-engine-plan.md). Cache-only
[rebuck](../README.md) remains the zero-config option.

## Roles

```text
rebuck2 driver [--grpc-port 9092] [--session S] [--store DIR]
               [--min-workers N] [--no-local-exec]
rebuck2 worker [--session S] [--store DIR] [--slots N] [--connect-wait-secs N]
```

- **driver** runs beside buck2: serves the REAPI buck2 dials on localhost
  (Capabilities + CAS + ByteStream + ActionCache + Execution) from a
  digest-keyed disk store, and dispatches `Execute` to workers over iroh.
  With no workers (and local exec enabled, the default) actions run in-process
  — a one-box superset of cache-only rebuck.
- **worker** joins the mesh from anywhere, least-loaded-first claims actions,
  fetches input blobs P2P (local store as cache — shared inputs transfer
  once), executes, streams outputs back.

Rendezvous is keyless: both sides derive the driver's iroh key from
`--session` (default `$GITHUB_RUN_ID`), so runners in the same workflow run
find each other with no service and no secrets.

## buck2 wiring

`.buckconfig.local` in your project root (see `tests/e2e-local.sh`):

```ini
[build]
execution_platforms = root//platforms:re-exec

[buck2_re_client]
action_cache_address = grpc://127.0.0.1:9092
cas_address = grpc://127.0.0.1:9092
engine_address = grpc://127.0.0.1:9092
tls = false
```

`re-exec` (local_enabled=False — every action goes remote) and `re-cache`
(hybrid) platform rules live in [test/platforms](../test/platforms); copy them
into your repo as `platforms/`. Digests are SHA256 — buck2's OSS default.

## Tests

- `tests/e2e-local.sh` — driver + worker as separate processes (real iroh),
  buck2 builds `test/` with local execution disabled; asserts
  `remote: N, local: 0`.
- `.github/workflows/re-e2e.yml` — same, but the worker is a second runner:
  the compile happens cross-runner, blobs over hole-punched QUIC.

## v0 limits (by design, see the plan)

- SHA256 digests only; no compressed-blobs; `GetTree`/`SplitBlob` unimplemented.
- A dropped worker fails its in-flight actions (rescheduling is roadmap #4).
- Action env is passed through verbatim plus the worker's `PATH` when absent —
  system toolchains (rustc, cl.exe) are PATH-resolved on the uniform runner
  images.
- GH-cache seed/snapshot of the store not wired yet (roadmap #5).
