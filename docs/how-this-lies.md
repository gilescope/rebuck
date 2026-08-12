# How this system lies to you

A distributed build system that fails open has a specific hazard: **almost
every way it breaks still produces a correct build.** The user gets their
bytes, the exit code is zero, and the thing you built does nothing.

These are the shapes that actually occurred while building it, each with the
instance that taught it and the countermeasure that now exists. They are not
general testing advice - they are what goes wrong *here*.

## 1. Fail-open hides an idle fleet

Nothing dispatched, everything built at home, build succeeded. Indistinguishable
from a fleet with nothing to distribute.

**Instance:** following this repo's own quickstart. The daemons had no
`[registry]` stanza, so every peer refused; publishing is insecure per-solve
so the mirror filled and the log looked healthy.

**Countermeasure:** `placed` in the wire report is the only honest evidence,
and the proxy now says `no solve completed on a peer` with a likely cause.

## 2. An assertion that cannot fail

A check that would pass whether or not the thing under test worked.

**Instance:** `grep -c 'placed : {0: '` to assert an emulated peer got no
work. True whichever peer got it. It passed while that peer took a third of
the build.

**Countermeasure:** run the negative case. If removing the mechanism does not
turn the test red, the test is decoration.

## 3. A cached pass

The assertion is right, the setup is right, and the system answers from
memory instead of doing the work.

**Instance:** the ssh probe reported success with the agent removed, in
0.19s. Buildkit served the previous run's result, so the strengthened
assertion had never once executed.

**Countermeasure:** `ignore_cache` on any exec a probe depends on. Suspect
any pass that is faster than the work it claims to have done.

## 4. A setup that does not create the condition

The test names a scenario it does not construct.

**Instances:** an "unresolvable secret" test that set the variable to an EMPTY
string, which `env::var` returns as `Ok("")`. An emulation test that used an
UNPINNED graph, where every peer is native by design.

**Countermeasure:** assert the discriminating observation, not the outcome -
and check that the two arms of a pair actually differ.

## 5. A metric that improved for another reason

Real improvement, wrong attribution.

**Instance:** rounds 2 and 3 dropped 39s to 10s after adding peer strikes. A
control with strikes disabled also dropped to 10s: the peers had warmed their
own caches. 10s was the floor either way.

**Countermeasure:** the control run. Twice in this project it overturned the
conclusion.

## 6. A plausible stand-in for the quantity you want

**Instance:** weighting a 32-core peer 2x against a 16-core host. Cores are
not throughput once transfer dominates, and the "informed" weight measured
WORSE than a flat split - 21s against 18s.

**Instance:** counting orphaned BLOBS to size the mirror's garbage. 60 of 75
were unreachable, which reads as 80% waste and an obvious case for a gc. By
bytes it was 0.6%: the base image is one shared 4MB object and the orphans
are kilobytes of metadata. The stand-in argued for the opposite conclusion.

**Countermeasure:** measure the quantity, or leave the knob at its default.

## 7. A feedback signal the controller moves

**Instance:** deriving peer weights from observed service time. Load a side,
its mean rises, the controller reads "straggler" and sends more work away,
which raises the other mean. One run looked like a win; the next was worse
than doing nothing.

**Countermeasure:** ask whether the input is a function of the output. If it
is, the loop chases itself - ship it off by default with the measurement
attached.

## 8. Attempted counted as achieved

**Instance:** the idle-fleet check first used `placed`, which counts placement
DECISIONS. A solve sent to a peer that refused it is placed and not routed -
exactly the case the check existed for - so it stayed silent on its own
motivating bug.

**Countermeasure:** every counter says which it is. `placed` is decisions,
`routed` is successes, and they are never the same number when it matters.

## 9. An environmental failure read as a regression

**Instance:** twelve assertions went red right after a registry change.
Consistent failure looked like code. It was `429 Too Many Requests` from
Docker Hub, from a day of fresh daemons pulling the same base image.

**Countermeasure:** read the failing output before forming a theory. The
harness now prints `RATE LIMITED` and the suite skips rather than fails.

## 10. The author's environment inside the instructions

**Instance:** the quickstart, written from a working system, omitted the
daemon config the author had set up months earlier.

**Countermeasure:** run the instructions from an empty directory. A
quickstart nobody has executed is a hypothesis.

## 11. A symptom standing in for a property with no instrument

The property is real and the test is about the right thing, but nothing
measures the property directly - so the assertion greps for something that
usually accompanies it.

