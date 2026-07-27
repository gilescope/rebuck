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

| run | wall | led | MERGED | result |
| ------------------------------- | ---- | --- | ------ | ------ |
| solo, 1 instance | 361s | 270 | - | PASS |
| pair, 2 instances, single-flight | 373s | +270 | **270** | PASS |
| pair, 2 instances, `CONTROL=1` | TBD | TBD | 0 | not yet run to completion |

The second instance led 270 vertices and adopted 270: it rebuilt **nothing**,
`abandoned=0`. Doubling the work cost +12s (3.3%).

Caveats, stated because the number flatters us:

- same-target is single-flight's **best case**. Real CI runs different shards
  where the shared prefix is a fraction. That measurement is still outstanding.
- green integration tests on the adopting instance is good evidence the adopted
  vertices were correct, but it is not a byte-comparison of artifacts.
- one box, two instances. Shared cores, disk and NIC is not a fleet. A win here
  is evidence, not proof, for two physical machines.

## Recipe that works

```bash
ISOLATE=1 \
BUILDKIT_FORK=$HOME/git/EarthBuild/buildkit \
EARTHBUILD_SRC=$HOME/git/EarthBuild/earthbuild \
  ./rebuck2/tests/load-earthbuild-examples.sh "+test-no-qemu-group1"
```

Prepend `CONTROL=1` for the uncoordinated arm. Omit `ISOLATE=1` only if you want
to reproduce the shared-tree failure deliberately.

## Open questions

- disjoint shards (`group1` vs `group2`) with `ISOLATE=1` - the case that
  actually resembles CI, and the one that answers "does my CI get faster"
- two physical machines rather than two instances on one box
- artifact-level proof that adopted vertices are byte-identical to built ones
- `run-groups.sh` does not set `ISOLATE=1`, so every group experiment run
  through it so far has been measuring the shared-tree failure
