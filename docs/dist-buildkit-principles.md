# dist-buildkit — principles

Why the design is the shape it is. Each of these was paid for: most arrived by
being wrong first, and the cost is noted where it is instructive.

Companion to [buildkit-plan.md](buildkit-plan.md) (what we are building) and
[buildkit-optimizations.md](buildkit-optimizations.md) (what we measured).

## 1. The grid must behave as ONE machine

The north star. N buildkitds building one logical build must produce what one
buildkitd would have produced. Everything below follows from this.

On one machine, `RUN apt-get update` executes once and every downstream vertex
sees the same apt state. On a grid without coordination, machine A gets the apt
state from 10:00 and machine B from 10:05, and the final artifact is stitched
from both — a build **no single machine would ever produce**. That is not a
slower build, it is a different one.

## 2. Single-flight is a CONSISTENCY mechanism, not an optimisation

It reads like a performance feature — "don't build the same thing twice" — and
measuring it that way produces the wrong conclusion. Measured, on an idle
2-worker fleet:

| face | verdict |
| ---- | ------- |
| latency | **loses** ~`T_xfer` for a follower that arrives with the leader |
| throughput | wins — one build instead of N |
| **consistency** | **required** — it is what makes (1) true |

We nearly shipped a "skip the lease when the fleet is idle" gate on the strength
of the first two rows. It would have bought a little latency by making the grid
non-deterministic. **A correctness mechanism cannot be gated on load.**

## 3. One canonical result per key — first writer wins

If we accidentally build the same key twice (a race, or a fail-open), the LATER
result is discarded and the earlier, published one is adopted. The build is
already paid for; keeping it would leave the grid multi-valued for one key, which
is exactly (1) violated.

Liveness never requires keeping *your own* bytes — only *some* bytes.

> Status: not yet implemented. Today we fail open and keep our own result, which
> protects liveness and quietly abandons consistency in the one case that matters.

## 4. Identity is what BUILDKIT matches on — not what we can compute

A lease key must be content-addressed AND machine-stable. Buildkit hands us
several keys per dep and they are not equivalent:

- **fast key** — the dep's own cache key (output >= 0). What buildkit actually
  matches on. Content-addressed for an image; `random:` for a local source,
  where it is per-run noise.
- **slow key** — a contenthash of the dep's RESULT (output `-1`). Present only
  when `ContentBasedHash` is set.

Rule: **use the fast key when it identifies the dep; fall back to the slow key
only when the fast key is `random:`; refuse when neither identifies anything.**

The slow key is a FALLBACK, not an extra ingredient. `RUN apt-get update` on a
fixed base is a cache hit on a second local run even though apt fetched different
bytes — the key is `f(base, command)` and never hashes the output. Mixing the
contenthash in as well poisons a key that already agrees across machines, over
bytes buildkit itself ignores.

Both halves of this were learned by getting them wrong: first by inheriting
`random:` (single-flight was INERT for every build with a `COPY`), then by
unioning the slow key in (vertices whose fast keys already matched still would
not merge).

## 5. Fail open, never fail wrong

No cross-machine identity => no lease => build locally, exactly as unmodified
buildkit would. Duplicate work is always correct. A wrong layer never is, and a
stall is worse than the duplicate work we set out to prevent.

Corollary: prefer keys that are OVER-specific to keys that are under-specific. An
over-specific key is useless (never merges); an under-specific key hands a
follower someone else's layer.

## 6. The coordinator is never on the data path

Layers travel leader -> follower, peer to peer. The coordinator arbitrates the
lease and nothing else. The test for this is deliberately blunt and hard to fake:
after the build, look at what is on the driver's DISK. If it is out of the data
path, the layer is simply not there. (Measured: 0 MiB.)

## 7. Determinism is the ceiling

A vertex's mergeability is bounded by its inputs' reproducibility. This is not a
limitation we can engineer away:

- **cache mounts** are machine-local mutable state. Buildkit content-hashes the
  mount as an input, so two machines hash different bytes. They cannot merge
  until the mount itself is shared (plan P3).
- **genuinely divergent inputs** cannot merge, by definition.

So measure `merged / (led + merged)` against what the workload can ACHIEVE, not
against 100%. A byte-reproducible target merges everything; a target with cache
mounts cannot. This is the house rule showing up as an engineering ceiling:
*same inputs -> same artifact*, or no dedup for you.

## 8. Prove it on a real graph, or you have proved nothing

The rule that would have saved the most time. Single-flight was **inert for every
real build** from the day it was written, and four green e2e tests said
otherwise — because not one of them had a `COPY`. Every rig used
`RUN … /dev/urandom … sleep`, whose only input is a base image: the one shape
where the bug cannot bite. The random-marker trick that made those tests
"decisive" is exactly what kept local sources out of them, because you cannot
salt a build with random content AND feed it real source files.

A synthetic workload agrees with whatever design produced it. Real graphs do not.
`rebuck2/tests/load-earthbuild-examples.sh` runs earthbuild's own examples and
found in one run what four tests had missed.

## 9. Instrument first, or you are guessing

We could not see whether single-flight engaged: the lease counters existed and
nothing surfaced them, so the e2e asserted on markers and passed. The first
honest look said `merged=0`.

When two machines disagree, the DIGEST tells you nothing — the pre-hash string
is what names the diverging component (`LeaseKeyDebugString`). Both bugs in (4)
were found by diffing that string, not by reasoning.

## 10. Baseline-then-bisect — the rig lies more than the product

Four rig bugs, and every one first presented as a product bug:

- `merged=0` — a warm daemon cannot collide; the solo run had killed the race.
- apt: `Network is unreachable` — no `NETWORK_MODE=cni`. Bisected by running the
  rig with the STOCK buildkitd, which failed identically.
- an impossible log (one marker printed, another absent, same code path) —
  `KEEP=1` left a container up, and the readiness probe passed against the STALE
  daemon, so the run tested the old binary.
- `Could not open file … (5: Input/output error)` — the host disk was full and
  the daemon died.

Before blaming the code, reproduce with a known-good reference (stock binary,
stock daemon) and move ONE variable.
