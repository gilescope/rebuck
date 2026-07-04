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

One `rebuck2` binary, `--role`:

- **driver** — runs beside buck2; serves the localhost REAPI gRPC that buck2
  dials (Execution + CAS + ActionCache); translates `Execute` into iroh dispatch
  to workers. It is the run's ephemeral coordinator and provider index.
- **worker** — joins the mesh, claims actions, executes, streams outputs to the
  CAS, returns the `ActionResult`.

Data planes:

- **mesh** — iroh net. Rendezvous by keys derived from `GITHUB_RUN_ID` (+ a
  cache-published ticket if discovery needs a nudge). No hub.
- **CAS** — iroh-blobs, fetched P2P on demand.
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

## Roadmap

0. **Punch spike** — ✅ direct P2P proven, 261 MB/s (`experiments/punch/`).
1. **Punch reliability** — ✅ 20-pair soak (`punch-soak.yml`): **20/20 direct,
   0 relay-only**, mean 70.6 MB/s (spread 16–254 MB/s with 20 concurrent
   pairs sharing runner egress). Hole-punch between GH runners is dependable.
2. **CAS over iroh-blobs** — a REAPI CAS/AC facade buck2 can hit; prove a buck2
   build pulling CAS P2P (local execution still).
3. **Execution v0** — minimal REAPI `Execute` on the driver; one worker runs one
   action remotely, end to end.
4. **Multi-worker + rendezvous barrier** — driver waits for ≥K workers; workers
   outlive the build; distribute a real build slice.
5. **GH-cache persistence** — seed/snapshot the iroh-blobs store + AC; a cold
   build warms from the previous run. The 10 GB budget forces a *selection
   policy* for what persists:
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
6. **Windows worker pool** — re-point the windows sweep at it. **Kills the OOM.**
   The concrete deliverable.
7. **Discovery scaling** — gossip/bloom provider index, only if the
   scheduler-implicit index proves insufficient.

## Open questions / risks

- ~~Punch success **rate** across runner placements~~ — answered: 20/20 direct
  (run 28702838934). Residual: rate under sustained many-stream CAS load.
- Worker **rendezvous + liveness** — jobs start independently, must overlap in
  time and survive the whole build; a dropped worker must reschedule gracefully.
- CAS footprint vs the 10 GB budget under a full sweep — mitigated by the
  roadmap-#5 selection policy (cost-aware + third-party-first), but the
  numbers need measuring on a real sweep.
- The **minimal REAPI surface** buck2 actually requires (`Execute`/
  `WaitExecution`, CAS `FindMissingBlobs`/`ByteStream`/`BatchUpdate`,
  `ActionCache`) — scope it before building.
- iroh + worker execution parity **on windows**.
- Relay bandwidth if hole-punch fails for a pair.

## Repo layout (working)

- `experiments/punch/` — the direct-vs-relay spike (done).
- `rebuck2/` — the engine (next).
- reuses rebuck's cache/platform plumbing; cache-only rebuck untouched.
