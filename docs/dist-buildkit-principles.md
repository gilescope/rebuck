# dist-buildkit — principles

Why the design is the shape it is: claims about what the PRODUCT must be, each
paid for, most by being wrong first.

Deliberately not here: how to test it, how to debug it, how to avoid fooling
yourself. Those are working practice, equally true of any project, and live
where they are used — the "Measured, not guessed" section of
[buildkit-optimizations.md](buildkit-optimizations.md), and the header comments
of the rigs in `rebuck2/tests/`, which record the specific ways each one lied.

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

A vertex's mergeability is bounded by its inputs' reproducibility. This is a
property of the system, not a limitation we can engineer away:

- **cache mounts** are machine-local mutable state. Buildkit content-hashes the
  mount as an input, so two machines hash different bytes. They cannot merge
  until the mount itself is shared (plan P3).
- **genuinely divergent inputs** cannot merge, by definition.

A byte-reproducible target merges everything; a target with an unpinned
`apt-get update` or a cache mount cannot, however good our key derivation gets.
The house rule — *same inputs -> same artifact* — is therefore also the
throughput and consistency ceiling, not just good hygiene.

Note the tension with (2): where a build is NOT reproducible, single-flight is
what supplies the consistency it lacks, by making one machine's result canonical
for the whole grid. The less reproducible the build, the more the lease is
carrying.
