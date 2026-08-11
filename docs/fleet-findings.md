# Fleet findings

Everything below was measured, and much of it overturned something an earlier
section believed.

**Order.** This began newest-first and is now oldest-first: the early
sections are how the mechanism came to exist, and everything from *"How much
of the Earthfile has actually been through this"* onward was appended at the
end as it happened. The header claimed the opposite for about thirty entries.

**Where the numbers come from.** The early ones are
`rebuck2/scripts/fleet.sh` on one machine. Everything current is
`.github/workflows/earthfile-fleet-multi.yml` - six separate runners, a
baseline and a fleet leg in the same run - and `rebuck2/scripts/seed-check.sh`
for anything that only needs one daemon.

**If you read five things**, read these:

| entry | why |
| ----- | --- |
| *Current state of belief* | what is established, open and retracted, in three tables |
| *Prefetch, once it worked: 1773s to 1240s* | the first change to move the headline by more than noise |
| *`-balance`: 1240s to 1050s* | and no machine idle; the fault was a preference with no brake |
| *Building is a quarter of lead time* | and the split that made every mechanism rankable |
| *The workload sets the ceiling* (principle 19) | most "the fleet is slow" numbers are Amdahl |

**Read `docs/how-this-lies.md` too**, if you are going to trust any number
here. Seventeen ways this system has produced a confident wrong answer, four
of them found in a single day.

It lived in `src/proxy.rs` as a module comment until it reached 1342 lines --
a third of the file, and a chronological log rather than documentation. The
code kept the invariants a maintainer needs; the reasoning and the numbers are
here.

Many of these claims are mechanically checked. `rebuck2/scripts/fleet-check.sh`
asserts the structural ones - where work was placed, whether the bytes match,
whether a build survived something being destroyed. It deliberately does not
assert wall clock: those numbers move for reasons that have nothing to do
with this code, and a suite that fails on a busy laptop gets switched off.

**It has also never passed in CI**: 96 failures and no successes in the last
hundred `fleet.yml` runs, and the run log does not contain the step's output,
so nobody has read a failure. Filed in `~/git/gilescope/rebuck-nits.md`. Treat
the script as documentation of intent until that is fixed.

## Current state of belief

Added after retracting the same class of claim three times in one session.
A document that records everything it ever believed needs a page saying what
it believes NOW, or the retractions get re-derived by whoever reads the
confident version first - which has happened to me, in this file, twice.

**Established.** Measured, and nothing since contradicts it.

| claim | evidence |
| ----- | -------- |
| The workload sets the ceiling, not the fleet | Amdahl ceilings of 3.1-6.0x on targets with 3-5 branches |
| A daemon does not re-fetch what it built | `reserve-check.sh`: 1870 KiB on solve 0, zero for five more, chained or not |
| Prefetch never worked for subtree results | 412 of 412 failures; a bare digest has no manifest URL |
| Affinity never scored parent images | `warmth` counted ops and mounts; a cut subtree carries neither |
| A 20-op dispatch floor is harmful | ~5x slower overall while every per-lead number improved |
| Building is roughly a quarter of lead time | 4,407s of `took Nms` against 14,812s of `lead_ms` |
| A cap inside a cap turns slow into failed | worker `timeout 3000` under a 150-minute job |
| Fixing prefetch took 30% off the fleet leg | 1773s -> 1240s, same target, same 412 solves |
| Choosing a worker costs nothing | `placing 0s (0%)` - no offer was ever refused |
| Queueing WAS the largest cost | `waiting 6583s (65%)`; trading warmth against queue depth cut it to 3670s (45%) |
| Spreading beats concentrating, and costs what it should | 1240s -> 1050s, no idle machine, `building` up 1004s against `waiting` down 2913s |
| **Building is now the majority term** | 4521s (55%) - what `-imports` and `-bcast` aim at |
| The instruments are not the problem | the status tap: 1ms across 3004 frames |

**Open.** Believed for a reason, not measured.

| question | why it is open |
| -------- | -------------- |
| How much of the fleet's traffic crosses a wire | `SERVED_BYTES` mixes loopback with peer serving; `SERVED_LOCAL_BYTES` exists and has never reported |
| Whether the fleet repeats itself, and by how much | the coordinator reports 1.1x; the per-worker figure has never printed |
| Whether placing a lead near its parent pays | `-balance-imports` in flight; it had to follow `-balance`, not precede it |
| What made the reference run take 50 minutes | not the tap, which costs 1ms. Still unexplained |
| What a 14-way target does with the fixes | it ran at 186s/525s BEFORE them - see the correction |

**Retracted.** Written here confidently and wrong. Left in place with the
correction attached, because a deleted mistake gets made again.

| claim | what was wrong |
| ----- | -------------- |
| "A lead costs what it fetches" | bytes served correlate with lead duration at r = 0.06 |
| "24.7 GiB moved across the fleet" | `SERVED_BYTES` is bytes served, not bytes fetched |
| "...therefore it was loopback" | `upstream: None` says where a registry FETCHES, not who it SERVES |
| "93x re-served" | fleet-wide served divided by one machine's distinct - two different registries |
| `worth_spreading`, a worker-count cap | 162 machine-seconds read as wall clock |

The pattern in every retraction is one shape: **a real counter, correctly
incremented, answering a question I was not asking.** Before building on a
number, establish what it is a number OF.

## Both lifts work across a real network

Verified locally first, which proves the wiring and not the topology. With
the peer on the other machine and everything forced away from home:

```text
secret   placed {peer1: 6}   failed 0
ssh      placed {peer1: 4}   failed 0
```

Each exec asserts the thing it needs - the secret's VALUE, and that
`ssh-add` can reach an agent - so finishing means the remote machine really
got them, not that it started.

Worth being plain about what the second line is. An ssh agent on this
laptop was used by a build running on a different computer, over a
connection this laptop opened outbound. That is a genuinely useful
capability and a genuinely large amount of trust: for the length of that
build, the other machine could have asked the agent to sign anything at
all. It is off by default and it should stay a decision someone makes
deliberately, per fleet, knowing who runs the peers.

## An ssh mount travels too, and it is the sharpest one

`RUN --mount=type=ssh` is stock buildkit and common in builds that clone
private repos, so unlike the host bind this exclusion is worth lifting.
Same shape as secrets: serve `moby.sshforward.v1.SSH` over the session we
already open, backed by this process's `SSH_AUTH_SOCK`.

```text
REBUCK2_FORWARD_AGENT=1   placed {peer1: 4}   failed 0
unset (the control)       placed {}   excluded: SshAgent x4
```

The exec runs `ssh-add -l; test $? -ne 2`, so a build reaching no agent
fails rather than passing quietly. Finishing means the peer talked to our
agent.

Two conditions gate it, because either alone is a lie: the operator has to
ask, AND there has to be an agent. Advertising the service with no socket
behind it makes the peer wait on a call that cannot succeed.

And it deserves more hesitation than a secret. A secret is a VALUE - handing
one over gives the peer that string and nothing else. An agent is a
CAPABILITY: while the build runs, whoever holds the socket can sign anything
they like with your key, and nothing in the protocol constrains what.

## A cache mount need not ground one either

Excluded because it is daemon-local state. True, and not the point: a cache
mount is not shared between daemons even without a fleet. It is not in the
cache key, gc drops it, and it does not survive a restart. A build whose
OUTPUT depends on what is in one is already non-reproducible on a single
machine - dispatch does not make that worse, it finds it sooner.

So a peer builds with its own, colder cache mount:

```text
REBUCK2_PEER_CACHE_MOUNTS=1   placed {home: 4, peer1: 4}   outputs identical
unset (the control)           placed {}   excluded: CacheMount x8
```

Byte-identical to a one-machine baseline, which is the assertion that
matters: the colder cache changed nothing, exactly as the contract says.

Off by default anyway. "Already broken" is a reason to allow it, not a
reason to assume nobody depends on it.

## A secret no longer grounds a subtree

The oldest exclusion, and the one that capped earthly at 1 solve in 12. A
secret mount names an ID the daemon resolves by calling back over the
client's session; a dispatched solve had no session, so the peer had nobody
to ask and buildkit refused with `no active sessions`.

`buildkit-session` gives the peer somebody. Eight builds whose exec mounts a
secret, four local slots:

```text
REBUCK2_SERVE_SECRETS=1   placed {home: 4, peer1: 4}   failed 0
unset (the control)       placed {}                    excluded: Secret x8
```

The exec is `test "$(cat /run/secrets/probe)" = the-value`, so a wrong or
missing secret is a non-zero exit and a failed build - the four dispatched
builds got the right value, they did not merely start. And the control shows
the gate holds: without the flag nothing leaves, and the reason names the
mount.

The value comes from the PROXY's environment, not the peer's. That matters:
the proxy runs beside the client, so its environment is far likelier to
match than a remote worker's, and the fleet does not have to be provisioned
identically for the answer to be right. It is off by default because it is
the one thing here that moves a user's credential off their machine, and no
amount of scheduling benefit makes that a decision to take on their behalf.

Lifted per GRAPH, not per capability. Being able to serve secrets in general
is the wrong question: an earthly build mounts `earthly_debugger_settings`,
whose value earthly generates per build and keeps in its own internal store -
no environment holds it, and earthly's own provider refuses it by name. So
the check is "can we serve every secret THIS graph asks for", all or none:

```text
proxy can resolve it     placed {home: 4, peer1: 4}
proxy cannot (client can)  placed {}   excluded: Secret x8
```

Without that, lifting on the general capability would offer eleven solves in
twelve that are certain to fail, and fail-open would rebuild each at home
having paid for the trip.

Two bugs on the way, both the same shape as ever. The first version of the
unresolvable test set the variable to an EMPTY string, and `env::var` returns
`Ok("")` for that - so the check called it resolvable, the test dispatched
anyway, and it looked like the feature was broken when the test was. Empty is
now treated as unset in both the check and the provider: a variable holding
nothing is a provisioning mistake, and serving it turns that mistake into a
build failure on someone else's machine.

And a correction: serving `sshforward` would NOT unlock earthly's two socket
mounts, which I claimed it would. `earthly_interactive` is served by a
handler inside the earthly process (`build_cmd.go:476`) rather than by an ssh
agent, so forwarding ours satisfies nothing. Only proxying the client's own
session reaches it.

Still excluded, deliberately: cache mounts, ssh sockets and host binds. Each
needs a different service, and lifting them together would be assuming three
things from evidence about one.

The shapes those wrong turns took, collected with their countermeasures, are
in [how-this-lies.md](how-this-lies.md) - nine of the ten produced a GREEN
result.

Recurring theme, stated once so the sections below do not each have to: nearly
every wrong turn recorded here was a plausible cause accepted without a
control. The ones that cost most were a metric that improved for an unrelated
reason, a check that always returned the same answer, and three consecutive
misattributions of the same cost.

rebuck2 in front of a buildkitd, changing nothing.

Point earthly at this instead of the daemon and every `SolveRequest`
passes through our hands. It forwards all of them unaltered — this
version dispatches NOTHING — and reports what it would have offered.

That order is deliberate. We do not yet know whether real earthbuild
shards contain subtrees worth shipping: principle 11 says a chain rooted
at `FROM <registry image>` is the best possible handover, and
[`crate::dispatch::analyse`] can now count them, but counting them on
invented graphs proves nothing. A proxy that only measures answers "is
there anything here to dispatch" before a line is written to exploit it,
and it costs one hop.

Interposing rather than asking earthbuild for the graph is what keeps
this in one repo: the Control service is a stable, versioned wire, and
we already speak it.

## MEASURED: this IS the right layer, once it serves the gateway too

An earlier round concluded the opposite, and was wrong. `Control.Solve`
really does arrive with no definition — but the graph was not missing,
it was on the other service. A buildkit client drives its build through
`LLBBridge` created with `NewLLBBridgeClient(c.conn)`: the SAME
connection, a second service. The `Unimplemented` blamed on session
relaying was this proxy not offering it.

Measured, with a real `buildctl` through a real daemon:

```text
[proxy] solve ...: NO definition on the wire (frontend="")
[proxy] GATEWAY solve: 3 ops, 0 cuts >= 4, 0 free-frontier
```

Three client shapes, and only the middle one is invisible:

| client | where the LLB is |
| ------------------------------- | ---------------------------- |
| raw LLB (our own solve, testkit) | `Control.Solve`, definition set |
| frontend by NAME (`--frontend dockerfile.v0`) | nowhere — the frontend runs INSIDE the daemon |
| client-built LLB (earthly, `buildctl build < llb`) | `LLBBridge.Solve` — **here** |

The invisible one costs nothing: if no graph crosses the wire there is
no graph to dispatch, and the daemon was always going to build it alone.

## First real measurement: earthly, end to end

A real `earthly +test` on a three-target Earthfile, through this proxy,
against earthbuild's own buildkitd. It succeeded, and it said:

```text
gateway solves : 6
ops per solve  : min 3 median 9 max 9
sources        : 6 registry, 0 local, 0 other
platforms      : {"linux/arm64"}
repeated ops   : 31 (73% seen again in a later solve)
distinct graphs: 3 of 6 solves (3 identical RESENDS)
overlap/solve  : [(0,3), (2,6), (5,9), (9,9), (6,6), (9,9)]
```

**The 73% is not what it looks like, and the first reading of it here was
wrong.** Half the solves are byte-identical RESENDS of a graph already
sent - the client driving the API, not work being shared. Read as
overlap it says routing whole Solves would duplicate three quarters of
the build, which would have been a conclusion drawn from an artefact.

What the three DISTINCT graphs show is nesting: 3 ops, then 6 of which 2
are already seen, then 9 of which 5 are. Each solve extends the last, so
genuine overlap is 7 of 18 ops - about 39%, not 73%.

Nesting is a sharper result than repetition would have been, and it
points the other way from the first note:

- **Per-Solve routing is worse than the raw number suggested.** The
  graphs are not merely overlapping, they are cumulative - the last
  solve CONTAINS the earlier ones. Handing solve #3 to another machine
  asks it to rebuild everything solve #2 just did.
- **The natural unit is the INCREMENT between successive solves**, which
  is a subtree. The client hands us its subdivision already, one layer at
  a time; we do not have to infer a cut, only diff.

Caveats: three targets is not a shard, and this Earthfile has no `COPY`
from the build context, which is why `local` sources are zero. A real
repo will not look like that. What this run establishes is that the
instrument works - and that a summary percentage was one decomposition
away from being a wrong answer.

## Second measurement: shape decides everything

The first Earthfile was a CHAIN (`test` -> `build` -> `deps`), which is
the worst case and was mistaken for a general result. A fan-out - four
independent targets over one shared base, with a `COPY` from context -
says something quite different:

```text
gateway solves : 12      distinct graphs: 12 of 12 (0 RESENDS)
ops per solve  : min 4 median 5 max 17
overlap/solve  : [(0,4), (1,5), (1,5), (1,5), (1,5), (1,17), ...]
sources        : 12 registry, 12 local
repeated ops   : 13 (13%)
```

**Each independent target arrives as its OWN gateway Solve, sharing
exactly one op with everything before it** - the common base. Overlap
falls from 39% on the chain to 13% here, and no graph is re-sent.

So overlap is a property of the BUILD SHAPE, not of the client. A chain
yields cumulative graphs and nothing worth routing; a fan-out yields
independent units on a plate. Real multi-target repos - the ones dispatch
exists for - are fan-outs.

That is strong for routing whole Solves, and it is the third verdict this
measurement has produced on the same question. Worth stating plainly: the
first two were drawn from one toy graph, and the honest lesson is that a
single build shape cannot price a mechanism.

## The context is carried by whoever proxies the session. Measured

Every cut in the fan-out reports a NON-free frontier, because `COPY` puts
a `local://` source in every graph. The question that matters is what
that costs, and it is not a matter of opinion:

| build context | session bytes, client -> daemon |
| ------------- | ------------------------------- |
| 16 bytes | 1 KiB |
| 32 MiB | 32,812 KiB |

The context flows over the session, so it flows through the proxy. A
relaying coordinator IS on the data path for context - not for layers,
which still go peer to peer, but for every byte of the repository a
builder needs. With N peers each needing it, N times.

**This reconciles the mechanism argument, and not in the direction the
previous measurement suggested.** Per-Solve routing looked strong because
a fan-out hands over independent Solves; but every one of those Solves
wanted the context, so routing any of them moves the repository through
the coordinator. The free-frontier requirement built for subtree dispatch
turns out not to be a nicety - it is the ONLY shape that dispatches
without putting the coordinator on the data path, which is to say it is
what principle 6 actually requires.

So the options are now concrete rather than architectural taste:

- dispatch only subtrees whose frontier is registry digests (free, §6
  intact, but a `COPY` anywhere in the chain disqualifies it)
- relay the context and accept being on the data path for it
- give the peer its own session to the client, which the client has no
  reason to offer and no protocol to be asked with
- make the context itself content-addressed and fetchable peer to peer,
  which is the mesh's existing job and the only option that both
  dispatches `COPY`-bearing work and keeps §6

The last one is worth the most and is not built.

## Pointing earthly at a proxy, which is not obvious

Earthly MANAGES buildkitd when it thinks the address is local, and
`containerutil.IsLocal` matches the literal strings `127.0.0.1`,
`localhost` and `::1`. So a loopback address spelled differently reads as
remote and it connects instead:

```yaml
global:
  buildkit_host: tcp://[0:0:0:0:0:0:0:1]:11234   # ::1, expanded
  tls_enabled: false
```

The upstream must be earthbuild's OWN `earthbuild/buildkitd`, not
`moby/buildkit`: earthly asks for an exporter named `earthly` that only
its fork has. Stock buildkit gets as far as the build and then says
`exporter "earthly" could not be found`.

## MEASURED: a gateway Solve cannot simply be routed

Routing gateway Solves to a second daemon gets a long way and then
fails on something structural:

```text
NotFound: forwarding Solve: no such job ow8s6ghu1knxv6jox461m0xmk
```

The gateway conversation is scoped to a JOB, created by `Control.Solve`
on one daemon. A peer never saw that call, so it has no such job and
cannot accept a gateway Solve under its id. The build id in the header
is not a name we can forward; it is a handle into one daemon's state.

Two earlier measurements said the same thing from different angles and
this completes them: refs are daemon-local (eleven `read_dir` calls
follow eleven solves), and now jobs are too.

**So the peer is reached through `Control.Solve`, not the gateway.** The
shape that works is the one the dedup line already had a name for:

1. the proxy solves the portable graph on a peer, via its own
   `Control.Solve`, exporting the result to the mirror
2. the client's gateway Solve is then answered on peer 0 with a graph
   that merely IMPORTS that image
3. peer 0 fetches content instead of building, and returns a ref that
   belongs to it - so `read_dir` and `return` work unchanged

That is adoption, not forwarding, and it is what `adoptLeaderResult`
does for the dedup line. The routing code below is kept because
everything except step 2 is right: peers, ref affinity, portability
rewriting and placement all stand.

## MEASURED: a sessionless peer cannot reach Docker Hub

Adoption works - a peer builds a portable graph through its own
`Control.Solve` and publishes the result - but the peer then tried to
pull its BASE image and said:

```text
error="no active sessions" host=registry-1.docker.io
panic: invalid memory address or nil pointer dereference
  solver/llbsolver.(*resultProxy).wrapError bridge.go:288
```

Registry auth travels over the session, and a peer has none - that was
the whole point of rewriting `local://` away. So a graph is only truly
portable when EVERY source is something the peer can fetch unauthenticated,
and `docker-image://docker.io/...` is not that, even for a public image.

Principle 9 says this too, and says it about exactly this: the origin
registry is a fallback, not a data path; fetch once into the fleet and
serve peer to peer. The rewrite has to cover base images as well as
contexts - both become references into the mirror, and then a peer needs
no session, no credentials and no upstream at all.

Also worth knowing: this buildkit PANICS rather than returning the
error, so the failure arrives as a dead daemon rather than a failed
build. A fleet must treat a peer that stops answering as a decline, not
wait on it.

## IT DISTRIBUTES

A real `earthly +all`, two daemons, and one of the build's solves ran on
a machine the client never spoke to:

```text
[proxy] base docker.io/library/alpine:3.20@sha256:... mirrored as .../rebuck2/base:9ae21ebe
[proxy] adopted from peer 1: docker-image://.../rebuck2/adopted:c3eb00fb
[wire]  solves routed  : 1 to other daemons
peer 2 cache          : Total 13.73MB
```

The chain, all of it measured into existence rather than designed up
front: the graph arrives at the GATEWAY; its context is published as
content by the daemon that holds the session; its base images are
copied into the mirror by the daemon that holds the credentials; the
rewritten graph names nothing but content; a peer builds it through its
own `Control.Solve` and publishes the result; and the client's solve is
answered here with an import, so the ref it gets back belongs to the
daemon holding its job.

Solves whose sources were not all mirrored yet were NOT sent - they
failed the portability check and built locally. That is the system
working: an unportable graph is not a dispatch failure, it is a graph
that stays home.

## The dispatch TAX is nil, and the speedup cannot be measured here

Four concurrent CPU-bound builds (each hashing ~1.8 GB), same work both
ways:

```text
one daemon, no proxy : 10s
two daemons, proxied : 10s   (4 of 4 solves routed)
```

So the machinery is FREE at this scale: publishing a context, mirroring
a base image, rewriting a graph, solving on a peer, publishing the
result and importing it costs nothing measurable against the work
itself. That is worth knowing - a tax of 3x would have made capacity
irrelevant.

It is not a speedup, and no arrangement of this machine could show one.
Both daemons are containers on one host sharing one CPU, so the fleet
has no capacity the single daemon lacked; 10s versus 10s is the correct
answer, not a disappointing one. Measuring speed needs a second
MACHINE.

Note before anyone reaches for the x86 box for that: its docker0
firewall stops containers reaching host services, which is precisely how
a peer would reach the mirror. That does not fail loudly - it would zero
the measurement while looking like a slow fleet.

A sleep-based workload was tried first and is useless for this: one
daemon serves four sleeps as fast as four daemons, so the fleet looks
free because nothing is contended. The work has to be CPU-bound before
either number means anything.

Both numbers above, and every number below, are now produced by
`scripts/fleet.sh` - which exists because they were produced by hand
twenty-five times first, and a measurement you retype is a measurement
you cannot compare.

## The fleet was OFFLOADING, not parallelising

Same harness, after peer 0 was let back into the round robin - the
per-daemon cache is the honest witness, because work leaves a mark where
it actually ran:

```text
before  daemon 0: 0        daemon 1: 13.60MB   (4 routed, 0 home)
after   daemon 0: 13.60MB  daemon 1: 13.60MB   (2 routed, 2 home)
wall    10s direct -> 11s proxied
```

"4 of 4 solves routed" read as a triumph and was a symptom: peer 0 was
excluded from dispatch by arithmetic (`1 + n % (len - 1)`), so a
two-machine fleet ran every exec on ONE machine and shipped bytes with
the other. It could not have gone faster than a single daemon no matter
how many machines were added, because peer 0's share was always zero.
See `turn` for why the reasoning behind that was sound and the conclusion
was not.

## Where the tax actually is, and the two guesses that were wrong

"Two adoptions' worth of registry round-trip" was the obvious reading of
10s -> 11s and it was wrong. Timing every phase instead:

```text
solve 0  total     1 = portable    0 + peer    0 + answer 1   home
solve 1  total     2 = portable    0 + peer    0 + answer 2   home
solve 2  total 10984 = portable 1555 + peer 9427 + answer 0   dispatched
solve 3  total 10984 = portable 1546 + peer 9437 + answer 0   dispatched
build ms  [10661, 10655, 11049, 11044]   what the client waits for
return ms [1, 0, 0, 0]
```

Three things fell out, none of them the round-trip:

1. `make_portable` costs ~1.6s - publishing the context and mirroring the
   base - and EVERY solve paid it, including the two that were never
   going to leave. Moving the rewrite behind the placement decision took
   those from 1613ms to 1ms. The cheap check (exclusions) now runs first
   and the expensive one (rewrite) only for graphs that are leaving. That
   matters most where dispatch is worst: earthly excludes eleven solves
   in twelve, and was mirroring a base for each of them.
2. A gateway Solve is LAZY - ~1ms whether it stands for a nine-second
   build or a pull. Read alone it says answering is free.
3. `return` is ALSO ~1ms, which was the next guess and also wrong. The
   client's wait lives in `Control.Solve`, which is now timed.

