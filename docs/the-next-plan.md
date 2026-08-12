# The next plan

Every target loses. The file that says so is `fleet-findings.md`; the file
that says why it cannot be a scheduler is principle 30. This one is about
what to do instead, and it starts by naming three things the project has
been assuming rather than measuring.

Nothing here is a new mechanism. Three of the six moves are environment
variables, one is a calculation with no run attached, and one is a decision
about what to benchmark.

## The arithmetic, on this project's own numbers

`+test-ast`, six machines, run ref1 throughout — **one run, one clock**, so
nothing here is a cross-run join. (An earlier draft of this section built
its headline on a 1-worker rate extrapolated across a 6-worker fleet, and
on machine-seconds read as wall clock. Both were wrong; see the retraction
below, which is kept because the corrected numbers are stronger.)

| quantity | ref1 | |
| ------------------------- | ---------- | ------------------------------ |
| baseline CPU | 607s | |
| baseline wall | 216s | ⇒ **2.81x** internal parallelism |
| worker CPU, all six | 5,575s | ⇒ amplification **9.2x** |
| solves | 412 | |
| leg | 1,092s | |
| ancestry, per machine | ~705 MiB | principle 30 |
| materialisation rate | 7.2 MB/s | ⇒ ~103s per machine |

### The one comparison that is safe to make

Both of the terms below are **machine-seconds**. Neither is converted to a
wall clock, and that is deliberate — the conversion is where this project
keeps hurting itself.

```text
premium   =  5,575  -  412 x 1.9   =  ~4,790 machine-seconds
ancestry  =  6  x  103             =    ~620 machine-seconds
```

**The premium is roughly eight times the ancestry.**

That is the finding, and it is a sharp one, because principle 30 names
`N x ancestry` as the term that binds and the whole principles file is
organised around it. On this run **the ancestry is a minority of the
deficit — about an eighth of it.** The dominant term is a per-solve cost
that principle 30 does not model at all, and that no mechanism in this
project has ever varied.

It is also internally consistent, which the previous draft was not: the
premium is derived by *subtraction from* the measured worker total, so it
cannot exceed it. Any figure larger than 5,575 was arithmetically impossible
and should have been caught by inspection.

### What the corrected numbers do NOT say

They do not say the fleet lands at parity. That claim came from multiplying
the ancestry by six and comparing 600 machine-seconds against a 216-second
wall clock — **shape 22, committed afresh, three sections after invoking
it against someone else.** The ledger has already retracted exactly this
("a worker costs about 27 seconds of WALL to become useful, once, not 162")
and records that a scheduling rule built on the confusion was written,
tested and **removed before it shipped**.

The honest statement of the counterfactual is that it cannot be computed
from what is measured. A zero-premium fleet costs about 103s of parallel
materialisation plus its share of the work, against a 216s baseline — which
looks like a win of roughly 1.5x. But the baseline's 607 CPU-seconds
*already contain* its own one-off materialisation of the same ancestry, and
**nothing separates those two**, so the subtraction that would make the
comparison like-for-like has no measured input.

So: **there is probably headroom of order 1.5x behind the premium, and this
document cannot tell you more than "probably" until the building split
exists.** What does not depend on that unknown is the ordering — premium 8x
ancestry — and the ordering is what decides what to work on.

The residual is worth stating too, since it is the honest size of what is
unexplained. Charging the premium and the work against six machines at the
baseline's own parallelism accounts for roughly 420 of the 1,092-second leg.
**About 60% of the leg is not in this model at all**, and `waiting 16%` plus
placement is where to look for it. A model that explains 40% is a lead, not
a diagnosis.

## 1. The premium is an OCI image export, per solve, of the whole ancestry

`solve.rs:145`, `publish_attrs` — the attrs on every result a worker hands
back:

```rust
("push".to_owned(), "true".to_owned()),
("source-date-epoch".to_owned(), "0".to_owned()),
("rewrite-timestamp".to_owned(), "true".to_owned()),
```

`rewrite-timestamp` is **off by default in buildkit, and its documentation
says why: "the overhead of rewriting image layers"**. moby/buildkit#4805
records that it rewrites *base image* layers too, not only the ones the
solve produced. So each dispatched solve re-tars — and, whenever a
compression codec is also set, recompresses — every layer beneath it.

