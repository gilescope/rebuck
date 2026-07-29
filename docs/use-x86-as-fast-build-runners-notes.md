# Using x86 boxes as fast build runners - notes

Field notes from getting a two-instance dist-buildkit measurement to produce an
honest number on a NixOS x86 box. Most of what follows is about the *rig*, not
the product, because every failure here presented as a product failure first.

Goal: **builds faster, not at the cost of consistency.** Those are separate
properties with separate mechanisms, and conflating them wastes days.

## The three levers, and which one is which

| lever             | speed comes from                  | consistency cost    |
| ----------------- | --------------------------------- | ------------------- |
| cross-run cache   | 137s -> 4s warm (measured, buck2) | none                |
| fleet parallelism | N machines on **disjoint** work   | **the risk**        |
| single-flight     | avoids redoing shared work        | none - it's the fix |

Single-flight is not the accelerator. Parallelism is. Single-flight is what
makes parallelism safe to use, and per
[dist-buildkit-principles.md](dist-buildkit-principles.md) it can *cost* a
follower ~`T_xfer` in latency. Measuring it as a speed feature will keep
disappointing you while it is doing its job.

## What blocked us, in the order it bit

Each of these produced `led=0 MERGED=0` or a bare `Canceled`, i.e. a result that
looks like a real negative rather than a broken rig.

### 1. Container -> host is not routable (the expensive one)

The agents run on the host; buildkitd runs in a container. On a host with a
default-deny firewall (NixOS ships one active) `docker0 -> host` is dropped.
The host itself reaches `172.17.0.1:<port>` fine, which makes it look like a
container bug.

Consequence: `BUILDKIT_SINGLEFLIGHT_URL` unreachable, single-flight silently
inert, `led=0 MERGED=0` in **both** arms of an A/B. A 2349s SF-ON run on
2026-07-17 was read as "single-flight does nothing on a real graph". It was a
dead wire. Ten days of belief rested on it.

Fix, narrow (preferred - keeps the ports off the LAN):

```nix
networking.firewall.interfaces."docker0".allowedTCPPorts = [ 5090 5091 5092 ];
```

Do **not** reach for `networking.firewall.trustedInterfaces = [ "docker0" ]`
without meaning it: it grants every container access to every host service
listening on `0.0.0.0`, which on a box whose job is executing other people's
build steps is a poor trade for three ports. The global
`networking.firewall.allowedTCPPorts` also works but opens the ports on *every*
interface, LAN included (verified: `http=200` from another machine).

### 2. `host.docker.internal` - the name is not the route

Docker Desktop auto-provides that name; Linux does not. `--add-host
host.docker.internal:host-gateway` fixes resolution and **nothing else**. It is
easy to read that fix as having restored reachability - it has not, if a
firewall is in the way. Only an end-to-end fetch proves both halves.

### 3. Single-flight degrades in silence

An unreachable coordinator does not fail the build. buildkitd starts, builds
every vertex correctly, coordinates with nobody, and reports `led=0 MERGED=0` -
byte-identical to a deliberate `CONTROL=1` run. There is no way to tell a broken
SF-ON run from an honest negative by looking at the numbers.

The load rig now asserts reachability from *inside* each daemon before it
believes any number. Do not remove that check to save 5 seconds.

Note the obvious fix is wrong: do not make an unreachable agent fatal in the
daemon. Single-flight is best-effort by design and a fleet must survive its
coordinator blinking. The defect is the *silence*, not the *tolerance*.

### 4. Two instances, one working tree (`ISOLATE=1`)

The rig defaults to running both instances from the same `$EARTHBUILD_SRC`.
earthbuild writes into that tree as it builds (`SAVE ARTIFACT AS LOCAL`, git
plumbing touching `.git/index`), so two concurrent instances corrupt each
other's context. Two distinct symptoms, both undiagnosable:

- the build dies as a bare `Canceled` with no error line anywhere
- the contexts genuinely differ, so contenthash yields different lease keys and
  single-flight **correctly** declines to merge

That second one is why a disjoint-shard pair measured `MERGED=4` of 116. The
mechanism was fine; it was being fed two different builds.

`ISOLATE=1` gives each instance a detached worktree at the same commit. This is
**more** faithful to real CI, not less - two runners never share a working
directory, they each check out the same commit. The shared-tree default is the
artificial mode, and it is the wrong polarity for a flag that silently produces
plausible-but-wrong numbers.

Incidentally proves lease keys do not embed absolute paths: the worktrees sit at
different paths and still merged 270/270.

### 5. A test suite cannot answer a build-speed question

`+test-no-qemu-group1/2` are earthbuild's integration suites: they need
docker-in-docker, hammer package CDNs, and are flaky by construction. Running
two concurrently measures their flakiness, not your build. Three attempts
produced three different environmental failures and zero wall-clock data.

If the question is "is the fleet faster", benchmark something deterministic.

### 6. Errors that look fatal and are not

`ERROR: Cannot connect to the Docker daemon at unix:///var/run/docker.sock`
appears **30 times in a passing vanilla run**. Several tests assert failure. Do
not diagnose from a grep for `ERROR`.

Similarly, apk reporting `no such package` for everything usually means the
*index* fetch failed upstream (`temporary error (try again later)`), not that
packages are missing. The error names the symptom, not the fault.

## Baseline first, always

Every one of the above was resolved by moving one variable against a known-good
reference, never by reasoning from the logs:

