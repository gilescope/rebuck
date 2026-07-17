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

> Implemented: a released success becomes the key's canonical answer, and every
> later claimant adopts it (`Claim::Done`). Only a success is canonical — a
> failure drops the entry, or one machine's transient OOM would be cached for
> the fleet. Measured on `+examples-1`: led 15, merged 30 across a solo run plus
> two concurrent instances — each vertex built exactly once, grid-wide.

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

A vertex's mergeability is bounded by its inputs' reproducibility — but the bound
is far looser than it first appears, and we twice mistook our own bugs for it.

Merging needs two things, and we learned them one failure apart:

1. **The KEY must agree.** It is `f(op, deps)` and does not hash the output
   (principle 4), so a vertex is key-mergeable whenever buildkit itself would
   call it a cache hit — including `RUN apt-get update`, whose output is wildly
   non-deterministic and whose key is perfectly stable. Measured on
   `+examples-1`: **14 of 14 keys agree**, cache mounts and unpinned apt
   included.
2. **The ADOPTION must be sound**: the published layer must BE the whole
   result. A cache-mounted vertex fails this — bazel keeps its real output tree
   in the mount and leaves only a symlink in the layer, so a follower adopting
   it gets a dangling result (measured: `readlink -f ./bazel-out` -> nothing).
   Key agreement is necessary, not sufficient. Such vertices are excluded from
   the lease (`hasCacheMount`) until mounts are fleet-shared (P3).

The identity bound proper is narrower still: an input whose IDENTITY differs
across machines cannot merge. In practice that means a local source with no
content key at all — we refuse the lease there (principle 5) rather than invent
one.

Twice we blamed reproducibility for what was our own key derivation:
`random:` inheritance, then unioning the slow key in. Both times the vertices
were mergeable all along. **Before concluding "this build is too
non-deterministic to merge", check that the key derivation is not the thing
diverging** — the pre-hash string says which, in one run.

Where a build IS non-reproducible, note the tension with (2): the lease is then
carrying MORE, not less. It is what makes one machine's result canonical for the
whole grid, and so supplies the consistency the build itself lacks. The less
reproducible the build, the more the lease is carrying.
