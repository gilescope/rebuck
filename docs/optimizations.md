<!-- markdownlint-disable MD013 -->
# rebuck2 optimizations

Why each change exists. rebuck2 is a distributed buck2 remote-execution
engine over an iroh QUIC mesh: one **driver** owns the buck2 invocation and
serves REAPI on localhost; N **worker** jobs lend CPU. The hetero CI sweep
runs three concurrent buck2 legs (win/linux/mac) against one driver.

Headline result: the hetero sweep went **62 min → 14 min** with all three
legs individually **under 10 min** (mac 6m40s). The work splits into
*correctness* (make the cache incapable of lying), *distribution* (move work
and data efficiently), *persistence* (keep warmth across CI runs), and
*measurement* (prove gains without a cloud lap).

The guiding discovery: a warm build's buck2 critical path is **~2 seconds** —
the minutes are almost entirely distributed-system overhead (round-trip
latency, validation, banking), not compilation. Every lever below attacks
overhead, not compute.

## 1. Correctness — a cache that cannot lie

A distributed cache is only fast if hits are honest. An unservable "hit"
detonates later as a missing-blob storm, which is slower than a miss.

| change | problem | fix |
| ------------------------ | ------------------------------------------ | --------------------------------------- |
| validated AC | driver served 17k AC hits referencing blobs nobody held → 34k client extract failures | `validated_ac_get`: withhold a hit unless every referenced blob is fetchable; self-healing (re-exec re-uploads) |
| both AC doors gated | the Execute short-circuit was a second, *unvalidated* door | route GetActionResult AND Execute's short-circuit through the same gate |
| transitive tree validation | a directory output's Tree *proto* existed but its interior files were gone → 5,390 actions failed on "validated" hits | expand each output Tree, demand every interior file + child-dir digest before serving |
| provider index is a hint | a worker's 10GB LRU evicts a blob minutes after announcing it; the driver testified on the bare index entry → 3,650 hard failures/lap | HasMany-verify index entries like bloom claimants; evict failed hints; serve path walks index → bloom claimants → fan-out probe |
| transient fetch ≠ Missing | one peer's transient QUIC error became `Missing` (a hard client verdict) → 3,212 actions lost | bounded retry rounds with backoff; a claimed-but-unfetchable blob is a retryable **infra error**, never Missing |
| zero-hit tripwire | a silent cache regression stayed green for 3 laps while re-executing everything | driver-finish warns when a lap seeded AC yet served zero hits |

### E0460 — the crown-jewel correctness fix

rustc bakes `env!()`-read values into the crate hash (SVH). The buck2 prelude
absolutizes `CARGO_MANIFEST_DIR` against each action's scratch dir, so a
crate's pipelined **metadata-full** and **rlib** twins — two actions, two
scratch dirs — hashed differently, and every downstream link died with E0460.

Fix (`canonical run-stable exec dirs`): all of a crate's emit flavours (same
`__<name>__/` output prefix) execute in one canonical directory, fixed per
OS-family + key hash, identical across actions, workers, and runs. The
alternative — patching vendored source to a literal `"."` — was rejected as
egregious and lost-on-re-vendor. This let gooseberry's polkavm stay
byte-pristine.

## 2. Distribution — move work and data cheaply

| change | problem | fix |
| ------------------------- | ------------------------------------------- | --------------------------------------- |
| input-root / crate affinity | pipelined rust pairs split across machines diverged (E0460); big-input actions fetched rlibs cross-mesh | actions sharing an input root / crate prefix pin to one worker |
| **locality dispatch** | a job landing off-data fetches its rlibs across the mesh; ~(1−1/N) of jobs do | score each worker's bloom against the job's heaviest inputs; prefer the holder for a 500ms patience window (delay scheduling). **Mesh traffic −60× (5-11 GiB → 0.07 GiB)** |
| pooled peer connections | a dial storm melted tonic h2; sequential probes cost hours | QUIC connection pool (reused streams); 64-permit fetch bound; `probe_workers` fans out concurrently |
| mesh Get read-through | worker exec inputs live on *other* workers' shards; a store-only serve returned Missing 2,756× | driver's Get serve arm relays + caches from the holder |
| platform routing via mesh | AC-only-seeded driver read Actions locally → put `/bin/sh` on windows workers | read Action/Command through the mesh, not the local store |
| session validation memo | ~59k lookups over ~20k unique entries/lap, each re-validating (tree fetch + fleet HasMany) | memoize verdicts per session; servable cleared on worker disconnect, unservable on rewrite/TTL. In-memory read-mostly (RwLock) serves a hit with no disk read |
| **parallel input staging** | `materialize()` fetched one blob per awaited round-trip (~12/s peer-bound); big substrate crate forests spent **10-22 min staging before rustc started** — run 29160244348's 72-min mac leg was almost entirely these silent gaps (compiles took seconds) | level-by-level tree walk with one batched `prefetch` per depth, then one batch over all file blobs and 64-wide concurrent writes; warm store makes each write a hardlink, not a byte-copy |
| `BlobReq::GetMany` | request overhead (stream open + frame round-trip) paid **per blob** everywhere blobs move in bulk | one request per batch, per-digest reply frames + bytes back-to-back on one stream; served by worker (store-only) and driver (read-through, per-item Provider redirect preserved) |
| sequential-await sweep | six more sites serialized independent awaits: `has_blobs` peer verification (≤12 sequential RTTs under FindMissingBlobs), `batch_read_blobs`/`batch_update_blobs` (one at a time), `sync_shard` (one stream per blob during fleet-wide finalize), AC tree expansion (×32 under scrub), eager metadata prefetch (routed per-blob) | fan-out with `join_all`/`buffered`; GetMany chunks (512, 4 concurrent) for shard sync and prefetch-by-holder; `mesh_fetches` permits acquired *inside* concurrent closures; REAPI reply order kept via `buffered` |
| staging observability | staging was invisible — a 20-min input fetch read as a dead scheduler | `[action] staging <label> (n files)` before the fetch phase, `(staged in X.Xs)` on the start line: the next stall diagnoses itself from the CI log |