That is a per-solve cost proportional to the ancestry, which is precisely
the shape of a premium that is ~8x the whole baseline while the work per
solve is 1.5s. A local build pays none of it: the baseline never exports.

This has never been varied. It has been on in every run in the ledger,
which is why it reads as part of the furniture.

**The experiment is an env switch and one run.** Drop `rewrite-timestamp`
and `source-date-epoch` behind `REBUCK2_REPRODUCIBLE` (default on, so no
existing number is repriced) and run a `-norepro` arm against the
reference. Prediction, stated first as this file requires: worker CPU falls
by more than any change measured so far. If it does not, the premium is the
portable rewrite and that is the next instrument.

What is given up is byte-identical layers between machines, which buys
cross-worker blob dedup and nothing else — each result is pushed by exactly
one worker, so correctness does not depend on it. `publish_attrs`' own
comment already records that client-visible output digests were identical
with and without it.

### Why `rewrite-timestamp` is on, and why that reason does not survive here

It is not an oversight. `81a0ce7` put it there deliberately, with a
measurement: buildkit stamps wall clock into the config and real mtimes into
the layer tar, so **republishing an unchanged graph produced fresh bytes,
moved the tag, and orphaned everything the tag used to name.** Over 24
adoptions of 4 distinct graphs into one persistent store:

| attrs | blobs | orphaned |
| ------------------------ | ----- | -------- |
| neither | 75 | 60 |
| `source-date-epoch` only | 75 | 60 |
| both | **15** | **0** |

An earlier draft of this section called it "a cost with no matching
benefit". That was wrong — the benefit is real and measured. What is wrong
with it is narrower, and there are three things:

**1. The benefit is sized in blob COUNT, and this repo has already
discredited that unit — with these exact numbers.** `how-this-lies.md`
shape 6:

> counting orphaned BLOBS to size the mirror's garbage. 60 of 75 were
> unreachable, which reads as 80% waste and an obvious case for a gc. **By
> bytes it was 0.6%**: the base image is one shared 4MB object and the
> orphans are kilobytes of metadata. The stand-in argued for the opposite
> conclusion.

Same 75, same 60. `56af5eb` wrote that down at 21:07. `81a0ce7` used the
same count as its justification at **21:44 — thirty-seven minutes later.**
So the honest size of the win is **0.6% of the store by bytes**, and it is
being bought with a whole-ancestry re-tar on every solve.

That is shape 25 — the same misreading twice inside the hour — in a second
instance the file does not record.

**2. The rig is not the fleet's regime.** 24 adoptions of 4 graphs into
**one persistent store** is six republishes per graph, deliberately
constructed so repeats dominate. The fleet runs on ephemeral runners with a
fresh mirror per run, and each subtree result has exactly one producer that
pushes it exactly once. The repeat the attr exists to make cheap barely
happens where the attr is being paid for.

**3. The cost was never on the other side of the scale.** `81a0ce7`
measured the benefit carefully and priced nothing. That is shape 24 — a
trade-off judged on one side of the trade — which this file already invokes
against cache-mount seeding.

**4. And it may be breaking build caches, which is not a cost at all but a
correctness bug.** `rewrite-timestamp` does not truncate mtimes — it sets
every file in the layer to **epoch 0**. Any tool that decides freshness by
comparing timestamps sees a source tree entirely dated 1970.

Cargo is exactly such a tool. rustc's incremental compilation is content-
hashed and does not care, but **cargo** decides whether to invoke rustc at
all by comparing source mtimes against output mtimes, and cargo already
knows about this failure mode in containers — `rust-lang/cargo#6529`:

> when a step is cached in Docker, the nanosecond portions of all files is
> zeroed out

Epoch 0 is that failure mode taken to its limit. The workaround on the
cargo side is `-Z checksum-freshness` (nightly; build-script inputs still
use mtimes), which swaps mtimes for content checksums and is documented as
being for exactly this — "systems with a poor mtime implementation, or in
CI/CD".

This does not explain any number currently in the ledger: the benchmarked
targets are Go, and Go's build cache is content-hashed, so it is immune.
**It becomes live the moment a Rust workload is used**, which is one of
this document's own recommendations, and it would show up as dispatch
overhead rather than as a broken cache.