| step | result | eliminates |
| ------------------------------ | -------------- | ------------------------ |
| stock earthly, 1 instance | PASS | the box, dind, the suite |
| our buildkitd, 1 instance, cold | PASS 329s | our daemon |
| our buildkitd, 2 instances | FAIL `Canceled` | isolates concurrency |
| ... + `ISOLATE=1` | PASS | shared working tree |

The vanilla run is the highest-value 5 minutes in this whole exercise. It killed
a docker-in-docker theory that was about to cause a NixOS rebuild for nothing.

## Measurements

Target `+test-no-qemu-group1`, cold daemons, 32-core box, 62 GB RAM.

All runs `ISOLATE=1`, all PASS.

| config              | solo | pair | MERGED |
| ------------------- | ---- | ---- | ------ |
| single-flight on    | 361s | 373s | 270    |
| `CONTROL=1` (no SF) | 328s | 342s | 0      |

The second instance led 270 vertices and adopted 270: it rebuilt **nothing**,
`abandoned=0`. Single-flight works exactly as designed.

**And it is not faster.** The pair costs 373s coordinated against 342s
uncoordinated. Both configs scale almost perfectly (2x the work for +12s and
+14s respectively) because a 32-core, 62 GB box running two jobs is nowhere near
contended. Coordination adds a roughly constant ~31s on top and buys no
throughput back, because there was no throughput to reclaim.

This is [dist-buildkit-principles.md](dist-buildkit-principles.md) §2 measured
rather than argued: latency loses ~`T_xfer`, throughput wins only on a contended
fleet, consistency is the actual product. It is also the trap
[buildkit-optimizations.md](buildkit-optimizations.md) already named - "the one
thing an idle 2-worker rig cannot show" - so this rig cannot answer the
throughput question by construction.

**Do not quote the 9% as a result.** SF-ON solo measured 329s and 361s on two
runs: a 10% spread, the same size as the effect. With n=1 per cell the only
defensible statement is *no detectable difference*. Three to five repeats per
cell are needed before either number means anything.

Further caveats, stated because the merge count flatters us:

- same-target is single-flight's **best case**. Real CI runs different shards
  where the shared prefix is a fraction. That measurement is still outstanding.
- green integration tests on the adopting instance is good evidence the adopted
  vertices were correct, but it is not a byte-comparison of artifacts.
- one box, two instances. Shared cores, disk and NIC is not a fleet.
- the `CONTROL=1` pair failed on its first run (both instances cancelled, no
  failing target) and passed on retry. Bare `Canceled` with nothing failing is
  flake on this rig, not signal - do not build a story on a single failure.

## Recipe that works

```bash
ISOLATE=1 \
BUILDKIT_FORK=$HOME/git/EarthBuild/buildkit \
EARTHBUILD_SRC=$HOME/git/EarthBuild/earthbuild \
  ./rebuck2/tests/load-earthbuild-examples.sh "+test-no-qemu-group1"
```

Prepend `CONTROL=1` for the uncoordinated arm. Omit `ISOLATE=1` only if you want
to reproduce the shared-tree failure deliberately.

## The merge count is already optimal - the graph is the limit

`merged=32` looked low against `led=352`, so the obvious question is how many
*ought* to merge. Measured directly rather than inferred:

| target (solo, cold)                 | led |
| ----------------------------------- | --- |
| `+earthbuild-integration-test-base`  | 32  |
| `./tests+base`                       | 32  |
| `+test-no-qemu-group1`               | 270 |

`./tests+base` is what BOTH groups derive from, so its vertex count IS the
shared prefix - and it adds nothing above the stem. So 32 is the complete
intersection, and single-flight adopts all 32. There is no bug above the stem
and no divergent lease key; the merge logic is doing everything available to it.

Do not repeat the mistake of estimating the intersection as
`led(g1) + led(g2) - led(pair)`. Both group builds are ~50% flaky (bare
`Canceled`), so the subtraction is noise-dominated, and an early estimate of
"~188 ought to merge" was wrong by 6x. Measure the shared target directly.

The ceiling is therefore structural: two shards share ~12% of their vertices
(32 of ~270) because every test target is its own `FROM`/`RUN`. Merging more
requires earthbuild's graph to share more - an architecture question, not a
coordinator one.

Combined with the wall-clock finding above, that is the honest verdict for this
workload: a small shareable fraction, adopted perfectly, in a form whose
adoption costs about what building costs.

## Open questions

- **repeats.** Every cell above is n=1 against ~10% run-to-run variance. Nothing
  here supports a speed claim in either direction until that is fixed. Cheapest
  useful next step.
- **a contended fleet.** Single-flight's only wall-clock case is when the box
  cannot absorb the duplicated work. Two jobs on 32 idle cores is the opposite
  of that. Either raise the job count well past capacity, or cap parallelism.
- disjoint shards (`group1` vs `group2`) with `ISOLATE=1` - the case that
  actually resembles CI, and the one that answers "does my CI get faster"
- two physical machines rather than two instances on one box
- artifact-level proof that adopted vertices are byte-identical to built ones
- `run-groups.sh` does not set `ISOLATE=1`, so every group experiment run
  through it so far has been measuring the shared-tree failure

## If the goal is "faster"

Single-flight is not the lever, and this rig now shows why. Reach for cache
reuse and for putting **disjoint** work on more machines; keep single-flight
because it is what makes the second of those safe, not because it pays back on
wall clock. Sell it internally as consistency, or it will keep looking like a
regression to anyone holding a stopwatch.