**Instance:** "a dead mirror costs no peer its standing" was checked by
grepping for the message `peer not blamed`. Killing the registry has two
correct outcomes depending on whether the base was mirrored yet; the other
one reports `base unmirrored` and never emits that string. Measured at 1 in
3 - and a flake is worse than a failure, because a flake gets retried.

**Countermeasure:** when a test greps for a symptom, ask what observable the
property actually has. If the answer is "none", that is the bug. `struck` is
now reported always, including empty, because a line that appears only on
failure cannot evidence an absence.

## 12. A check that finds nothing without proving it looked

An audit prints no findings. That reads as "clean" and is equally consistent
with "never ran".

**Instance:** auditing which `REBUCK2_*` variables the suite exercises. The
loop ran ONCE rather than seventeen times - zsh does not word-split unquoted
parameter expansions - and its `grep -q` was handed a multi-line pattern,
which greps as an alternation and so matched because SOME name appeared. It
printed nothing. Reported as "all exercised". Six of seventeen were not, and
one of those was hiding an unbounded map that nothing read.

**Countermeasure:** make the search state its own size. `found N, checked N,
unchecked M` cannot be satisfied by a loop that never ran.

## 13. A knob that is wired, documented, and inert

Not dead code - dead *effect*. Every part exists and the value arrives
where it is read, but something upstream is constant, so the mechanism
cannot express itself.

**Instance:** peer weights. `--peer url*4` parses, reaches the proxy, prints
in the banner, and is consumed by the placement score - divided into a load
counter that was still zero every time the score was computed. Zero over any
weight is zero. Measured 6/6 against 6/6 with a fourfold weight under full
contention, and 3/9 once fixed.

The counter WAS maintained - around the adoption. About 1.6s of preparation
sits between choosing a peer and adopting on it, and a burst of twelve solves
decides inside 130ms, so every decision read a counter that nothing had
reached yet. Right quantity, wrong instant.

My first write-up of this said the counter was "never incremented anywhere".
It was: `grep 'outstanding.*fetch_add'` is a single-line pattern and the call
is split over two lines by the formatter. That is shape 12 again - a search
that found nothing and did not prove it looked - used as evidence for a
stronger claim than the symptom supported.

**Countermeasure:** vary the knob and require the OUTPUT to move. This one
was never exercised end to end, so nothing ever asked it to make a
difference.

**And then the same shape again, in the correction.** The first write-up of
this said the finding "informed weights measured worse - 21s against 18s" was
therefore noise. It was not. That sweep ran on TWO daemons, where the weight
acted through `turn` on the home-versus-away decision, and `turn` worked. The
inert path was choosing between two away peers, which a two-daemon fleet never
does. A later, correct fix - stopping saturation and the rotation from both
deciding home-versus-away - retired the path the sweep had used.

So the number was real evidence about a mechanism that no longer exists,
which is a third thing, distinct from both "valid" and "noise". Deciding a
past measurement is void is itself a claim, and wants the same standard of
evidence as the measurement did.

## 14. A zero that means "switched off"

`[wire] grafted : 0 subtree(s) started from a built ancestor` printed in
every run, including every run where `REBUCK2_GRAFT` was unset - which was
all but one of them.

**Instance:** I read that zero as "grafting never fires", wrote it into the
workflow as a standing comment, and used it to rank grafting below other
work. The mechanism had simply never been switched on. `seeds=0/0` had the
same defect: a run with no seeds configured and a run whose seeds all failed
to resolve printed the identical pair.

**Countermeasure:** `OFF (REBUCK2_GRAFT unset) - not a zero`, and
`seeds=off`. A reported number must distinguish "measured, none" from "not
measured", and naming the variable in the output is what makes that
impossible to misread.

## 15. A fallback that turns "I could not look" into "nothing changed"

**Instance:** `check-reserve` printed `0 KiB` for all six solves including
the first, which reads as "the base is never fetched". Every stats request
had failed - it was polling `host.docker.internal`, which is how the DAEMON
reaches the host and not a name the host resolves - and the code fell back
to the previous sample. Total instrument failure was indistinguishable from
the clean flat line it was supposed to prove.

**Countermeasure:** a `--stats` address over loopback, and the general rule:
a fallback to the last good value must be counted, or silence becomes
evidence. The status tap counts its dropped samples for the same reason.

## 16. An instrument on the critical path of what it measures

**Instance:** the vertex tap took `wire` - the mutex the placement path uses -
once per frame of earthly's progress stream, from inside a stream poll on an
async worker thread. Blocking there while another task holds the guard
across an await is a deadlock, not a slow path, and that exact hazard had
already been tripped three times in that file.

**Countermeasure:** `try_lock` and count the misses; and the tap reports its
own cost, which is how the 50-minute run was cleared without a second run -
`3004 frame(s), 1ms total`.

