# rebuck2 — ad-hoc distributed Remote Execution for buck2 on GitHub Actions

Extends [rebuck](../README.md) (persistent action *cache*) with
distributed *execution*: N ephemeral GitHub-Actions jobs form a throwaway
Remote Execution cluster for one buck2 build, over [iroh](https://iroh.computer)
P2P, with the free 10 GB actions cache as cross-run persistence. No external
service, no persistent host, no account.

rebuck (cache-only) stays a supported mode: if you don't need distribution,
the original action keeps working unchanged — rebuck2 is opt-in on top.

## Why

buck2 is RE-native — remote execution *is* its scaling axis. Two motivations:

- **Concrete trigger — the windows crate sweep.** The
  [buck2-fixups](https://github.com/gilescope/buck2-fixups) sweep builds ~2185
  crates per OS. [facebook/buck2#1359](https://github.com/facebook/buck2/pull/1359)
  (our fix) made windows `cc-rs` build scripts actually compile, so the windows
  sweep now genuinely compiles C on a single 16 GB runner and gets OOM-killed.
  Throttling concurrency is a dead end, and splitting into N independent jobs
  forfeits the shared build graph — the answer is N runners behind one buck2.
  This engine is the sweep fix.
- **General.** Any buck2 build distributes + caches across free GH runners.

## Non-goals

- **NativeLink / BuildBarn** — general-purpose, too much config. This is
  purpose-built for GH runners + the actions cache.
- **No Persistent hub / external scheduler** (e.g. a NixOS box). Everything
  ephemeral, on GH runners.
- **Tailscale** — requires account + control plane - use iroh instead (P2P, keyless rendezvous).
- **Dont Cross-compiling MSVC from linux** — keep native `cl.exe`.

## Building blocks (proven)

| piece | status | what it gives |
| --------- | ------ | ------------- |
| rebuck | shipped | buck2 REAPI wiring (`execution_platforms` in-graph, `[buck2_re_client]`), GH-cache persistence, a platform already `remote_enabled=True`. Cache-only today: misses run local. |
| iroh 1.0 | punch-tested | P2P QUIC, N0 discovery + relay fallback. Spike: two ubuntu-latest runners hole-punched a **direct** path, **261 MB/s**, zero rendezvous service (keypairs derived from `GITHUB_RUN_ID`). → blobs go P2P. |
| iroh-blobs | to wire | BLAKE3 content-addressed store; its hashes **are** REAPI digests. This is the CAS. |

## Architecture

One `rebuck2` binary, role as subcommand (`rebuck2 driver` / `rebuck2 worker`):

- **driver** — runs beside buck2; serves the localhost REAPI gRPC that buck2
  dials (Execution + CAS + ActionCache); translates `Execute` into iroh dispatch
  to workers. It is the run's ephemeral coordinator and provider index.
- **worker** — joins the mesh, claims actions, executes, streams outputs to the
  CAS, returns the `ActionResult`.

Data planes:

- **mesh** — iroh net. Rendezvous by keys derived from `GITHUB_RUN_ID` (+ a
  cache-published ticket if discovery needs a nudge). No hub.
- **CAS** — digest-keyed disk store on the driver, blobs fetched P2P on
  demand over the mesh (workers cache locally). iroh-blobs (BLAKE3, provider
  discovery) is the intended replacement — roadmap #2 residue.
- **persistence** — the actions cache seeds CAS + AC at job start and snapshots
  at job end (rebuck's dance, ported from bazel-remote's dir to iroh-blobs' store).

### "already built" is two layers

| layer | question | mechanism |
| ----- | -------- | --------- |
| ActionCache | was action *X* executed before? | shared AC lookup — hit ⇒ skip execution. No peer chatter. This is the dedup. |
| CAS | where are blob *Y*'s bytes? | provider discovery: **scheduler-implicit index** (the driver dispatched the action + got the result, so it knows `digest → producer` for free) → iroh-blobs providers → gossip/bloom only at scale |

Cross-run reuse ("another machine built it last week") is **not** live gossip —
it's the actions-cache seed: a prior run's outputs persisted to the GH cache,
restored cold, and the AC hits.

## Constraints

- **Workers must match target OS.** Windows sweep ⇒ windows worker matrix
  (MSVC actions can't run on ubuntu). ubuntu workers cover linux/mac.
- **GH cache is a seed, not the live store** — 10 GB + LRU eviction mid-build
  would break the graph. Live CAS lives on the mesh.
- **Relay is standby.** Punch works, so P2P is primary; relay only if a pair
  fails to hole-punch.
- **Driver disk is the CAS's ceiling** — every output lands in the driver's
  store (~14 GB free on a stock runner). Mitigation idea: once dispatch goes
  quiet the driver is network-bound, so it can spend the idle CPU deleting
  unused runner ballast (Android SDK, .NET, GHC, CodeQL, preloaded docker
  images — ~50 GB reclaimable) ahead of the rlib flood. Do it lazily/async,
  not upfront: cold-start time matters more than disk until the build is
  actually running.

## Roadmap

0. **Punch spike** — ✅ direct P2P proven, 261 MB/s (`experiments/punch/`).
1. **Punch reliability** — ✅ 20-pair soak (`punch-soak.yml`): **20/20 direct,
   0 relay-only**, mean 70.6 MB/s (spread 16–254 MB/s with 20 concurrent
   pairs sharing runner egress). Hole-punch between GH runners is dependable.
2. **CAS facade** — ✅ REAPI CAS/AC/ByteStream/Capabilities served by
   `rebuck2 driver` from a digest-keyed disk store; blobs travel P2P via a
   framed protocol on the mesh. (Swapping the store for iroh-blobs proper —
   BLAKE3 digests, provider discovery — stays open.)
3. **Execution v0** — ✅ `rebuck2` driver+worker, `re-e2e.yml`: buck2 on one
   runner, both compile actions executed on a second runner over iroh —
   `Commands: 2 (remote: 2, local: 0)`. AC round-trip also proven
   (restart driver, `Cache hits: 100%`).
4. **Multi-worker + rendezvous barrier** — ✅ `--min-workers K` barrier,
   least-loaded dispatch (CI: 3 actions spread 1/2 across two workers), and
   requeue-on-drop (`e2e-requeue.sh`: `kill -9` the executing worker → job
   re-lands on the survivor, build green; ≤3 attempts then fail).
5. **GH-cache persistence** — ✅ mechanism proven (`re-e2e.yml` `warm` job:
   driver store saved via `actions/cache`, restored on a fresh runner →
   `Cache hits: 100%, cached: 3`, zero execution). The 10 GB budget still
   forces a *selection policy* for what persists at sweep scale:
   - **Cost-aware**: the driver already times every action
     (`ExecutedActionMetadata` timestamps flow back with each result), so it
     gets a `digest → rebuild-cost` index for free. Snapshot greedily by
     rebuild-cost-per-byte — the slowest-to-rebuild artifacts are the most
     valuable cache residents.
   - **First-party vs third-party split**: third-party outputs (reindeer /
     `third-party//...` targets) are identical across branches and PRs —
     high hit rate, long-lived. First-party outputs churn per-branch. Persist
     third-party by default; spend any remaining budget on the costliest
     first-party actions. Target provenance is visible to the driver via the
     action's target label in the buck2 metadata.
6. **Windows worker pool** — ✅✅ **whole-tree GREEN** (run 28719654654):
   base + conflict rigs + all four snapshots in ONE buck2 invocation,
   8 workers, 2 h 35 m, failure diff clean. Ballast reclaim kept the
   driver's 17.4 GiB store inside disk. Earlier milestone — **the OOM is
   dead:** Full base leg
   (buck2-fixups PR #66): 17,296 actions executed across 4 windows workers,
   driver compile-free, no OOM, no disk death, sweep reached its
   failure-diff stage for the first time. Heavyweights that used to fail
   (`sqlx`, `arrow`, `aws-config`) now pass — they were OOM victims all
   along. Residual triage: ~25 new-vs-expected failures, largely
   platform-gated crates plus an `OUT_DIR`/buildscript-output cluster
   (gl_generator/khronos_api/built) that looks like one real RE bug.
   Six windows bugs were shaken out en route: 32K argv cap (→ argfile),
   local_only vswhere (→ limited hybrid), tmp-name race, missing system
   env, relative-argv0-vs-parent-cwd, symlink materialization.
7. **Decentralised CAS** — ✅ v1 built (opt-in `--decentralized-cas`;
   `e2e-decentralized.sh` green: outputs stay on workers, driver keeps the
   index + read-through cache). Not yet sweep-proven; known gap: a dead
   provider's blobs are unrecoverable (requeue re-runs actions, not their
   inputs' producers). Design: `Get` returns the producing worker's endpoint instead
   of bytes (the scheduler-implicit index made real), outputs stay on the
   worker that built them, consumers fetch direct over the mesh. The driver
   keeps only the index + AC + buck2's own uploads, and pulls the
   *persistence set* (roadmap #5's selection) from workers before releasing
   them at build-end. Note: cross-rig overlap already dedups twice without
   this — identical sources share blobs (content addressing) and identical
   actions don't re-execute at all (AC hit inside one graph), so the
   mono-sweep's store should come in well under the sum of its legs.
   Index-lookup amortisation ladder: (1) worker-local cache — ask once per
   blob per worker (v1); (2) batched per-action manifest — one GetMany for
   the whole input tree, then one connection per provider; (3) provider
   hints attached to the Run dispatch itself — zero marginal lookups;
   (4) bloom gossip (#8). Rung 1 until the stats heartbeat says otherwise.
8. **Discovery scaling** — ✅ v1 built: workers gossip blooms of their
   stores (30s cadence, probe indices sliced from the uniform sha256 hex,
   ~0.6% FP at 12 bits/entry); driver rebroadcasts. Consumers fetch from
   bloom-claiming peer caches before the driver — hot blobs spread across
   holders (deterministic pick from the hash), centralized mode gets egress
   offload, decentralized mode gets provider-death tolerance via surviving
   copies. Layering: bloom (many holders, approximate) → provider index
   (exact, producer) → driver store → re-derive. Unproven at sweep scale.

### Snapshot v2: sharded, content-addressed (designed, not yet built)

One-big-tar snapshots (v1, shipped) update monolithically and share poorly
across branches. v2: partition the store by digest prefix into ~32 shard
bundles; each saved under a key derived from its own content
(`rebuck2-shard-<nn>-<sha of member list>`), plus a tiny per-run manifest
listing the exact shard keys. Unchanged shard => key exists => save is a
no-op (incremental upload for free). GH cache branch scoping does the
sharing: main's scheduled sweep is the canonical donor (branches restore
default-branch entries), feature branches save only their dirty shards.
The always-dirty AC rides in its own small bundle, separate from the fat
stable CAS shards — the selection policy falling out of addressing.
Request count is constant in store size (~32 restores), dodging both
too-coarse (monolith) and too-fine (per-blob API storm).

### Scheduler v2 (designed, not yet built)

Pull-based dispatch replaces push-and-pin: driver holds the queue; a worker
runs `slots` actions and prefetches `n` more, where the DRIVER sets `n`
adaptively — large while the queue is deep (round-trip amortisation, any
imbalance self-corrects), decaying to 1 as it drains (placement precision
exactly when it matters). ~`n = clamp(queue / (workers*4), 1, 16)`,
recomputed per reply; later weighted by measured per-worker drain rate.
Garnishes: tail speculation (duplicate the last `< workers` stragglers,
first result wins) and longest-processing-time-first ordering from the
per-action duration history. Also: efficiency ledger from the 2h35m
whole-tree run — (1) wire GH-cache warm seed into sweep-re (weekly sweeps
should rebuild only what changed), (2) hardlink materialisation instead of
copying input trees per action, (3) driver runs a --slots 2 worker.

## Open questions / risks

- ~~Punch success **rate** across runner placements~~ — answered: 20/20 direct
  (run 28702838934). Residual: rate under sustained many-stream CAS load.
- Worker **rendezvous + liveness** — barrier + requeue now proven at n=2;
  unproven at pool scale (20+ workers) and for long builds (runner 6 h cap,
  workers outliving buck2's patience on a thin pool).
- CAS footprint vs the 10 GB budget under a full sweep — mitigated by the
  roadmap-#5 selection policy (cost-aware + third-party-first), but the
  numbers need measuring on a real sweep.
- ~~The **minimal REAPI surface** buck2 actually requires~~ — answered by
  building it: Capabilities, `Execute` (single done-Operation stream),
  CAS `FindMissingBlobs`/`BatchUpdate`/`BatchRead`, ByteStream `Read`/`Write`,
  ActionCache get/update. `GetTree`/`SplitBlob`/compression: unneeded.
- iroh + worker execution parity **on windows**.
- Relay bandwidth if hole-punch fails for a pair.

## Repo layout (working)

- `experiments/punch/` — the direct-vs-relay spike (done).
- `rebuck2/` — the engine (v0 shipped: driver/worker, REAPI surface, iroh
  dispatch — see its [README](../rebuck2/README.md)).
- reuses rebuck's cache/platform plumbing; cache-only rebuck untouched.

## Roadmap #9: sharded snapshots via the fleet (kill the seed/save bookkeeping)

Today the driver alone tars/untars the whole store (~3-6GB) against the GH
actions cache: ~3-5min each way, serialized on one box, inside the run's
critical path. But we have 11 machines and a content-addressed store that
shards trivially by cas/<2hex> prefix. Make the fleet do it:

- **Shard function**: prefix byte % 8 -> shard id (disjoint, no duplication).
- **Restore**: each worker restores cache entry `rebuck2-cas-shard-<i>-*`
  into its own store BEFORE serving - overlapped with the join window, so
  restore costs ~zero wall time. The driver restores only the AC (tiny,
  ~50-100MB) plus dice snapshots.
- **Serving**: workers already gossip blooms of their holdings. The driver,
  on a local miss, consults blooms and VERIFIES with a new exact
  `BlobReq::Has` round-trip (blooms have ~0.1% false positives - fine for
  routing, not for FindMissingBlobs honesty). Confirmed holders land in the
  providers map; fetches redirect via the existing Provider plumbing
  (built for decentralized mode, enabled for seeded shards too).
- **FindMissing honesty**: buck2's input-upload FindMissing against the
  driver must count worker-held blobs as present, else buck2 re-uploads
  (or worse, fails on blobs it never had under materializations=none).
  Bloom hit -> Has verify -> present.
- **Save (as built - fully distributed)**: after the build, the workflow
  touches the driver's `--finalize-file`; the driver assigns shards
  round-robin (`D2W::Finalize{shard, of}`) across the connected fleet.
  Each worker SYNCS its shard to completeness first
  (`BlobReq::ListShard` -> diff -> fetch missing over the mesh, in the
  otherwise-idle post-build window - this closes the coverage hole where
  a blob is held only by a non-assigned worker), writes `shard.id`
  beside its store, replies `W2D::Finalized`, and exits its serve loop.
  The worker JOB then tars `cas/<prefixes of its shard>` and saves the
  cache entry - 8+-way parallel, in job teardown, off the critical path.
  The driver waits for `<finalize-file>.done` and saves ac + dice only.

Expected effect: seed+save drop from ~8-10min of driver critical path to
~1min (AC only); the 10GB cap holds disjoint shards instead of one
monolith, so eviction hits one shard (self-heals: that shard rebuilds
cold) instead of the whole store.

Protocol additions (as built): `BlobReq::{HasMany, ListShard, GetMany}`,
`BlobResp::{HaveMany, HashList}`, `D2W::Finalize`, `W2D::Finalized`;
driver flags `--addr-file` (published-addr dialing) and
`--finalize-file` (shard handoff signal); worker flag
`--driver-addr-file`.

Risks: shard entry eviction skew (popular shard rebuilt often - acceptable,
self-healing); worker count < shard count (a worker restores 2 shards);
first run migrates via the driver's monolith-restore fallback (gone once
shard entries exist); a worker lost before Finalized leaves its shard
stale for one run (next run's finalize re-syncs it).
