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

## The common thread

Twelve of these thirteen produced a GREEN result. Not one announced itself.

The discipline that caught them is the same every time: **find the
observation that differs between the world where it works and the world where
it does not, and check that one.** Wall clock rarely is that observation.
`placed` usually is.
