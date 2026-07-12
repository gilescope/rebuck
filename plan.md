# Batching/parallelization sweep — driver + worker

Follow-on from the parallel-staging fix (36a67ad). Six sequential-await
sites remain; all get the same two techniques: bounded fan-out for
independent awaits, `BlobReq::GetMany` where many blobs ride one stream.
Invariants throughout: `mesh_fetches` permits acquired INSIDE concurrent
closures (never across a fan-out); shared-map merges stay sequential
after the parallel RTTs; response ordering preserved where a protocol
demands it (REAPI batch replies).

## 1. `Driver::has_blobs` — parallel peer verification (driver.rs:701)

Hot path: under buck2's FindMissingBlobs. Today each peer's `HasMany`
awaits in turn — up to 12 sequential RTTs per call.
Change: `join_all` over `by_peer`, permit inside each future, collect
`(peer, idxs, confirmed)` then merge into `have` + `providers`
sequentially. Same lesson as probe_workers' comment.

## 2. `batch_read_blobs` — concurrent reads (rpc.rs:194)

buck2 sends a batch; we serve it one `get_blob` at a time (cold blob =
full mesh fetch each). Change: `stream::iter(...).buffered(32)` —
`buffered` (not `buffer_unordered`) keeps REAPI response order for free.
Stats counters are already atomics.

## 3. `sync_shard` — chunked GetMany (worker.rs:647)

Finalize tail (~3 min/run). 24-wide but one stream per blob: a
1,000-blob sync = 1,000 stream opens through one driver conn while
every worker does the same. Change: `missing` in chunks of 512, one
`GetMany` per chunk, 4 chunks concurrent (mirrors `prefetch`). Receive
loop: per-digest frame, `store.put` on Found, skip Missing/Err
(best-effort contract unchanged).

## 4. `validated_ac_get` — parallel tree expansion (driver.rs:759)

One awaited `get_blob` per output directory; scrub_ac funnels 60k
entries through here 32-wide so the inner serialization multiplies.
Change: `join_all` the tree-digest fetches, then the existing
decode/expand loop over the results. Any fetch miss → Unservable
(semantics unchanged).

## 5. `batch_update_blobs` — concurrent puts (rpc.rs:172)

Local disk writes, sequential. `buffered(16)`; store tmp-names are
already collision-free per call. Order preserved for the REAPI reply.

## 6. `eager_prefetch_metadata` — GetMany by holder (driver.rs:1093)

Background task, 48-wide but per-blob `get_blob` routing. Change: new
`Driver::fetch_many_from(peer, digs)` (mirror of the worker's — the
pooled `peer_request` can't do multi-frame replies), group `want` by
bloom/provider claimant, chunked GetMany per holder concurrently;
leftovers keep the existing per-blob path. Workers' GetMany arm is
store-only, so no relay recursion.

## Verify

- `cargo test` (18 tests) after each file settles
- perf gate: `cargo test locality_and_caching_metrics_hold -- --ignored`
- `cargo fmt`, clippy clean
- next sweep run: FindMissingBlobs latency + finalize step timing