Locality is *soft*: blooms only err toward false positives (cost = a fetch
we'd do anyway), and the patience window means a busy/dead preferred worker
never starves a job. Proven on the fleet bench before CI: **146× less mesh
traffic, 3.2× faster** at 4 workers.

### Sequential staging — the invisible 40 minutes

Run 29160244348 (85 min) looked like a scheduler bug: buck2 "Waiting on"
one crate for 22 min while three mac workers sat idle. The worker logs
showed nothing — because the `[action] start` line printed *after* input
materialization, and materialization was the whole stall: one awaited
fetch per file, tens of thousands of files, rlibs scattered across the
fleet. Deeper dep tree → longer stall (4 → 10 → 22 min up the substrate
stack). Three aggravators: the slot permit was held while staging (a
0.8s compile queued 14 min behind staging jobs), canonical exec dirs
re-staged the identical tree for each emit flavour, and affinity jobs
are (correctly) exempt from tail speculation, so nothing rescued them.
The fix is the staging rows above; the log line exists so this class of
stall can never hide again.

## 3. Persistence — warmth across CI runs

CI has no shared disk; warmth must be packed into `actions/cache` (10GB LRU,
branch-scoped) or run artifacts (no eviction, repo-global). The model is
per-action/per-blob incremental; the *transport* was the brittleness.

| change | problem | fix |
| --------------------------- | ------------------------------------------ | --------------------------------------- |
| sharded CAS artifacts | one monolith tar in one LRU cache entry = all-or-nothing warmth; one eviction → stone cold | CAS as digest-range shard **artifacts** (no LRU, no branch scope); per-shard unchanged-skip; losing one costs 1/N, not all |
| verify-on-import + fork filter | artifacts are a repo-wide namespace: a fork PR run could publish a poisoned `cas-shard-N` under legit digest names | hash every imported blob against its filename (a CAS name IS its sha256); drop mismatches; accept only same-repo uploads |
| native `verify-store` | a sequential shell hasher took **31 min** over 95k blobs on a windows runner, delaying that worker's join past the validation storm; parallelizing the shell hasher onto one pipe **deleted good files** (interleaved stdout) | move hashing into the engine: portable, native-fast, immune to pipe physics |
| **hot-CAS** | buck2 clients pull ~35 MB of pipelining metadata *through* the driver, relayed per-blob from workers at 84 KiB/s — latency swings with worker network placement (linux 36m vs mac 11m, identical work) | driver banks its small (<256KB) metadata CAS and restores it locally, so client downloads are local-disk, not relays. Bench: driver-local reads 584×-2236× faster than relay |
| crate-source binary cache | a rev-keyed engine binary cache recompiled (~4.5min) on every actions-only pin bump | key the binary cache on the `rebuck2/src` tree sha + Cargo.{toml,lock}, not the commit |
| era hygiene | a poisoned 10-min lap overwrote shard artifacts with thin partial sets while the restored AC referenced the old rich era → validation and exec saw different worlds | bump the AC era prefix when a fleet lap poisons shards; monolith fallback restores are self-consistent (ac+cas in one tar) |

### Rejected: eager bulk-prefetch

Pulling *all* <256KB blobs to the driver at startup grabbed **572k blobs** —
near the whole CAS, since small-crate rlibs are also <256KB — making the
driver the monolith and slowing the lap to 22m. Size can't distinguish
metadata from small rlibs. Reverted. Hot-CAS (read-through caches *exactly*
what clients download) is the correct mechanism.

## 4. Banking — persist warmth reliably at teardown

Finalize hands CAS persistence to the fleet: the driver assigns shards,
workers sync + save them in their own (parallel, off-path) teardown.

| change | problem | fix |
| --------------------------- | ------------------------------------------ | --------------------------------------- |
| preload-sticky finalize | join-order round-robin repacked ranges a worker barely held (47-92MB shards over 452-500MB ones); the co-worker was always shard-0's assignee → the eternally-absent cas-shard-0 | a worker packs the shard it *restored* (rich in that range); the co-worker (nothing packs its store) is ineligible |
| fleet-union shard lists | the banked shard was owner-store ∪ driver-store; blobs built on *other* workers in-range never reached it → the same ~23.7k entries re-executed every lap | the driver's ListShard reply unions its range with every worker's; the syncing worker gathers the whole fleet's holdings |
| bank-time AC scrub | ~13.5k dead-era rows re-validated ~13×/lap (120s memo TTL over a 27min lap) | delete unservable rows at finalize (buck2 ignores their refusal anyway); scrub skips memo-proven-servable entries so a warm lap validates almost nothing |
| done-file / deadline fixes | `Path::with_extension("done")` REPLACED `.signal` → CI polled a file that never appeared → **16m40s of pure sleep every lap**; a 900s ack wait burned 90s waiting for departed workers | append `.done`; ack deadline 900→180s (warm laps skip-pack and ack in seconds; the deadline only spends time on transition laps that re-pack) |

**Open item — reliable 8/8 banking.** Finalize historically banked only 5-7
of 8 shards (workers slow to union-sync + pack ~500MB), and each partial bank
leaves missing ranges that force a ~60-min transition lap to rebuild. This is
the one thing gating *reproducible* sub-10. Redundant assignment was tried
(doubled pack work, published duplicate artifacts, still 6/8) and reverted to
primary-only + 180s. Run 29194749613 (chunked-GetMany shard sync) banked the
first complete **8/8 in 65s** — one clean bank, not yet proven reliable;
watch the next laps.

## 5. CI — reusable, self-installing, self-healing

| change | why |
| --------------------------- | --------------------------------------- |
| composite actions | five reusable actions (`driver`, `driver-finish`, `worker`, `setup`, `buck2`) — a distributed build is two `uses:` lines; one sha pins engine + choreography together |
| autoscaler / addr-run-id | workers summoned by demand find the driver's addr artifact cross-run; probe-then-matrix sizes the fleet to zero on warm laps |
| checkpoint-capped legs | a lap that hits the 350-min job ceiling loses its teardown (post-timeout `always()` steps don't run); bound the *build* (timeout inside the step) so teardown always banks |
| quorum below fleet size | one no-show runner must not scrap the run; late joiners drain the pull queue; the join barrier is a latch (a shrinking pool never re-blocks) |

## 6. Measurement — prove gains without a cloud lap

Wall-clock on CI is hostage to runner variance (the same code ran 17m and
46m). Performance is guarded by the **counters**, which are deterministic.

- **`rebuck2 bench`** — synthetic REAPI load: seed AC entries (some poisoned),
  fire concurrent GetActionResult, report per-verdict latency percentiles.
  Killed a *neutral* memo change in 30s (driver serves 26k lookups/s → all
  110k lookups = ~4s, so the driver was never the bottleneck).
- **`rebuck2 bench-fleet`** — Docker-free in-process fleet (driver + N workers
  over the loopback mesh); pre-places data asymmetrically; reports mesh bytes
  shipped + relay-vs-local read speed. Proved locality (146×) and hot-CAS
  (584×) *before* any CI lap.
- **perf-regression gate** (`cargo test --release perf_regression -- --ignored`,
  ~27s) — asserts mesh traffic stays under the *computed* no-locality baseline
  and driver-local reads beat relay ≥2×. A routing or caching regression fails
  it in seconds.

## Performance journey

| milestone | total | note |
| --------------------------- | ------- | ------------------------------------ |
| baseline | 62 min | shallow-validated (served lies) |
| integrity-hardened warm | ~44 min | validated AC, no more storms |
| + locality | ~35 min | mesh traffic −60× |
| clean warm lap | **14m01s** | all 3 legs sub-10, mac 6m40s |
| single leg (clean) | **6m40s** | proves per-leg sub-10 |
| parallel staging + batching | 24m48s | run 29194749613 vs its 85-min predecessor: mac leg **72m → 5m35s** (staging p50 0.9s / p99 5.4s / max 11.3s over 558 actions), finalize 3m10s → **65s with the first 8/8 shard bank**; win leg (15m25s) is the new long pole |
| + win staging fs fixes | legs 8m44s | run 29195973667: concurrent mkdir pass + rename-aside delete; win staging p90 **18.7s → 3s** (max 89s → 20s), win leg 15m25s → 8m39s, second straight 8/8 bank |
| + concurrent output ingestion | **17m39s** | run 29198032688: upload_tree fan-out; openssl-src unpack 254s → **56s**, legs converge (linux 4m01s / mac 4m55s / win 5m31s, build step 5m37s); exit broadcast ends worker idling (7/8 bank — the reliability tail remains) |

Reproducible sub-10 *total* remains gated on reliable 8/8 shard banking (§4).
The engine speedup is done and proven; the tail is a stabilization tuning
problem, not a missing capability.