What is left is ~0.4s per dispatched build (11.05s against 10.66s at
home) plus a one-time ~1.6s to mirror a base image, shared by every
solve that needs it and amortised to nothing across a real build. On a
real fleet that is paid once against a machine's worth of parallelism.

## One slow machine costs the whole build, and load-awareness does not fix it

Three daemons, six builds, the last daemon held to a quarter of a CPU
(`SLOW=0.25 DAEMONS=3 BUILDS=6 scripts/fleet.sh`):

```text
uniform fleet : wall  12s
one slow peer : wall 164s   build ms [10549, 10581, 10972, 10973, 162953, 162976]
```

Two of six solves landed on a machine four times slower, and the build
waits for them. Placement gave it an equal share because placement had no
idea it was slow.

`least_loaded` was written for this and DOES NOT FIX IT, which is worth
saying plainly rather than shipping it as a win. Six solves of a fan-out
arrive at once, so every peer's outstanding count is zero at the moment
each is placed, and least-loaded degenerates to exactly round robin. It
earns its place for the other shape - solves arriving spread across a
long build, where a busy peer is visibly busy - and it is never worse.
But load is not capacity.

Slot limits do not fix it either, and the arithmetic says why before the
experiment does: any placement that gives the slow machine even one of
these tasks waits 163s for it. The fix has to be a peer that can REFUSE
(principle 12 - refusal is backpressure) or a placer that knows roughly
how long the work takes and roughly how fast each machine is (principle
13 - coarse estimates). That is what `bank::timings` is for, and it is
not wired to placement yet.

The outputs were byte-identical anyway: a starved peer returns correct
bytes slowly, which is the failure mode to prefer.

## Taking it back: 164s -> 40s

Placement cannot fix a straggler, because the placement was CORRECT on
the information available - the peer was idle. The information only
arrives afterwards, when a normal adoption finishes and the straggler does
not. So the wait is bounded, and the bound is re-read while waiting:

```text
uniform fleet        : wall  12s
one slow peer        : wall 164s
one slow peer, bound : wall  40s   not routed {"peer too slow": 2}
```

Both solves on the quarter-CPU peer were withdrawn after three times the
observed median and built at home. The peer is not cancelled and its work
is not wasted if it lands: the tag is the digest of the graph, so whoever
pushes first wins (principle 3) and a later identical push is a no-op.
Outputs stayed byte-identical against a recorded baseline, including the
builds where home and peer were racing.

The threshold takes no cold default on purpose. Any number between 9.5s
and 163s "fixes" this fixture, which is precisely why one should not be
chosen before the fleet has said what normal is - with nothing observed,
nothing is slow and the wait is unbounded, exactly as before.

## The exogenous signal exists, and it is the wrong signal

Every timing that could rank peers was endogenous - load a peer and its
service time rises, which is what made the feedback controller chase
itself. One measurement is not: a peer's duration sampled while it holds
NOTHING ELSE. Placement cannot move that by deciding differently.

It works, and it says the peers are the same:

```text
peer solo ms: {1: 9088, 2: 9055}
  peer 1  a second daemon on THIS machine
  peer 2  the remote 32-core box
```

And the fleet says they are not. At identical placement, the same two
peers gave 22s and 16s. The difference is CO-LOCATION: peer 1 competes
with home's sixteen builds for the same sixteen cores, peer 2 does not.
Uncontended speed cannot see that, because uncontended is exactly the
condition under which it does not happen.

So the only clean signal available measures the wrong property. What
distinguishes peers here is capacity under load, and capacity under load
is endogenous by nature - it exists only when something is loading it.
That is a fact about the problem rather than a gap in the implementation,
and it is why the saturation gate, which needs no peer ranking at all,
has outperformed every attempt to rank them.

The sampler is kept because it cost little and disproved something. It
also needed its own bug fixed first: the "still alone at the end" check
read the counter AFTER giving the slot back, so it asked whether the peer
was empty - always true - and recorded nothing from two builds that were
genuinely alone. `fetch_sub` returns the previous value; use that.

## What a dispatch actually costs: 0.1s, plus one base image

Three iterations blamed three different things for the cost of shipping a
build - the registry, the network, then peer 0's import. One measurement
with no contention at all settles it. A single build, once locally and
once forced away:

```text
1 build, no fleet     10s
1 build, dispatched   11s
  portable 1586ms + peer 8815ms + answer 3ms, client saw 10511ms
```

Subtract: 10511 - 1586 - 8815 leaves **110ms** for everything else,
including peer 0 importing the result. The import is not the cost either.

What a dispatch costs, finally:

| | cost |
| ------------------------------ | ---------------------------------- |
| mirror the base, per arch | ~1.6s ONCE, shared by every solve |
| ship and import a result | ~0.1s per build |
| the peer building it | whatever that peer is worth |

Note the peer built it in 8.8s against 10s here, so on this pair
dispatching a single build is very nearly free before the base is
mirrored, and free after.

Which means the earlier "pushing more work out made a four-peer fleet
slower" was never about dispatch overhead. Four daemons sharing one
32-core box, amd64, are simply worth less per build than local cores when
all of them are loaded. That is peer CAPACITY - heterogeneity again -
and the weights measurement already showed capacity cannot be guessed
from hardware. It has to be observed, and observing it is the open
problem, not the transport.

## Where added machines stop paying

Twenty-four builds, remote daemons added one at a time:

```text
remotes  wall   placed
      0   24s   {home: 24}
      1   16s   {home: 16, 8}
      2   15s   {home: 16, 4, 4}
      3   15s   {home: 16, 3, 3, 2}
      4   15s   {home: 16, 2, 2, 2, 2}
```

It plateaus at two, and the arithmetic says why before the experiment
does: with sixteen local slots, a twenty-four build fan-out has exactly
EIGHT solves to export. Further peers subdivide the same eight into
smaller shares and sit idle the rest of the time.

The obvious reading is that the gate is too conservative and home should
keep less. Tested, and it is wrong - pushing MORE work to a four-peer
fleet makes it SLOWER:

```text
home slots  placed                      wall
        16  {home: 16, 2, 2, 2, 2}       14s
         8  {home:  8, 4, 4, 4, 4}       17s
         4  {home:  4, 5, 5, 5, 5}       20s
```

Home is not the bottleneck. I wrote "the wire is" and that was wrong;
two further measurements refuted it.

First, what the mirror actually serves. Eight dispatched builds cost 112
requests - fourteen each, all small:

```text
{"GET blob": 20, "GET manifest": 10, "HEAD blob": 18, "HEAD manifest": 19,
 "POST upload": 18, "PUT manifest": 9, "PUT upload": 18}
```

Fourteen LAN round trips is tens of milliseconds, not the seconds a
dispatch costs. Second, and decisive: a peer on ANOTHER MACHINE beats a
peer on this one, at identical placement and identical mirror traffic.

```text
peer is a second local daemon   {home: 16, peer: 8}   22s
peer is the remote box          {home: 16, peer: 8}   16s
```

The remote peer pays LAN transfer the local one does not, and still wins,
because a local peer competes for the same sixteen cores while a remote
one adds capacity. Transfer is cheaper than the contention it avoids.

So the useful size of a fleet is set by HOW MUCH WORK IS WORTH EXPORTING,
and what makes exporting cost anything is still not isolated. The
remaining suspect is per-solve machinery rather than bytes - and note that
part of it lands on PEER 0, which imports every dispatched result while
also building sixteen things. That would explain why pushing more work out
made a four-peer fleet slower without any of it being the network.

The saturation gate still lands near the measured optimum, and now for a
reason that survives the correction: exporting only the local surplus
keeps peer 0's import load proportional to what it was going to be idle
for anyway.

## Three daemons, two hosts

Every fleet measurement until now had exactly ONE away peer, which means
`least_loaded` was answering a question with one possible answer. A second
daemon on the remote host gives it a real choice:

```text
peer 0 upstream            native linux/arm64
peer 1 192.168.1.137:18400 native linux/amd64
peer 2 192.168.1.137:18401 native linux/amd64
placed {home: 16, peer1: 4, peer2: 4}   wall 15s
```

Even, which is what it should be for two identical idle peers, and the
failure it rules out is the one that has already happened once at another
level: `min_by_key` keeps the FIRST minimum, so scanning from zero would
have sent all eight to peer 1 and left peer 2 idle. The unit test for that
now has a hardware witness.

Wall clock across the whole series, same twenty-four builds:

```text
no fleet                        24s
+ one remote daemon             19s
+ a second remote daemon        15s
```

Both remote daemons are on the same 32-core box, so this is capacity, not
a third machine. Three separate HOSTS, and anything across a WAN, remain
untested.

## What was deleted, and what was kept

Ref affinity is gone. `ref_home`, `home_of`, `gw_of` and `remember`
existed to route each gateway call to the daemon that minted the ref it
names - the right design when a solve might be answered anywhere. Adoption
made it moot: the peer builds and publishes, and the client's solve is
always answered on peer 0 with an import, so every ref belongs to peer 0
by construction. The map was still being written on every result and read
by nothing.

`relay` went with it. Its comment was worth keeping and is worth
repeating here, because the lesson outlives the function: buildkit
associates a session by REQUEST HEADERS, so forwarding a stream without
them leaves the daemon holding a session it cannot match to a build, and
the frontend's filesync comes back `Unimplemented` - an error naming
nothing to do with metadata. Every forwarding path preserves metadata for
that reason.

What was NOT deleted, though clippy reports it: `dispatch::next_work`,
`worth_offering`, `STALL`, and the whole of `lease` and parts of `driver`.
Those belong to the sibling product line - a driver arbitrating offers
between workers - not to this proxy. They are unreferenced HERE, which is
not the same as unused, and deleting another line's work because this one
outgrew it would be vandalism dressed as tidying.

## Stop offering while the shared thing is down

Per-peer memory cannot express "no peer is at fault and every peer is
unusable", so a healthy peer behind a dead registry kept being offered
doomed adoptions - eight per round, each waiting out a push it could never
complete. A fleet-wide breaker fixes that:

```text
registry dies   round 1  placed {home: 16, peer1: 8}   8 refused, breaker arms
                round 2  placed {home: 24, peer1: 0}   "mirror down, not offering": 24
                wall 27s then 24s - the no-fleet baseline
healthy fleet            placed {home: 16, peer1: 8}   wall 22s, unchanged
```

Round one still offers eight, and cannot do otherwise: all eight are
placed within 393ms, before the first refusal returns. The breaker earns
its place on everything after.

Recovery is optimistic rather than polled - the state simply expires after
fifteen seconds and the next solve is offered normally. If the mirror is
still dead that solve refuses, re-probes and re-arms: one wasted adoption
per cooldown instead of one per solve, and nothing in the background
poking a service nobody is using.

Refusing to offer is not a risk here, which is why the breaker can be
blunt: building at home is the fail-open answer already, so a false
positive costs the fleet and nothing else.

## Telling a bad peer from a bad network

Both failures below look identical from the placement side - a refusal -
and they call for opposite responses. Asking the mirror whether it is
alive separates them, and the two runs are now different:

```text
registry dies  placed {home: 32, peer1: 16}   "mirror down, peer not blamed": 16
peer dies      placed {home: 40, peer1:  8}   "peer 1 refused": 8, then nothing
```

The dead peer is struck and gets nothing in round two. The healthy peer
behind a dead registry keeps its turns, because it did nothing wrong and
will work the moment the mirror returns.

The probe had to be aimed correctly to say anything at all. The mirror is
named for the DAEMONS - `host.docker.internal:15000` - and that name only
exists inside a container. Asked from the host it fails to resolve, so the
first version answered "dead" every time and nobody was ever blamed,
including a peer that really had died. Substituting loopback is exact
rather than clever: that hostname means "the machine this proxy is on".

Still wasteful in one direction: with the registry down, the peer keeps
being offered work it cannot complete - eight more in round two. Correct
but not clever; a fleet-wide "the shared thing is down" state would stop
offering at all, and that is a different mechanism from per-peer memory.

## Killing the REGISTRY mid-build

The mirror is the one thing the whole fleet shares - published contexts,
mirrored bases, adopted results all pass through it - so it is the single
point the design actually depends on. Destroyed five seconds into
twenty-four builds:

```text
wall 28s (baseline with no fleet: 24s)   failed 0
placed {home: 16, peer1: 8}   not routed {"peer 1 refused": 8}
outputs identical to the single-machine baseline
```

Nothing hung and nothing was wrong. The eight in flight could no longer
push or pull, refused, and were built at home; the 4s over baseline is
what those doomed offers cost before failing. Principle 9 holds in the
strong sense: the registry is a fallback, not a data path a build cannot
live without.

One honest wrinkle. The refusals strike PEER 1, and peer 1 did nothing
wrong - the shared infrastructure failed. It costs nothing while the
registry is down, because no dispatch can work anyway, but a healthy
machine stays penalised after the registry comes back. Distinguishing
"this peer is bad" from "the thing between us is bad" needs a signal
neither end has on its own.

## Killing a peer mid-build

Fail-open is principle 5 and had never been tested by actually breaking
something. Twenty-four builds, two daemons, the peer destroyed four
seconds in while holding eight of them:

```text
wall 24s (baseline with no fleet: 24s)   failed 0
placed {home: 16, peer1: 8}   not routed {"peer 1 refused": 8}
outputs identical to the single-machine baseline
```

Every build finished and every byte matched. The fleet's contribution
vanished and nothing else did, which is exactly what fail-open should look
like: back to the baseline, not below it.

It also showed a gap. The dead peer was offered work eight times, each
offer waiting out a transport error, because only SLOWNESS was remembered

- a take-back struck a peer and an outright refusal did not. Refusals now
strike too, and the report names the machine.

That does not reduce the eight, and the reason is the one that keeps
recurring here: all eight are placed within 393ms, before the first
refusal comes back. No memory can act on a fan-out that is decided before
any of it returns. What it does fix is everything after - a second round
through the same proxy offers the dead peer NOTHING:

```text
round 1  placed {home: 16, peer1: 8}   8 refused
round 2  placed {home: 24, peer1: 0}
```

## Dispatching on saturation, and the suspect that was wrong

Work ships only once the local machine is FULL - `home_slots` against
builds running at home. One mechanism, no per-workload tuning, against the
splits that had to be swept for by hand:

```text
work   no fleet   saturation gate   best hand-swept
  20        8s      10s  {16, 8}     7s  at 22:2
  90       24s      17s  {16, 8}    15s  at 16:8
 250       64s      32s  {16, 8}    32s  at 12:12
```

It matches the swept optimum at the size where the fleet matters most and
is a few seconds off at the smallest, where the fleet is worth little
either way. No number in it was chosen to make a fixture look good.

Getting there meant refuting my own explanation. The gate produced
`{20, 4}` at every build size, and the prime suspect was the harness -
each `buildctl` is a container start, so perhaps the clients did not
really arrive together. Measured instead of assumed:

```text
arrivals: 24 solves spread over 393ms
```

Simultaneous. The suspect was wrong, and the counter that settled it -
`"home has room": 16` - named the real fault. TWO MECHANISMS WERE DECIDING
THE SAME THING IN SERIES. Saturation decides home-or-away; then `place`
ran its weighted rotation and sent half the saturated solves home anyway.
Sixteen placed while home had room, then eight saturated of which the
rotation kept four: exactly the twenty and four observed.

Three accounting bugs came first, each found by printing the counter
rather than reading the code:

1. Load-then-increment is check-then-act. Twenty-four concurrent solves
   all read 15 before any increment landed - `home peak 20 of 16 slots`.
   A slot is now CLAIMED with `fetch_update`.
2. An excluded solve claimed a slot and never released it, because only
   dispatchable solves were recorded. Eleven in twelve are excluded on an
   earthly build, so the counter would have drifted up until the fleet
   believed home was permanently full.
3. A saturated solve with nowhere to go still builds at home, and was
   releasing a slot it had never been granted.

Work is now shipped only once the local machine is FULL - `home_slots`
against builds currently running at home. The core count is a legitimate
input here, unlike in the peer weights below: "is this machine full" is a
count against a count, where "how fast is this machine" was a throughput
that core counts proved uncorrelated with.

Getting the counter right took three fixes, each found by measurement
rather than reading:

1. Load-then-increment is check-then-act. Twenty-four concurrent gateway
   solves all read 15 before any increment landed, and the report said so:
   `home peak 20 of 16 slots`. Now a slot is CLAIMED with `fetch_update`.
2. An excluded solve claimed a slot and never released it, because only
   dispatchable solves were being recorded. Eleven solves in twelve are
   excluded on an earthly build, so the counter would have drifted up
   until the fleet believed home was permanently full.
3. A saturated solve with nowhere to go still builds at home, and was
   releasing a slot it had never been granted - handing back capacity that
   did not exist. Only a solve that actually held one releases one.

With all three fixed the peak is exactly 16 of 16, so the gate does open.
What is NOT explained is the split it produces:

```text
work    saturation gate     best hand-swept
  20    7-10s   {20, 4}     7s   at 22:2
  90    16s     {20, 4}     15s  at 16:8
 250    39-40s  {20, 4}     32s  at 12:12
```

Twenty home and four away at every size, when sixteen slots should give
sixteen and eight. Four slots are being released before the last solves
are placed, and the prime suspect is the harness rather than the proxy:
each `buildctl` is a container start, so clients may not arrive as
simultaneously as "twenty-four in parallel" suggests. That is unmeasured,
and until it is measured the gate's behaviour at large build sizes - 40s
against a hand-swept 32s - is not something to claim as an improvement.

## Build size was a proxy for SATURATION, and wiring the gate proved it

The sweep below argued for a per-subtree rule: ship a subtree only if it
is bigger than its own transfer. Wiring it is what showed the rule is
wrong, and it took two corrections to get there.

The first overhead estimate was away-time minus what the peer spent
building. The gate never fired, because the duration compared against is
itself an away duration and already contains that overhead - the test was
`est > (est - build)`, true almost always. Comparing two client-visible
numbers in the same units fixes that.

It still does not fire, and the model is why. At small build sizes home is
not SATURATED: twenty-four short builds fit comfortably in sixteen cores,
so shipping adds latency without relieving anything. Per-build durations
cannot see that, however carefully they are compared. Build size was only
ever standing in for how full the local machine is - bigger builds
saturate it sooner, which is why the sweep looked like a size effect.

Ships off, behind `REBUCK2_GATE=1`, with the estimator live and reported.
The measurement is sound; the question put to it was wrong. Saturation is
a different mechanism and the local core count is a legitimate input to it

- "is this machine full" is not the same question as "how fast is this
machine", which is the one core counts got wrong two sections down.

## The optimal split belongs to the WORKLOAD, not the fleet

Two machines, twenty-four builds, the split swept at three build sizes.
`work` is the fixture's inner loop count and scales compute per build;
transfer cost per dispatched build is roughly constant.

```text
work   no fleet   12:12   16:8   20:4   22:2
  20        8s      12s    10s     7s     7s
  90       24s      18s    15s    18s      -
 250       64s      32s    33s    40s      -
```

Three things fall out, and the first is the one that matters:

1. At `work=20` THE FLEET HURTS. An even split takes 12s against 8s for
   not having a fleet at all - a 50% slowdown - and only by barely using
   the peer (22:2) does it draw level. Every dispatched build pays a
   constant transfer, and when the build is shorter than the transfer,
   shipping it is a loss.
2. At `work=250` the fleet is worth 2x (64s -> 32s) and the optimum is at
   or beyond an even split, because the transfer has amortised away.
3. The optimal dispatched SHARE rises monotonically with build size: ~8%,
   33%, >=50%.

So a fleet-wide weight - declared, derived, or swept for - is answering
the wrong question. The 20:4 that is optimal for small builds costs 40s
against 32s on large ones, 25% worse. There is no single number, because
the number is a property of the work.

The decision belongs PER SUBTREE: dispatch when the estimated compute
exceeds the transfer cost, keep it at home when it does not. That is what
`dispatch::worth_offering(est_p90, running_for)` was written for, and what
`bank::timings` estimates durations for - both still unwired, which is now
the largest gap between what this system knows and what it does.

## The balance condition is real; feeding it back is unstable

The sweep left an obvious question: can the optimum be found rather than
swept for? Measuring what the client waits, split by where the work went,
says yes - the optimum is exactly where the two sides finish together:

```text
12:12  wall 18s   home 11425ms  away 17235ms  ratio 1.51
16: 8  wall 15s   home 15228ms  away 15506ms  ratio 1.02
```

And the arithmetic on the bad split points at the good one:
`derive_weights(11425, 17235)` is 3:2, next door to the 16:8 that measured
best. That is a control law with no core counts in it.

Feeding it back does not work, and two runs of the same experiment are how
that showed:

```text
adapt   24 builds x3   18 14 14s   placed 40:32
adapt   same again     24 17 24s   placed 34:38   <- drifted the wrong way
pinned  control        18 15 16s   placed 36:36
```

Service time is ENDOGENOUS - what is measured is caused by what is set.
Load the home side, its mean rises, the ratio drops below one, the
controller reads "home is the straggler" and sends more work away, which
raises the away mean in turn. The first run looked like a win over the
hand-tuned 15s; the second was worse than doing nothing.

So it ships OFF, behind `REBUCK2_ADAPT=1`, with the measurement kept: the
law is worth having and this loop around it is not. A stable version has
to break the feedback - compare against a quantity the controller does not
move, or damp and converge rather than jump to the ratio each time.

The control run earns its own line. Rounds two and three are faster
whatever placement does, because the peers warm their own caches; a
self-tuner left switched on would have taken credit for that too.

## Capacity cannot be guessed from cores, and the guess made it WORSE

The 50/50 split of the previous result looked like the obvious waste: the
peer has 32 cores against this machine's 16, so it should take twice the
work. Placement was made weighted to allow exactly that, and then measured.
Twenty-four builds, the split swept:

```text
home:peer   wall
 24: 0      24s    no fleet at all
  8:16      21s    peer weighted 2x, "because it has twice the cores"
 12:12      18s    flat
 16: 8      15s    <- best
 18: 6      16s
 20: 4      18s
```

There is an interior optimum and the informed-looking guess sits on the
WRONG SIDE of it - worse than the flat split it was meant to improve, and
only three seconds better than not having a fleet. Sending more work to
the bigger machine costs more, because every dispatched build pays a push
and a pull across the LAN and a local build pays neither. Cores measure
what a machine can compute; they say nothing about what it costs to give
it something to compute.

So the weighting mechanism stays - it is what makes the optimum reachable
at all - and the DEFAULT stays 1. A weight is a statement about observed
end-to-end throughput, not about hardware, and setting it from a core
count is worse than leaving it alone. Deriving it from measurement is the
next thing; the sweep above is what it has to beat.

## Load-aware placement is round-robin plus strikes, in practice

The score divides a peer's in-flight count by its declared weight, so a
busier or smaller machine should be picked less. Once the counter feeding it
was fixed, the obvious question was whether it BUYS anything. Controlled
against the same binary with the reservation disabled - one variable, three
runs each:

```text
                        placed        struck    wall
slow peer, 0.3 cpu
  load-aware            {1:11, 2:1}   {2: 1}    15 4 3 4s
  round-robin           {1:11, 2:1}   {2: 1}    15 4 4 3s
mildly slow, 0.6 cpu
  load-aware            {1: 9, 2:3}   {2: 2}    8 14 3 4s
  round-robin           {1: 9, 2:3}   {2: 2}    8 14 3 4s
```

Identical. Not close - identical, including the strike counts. Whatever is
routing around the slow peer, it is not this: it is the hedge withdrawing an
adoption and the strike that follows, and it gets there first at every
slowness that can be produced locally. 0.6 CPU already trips it.

The first reading of the top-left cell was that load-awareness had shifted
eleven of twelve builds off a slow machine. That was the control's to
refute, and it did.

Two regimes are left where the score could still matter and neither is
reachable here: peers that differ in capacity while all staying inside three
times the median, and arrivals spread thinly enough that one peer is idle
while another is not. In BURST arrival - twelve solves inside 130ms, which is
what a fanout build does - every decision is taken before any completion, so
the counter rises symmetrically and the score alternates exactly as round
robin would.

So the fix stands on correctness, not throughput. Peer weights are a
documented feature that did nothing (6/6 against 3/9 once fixed) and
`solo_ms` called every sample uncontended. Neither claim is about speed, and
neither should be sold as one.

**The mechanism that sweep used no longer exists.** At the time, a weight
reached the home-versus-away decision through `turn`. Saturation was then
made the single authority on that question - two mechanisms deciding it in
series was its own bug - and `turn`'s home branch became unreachable. Today
the same sweep is done with `REBUCK2_HOME_SLOTS`, and the numbers above
should be read as a slots sweep, which is what they measured.