## 17. A guard with a symmetric hole

**Instance:** `mech.rs` asserts every REPORTED mechanism has a counter. It
cannot assert the converse, and three switches that changed behaviour -
`peer_cache_mounts`, set in every CI run and responsible for seven
dispatches in eight; `fleet_cache`; `warm` - were counted nowhere and named
in no report. Two more, `serve_secrets` and `forward_agent`, decide what may
leave the machine at all.

**Countermeasure:** the mirror test. Every `REBUCK2_*` in `src/` must be
counted under its lowercased name or exempted in a list that carries a
reason. It found five the moment it ran.

## 18. A counter in the wrong process

**Instance:** `prefetch_broadcast` is incremented by `my_share`, which runs
on the WORKER, and was listed in the coordinator's `[wire] mechanisms` line.
`mech::APPLIED` is a per-process map, so the coordinator's report could
never see it however often the mechanism fired - and would have printed
`prefetch_broadcast=0 (never needed)` on every run of a mechanism working
perfectly.

The source-consistency guard cannot catch this. It asserts that every
reported name has an `applied("name")` call somewhere in `src/`, and
`worker.rs` has one. **Process boundaries are invisible to a grep over a
source tree**, and this is a distributed system whose two halves are
compiled from the same crate.

**Countermeasure:** removed from the coordinator's list, with the real
evidence named at the removal site - the worker's own
`prefetched N/N of my share (M announced)` line, where N equals M under
broadcast. No general fix: the honest rule is that a counter belongs to the
process that increments it, and a report may only list what its own process
counts.

## 19. A name that states the filter and hides the aggregation

**Instance:** the verdict line carried `mounts_ms=8527063` beside
`lead_ms=9033118`. Read straight off, that is *94% of all lead time spent
arming cache mounts* - a spectacular finding, and one I wrote down before
checking. The counter behind it is `driver.rs:949`:

```rust
if !caches.is_empty() {
    self.cache_lead_ms.fetch_add(ms, Ordering::Relaxed);
}
```

`ms` is the WHOLE lead. The condition is a filter on which leads count; the
value is not mount time at all. The true statement is the far duller *94% of
lead time sits in leads that happen to touch a cache* - which, with 359 of
414 leads touching one, is nearly a tautology.

The function's own doc comment said this correctly (`Lead time spent in leads
that named ANY cache mount`). Only the wire name lied, and the wire name is
what gets pasted into a table. **A metric is read at the width of its name,
not the width of its definition** - nobody greps `verdict` and then opens
`driver.rs`.

The tell was arithmetic, and it is worth keeping: mean mount-lead cost came
to 23.7s while the arms line right underneath said `cold p50 5677ms`. A mean
four times the median is possible, but here it meant the two lines were
measuring different things.

**Countermeasure:** renamed to `lead_ms_with_cache` / `leads_with_cache`,
which states the filter AND the aggregation and cannot be read as a duration
of anything. No general guard - the rule is that a field name must survive
being read alone, beside `lead_ms`, by someone building a table.

## 20. A test that supplies both sides of a seam

**Instance:** the prefetch consumer gate. `consumers_of(op)` counts how many
distinct workers have been sent an op, and refuses to pre-position anything
with fewer than two. In the field it recorded **0 acceptances, 14 refusals,
292 bypasses** - it has never once said yes.

It could not. Two production sites write the same digest two ways:

```rust
// place_subtree, storing what a worker will need:
pairs.insert((crate::store::sha256_hex(bytes), first));   // bare hex