**What none of this changes:** integrity. Everything here is fetched by
digest, so a client that asks for a digest and gets bytes hashing to it has
the right bytes, whatever their mtimes. Determinism is about *cheap
republish*, not correctness, and `publish_attrs`' own comment confirms
client-visible output digests were identical with and without it.

**So: off by default, on where it was measured.** Keep both attrs for the
persistent-mirror path they were justified on; do not pay them on per-solve
dispatch, where the repeat does not occur, the price is the largest term in
the build, and the side effect is a layer full of 1970. Same argument for
`force-compression`, which normalises a layer's encoding — something a
digest-addressed transport never needs.

**Flipping the default reprices the ledger**, and `solve.rs`' own comment
says so about the sibling codec knob. Do it behind a named switch reported
in the wire line, so a run says which regime it was in and no future reader
compares across the change without noticing.

### The `-nocomp` arm as committed is confounded

`compression_attrs` returns an EMPTY map when `REBUCK2_COMPRESSION` is
unset, and `{compression, force-compression=true}` when it is set. So the
reference has no `force-compression` and `-nocomp` has it. The arm moves
two variables:

- codec: buildkit default → uncompressed
- `force-compression`: absent → true

and `force-compression=true` is the one that *forces conversion of every
layer*, including base layers that were already fine. A null result is
therefore unreadable — it could be the compress saving cancelling against a
newly-forced whole-image conversion. Add a `-forcecomp` arm (codec =
buildkit default, `force-compression=true`) or the pair does not bisect.

### Where a layer comes from barely matters — only what happens on arrival

Asked directly: what is the difference between materialising a layer from
Docker Hub and from a peer or the driver? Everything in the ledger says
**almost none**, and the reason is the same premium.

| path | rate | source |
| ---------------------------------- | ------------- | ------------------------------ |
| worker serving OUT to a peer | **38.3 MB/s** | run C w1: 4005 MiB / 104,435ms |
| anything IN to a worker | **7.2 MB/s** | 17 leads >1 MiB, 87% of lead time |
| loopback rig, same box | ~286 MB/s | 3.5 ms/MiB, the local seed rig |
| Docker Hub | **never measured** | only its failure modes are |

The inbound figure does not distinguish peer from coordinator because the
distinction has already been made irrelevant: **81% of fetches are local
and the coordinator served 742 MiB while 32,924 MiB moved.** The mesh
eliminated the remote-source term almost completely — and the receive rate
is still 7.2 MB/s. The transport was won and the leg did not move.

**The asymmetry is the tell.** The same worker, same hardware, same link,
serves at 38.3 MB/s and receives at 7.2. Serving is a read of a stored
compressed blob; receiving is fetch, gunzip, overlayfs write. A 5.3x gap on
one machine puts the cost on the arrival side, not the wire, so changing
the *source* moves the small term. (◈ conf 0.8 — the two rates are from
different runs, so this is an order-of-magnitude argument, not a controlled
pair.)

**And 7.2 MB/s is slower than it should be even for gunzip.** The prose
around it attributes it to "two cores"; these are public-repo runners, so
there are **four** — and four cores of gzip is nearer 200 MB/s than 7. The
rate is one stream's worth at best, which is what you would expect from a
chain that *must* be applied in dependency order. Unpack is serial by
construction. Cores cannot fix it, a faster codec can dent it, and not
unpacking at all is the only thing that removes it — which is the estargz
lever, and why it outranks every transport idea left.

What Docker Hub does differ in is its failure modes, all already recorded:
it rate-limits (429, twelve red assertions), a sessionless peer cannot
reach it at all, and **it does not serve estargz** — so lazy pull is
reachable for the fleet's own results and not for base layers, unless the
mirror converts on the way through.

**Cheap settling experiment**, if it is wanted: one machine, one blob,
materialised from Hub, from the coordinator, and from a peer, timed. A
`reserve-check.sh`-shaped local rig — minutes, not a CI run. Principle 23.

### Worker-to-worker saturation is unknown, and unknowable as instrumented