The conclusion survives the re-labelling: there is an interior optimum, and
"it has twice the cores" lands on the wrong side of it. What does not survive
is the implication that a peer WEIGHT is the control for this. It is not.
Weight chooses between away peers, and did nothing at all until the load
counter it divides into was fixed - see "a knob that is wired, documented and
inert" in how-this-lies.md.

## It is faster on two machines: 24s -> 18s

The measurement this whole thing existed to make, and which one host could
never produce. Twenty-four CPU-bound builds against sixteen local cores,
then the same twenty-four with a 32-core x86 box across the LAN:

```text
one machine  : wall 24s
two machines : wall 18s   placed {home: 12, peer1: 12}
outputs identical to the single-machine baseline
```

Twelve builds executed on another physical machine, across an
ARCHITECTURE boundary, and every byte came back the same.

1.33x rather than 2x, and the reasons are known rather than guessed: each
dispatched build pays a push and a pull over the LAN, twenty-four builds
on sixteen cores is only mildly oversubscribed, and a flat 50/50 split
ignores that the peer has twice the cores and adds latency. Capacity-aware
placement is the next thing worth measuring, and it is now measurable.

What made it work is that base images are mirrored FOR THE PEER
(`solve::mirror_image`'s `platform`, and the architecture in the tag).
Before that the mirror held whatever this host resolved, and the previous
section is the record of the x86 peer dying on an arm64 binary.

One bug in the middle, of a shape worth naming: the mirror wrote under the
key `base:linux/amd64` and the rewrite read under `base`. The image was
copied correctly, the lookup found nothing, the graph kept naming
docker.io, and the solve was then refused as "base unmirrored" - the log
printing `base ... mirrored as ...` and `base unmirrored` one after the
other. A success and a failure that never meet.

## A SECOND MACHINE, and what it refuted

Everything above ran several daemons on one host, which can measure
overhead and placement but never capacity - the fleet had no more CPU than
the single daemon did. One daemon on this arm64 host, one on a 32-core x86
box across the LAN, mirror named by an address both can reach:

```text
peer 0 upstream            native linux/arm64
peer 1 192.168.1.137:18400 native linux/amd64
peer 1 could not take it: exit code: 255
  sources=["docker-image://192.168.1.91:15000/rebuck2/base:45ee4c56..."]
wall 16s (baseline 10s), all six outputs correct
```

The plumbing works: the remote peer was reached, and it PULLED the base
image from this machine's mirror over the LAN. What fails is the image
itself. `make_portable` mirrors `alpine:3.20` as resolved HERE, so the
mirror holds the arm64 variant and the portable graph names a
single-architecture base. An x86 peer pulls it and dies with `exit code:
255` - a binary it cannot execute - after six seconds of downloading.
Fail-open recovered every build; the cost was time and an alarming log.

This refutes what the multi-arch section below concluded. "Emulation is
legal and merely 5-10x slower, so deprioritise rather than refuse" is true
of a peer resolving a manifest LIST for itself, and false of a peer handed
a single-architecture copy. Until the mirror carries manifest lists, a
peer that is not native cannot build the graph at all, so:

- an unpinned graph is read as pinned to whatever peer 0 resolved it as,
  because that is what the mirrored base actually is; and
- a fleet with no native away peer builds at HOME rather than offering.

With both, the cross-architecture fleet places `{home: 6}`, wastes
nothing, and matches the single-machine baseline. Real speedup across
machines needs the mirror to carry manifest lists - that is the next
thing, and it was invisible from one host.

Worth recording: the x86 box's docker0 firewall, which stops ITS
containers reaching ITS host services, did not bite. The mirror lives on
the other machine, so the traffic is ordinary LAN traffic.

## The context path finally ran, and the fixture was the bug

Every fixture until now sourced only from `docker-image://`, so
`contexts published: 0` in every single run: the machinery that unpins a
subtree from the machine holding the client's disk had never executed. A
fixture with a real `local://context`:

```text
sources 4 registry, 4 local    contexts published: 4
placed {home: 2, peer1: 2}     outputs identical to baseline
```

Files on the client's disk became content in the mirror, and a peer with
no session and no access to that disk built from them. That is the whole
claim of the design, executed for the first time.

Getting there cost two fixture bugs, both of which produced WRONG BYTES
while every exit code stayed zero - and both of which would have read as
dispatch corrupting results:

1. All N graphs were byte-identical, because a context that differs only
   in CONTENT does not change the graph that names it. Buildkit correctly
   treats identical vertices as one build: four solves, one execution,
   every client handed build 0's bytes.
2. With the graphs made distinct, the LOCAL SOURCE vertex was still
   identical across builds - so buildkit synced the first client's
   directory and served it to all four. Distinct graphs, distinct
   directories, every output still `task-0`. `local.unique` exists for
   exactly this, and `solve::publish_context` had been setting
   `local.session` for the same reason all along.

What settled both in one command was running the fixture with NO proxy on
ONE daemon. All four outputs were still `task-0`, so nothing about
dispatch was involved. Reaching for the baseline first turned a
"distributed builds return wrong bytes" panic into a fixture fix.

## A Dockerfile build dispatches NOTHING, and cannot

The claim "any project that speaks buildkit can use this" needed a client
that is not earthbuild and not a hand-written graph. `buildctl build
--frontend dockerfile.v0` is that client, and it dispatches zero percent:

```text
solve <id>: NO definition on the wire (frontend="")
gateway solve with no definition: frontend="dockerfile.v0" opts=["no-cache"]
placed {}   routed 0
```

This is structural, not a bug to fix. Naming a frontend asks the DAEMON to
resolve it; the daemon runs that frontend as its own gateway client, the
frontend generates LLB against the daemon's internal bridge, and none of it
crosses the proxy. There is no wire to cut because the graph is never on a
wire.

So the product line is narrower and sharper than "any buildkit client":

| client                              | dispatches |
| ----------------------------------- | ---------- |
| earthbuild (builds its own graph)   | yes        |
| `buildctl build < graph.llb`        | yes        |
| anything driving the gateway w/ LLB | yes        |
| `--frontend dockerfile.v0`          | no         |
| `docker build` / `buildx`           | no         |

The rule is not about the tool, it is about WHERE the graph is built: a
client that constructs LLB itself can be distributed, and a client that
asks the daemon to construct it cannot. Making `docker build` work means
running the dockerfile frontend client-side, which is a change to the
client, not to this proxy.

The proxy now says this rather than reporting "no definition" - a number
that is true and gives the reader nothing to do. Kept as a harness mode
(`DOCKERFILE=1`) so the message stays honest.

Worth noting what is still untested: every LLB fixture here sources from
`docker-image://`, so `contexts published: 0` in every run, and the
context-publishing path has never actually run. The Dockerfile mode was
meant to exercise it and cannot, for the reason above.

## Native multi-arch, and why a platform FILTER would have done nothing

Placement ignored platform entirely, in a system whose stated first job is
native multi-arch. The obvious fix - ask each peer what platforms it
supports and filter - is a no-op, and the daemons say so plainly. A stock
buildkitd on an arm64 host:

```text
linux/arm64,linux/amd64,linux/amd64/v2,linux/riscv64,linux/ppc64le,...
```

and the same image forced to amd64:

```text
linux/amd64,linux/amd64/v2,linux/amd64/v3,linux/arm64,linux/riscv64,...
```

Every daemon claims nearly every platform, because binfmt will run
anything. "Supports linux/amd64" is answered YES by every peer in any
fleet. The worker's OWN architecture is the one it lists FIRST, and that
is the whole distinction: an emulated build is legal and five to ten times
slower.

Three daemons, the last one `--platform linux/amd64`, six arm64 builds:

```text
peer 0 upstream native linux/arm64
peer 1 ...:18373 native linux/arm64
peer 2 ...:18374 native linux/amd64
placed {home: 2, peer1: 4}      wall 12s, outputs identical
```

The emulated peer took none of six, against two in a uniform fleet, and
the build paid no emulation penalty. Emulation is deprioritised rather
than refused: a fleet with no native peer still builds, slowly, and if
that turns out ruinous the take-back catches it.

This also caught the harness measuring something other than it claimed.
`IMAGE` pinned a TAG, so the daemons' architecture was whatever was in the
local image cache - and pulling that tag once with `--platform
linux/amd64` leaves an amd64 image under it, after which every daemon runs
under QEMU. Docker says so in one warning line on stderr and nothing else
changes. The harness now pins and prints the architecture.

## Remembering, and a control that nearly was not run

Taking work back is reactive: without memory the fleet rediscovers the
slow machine once per solve, paying the bound every time. A peer taken
back from is struck, and a strike biases placement away from it.

Three rounds of the same six builds through ONE proxy, and a uniform
fleet run as a control:

```text
slow peer  wall 39 10 10s   placed {home: 6, peer1: 10, peer2: 2}
uniform    wall 12 10 10s   placed {home: 6, peer1:  6, peer2: 6}
```

The wall clock is NOT the evidence, and reading it as such was the near
miss. "Round 2 dropped to 10s, so avoidance works" is wrong: the control
shows a uniform fleet also drops to 10s, because rounds 2 and 3 hit the
peers' own caches. 10s is the cached floor, reached either way.

The evidence is the placement counts, which had to be added to see it:
peer 2 took 2 of 18 placements - its two cold round-1 solves - and nothing
afterwards, against 6 in the control. The inference from timing does hold
once stated properly (peer 2 is both slow AND still cold, so a round-2
placement there would have cost ~160s, not 10s), but an argument that
subtle is a reason to count the thing directly.

A strike is a bias, not a ban: a struck peer still wins against peers
holding several jobs each, because a fleet that banned machines outright
would shrink itself on one bad minute. And when EVERY away peer is struck,
the work goes home - otherwise a two-daemon fleet with one bad peer would
offer, wait out the bound and take it back, on every single solve.

Two counting bugs surfaced here, both of the same kind - a number that
blames the wrong thing. The take-back arrived at the caller as an `Err`
and was counted as "peer refused", accusing a machine of refusing work it
was still doing. And a baseline recorded with four builds compared against
a six-build run reported "outputs DIFFER" when builds 0-3 were identical
and 4-5 merely did not exist in it.

## The bytes are the same, which nothing had checked

Every measurement up to here read exit codes. A distributed buildkit that
returns the wrong bytes is worse than a slow one, and identity is what
buildkit matches on (principle 4) - a result one byte off a local build
poisons every cache downstream of it while every log line says success.

`scripts/fleet.sh` now exports each result and hashes it. Baseline on one
daemon with no proxy, then the same four builds through the fleet:

```text
build 0: 7d2d122a...   build 1: e37f56da...
build 2: ccaca3f5...   build 3: 5f1f6a93...
outputs identical to base/digests.txt
```

Builds 2 and 3 were the dispatched ones: rewritten, mirrored, built on
another daemon, pushed to a registry, imported back, exported to the
client - and byte-identical to having built them at home.

Getting a result out at all took two corrections worth keeping. Exporting
the ROOTFS to a local directory fails on `lchownat proc: permission
denied`, so the fixture writes to a scratch mount instead. And the rootfs
mount must still declare `output: 0` even though nothing wants it,
because on an LLB mount `output` also decides writability - with `-1`,
runc cannot create `/etc/resolv.conf` and the command never runs.

## 100% dispatch, on a client that is not earthly

Four plain-LLB builds through the proxy, two stock buildkitds:

```text
[wire] gateway solves : 4
[wire] solves routed  : 4 to other daemons
[wire] not routed     : {"considered": 4}
build 0..3 exit=0
peer 2 cache          : Total 13.63MB
```

Every solve dispatched. The 1-in-12 ceiling on earthly builds was
entirely the debugger plumbing described below - not a limit of the
mechanism, not a property of build graphs, and not the user's secrets.

It also settles what the product is. A distributed BUILDKIT works today
for clients that send ordinary LLB; a distributed EARTHLY additionally
needs one upstream change. Those are different amounts of work and the
difference was invisible until a non-earthly client was tried.

## What actually limits dispatch here: earthly's DEBUGGER

The eleven excluded solves do not carry a user secret. They carry this,
and every earthly `RUN` carries it:

```text
mount /run/secrets/earthly_debugger_settings
  id=name=da39a3ee5e6b4b0d3255bfef95601890afd80709&org=&project=&v=1
```

`earthfile2llb/converter.go` attaches a debugger settings SECRET mount
and a `llb.HostBind()` mount for the debugger binary to every exec, and
the only guard is `if !opts.Locally`. Not `--interactive`: the
interactive flag decides whether to ERROR when the capability is
missing, not whether to attach the mounts. The id is a query string
whose org and project are empty and whose name is the sha1 of the empty
string - nothing is being protected here.

So every earthly exec is pinned to one machine twice over, by a secret
and a host bind, in support of a debugger nobody asked for. The
exclusion is CORRECT - principle 10 does not get to make exceptions
about secrets - and the consequence is that essentially no earthly exec
can be dispatched as things stand.

Three ways out, and the first is the honest one:

- earthbuild omits the debugger plumbing when not debugging. A small
  upstream change with an obvious rationale, and the only one that does
  not weaken a safety rule or lie about a graph.
- the proxy strips known-inert frontend plumbing. Tempting and wrong by
  default: it changes the graph the client asked for, and "inert" is a
  judgement about someone else's mount.
- target clients that do not do this. buildx and dagger graphs carry no
  such plumbing, so they are dispatchable today - which is an argument
  for the product being a distributed BUILDKIT rather than a
  distributed earthly.

This is also the honest cost of principle 15. "The client must not have
to change" is the right claim, and this client's own plumbing prevents
distribution - so for earthly, either earthbuild changes or nothing
moves.

## What the secret exclusion previously looked like

With the exclusion check finally wired into placement, the twelve
solves of a real earthly build resolve as:

```text
[wire] solves routed : 1 to other daemons
[wire] not routed    : {"considered": 12, "excluded: Secret": 11}
```

Eleven carry a SECRET, and principle 10 has always said what that means:
shipping the spec ships the secret reference, so the subtree does not
travel. They were never dispatchable. What was wrong was WHERE they were
refused - offered to a peer, which then failed with "no active sessions"
because a secret needs the session's service, and the error named the
session rather than the secret.

The check for this was written early and simply never consulted on the
path that places work. Every diagnosis that followed - the fork, the
capability, the mirror, the auth - was chasing an error message that was
true and irrelevant.

It also says something about the WORKLOAD rather than the mechanism: for
earthly builds, what limits dispatch is not contexts or base images,
both of which are now solved, but how much of the graph touches secrets.
That is worth measuring on a real repo before optimising anything else.

## The bind a peer is currently in

Twelve solves are offered and one completes. The other eleven fail on
the PEER, and which way they fail depends on which daemon it is:

| peer | failure |
| ----------------------- | ------------------------------------ |
| `earthbuild/buildkitd` | `no active sessions` - it wants a session to export |
| `moby/buildkit` | `unknown API capability exec.mount.sock` |

So a peer cannot be stock buildkit, because earthly's LLB uses a
capability only its fork declares; and the fork will not export without
a session, which is exactly what a peer does not have.

The way out is to give the peer a session of OUR making - not the
client's. Buildkit does not need the client's credentials here, it needs
SOMEBODY to ask; an empty auth service satisfies it. That means serving
a session to the peer, which is the tunnelled gRPC server this proxy has
so far avoided implementing.

The one solve that does complete is the one whose export finds
everything already local, so no auth is resolved. That is a warm-cache
success, and it is why the number was 1 rather than 0 - not evidence
that placement works better than it does.

## Where this actually got to

A real `earthly +all` on a two-daemon fleet, and the whole chain fired:
the context was published as content, the graph was made portable, the
work was OFFERED to a peer through its own `Control.Solve`, and when the
peer died the build fell back and succeeded.

```text
[proxy] peer 1 could not take it: peer solve: Unknown error transport error
=========================== Earth Build  SUCCESS ===========================
```

That failure is principle 5 doing its job under a real crash rather than
a simulated one: duplicate work is always correct, so a peer that dies
mid-offer costs a retry on the machine that was going to build it
anyway. The client saw a normal build.

What still does not work is the PEER, not the dispatch. It panics
reaching for Docker Hub because a sessionless daemon has no registry
auth, and declaring our mirror as a `mirrors` entry for `docker.io` in
its config did not divert it. Until a peer can obtain base images with
no credentials, adoption offers work that no peer can complete - so the
fleet is correct, and idle.

The remaining question is therefore plumbing rather than design: make
the mirror answer for `docker.io` in a way buildkit honours. Everything
above it - portability, publication, placement, adoption, ref affinity,
fail-open - is built and has run.

## What "transparent" has to mean

All nine methods, including the streams. `Session` in particular is
BIDIRECTIONAL and carries filesync and credentials — a proxy that
forwards Solve but not Session works for exactly the builds that need no
local context, which is not the workload we care about.

## Grafting is correct, measured, and slower

A/B on `+test-no-qemu-group10`, six GitHub runners, same commit, only
`REBUCK2_GRAFT` moved:

| mechanism          | baseline (1 machine) | fleet (6 machines) | routed | mean lead |
| ------------------ | -------------------- | ------------------ | ------ | --------- |
| neither            | 216s                 | 504s               | 44     | 18.6s     |
| grafting           | 212s                 | 611s               | 37     | -         |
| fleet cache (rw)   | 211s                 | 838s               | 37     | 27.6s     |

Both legs at parity with their baseline - zero failed targets either
way. So grafting works: 31 subtrees started from an ancestor somebody
else had already built, rather than rebuilding it. It costs 107s.

That is worth stating plainly because the model said the opposite. The
duplicate-vs-transfer rule (Ahmad & Kwok 1998) says duplicate when
communication dominates computation, and the measured CCR here is
0.003-0.013 - three orders below the crossover, an emphatic "transfer".
The rule is not wrong; the input was. CCR was computed from BYTES, and a
handover in this system is not a byte copy: it is a registry publish
(compress, push) followed by a pull (fetch, decompress, unpack into the
snapshotter) on every machine that wants it. Those constants are large
and they do not appear in a byte count.

The consequence for what to build next: the lever is not smarter
placement, it is a cheaper handover. Placement has now been measured
nine ways and every one has come back null or negative, which is a
consistent enough result to stop testing. A snapshot moved peer to peer
as content, without a registry round trip on either end, changes the
constant that the model was missing. Nothing above it needs to change.

Grafting stays in the code and stays off by default. It is not a bug to
be fixed, it is a mechanism whose price is now known.

### And the same again for the shared cache

`REBUCK2_FLEET_CACHE=readwrite` - buildkit's own registry-backed cache,
imported and exported by every worker - is the third mechanism measured
and the worst: 838s against 504s, with the mean lead going 18.6s ->
27.6s and the longest 134s -> 194s. It is the purest test of the
handover hypothesis, because a shared cache is nothing BUT handover, and
it made every lead slower rather than fewer.

Three mechanisms, one direction. Grafting, prefix cutting and the shared
cache all move built state between machines through an OCI registry, and
each one costs more than the work it avoids. That is no longer three
results; it is one result measured three ways.

### What the numbers say the unit should be

The arithmetic that matters is per-lead, not per-build. 56 leads at a
mean of 18.6s is 1041s of lead time; the same target on one machine is
216s, so an equivalent unit of work costs ~3.9s at home. The dispatched
unit is therefore ~4.8x more expensive than the local one, and average
concurrency across six runners was 2.1 (peak 8). Six machines running at
2.1x concurrency against a 4.8x tax lose, and no placement policy
changes either number.

Both numbers are properties of the UNIT. A fixed per-lead cost - pull the
base, unpack it, warm nothing, throw it away - is amortised by making
leads bigger, and only by that. At earthly's gateway granularity a lead
is one `RUN`-ish subtree, which is far too small to carry it.

Which is the same conclusion the prior-art survey reached from the other
end, and it points somewhere specific: the distribution unit should be
the TARGET, not the LLB subtree. That is also the unit earthbuild's own
CI already uses - twelve `+test-no-qemu-groupN` jobs on twelve runners -
and it is why that arrangement beats this one.

### Scaling is flat, and that settles which number to attack

Same target, same commit, only the machine count moved:

| machines | fleet wall | routed | peak in flight | mean lead |
| -------- | ---------- | ------ | -------------- | --------- |
| 1        | 213s       | -      | -              | -         |
| 2        | 568s       | 50     | 8              | 19.3s     |
| 6        | 504s       | 44     | 8              | 18.6s     |

Tripling the fleet bought 11%. Peak concurrency was 8 either way, and the
mean lead did not move. So the fleet is not machine-limited: four idle
runners were available the whole time and the build could not use them.
Every remaining question is about the tax per lead and the concurrency
the client offers, not about how many machines are in the room.

### Resends are already cheap, so memoising them is not the lever

`[wire] resend cost: 28 resends 2235ms mean, 63 first-sightings 19797ms
mean`.

28 of 91 solves being byte-identical repeats looked like a third of the
build available for free. They are answered ~9x faster than a first
sighting - buildkit's own cache already has them - so the whole
opportunity is worth at most ~60s of a 504s run, and realistically far
less because those seconds overlap other work.

Worth recording as a hypothesis KILLED rather than a fix shipped. It cost
one instrumented run to close, which is the point of paying for the
instrument: the count alone had pointed the opposite way.

## The whole of `+test-no-qemu`, at parity

All fourteen groups, six runners, the largest target attempted:

```text
one machine 186s   6 machines 525s
PARITY: the same 1 target(s) failed either way
gateway solves 163 · routed 130 · peak in flight 14
leads n=148 p50=2943ms p90=17479ms max=209141ms mean=12691ms
op duplication 8.0x sent, 2.3x built
```

It took two fixes to get here, and one of them turned out not to be a fix.

**Corrected.** This section first claimed the h2 collapse was hyper's
`max_pending_accept_reset_streams`, which defaults to TWENTY - plausible,
since earthly cancels every solve in flight the moment a target fails, and
a full run has 16 in flight plus a nested earthly per group. Raising it to
10_000 was followed by one clean run, and that was recorded as proof.

It was one run and it was luck. Setting the limit to `none` - no limit at
all - and the error came back:
`Error: h2 protocol error: error reading a body from connection`, same
place, after the same mass cancellation. So the reset limit was never the
cause, and the run that appeared to confirm it confirmed nothing.

What the evidence does support: earthly is Go, and that phrasing is
hyper's, not Go's. The string reaching the user as earthly's exit error is
a Rust error this proxy RELAYED - a call the proxy made, not one it
served. The server-side connection log staying silent throughout fits the
same reading, where before it was read as exoneration and is not: hyper
answers a tripped limit with a GOAWAY and a graceful close, so
`serve_connection` returns Ok and never logs. Instrumented now on the
upstream side, which is where the next run will say.

The other fix was real, and it was in the measurement rather than
the code: a leg that DIES prints no
`*failed*` markers at all, so it scored zero failed targets and beat a
baseline that had one. The first full run was recorded as a win for the
fleet. It is now judged on the exit code.

### Why this target is the interesting one

Everything before it was measured on a single group, and a single group is
too serial to distribute: demand peaked at 8 concurrent solves, which two
workers (4 slots each) already satisfy. That is the whole explanation for
flat scaling - six machines were never asked for more than two machines'
worth of work.

`+test-no-qemu` is fourteen independent groups and asks for more: peak 14-17
in flight, 148 leads against 56. It is the first workload whose shape could
pay for six machines. It still does not - 525s against 186s - because the
per-lead tax is unchanged, but the ceiling that made the question moot is
gone.

## Affinity: the first mechanism that subtracts instead of adding

Least-loaded-first is load BALANCING, and balancing is the wrong default
for this workload. It spreads work sharing a base across machines, and
every machine that touches a layer pays to pull, decompress and unpack it
into its own snapshotter. `REBUCK2_AFFINITY=1` prefers a peer that already
holds the subtree's ancestry, subject to capacity - the free-slot filter
still runs first, so a warm peer that would decline is still not asked.

Full `+test-no-qemu`, six runners:

| affinity | built duplication | routed | targets touched (base/fleet) |
| -------- | ----------------- | ------ | ---------------------------- |
| off      | 2.2x              | 127    | 51 / 46                      |
| on       | 1.3x              | 167    | 48 / 85                      |

Duplication 2.2x -> 1.3x against an ideal of 1.0, and the fleet routed
MORE while duplicating less. This is the first change here that made the
fleet do more work rather than less.

The wall clocks are not directly comparable and the run says so, which is
the point of measuring work attempted: with affinity the fleet got through
85 targets to the baseline's 48, where without it the fleet managed 46 to
the baseline's 51. Per target that is 577s/85 = 6.8s against the
baseline's 221s/48 = 4.6s - a tax of ~1.5x, where the same arithmetic
before affinity gave ~4.8x.

Both legs stop early (the suite contains targets that fail by design), so
"targets touched" is a coarse proxy - different targets cost differently.
It is not a speed claim. It is the first evidence that the tax is
addressable at all, after three mechanisms that only added to it.

## Consolidation must stay, but the daemon it shares should be local

earthly forwards its own `BUILDKIT_HOST` into every `RUN`, so a nested
earthly shares the outer daemon instead of starting one. Three results
bound the question.

**Turning it off is catastrophic.** `consolidate=0` - the outer address
passed via a config file so nothing is forwarded, and every nested build
starts its own daemon - ran for over an hour against 5-10 minutes
consolidated, and was cancelled rather than finished. So sharing a daemon
is not the problem.

**But the shared daemon is the coordinator.** In a fleet, "the outer
daemon" is one machine's gateway, so a nested build on any worker dials
back to it. The five leads that own a full `+test-no-qemu` critical path
are all nested builds at 231-258s each - each longer than the entire 221s
single-machine build, and clustered the way a queue clusters.

**A local shared daemon is reachable after all.** The patch always carried
the fix - forward `tcp://buildkitsandbox:8372`, a buildkit constant in
every exec's `/etc/hosts`, identical everywhere and resolving locally -
disabled as `if false` because under `NETWORK_MODE=cni` the name resolves
to the exec itself. That was measured on ONE machine, where the CLI's
address is already local, the funnel costs nothing and the safe branch is
free. Re-enabled as a switch, the first full-target run produced ZERO h2
protocol errors, where every previous full-target run collapsed with one.

That last result came with a caveat worth keeping: the same run was
reported as having DIED, and had not. `*failed*` markers are indented
whenever earthly's target column is widened by a long target name, and the
extraction was anchored at column 1 - so it found no failed targets, which
is precisely the signature the died-check looks for. The verdict was an
artefact of the instrument, not the run.

### The local shared daemon works, and costs a test

`sandbox_host=1` forwards `tcp://buildkitsandbox:8372` so a nested earthly
uses the daemon on its OWN machine. Two full-target runs, and both had
zero h2 protocol errors where every previous one collapsed. The second
reported `PARITY: the same 1 target(s) failed either way` - the full
`+test-no-qemu` completing and reporting honestly rather than dying.

It is not a speed win and it is not free:

| group10, 6 runners | baseline | fleet | routed | mean lead | duplication |
| ------------------ | -------- | ----- | ------ | --------- | ----------- |
| plain              | 216s     | 504s  | 44     | 18.6s     | 2.2x        |
| affinity + sandbox | 208s     | 527s  | 30     | 32.6s     | 1.2x        |

Fewer, longer leads: a nested build now runs entirely inside the worker's
lead instead of re-entering the gateway, so the work is relocated rather
than reduced. And `./tests+remote-test` fails through the fleet while
passing on one machine - the first parity break the fleet has caused, and
exactly the hazard the `if false` comment warned about.

So it stays off by default. What it establishes is that the full-target
collapse is caused by the funnel and not by anything unfixable, which
makes the next question worth asking: whether retrying a relayed solve
once on `Unavailable` survives the same mass cancellation without
relocating any work, and so without breaking `remote-test`.

## What the traces say, which is not what the counts said

earthly has been emitting OTLP spans all along and every run logged
`traces export: exporter export timeout` while throwing them away. Collected,
they name targets directly, and one run of the full `+test-no-qemu` gives
two traces - one per leg:

| leg               | target spans | span time | `+base` | `+base` mean |
| ----------------- | ------------ | --------- | ------- | ------------ |
| baseline (274.6s) | 1215         | 9713s     | 575x    | 3.0s         |
| fleet (364.7s)    | 236          | 5250s     | 121x    | 20.5s        |

`+base` resolved 696 times across the job looked like the finding of the
week: 28% of all target time, the same target over and over, obviously
something to memoise. Split by leg it says the opposite. The repetition is
earthly's, not the fleet's - ONE machine does it 575 times and still
finishes in 275s, because there it costs 3s. The fleet does it a fifth as
often at seven times the price.

So there is no redundant work to remove. The fleet's problem has only ever
been the price of a unit, and this is the third instrument to say so
independently: leads at 4.8x their local equivalent, op duplication at
2.2x before affinity, and now a `FROM` at 7x. A `FROM` is a pull and an
unpack, which is exactly the cost a cold machine pays and a warm one does
not.

That is also a warning about aggregate counts. `+base 696x, 4232s` was a
real number, correctly computed, pointing at a fix that would have done
nothing - and it took a per-leg tag to see it. Three instruments have now
had to be corrected before they told the truth; this one was correct and
still misleading until it was split.

## Why the fleet loses: the workload is 71% serial

The traces make the shape plain. Long spans, on a timeline, for one full
`+test-no-qemu`:

```text
baseline 271s              fleet 363s
  +earthly-docker    187s    +earthly-docker    227s   (+40s)
  ...FROM it, apk      5s    ...FROM it, apk     55s   (+50s)
  base chain ends    192s    base chain ends    282s
  TESTS, parallel     79s    TESTS, parallel     81s   (+2s)
```

The parallel phase is the same in both: 79s against 81s. Every second of
the fleet's deficit is in the serial base chain that precedes it.

That chain is three deep and cannot be divided: `+earthly-docker` builds an
image, `+earthbuild-integration-test-base` is `FROM` it plus an `apk add`,
and `+test-base` is `FROM` that. Nothing else in the build can start until
it finishes. 192 of the baseline's 271 seconds are spent there, so the
workload is 71% SERIAL, and Amdahl caps any distribution of it at 1.4x with
infinite machines.

The fleet then makes the serial part 47% longer, and the split says why:
+40s inside `+earthly-docker` itself, and +50s in the step that consumes it,
which is a `FROM` - materialising the parent image on the machine that
builds the child.

This retires the whole placement question. Nine mechanisms were measured
against a critical path that no placement policy can shorten, because the
path is one chain of three targets on one machine either way.

### What would actually win

If the base chain were already present on the workers, the fleet's own work
is the 81-second parallel phase. Against a 271-second baseline that is a
real win, and it is the only shape of win available:

- warm the workers with the base chain in the window they currently spend
  idle - they wait out the entire baseline leg doing nothing
- or seed it faster. The cascade already exists and works: worker 1 finds
  nothing on any peer and pulls 75 blobs from the coordinator, after which
  worker 2 gets 26 of 36 from PEERS. What is missing is that the seed is
  serial - one machine pulls the whole base while five wait for it. Six
  workers each pulling a different sixth, then exchanging, is the same
  bytes off the coordinator at six times the seed bandwidth.

Both attack the 192-second chain. Nothing else on the table does.

## Where this stands, and what each knob is for

Every mechanism is off by default and each is one environment variable, so
any of them can be A/B'd in a single run. What they attack, and what is
known about each:

| knob | attacks | status |
| ------------------------ | ------------------------ | ---------------------- |
| `REBUCK2_AFFINITY` | duplicate materialisation | 2.2x -> 1.2-1.4x built |
| `REBUCK2_MIN_SIBLINGS` | dispatching serial work | untested |
| `REBUCK2_WARM` | the 192s base chain | never yet ran |
| `REBUCK2_LOCAL_NESTED` | the nested-build funnel | untested |
| `REBUCK2_SANDBOX_HOST` | the same funnel | works, breaks one test |
| `REBUCK2_GRAFT` | rebuilding known ancestry | costs 107s, and was ON |
| `REBUCK2_FLEET_CACHE` | rebuilding known ancestry | costs 334s |

Only two of those have a measured benefit, and only affinity is unambiguous.

**And "off by default" was not true of all of them.** `REBUCK2_GRAFT` reads
`${{ inputs.graft || '0' }}`, which cannot be off while the `graft:` input
declares its own default of `"1"`: the value is never empty, so the fallback
never evaluates. Grafting therefore ran in every run after the commit that
turned it off, at a measured cost of 107s, and several comparisons in this
document were taken with it on. `scripts/check-workflow-defaults.sh` checks
the two defaults agree, and found a second instance in the single-runner
workflow the first time it ran.

The honest summary after all of it: the whole of `+test-no-qemu` runs
through six machines and agrees with one machine on the answer, which is
what "distributed buildkit" had to mean first. It is not faster, and the
traces say why in a way no placement policy can address - 71% of the
workload is a serial chain of three targets, and the parallel remainder
costs the same either way.

The two knobs that could still change that both attack the chain rather
than the placement: warming it onto the workers in the window they already
spend idle, and splitting the seed so six machines fetch a sixth each
instead of one machine fetching all of it.

## How big the layers are, and what that implies for splitting the seed

Measured on one full `+test-no-qemu`, from the coordinator's registry:

```text
served 269 requests, 617 MiB
26 blobs over 1MiB, 527 MiB of the total
largest: 65, 64, 63, 47, 37, 36, 31, 24 MiB
```

So the base chain moves as a few dozen large layers, not thousands of small
ones - three of them are ~64 MiB and together are 192 MiB.

That decides the shape of the seed split. Blob granularity is enough: 26
large blobs across six workers is about four each, so the critical path for
seeding the fleet falls from 527 MiB pulled serially by one machine to the
LARGEST SINGLE BLOB, 65 MiB, which one machine must still pull whole. A blob
cannot be range-split without teaching both ends about ranges, and at 65 MiB
against 527 MiB there is no reason to.

The same numbers say what pre-positioning cannot fix. 617 MiB has to be
decompressed and unpacked on every machine that uses it however it arrives,
and that cost is per-machine by construction - see principle 18's limit.
Splitting the seed shortens the wire, and only the wire.

## Lazy pulling is the obvious alternative, and it is wrong for a build

The inverse of pre-positioning is not fetching at all until a byte is read:
a FUSE or EROFS mount presented immediately, with HTTP range requests
behind it. That is stargz / eStargz / SOCI / Nydus, and buildkit supports it
natively (`--oci-worker-snapshotter=stargz`), so a `RUN` can begin executing
before its image has arrived.

The number that makes it compelling is that a container reaches ready state
having touched about **6.4% of its image bytes** (SOCI, USENIX ATC 2023).
Moving 6% instead of 100% is not an optimisation, it is a different problem.

It does not transfer to this workload. A BUILD touches the toolchain: the
compiler, the linker and the header tree between them read 60-90% of that
layer, and the breakeven where lazy pull becomes SLOWER than bulk transfer
is around 80%. Paying FUSE round-trip latency per read, to avoid moving
bytes you are going to read anyway, is a loss - and the `apk add` in
`+earthbuild-integration-test-base` is exactly the shape that reads most of
what it was given.

So the family is right: move the bytes in bulk, early, to the machines that
will want them. What is worth stealing from that ecosystem is narrower -
containerd's `content.Store` write API lets a third party push blobs
directly into a worker's local store, which is a real push channel rather
than an advisory hint that the worker then has to pull through a registry.

## The h2 collapse was our own client connection

Every full `+test-no-qemu` for two days ended the same way:

```text
Error: h2 protocol error: error reading a body from connection
```

reported by earthly, naming nothing. Five fixes were aimed at it before the
cause was known, and all five were on the wrong side of the wire:

| attempt | reasoning | outcome |
| -------------------------------- | ------------------------------ | ------------------ |
| `max_concurrent_streams(None)` | our server refuses a stream | error persisted |
| `max_pending_accept_reset_streams` 20 -> 10k | mass cancellation trips a GOAWAY | one clean run, then it returned |
| the same limit removed entirely | prove it either way | error persisted |
| retry `Control.Solve` on Unavailable | the failing call is the solve | never fired, wrong code |
| retry on `Unknown: transport error` | it is the solve after all | fired 4x, 3 cancelled |

The answer came from instrumenting the SESSION relay, which had been
swallowing its errors - `m.ok()` turning a stream error into a clean
end-of-stream:

```text
[proxy] session stream from daemon failed: Unknown error
h2 protocol error: error reading a body from connection
```

**From the daemon.** The connection that breaks is the one this proxy MAKES
to peer 0's buildkitd, and the text reaching the user is that error relayed
outward. Every fix had been applied to the server we run.

The cause is structural rather than a limit: ONE h2 connection carried the
whole Control surface - 163 gateway solves, `Control.Solve`, `Status`, and
the long-lived `Session`. A connection-level event takes every stream on it,
and `Session` is the one that cannot be retried: it carries filesync and
credentials, so the build dies with it.

Session now has its own connection.

The lesson generalises past this bug. A relay that drops errors is not
quiet, it is lying, and it took five attempts at the wrong component before
anyone asked the relay what it had seen. Principle 17's third shape - an
instrument that cannot be distinguished from its own silence.

## Warming inverts the argument for keeping serial work at home

The warm-up builds the base chain on each worker in the window they would
otherwise spend idle - `+earthly-docker` through to `SAVE IMAGE`, ~500 lines
of build output per machine, `warm exit=0` on all six. Each worker therefore
enters the fleet leg with the chain already in its own daemon cache.

That inverts the `MIN_SIBLINGS` argument. Its reasoning was: the base chain
is serial, so dispatching it gains nothing and pays a handover - keep it
home. True on a COLD fleet. On a warm one the chain is not work at all on a
worker, it is a cache hit, so dispatching it is the fastest thing available
and keeping it home means the coordinator builds all 192 seconds of it.

**Corrected.** I wrote here that the coordinator stays cold because its two
legs use different daemons. They do not. The baseline dials
`tcp://$LAN:18372` and the proxy's upstream is `http://127.0.0.1:18372` -
the same `own-bk` container. Peer 0 therefore enters the fleet leg having
just built the entire target on that daemon: it is the WARMEST machine in
the fleet.

Which makes `REBUCK2_HOME_SLOTS=0` look very different. It exists to force
dispatch - "otherwise peer 0 takes the work itself whenever it has room,
which on an idle runner is always" - and what it actually forces is work OFF
the only machine that already has the answer cached, and ONTO cold ones.
Every fleet leg measured so far has been shipping work away from a warm
daemon to machines that must rebuild or refetch it.

That is not a small confound. It is a plausible explanation for the entire
deficit, and it is a property of the harness rather than of distribution:
running the baseline first, on the same daemon, is what warms peer 0.

So the two mechanisms are not additive and may be opposed:

| configuration      | who builds the base chain | expected          |
| ------------------ | ------------------------- | ----------------- |
| cold, dispatch     | a worker, from nothing    | 192s + transfer   |
| cold, MIN_SIBLINGS | the coordinator           | 192s, no transfer |
| warm, dispatch     | a warm worker: cache hit  | ~0                |
| warm, MIN_SIBLINGS | the cold coordinator      | 192s              |

If that table is right, `warm=1` wants `MIN_SIBLINGS=0`, and the two should
never be measured together without saying which is expected to dominate.

## The honest comparison: still 2x, and the confound was not the cause

With the baseline moved to its own daemon, so the fleet's coordinator starts
as cold as the machine it is measured against:

| measure | baseline | fleet, 6 machines |
| --------------- | ------------ | ---------------------------- |
| daemon | fresh, 28372 | cold coordinator + 6 workers |
| wall | 214s | 441s |
| targets touched | 49 | 53 |
| failed targets | 1 | 1 |

Both legs completed, both failed the same one target, and for the first time
they attempted comparable work. The fleet is 2.06x slower.

So the warm-coordinator confound was real and was NOT the explanation. That
matters more than it sounds: it was the best remaining candidate for a
harness artefact hiding a genuine win, and removing it changed the ratio
from 1.8x to 2.06x - the wrong direction for that hypothesis. What is left
is the workload, and the workload is 71% serial.

### And the session errors were never errors

Four sessions "failed" per run, and the fix for it went through five
components. The lifetimes end the question:

```text
session 60j9x98... after 22598ms
session xp4n2dt... after 22728ms
session ffdofjc... after 22746ms
session syrwnny... after 439474ms
```

Three inside 150ms of each other, on SEPARATE connections, which is not a
connection fault: it is three nested earthlys that started together and
exited together. The fourth is the outer session, ending when the build did.

A stream ending because its client went away is the normal case. Logging it
as a failure sent five fixes at the wrong component over two days, and the
line now says "ended".

## Warming cannot work, because portability changes the cache keys

The warm-up succeeds - all six workers, `warm exit=0`, the whole base chain
built locally - and the fleet is no faster. The per-lead bytes say why:

```text
job 22 took 37626ms (92 ops, 362249 KiB fetched)
job  3 took  3659ms  (4 ops,  45573 KiB fetched)
```

A warm worker fetched 354 MiB. If its cache were usable those layers would
already be present, so the cache is being missed entirely.

The reason is structural rather than a bug. A dispatched graph is made
PORTABLE first: `local://` sources become published context images, base
images become mirrored refs. That rewrite changes the op bytes, so it
changes their digests, so it changes every cache key beneath them. A worker
warmed by running earthly directly has a cache keyed on the UNREWRITTEN
graph, and the two can never meet.

This is not fixable by warming harder. Any warm-up that runs an ordinary
build produces ordinary cache keys, and the fleet only ever asks for
rewritten ones.

What follows is that pre-positioning has to move the exact blobs the
rewritten graph names - which is what `REBUCK2_PREFETCH` does, and is now
the only member of this family left untested. It does not need cache keys to
match: it puts the bytes where the pull will look, and the pull is
`docker-image://mirror/x@sha256:...` by digest.

## The h2 collapse: five theories eliminated, by evidence

Two days, five fixes, none of which changed the symptom. Recorded because
the eliminations are worth as much as a cause would be, and because each
theory was plausible when it was formed:

| theory | why it looked right | how it died |
| ------ | ------------------- | ----------- |
| our server refuses a stream | the client reports a protocol error | `max_concurrent_streams(None)` changed nothing |
| hyper's reset limit (default 20) | earthly mass-cancels on failure | raised to 10k, then removed entirely; error persisted |
| the failing call is `Control.Solve` | it is what the client blocks on | retried; the retry itself was cancelled |
| one connection carries everything | streams share a fate | Control, gateway pool, per-Session - unchanged |
| the daemon is dying | simultaneous unrelated failures | `own-bk: running exit=0 oom=false restarts=0` |

The session relay, once instrumented, said the failing connection is the one
this proxy MAKES rather than the one it serves - and then the session
lifetimes said those "failures" were nested builds ending normally. So the
error the user sees is relayed from somewhere that is not obviously broken.

What is left, and what the evidence now points at, is a NotFound:

```text
[proxy] upstream solve failed: Some requested entity was not found
[proxy] upstream Control.Solve failed: Unknown error transport error
```

One of each, in every failing run. A NotFound is a graph referring to
content that is not there - which two mechanisms can cause, because both
rewrite a graph to point at an image that must already exist. Grafting was
the obvious suspect and has been eliminated: `REBUCK2_GRAFT: 0`, `grafted:
0`, NotFound unchanged. `cut_prefix` is the other, and is on by default.

### routed collapses when prefetch is on -- WRONG, retracted below

| run | prefetch | routed |
| ----------- | -------- | ------ |
| 31472465352 | 0 | 94 |
| 31474229913 | 1 | 7 |
| 31475431680 | 1 | 7 |
| 31476911139 | 1 | 8 |

Not subtle, and not noticed for three runs because the wall clock and the
h2 error were what was being read. Two readings, and they are not
equivalent: either prefetch occupies workers so they decline leads, or it
stalls the dispatch path itself - `subtree_built` now takes `job_terminal`
and then `op_by_worker` on the completion path, and `place_subtree` takes
`op_by_worker` too, which is the shape a lock-order problem has.

Worth recording separately from the h2 question, because it is a
regression introduced by a mechanism rather than a pre-existing fault, and
because a fleet that routes 8 solves is not a fleet.

**Retracted.** A later run with `prefetch=0` also routed 8, which kills the
correlation. Across eight runs:

| configuration | routed |
| ------------- | ------ |
| warm=1, four runs | 21, 29, 54, 94 |
| no warm, four runs | 7, 7, 8, 8 |

and even that is probably not a mechanism: the warm runs survived 267-441s
while the others died at 209-223s, so they simply had longer to route.
`routed` tracks how long the leg lived. Four data points and a plausible
story were enough to convince me of a regression that is not there - the
same error as the `+base` aggregate, which was also real, correctly
computed, and pointing the wrong way.

The lock held across an await was a genuine bug and the fix stands: a guard
taken in an `if let` scrutinee lives for the body, and this one was held
while taking two further locks. It is a latent deadlock. It is not the
explanation for anything measured.

## The h2 collapse: solved, and it was our own keepalive

Confirmed by A/B on the full `+test-no-qemu`, one variable:

| | keepalive 20s | keepalive off |
| --- | --- | --- |
| h2 protocol errors | every run | 0 |
| `no such job` | 9 | 0 |
| solves routed | 8 | 147 |
| outcome | died | completed, 1 failed target, as the baseline |

The mechanism, from grpc-go's `http2_server.go` as vendored into the
buildkitd we talk to:

```text
maxPingStrikes     = 2
defaultPingTimeout = 2 * time.Hour
```

With no active streams and `PermitWithoutStream` false, every ping inside
two hours is a strike, and three strikes sends GOAWAY with
ENHANCE_YOUR_CALM and `too_many_pings`, closing the connection. A 20-second
keepalive kills an idle connection after exactly three pings - 60 seconds -
which is the boundary every failure landed on. With active streams the bar
is `EnforcementPolicy.MinTime`, five minutes by default, so anything faster
is fatal either way.

Two readings that were wrong for two days:

- `transport error` is TONIC's string - `Kind::Transport` - not the
  daemon's. Read as an answer from buildkitd, it produced five theories
  about a server that was behaving exactly as documented.
- the `no such job` NotFounds were consequences, not causes. Nine of them,
  all for one job id, after that job's `Control.Solve` had already died.

And the collapse in routing - 8 solves against the usual 40-130 - was the
same fault, not the prefetch regression it was briefly recorded as. The
build died before it could dispatch.

The keepalive was added as a FIX for this symptom, in the commit that gave
Session its own connection. It caused the failure it was meant to prevent,
for a dozen runs.

## Affinity and prefetch are in tension by construction

Measured on the first fleet that completes. Prefetch works: `local=3` on a
worker, the first non-zero local-hit count in this project, so a
pre-positioned blob does turn a registry pull into a local one. The stated
criterion is met.

The shares are almost empty though - `prefetched 0/0`, `prefetched 1/1` -
and the reason is structural rather than a bug. Prefetch announces a subtree
result only when at least two workers have been sent the op that produced
it, because pushing a leaf result to five machines that will never read it
is bandwidth spent for nothing. Affinity's entire purpose is to make each
op land on ONE machine: it took duplication from 2.2x to 1.0-1.4x.

So affinity drives the consumer count towards one, and the prefetch gate
fires at two. The better affinity works, the less there is to pre-position.
With affinity on, prefetch reduces to base images - which are announced
unconditionally from `make_portable`, since a base is shared by
construction.

That is not an argument against either. It says the gate is asking the wrong
question: "how many machines HAVE been sent this" is a measurement of the
past, and pre-positioning needs a prediction. The base chain is needed by
everyone before anyone has asked for it, which is exactly why the
unconditional base-image path is the one that fires.

Wall clock is not the discriminator here: 483s against a 219s baseline with
prefetch, 508s against 285s without. The baselines differ by 30% between
runs, which is larger than any effect being looked for.

## How much of the Earthfile has actually been through this

The standing goal is larger and larger parts of it, so here is the ledger.

| target                   | shape                                  | status                                                    |
| ------------------------ | -------------------------------------- | --------------------------------------------------------- |
| `+test-no-qemu-group1`   | one group, nested earthly, WITH DOCKER | parity, locally                                           |
| `+test-no-qemu-group2`   | the same, one group                    | parity, in CI, six machines                               |
| `+test-no-qemu-group10`  | the same, one group                    | where graft and cut-prefix were measured                  |
| `+test-no-qemu` (all 14) | 14 groups on one 192s base chain       | completes, parity, 508s vs 285s                           |
| `+all-binaries`          | 5 cross-compiles off one `+code` stem  | **green both legs**, 262s vs 712s, 2.3x ceiling           |
| `+lint-all`              | 3 independent lint targets, no docker  | parity, 88s vs 252s after the retry fix (was 628s)        |
| `+all-buildkitd`         | multi-arch buildkitd, needs qemu       | **parity**, 1161s vs 1188s - arm64 half is undispatchable |

Everything above the last line is the same shape wearing different numbers:
a long serial base chain, then nested earthly builds that each want a 600
MiB image. That shape caps at 1.4x (principle 19), and four entries of it is
four measurements of Amdahl.

`+all-binaries` is the first structurally different one in the repo:

- five leaves, genuinely independent, each a Go compile of minutes
- one shared stem (`+code`), which cut-prefix already handles - and it is
  cut once, behind the same `OnceCell` the contexts use, so the four later
  solves await the first rather than each rebuilding it
- no nested earthly, so no `BUILDKIT_HOST` forwarding and no second daemon
- the result of each leaf is `alpine` plus one binary, tens of MiB, against
  the 617 MiB the test groups move
- different `GOOS`/`GOARCH` share almost nothing in the go-build cache, so
  the one-machine baseline pays for five near-cold compiles in a row rather
  than one and four cheap ones

Still untried, in rough order of how much they would add:

| target      | why it is interesting                      | why not yet                         |
| ----------- | ------------------------------------------ | ----------------------------------- |
| `+all`      | `+all-binaries` plus two multi-arch images | wants `+all-binaries` to pass first |
| `+lint-all` | three independent, cheap, no docker        | small, but a good smoke target      |
| `+test`     | `+test-no-qemu` plus the qemu legs         | qemu on a runner is its own fight   |

## Lifting a cache mount does not make it free. Measured

`+test-no-qemu-group2`, six machines, prefetch on, affinity **off**:

|                |                                                            |
| -------------- | ---------------------------------------------------------- |
| baseline       | 259s, 0 failed                                             |
| fleet          | 789s, 1 failed (`copy-test-verbose-output`, the known one) |
| solves         | 157 seen, 130 routed, 13 kept home (`Insecure`)            |
| peak in flight | 22 subtrees at once                                        |
| leads          | 142; p50 8.6s, p90 19.5s, max **148.8s**                   |
| op duplication | 6863 sent / 389 distinct, **2.9x built**                   |
| registry       | 448 MiB served, 22 blobs over 1 MiB                        |

Dispatch is not the problem. Twenty-two subtrees in flight at once is a
fleet working; 3x slower than one machine while doing that is something
else, and two rows say what.

**2.9x built.** Nearly three machines executed each distinct op. Affinity
takes that to 1.0x and had been left off because the gain looked marginal
against op counts.

**The cache mounts.** Lead time attributed to each cache id the lead named:

| cache id       | lead time   | leads |
| -------------- | ----------- | ----- |
| `go-mod`       | 1,524,789ms | 64    |
| `/go/pkg/mod`  | 1,443,534ms | 58    |
| `/root/.cache` | 1,443,534ms | 58    |
| `go-build`     | 1,429,289ms | 59    |

These rows OVERLAP - a lead naming all four appears in all four - so they do
not sum, and the first read of this table produced "5,841,146ms of cache
cost" for a fleet leg of 789 seconds. The report now says so, and prints the
once-per-lead total beside it.

Read correctly it is still the headline: ~64 leads, ~24s each, all of them
behind a Go cache mount. `REBUCK2_PEER_CACHE_MOUNTS=1` is what makes those
subtrees dispatchable at all - a cache mount otherwise grounds a subtree to
the machine holding it. But lifting the hazard does not lift the cost. It
moves it: the worker builds against its OWN cache mount, which is cold, so
`go mod download` and `go build` redo work the coordinator had already
cached.

That reframes affinity entirely. It was measured against duplication and
filed as marginal. Its real value is that a cache mount is **per-worker
state**, and affinity is **worker stickiness** - the same op keeps meeting
the same warm cache. The two mechanisms are in agreement, and nobody had
measured the pair.

So affinity is on by default from here, and the next run is the A/B. If it
does not move the 24s leads, the conclusion is that lifted cache mounts are
too expensive to dispatch behind and the policy should be to keep
cache-mounted subtrees at home - which is the opposite of what
`REBUCK2_PEER_CACHE_MOUNTS` was built to allow, and would be worth knowing.

## Warming was buried for the right reason and the wrong scope

The earlier finding - *Warming cannot work, because portability changes the
cache keys* - is correct and stays. Making a graph portable rewrites the ops
that name a local context or a base image, every digest downstream of them
changes, and buildkit's cache is keyed on exactly those digests. A warm
worker cannot key-match a dispatched graph. One measured warm worker still
fetched 354 MiB, which is what that looks like.

It applies to buildkit's **op cache**. It does not apply to **cache
mounts**, and the previous section puts most of the time there.

A cache mount is a directory, not a cache key:

|                   | keyed on                     | survives a rewritten digest?   |
| ----------------- | ---------------------------- | ------------------------------ |
| buildkit op cache | the op digest, transitively  | no - that is the whole finding |
| `go-mod` mount    | module path and version      | yes; `go` looks it up itself   |
| `go-build` mount  | the compiler's own action id | yes                            |
| `npm` mount       | package name and version     | yes                            |

So warming cannot make a worker SKIP an op. It can make the op it runs
cheap, and against ~64 leads at ~24s each that is the larger of the two
prizes. The two claims are not in conflict; the first was simply stated
about the whole of caching when it is true of half of it.

Which also picks the warm-up target. If the prize is a filled mount rather
than a cache hit, warm with the cheapest target that fills the expensive
mounts - `+deps`, which is `go mod download` into `/go/pkg/mod` behind
`id=go-mod`, the costliest id in the table. The base chain fills them too
and takes minutes about it. The default moved accordingly.

Still off, because affinity is the variable under test and two at once is
neither.

## `+all-binaries`: the ceiling doubles, and the fleet still loses

First run of the structurally different target. Six machines.

|                    | baseline | fleet                                   |
| ------------------ | -------- | --------------------------------------- |
| wall               | 262s     | 712s                                    |
| failed targets     | 0        | **0 - the same, so parity**             |
| solves             |          | 33, all 33 routed, 0 home               |
| peak in flight     |          | 6                                       |
| occupancy          |          | 2.29                                    |
| **Amdahl ceiling** |          | **2.30x**                               |
| leads              |          | 34; p50 **47.6s**, max 224s             |
| op duplication     |          | 612 sent / 101 distinct, **3.4x built** |

Two things are confirmed and one is now unavoidable.

**Confirmed: the workload is what principle 19 said it would be.** A 2.30x
ceiling against 1.4x for the test groups, occupancy 2.29 out of a possible
2.30 - the fleet is extracting essentially all the parallelism this graph
contains. Peak 6, so every machine had work. Nothing about dispatch is
failing here.

**Confirmed: it builds correctly.** Zero failed targets either way. That is
the first target in this ledger that is green on both legs rather than
equally red, and it is a stronger parity statement than any group has made.

**Unavoidable: the baseline's advantage is one shared warm cache mount.**
p50 lead 47.6s against a 262s baseline that produced all five binaries. One
machine cross-compiling five platforms downloads the module graph ONCE into
one `/go/pkg/mod` and reuses one `go-build` cache; six machines each pay for
their own, and 3.4x built duplication says most ops were materialised on
three of them. Two structurally different workloads now fail the same way.

And it puts affinity in an awkward position. Affinity for cache mounts
prefers the worker that already has the warm one - which for five
independent leaves means piling all five onto one machine, i.e.
reconstructing the baseline. Concentrating is right when only one mount is
warm; it is a workaround for the mount not being available anywhere else.

So the mount has to become something a cold worker can be GIVEN. That is not
a metaphor: buildkit's `getRefCacheDirNoCache` creates a cache dir as a
copy-on-write ref over `Mount.input` when the dir does not exist, so an LLB
cache mount can carry a starting image. The Earthfile never has to know
(principle 15) because we are the ones rewriting the graph.

Footnote on the byte-parity check, which reported nothing: `+all-binaries`
reaches the per-platform targets through `COPY`, and earthly only writes
`SAVE ARTIFACT ... AS LOCAL` for targets named on the command line. So there
were genuinely no local files to hash, on either leg, and the check
correctly said nothing rather than passing vacuously.

## The h2 error is the daemon dying in ReadFile, not our connection

Sixth theory, and the first with a stack trace attached.

`+test-no-qemu-group2`, affinity on. The fleet leg reached 14 of 15 targets,
named zero failed targets, and then earthly stopped with

```text
Error: h2 protocol error: error reading a body from connection
```

Our proxy logged one matching event - a session ending after 6673ms with the
same message - and `own-bk` was `running exit=0 oom=false restarts=0`, so
none of the "the daemon went away" theories fit either.

But the daemon's log ended in a goroutine dump, and the frame above the
gRPC plumbing is:

```text
github.com/moby/buildkit/frontend/gateway/pb._LLBBridge_ReadFile_Handler
    /src/frontend/gateway/pb/gateway.pb.go:3241
google.golang.org/grpc.(*Server).processUnaryRPC
```

So the daemon blew up inside a gateway **ReadFile**, which is a call this
proxy relays, and the h2 error is the downstream consequence rather than the
event. That is a different fault to any of the five already eliminated -
keepalive, reset limits, connection sharing, session isolation, the daemon
being killed - and it is the first one that names a method.

The obvious suspect was that earthly reads results at the END of a run and
reads them against refs from solves we ROUTED - a ref belonging to a build
another machine did, which this daemon never created. **Refuted before it
could become theory number seven**, by reading the handler rather than
reasoning about it: `getImmutableRef` returns
`no such ref: %s, all %+v` for an id it does not hold, and the nil path
below it returns `os.ErrNotExist`. An unknown ref is an error, not an
unwind, and `LocalMounter(nil).Mount()` does not fault either - `lm.mounts`
stays nil and `mount.All(nil, dest)` is harmless.

So the trigger is something else in that handler, and the honest position is
that it is not yet known. Five theories have already been eliminated by
guessing the right grep before the run; guessing a sixth from a truncated
stack is how this hunt has been going wrong. The panic header decides it.

What is missing is the panic line itself: `docker logs --tail 25` captured
the bottom of the stack and cut off the header that says why. Widened, and
the daemon log is now an artifact - five theories were eliminated by
guessing the right grep before the run, and this is the run that showed
guessing has a limit.

Affinity, from the same run, and it works:

|                       | before | after                     |
| --------------------- | ------ | ------------------------- |
| op duplication, built | 2.9x   | **1.9x**                  |
| `affinity` applied    | -      | 74                        |
| peak in flight        | 22     | 17                        |
| occupancy             | -      | 3.49                      |
| cache-mount lead time | -      | 1,817,318ms over 51 leads |

Wall clock is not readable from this run - the leg died - so affinity is
confirmed on duplication and unproven on time.

## What each CI branch is for

`workflow_dispatch` is unreachable for a workflow that lives off the default
branch - the API resolves a workflow by its presence there - so an
experiment is chosen by where it is pushed.

| branch                            | target                              | what it asks                                    |
| --------------------------------- | ----------------------------------- | ----------------------------------------------- |
| `giles-dispatch-ci`               | `+test-no-qemu-group2`              | the default rig; 1.4x ceiling                   |
| `giles-dispatch-ci-binaries`      | `+all-binaries`                     | five independent compiles; 2.3x ceiling         |
| `giles-dispatch-ci-lint`          | `+lint-all`                         | three cheap independent targets, no docker      |
| `giles-dispatch-ci-buildkitd`     | `+all-buildkitd`                    | multi-arch buildkitd; the qemu path             |
| `giles-dispatch-ci-all`           | `+all`                              | binaries plus two multi-arch images             |
| `giles-dispatch-ci-seed`          | the DEFAULT target                  | seeded mounts, so it is comparable with `-ci`   |
| `giles-dispatch-ci-lint-seed`     | `+lint-all`, seeded                 | the same, fifteen minutes instead of forty      |
| `giles-dispatch-ci-ast`           | `+test-ast`                         | three groups, no docker, nothing red on purpose |
| `giles-dispatch-ci-lint-graft`    | `+lint-all`, grafting               | can a WARM bank make grafting pay?              |
| `giles-dispatch-ci-buildkitd-arm` | `+all-buildkitd`, two arm64 workers | can the emulated half run natively?             |

`-seed` keeps the default target deliberately. It selects a mechanism rather
than a workload, and a mechanism has to be measured against the same graph
it is meant to help.

## A failing target cost six times what it should. Measured on `+lint-all`

`+lint-all` was added as a cheap smoke target - three independent lint
targets, no docker, no cache mount worth naming. It produced the worst ratio
in the ledger and the clearest cause.

|                | baseline | fleet                                  |
| -------------- | -------- | -------------------------------------- |
| wall           | **92s**  | **628s**                               |
| failed targets | 1        | 1 - the same one, so parity            |
| solves         |          | 19 seen, 15 routed, 4 home             |
| op duplication |          | 290 sent / 62 distinct, **1.3x built** |
| occupancy      |          | 3.63, peak 8                           |

1.3x duplication and occupancy 3.63 - by every measure of *dispatch* this is
the best run recorded. And it took nearly seven times the baseline.

The per-solve lines say where:

```text
solve 15 : 105242..541322  total 436080
solve 16 : 105243..569311  total 464068
solve 17 : 105239..589879  total 484640
solve 18 : 105244..593286  total 488042
```

Four solves, all starting in the same millisecond, each running seven to
eight minutes, in a build whose single-machine total is ninety-two seconds.
And `not routed`:

```text
{"fleet: build failed: solve: ... golangci-lint run ... exit code: 1": 4}
```

The same failure, four times. `+lint` fails on purpose in this tree, and
`subtree_declined` re-offered it on any reason at all: peer 1 ran the whole
lint and failed, peer 2 ran the whole lint and failed, and so on down the
placement order before the requester finally built it at home and failed
too.

**A single machine pays a deterministic failure once. The fleet paid it per
peer.** Nothing about the scheduler was wrong - it placed the work well, by
its own numbers better than in any other run. It simply had no way to tell
"this peer could not" from "this build does not succeed".

The distinction is the same one `worth_retrying` draws for connections, one
level up, and the bias has to point the same way: retrying a verdict costs
time, refusing to retry a machine fault costs the build. So a verdict is
matched on buildkit's own framing - the container ran and the process exited
non-zero - and everything else stays retryable.

Worth stating plainly, because it changes how the other numbers read: this
suite contains targets that fail on purpose, and every fleet measurement
taken on `+test-no-qemu-*` includes at least one of them. Some part of every
"the fleet was slower by N seconds" in this document is this bug.

## What `+lint-all` would have cost without the retry storm

From the same run's per-solve windows, so this is arithmetic on a
measurement rather than a new measurement.

Every solve that was not the failing lint had finished by **112.6s**:

```text
solve 11 : 105242..107094
solve 12 : 105243..108437
solve 13 : 105244..111914
solve 14 : 105242..112645     <- the last honest one
solve 15 : 105242..541322     <- the lint, retried around the fleet
```

Against a 92s baseline, that is **1.2x slower, not 6.8x**. Everything the
fleet did with work that could succeed finished within a quarter of the
baseline's own time of it.

The failing target does not become free, and the fix does not make it free.
A verdict now goes straight to `unplaced`, so the cost is one peer attempt
plus one build at home - roughly 220s on this run's numbers - where the
baseline pays one attempt. **The fleet pays a deterministic failure twice.**

Whether that second attempt is necessary is a real question and not settled
here. The home rebuild exists because the peer ran a REWRITTEN graph: local
contexts became images, base images were repointed at our mirror, and this
project has produced failures from exactly that (`no active sessions`,
`security.insecure is not allowed`). Reporting the peer's error would then
mean reporting a failure the client's own daemon would not produce - red
where the truth is green, which is the one direction principle 5 forbids
absolutely.

A precise version exists and is worth building: if the dispatched graph was
byte-identical to the client's and the peer's platform matches, the peer ran
the same thing on the same architecture and its verdict is the client's.
That check is cheap - `portable == def` is already known at the dispatch
site.

One correction to record, because it was shipped wrong for an hour: the
first predicate matched **any** `exit code:`, including 137. That is
SIGKILL, and on a build runner it is nearly always the OOM killer. Treating
it as a verdict would turn "this worker ran out of memory" into "your build
fails", which is the confusion the predicate exists to prevent, pointed at
the one case where another machine genuinely could do better. 128+N is a
signal and stays retryable.

## The verdict fix, measured: 628s -> 252s

Same target, same rig, one change.

|                       | before        | after         |
| --------------------- | ------------- | ------------- |
| baseline              | 92s           | 88s           |
| fleet                 | **628s**      | **252s**      |
| ratio                 | 6.8x          | **2.9x**      |
| `verdict_stops_retry` | -             | **4**         |
| parity                | same 1 failed | same 1 failed |
| op duplication        | 1.3x          | 1.3x          |
| occupancy             | 3.63          | 2.92          |

```text
[driver] subtree job 14 FAILED on worker 2 - not re-offering, the build itself did not succeed
[driver] subtree job 18 FAILED on worker 2 - not re-offering ...
[driver] subtree job 20 FAILED on worker 2 - not re-offering ...
[driver] subtree job 21 FAILED on worker 2 - not re-offering ...
```

Four, which is the number of Go modules `+lint` loops over, each with its
own failing `golangci-lint`. The mechanism counter agrees with the log and
with the `not routed` table, which is the check that stopped three earlier
mechanisms being credited with work they never did.

**What is left is the other half of the same bug.** 252s against 88s, and
the leads only account for about 79s of wall at this occupancy. The rest is
four home rebuilds of a target that already failed on a peer: a verdict goes
to `unplaced`, the requester builds it, and it fails again. Roughly 40s
apiece, roughly 160s, which is most of the remaining gap.

The obvious fix is to pass the peer's error to the client instead. The
obvious fix is also the one that can report red for a build that is green,
so it needs a condition rather than a decision. `verbatim` - the dispatched
definition being byte-identical to the client's - is most of it, and cheap:
`portable == def` at the dispatch site.

It is not all of it, and `+lint` shows why. Its three cache mounts are
lifted, so the peer ran the client's exact graph against a COLD `go-mod`,
and a cold `go-mod` downloads modules. A network blip there is `exit code:
1` from `go mod download` - a transient, reported as a verdict, on a build
that would pass anywhere else. Principle 20's distinction again, from the
other side: seeding a self-keying cache is safe because a wrong entry is
never found, and TRUSTING a verdict from a cold one is a different claim
entirely.

So it goes behind a flag, off by default, and gets measured rather than
argued about.

## What seeding should and should not be expected to buy

Written before the first seeded run returns, because the cost table cannot
settle it afterwards: a lead is counted under every cache id it names, so
all four ids score nearly the same seconds and none of them is separable
from the others. Only seeding subsets across runs can tell them apart.

The prior, so the result can disagree with something:

| cache                        | what a cold one costs                                                   | seed likely to pay? |
| ---------------------------- | ----------------------------------------------------------------------- | ------------------- |
| `go-build`                   | recompiling the dependency tree - CPU, minutes                          | **yes**             |
| `/root/.cache/golangci_lint` | re-type-checking every package                                          | **yes**             |
| `go-mod`                     | `go mod download` - network, and the network on a GitHub runner is good | **doubtful**        |

`go-mod` is the big one and probably the least worth shipping. Its
alternative is a fetch from the module proxy, which on a hosted runner is
fast; the seed is hundreds of megabytes that every worker must pull AND
unpack. Principle 18's last clause applies exactly here - pre-positioning
shortens transfer, not unpack - and a cache whose miss path is a fast
download is the worst trade available.

`go-build` and `golangci_lint` are the opposite: their miss path is CPU that
no amount of bandwidth avoids, and it is paid per worker per module.
`+lint-all` loops over four Go modules, so the baseline fills those caches
once and reuses them three times while each peer starts empty. That is the
best explanation on offer for a lint costing the baseline ~15s and a peer
~40s.

So the failure this predicts, if seeding does not pay: the whole thing is
dominated by shipping and unpacking `go-mod`, and the answer is to seed the
compute caches and let the module cache download itself.

## `+lint-all` is a low-variance target, and that was worth finding out

Two runs of the same commit, six machines each:

| run | baseline | fleet |
| --- | -------- | ----- |
| a   | 88s      | 252s  |
| b   | 86s      | 249s  |

2.3% apart on the baseline, 1.2% on the fleet. The 30% figure this document
has been quoting is from `+test-no-qemu`, and it does not transfer: a target
whose whole build is ninety seconds of CPU on a fixed graph is a far
steadier instrument than one that pulls 617 MiB and runs nested earthly
fourteen times.

That partly retracts the argument for the within-run arm comparison. It was
justified here as "a 10% effect between two runs is unreadable", which is
true of group2 and false of `+lint-all`, where 5% is readable. The
within-run split is still the better instrument - it removes the whole
question rather than bounding it, and it separates per-id value that no
across-run comparison can - but it is not the only one available, and the
cheap experiment is now cheap enough to just run twice.

Practical consequence: **use `+lint-all` for anything that needs a number,
and group2 only for things that only group2 exhibits.** Fifteen minutes and
±2% beats forty minutes and ±30% for every question either can answer.

Also confirmed in the same pair: the guard markers work.
`read_retry=0 (never needed)` where the previous run said
`read_retry=ON BUT NEVER APPLIED`, and `verdict_stops_retry=4` both times -
the same four Go modules, the same four failing lints.

## Every number here is a cold-start number, and a build farm is not cold

Worth stating plainly, because it bounds what any of this proves.

A GitHub hosted runner is fresh. Each worker joins with an empty
snapshotter, an empty cache mount, and no image layers at all, and it is
torn down at the end. So every measurement in this document is the **first
build a fleet ever does**, repeated.

That is the worst case for exactly the amplification sources principle 21
lists:

| source            | cold                                  | warm, a real farm                  |
| ----------------- | ------------------------------------- | ---------------------------------- |
| cold cache mounts | every worker fills its own, every run | filled once, then reused for weeks |
| transfer          | every layer crosses the wire          | most layers already present        |
| unpack            | every worker decompresses everything  | only what changed                  |
| duplication       | unchanged - a property of the graph   | unchanged                          |

Three of the four disappear on a second build. Duplication does not, which
is why affinity was worth doing regardless.

A permanent fleet - the thing this is for - is warm after its first hour.
The honest statement of what has been measured is therefore: **the fleet is
about 2.5x slower than one machine when every machine involved has never
seen the project before.** Not "the fleet is slower".

This rig cannot measure the warm case. Runners are destroyed after every
job, and the bank (`~/.cache/rebuck2/coord`, restored per target) carries
the coordinator's built-op refs across runs but nothing of a worker's
snapshotter or its cache mounts - those live in a container that no longer
exists.

Two consequences worth keeping:

- **Seeding is the warm case, simulated.** That is what makes it the right
  thing to chase: it hands a cold worker the state a warm one would already
  have, which is the difference between the two columns above.
- **Do not fix the cold case at the warm case's expense.** Anything that
  helps a first build by adding per-build work - re-shipping a cache every
  run, say - is a loss on every build after it, and this rig would report it
  as a win.

## Cache-mount seeding works, and what buildkit's mounts actually do

Proven, against a real daemon, by `scripts/seed-check.sh`:

```text
[check] writing rebuck2-seed-marker into cache seedcheck-src
[check] harvesting seedcheck-src
[check] reading the marker back out of seedcheck-dst, seeded from it
[check] SEEDING WORKS: a cold seedcheck-dst started from src's contents
```

Thirteen faults stood between writing the transform and seeing it work and
none of them was the mechanism. Two are worth keeping, because they are
properties of buildkit that the documentation does not mention and the
source states backwards.

**`readonly: true` on the root mount is what gets it a MUTABLE ref.** From
`PrepareMounts`:

```go
// if dest is root we need mutable ref even if there is no output
if m.Dest == opspb.RootMount {
    p.ReadonlyRootFS = m.Readonly
    if m.Output == int64(opspb.SkipOutput) && p.ReadonlyRootFS {
        active, err := makeMutable(m, ref)
```

The comment says a root needs a mutable ref "even if there is no output".
The code supplies one only when `Readonly` is *set*. So a hand-built exec
with `readonly: false` and no output gets the immutable ref as its root, and
the container fails with `exit code: 1` before running a line - which is
indistinguishable from the command failing, and cost three attempts blaming
an innocent `cp`.

**Results are numbered by ORDER OF APPEARANCE, not by the `output` value.**
`p.OutputRefs` is appended in mount order. Putting the root at `output: 1`
and the payload at `output: 0` does not make the payload result 0: the root
appears first among mounts with an output, so the root is result 0. The
symptom was a harvested "cache" image containing 1.9 MB of `/bin` and a
97-byte layer holding `proc/` and `sys/` - read out of the registry with
curl and `tar tzf`, which is what turned a mystery into a five-minute fix.

So the working shape for an exec that exports one thing that is not its
rootfs:

| mount     | input    | output | readonly | why                                                                          |
| --------- | -------- | ------ | -------- | ---------------------------------------------------------------------------- |
| `/`       | the base | -1     | **true** | readonly is what makes it mutable, and no output keeps it out of the results |
| the cache | -1       | -1     | true     | a cache mount is never exportable                                            |
| `/.seed`  | -1       | **0**  | false    | scratch, and the only output, so it is result 0                              |

The general lesson is priced rather than argued. Faults 1-4 cost a
twenty-five minute CI run each. A five-minute smoke job took the next
several. A local rig - buildkitd in docker, our registry beside it - took
the last two in four minutes, and one of those had already consumed three CI
runs on its own. **Build the cheapest instrument first**; `scripts/seed-check.sh`
is that instrument and it should have existed on day one.

## Unpacking a seed is cheap, which was the doubt

`scripts/seed-check.sh --fill-mb N`, locally, against a real daemon.
Incompressible bytes, so the layer is honest about its size.

| fill    | write once | harvest once | **seed+read, per worker** |
| ------- | ---------- | ------------ | ------------------------- |
| 0 MiB   | 360ms      | 371ms        | 248ms                     |
| 200 MiB | 1557ms     | 9195ms       | **937ms**                 |

About 3.5ms per MiB to seed, so a 600 MiB module cache would cost a worker
roughly **two seconds** to start from. Against the ~24s per lead that a cold
`go-mod` was measured to cost, that is not a close call.

**This revises the prior written two hours ago**, which said `go-mod` was
probably not worth shipping because principle 18's caveat - pre-positioning
shortens transfer, not unpack - would eat the gain. The caveat is sound and
the estimate behind it was wrong: unpack is the cheap half.

What this does NOT measure, and the distinction matters: the transfer. Both
ends are on one machine here, so the bytes move over loopback and the 937ms
is essentially pure unpack. On a fleet the seed crosses the mesh, and
whether that is cheap is what the seed spread and prefetch exist to decide.
So the honest reading is:

- **unpack per worker: measured, and cheap.** ~2s for a cache the size of
  the one in question.
- **transfer per worker: not measured here.** One copy leaves the
  coordinator under the seed spread and the rest move peer to peer, which is
  the design; whether it behaves is a fleet question.

The harvest at 9.2s for 200 MiB is the one number that grows awkwardly, and
it is also the one paid ONCE, on one machine, off the critical path.

Also worth recording as method: the first pair of runs came back with
identical timings at 0 and 200 MiB, because the script did not forward
`--fill-mb` to the binary. That reads as "200 MiB is free" rather than as
"the flag did nothing", and it is the same shape as every ON BUT NEVER
APPLIED bug in this project. A measurement that cannot distinguish those two
is not a measurement.

## `+all-buildkitd`: parity in time, and it says why

The largest target attempted, and the closest result recorded.

|                   | baseline  | fleet                           |
| ----------------- | --------- | ------------------------------- |
| wall              | **1161s** | **1188s** (+2.3%)               |
| failed targets    | 0         | 0 - parity                      |
| solves            |           | 28 seen, 14 routed, **14 home** |
| occupancy         |           | 0.57                            |
| **amplification** |           | **0.5x**                        |
| op duplication    |           | 1.7x built                      |

Amplification below 1.0 for the first time: 545s of lead work against a
1161s baseline. Occupancy 0.57 says the same thing from the other side - for
most of the wall clock there was no dispatched solve running at all.

`not routed` explains both numbers in one line:

```text
fleet: no peer can take it (wanted Pinned("linux/arm64");
  had 1:linux/amd64 1/4, 2:linux/amd64 0/4, 3:linux/amd64 0/4, ...)
```

`+all-buildkitd` builds `linux/amd64` and `linux/arm64`. Every worker is
amd64, so the arm64 half is pinned to a platform nobody has, stays home, and
runs under qemu on peer 0 exactly as the baseline runs it. Fourteen solves
routed, fourteen kept.

So the fleet distributed the cheap half, left the expensive half where it
was, and finished 27 seconds behind. That is a fair description of a draw,
and it is the first target where distribution costs almost nothing - because
the part that dominates never entered the fleet.

**Which points somewhere useful.** The arm64 build is essentially the whole
critical path and it is slow *because it is emulated*. A fleet with one
arm64 worker would not merely divide that work, it would run it natively -
qemu against native is not a 2x difference. Platform-diverse workers are the
obvious next experiment for this target, and nothing else measured here
would benefit from them at all.

Footnote, and an unflattering one: this run was reported RED. The
`is the daemon still alive` step greps for a panic line, and GitHub runs
steps under `bash -e` whatever the script's own `set` says, so an assignment
from a grep matching nothing exits the step. Matching nothing is the good
news. A diagnostic added to make failures legible turned the best result in
the ledger into a failure.

## Seeding ran in the fleet, and changed nothing measurable

Attempt six. `seeds=4/4`, all four cache ids harvested and resolved, and
`check-seeding` passed inside the run - so the mechanism worked on that
daemon, in that run, before the fleet leg started.

|                    | unseeded (two runs) | seeded        |
| ------------------ | ------------------- | ------------- |
| baseline           | 88s, 86s            | 90s           |
| fleet              | 252s, 249s          | **260s**      |
| mount-naming leads | ~24s each           | **p50 24.7s** |

Nothing moved. The seeded leads took what the cold ones took, to within the
noise this target has already been shown to have.

**Which of the two explanations it is, this run cannot say**, and that is a
reporting failure rather than a null result. Every harvest reported `2
blob(s)` - config plus one layer - and two is also exactly what an EMPTY
cache produces. The count was added to tell "harvested nothing" from
"seeding did not pay", and it cannot: those are the same two blobs.

So the harvest now reports BYTES, and warns below 64 KiB. Until that number
comes back, the honest position is that seeding has been shown to work and
has not been shown to help, and the most likely reason is that the baseline
leg did not leave those caches where the harvest looked - the baseline runs
on `base-bk`, and whether a `+lint-all` baseline fills `go-mod` under that
exact id is an assumption nobody has checked.

The general shape has now bitten three times in this project: a mechanism
reports that it ran, the effect is absent, and the instrument cannot
separate "did nothing" from "did nothing useful". `mech::applied` was built
for the first version of this, the blob count for the second, and the byte
count for the third.

## The caches were empty, and a seeded mount is a DIFFERENT mount

Attempt seven, with the byte counter in place:

```text
[harvest] go-mod at /go/pkg/mod -> ...@sha256:001f3d..., 2 blob(s), 0.0 MiB
[harvest] WARNING: go-mod harvested under 64 KiB - that cache was
  effectively empty, so seeding it changes nothing.
```

All four ids, all 0.0 MiB. So the previous run's null result was never
"seeding does not pay" - nothing was shipped. The instrument that could tell
those apart was added one run earlier and answered on its first use.

Note the two `go-mod` and `/go/pkg/mod` harvests returned the SAME digest.
Two ids, one empty layer, identical content - which is what identical
nothing looks like.

**Why the caches are empty is not yet known.** The baseline runs `+lint-all`
on `base-bk`, that build fills `go-mod` through `+deps`, and the harvest
runs against the same daemon minutes later. One of those three statements is
false and the run does not say which, so `buildctl du -v` now runs against
`base-bk` before the harvest. It names each cache mount the way `mount.go`
builds the name - `cached mount <dest> from <manager> with id "<id>"` - so
it answers all three at once: do the mounts exist, how big are they, and
under which id.

**And a semantic worth knowing before the answer arrives.** From
`getRefCacheDir`:

```go
key := id
if ref != nil {
    key += ":" + ref.ID()
}
```

A cache mount with an INPUT is keyed on the id *and the input's ref*. So a
seeded mount is not the warmed version of the unseeded one - it is a
**separate cache directory**, initialised from the seed and never shared
with the plain `go-mod` dir beside it.

**Correcting that, an hour later, before building anything on it.** The
first reading was that this puts seeding and affinity in tension: a worker
with a warm `go-mod`, handed a seeded graph, would start from the seed
instead and might go slower. That is wrong, for a reason worth writing down.

`cache_seeds()` is read once per run, so every dispatched graph naming
`go-mod` carries the SAME seed ref, so every one of them keys to the same
`go-mod:<seed>` directory on a given worker. That directory accumulates
across leads exactly as the plain one would. There is no split and affinity
keeps working: a worker that has built five seeded leads has a warm seeded
mount, and the sixth meets it.

What does split is HOME builds. Those solve the client's original graph, not
the rewritten one, so they use the plain `go-mod` while every dispatched
graph uses `go-mod:<seed>`. Two directories on peer 0, one of them per-run.
That is a real cost and a small one - home builds are the minority by
construction, `built at home` is 4 of 19 on this target - and it is
disk rather than time.

So: no machinery needed, and the `cache_by_worker` gate described above
would have been solving a problem that does not exist. Reading the key
construction was worth it anyway; assuming what it implied was not.

## Why every harvest was empty: earthly's cache mounts carry an input

Answered from earthly's own source rather than from another run.
`earthfile2llb/runmount.go`, the `cache` case:

```go
mountOpts = append(mountOpts, llb.AsPersistentCacheDir(cacheID, sharingMode))
state = c.cacheContext                       // pllb.Scratch()
state = state.File(pllb.Mkdir("/cache", mountMode))
mountOpts = append(mountOpts, llb.SourcePath("/cache"))
return []llb.RunOption{pllb.AddMount(mountTarget, state, mountOpts...)}
```

The mount has a **state** as its input - scratch with `/cache` created - and
a `SourcePath` selector. Put that beside `getRefCacheDir`:

```go
key := id
if ref != nil { key += ":" + ref.ID() }
```

Earthly's `go-mod` lives at key `go-mod:<ref of that scratch+mkdir>`. The
harvest mounts `go-mod` with **no** input, so its key is plain `go-mod` - a
different directory, which nothing has ever written to. Four harvests
returned 0.0 MiB because they were reading an empty directory that they
themselves had just created.

Everything about the harvest was working. The graph solved, the layer
exported, the reference resolved, the seeded read succeeded - the local
round trip proves all of it. It was pointed at the wrong dir.

**The fix is to construct the same input**, not to guess at it: scratch, a
`Mkdir("/cache")`, and `selector: "/cache"` on the mount. Identical LLB
gives an identical digest gives the same ref, so the same key. That is a
FileOp built by hand, which is more work than a SourceOp and is the only
honest way to read the directory earthly writes.

**Confirmed independently by the daemon's own accounting**, one run later.
`buildctl du -v` against `base-bk`, before the harvest:

```text
cached mount /root/.cache/go-build   ... with id "go-build"
cached mount /go/pkg/mod             ... with id "go-mod"
cached mount /root/.cache/golangci_lint ... with id
  "/run/cache/b369714dd3084d9bf3adc7911b40056e0f36f2d79516e5474021e7d61bddc541/root/.cache/golangci_lint"
Total:  1.71GB
```

1.71 GB, on the right daemon, under the exact ids the harvest asked for -
and the harvest still read nothing. Same id, different key, therefore the
input. The source said it and the daemon agrees.

The third line carries a second bug for free. A mount with no `id=` in the
Earthfile is NOT keyed on its destination: earthly computes
`/run/cache/<per-target hash>/<target>`, so the seed list's
`/root/.cache/golangci_lint:/root/.cache/golangci_lint` names an id that
does not exist. Guessed from the dest; should have been read off a run, and
now can be.

Worth noting what this cost and what it did not. Eight CI attempts reached
"the harvest runs and finds nothing"; the answer took two minutes of reading
`runmount.go`, and it was available from the first attempt. The local rig
could never have found it - it reproduces MY writer, and my writer was
consistent with my reader. Principle 23 says build the cheapest instrument
first, and this is its limit: an instrument that reproduces your own
assumptions confirms them.

## What attempt nine should show, written before it does

Three independent reasons every harvest was empty, all fixed, all found by
reading rather than running:

| # | fault                                                                                         | found in                         |
| - | --------------------------------------------------------------------------------------------- | -------------------------------- |
| 1 | the harvest mounted the cache with no INPUT, so it read a directory it created itself         | `runmount.go` + `getRefCacheDir` |
| 2 | `seed_cache_mounts` filtered on `input < 0`, so it never applied to any earthly graph         | its own source                   |
| 3 | an id with no `id=` is `/run/cache/<hash>/<target>`, so the name we asked for existed nowhere | `buildctl du -v`                 |

Attempt nine carries 1 and 2. The id resolution landed after it started, so
`golangci_lint` will still skip and `go-mod` and `go-build` are the test.

The prediction, so the run has something to disagree with:

- **`go-mod` and `go-build` harvest with real size.** Hundreds of MiB, not
  0.0. If they do not, none of the three explanations was the whole story
  and the next move is `du` again, after the harvest rather than before.
- **`seed_mounts` appears in the mechanisms line with a non-zero count.** It
  has never once been applied - fault 2 guaranteed that - so any number
  above zero is new information on its own.
- **Seeded leads drop from p50 ~24s.** That figure is what a cold `go-mod`
  and `go-build` cost, and it has been stable across four runs. Halving it
  would be a clear result; unchanged would mean seeding arrives but does not
  help, which is finally the interesting question rather than a plumbing
  report.
- **Fleet wall 252s -> somewhere under 200s**, baseline unchanged at ~88s.
  Amplification 3.9x should fall with it.

And the honest bound, unchanged: this is a cold-start measurement. Seeding
is the warm case simulated, so a result here is evidence about what a
permanent fleet would already have, not about what a hosted runner does.

## Attempt nine: the transform fires, the reconstruction does not

Against the prediction written before the run:

| predicted                            | outcome                                           |
| ------------------------------------ | ------------------------------------------------- |
| `seed_mounts` appears at all         | **`seed_mounts=9`** - confirmed, first time ever  |
| `go-mod`/`go-build` harvest real MiB | **0.0 MiB** - refuted                             |
| seeded leads drop from p50 ~24s      | p50 34s, and meaningless while nothing is shipped |

So fault 2 is fixed and proven: the transform now applies to real earthly
graphs, nine times in this run, where the `input < 0` filter had meant it
could never apply at all.

Fault 1 is not fixed. The harvest reconstructs earthly's cache-mount input -
scratch with `/cache` created, mode 0644 - and still reads an empty
directory, which means the reconstructed op does not hash to the same ref.
`getRefCacheDir` keys on `ref.ID()`, so *near enough* is not a thing that
exists here: any field that differs gives a different digest, a different
ref, a different directory.

Candidates for the difference, none checked yet: earthly sets a `platform`
on its ops and the reconstruction sets none; `constraints` likewise; the
`FileAction`'s unused `secondaryInput`/`output` conventions in Go's llb
builder may not be what was assumed.

**And that is the wrong thing to chase.** Reproducing another program's op
bytes field by field is a guess that has to stay right across every earthly
release, and it fails silently - an empty directory, not an error.

The proxy already HAS the bytes. It sees every graph earthly sends, cache
mounts and all, so the input op can be taken verbatim instead of rebuilt.
That is exact by construction and cannot drift. The awkwardness is timing:
the harvest runs between the legs, and the proxy only sees graphs during the
fleet leg.

Which points at a better shape anyway. Harvest at the END of a run, from the
graphs actually observed, and let the bank carry the seed refs and their
blobs to the next run - the coordinator's store is already restored per
target. That is the warm-CI case rather than a simulation of it, and it is
what a permanent fleet does naturally.

Two other numbers from the same run, both the best recorded: op duplication
**1.2x** and amplification **3.4x**, down from 1.6x and 3.9x. Neither is
attributable to seeding, which shipped nothing.

## How cache-mount seeding works, end to end

Thirteen faults and nine runs got here by accretion, so here it is in one
piece. Read this instead of the chronology if you only want the mechanism.

**The problem.** Lifting a cache mount is what makes a subtree dispatchable
at all - otherwise it is grounded to the machine holding the mount - but
lifting the hazard does not lift the cost. The worker builds against its own
mount, which is empty, so a `go mod download` the coordinator did once is
done again per machine. Measured at ~24s a lead across 64 leads.

**The four things buildkit does that decide the design:**

| fact                                                                                           | where                   | consequence                                                  |
| ---------------------------------------------------------------------------------------------- | ----------------------- | ------------------------------------------------------------ |
| a cache dir is keyed `id` + `":" + ref.ID()` when the mount has an input                       | `getRefCacheDir`        | the input selects WHICH directory, not just its contents     |
| `ref.ID()` is `identity.NewID()` - random per record                                           | `cache/metadata.go`     | a near-miss op gets a fresh empty dir, never a partial match |
| a dir with no existing record is created as copy-on-write over the input                       | `getRefCacheDirNoCache` | an input IS a seed                                           |
| earthly mounts every cache with an input: `Scratch().File(Mkdir("/cache"))`, selector `/cache` | `runmount.go`           | nothing here works without reproducing that input exactly    |

**The path, as built:**

1. The proxy observes every graph earthly sends and lifts the exact input op
   behind each cache id - `cache_mount_inputs`, bytes verbatim. Written to
   `cache-inputs.tsv` under the banked store.
2. `harvest-cache` reads that file, and for each `id:path` pair resolves the
   id against what the daemon actually holds - exact match, then unique tail
   match, since an unnamed mount is keyed `/run/cache/<hash>/<target>`.
3. It solves a graph that mounts that cache read-only beside a scratch
   output and copies across, then publishes the scratch as an image. The
   rootfs is `readonly: true` with no output, which is what gets it a
   mutable ref and keeps it out of the results.
4. `seed_cache_mounts` rewrites dispatched graphs: for each seeded id, the
   cache mount's input is REPLACED by the seed image and the old selector
   cleared. Every dispatched graph in a run carries the same seed, so they
   all key to one directory per worker and it accumulates normally.
5. The seed is pre-positioned like any other layer, and resolved before use
   so a seed nobody can pull is dropped rather than becoming a hard
   dependency.

**What is measured, and what is not.** The round trip is proven against a
real daemon by `scripts/seed-check.sh` in about a minute, including at 200
MiB where the per-worker cost is under a second. Whether it makes a fleet
faster is not measured: the harvest has only just started reading the right
directory, and the observed inputs a run writes are for the run after it.

**The timing, which is a feature.** The proxy sees graphs during the fleet
leg, after the harvest. So a run harvests using what the previous run
observed - which is the warm-CI case rather than a simulation of it, and
what a permanent fleet does without being asked.

## Concurrent runs distort the baseline, and this session ran three at a time

`+all-buildkitd`'s baseline leg took **1161s** when it ran alone. The same
leg, same target, same `ubuntu-latest` coordinator, took **over 3100s** in
the mixed-architecture run - and the only difference outside the run was
that two other fleet runs were in flight beside it, seven jobs each.

Nothing inside the run explains it. The baseline runs before any worker is
contacted, so the arm64 workers cannot be the cause; the coordinator does
the same work either way.

The likely mechanism is the hosted pool: a free public repo gets whatever
hardware is spare, and twenty-one jobs asking at once is a different
question from seven. Whatever the cause, the consequence for this document
is the same.

**Every wall-clock comparison here is between two legs of ONE run**, which
is why the rig was built that way and why it survives this. A baseline and a
fleet leg on the same runner in the same half hour see the same conditions.

**Across runs is where it bites.** The `+lint-all` pair quoted at 2.3%
apart, 88s and 86s, were fired minutes apart with similar load, and that is
luck rather than design. A pair fired hours apart under different load could
differ by three times, and the recorded numbers give no way to tell.

So, going forward: fire the comparison and its control close together, and
prefer the within-run instruments - occupancy, amplification, duplication,
the mount arms - which are ratios inside a single run and immune to the
whole question.

## Seeding works, ships 300 MiB, and buys nothing on `+lint-all`

Attempt eleven, the first harvest that presented earthly's own input bytes:

```text
[harvest] go-mod: using the input observed from a real graph
[harvest] go-mod at /go/pkg/mod -> ...@sha256:57730aaa, 171.8 MiB
[harvest] go-build at /root/.cache/go-build -> ...@sha256:eff9e2e9, 128.7 MiB
[wire] mechanisms : affinity=14 seed_mounts=9 ... seeds=3/3
```

Three hundred megabytes of real cache, harvested from the baseline's daemon,
published, resolved, pre-positioned, and grafted into nine dispatched
graphs. Fourteen faults to get here.

And the clock did not move:

|                    | unseeded   | seeded, empty | **seeded, 300 MiB** |
| ------------------ | ---------- | ------------- | ------------------- |
| baseline           | 88s, 86s   | 90s, 88s      | **87s**             |
| fleet              | 252s, 249s | 260s, 282s    | **252s**            |
| mount-naming leads | -          | 24.7s, 34.3s  | **34.8s (n=6)**     |
| amplification      | -          | 3.4x, 2.4x    | **3.1x**            |

Within the run-to-run spread, which the concurrency finding above says is
wider than it looks. Nothing here is a gain and nothing is a clear loss.

**Why, and it is the prior holding up.** Two hours before this ran, the
guess written down was that `go-mod` is the least worth shipping - its miss
path is a download from a fast module proxy, while `go-build` and
`golangci_lint` miss into CPU. What shipped was `go-mod` (171.8 MiB) and
`go-build` (128.7 MiB). What did not ship was `golangci_lint`, at 0.0 MiB.

And that 0.0 is **correct, not another fault**. `+lint` fails on the first
Go module, so the baseline barely fills a golangci-lint cache before it
stops. The cache that would have paid for this target is empty in the
baseline because the target is red on purpose.

So `+lint-all` cannot answer the question it was chosen to answer cheaply.
It is the wrong target for seeding: the expensive cache never fills, and the
two that do fill are the two the prior said would not pay.

**What to do about it**, in order:

- Measure on `+test-ast` instead. Nothing in it fails on purpose, so its
  caches fill properly, and `+earthly` gives it a real Go compile to reuse.
- Seed `go-build` alone and compare against seeding nothing. Shipping 171.8
  MiB of module cache to save a fast download is the trade the prior says is
  bad, and it can now be tested rather than argued.
- Fire the pair back to back with nothing else running, per the concurrency
  finding.

## The per-lead cost is bytes, at about 7 MB/s

From data already in the logs - `[worker] job N took Xms (Y ops, Z KiB
fetched)` - across attempt eleven's 41 distinct leads.

|                            |                                                    |
| -------------------------- | -------------------------------------------------- |
| leads under 15s            | 73 of 82; median **138ms**, median fetch **1 KiB** |
| leads fetching over 1 MiB  | 17, and they hold **87%** of all lead time         |
| median rate across those   | **7.2 MB/s**                                       |
| total fetched by the fleet | **1164 MiB**                                       |

The correlation is not subtle:

```text
      ms       KiB    MB/s
    1317      7970     5.9
    8686     86823     9.8
   10527     94020     8.7
   19151    139421     7.1
   25905    266864    10.1
```

**A lead costs what it fetches.** A trivial one is 138ms - dispatch overhead
is not the problem and never was, which retires a suspicion this document
has carried for a while. The whole per-lead constant is layer
materialisation, and 87% of all lead time is in the seventeen leads that
move real bytes.

7 MB/s is slow for a hosted runner on a gigabit link, so this is unpack and
not the wire: gzip decompression plus overlayfs writes on two cores. That
matches principle 18's caveat exactly - pre-positioning shortens transfer,
and unpack is what is left.

**This corrects principle 24's arithmetic, and not by a little.** The local
rig measured seeding at ~3.5ms per MiB, on loopback with a fast disk. In the
fleet it is **~140ms per MiB**, forty times worse. So:

| seed                  | at 3.5ms/MiB | at 140ms/MiB |
| --------------------- | ------------ | ------------ |
| `go-mod`, 171.8 MiB   | 0.6s         | **24s**      |
| `go-build`, 128.7 MiB | 0.5s         | **18s**      |
| both, per worker      | 1.1s         | **42s**      |

Forty-two seconds per worker to save a `go mod download` and some
recompilation. That is why attempt eleven shipped 300 MiB and the clock did
not move: the seed cost about what the cold caches cost, and the two
cancelled.

Which sharpens the rule rather than overturning it. Seeding still pays when
the miss is expensive CPU - but the bar is 140ms per MiB shipped, not 3.5,
and almost nothing clears that at 300 MiB. **Seed small caches with
expensive misses.** `golangci_lint` on a target where it actually fills is
the remaining candidate; `go-mod` is not close.

And the general lesson for the instrument: the local rig gave a number that
was right for the rig and wrong for the fleet by a factor of forty. It could
not have known - loopback is loopback. The fleet number was available all
along in a log line that has been printing since long before any of this.

## The fleet spent 1.9x the whole build just materialising layers

Same run, arithmetic on the same log lines.

|                                |                                     |
| ------------------------------ | ----------------------------------- |
| fetched across the fleet       | **1164 MiB**                        |
| per worker, six of them        | 194 MiB                             |
| at the measured 7.2 MB/s       | 27s each, **162s across the fleet** |
| total lead time                | 207s                                |
| the whole single-machine build | **87s**                             |

**Careful with that 162s: it is machine-seconds, not wall clock.** The six
workers unpack concurrently, and the registry report settles whether they
contend - the coordinator served **158 MiB** while the fleet fetched 1164,
so 86% moved peer to peer and the joins really are parallel. A worker costs
about 27 seconds of WALL to become useful, once, not 162.

The first version of this section said "1.9x the entire baseline" and meant
machine-seconds while reading like wall clock. It is corrected here rather
than deleted because the mistake is the interesting part: machine-seconds
and wall clock differ by exactly the occupancy, and this document quotes
both.

What survives is the composition: layer materialisation accounts
for 162 of the 207 seconds of lead work. Duplication is 1.2x, so this is not
the same op arriving twice - it is six machines each needing the base chain
once, which is the irreducible shape of a cold fleet.

A worker pays `base_bytes / unpack_rate` to become useful - here about 27
seconds - and it pays it in parallel with the others. So the naive model is

```text
wall  ~  join + work / N
```

which DECREASES in N, and says use every machine. That is what the fleet
does, and on this evidence it is not obviously wrong.

The tempting next step was a rule capping workers by how much work there is.
It was written, tested, and removed before it shipped: it rests on treating
162 machine-seconds as wall clock, and the coordinator-served figure says
the joins are parallel rather than queued. A scheduling rule built on that
confusion would have made things worse while looking principled.

What the numbers DO support is narrower and still useful: 78% of lead time
is layer materialisation, so anything that reduces bytes or speeds unpack
attacks the largest term, and anything that reduces scheduling overhead
attacks the smallest.

Three ways out, in increasing order of how much they change:

- **Fewer workers.** Cheap to test and NOT justified by anything measured
  yet - see above. Worth a run precisely because the model says it should
  not help: if fewer workers is faster, the joins contend more than the
  registry figures suggest.
- **Warm workers.** The 194 MiB is paid once per machine per RUN because a
  hosted runner is destroyed afterwards. A permanent fleet pays it once,
  ever - which is the cold-start bound already recorded, quantified.
- **Faster unpack.** zstd instead of gzip, now selectable. 7.2 MB/s is
  decompression on two cores, and zstd is several times quicker at similar
  size.

The last is the one with evidence behind it. 7.2 MB/s is decompression on
two cores, zstd is several times quicker at similar size, and every layer
the fleet moves is one this project exported - so it is a setting rather
than a redesign.

## zstd export: confirmed applied, before spending a run on it

`REBUCK2_COMPRESSION=zstd`, harvested locally, manifest read back:

```text
mediaType: application/vnd.docker.distribution.manifest.v2+json
  layer application/vnd.docker.image.rootfs.diff.tar.zstd  20972615 bytes
```

The layer really is zstd, so `force-compression` is doing its job - without
it buildkit reuses whatever a layer already carried and the setting reads as
having done nothing, which is the single most common failure mode in this
document.

Twenty megabytes of `/dev/urandom` compresses to twenty megabytes, so this
says nothing about size. It was never meant to: the question was whether the
attr reaches the exporter, and it does. Real cache contents - Go modules,
compiled objects - compress well and decompress several times faster than
gzip, which is the whole point at 7.2 MB/s.

Worth noting what it cost to establish: one minute with the local rig,
against a twenty-five minute fleet run that would have answered the same
question with more noise. Principle 23, applied on purpose this time rather
than after three runs.

## What is built, what is on, and what has actually been measured

An audit, because the answer surprised me and it decides what to run next
rather than what to build next.

| mechanism             | default | measured?                                                               |
| --------------------- | ------- | ----------------------------------------------------------------------- |
| `cut_prefix`          | **on**  | yes - 444s to 407s locally, and the +107s that took graft off           |
| `affinity`            | **on**  | yes - op duplication 2.9x to 1.2x                                       |
| `verdict_stops_retry` | **on**  | yes - +lint-all 628s to 252s                                            |
| `read_retry`          | **on**  | guard; has never needed to fire                                         |
| `prefetch`            | **on**  | partly - it announces, and no run isolates its effect                   |
| `peer_cache_mounts`   | **on**  | yes, as a cost: it is what makes cold mounts possible                   |
| `seed_mounts`         | off     | yes - ships 300 MiB, changes nothing on `+lint-all`                     |
| `graft`               | off     | measured ON one shape (+107s) and never re-measured against a warm bank |
| `trust_verdict`       | off     | **no**                                                                  |
| `min_siblings`        | off     | **no**                                                                  |
| `warm`                | off     | **no** - and its target was changed to `+deps` on a theory              |
| `local_nested`        | off     | **no**                                                                  |
| `fleet_cache`         | off     | measured once, badly - 755s against 590s                                |
| `compression` (zstd)  | off     | **no**                                                                  |
| `arm_workers`         | off     | **no** - the run is still going                                         |

Five mechanisms have never been measured at all, and two more were measured
once under conditions that no longer hold. That is not a backlog of work to
build; it is a backlog of runs.

The ones on by default all earned it with a number, which is the part to
keep doing.

**What that says about what to run next**, in order of expected value:

1. **zstd on `+test-ast`.** 78% of lead time is layer materialisation and
   the codec is a one-line setting. The instrument that says whether it can
   possibly help - fetch time against unpack time - landed this hour and has
   never run.
2. **`graft` against a warm bank.** Its +107s was measured within a single
   run, where the ancestor has to be built before it can be imported. Across
   runs the bank already holds it. Nothing has tested that and the machinery
   is all there.
3. **`trust_verdict`.** Removes one of the two attempts a deterministic
   failure costs, and every test target here has a red leg.

And the discipline the audit exists to enforce: a mechanism that is off and
unmeasured is not a feature, it is a hypothesis with code attached.

## The mixed-architecture run, cancelled rather than reported

`+all-buildkitd` with two `ubuntu-24.04-arm` workers. The arm64 runners
scheduled fine and the label works, which is the one thing it did establish.

It was cancelled at 100 minutes with its **baseline leg** still running. The
same leg, same target, same amd64 coordinator, took 1161s when it ran alone.

Cancelled rather than left to finish, and the reason is the finding two
sections up: it was sharing the pool with two other fleet runs, its baseline
was already five times the reference, and a wall clock from it could not
have been compared with anything. Letting it run would have cost another
hour of seven runners while distorting every other measurement taken beside
it - and produced a number nobody could use.

The question it exists to ask is still open and still the best one available:
`+all-buildkitd`'s arm64 half is pinned to a platform no worker had, so it
stayed home and ran under qemu exactly as the baseline ran it. A native
arm64 worker does not divide that work, it stops emulating it. Nothing else
measured here would gain from a platform-diverse fleet.

It needs a quiet pool and one run at a time, which is now the standing rule
rather than a preference.

## `+test-ast`: 73x amplification, and the byte model at its extreme

The target chosen because nothing in it fails on purpose. It produced the
worst result recorded, by a wide margin, and it is the clearest single piece
of evidence in this document.

|                   | baseline | fleet                                     |
| ----------------- | -------- | ----------------------------------------- |
| wall              | **204s** | **1773s**                                 |
| failed targets    | 0        | 0 - parity                                |
| solves            |          | 412, **all 412 routed, 0 home**           |
| leads             |          | 414, **14,812s of lead work**             |
| **amplification** |          | **73x**                                   |
| occupancy         |          | 8.41, against a 7-machine ceiling of 3.50 |
| total fetched     |          | **24.7 GiB**                              |

Occupancy above the ceiling is not an error in either number. The ceiling is
what the GRAPH allows; occupancy above it means the fleet was busier than
the graph's own critical path - because distribution ADDED the work rather
than dividing it.

**24.7 gigabytes moved to run a 204-second build.** Median lead: 2.6
seconds, 26 MiB fetched. Every one of those 412 tiny AST tests - `jq`, a
`diff`, milliseconds of real work - was dispatched, and each had to
materialise the `+earthly` image before it could run.

This is the byte model from two sections up, taken to its conclusion. A lead
costs what it fetches; a lead that fetches 26 MiB to run a `diff` is pure
loss; and 412 of them is 73x.

**It also settles a question I retracted an hour ago for the wrong reason.**
I removed a worker-count cap because I had confused machine-seconds with
wall clock, and that retraction stands - capping WORKERS was not justified.
Capping which SOLVES are worth dispatching is a different rule and this is
overwhelming evidence for it: 54 of 376 leads had 20 ops or fewer, cost 232
seconds between them, and fetched 1.2 GiB to do work that a single machine
does in the noise.

The gate has to be on the solve, not the fleet: **do not dispatch a graph
whose work is smaller than the bytes it would drag across.** Op count is a
crude proxy for the first and is known before dispatch; the base image size
is known too, because we mirrored it.

`min_siblings` exists, is off, and gates on queue depth rather than size -
it would not have stopped any of this.

### Correction: the 24.7 GiB never crossed the network

Written the same day as the section above, after taking the per-lead lines
apart. The 73x is real; the explanation I attached to it is not.

`SERVED_BYTES` counts HTTP responses out of a worker's **own** registry -
bytes it handed to a client, which is a different thing from bytes it
fetched to serve them. What the worker *fetches* is counted separately and
is small: `[cas] fetches: local=2 peer=37 driver=82` is the high-water mark
on any worker, a few hundred fetches across the whole run, most of them
frontier blobs. So "24.7 GiB fetched" is wrong on its face.

> **And the first replacement was wrong too.** I wrote that the 24.7 GiB
> was therefore loopback, reasoning from `serve_with_upstream(addr, reg,
> None)`. That argument does not hold: `upstream` says where the registry
> FETCHES when it misses, not who it SERVES. A worker binds
> `--registry-bind 0.0.0.0:15000`, so its clients are the buildkitd beside
> it **and any peer pulling a result**. Nothing in the run separates them.
> `SERVED_LOCAL_BYTES` now does, splitting on whether the client's socket is
> loopback, with unknown counted as remote so the unproven case cannot land
> on the flattering side. Until a run reports it, the local/remote split of
> that 24.7 GiB is **unknown**.

The registry file already carried the warning, ten lines above the counter:

> two runs reported "0 KiB" across 160 blob GETs, and I read that as an
> architectural finding about who serves a worker's inputs.

Same counter, opposite direction, same mistake.

What the per-lead numbers actually say, on 414 distinct leads:

| relationship | pearson r |
| ---------------- | --------- |
| ops ~ KiB served | 0.32 |
| ops ~ ms | 0.27 |
| **KiB served ~ ms** | **0.06** |

Bytes served do not predict how long a lead takes. Nor does op count
predict bytes. And the served volume is a handful of fixed artifacts
repeated: worker 2 served an identical 26,726 KiB blob **76 times**, worker
4 served an identical 188,827 KiB blob 14 times. Three buckets of the ops
histogram have medians of 26,726 / 26,729 / 26,726 KiB - that is one
artifact, re-served per lead, not a size distribution.

So three claims retract:

- **"A lead costs what it fetches"** - unsupported per-lead (r = 0.06).
  It survives only as an aggregate intuition, which is not a mechanism.
- **"24.7 GiB moved"** - it was served, over loopback. Say served.
- **The op floor's justification.** ops~KiB at 0.32 makes op count a weak
  proxy for bytes, and bytes are not the cost anyway.

What survives, and it is still the finding: **204s against 1773s, 412
solves all routed, and a per-lead cost of ~2.5s that barely varies with
what is in the lead.** The cost of a lead is close to CONSTANT. That is a
much stronger reason to refuse small work than any byte argument - a fixed
toll is paid in full by the smallest job - and it points somewhere the byte
story did not: at the ~26 MiB re-materialised per lead on a machine that
already had it.

The floor ships anyway, and `-ast-minops` was already running when this
came out. It is now a calibration, not a fix: the counterfactual on the
recorded distribution says a floor of 20 refuses 63 of 414 leads, so if the
20-op run moves the wall clock much at all, the constant is not as constant
as this section claims.

| floor | leads refused | of 414 |
| ----- | ------------- | ------ |
| 5 | 10 | 2% |
| 20 | 63 | 15% |
| 40 | 221 | 53% |
| 100 | 315 | 76% |

### 277 MiB distinct, 24.7 GiB served

The coordinator's registry printed this at shutdown and I had not read it:

```text
[registry] 17 blobs over 1MiB, 277 MiB of the total; largest:
[registry]        65 MiB  sha256:b05ba1b396096c36a
[registry]        37 MiB  sha256:4cea535e2a7b45e35
[registry]        36 MiB  sha256:4c9ed40f1723d92dc
[registry]        22 MiB  sha256:977a4b8e92e1b94c1
```

**Seventeen blobs. 277 MiB of distinct content in the whole build.** The
fleet served 25,658 MiB. That is a factor of **93**.

So the 24.7 GiB is not volume at all - it is repetition. The same seventeen
artifacts, handed to the same buildkitd over and over, once per lead. Which
is exactly the shape the per-lead medians showed (26,726 / 26,729 / 26,726
KiB across three separate op-buckets) without my reading it that way.

The map that would have said so kept `digest -> size` and **overwrote** on
every serve, so seventy-six serves of one layer reported as one 26 MiB
blob. It is now `digest -> (size, times)`, sorted by size x count rather
than by size, because the biggest single layer is rarely the biggest cost.
And `fetch_summary` prints it on the worker's own exit path: the registry
only reported it on ctrl-c, which is why no worker ever did.

The unit of the problem has moved. It is not "how much does the fleet
move" - it moves almost nothing, twice. It is **"why does a machine that
already holds a layer materialise it again for the next lead"**, which is a
question about buildkitd's content store and the refs we hand it, and it is
worth more than any transport work on the list.

## The 20-op floor is harmful, and every per-lead number improved

`-ast-minops`, floor 20, same target, own baseline leg. Cancelled at 96
minutes with the fleet leg 85 minutes in and roughly half done - against
1773 seconds for the whole thing ungated. Per solve that is about **five
times slower**.

The gate itself worked exactly as specified:

```text
[wire] mechanisms : ... min_ops=17 ...
[wire] not routed : {"considered": 110, "smaller than the bytes it would drag": 17}
```

And everything it was supposed to improve, it improved:

| | ungated | floor 20 |
| ------------------------ | -------------- | -------------- |
| lead round trip, cold p50 | 16472ms (n=359) | **8745ms (n=82)** |
| ops sent / distinct | 23.0x | **4.6x** |
| (op, worker) pairs built | 1.5x | **1.2x** |
| smallest lead dispatched | 2 ops | 29 ops |
| worker-side lead p50 | 2583ms | 2471ms |

Halved round trips, a fifth of the op duplication, and the same per-lead
cost. Then it took five times as long.

So the loss is in the seventeen solves it kept home, and the reason is the
thing the rule got wrong: **op count measures size, and the fleet's problem
is criticality.** A three-op `FROM ... / RUN ...` at the head of the chain
is small and everything waits on it. Keeping it home does not save a
placement toll; it serialises the build behind one machine, and on this run
that machine was also the gateway.

That is a sharper statement of principle 11 (subdivide at the narrowest
declared seam) than I had: the seam is not about size either way. Small
work is cheap to keep AND cheap to send; what decides is how much waits
behind it.

### The instrument that would have proved it is dead

```text
[wire] service ms : home 0 (0) away 0 (0) ratio 0.00
```

Zero home solves and zero away solves in a run that placed 82 and kept 17.
The buckets fill from `Control.Solve`, keyed by its `ref`. Under earthly
there is ONE `Control.Solve` for the whole build; every placement decision
happens on an inner gateway solve with a different id, so the lookup misses
and lands in the "never placed" arm every time. It has been printing
`0 (0) 0 (0) ratio 0.00` for at least two runs and I read it as data.

A gateway solve returns a ref in about a millisecond, so it is not a build
time either: **there is no home-side equivalent of the driver's lead
timings.** That absence is why this run could not be diagnosed from its own
output, and it is now the next thing to build - the line says NOT MEASURED
rather than zeros in the meantime.

## A daemon does not re-fetch a base it already holds

`scripts/reserve-check.sh` - one buildkitd, one registry, six trivial solves
on the same base, about a minute:

```text
[reserve] solve 0:   342ms  registry served   1870 KiB in 2 request(s)
[reserve] solve 1:   223ms  registry served      0 KiB in 0 request(s)
[reserve] solve 2:   226ms  registry served      0 KiB in 0 request(s)
[reserve] solve 3:   243ms  registry served      0 KiB in 0 request(s)
[reserve] solve 4:   240ms  registry served      0 KiB in 0 request(s)
[reserve] solve 5:   234ms  registry served      0 KiB in 0 request(s)
```

Fetched once, re-used five times. So of the two explanations for 277 MiB
distinct against 25,658 MiB served, **one is eliminated**: it is not a
daemon re-materialising a base per solve. There is no per-solve
re-materialisation to fix.

What remains is repetition **across machines** - six daemons each needing
the same artifacts, and every lead's parent result travelling to whichever
worker took the child. That is consistent with worker 2 serving one 26,726
KiB blob seventy-six times: it was the machine that HELD the popular
artifact and handed it out, not a machine fetching it over and over.

Consistent, not proven. `SERVED_LOCAL_BYTES` decides it, and the run
carrying it is in flight. If most of a worker's serving is remote, the cost
is transport and the levers are placement, affinity and prefetch. If it is
local, this rig missed something and the local result is the one to
re-examine.

Note what the rig cost against what it settles: a minute, against half an
hour per CI run, for a question two CI runs had already failed to answer.
Principle 23, earning its keep for the second time.

**And it nearly answered wrongly.** The first run printed `0 KiB` for every
solve including the first, which reads as "the base is never fetched at
all". The stats were being polled at `$HOSTADDR`, which is how the DAEMON
reaches the host - `host.docker.internal` - and the host does not resolve
it. Every stats request failed and the code fell back to the previous
sample, so the failure looked exactly like a clean flat line. The same
address confusion is already documented three sections up in a different
guise; it now has a `--stats` flag and a comment saying why.

## Prefetch has never worked for subtree results

```text
412 [driver] prefetch: could not read the manifest for <img>
```

412 out of 412 in the ungated `+test-ast` run; 82 of 88 in the gated one.
The six that worked were mirrored base images. Every subtree result failed,
in every run, since prefetch was written.

The cause is one line, and it is not a bug in prefetch. `published_reference`
returns a **bare `sha256:...`** on purpose - a digest names content rather
than a location, and that is precisely what lets a result travel between
machines. `image_blobs` then tries `split_once('/')` to build
`http://host/v2/repo/manifests/...`, finds no host, and returns `None`. One
`None`, one message, no reason.

So the mechanism aimed squarely at cross-machine repetition - the thing the
93x re-serve ratio now points at - has been announcing nothing at all for
its entire life, while reporting `PREFETCH=1` as enabled and printing a
"prefetching, shared before it was placed" line just above each failure.

The fix needs no URL. A manifest is a blob addressed by its own digest, and
`FleetBlobs::by_hash` is already how the coordinator's registry reaches a
blob some worker holds - the same path a lead's result takes. A bare digest
now goes through it and the layers get announced.

`image_blobs` also stopped folding four faults into one silence:
unparseable reference, unreachable host, non-2xx from the registry, and a
manifest with no layers each say so, with the URL and the status. `mech.rs`
was built to catch a mechanism that is on and never applied; it cannot
catch one that runs, fails, and says so in a message that names no cause.

### What the fixed prefetch should look like, stated before the run

Every worker line in both `+test-ast` runs:

```text
126 [worker] prefetched 0/0 of my share (2 announced)
 62 [worker] prefetched 1/1 of my share (2 announced)
 36 [worker] prefetched 0/0 of my share (6 announced)
```

Two, four or six blobs announced - base-image manifests, the only ones whose
refs carried a host. **Eighty-four blobs prefetched in total**, across every
worker of both runs, against 17 distinct artifacts over a megabyte and 25.6
GiB served. Prefetch has been moving nothing, and the counter said
`prefetch=6` rather than `ON BUT NEVER APPLIED`, because it did fire - six
times, on the wrong six images.

So the signature to look for, written down first:

- announcements should carry **tens** of blobs, not two - a subtree result's
  manifest lists its whole layer chain
- `prefetched N/N` with N > 0 on most workers, not `0/0`
- and the number that decides it: **MiB served should fall**, because a
  worker that already holds a layer does not ask a peer for it

If announcements grow and served bytes do not fall, the answer is in
`my_share`: it splits the announced blobs across workers so that SOME peer
holds each one, which spreads the source rather than pre-positioning the
content where it will be used. That is the right shape for avoiding a herd
on the coordinator and the wrong shape for content every worker needs. It
is deliberately not changed now - one variable, and this one has been dead
long enough that its live behaviour is unknown.

## Affinity never looked at the expensive thing

Chained on one daemon - each solve building on the previous solve's result,
which is the fleet's actual shape:

```text
[reserve] solve 0:   344ms  registry served   1870 KiB in 2 request(s)
[reserve] solve 1:   838ms  registry served      0 KiB in 0 request(s)
[reserve] solve 5:   569ms  registry served      0 KiB in 0 request(s)
```

**Zero, even though the base changed every time** - because the daemon built
that base and still has it. That is the entire difference between one
machine and six: in a fleet the parent was built somewhere else, so the
whole parent image crosses. 25.6 GiB served against 277 MiB distinct is that
difference, counted.

And the placement decision has never looked at it. `warmth(ops, caches)`
scored two things:

- ops the candidate has already built - but a **cut** subtree names its
  parent as `docker-image://host/repo@sha256:...` instead of carrying its
  ops, so op overlap is structurally blind to the parent
- cache mounts it holds - real, and measured at ~24s, but not this

So affinity was sorting on the small term while the large one went
unexamined. `mech` reported `affinity=10` against 82 placements in the gated
run: it reordered the queue ten times, and never once because a machine
already held the image about to be pulled onto it.

`warmth` takes a third term. The blooms already answer it - they exist, they
are gossiped, and `FleetBlobs::by_hash` already trusts them for exactly this
question. A bloom lies only in the safe direction, so a false positive
misplaces one subtree and a false negative cannot happen.

Weight: the same 64 as a cache mount. A warm mount saves ~24s of `go mod
download`; a parent already local saves a 188 MiB transfer, which at this
fleet's ~7 MB/s is the same order. Two constants would imply a precision
that comes from nowhere.

## Building is a quarter of lead time; the rest is unattributed

From the ungated `+test-ast` run, two totals over the same 414 leads:

| | seconds |
| ---------------------------------------- | ------- |
| worker-side, summed `took Nms` | **4,407** |
| driver-side, summed `lead_ms` | **14,812** |

A lead spends **3.4x longer in the driver's books than the worker spends
building it.** Median: 16.5s round trip against 2.6s of build.

`lead_ms` runs from the subtree record being opened to the worker reporting
`Led`, so it contains the offer round trips, any declines and re-offers, the
time the lead sits queued on a busy worker, and the build. The worker's own
figure contains the build and its fetches, and nothing else.

**Not all of that gap is waste.** Peak in flight was 11 across 6 workers, so
some of it is a lead legitimately waiting its turn - that is a fleet being
used, not a fleet being slow. But nothing splits the two, and the quantity
is larger than everything else measured put together: 10,400 seconds
against 4,407 of building and against a 204-second baseline.

Every remedy so far - seeding, compression, the op floor, prefetch,
affinity - aims at the 4,407. The instrument that would price the other
10,400 is three timestamps on the subtree record: offered, accepted,
started. That is the next thing to build after the two fixes in flight, and
it is cheap.

`scratchpad/read-run.sh` now pulls a run's logs and prints exactly the lines
that decide something. Written mid-run because the last four analyses were
ad-hoc greps and two of them read a counter as the wrong quantity.

## How much of the Earthfile has actually been through the fleet

Read from earthbuild's own `Earthfile` rather than assumed, because the
coverage half of this exercise had drifted into a performance investigation
and the ladder turned out not to be the one I had in my head.

| target | children | run through the fleet |
| ------------------ | ------------------------------------ | --------------------- |
| `+lint-all` | 3 | yes, many times |
| `+all-binaries` | 5 cross-compiles | yes |
| `+all-buildkitd` | multi-arch, qemu | yes, at parity |
| `+test-ast` | 3 AST groups | yes |
| `+test-no-qemu-group2` | 1 of 14 | yes, the default |
| **`+all`** | `+all-buildkitd`, `+all-binaries`, `+earthly-docker`, `+prerelease` | **never** |
| `+test-no-qemu` | `+test-misc`, twelve groups, `+test-no-qemu-slow` | **yes** - 186s/525s, the best ratio here |

Two things this corrects.

`+all` is **not** the test surface. It is the release chain -
buildkitd, binaries, the docker image and the prerelease bundle. Three of
its four children have been through the fleet individually, so it is less of
a coverage step than its name suggests.

`+test-no-qemu` is the widest target in the file: **fourteen siblings with
no dependency between them.** That is exactly the shape a fleet exists for,
and precisely one of the fourteen has ever been run. Every speed number in
this document comes from targets with 3 to 5 branches, on a fleet of 6 or 7
machines - which is a fleet that cannot be busy, whatever the scheduler
does. The Amdahl ceilings recorded here (3.11x, 3.50x, 5.99x) are the
workload's, not the system's, and principle 19 says so; the fourteen-way
target is the first chance to test that claim.

So the coverage ladder is `+test-no-qemu` first and `+all` second, which is
the reverse of the order I had been assuming. `-tests` selects it.

## The reference run died, and taught more than it would have measured

`-ast`, instruments only, no mechanism changed. It ran the fleet leg for 50
minutes against 29.5 for the same target before the instruments, then failed
with exit 124.

The chain, in order:

1. The leg ran long. Cause still open - see below.
2. It crossed `timeout 3000` on the WORKER command: fifty minutes.
3. All six workers were killed at once.
4. The coordinator's registry went on being asked for subtree manifests
   that only those workers held, and `FleetBlobs::by_hash` walked each dead
   peer in turn with no deadline.
5. buildkit gave up: `Head ".../v2/rebuck2/subtree/manifests/sha256:...":
   net/http: timeout awaiting response headers`, and failed the build.

Two defects, both independent of whatever made the leg slow.

**A cap inside a cap turns "slower than expected" into "failed".** The job
allows 150 minutes and the worker allowed 50. That is the worst possible
reading of a performance experiment: it destroys the measurement and
misattributes the cause in the same stroke. Now 8100s, under the job's cap
rather than a third of it.

**A miss must 404 quickly.** A dead runner does not refuse, it hangs, and
the peer walk is what a manifest HEAD waits on. Five seconds per peer now;
six unanswering peers still comes in under any client timeout worth the
name, and 404 is an answer buildkit knows what to do with.

### And a number I got wrong

```text
[registry] 26 blobs over 1MiB: 514 MiB distinct, 545 MiB served (1.1x re-served)
```

**1.1x, not 93x.** The per-digest count I added for exactly this question
says the coordinator's registry barely repeats itself.

The 93x came from dividing the fleet's total served bytes by the
COORDINATOR's distinct bytes - two different registries, and no more valid
than the earlier loopback claim. The re-serve ratio that matters is
per-worker, and the workers' own summary line did not appear in this run's
logs because they were killed before printing it.

So the repetition claim is retracted a second time and remains open. What is
NOT open: `scripts/reserve-check.sh` shows a single daemon serving a base
once and never again, chained or not - so whatever the fleet is repeating,
it is not one machine re-materialising its own work.

### The prefetch fix's own risk, stated before its result

Written while the run is in flight, so it cannot be a post-hoc explanation
of whatever comes back.

The fix makes the driver resolve a bare digest through
`FleetBlobs::by_hash`, once per subtree result. That path checks the local
store first and then walks peers - and the coordinator usually will NOT have
the manifest, because the worker pushed it to its own registry and the
coordinator only acquires it when the requester pulls the result, which
happens after.

So the expected new load is roughly one peer round trip per lead, in a
spawned task, bounded at five seconds per peer. On `+test-ast` that is ~400
of them. Three ways this could read:

- **Faster, announcements in the tens.** The fix worked and the signature
  above is met.
- **No change, announcements in the tens.** Prefetch reaches the right
  content and the content is not the bottleneck - which would point at
  `my_share` splitting it one-blob-one-worker, already built and gated
  behind `-bcast`.
- **Slower, with `nobody in the fleet holds <digest>` lines.** The manifest
  lookup is costing more than the prefetch saves, and the answer is to
  resolve from the requester's pull rather than ahead of it.

The third is a real possibility and worth saying out loud: this session has
already produced one mechanism that improved every number it aimed at and
still made the run five times slower.

## Affinity concentrates work onto whoever is already warm

Placements per worker, three runs, six workers available in every one:

| run | spread | idle workers |
| ---------------- | ------------------- | ------------ |
| `+test-ast` ungated | 223 / 125 / 63 / 3 | **2 of 6** |
| `+test-ast` floor 20 | 46 / 37 / 10 | **3 of 6** |
| `+test-ast` reference | 153 / 142 / 81 / 3 | **2 of 6** |

Two or three machines never take a single lead, and one takes more than
half. That is not a scheduling accident - it is `offer_order_warm` doing
exactly what it says:

```rust
able.sort_by_key(|c| (Reverse(warm(c.id)), Reverse(c.load.free()), c.id));
```

Warmth is the FIRST key and free capacity the second, so warmth dominates
lexicographically: a warm worker with one free slot beats a cold worker with
sixteen, every time, and the warm worker gets warmer with each lead it
takes. The only thing stopping runaway concentration is `free() > 0`.

This is very likely where the 10,400 seconds of non-building lead time
lives. `lead_ms` runs from the offer to the result, so a lead queued behind
five others on the warm machine books all of that as lead time - and the
split now in the code will say whether it is `waiting` (queued, a fleet
being used) or `placing` (offers and refusals, pure overhead). No decline
line appears anywhere in three runs' logs, which points hard at `waiting`.

If that is right, then every remedy aimed at making a lead cheaper -
seeding, compression, prefetch, the op floor - has been working on a quarter
of the problem while two machines sat idle for the whole run.

The fix is not to remove affinity. Warmth is real and measured: a cold cache
mount costs ~24s and a parent image transfer is the same order. The fix is
that warmth must be traded against queue depth rather than ranked ahead of
it - a warm machine with five leads waiting is worse than a cold machine
with none, and the current comparator cannot express that.

## Prefetch, once it worked: 1773s to 1240s

The A/B for the bare-digest fix. Same target, same 412 solves, same 23,690
ops sent - as clean a comparison as this workload gives.

| | before | after |
| ----------------------- | -------- | -------- |
| **fleet leg** | **1773s** | **1240s** |
| baseline | 204s | 227s |
| amplification | 73.0x | **44.5x** |
| `prefetch` applied | 9 | **421** |
| blobs per announcement | 2, 4, 6 | 5, 6, 8, 13 |
| total lead time | 14,812s | **10,101s** |
| worker-side build time | 4,407s | 3,518s |
| mount arms, cold p50 | 16,472ms | **9,783ms** |
| built duplication | 1.5x | 1.4x |

**Thirty percent off the fleet leg**, and the pre-registered signature was
met on every point: announcements in the tens rather than twos, `prefetched
N/N` with N above zero on most workers, and the cold-mount median cut by
41%. The mechanism went from firing nine times on six base images to firing
421 times on the content the fleet actually moves.

It is still 5.5x slower than one machine. But this is the first change in
the project to move the headline by more than noise, and it did it by
repairing something that had never worked rather than by adding anything.

### And the phase split, first reading

```text
[wire] lead phases : placing 0s (0%) waiting 6583s (65%) building 3517s (35%)
```

**Placing is zero.** Offers are never refused, so choosing a worker costs
nothing measurable - the entire offer/decline/re-offer protocol, which three
sections of this document worried about, is free. Retire that concern.

**Sixty-five percent is waiting**: 6,583 seconds of leads sitting on a busy
worker while another machine is idle. That is the concentration finding
priced, and it is now the largest number in the system by a wide margin -
larger than all the building, and far larger than anything seeding,
compression or the op floor could ever have reached.

`-balance` is the experiment that addresses it, and it is already built.

### The instruments are exonerated

```text
[wire] status tap : 3004 frame(s), 1ms total, 0 missed (busy)
```

**One millisecond**, across three thousand frames. So the 50-minute
reference leg was not the tap, and the suspicion that had me rewriting it
twice was wrong - which is exactly what that self-measurement was added to
settle, and it settled it without a second run costing forty minutes.

### What `waiting` is, exactly - and why more slots is not the fix

Checked rather than assumed, because "the remainder" is where a
misattribution would hide.

`build_ms` is started at `worker.rs:845`, and the only awaits before it are
a local `daemon_platforms` gRPC call and - the important one -
`slots.acquire().await`. So the worker's own slot semaphore is **outside**
the build timer, and everything spent queueing for a slot lands in the
driver's `waiting` bucket.

`waiting 6583s (65%)` therefore means precisely "queued for a slot on a
worker", not "some unattributed remainder". The bucket boundary is a
semaphore, not a guess.

That also rules out the obvious alternative fix. Slots default to
`available_parallelism()`, so six hosted runners advertise roughly 24
between them, and peak in flight was **11**. The fleet was never short of
slots in aggregate - it was short of slots *on the one machine affinity kept
choosing*, while two machines held four each and did nothing.

More slots would not have helped. Spreading the work is the fix, which is
what `-balance` does.

### Pre-flight for `+test-no-qemu`, before spending ninety minutes on it

Read the twelve unrun groups rather than discovering them at minute sixty.

`tests/Earthfile` partitions them by hand, and they are not
interchangeable. Group 2 - the only one ever run, and the workflow default -
is almost all local targets: `+copy-test`, `+copy-tilde-test`,
`+copy-keep-own-test`. The others are not.

- **Group 1 crosses directories**: `./autocompletion+test-all`,
  `./dockerfile+test-all`, `./dockerfile2/subdir+test`. Each sub-Earthfile
  brings its own `local://` context, and the proxy publishes every one
  before a graph naming it can leave. More contexts than any run so far has
  handled, and `contexts published` is the line to watch.
- **Group 3 uses secrets**: `+secrets-test`,
  `+secrets-optional-prefix-test`. A dispatched solve is sessionless and has
  nobody to ask for a secret.

The second one looked like a predicted failure and is not, which is worth
recording as the machinery working rather than as a near miss. `Verdict`
carries a `secrets` flag, `dispatchable()` passes `serving_secrets: false`,
and `REBUCK2_SERVE_SECRETS` is unset in the workflow - so a graph naming a
secret is excluded from dispatch and builds at home. No failure, no
distribution for those targets, and the run stays green.

That is "fail open, never fail wrong" collecting on a bet made long before
this target existed. The exclusion was written when a secret was simply
fatal; it now protects a target nobody had in mind.

The remaining risk is context publishing volume in group 1, which is
measurable and has no exclusion behind it.

### `-imports` must be tested on top of `-balance`, not against the reference

Noticed before spending a run on it, which is the only place noticing is
worth anything.

The imports term adds warmth mass: a candidate holding the parent scores
another 64. Under the comparator as it stands, warmth is the FIRST sort key,
so a bigger warmth number does not just break ties differently - it
concentrates work harder onto whichever machine already holds the popular
parents. That is precisely the fault `-balance` exists to fix, and it is
worse for the machines that were already idle.

So `-imports` alone can only make the 65% worse, however sound the idea is.
Composed with `-balance` it becomes a fair question: does knowing WHERE a
parent lives improve placement, once warmth is being traded against the
queue rather than ranked above it?

The suffixes compose in either order - verified by running the selection
shell against `-ast-balance-imports` and `-ast-imports-balance`, which is
how the `-ast-zstd-nobase` ordering bug was found and the only way I trust
that loop.

Sequence, then: `-balance` alone (in flight), then
`-balance-imports`, then `-bcast`. `-minops` stays off - measured harmful.

## Every mechanism, against the bucket it can actually reach

The phase split makes this checkable for the first time. A lead is
`placing + waiting + building`, measured at 0% / 65% / 35%, so a mechanism
that cannot name its bucket cannot be argued for.

| mechanism | bucket | why |
| --------- | ------ | --- |
| `-balance` | **waiting** | spreads leads off the machine they queue on |
| `-imports` | building | places a lead where its parent already is, so buildkit's pull is local. `build_ms` brackets `build_subtree`, and the parent pull happens inside it |
| `-bcast` | building | pre-positions announced layers on every worker rather than one, so the pull finds them present |
| prefetch fix | building | done: 4,407s -> 3,518s, and 30% off the leg |
| seeding | building | a warm cache mount, inside the same bracket |
| `-zstd` | building | cheaper unpack, same bracket |
| `-minops` | *none* | it removed leads rather than making them cheaper, and measured 5x worse |
| `min_siblings` | *none* | gates on queue depth to decide whether to dispatch at all; the queue is the problem, not the trigger |

Two things fall out.

**Everything except `-balance` competes for the same 35%.** Seeding,
compression, prefetch, imports and broadcast all shorten `building`, and
`building` is a third of lead time. Perfect success on every one of them
cannot reach the other 65%.

**`-minops` and `min_siblings` do not appear.** Both decide *whether* to
dispatch, and the phase split says the dispatch decision costs nothing:
`placing 0s (0%)`. There is no bucket for them to improve, which is a
cleaner statement of why the op floor failed than the five-times-slower
measurement was - it was optimising a term that is zero.

That is the value of the split. Before it, every one of these was arguable.

## `-balance`: 1240s to 1050s, and every machine finally works

Trading warmth against queue depth, on top of the fixed prefetch. Same
target, same 412 solves.

| | prefetch only | `+ -balance` |
| ------------------- | ------------- | ------------ |
| **fleet leg** | 1240s | **1050s** |
| baseline | 227s | 212s |
| amplification | 44.5x | **38.9x** |
| **placement spread** | 53/78/103/182, **2 idle** | **37/38/58/68/77/140, none idle** |
| total lead time | 10,101s | **8,239s** |
| `waiting` | 6,583s (65%) | **3,670s (45%)** |
| `building` | 3,517s (35%) | 4,521s (55%) |
| `placing` | 0s (0%) | 47s (1%) |
| built duplication | 1.4x | 1.7x |

**Every machine took work.** Six workers, none idle, and the biggest share
fell from 182 placements to 140. The `balance` counter read 400 - it
reordered the queue four hundred times, and `affinity` rose to 400 with it,
so the two agreed on almost every placement rather than fighting.

**Waiting fell by 2,913 seconds**, which is 44% of the number this was aimed
at, and the wall clock moved 190s with it.

Everything the trade predicted happened, including the costs.

- `building` rose, 3,517s to 4,521s. That is the trade, made deliberately: a
  cold machine taking work a warm one would have done pays for the parent it
  does not have. Duplication rose with it, 1.4x to 1.7x, for the same
  reason.
- `placing` appeared: 47s, 1%. Non-zero for the first time, because a
  candidate that would have been chosen outright is now sometimes passed
  over and the offer goes further down the list.

The two together cost about 1,050 seconds against 2,913 saved. Worth it, and
it says exactly where the next gain is: **`-imports` and `-bcast` both aim at
`building`**, which this run has just made the majority term. The sequencing
argument from two sections up holds - `-imports` had to ride on top of
`-balance` or it would have concentrated harder - and `-balance` is now in
place for it.

Two runs, two mechanisms, 1773s to 1050s. The first repaired something that
had never worked; the second stopped a working mechanism from working too
hard.

### The coordinator does seven seconds of work in a 1,050-second build

The home-vertices instrument reported for the first time in the `-balance`
run:

```text
[wire] home vertices : 211 ran in 7084ms, 0 cache hit(s)
[wire] service ms    : home 0 (0) away 1049178 (1)
```

**Seven seconds.** 211 vertices ran on the coordinator's own daemon across a
leg that took 1,050, and none of them was a cache hit - so that is seven
seconds of real work, not seven seconds of lucky lookups.

Two things retire on this.

**The gateway is not a bottleneck.** It has been a standing suspicion -
every solve funnels through one proxy, every result is pulled back through
one registry - and the machine doing that funnelling spends 0.7% of the
build computing. Whatever the remaining 38.9x is, it is not the coordinator
running out of hands.

**`service ms` is finally legible**, and it says what the NOT MEASURED
branch predicted: `away 1049178 (1)`. One `Control.Solve`, holding the
entire build, lasting the entire leg. That is why the home/away pair could
never fill - there is exactly one outer solve and it is the whole thing, so
the ratio it was built to compute has one sample and no counterpart.

The pair stays, now that it has a use it did not have: `away` is a
serviceable check that the leg time and the client's wait agree, which is
one more thing that cannot silently drift.

### `-bcast`'s risk, stated before its run

One lane. `prefetch_permits` defaults to 1, and the worker holds that
permit for its whole share rather than per blob - deliberately, so six
announcements cannot interleave into six concurrent pulls.

That was sized for a share. Broadcast makes every worker take every blob, so
the same single lane carries six times the bytes it was tuned for, and the
`-balance` run announced 421 times with up to 13 blobs each.

Three readings, written down first:

- **`building` falls.** The bytes arrive before the lead does, which is what
  pre-positioning is for.
- **Nothing moves.** The lane is saturated, so the prefetch finishes after
  the lead that needed it and the build pulls the blob itself anyway - a
  mechanism that runs, costs bandwidth, and changes nothing.
- **`building` rises.** Worse than nothing: the lane is competing with the
  demand fetches on the same worker for the same network.

If it is the second or third, the next move is `REBUCK2_PREFETCH_LANES`
rather than abandoning the idea - the flag exists, defaults to 1, and has
never been set. That is a cheaper follow-up than it looks, and worth knowing
before reading a flat result as a verdict on broadcasting.

## Correction: `+test-no-qemu` has been run, and at parity

Two sections below I wrote that it "has never run" and built a coverage plan
on it. That is false, and the counter-evidence is in this file - *The whole
of `+test-no-qemu`, at parity*:

```text
one machine 186s   6 machines 525s
PARITY: the same 1 target(s) failed either way
gateway solves 163 - routed 130 - peak in flight 14
op duplication 8.0x sent, 2.3x built
```

All fourteen groups, six runners, one target failing both ways. Principle
18's fetch timeline comes from that same run.

**How I got it wrong:** I grepped `docs/fleet-findings.md` for
`test-no-qemu`, filtered out `group2` to remove the default-target noise,
and read the remainder as talk about a target nobody had run. The section
that says otherwise was in the output; I built the plan without opening it.

**What it changes, and it is mostly good.** 186s against 525s is a ratio of
2.8x - far better than anything `+test-ast` has managed, and the best result
in this document. The wide target was already the fleet's best case before
today's fixes, which is exactly what principle 19 predicts and I was about
to claim as untested.

So the coverage run is a **re-run with a reference**, not a first attempt.
That is worth more: 525s is a number to beat, and the fixes since - prefetch
from 9 applications to 421, and a queue-aware placement that put all six
machines to work - are aimed at the two faults that run would have had.

The rest of the plan stands: `-nobase` for the budget, twelve machines to
test the ceiling claim, deliverable is the failed-target list. Only the
justification was wrong, and it was wrong in the direction of underselling
what already worked.

## Next is coverage, not another `-ast` variation

Deciding the order now, while the current run is still in flight, so the
decision is not made by whichever result happens to be interesting.

`-bcast` targets `building`, which is 55% and the largest term. It is still
the right mechanism. But `+test-no-qemu` goes first, for three reasons that
have nothing to do with which is more appealing.

**It is the mandate.** "Larger and larger parts of the Earthfile" is the
brief, and every measurement TODAY comes from targets with three to five
branches. The fourteen-way target last ran before prefetch worked and
before placement spread beyond four machines.

**It is the only workload that can answer the question the others cannot.**
`+test-ast`'s Amdahl ceiling is 2.97x at seven machines - the graph itself
does not permit more, so a fleet that beats one machine on it is impossible
by construction, and 1050s against 212s was never going to become 200s.
Principle 19 has been asserted in this document for a long time on the
strength of small targets. A fourteen-way target is the first chance to test
it rather than repeat it.

**Its result changes what the next mechanism should be.** Every phase
percentage here comes from a workload where two machines were idle for
structural reasons. On a wider graph the split will be different, and
tuning `-bcast` against `+test-ast`'s 55% risks tuning for a shape that does
not occur at the size that matters.

Pre-registered, so the result cannot be read generously:

- **The deliverable is the failed-target list.** `-nobase` means no parity
  check, so a target that fails needs chasing by hand before it counts
  against the fleet.
- **Ceiling above 6x** would be the first evidence that principle 19 is
  about the workload rather than an excuse.
- **All six machines working**, as in the `-balance` run. If a wider graph
  re-concentrates, the queue penalty is too weak at this scale.
- **A leg under 135 minutes**, or the workers hit their cap and the run
  dies rather than degrading - which is the failure this session already
  paid for once.

### A quantitative prediction for the coverage re-run

The old `+test-no-qemu` numbers, plus today's measured deltas, give
something falsifiable rather than a hope.

Last time, six runners: **186s / 525s**, 163 gateway solves, 130 routed,
peak 14 in flight, 148 leads at p50 2,943ms and max 209,141ms, duplication
8.0x sent and 2.3x built.

What has changed since, all measured on `+test-ast`:

| change | effect there |
| ------ | ------------ |
| prefetch actually announces | leg -30%, cold mount p50 -41% |
| warmth traded against queue | leg -15%, every machine working, `waiting` -44% |

Neither is workload-specific: one repairs a manifest lookup, the other a
sort key. So the honest prediction for six runners is **525s falling to
roughly 350-400s**, and I will take anything under 450s as the fixes
carrying across and anything over 500s as them not.

Three specific things to check, each of which can fail on its own.

- **That 209-second lead.** It is 40% of the whole leg on its own. If it is
  a single test group, no amount of placement helps and the ceiling is that
  lead - which would be principle 19 in its purest form.
- **Duplication 2.3x built.** `-balance` pushed `+test-ast`'s from 1.4x to
  1.7x by design. On a wider graph with less sharing the same trade should
  cost less, so 2.3x should not get much worse; if it does, the queue
  penalty is mis-weighted for graphs this shape.
- **Twelve machines against six.** The old run peaked at 14 in flight on six
  runners, so it was queue-bound in exactly the way `-balance` addresses.
  Twelve should help more here than it would on `+test-ast`, where peak was
  11 and the graph's ceiling is 2.97x.

If 525s does not move, the fixes are `+test-ast` artefacts and this document
has been measuring one target's quirks all day.

### The wide target's critical path is five nested builds

From the earlier `+test-no-qemu` analysis, re-read while planning the
re-run:

> The five leads that own a full `+test-no-qemu` critical path are all
> nested builds at 231-258s each - each longer than the entire 221s
> single-machine build, and clustered the way a queue clusters.

That is the 209-second lead in the newer numbers, and it is not a slow test.
It is a nested `earthly` inside a `RUN`, dialling **back to the
coordinator's gateway** across the network and re-entering through one
funnel - because earthly forwards its own `BUILDKIT_HOST` into every exec,
and in a fleet that address is the coordinator's.

`REBUCK2_LOCAL_NESTED` exists for exactly this: `retarget_buildkit_host`
points a nested build at the daemon on the machine actually running it. It
is **`0` in every CI run**.

So the biggest single cost on the widest target has a purpose-built fix that
has never been switched on in a fleet run. Five leads at ~245s each against
a 525s leg is most of the leg.

It is not going into the coverage run. The deliverable there is the
failed-target list, the switch is unmeasured, and it carries a documented
hazard: nested builds under `--privileged --entrypoint` die on
`could not connect to buildkit: timeout 1m0s` when retargeted. Turning it
on for a run whose job is to find out what breaks would make every failure
ambiguous.

It goes next, on `+test-no-qemu`, alone. Ahead of `-bcast`: `-bcast` shaves
a term that is 55% spread over 400 leads, and this one addresses five leads
holding perhaps 40% of the critical path on the target that matters.