// place_subtree, recording the terminal for the same subtree:
self.job_terminal.lock().await.insert(job, input.digest); // "sha256:<hex>"
```

and `consumers_of` compared them with `==`.

The existing test passes, and always would have:

```rust
pairs.insert(("shared".to_owned(), 1));
assert_eq!(d.consumers_of("shared").await, 3);
```

It invents `"shared"`, writes it, and reads it back. **A test that chooses
the spelling for both sides of a comparison cannot detect that the two real
sides spell it differently.** The function is correct in isolation; the seam
is where the bug lives, and the test carefully stayed off it.

This is the sibling of shape 18. There the boundary was a process; here it
is two call sites in one file. Both are invisible to a test that mocks both
ends, and both were found by reading what production actually passes rather
than what the test passes.

**Why only one site broke.** `op_by_worker` has three other readers - the
affinity term, the duplication metric, the placement scorer - and all three
are correct. Each of them re-derives its key by hashing the op bytes
(`sha256_hex(b)`), so it cannot help but produce the stored spelling.
`consumers_of` was the only caller handed a key from LLB metadata
(`Input.digest`) instead of computing one. **The odd one out was the one
that did not hash**, and that is a cheap thing to look for: when a table is
keyed by a derived value, any reader that receives its key rather than
deriving it is where the spellings can diverge.

**Countermeasure:** a second test that constructs each side the way its
production site does - `sha256_hex(bytes)` on the write, `format!("sha256:
{hex}")` on the read - with `assert_ne!` on the two spellings first, so the
test fails loudly if someone later unifies them and makes it vacuous. The
fix normalises both sides through `store::bare_digest`.

## 21. A field whose name is the noun and whose value is the exception

**Instance:** `placing` in the lead-phase split. It reads as "time spent
placing this lead", sits beside `waiting` and `building`, and the three sum
to the lead. Everything about the presentation says *this is the placement
share of the work*.

It is `offered_ms`, which is initialised to zero and stamped in exactly one
place - the DECLINE path:

```rust
// driver.rs, in the "a worker said no" branch only
st.offered_ms = st.started.elapsed().as_millis() as u64;
```

A lead accepted by its first candidate keeps zero for its whole life. So the
field is not the cost of placing, it is the cost of **re**-placing, and it
is silent for every lead that went smoothly.

I read it wrongly twice in one evening, in opposite directions:

- `placing 0s (0%)` became "placement is free, the dispatcher is not the
  problem". The conclusion happened to hold, but only by luck - the field
  cannot report placement cost, so a zero was never evidence about it.
- `placing 1872s (15%)` became "seeding is expensive because it rewrites
  every graph". Wrong mechanism entirely. The true reading is far more
  interesting: **seeding made workers refuse leads**, and 1,872 seconds went
  on decline-and-re-offer round trips.

The second misreading is the dangerous one, because it is actionable. It
points at optimising a graph rewrite that costs nothing, and away from
asking why a seeded lead gets refused.

**Countermeasure:** none general, and that is the honest answer - a summing
triple whose members are named after phases will be read as phases. The
specific fix is to say so in the line, and the discipline is the one that
caught it: when a number moves a lot, re-read the definition of the field
before explaining the movement. Both readings were of a real number
correctly computed.

## 22. A sum over concurrent things, divided by a wall clock

**Instance:** `building` is 4,917 seconds. The baseline builds the same
target in 201. I wrote "24.5x the build work one machine does", subtracted
the measured 1.8x duplication, and reported a 13.6x per-unit cost as the
project's largest open question.

`lead_ms` and its phase shares are summed over leads that RUN AT THE SAME
TIME - across six workers, and more than one per worker. This file already
depends on that fact: the leg identity is `lead_ms / occupancy = leg`, and
it checks out to 1%. I used the identity in one paragraph and contradicted
it in the next.

At the measured occupancy the same 4,917 lead-seconds are 3,602
worker-wall-seconds, so the ratio is 17.9x, and after duplication ~10x
rather than 13.6x. The same slip inflated a utilisation figure from 54% to
74%.

And 17.9x is still not a like-for-like comparison, because the baseline's
201 seconds is one wall clock over work buildkit parallelises across the
runner's cores. Both sides have to be in wall time or both in CPU time, and
nothing here measures the baseline's CPU time.

**What makes this shape dangerous is that it survives sanity checks.** Every
input was real, the division was arithmetically correct, and the result was
plausible - large enough to be interesting, not so large as to look absurd.
It sat at the top of the open-questions table.

**Countermeasure:** a summed duration is not a duration. Before dividing one
by anything, state what it is a sum over and whether those things overlap -
and if the denominator is a wall clock, the numerator has to be one too. The
tell here was free and ignored: an identity three paragraphs earlier already
said how much the leads overlap.

## 23. A comment that asserts what the code does not do

**Instances, all three found in one night:**

| comment | what the code does |
| ---------------------------------------------- | ------------------------------ |
| `share_of`: "a broadcast pulls each blob from a different peer rather than stampeding one" | the broadcast branch returns before `seeder_for` is reached |
| `usable_seeds`: "every graph naming that cache id wants it, on every machine" | hands it to `prefetch_image`, which the worker splits 1-in-N |
| `harvest_one`: "that cache was empty, so seeding it changes nothing" | emits the seed anyway; every worker pulls and unpacks it |

Each is a statement of intent that reads as a statement of fact. None is
lazy or vague - they are the best-written comments in their files, which is
exactly the problem. A vague comment gets checked; a precise, confident,
mechanism-naming comment gets believed.

**This is not shape 13.** There the knob is wired, documented and inert - the
code is dead. Here the code is alive and doing something else, and the
documentation is what is wrong. The failure is in the reader, and the reader
is whoever trusts the file's own account of itself.

It bit hardest where the codebase is strongest. This project records its
reasoning in comments rather than in commit messages or a wiki, deliberately
and to great effect - which means the comments are load-bearing, and
**nothing checks them.** `cargo test` cannot fail on a sentence.

**Countermeasure, and it is weak:** when a comment names a mechanism
(`seeder_for`, `prefetch_image`, a specific function), read the three lines
under it before believing it. That is a habit rather than a guard, and
habits are what this file exists because of.

The stronger form, where it is cheap: make the sentence a test. `share_of`'s
claim became `broadcast_takes_everything_but_its_own_share_first`, which
fails if the ordering is ever removed. A comment that can be executed stops
being a comment.

**Audited the rest of the strong claims** - every `so nothing`, `cannot
happen`, `never fires`, `guarantees`, `is impossible` in `src/` - and found
no further instance. One is worth a footnote rather than a correction:

> A bloom lies only in the safe direction, so a false positive here
> misplaces one subtree and a false negative is impossible.

True of the data structure and not quite true of the system: a worker that
has just fetched a blob is a false negative to every peer until its bloom is
gossiped. The codebase already knows - `note_gained` makes the filter
additive precisely so gossip can be prompt, because "30 seconds is longer
than the window in which a freshly-fetched share is worth anything to
anyone". So the window is bounded by design rather than absent, which is a
different sentence from the one in the comment and matters exactly when a
seed is being raced to six machines.

## 24. A trade-off judged on one side of the trade

**Instance:** cache-mount seeding, measured four times and written off three
times, on the leg alone.

| run | leg | CPU amplification |
| ---- | ------ | ----------------- |
| unseeded | ~1,065s | 9.2x |
| seeded | 1,396s+ | **6.5x** |

Seeding makes the fleet do **29% less total work** and take **31% longer**.
It fills cache mounts so a worker skips work it would otherwise repeat, and
it lengthens the critical path because a lead cannot start until its seed
arrives. Both effects are real, both were measured correctly, and reading
only the leg gives "seeding does not pay" - which is true of this fleet and
false of the mechanism.

The difference matters: on a throughput-bound fleet - many targets queued,
machines saturated - a 29% cut in total work is exactly what you want, and
the extra critical path is absorbed by the queue. The same code, the same
number, opposite verdict.

**This is not shape 6.** There the metric is a stand-in for the quantity you
actually want. Here both metrics are the quantity you want; they simply
disagree, because the mechanism *trades one for the other* and nothing
forced me to look at both.

What made it invisible for four runs was that the leg is the number this
file has always led with, for good reasons - it is what a user waits for.
CPU only became readable tonight, and the instant it did the verdict
inverted.

**Countermeasure:** before judging a mechanism, ask what it trades. If the
answer is "nothing, it is strictly better", that is a claim to check rather
than an assumption. Anything that moves work between machines, or between
now and later, is trading latency against throughput and needs both numbers
or neither.

## 25. The same misreading, twice, forty minutes apart

**Instance:** the coverage ledger writes results as `A vs B` without saying
which is which. I read `+lint-all`'s "88s vs 252s" as fleet-then-baseline,
called it the one target the fleet wins, then found the detail table saying
baseline 88s and fleet 252s - **2.9x slower** - and fixed it, with a note
about how easily a transposition survives because "the premise looked like
data".

Forty minutes later I read `+all-binaries`'s "262s vs 712s" the same way,
concluded the fleet wins 2.7x there, and built the evidence table for
principle 30 on it. The detail table for that run says `wall | baseline
262s | fleet 712s`.

**Having just written the countermeasure did not stop me applying the
misreading to the next row.** The fix I made was local - I corrected one
row and moved on - when the defect was in the FORMAT, which produces the
error afresh on every row a reader meets.

Worse, the second misreading was load-bearing in a way the first was not. It
became the positive example in a principle, the "proof" that large leads can
win, and the thing that made "the fleet is not slow, its leads are too
small" sound established rather than speculative.

**Countermeasure, and this time to the format rather than the instance:**
the ledger now writes "fleet 712s against a 262s baseline" - naming both
sides in every cell. A cell that cannot be read backwards cannot be
misread backwards. Correcting one row taught me nothing; the row after it
proved that within the hour.

## The common thread

Twenty-four of these twenty-five produced a GREEN result. Not one announced itself.

The discipline that caught them is the same every time: **find the
observation that differs between the world where it works and the world where
it does not, and check that one.** Wall clock rarely is that observation.
`placed` usually is.