`38.3 MB/s from this registry` is `SERVED_BYTES / SERVED_MS`, and
`SERVED_MS` is accumulated by an axum middleware that adds **each
request's own elapsed time** (`registry.rs:1341`). Requests are served
concurrently. So the denominator is a **sum over overlapping handlers**,
not a wall clock — six simultaneous fetchers make it advance six times
faster than time does.

**That is shape 22 for the second time**, in a metric this file has quoted
as a link rate. It does not merely fail to measure saturation: it fails in
the *flattering* direction, understating egress by roughly the concurrency
factor, so a saturated link would read as a comfortable one.

What can be said, from wall-clock-denominated figures only:

| | run D |
| --------------------------------- | ---------- |
| content that left the workers | 32,924 MiB |
| leg | 1,396s |
| ⇒ aggregate egress across 6 workers | **~23.6 MB/s** |
| ⇒ per worker, mean | **~3.9 MB/s** |

Against a gigabit link that is **~3%**; against a pessimistic 100 Mbit cap,
~31%. **On the mean the mesh is nowhere near saturated**, which is the same
conclusion the 7.2 MB/s inbound rate points at from the other side, and it
is consistent with the cost living in unpack rather than on the wire.

The mean is not the question, though. Saturation is a burst property, and
the ledger has two candidate bursts it has never measured: the
six-concurrent-fetchers-for-one-digest race, and broadcast, where 590 MiB
left one machine seven times over. Nothing samples the link at burst
resolution.

**The instrument is about five lines** and belongs with the others in §2:
egress divided by *elapsed* time since the registry started, plus an
in-flight-request gauge carrying a high-water mark. Both are trivial, both
are wall-clock honest, and together they turn "are we saturated" from a
guess into a reading. Until then the answer to the question is **no, we do
not have a feel for it, and the number we thought gave us one was
measuring something else.**

## 2. The ceiling that has been quoted is not a ceiling on this fleet

`proxy.rs:1182`. `ceiling` is Amdahl over the concurrency of **solve
spans** — wall time during which at most one *solve* was in flight. It
bounds a fleet against a world where solves run one at a time.

The baseline does not live in that world. It runs one solve containing
everything, and buildkit parallelises inside it: **2.73x and 2.81x** on the
two attested single-run pairs (604 CPU-s / 221s, 607 CPU-s / 216s) — call
it ~2.8 vertices at once, on one machine. So `ceiling 2.97x at seven
machines` is not 2.97x over the baseline wall — the two numbers are
denominated against different things, and putting them side by side is
shape 22 with a different numerator.

The number that actually decides whether this project can ever win is the
**critical path of the op graph**: the longest chain of dependent vertices,
in seconds. It is the floor under any execution, distributed or not.

- It costs **zero runs and zero new instrumentation**, but it is not zero
  code, and an earlier draft said it was. The two halves exist separately
  and have never been joined:
  - **weights** — `home_vertices` (`proxy.rs:1398`), digest -> (ms, cached),
    filled by `note_vertices`.
  - **edges** — NOT in the vertex stream. `note_vertices` (`proxy.rs:129`)
    takes digest, ms and cached, and never touches `v.inputs`. The edges are
    in the **LLB**, which `dispatch.rs` already walks (`op.inputs`, used at
    `:669`, `:1524`, `:2278`), keyed by the same digests.

  So the join is real and the keys already match; what is missing is the
  join itself and a longest-path walk over it. **Caveat:** watch-vertices is
  currently gated — the one run with it active failed parity (ledger 5929)
  and the history read is the named suspect. One clean validation run before
  its data is trusted.
- If the critical path is near the 216s baseline wall, that baseline is
  already at the graph's
  floor and **no fleet beats it, at any efficiency, ever.** That is a real
  possible answer and it should be allowed to arrive cheaply.
- If it is 30s, there is 7x on the table and everything below is worth doing.

Build this before anything else. It is the only instrument here that can
end the project, which makes it the one with the highest expected value.

## 3. Admission: compute the ratio, print it, refuse below it

Principle 30 says the fleet wins only above `N x ancestry`. It is stated as
a principle and applied as a habit. Make it a predicate the dispatcher
evaluates and prints, before it places anything.

**Get the units right, or this rule has already been written and deleted
once.** The obvious form —

