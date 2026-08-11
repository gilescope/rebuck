# Fleet findings

Everything below was measured, mostly by `rebuck2/scripts/fleet.sh`, and much
of it overturned something the previous section believed. It reads
newest-first: each investigation was written at the top as it finished, so the
early sections are conclusions and the late ones are how the mechanism came to
exist at all.

It lived in `src/proxy.rs` as a module comment until it reached 1342 lines --
a third of the file, and a chronological log rather than documentation. The
code kept the invariants a maintainer needs; the reasoning and the numbers are
here.

Most of these claims are now mechanically checked. `rebuck2/scripts/fleet-check.sh`
runs the structural ones - where work was placed, whether the bytes match,
whether a build survived something being destroyed - and asserts them. It
deliberately does not assert wall clock: those numbers move for reasons that
have nothing to do with this code, and a suite that fails on a busy laptop
gets switched off.

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
| `REBUCK2_GRAFT` | rebuilding known ancestry | costs 107s |
| `REBUCK2_FLEET_CACHE` | rebuilding known ancestry | costs 334s |

Only two of those have a measured benefit, and only affinity is unambiguous.

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
