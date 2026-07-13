# rebuck2

Ad-hoc distributed Remote Execution for buck2 — N GitHub-Actions runners (or
any machines) form a throwaway RE cluster for one build, over
[iroh](https://iroh.computer) P2P. Design + roadmap:
[docs/re-engine-plan.md](../docs/re-engine-plan.md). Cache-only
[rebuck](../README.md) remains the zero-config option.

## Roles

```text
rebuck2 driver [--grpc-port 9092] [--session S] [--store DIR]
               [--min-workers N] [--no-local-exec] [--decentralized-cas]
rebuck2 worker [--session S] [--store DIR] [--slots N] [--connect-wait-secs N]
```

`--decentralized-cas`: outputs stay on the worker that built them; the driver
keeps a digest→producer index and redirects fetches (peers pull direct over
the mesh; the driver read-through-caches only what buck2 itself asks for).
Slashes driver disk/egress at sweep scale. Trade-off: a dead worker takes its
blobs with it — requeue re-runs an action but cannot resurrect a lost input,
so keep it off for small builds where the driver's disk is a non-issue.

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

## Name-independent caching (on by default)

Two targets that perform byte-identical work under different labels — the
same crate vendored twice, a snapshot universe sharing versions with the
base, two repos building the same dependency — normally miss each other's
cache: the label leaks into the action digest via output paths,
`-Cmetadata`, and path-env absolutization. The driver closes that gap with
a second, canonical key (`norm.rs`):

```text
canonical key = SHA-256( normalize(Command)
                       ∥ normalize(each @-argsfile blob, one level deep)
                       ∥ sorted source-input content hash )
```

Normalization collapses label-derived tokens (`-Cmetadata`,
`--buck-target`, `__target__/<hash>/` path segments, `lib*-<hash>.`
filename suffixes) to fixed placeholders; work-relevant tokens (crate
versions, `--target` triples, features) pass through untouched. A hit
serves the original result's blobs under the requester's declared output
paths, gated by the same blob-reachability check as the digest-keyed AC.

Correctness properties:

- a hit requires identical normalized command, identical argsfile content,
  and an identical source tree — any divergence changes the key;
- canonical-layer errors degrade to plain misses, never fail an action;
- hit-consumers ingest byte-identical dep artifacts, so dedupe propagates
  up the dependency graph one honest level per build.

The one observable behaviour change: twin labels now receive identical
symbol hashes, so linking two byte-identical twins into ONE binary fails
loudly (duplicate symbols) where label-salted metadata previously let it
slip through — a shape dependency resolvers don't produce. If you need
that, or want the old behaviour: `--no-name-independent`.

Currently probed for rustc actions (the first validated category); the
mechanism is category-agnostic and widens as categories are proven.

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