```text
dispatch iff   work_CPU  >  K x N x ancestry_bytes / materialise_rate     ← WRONG
```

— multiplies ancestry by `N` and compares it against a work figure, and the
ledger removed a scheduling rule resting on exactly that before it shipped:
the six machines materialise **in parallel**, so `N x ancestry` is
machine-seconds and belongs on neither side of a wall-clock inequality.
The correct form has both sides in wall seconds and no `N` on the ancestry:

```text
dispatch iff   work_CPU / (N x p)   >   K x ancestry_bytes / materialise_rate
```

where `p` is the baseline's own internal parallelism (~2.8). Ancestry is
paid **once per machine, concurrently**, so it enters as one machine's
share; the work is what divides.

`+test-ast` at N = 6: the left side is 607 / (6 x 2.8) = **36 wall-seconds
of distributed work** against **103 wall-seconds of materialisation**. It
fails **even at K = 1**, by 2.9x — so no choice of `K` rescues this target,
which is a stronger and better-founded statement than the one this section
made when it had the units wrong.

Both sides are already measured per target: ancestry from the manifest,
`materialise_rate` from the registry report, `work_CPU` and `p` from the
previous run's history.

The output is the point as much as the gate: a target that fails prints
*which* term failed and what N would pass. A fleet that declines to
distribute and says why is worth more than one that distributes and loses,
and the whole of shape 1 is about the first being indistinguishable from
the second unless it is made to speak.

## 4. Change the load, and change the machine lifetime

**The load.** earthbuild's Earthfile is exhausted, and it was a hostile
benchmark: 607 CPU-seconds over 705 MiB of ancestry is 0.9 CPU-s per MiB.
No dispatcher wins that. What clears admission is minutes-per-leaf over a
shared base — a large Rust workspace, a wide test matrix, a fuzz sweep.
That is what "embarrassingly parallel" means quantitatively, and it is a
property of the workload, not of the code that distributes it.

The ledger has the evidence already and it was read as a defeat:
`+all-binaries` is five leads of minutes and still loses, because five
leaves sharing one 700 MiB stem does not clear the ratio. **Fifty leaves
would.** The failure was one of scale, not of kind.

**The lifetime.** Hosted runners are destroyed after every run, so ancestry
is paid cold, on every machine, every time — forever. This is the term
principle 30 says no scheduler can touch, and a warm worker touches it
directly: paid once per machine per *lifetime* instead of per run.

This is where the industry has landed and the numbers are not close.
Blacksmith measured persistent builder state against export/import cache
backends at **10-30x across warm and source-change scenarios**, with a
Rust *dependency* change as an outlier well outside that range — **3.6s
against 164s, about 46x** ([the physics of Docker build
caching](https://www.blacksmith.sh/blog/the-physics-of-docker-build-caching);
neither figure is in this repo's ledger, so it is cited, not attested). The
mechanism they name is exactly this one:
serialising and re-transferring state that a persistent machine simply
still has. Earthly Satellites and Depot are the same bet. Nobody sells
"split one build across N cold machines", and this project now has 8,600
lines of evidence about why.

## What this de-prioritises

Every remaining placement idea. Affinity, balance, prefetch, seeding,
broadcast and grafting all decide WHERE work goes and WHEN bytes arrive.
The premium in §1 is paid per solve wherever the solve runs, and the ratio
in §3 is a property of the target. The pair of runs that reproduce worker
CPU to 0.1% while the per-worker split moves every time already said this
in one line: **placement decides who does the work, not how much there is.**

## Order

1. Critical path of the op graph. No run. Can end the project.
2. `-norepro`. One env var, one run. Predicted the largest CPU drop yet.
3. `-forcecomp`, to de-confound `-nocomp`. One run.
4. **`-estargz`, and it is no longer speculative.** Measured locally on
   x86: a consumer read one file out of a 192 MiB image and fetched
   **1,143,096 bytes — 5/1000 of the layer**, 9 of 12 blob GETs being
   HTTP 206. See `scripts/stargz-check.sh` and the ledger entry.

   Two pieces are needed and only one is wired. `compression_attrs`
   (`solve.rs:135`) accepts `estargz` today, so the export side is an env
   var plus `*-estargz*) comp=estargz ;;` in the workflow case statement.
   The consuming side is the work: the snapshotter **cannot** be set via
   `EARTHLY_ADDITIONAL_BUILDKIT_CONFIG` — the image template hardcodes
   `[worker.oci] snapshotter = "auto"` and TOML forbids a duplicate table
   — so each worker's `docker run` must edit the template before the
   entrypoint renders it, and pass `--device /dev/fuse`.

   **Shipping only the export half measures a regression**, because an
   estargz blob is bigger than gzip and nothing is skipped without the
   mount. That is the shape-13 trap this arm has to be built to avoid.
5. Admission predicate, printed, in the corrected units. No run.
6. Then, and only if 1 leaves headroom: a load that clears admission, on
   workers that outlive it.
7. **Last, and only if a Rust workload makes it pay: patch containerd to
   keep nanosecond mtimes.** See below.

Steps 1 and 5 cost nothing and are the two that say whether the rest is
worth running at all. That ordering is the actual change of plan: **stop
measuring the fleet, and start measuring whether the target has a win in
it.**

### 7, in full: nanosecond mtimes in exported layers

Layer tars written by containerd carry **whole-second** mtimes, and this is
not a setting. `pkg/archive/tar.go`:

```go
// truncate timestamp for compatibility. without PAX stdlib rounds timestamps instead
hdr.Format = tar.FormatPAX
if cw.modTimeUpperBound != nil && hdr.ModTime.After(*cw.modTimeUpperBound) {
    hdr.ModTime = *cw.modTimeUpperBound
}
hdr.ModTime = hdr.ModTime.Truncate(time.Second)
hdr.AccessTime = time.Time{}
hdr.ChangeTime = time.Time{}
```

Note it selects **PAX**, which can carry nanoseconds, and discards them on
the next line. Access and change times are wiped outright. BuildKit uses
this differ, so it applies to every layer this fleet exports.

The precision exists at every other stage — ext4, xfs, btrfs and APFS all
store nanoseconds, and PAX has a field for them. Container tooling is the
only place it is thrown away.

**The patch is one line** (drop the `Truncate`) plus a BuildKit built
against it. `rebuck2/patches/` already carries fork patches, so the
machinery exists.

**Why it is step 7 and not step 2.** Three reasons, in order:

- **`rewrite-timestamp` dominates it.** Setting every mtime to 1970 is
  strictly worse than truncating to a second, so while that is on, this
  patch changes nothing. Step 2 has to land first or this cannot even be
  measured.
- **It fights dedup.** Finer timestamps make identical builds produce
  different layer bytes more reliably, which is the opposite of what
  `81a0ce7` was reaching for. Worth paying only where a build cache
  depends on it.
- **Nothing currently benefits.** The benchmarked targets are Go, whose
  build cache is content-hashed. The beneficiary is cargo, and cargo has
  its own answer in `-Z checksum-freshness`, which needs no patched
  containerd at all.

So this is the right fix for the case where a Rust workload is the load,
`-Z checksum-freshness` is unacceptable (it is nightly-only), and the
measurement says mtime granularity is what is costing rebuilds. **Record
that condition before doing it** — a containerd fork is a maintenance
liability, and this project has been bitten before by mechanisms adopted
on a plausible mechanism rather than a measured need.

## What this document got wrong, and what caught it

Reviewed adversarially before it was acted on: 23 findings raised, 10
survived an independent attempt to refute them. Two were load-bearing and
both are corrected above — the parity claim (shape 22) and a premium figure
extrapolated from a 1-worker run across a 6-worker fleet, which came out
**larger than the measured worker total it was supposedly a part of**.

That second one deserves its own line in `how-this-lies.md` eventually,
because the check that would have caught it was free: **a part cannot
exceed its whole.** 8,900 against a measured 5,575 needed no ledger
archaeology and no second opinion, only the arithmetic already on the page.

The review is not a guarantee. It could not check the earthbuild fork's own
`rewriteRemoteWithEpoch` for a content-store lookup that would amortise the
re-tar across solves — which, if present, weakens §1 considerably — and it
could not attribute the premium between export, the portable rewrite and
materialisation, because the building split still does not exist. Those are
the two places this plan is most likely to be wrong.
