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
| *Superseded figures* (just below it) | the number you are about to read may be one this file has since withdrawn |
| *Run D, and the correction it forces* | the amplification is 6-9x in CPU, and one run does not measure it |
| *`-balance` evens the counts* | leads vary 1.96x, CPU varies 8.2x - the cheapest remaining win |
| *The workload sets the ceiling* (principle 19) | most "the fleet is slow" numbers are Amdahl |

**Read `docs/how-this-lies.md` too**, if you are going to trust any number
here. Twenty-three ways this system has produced a confident wrong answer,
six of them found in a single night - including four where the confident
wrong answer was mine and the correction is in the same file.

**The state in one paragraph.** Every named target in earthbuild's Earthfile
runs through the fleet; coverage is done. The fleet spends about **6.5x the
CPU** one machine spends on the same target, which is the whole remaining
problem. Two runs of identical code put it at 6.5x and 8.7x, so treat it as
a range with a third of variance rather than a figure; every LARGER number
in this file divided a sum over concurrent leads by a wall clock. Cache-mount
seeding works mechanically and is not yet deliverable at scale. Nothing
currently explains the 6.5x.

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
| No offer was ever refused, on the runs that worked | `placing 0s (0%)` - and that is ALL that field says: it is stamped only on a decline, so it can never report the cost of choosing |
| Queueing WAS the largest cost | `waiting 6583s (65%)`; trading warmth against queue depth cut it to 3670s (45%) |
| Spreading beats concentrating, and costs what it should | 1240s -> 1050s, no idle machine, `building` up 1004s against `waiting` down 2913s |
| **Building is now the majority term** | 4521s (55%) - what `-bcast` aims at |
| Placing a lead near its parent does not pay as weighted | `-imports`: 1050s -> 1475s, `waiting` +2849s, a machine idle again |
| A brake sized for one preference does not hold two | the imports term fired 1724x against 361 reorders |
| The instruments are not the problem | the status tap: 1ms across 3004 frames |
| **The whole of `+test-no-qemu` runs through the fleet** | 520s, fourteen groups, one failed target matching the reference |
| **Every named target runs through the fleet** | `+all` at 2,724s with zero failed targets was the last |
| `+all` is a chain, not a fan-out | occupancy 0.99 against a 2.81 ceiling, `building` 84% |
| The h2 collapse is cured by `REBUCK2_SANDBOX_HOST` | it died at 39 targets without it, completed with it |
| A fix is worth what its target's bottleneck is worth | prefetch and `-balance`: 41% on `+test-ast`, 0% here |
| `WITH DOCKER` cannot be dispatched | mount type 100 is `HOST_BIND` in earthly's fork |
| That ceiling is 2.32x on six machines | 670s of 2,120s serial by construction |
| **And cutting at the exclusion does not raise it** | 13s before the first exclusion against 2,223s from it on - 1% |
| A red run can be a green experiment | coordinator green, one worker exiting non-zero, intermittent |
| `-bcast` breaks the build on `+test-ast` | parity failed, one target unreached - and the media-type error is NOT a broadcast mechanism: it appears in run B with no broadcast at all, among 185 other fetch failures |
| **The leg is lead time over concurrency** | 8,239s / 7.85 = 1049 against a measured 1050s |
| `+test-ast` is 15x off its own ceiling | 2.97x at seven machines means a 71s best leg against 1050s |
| The prefetch counting gate is dead | 0 acceptances, 14 refusals, 292 prefetches bypassing it |
| The worker vertex tap costs 15% and is confounded | 1050s -> 1207s; 4,964s of vertex time in a 1,207s leg |
| **The apparatus still works after ~640 commits** | checkpoint run: parity green, 1113s leg, 201s baseline, 412/412 routed |
| The amplification is inside `building` | placing 0s, waiting 4115s, building 4917s - the RATIO to the baseline is unsettled, see the correction |
| More machines cannot fix it | building alone needs 25 machines to reach the baseline; Amdahl caps at 5.67x |
| Cache-mount seeding has never once run | four mechanical reasons, plus a fifth that defeats even a warm bank |
| Seeding trades critical path for total work | CPU 9.2x -> 6.5x, leg ~1065s -> 1396s+. It loses HERE because this fleet is critical-path bound, not because it does nothing |
| Delivery failure is NOT why the fleet is slow | 325 prefetch misses cost 7 lead failures; the lazy path absorbs the rest |
| Cold cache mounts are NOT the amplification | each worker ends with 2-4 mounts, so they are reused across its ~69 leads |
| The fleet costs ~6-9x the CPU of one machine | 3900/604 and 5155/592 on two runs of identical code - like-for-like at last, and not yet precise |
| The baseline parallelises 2.73x internally | 604 CPU-s in 221s wall - which is why every wall-clock ratio overstated |
| Worker load is 8.2x unequal | 160s to 1316s of CPU across six machines on one target |
| The seeding mechanism itself works | `check-seeding` green on x86 CI and arm64 local: write, harvest, seed a COLD mount, read back |
| A diagnostic deleted the data its consumer needed | `check-seeding` truncated the inputs file 3s before `harvest-cache` read it |
| Merging instead of truncating fixed the harvest | run A harvest 8s (read nothing), run B 65s against 1.6 GB held |
| An empty seed is not neutral | emitted anyway, applied to 360 mounts, leg 1113s -> 1783s |
| A seed was split 1-in-N across the fleet | `usable_seeds` says "on every machine" and used the splitting path; 274 leads failed fetching it |
| One timeout bounded two different failures | 5s around dial AND transfer makes any blob over ~50 MB unfetchable by construction |
| The two peer fetches disagreed | driver over-bounded, worker unbounded - two wrong answers, nothing connecting them |

**Open.** Believed for a reason, not measured.

| question | why it is open |
| -------- | -------------- |
| Whether the small leads matter | BOUNDED: they are half the count and at most 15% of lead time, so `worth_offering` is worth wiring and cannot be the 6-9x |
| **Why the BIG leads cost what they do** | THE live question. The top 10% of leads hold 45% of lead time at 87-302s each - the size a build farm wants - and `+all-binaries` proves large leads can win. Something makes these cost several times their one-machine equivalent |
| **What the per-unit cost is** | Two samples of the same binary: 6.5x and 8.7x in CPU. So ~6-9x with a third of run-to-run variance, not a number. Cold mounts eliminated. Export bounded at 5-10%. Delivery eliminated (325 prefetch misses cost 7 leads). No candidate for the remainder |
| ~~What the run-to-run noise band is~~ | MEASURED: worker CPU total 0.1%, baseline CPU 0.5%, leg 5.1%. The per-worker split is not reproducible at all |
| How much of the fleet's traffic crosses a wire | `SERVED_BYTES` mixes loopback with peer serving; `SERVED_LOCAL_BYTES` exists and has never reported. The per-worker `N MiB left it` lines now answer most of this |
| Whether the fleet repeats itself, and by how much | the coordinator reports 1.1x; per-worker figures now print and run to 21x re-served |
| What made the reference run take 50 minutes | not the tap, which costs 1ms. Still unexplained, and now suspected to be ordinary variance |

**Retracted.** Written here confidently and wrong. Left in place with the
correction attached, because a deleted mistake gets made again.

| claim | what was wrong |
| ----- | -------------- |
| "A lead costs what it fetches" | bytes served correlate with lead duration at r = 0.06 |
| "24.7 GiB moved across the fleet" | `SERVED_BYTES` is bytes served, not bytes fetched |
| "...therefore it was loopback" | `upstream: None` says where a registry FETCHES, not who it SERVES |
| "93x re-served" | fleet-wide served divided by one machine's distinct - two different registries |
| `worth_spreading`, a worker-count cap | 162 machine-seconds read as wall clock |
| "94% of lead time is cache mounts" | `mounts_ms` counts the WHOLE lead of any lead naming a cache - a filter, not a duration |
| "placing is free, the dispatcher is not the problem" | the field is stamped only on a decline; a zero was never evidence about placement |
| "seeding costs 1872s rewriting every graph" | wrong mechanism - that 1872s is decline-and-re-offer round trips |
| "the seed address is unreachable from a worker" | `172.17.0.1:15000` correctly names each worker's own mesh-backed registry; the bytes were not there yet |

**Superseded figures.** This file keeps its mistakes in place, so a number
you meet in the middle of it may be one. These are the ones that recur, and
what each became:

| you will read | it is now | why |
| ------------- | --------------------------- | ------------------------------- |
| `12.5x` | see below | earliest form of the same figure |
| `21.3x` | see below | fleet work over baseline wall |
| `24.5x` | **17.9x**, then unsettled | lead-seconds divided by a wall clock |
| `13.6x` | **~10x**, then unsettled | same, after duplication |
| `74%` utilisation | **54%** | overlap not divided out |
| `94%` cache mounts | not a duration at all | `mounts_ms` was a filter |
| `mounts_ms` | `lead_ms_with_cache` | the name stated a filter as a total |

The per-unit cost currently has no settled value: somewhere between about 6x
and 18x depending on which units, and the CPU counters added on 2026-08-12
are the first instrument that can narrow it honestly. Use
`scripts/leg-arithmetic.sh` rather than dividing by hand - it exists because
dividing by hand is what produced four of the seven rows above.

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

So it shipped OFF behind `REBUCK2_ADAPT=1` - and the flag is **gone from
the code**: no `.rs` file mentions it, though `docs/running-a-fleet.md`
listed it for operators until today. Whether it was removed deliberately or
lost in a refactor, the effect was a documented switch that did nothing, and
the reasoning below is what survives. The measurement is kept: the
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
| `+lint-all`              | 3 independent lint targets, no docker  | parity - fleet 252s against an 88s baseline (was 628s)    |
| `+all-buildkitd`         | multi-arch buildkitd, needs qemu       | **parity**, 1161s vs 1188s - arm64 half is undispatchable |
| `+test-ast`              | 412 solves, the densest fan-out here   | **parity** - the target every mechanism is measured on    |
| `+all`                   | the whole Earthfile                    | **parity**, 2724s, zero failed - a chain, not a fan-out   |

**Coverage is complete.** Every named target in earthbuild's Earthfile has
been through the fleet with parity against a one-machine baseline. The
standing goal of "larger and larger parts" has no larger part left to take;
what remains is the cost of taking them, which is the rest of this file.

`+test-ast` earns its place at the bottom of the list and the top of the
attention: it is where the 6.5x CPU amplification was finally measured,
because 412 solves on a 221-second baseline is the densest fan-out in the
repo and the only target where a mechanism's effect is visible above noise.
`+all` is the opposite - occupancy 0.99 against a 2.81 ceiling, one solve in
flight at a time, because earthly orders its targets and the fleet cannot
reorder them.

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
| `-imports` | building, **but it costs more in waiting** | it does reach `building` - 4521s to 4249s - and put 2849s back into `waiting` by sending leads to the machine that already had a queue. Off. |
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

The rest of the plan needed one fix, and it is the one-variable rule
applied to my own plan. **Six machines first, not twelve.** The reference is
525s on SIX runners; a twelve-machine leg beside it conflates the fixes with
the machine count, and the prediction I registered - 525s to roughly
350-400s - would not be testable against it.

So: `-tests-balance-nobase` at six, which tests the prediction and produces
the coverage list in one run. Then `-w12` afterwards as its own variable,
against the six-machine number this run establishes. The `-w12` branch stays
listed; it is simply second.

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

## `-imports` fails, in exactly the way it was predicted to

| | `-balance` | `+ -imports` |
| -------------------- | ------------------- | ------------------- |
| **fleet leg** | **1050s** | **1475s** |
| amplification | 38.9x | 48.7x |
| total lead time | 8,239s | 10,769s |
| `waiting` | 3,670s (45%) | **6,519s (61%)** |
| `building` | 4,521s (55%) | 4,249s (39%) |
| **placement spread** | 37/38/58/68/77/140 | **44/51/54/83/182, 1 idle** |
| built duplication | 1.7x | 2.1x |
| `affinity_imports` | - | **1724** |

The prediction, written before the run:

> `building` could fall while `waiting` rises by more. Sending a lead to the
> machine holding its parent means sending it to a machine that has been
> doing related work - which is a machine with a queue.

**That is what happened, to the number.** `building` fell 272s, which is the
term `-imports` aims at and it did move it. `waiting` rose 2,849s, which is
ten times as much, and one machine went idle again. The whole `-balance`
gain was given back and then some.

`affinity_imports` fired **1,724 times** against `balance`'s 361. The
imports term did not tip a few close decisions - it fired roughly five times
for every time the queue penalty reordered anything, which means it was not
being traded against the brake, it was overwhelming it. Principle 27 named
this shape and I built the counterweight at one warm item per queued lead;
adding a second 64-point term to warmth doubled the thing being braked
without touching the brake.

**The fix is arithmetic, not conceptual.** Both terms are worth 64 and a
queued lead cancels 64, so a candidate holding a parent AND a warm mount now
needs three leads queued before an idle machine wins. Either the imports
term is worth less than a mount, or a queued lead is worth more than one
item - and the two runs give a ratio to pick from rather than a guess:
`waiting` rose 78% when `affinity_imports` fired 4.8x per `balance`
application.

Not tuning it now. `-imports` goes off, `-balance` alone is the shipped
configuration at 1050s, and the coverage run - which is what the mandate is
actually about - goes next on six machines. Tuning a weight against
`+test-ast`, whose ceiling is 2.97x and where two machines are idle for
structural reasons, is exactly the mistake the ordering argument warned
about two sections up.

### What the 525s reference is, exactly

Checked before comparing anything to it, because "the previous run" is a
phrase that hides how much moved in between.

`3b4271e`, recorded **the same day** - not an old number from another era of
this branch. But 291 commits back, **133 of them touching `rebuck2/src`**,
and 20 of those name prefetch, balance, affinity, warmth, lead timing,
timeouts or `by_hash`.

So the comparison is fresh and it is not clean. If the leg improves, the two
fixes measured on `+test-ast` are the largest known contributors and they
are not the only changes. What can be said honestly:

- **The direction is attributable.** Both measured fixes cut lead cost, and
  nothing between the two commits was measured to make anything slower.
- **The magnitude is not.** Any number I quote is "525s then, X now, with
  133 source commits between", and that is the sentence to write rather
  than a delta with a cause attached.
- **The failed-target list is clean either way.** It is a count of what
  broke, not a difference of two timings, which is exactly why the coverage
  deliverable was chosen to be that and not a clock.

The clean version of this experiment would have been to run `+test-no-qemu`
at `3b4271e` and again at HEAD. That is two more 45-minute runs to attribute
a number I already have a mechanism-level explanation for, and the mandate
is coverage. Noted as the shortcut it is.

## The coverage run: died on h2, and named two real blockers first

`+test-no-qemu`, six machines, `-balance`, no baseline. **It died** - and the
died-check caught it, which is the one part of this that went to plan:

```text
Error: h2 protocol error: error reading a body from connection
fleet failed targets: 0
fleet touched 39 target(s)
::error::the fleet leg neither succeeded nor named a failed target - it died
```

Zero failed targets and a non-zero exit is precisely the shape that once got
reported as *beating* a baseline. It is now an error with a name.

It reached 39 targets and 138 solves against the old run's 163, so this is
not a comparison with 525s and I am not going to make one.

What it did establish, and both are bigger than a clock.

### Thirteen solves cannot be dispatched at all

```text
excluded: UnknownMount(100) ["env EARTHLY_DOCKER_LOAD_REGISTRY=..."]  x13
excluded: Insecure []                                                  x4
```

`WITH DOCKER` targets carry a mount type the dispatcher does not know -
buildkit mount type 100, holding earthly's docker-load registry handle - so
seventeen solves are refused and build at home. That is `dispatchable`
working exactly as designed: fail open, never fail wrong.

It is also the ceiling. `+test-ast` has none of these, which is why every
number in this document comes from a target that happens to contain nothing
the dispatcher must refuse.

### The coordinator did eleven minutes of building

```text
[wire] home vertices : 309 ran in 670427ms, 126 cache hit(s)
```

**670 seconds at home**, against seven seconds on `+test-ast`. The
home-vertices instrument, built this morning to retire the suspicion that
the coordinator was a bottleneck, has just found the case where it is one -
and the cause is the exclusions above, not the gateway.

`occupancy 1.41` against `ceiling 1.65`: this workload, as executed, barely
parallelises. Not because the graph is serial - fourteen independent groups
are not - but because a seventh of the solves cannot leave the machine, and
they are the long ones.

### And placement went bad again

```text
6 worker(s) took work, 122 placements: 2 2 3 5 41 69
```

Every machine took something, so `-balance` is doing its job in the sense it
was measured for. But two machines took 90 of 122 placements. On this
workload the queue penalty is not enough - which is the shape principle 27
predicts when the preference is strong and the work is lumpy, and it is the
first evidence that the 64 constant is target-specific.

### `WITH DOCKER` is undispatchable, and fail-closed caught it

The seventeen exclusions in the coverage run deserved five minutes of
reading rather than a guess, and the guess would have been wrong.

The message names an env var - `env EARTHLY_DOCKER_LOAD_REGISTRY=...` - and
earthbuild passes that as `llb.AddSecret(..., SecretAsEnv(true))`, whose
value is just a list of image names, passed as a secret only "to prevent
busting the cache, as the intermediate image names are different every
time". That reads like a misclassification: a harmless secret refused as an
unknown extension, and therefore unliftable by `REBUCK2_SERVE_SECRETS`,
which exists precisely to lift secrets.

It is not. From the fork's own proto:

```text
MountType_HOST_BIND MountType = 100 // Earthly specific.
```

**Type 100 is a host bind.** A path from the machine running the build, which
a remote worker cannot satisfy at any price. The env var is just what else
that op carries. Refusing it is correct and no flag should lift it.

So the dispatch ceiling on `+test-no-qemu` is `WITH DOCKER`, and it is
structural: those targets bind the host, and a host is the one thing a fleet
cannot ship. Seventeen of 138 solves, and they are among the longest - 670
seconds of coordinator building.

### The comment that said this could not happen

`dispatch.rs` reasons about exactly this case and concludes it cannot arise:

> a host bind can only reach us from an earthly client, and those graphs are
> already excluded by the secret and ssh mounts that come with it. Guessing
> at a detection rule for an encoding this tree cannot produce would be a
> check that never fires, tested by nothing.

Host binds reached us thirteen times in one run. The reasoning was wrong -
`WITH DOCKER` brings a host bind without bringing a secret or an ssh mount -
and the code was right anyway, because the fail-closed branch beneath that
comment caught every one:

```rust
// Fail CLOSED on anything else. A number we do not recognise is a
// fork extension whose requirements we cannot see.
if !KNOWN_MOUNTS.contains(&m.mount_type) {
```

An allow-list caught what the argument for not needing one said would never
arrive. That is the whole case for allow-lists over reasoning, and it is
worth more than the finding it produced.

### What the host-bind ceiling costs, in numbers

Seventeen solves that cannot leave the machine held **670 seconds** of
building, against 1,450 seconds that did leave. So on `+test-no-qemu` as it
stands, 32% of the work is serial by construction - not by dependency, but
because `WITH DOCKER` binds a host.

| machines | best possible speedup |
| -------- | --------------------- |
| 6 | **2.32x** |
| 12 | 2.68x |
| 24 | 2.90x |
| infinite | **3.2x** |

That is the answer to "how much of this can be distributed", and it is
worth more than any scheduling result: **six machines cannot beat 2.32x on
this target however perfect the placement, and doubling to twelve buys
0.36x.**

It also retires the `-w12` experiment for this target before it costs
forty-five minutes. Twelve machines was going to test whether the ceiling
is the workload's - and the ceiling is the workload's, calculable from one
run, at 2.68x against the 2.32x six already permits. There is no scheduling
work worth doing for that 0.36x while a third of the build cannot move at
all.

Coarse on purpose: `home` is buildkit vertex time and `away` is the sum of
worker-side lead durations, which are not the same clock. The conclusion
survives being wrong by a factor of two in either direction - at 20% serial
it is 5x, at 45% it is 2.2x, and the shape of the answer does not change.

The way to raise this ceiling is not a better scheduler. It is to make
`WITH DOCKER` dispatchable, which means a worker's dind reaching the images
the coordinator loaded - and that is the mesh problem this project already
solves for base images, applied to a registry earthly stands up per build.
Whether that is worth doing is a real question. Pretending the scheduler
can reach it is not.

### How much of the Earthfile is behind the host bind

**Thirty of earthbuild's 192 Earthfiles contain `WITH DOCKER`** - 16% of the
files, and they are not evenly spread: five uses in the root Earthfile, two
in `tests/`, the rest in the per-test directories the groups pull in.

So the ceiling is not a corner case and it is not most of the tree either.
It is one construct, used in a sixth of the files, holding a third of one
target's work.

Which makes the size of the prize legible. Making `WITH DOCKER` dispatchable
would raise `+test-no-qemu`'s ceiling from 3.2x toward whatever the
dependency graph allows, and it would do nothing at all for `+test-ast`,
`+lint-all` or `+all-binaries`, none of which use it. It is a targeted piece
of work with a bounded payoff, not a general improvement.

And it is the only remaining item on this list that raises a CEILING rather
than approaching one. Every other mechanism - prefetch, balance, imports,
broadcast, seeding, compression - moves the fleet closer to a limit set by
the workload. This moves the limit.

Whether to do it is a judgement about how much `WITH DOCKER` matters to the
builds someone actually wants distributed. What is no longer in doubt is
that no amount of scheduling reaches past it.

### One host bind blocks a whole solve, and the cut already exists

`dispatchable_when` is a single `any`:

```rust
let blocked = self.exclusions.iter().any(|(_, e)| !lifted_by(e, allow));
```

So one op with a host bind keeps the ENTIRE graph at home, including every
op in it that has no host bind. On a `WITH DOCKER` target that is the dind
setup poisoning the test that follows it, and it is most of the 670 seconds.

Two things make this look tractable rather than fundamental.

**The position is already recorded.** Exclusions are `(usize, Exclusion)` -
the index of the offending op, kept and never used for anything but the
diagnostic message. Everything needed to say *where* the graph stops being
dispatchable is in hand.

**The cut already exists.** `cut_prefix` publishes a prefix of a graph as a
separate image so peers can start from it - the same OnceCell as contexts
and base images, `[proxy] publishing a N-op prefix before an M-op graph`. It
fires twice a run and it cuts on a different criterion; nothing about the
mechanism cares why the cut is there.

So the shape of a fix is: cut at the last excluded op, build the poisoned
prefix at home, dispatch the clean suffix. That is not a new subsystem, it
is an existing one pointed at the index the verdict already carries.

**Not attempting it now, and the reason is not caution.** It is unmeasured
whether the clean suffix of a `WITH DOCKER` target is worth anything - the
dind setup may BE the expensive part, in which case cutting it out
dispatches a rounding error and the ceiling barely moves. That is one
instrumented run to find out: attribute the 670 seconds to ops before and
after the excluded index, which the verdict already knows.

Measure which half the time is in before building anything. This document
contains five mechanisms built before that question was asked.

## The whole of `+test-no-qemu`, through the fleet, alive

`-balance -sandbox`, six machines, no baseline. **It completed.**

```text
one machine ?s   6 machines 520s
fleet failed: 1 target(s)
[wire] verdict : solves=150 routed=127 home=20 peak_solves=10 occupancy=3.35 ceiling=2.99
[wire] lead phases : placing 0s (0%) waiting 443s (27%) building 1196s (73%)
[wire] home vertices : 404 ran in 737780ms, 249 cache hit(s)
```

All fourteen groups, one failed target - the same one the reference run
failed - and no h2 collapse. The prediction held exactly: **no h2 error and
the leg completes past 39 targets.**

The cure was `REBUCK2_SANDBOX_HOST`, recorded in this document before today
and switchable only through a `workflow_dispatch` input that a workflow off
the default branch can never receive. Third mechanism today whose sole
defect was being unreachable, after `LOCAL_NESTED` and the machine count -
and the one that decided whether the mandate's own target ran at all.

### And the two shipped fixes bought nothing here

**520s against the reference's 525s.** Prefetch and `-balance`, worth 41% on
`+test-ast`, are worth zero on this target.

That is not a disappointment, it is principle 19 arriving with a
demonstration. Look at where the time is:

| | `+test-ast` | `+test-no-qemu` |
| ------------ | ------------ | --------------- |
| `waiting` | 3,670s (45%) | **443s (27%)** |
| `building` | 4,521s (55%) | **1,196s (73%)** |
| solves | 412 | 150 |
| home vertices | 7s | **738s** |

`-balance` attacks queueing. On `+test-ast` queueing was 65% before it and
45% after; here it is 27% to begin with, so there is almost nothing for it
to take. Prefetch attacks per-lead fetch cost across 412 leads; here there
are 128.

**A fix is worth what its target's bottleneck is worth.** Both fixes were
found, sized and validated against a workload whose bottleneck this workload
does not have. Nothing was wrong with either measurement; they simply do not
transfer, and the phase split is what makes that legible instead of
mysterious.

The bottleneck here is `building` at 73%, plus 738 seconds of home vertex
time behind the host-bind exclusions. That is the `WITH DOCKER` ceiling,
already computed at 2.32x on six machines - and `occupancy 3.35` against
`ceiling 2.99` says the fleet is already working harder than the graph's
critical path permits.

There is no scheduling work left worth doing on this target.

### What `trapped ops` will and will not settle

Written before the number, and the second half matters more than the first.

**The threshold.** Suffix above 50% of refused ops: most of a `WITH DOCKER`
graph is clean work sitting behind one host bind, and pointing `cut_prefix`
at the excluded index is worth building. Below 20%: the bind is nearly the
whole graph, and the 2.32x ceiling stands as physics. Between: marginal, and
the answer depends on something this number does not measure.

**What it cannot settle, whatever it says.** Ops are not time. A suffix of
80% of the ops could be 5% of the seconds - one `RUN` that takes four
minutes and forty trivial ops after it. So a large suffix is **necessary and
not sufficient**: it says a cut is possible, never that it pays.

That distinction is the whole lesson of `-minops`, which counted ops as a
proxy for work, was measured five times slower, and taught that op count
measures size while the fleet's problem is elsewhere. The same proxy is
being used again here, for a different question, and it deserves the same
suspicion.

So the honest ladder is:

1. `trapped ops` says whether a cut is **possible** - one run, already going.
2. If it is, attributing the 738 seconds of home vertex time to before and
   after the excluded index says whether it **pays** - a second instrument,
   not yet built, and buildkit's vertex digests do not map to our op indices
   without work.
3. Only then is there a case for building the cut.

Recording step 2 as unbuilt rather than discovering it after reading a
promising number off step 1. The temptation with a good result is to skip
straight to the mechanism, and this document is largely a record of what
that costs.

### `-sandbox` cut the critical-path leads by a third, and `-nested` is now doubtful

Lead durations from the successful `+test-no-qemu` run, deduplicated:

```text
298 lead lines (each logged twice), p50 2,265ms, p90 18,742ms
longest, distinct: 163,698ms  162,742ms  162,284ms
```

Three leads at **~163 seconds**, against the five at **231-258s** recorded
before `-sandbox`. So forwarding `buildkitsandbox` into the execs took about
a third off the builds that own the critical path - which is the mechanism
working exactly as described, on the leads it was described for.

What is left is probably work. Three leads at 163s, if they serialise, is
489 seconds of a 520-second leg: **the critical path is now three nested
builds and almost nothing else.**

That makes `-nested` doubtful rather than promising. It attacks the same
leads by a different route - rewriting the graph's `BUILDKIT_HOST` instead
of changing what earthly hands the exec - and `-sandbox` has already made
those builds local. Two mechanisms for one problem, one of them measured to
work, and no evidence the remaining 163s is transport rather than
compilation.

It stays wired and stays off. The case for running it would be evidence that
those 163 seconds contain waiting rather than building, and the phase split
cannot see inside a lead.

This also explains the 520s against the reference's 525s. The critical leads
got a third faster and the leg did not move, because the leg is not bound by
them: 738 seconds of home vertex time, overlapping, behind the host-bind
exclusions. Shortening the longest leads in a build whose limit is the work
that cannot leave changes the shape and not the clock.

### `-bcast` belongs on the wide target, not the narrow one

The bucket table said `-bcast` aims at `building`, and the plan had it
queued against `+test-ast`. The phase splits say that is backwards.

| target | `building` |
| ------ | ---------- |
| `+test-ast` | 4,521s of 8,239s (55%) |
| `+test-no-qemu` | 1,196s of 1,639s (**73%**) |

`-bcast` pre-positions announced layers on every worker instead of one. Its
whole effect lands in `building`, and `building` is 73% of the wide target
and 55% of the narrow one - so the same mechanism has half again as much to
reach for on `+test-no-qemu`.

The lane risk cuts the other way and has to be weighed with it: 128 leads
here against 414 there, so a single prefetch lane carries far fewer
announcements and is less likely to saturate. Both considerations point the
same direction, which is unusual enough to note.

So `-bcast` runs on `+test-no-qemu` when it runs. The generalisation worth
keeping is smaller than the reordering: **choose the target by which bucket
the mechanism touches, not by which target is cheapest to run.** Nine hours
of this document were written against `+test-ast` because it is quick and
nothing in it fails on purpose, and its bottleneck turned out not to be the
one the wide target has.

### `contexts published: 11` - the group-1 risk, finally read

The pre-flight named context-publishing volume as the one group-1 hazard
with no exclusion behind it: group 1 crosses into `./autocompletion`,
`./dockerfile` and `./dockerfile2/subdir`, each bringing its own `local://`.
The number was printed every run into `proxy.log`, which is uploaded as an
artifact and which nothing I had been reading ever opened.

From the successful `+test-no-qemu` run:

```text
[wire] contexts published: 11
[wire] arrivals       : 150 solves spread over 511531ms (first 0, last 511531)
[wire] op duplication : 5620 sent / 415 distinct = 13.5x sent, 1.7x built
```

**Eleven contexts.** Against the one or two `+test-ast` names - so the risk
was real, it is five times the volume, and it was published concurrently
because that run carried the `join_all` change made for exactly this target.
Serially those eleven uploads would all have sat on the critical path before
the first subtree could leave.

A named risk, a fix built for it before it was measured, and the measurement
arriving afterwards to say the risk was real. That is the right order by
luck rather than by design - the fix went in because the serial loop was
obviously wrong, not because anyone knew there would be eleven.

`arrivals` is worth having too: 150 solves spread evenly across the whole
511-second leg rather than arriving in a burst. The fleet is fed
continuously, which is why `placing` is 0% and why queueing, not admission,
was the thing worth fixing on the narrow target.

## `trapped ops`: 5%, and I measured the wrong end

```text
[wire] trapped ops : 23 refused solve(s), 1012 ops, 55 past the last
                     exclusion (5%) - what a cut could still send
```

Against a pre-registered threshold of "below 20% means the host bind is
nearly the whole graph and 2.32x stands as physics". Five percent. By the
rule I wrote, the cut is not worth building.

**The rule was wrong, and the number says something better.**

Five percent *after* the last exclusion means **ninety-five percent before
it**. Twenty-three refused solves, 1,012 ops, and the average last exclusion
sits at op 42 of 44 - host binds are at the END of a `WITH DOCKER` graph,
not the start. Of course they are: the dind setup and the image loads happen
after the dependencies are built, because they need them.

So the dispatchable part is the **prefix**, not the suffix. And buildkit
marshals in topological order, so those 957 ops do not depend on the
excluded ones - they are exactly what a peer could build.

`cut_prefix` already publishes a prefix of a graph as a separate image for
peers to start from. It exists, it fires, and it is pointed at a different
criterion. The fix is to point it at the FIRST exclusion instead of my
suffix: dispatch everything before the host bind, keep the bind and its tail
at home, let home build on top of what came back.

I built an instrument to measure "what could still be sent" and defined it
as the suffix, because the mental model was a poison spreading forwards.
Dependencies run the other way. The number is right, the threshold was
right, and the conclusion I attached to the threshold was backwards - which
the number itself caught, because 5% is only disappointing if you assumed
the wrong end mattered.

**What is still unknown, and it is the same question as before.** Ops are
not time. 95% of the ops being dispatchable says nothing about whether they
hold any of the 869 seconds of home vertex time. The dind setup may be
cheap and the tests it runs expensive, in which case the prefix is
dependency-building already shared with other targets, and cutting buys
nothing. That measurement remains unbuilt.

### The op-digest assumption, confirmed in buildkit's source

Two mechanisms rest on a buildkit vertex digest being `sha256:<hex of the
marshalled op bytes>`: grafting, which looks up built ancestors by that key,
and the new `refused time`, which attributes home vertex timings to op
positions. Neither had ever confirmed it - grafting has fired zero times in
every run because it was switched off, so its silence proved nothing either
way.

`solver/llbsolver/vertex.go:337`, in the fork checked out three directories
from this one:

```go
for _, dt := range def.Def {
    dgst := digest.FromBytes(dt)
```

`digest.FromBytes` over each marshalled op, which is exactly
`format!("sha256:{}", sha256_hex(b))`. **The assumption holds**, so the
mapping works and grafting's key is sound - its zero was the flag and
nothing else.

Two minutes of reading a dependency, against eighteen minutes of run that
would have answered the same question less definitively. The fork has been
checked out the whole time and I spent much of today treating its behaviour
as something only a fleet run could reveal.

One caveat kept rather than dismissed: `vertex.go:305` computes a different
digest on a rewrite path. The load path uses the bytes as received, and the
proxy sees the definition the coordinator's daemon loads, so 337 is the one
that applies - but a fork that rewrote ops before digesting would show up as
partial matching, which is what the `Xms of Yms matched` figure exists to
make visible.

## `refused time`: 1%. The `WITH DOCKER` cut buys nothing

```text
[wire] refused time : 13s before the first exclusion, 2223s from it on
                      (1% dispatchable if cut, 2236772ms of 750337ms home
                      time matched) - ops are not time
[wire] trapped ops  : 35 refused solve(s), 863 ops, 62 past the last
                      exclusion (7%)
```

Thirteen seconds against 2,223. **One percent.** The prefix of a
`WITH DOCKER` graph holds essentially none of its time, and the two figures
together say why: 93% of the ops precede the last exclusion, 7% follow it,
and the first exclusion arrives after 13 seconds of work. So the host binds
bracket the expensive part rather than sitting at one end of it.

Cutting at the first exclusion dispatches 1% of the time. Cutting at the
last dispatches 7% of the ops. Neither is worth a mechanism, and the
`WITH DOCKER` ceiling of **2.32x on six machines stands as the final
answer** for this target.

That closes the last open question. Everything remaining on `+test-no-qemu`
is bounded by work that binds a host, and no scheduler reaches past it.

### And the sanity figure caught my own instrument

`2236772ms of 750337ms home time matched` - **matched exceeds the total, by
three times.**

Shared ops. A digest appearing in five refused graphs has its time added
five times, once per graph, because I sum per-graph without deduplicating
across them. The absolute totals are inflated; the RATIO is not, because
both sides inflate together.

So the 1% stands and the seconds do not. That distinction only exists
because the guard prints both numbers instead of gating on them silently -
the property argued for two commits ago, on the grounds that a guard can be
wrong about its verdict but must not be wrong about its evidence. It was
built expecting a mapping FAILURE and caught a double-count instead.

The fix is a `BTreeSet` of digests across all refused graphs before summing,
and it is not worth making: the answer is 1%, deduplication moves both
numerator and denominator, and nothing downstream depends on the absolute
seconds. Recorded rather than fixed, so the next reader does not take
`2236772ms` for a measurement.

### A red run whose coordinator was fine: one worker exits non-zero

The `refused time` run reports `completed/failure`, and the failure is not
in the measurement. `worker (5)`'s *serve until the coordinator goes* step
exited non-zero at 12m48s - well inside its 8100s cap - with no panic, no
error line, and driver-vitals still printing at the end. The other five
workers exited cleanly, the coordinator's steps all passed, and every number
above came out of that run.

Signature, so a future red run is not misread as a real failure:

- coordinator job green, one or two worker jobs red
- the worker's last lines are ordinary - vitals, a completed job, a prefetch
- exit well under the timeout, so it is not the cap
- intermittent: the identical configuration ran all-green twenty minutes
  earlier

Most likely the abrupt-driver-death family already in
`~/git/gilescope/rebuck-nits.md`: the driver's teardown is a SIGTERM with no
QUIC close, so a worker mid-operation gets a broken connection rather than a
clean end, and whether that surfaces as `Ok(())` or an error depends on what
it was doing at the time. The 30-second teardown measured earlier is the
happy path of the same race.

Not chasing it now. It costs nothing but a red tick, the results are intact,
and the fix belongs with the worker's exit handling rather than in the
middle of a measurement sequence. Recorded because "the run went red" is
about to stop meaning "the experiment failed", and that is worth knowing
before the next one.

### A reference band, not a reference point

Two `-balance -sandbox` runs on `+test-no-qemu`:

| run | fleet leg | solves | routed | home |
| ---- | --------- | ------ | ------ | ---- |
| first | **520s** | 150 | 127 | 20 |
| second | **517s** | 184 | 147 | 35 |

Three seconds apart, on a target whose solve count moved by 34 and whose
home builds moved by 15 between them. The leg is reproducible to 0.6% while
its own workload varies by a fifth - which says the leg is bound by
something other than how many solves there are, consistent with `building`
at 69-73% and a fixed serial fraction behind the host binds.

Every comparison earlier in this document rests on a single reference and a
shrug about pool variance. This one has two runs agreeing to within noise,
so **anything outside roughly 505-535s is a real change** and does not need
a repeat to believe.

Worth having by accident: the second reference is the run that reported
`completed/failure` because one worker exited non-zero. Its coordinator was
green and its numbers are sound, which is exactly what the red-run signature
two sections up exists to let a reader conclude.

## `-bcast`: halves `waiting`, moves the clock by nothing

Confirmed fired, from the worker logs rather than the wire report - the
counter lives in the worker's process:

```text
251 x "8 of 8"   80 x "5 of 5"   71 x "2 of 2"   66 x "4 of 4"   35 x "44 of 44"
```

N equals M everywhere: every worker took every announced blob, which is
exactly what broadcast means and what the split never did.

| | `-balance -sandbox` | `+ -bcast` |
| ---------- | ------------------- | ---------- |
| fleet leg | 517s, 520s | **517s** |
| `waiting` | 443s (27%), 453s (31%) | **245s (19%)** |
| `building` | 1,196s, 1,021s | 1,017s |
| occupancy | 3.35, 3.00 | 2.61 |

**`waiting` fell by 45% and the clock did not move at all.**

That is the mechanism working and not mattering, and the phase split is what
makes the difference legible. Pre-positioning layers on every worker instead
of one does remove queue time - 200 seconds of it - but this target's leg is
not set by queue time. It is set by 771 seconds of home vertex work behind
the host binds, which no amount of prefetching touches.

Two things follow.

**`-bcast` is a real improvement with no current value.** On a workload whose
serial fraction dominates, shaving the parallel part changes the shape and
not the clock - the same sentence written for `-sandbox` cutting the
critical leads by a third. Two mechanisms now, both working, both invisible
in the wall clock for the same structural reason.

**It would matter on a target without host binds.** `+test-ast` has none,
and its `waiting` was 45% after `-balance`. That is where `-bcast` should
pay - and it is the target I moved it AWAY from three hours ago, on the
argument that `building` was the bigger bucket there. The bucket was bigger;
the bucket was not the constraint. Choosing by bucket size was the right
correction to "choose by which target is cheapest" and still not right.

### The leg is lead time over concurrency, to within a second

`+test-ast` with `-balance`: leg **1050s**, total lead time **8,239s**,
occupancy **7.94**.

```text
8239 / 7.85 = 1049
```

The identity holds exactly, and it is worth more than any single
measurement here because it says what a mechanism has to do to matter.
There are two levers and only two: **cut total lead time, or raise achieved
concurrency.** Anything that does neither cannot move the clock, whatever
else it improves.

That is principle 28 in one line, and it retroactively explains both
mechanisms that worked without paying. `-sandbox` cut the critical leads by
a third on a target whose leg was already set by serial home work -
concurrency could not rise because the work could not leave. `-bcast` cut
queueing 45% on the same target - lead time fell, and the leg did not,
because that leg was not lead-bound.

It also sharpens the prediction for `-bcast` on `+test-ast`, which is
lead-bound: if `waiting` falls 45% again, 3,670s becomes ~2,020s, total lead
time becomes ~6,589s, and at unchanged concurrency the leg lands near
**840s**. Two ways to be wrong, both named in advance - concurrency drops
because a single prefetch lane serialises six workers, or `waiting` does not
fall because a 2.6-second lead cannot wait for a prefetch that has not
arrived.

And one number that frames the whole exercise: `+test-ast`'s graph ceiling
is 2.97x at seven machines, so its best possible leg against a 212s baseline
is about **71 seconds**. At 1050s the fleet is fifteen times off the ceiling
its own workload permits. Whatever binds here is not Amdahl, which makes it
the honest place to keep measuring.

### The 12.5x nobody has looked at

Following the identity through on `+test-ast` with `-balance`:

| | |
| ---------------------- | ---------------------------------- |
| baseline, whole build | **212s** |
| fleet `building` total | **4,521s** = 21.3x the whole baseline |
| built duplication | 1.7x |
| **unexplained** | **12.5x** |

The fleet spends twenty-one times the entire single-machine build just
*building*, and duplication accounts for 1.7 of it. The remaining **12.5x is
not the scheduler, not queueing, and not rebuilt ops** - it is the same work
costing twelve times more per unit when a worker does it than when the
baseline daemon does it.

Per lead that is 10.9 seconds of building for what the median worker-side
line reports as 2.5 seconds, so the mean is dragged by long leads, and those
long leads are where the 12.5x lives.

Nothing measured today addresses this. Prefetch cut what a lead FETCHES;
`-balance` cut what it WAITS for; neither touches what it costs to build
once started. The candidates, in the order they should be eliminated:

- **cold caches.** A worker's buildkit starts with nothing in its local
  cache for this graph, so work the baseline gets from cache is recomputed.
  `dup=1.7` counts ops rebuilt across WORKERS; it does not count ops the
  baseline never rebuilt at all.
- **cold cache mounts.** 359 of 414 leads name one, `mount arms cold p50` is
  9,783ms, and the seeded arm has never had a single sample.
- **unpack.** Every lead materialises a parent image; the registry serves it
  in milliseconds and buildkit unpacks it, and nothing has ever timed the
  second half.

This is the largest unexplained number in the document and it has been
derivable since the phase split landed. It is what "the fleet does 9x the
work" (principle 21) was gesturing at without a denominator.

### How to measure the 12.5x, without guessing which candidate it is

The three candidates - cold buildkit cache, cold cache mounts, unpack - are
not separable from anything currently printed. `took Xms ... in Wms` gives
serve time and total, so `took - served` is build-plus-unpack together, and
no line divides it.

But there is a discriminator that needs no new theory: **time the same op
digest on both sides.**

The baseline leg runs every op on the coordinator's daemon and the status
tap already records those timings by digest. A worker running a dispatched
lead runs some of the SAME ops - the digest is `sha256:<hex of op bytes>`,
confirmed in `vertex.go:337`, and identical bytes on either machine produce
an identical key. So:

- **worker time approximately equals baseline time** for the same digest -
  then the 12.5x is not per-op cost at all, it is ops the fleet runs that
  the baseline never ran, and the answer is in what gets dispatched rather
  than how it executes.
- **worker time greatly exceeds baseline time** for the same digest - then
  it IS environmental, and the split between cache and unpack is the next
  question rather than the first one.

What it needs: the worker taps its own buildkit status the way the proxy
does - the same `note_vertices`, twenty lines - and prints per-digest
timings at exit. Then one run has both halves and the comparison is a join
on the digest.

Deliberately not building it now, with a run in flight. Recorded because the
design is the part that was unclear, and because this document already
contains five mechanisms built before anyone asked which candidate they
addressed.

## `-bcast` on `+test-ast`: 353s, and the guard says it is not a win

The leg came in at **353 seconds against a 1050s reference** - a third of
the time, and far past the 840s the identity predicted. It is not a result.

```text
17 failure  did the fleet change the answer
```

The parity check failed, and the log says why:

```text
Error: async force execution for ./internal/earthfile/tests+base:
  unlazy force execution: failed to load cache key:
  unexpected media type application/octet-stream for sha256:37b8752f...: not found
```

The baseline touched five targets, the fleet four. **The leg was fast
because it stopped early**, which is precisely the failure the
`fleet touched N target(s)` counter and the parity gate were built to catch,
and precisely the number I would have reported as a 3x win if either had
been missing.

### What broke

`unexpected media type application/octet-stream` on a blob fetch. Broadcast
makes every worker fetch every announced blob rather than its 1-in-N share,
so a blob that one worker used to fetch is now fetched by six - and
something in that path serves the wrong content type for the same digest.

Two candidates, and the evidence does not yet separate them:

- **the mesh fetch path**, which serves blobs by hash with no manifest
  context and may be handing back a raw blob where a client expects a
  descriptor's media type
- **a race**: six concurrent fetchers for one digest through the
  single-flight gate, where the loser reads a partially-written store entry

The second is the one broadcast newly exposes, since the split guaranteed at
most one fetcher per blob per worker set.

**Both refuted, within the hour.** The blob path returning
`application/octet-stream` is correct OCI - blobs are octet-stream and
buildkit reads config blobs served that way routinely - and the manifest
path sniffs its content type properly with `media_type_of`. And the store
writes through a tmp file and renames, by design and with a comment saying
why, so a reader cannot see a partially-written blob however many writers
race.

So the cause is **undetermined**. What is known: broadcast changes only
which worker fetches which blob, the failure is a media-type mismatch on a
specific digest during cache-key resolution, and it did not occur in three
runs of the same target with the split. Recorded as open rather than
diagnosed, because two plausible mechanisms have now been checked and
neither survived - and a fix built on a third guess would be the sixth
mechanism in this file built before anyone asked which candidate it
addressed.

**`-bcast` goes off.** It cut `waiting` 45% on the wide target for no clock,
and on the narrow one it breaks the build. The mechanism is sound in
principle - pre-positioning is principle 18 - and the implementation is not
safe to run until the media-type failure is understood.

Recorded as a correctness failure rather than a performance one, because
that is what it is: the parity gate is the only reason this is not written
up as the best result of the day.

### The 12.5x join is blocked: the baseline does not go through the proxy

```yaml
EARTHLY_BUILDKIT_HOST="tcp://${LAN}:${BASE_PORT}" earthly $EARTHLY_FLAGS "$TARGET"
```

`BASE_PORT`, not the proxy's. The baseline leg talks **directly to a
buildkit daemon**, which is correct - a baseline that went through the proxy
would be measuring the proxy - and it means the coordinator's status tap
never sees a single baseline vertex.

So `home_vertices` holds only what the FLEET leg built at home. The worker
half of the join, built an hour ago and working, has nothing to join
against: I have per-digest times from workers and per-digest times from the
coordinator's refused solves, and both are the same leg.

Three ways forward, none free:

- **Tap the baseline daemon separately.** Clean, and needs the build's solve
  ref, which `Control.Status` requires and the baseline never surfaces
  because nothing is proxying it.
- **Run the baseline through the proxy with dispatch off.** Then the tap
  sees everything - and the baseline stops being a clean single-machine
  number, which is the one measurement in this whole rig that has stayed
  stable across nine hours and eight runs.
- **Compare worker vertices against the coordinator's HOME vertices from the
  same leg.** Free, and compares different populations: home vertices are
  the refused ops, which are refused precisely because they are unlike the
  dispatched ones.

Recorded as blocked rather than worked around. The worker half is committed
and costs nothing; the 12.5x stays the largest open number in this document,
and the honest statement is that measuring it needs a baseline-side
instrument nobody has built.

### A discriminator that needs no baseline

The join wanted worker-time against baseline-time for the same digest. There
is a cheaper split available from the worker alone, and it addresses the
same question.

The worker already reports, per lead, `took Xms (Y ops, Z KiB fetched in
Wms)`. It now also reports, once, `vertices: N ran in Mms`. So:

| quantity | meaning |
| -------- | ------- |
| sum of `took` | everything a lead cost on this worker |
| `M` (vertex time) | what buildkit spent actually executing ops |
| the difference | solve setup, pull, unpack, export - everything else |

**If vertex time is most of lead time**, the 12.5x is genuine building: the
fleet runs ops the baseline did not, and the answer is in what gets
dispatched. **If vertex time is a small fraction**, the difference is
materialisation overhead paid per lead, and unpack is the first suspect -
the registry serves a parent in milliseconds and nothing has ever timed what
buildkit does with it afterwards.

That is the same fork the baseline join would have resolved, from one side
only, with no new instrument and no change to the baseline. It needs one run
of the shipped configuration - which doubles as a third reference point for
`+test-ast` at 1050s.

Worth writing down that this was available before the join was designed. The
join is a better instrument and it is blocked; this one is worse and works.

### Third hypothesis refuted, and a real cost broadcast does have

The worker's `by_hash` reads its store or fetches from a peer, and nothing
else - no in-memory buffer, no partial serve. A peer that turns out not to
have the blob returns an error and the walk continues. So the third
candidate for the `-bcast` media-type failure goes the way of the first two,
and the cause remains unknown after three source reads.

The read did surface a real property of broadcast, unrelated to the failure:

```rust
peers.iter().filter(|(id, b)| **id != self.my_id && b.contains(hash))
```

Candidates are chosen by **bloom membership**. Under the split, a blob lives
on one worker and one bloom contains it, so the walk is short and usually
right. Under broadcast every worker holds every blob, **every bloom contains
everything**, and the candidate list becomes the whole fleet for every hash.

That does not break anything - the local check comes first, so a worker that
holds the blob never walks at all - but it means the peer-walk fallback goes
from "ask the one machine that has it" to "ask everyone, in order". Any
future fan-out mechanism inherits that, and the mitigation is the ordering
already there for the driver: claimants first, then everyone else.

Recorded because it is the kind of thing that shows up later as an
unexplained slowdown in a mechanism nobody connected to bloom saturation.

### The local rig cannot reproduce the `-bcast` failure, and cannot clear it

`scripts/fleet.sh`, three daemons, six builds, `REBUCK2_PREFETCH_ALL=1`:
six solves, six routed, zero failed, no media-type error.

**That is not evidence.** The log contains no `prefetched` line at all -
no announcement was ever made, so broadcast never fired. The rig's builds
are synthetic and share no ops, so `consumers_of(op) >= 2` is never true and
prefetch has nothing it considers worth announcing.

Which is the same gap that made the rig useless for `-balance` earlier
today: warmth requires shared work, and this generator produces none. The
rig is excellent for anything one daemon can show - it settled the
re-fetch question in a minute - and structurally blind to anything about
sharing.

So the media-type failure needs either a CI run or a generator that emits
overlapping graphs. The second is the better investment and is not a
five-minute job: it means synthesising a dependency graph with genuine
common ancestors rather than N independent chains.

Recorded because a clean local run is exactly the kind of result that talks
you into re-enabling something. The mechanism did not fire; nothing was
tested; `-bcast` stays off.

## `consumers_of` counts dissemination, not demand

```rust
async fn consumers_of(self: &Arc<Self>, op: &str) -> usize {
    let pairs = self.op_by_worker.lock().await;
    pairs.iter().filter(|(o, _)| o == op).map(|(_, w)| *w)
        .collect::<BTreeSet<u64>>().len()
}
```

`op_by_worker` is written when a subtree is **placed**. So this counts *how
many workers have already been sent the op*, and `prefetch_image_for` gates
on it being at least two:

- an op sent to one worker scores 1, and is not prefetched
- an op scores 2 only once **two workers already hold it**

At which point pre-positioning it is pointless. **The gate opens exactly
when the mechanism has nothing left to do.** It is not a threshold on
demand, it is a threshold on how far the content has already spread.

This explains two things that looked unrelated.

**The local rig announces nothing.** Six independent builds, no op ever
reaches a second worker, `consumers_of` never returns 2. Not because the rig
lacks sharing in some deep sense - because the counter cannot see sharing
that has not happened yet.

**Prefetch still worked after the bare-digest fix**, firing 421 times. Those
are the other call path: `prefetch_image_for(&image_ref, None)`, where
`None` means "the caller already knows this is shared" - mirrored base
images and cut prefixes. That path bypasses the gate entirely, and it is the
only reason prefetch does anything at all.

**Measured, not inferred.** Across four runs with logs:

| run | count-path fired | refused | shared-path |
| ---- | ---------------- | ------- | ----------- |
| art3 | **0** | 8 | 153 |
| art5 | **0** | 6 | 135 |
| pf | **0** | 0 | 2 |
| bal | **0** | 0 | 2 |

**Zero acceptances, fourteen refusals, 292 prefetches that bypassed it
entirely.** The gate has never once said yes. Its only observable effect is
refusing fourteen prefetches that might have been worth making, and
`prefetching <img>: N consumers` has never appeared in any log.

So the counting path has been dead weight since it was written, and its
comment says the opposite:

> ONE consumer is not shared content. Pushing it spends bandwidth to make
> five machines hold what none will read.

True of demand. This does not measure demand. The honest fix is to count
what a graph NAMES rather than where it has been - `imported_images` already
extracts exactly that, for affinity - but that is a mechanism change and it
goes behind a flag, after a measurement, like everything else here.

## The worker vertex tap costs 15%, and its number is confounded

Parity passed, so this is a legitimate leg:

| | `-balance` | `+ worker vertex tap` |
| ---------- | ---------- | --------------------- |
| fleet leg | 1050s | **1207s** |
| lead time | 8,239s | **9,383s** |
| occupancy | 7.94 | 7.86 |
| `waiting` | 3,670s | 4,357s |
| `building` | 4,521s | 5,026s |

**No mechanism changed.** The only addition was the tap. Lead time rose 14%,
the leg rose 15%, occupancy held - the identity again, and this time it is
measuring my own overhead.

The cause is where I put the call. `vertex_times` runs inside
`build_subtree`, and `build_ms` is measured around the whole of that - so
the tap's cost lands inside the number it exists to explain. Principle 16,
in code I wrote two hours after writing the entry about it, having checked
the PROXY tap for exactly this and never asked the same question of the
worker one. The proxy tap reports `1ms across 3,321 frames`; the worker tap
has no self-measurement at all.

### And the number it produces cannot answer the question

```text
[worker] vertices : 593 ran in 4964419ms, 125 cache hit(s)
```

**4,964 seconds of vertex time on one worker, in a 1,207-second leg.**
Buildkit runs vertices concurrently, so per-vertex wall clock sums to
several times the elapsed time. The discriminator assumed vertex time and
lead time were comparable quantities; they are not, and no ratio between
them means what I said it would.

So the split I designed to price the 12.5x measures nothing, and costs 15%
to collect. It goes behind a flag, off, with both facts recorded at the
call site.

The 12.5x remains open. What it needs is a per-vertex comparison against the
SAME digests on the baseline - the join that is blocked because the baseline
bypasses the proxy - and no amount of one-sided instrumentation substitutes
for it.

## `+all` runs through the fleet: 2,724s, zero failed targets

The last unrun target in earthbuild's Earthfile. `-balance -sandbox
-nobase`, six machines, 45 minutes, and every check green:

```text
16 success  the Earthfile, through the fleet
20 success  a fleet that distributed nothing is not a pass
fleet failed: 0 target(s)

[wire] verdict : target=+all solves=119 routed=95 home=24 peak_solves=10
                 occupancy=0.99 ceiling=4.03 ceiling_7m=2.81 dup=1.9
[wire] lead phases : placing 3s (0%) waiting 391s (16%) building 2103s (84%)
[wire] home vertices : 591 ran in 24947158ms, 293 cache hit(s)
[wire] trapped ops : 7 refused solve(s), 1810 ops, 133 past the last exclusion
```

**Zero failed targets**, 95 of 119 solves routed, and step 20 - the guard
against a green run that distributed nothing - passed on its own terms.

With that, **every named target in this Earthfile has been through the
fleet**: `+lint-all`, `+all-binaries`, `+all-buildkitd`, `+test-ast`,
`+test-no-qemu` and now `+all`. That is the coverage half of the brief,
finished.

### What the numbers say about it

**`occupancy 0.99`.** One solve in flight at a time, on average, across six
machines. The release chain is a chain: `+all-buildkitd` then `+all-binaries`
then `+earthly-docker` then `+prerelease`, each mostly waiting on the last.
`ceiling_7m 2.81` is what the graph would permit and 0.99 is what it used,
so this target is serial in a way `+test-no-qemu` is not - fourteen
independent groups against four dependent stages.

**`building` is 84%**, the highest of any target measured. `waiting` is 16%
and `placing` is three seconds. There is nothing here for a scheduler to
win: the work is serial, and what parallelism exists is already taken.

**Seven refused solves against 1,810 ops** - far fewer exclusions than
`+test-no-qemu`'s 35, because the release chain builds images rather than
running tests in them. The host-bind ceiling barely applies here.

**24,947 seconds of home vertex time** in a 2,724-second leg. Nine times the
elapsed, so buildkit is running roughly nine vertices at once on the
coordinator - which is where a serial chain's parallelism actually lives,
inside each stage rather than between them.

So `+all` is the target that most wants a fleet by size and least by shape.
It ran, it produced the same artifacts, and no amount of distribution will
make a four-link chain shorter than its longest link.

## Where this stands, with the Earthfile exhausted

Every named target has been through the fleet. The coverage question has no
larger part left to ask, so what remains is entirely about speed - and the
plain statement is that **the fleet is slower than one machine on every
target measured.**

| target | one machine | six machines | why |
| ------ | ----------- | ------------ | --- |
| `+test-ast` | 212s | 1050s | 12.5x unexplained per-unit build cost |
| `+test-no-qemu` | 186s | 517s | host binds cap it at 2.32x, and it is at 2.8x slower |
| `+all` | not measured | 2,724s | a four-stage chain; occupancy 0.99 |

Three different reasons, and only one of them is a scheduling problem.

**`+all` is shape.** Four dependent stages. No scheduler shortens a chain,
and the run proves the machinery handles it rather than that it should.

**`+test-no-qemu` is the Earthfile.** `WITH DOCKER` binds a host, seventeen
solves cannot leave, and 2.32x is arithmetic. Raising it means changing what
earthbuild's targets do, not what this dispatcher does.

**`+test-ast` is ours.** Its graph permits 2.97x at seven machines - a 71s
leg - and it takes 1050s. Fifteen times off, and 12.5x of it is the same
work costing more per unit on a worker than on the baseline daemon. That is
the only number left that a change to this codebase could move, and it is
the one still unexplained.

Today moved that target 1773s -> 1050s by repairing two mechanisms that had
never worked. Neither touched the 12.5x. Everything that did get measured
against it - seeding, compression, the op floor, imports, broadcast - either
missed the binding term or made things worse.

So the honest next step is not another mechanism. It is the baseline-side
instrument that makes the 12.5x attributable, and it is blocked on one
design decision: the baseline deliberately bypasses the proxy, which is
correct and which is also why nothing can see inside it.

## The 12.5x join is unblocked: status replays from build history

The blocker was that `Control.Status` needs a solve ref, and the baseline
never surfaces one because nothing proxies it. Reading buildkit rather than
working around it:

```go
// solver/llbsolver/solver.go:439
func (s *Solver) Status(ctx context.Context, id string, ...) error {
    if err := s.history.Status(ctx, id, statusChan); err != nil {
```

**Status is served from the build HISTORY**, not only from a live job. A
finished build's vertices can be replayed by ref, after the fact.

And the ref is discoverable. `BuildHistoryRecord` carries it:

```proto
message BuildHistoryRecord {
    string Ref = 1;
    ...
    google.protobuf.Timestamp CompletedAt = 7;
```

`ListenBuildHistory` streams those records, and the proxy already relays
that call - so the plumbing exists on both sides.

So the design is: after the baseline leg, a separate observer asks the
baseline daemon for its build history, takes the most recent record's ref,
and replays `Status(ref)` through the same `note_vertices` everything else
uses. **The baseline is not touched at all** - it keeps talking directly to
its daemon, keeps being a clean single-machine number, and gets read
afterwards rather than instrumented during.

That is the join, and it needs perhaps forty lines: a `watch-vertices`
subcommand and one workflow step after the baseline.

Worth noting what unblocked it. Three turns ago I wrote "needs a solve ref
the baseline never surfaces" and listed three unpalatable options. All three
were workarounds for a constraint that does not exist - and the fork has
been checked out three directories away the whole time.

## The join run failed parity, and the new suspect is my own history read

```text
17 success  the Earthfile, through the fleet     (636s, against 1050s)
18 failure  did the fleet change the answer
Error: async force execution for ./tests/integration-base+test-base:
  unlazy force execution: failed to copy: httpReadSeeker: failed open:
  could not fetch content descriptor sha256:485a2ed9f85270d9aaf6
```

**636 seconds is not a result.** The leg was fast because it stopped early -
the same shape as `-bcast`, caught by the same gate, and the second time
today a suspiciously good number has turned out to be a broken build.

### What is new in this run

Two things, and only one of them is exonerated.

**The worker vertex tap is not the cause.** Run 31541993907 carried it
ungated, passed parity, and cost 15%. Same code, same effect, no failure.

**The baseline history read is new.** `watch-vertices` opens
`ListenBuildHistory` and then a `Status` stream per record against the
BASELINE daemon, immediately before the fleet leg. It is meant to be a pure
read. Buildkit's history subsystem holds references to build results, and
whether enumerating it disturbs content lifetime is not something I know -
`could not fetch content descriptor` is exactly what a prematurely released
blob looks like.

That is a suspicion with a mechanism, not a diagnosis. What makes it
actionable is that it is cheap to separate: **the history step runs on any
run with a baseline leg**, so the next plain `-ast-balance` run tests it
without `-vtx` in the way.

### The immediate consequence

Every future run with a baseline now carries this step. If it is harmful, it
is harmful to the reference band itself - the 1050s number that six
comparisons today rest on. So it goes behind the same flag as the worker
half rather than running by default, and the join becomes a two-flag
experiment instead of a free rider on every baseline.

Recorded before changing anything, because "my instrument broke the build"
has happened twice today and both times the fix was to make it optional
rather than to argue it was safe.

## The checkpoint: the apparatus still works

Two runs in a row had failed parity - `-bcast` at 353s and the join run at
636s. Both were fast, and both were fast because they stopped early. Before
attributing either failure to the thing it was testing, the question worth a
half-hour of CI was the boring one: **does HEAD still produce a working
build?** Six hundred and forty commits is a lot of apparatus to have never
re-baselined.

Run `31550028841`, `giles-dispatch-ci-ast-balance`, plain shipped
configuration - no `-vtx`, so neither the worker vertex tap nor the baseline
history read. Step 15 reported `skipped`, which is the confirmation that
mattered: the two-job flag fix holds and the run genuinely excludes both
suspects.

| | |
| ---------------------- | ----------------------------------------- |
| baseline | 201s (band 204-227s) |
| fleet leg | 1113s (reference 1050s, +6%) |
| parity | green |
| solves | 412, all routed, 0 home |
| occupancy | 8.19, peak 11 |
| lead time | 9,033,118ms over 414 leads |

**Parity passes.** So the two failures belong to the things that were gated
off, and the 1050s reference band is intact - 1113s is 6% above it, which is
within the spread these legs have shown all week.

The leg identity holds again without adjustment: 9033s of lead work at 8.19
concurrency predicts 1103s against a measured 1113s, 1% out.

### What the run nearly taught me, wrongly

The verdict line also carried `mounts_ms=8527063`, next to
`lead_ms=9033118`. Ninety-four per cent. Cache-mount arming eating almost the
whole fleet, the 12.5x explained in a single field, and the seeding mechanism
that would fix it sitting at `n=0`.

It is not true. `mounts_ms` counted the WHOLE duration of every lead that
NAMED a cache mount - a filter, not a measurement of mounts. With 359 of 414
leads touching a cache, 94% was close to arithmetic necessity. The arms line
two rows down said `cold p50 5677ms`, and a 23.7s mean against a 5.7s median
was the tell.

Renamed to `lead_ms_with_cache`. Written up as shape 19 in
`docs/how-this-lies.md`, because the defect was not the counter - whose own
doc comment was accurate - but the name, and a name is what gets pasted into
a table.

The genuine residue is smaller and still worth having: **the 55 leads with no
cache mount average 9.2s; the 359 with one average 23.7s.** That is a 2.6x
difference between two populations of lead, and it is a real place to look
next. It is not 94% of anything.

## Why `seeds=off` in every run: the test that never ran

`seeds=off` and `mount arms : seeded p50 0ms (n=0)` have appeared in every
fleet run this project has recorded. The verdict line was rewritten
specifically so it could say `off` rather than `0/0`, because three seeding
attempts had produced no seeding for three different mechanical reasons and
each time the question "did it even run" cost a log dig.

There is a fourth reason, and it is not in the fleet workflow at all.

`selftest.yml` has a job called `harvest` whose second-to-last step,
`can a cold cache mount be seeded at all`, is a complete end-to-end test of
the mechanism: write a marker into cache A, harvest A, seed a COLD cache B
from that harvest, read the marker back out of B. Different ids on purpose,
so it cannot pass by meeting A's own warm mount. Thirty seconds, against the
twenty-five-minute fleet run that is the only other way to ask.

**It has never run.** Not once.

The step before it, `harvest an empty cache mount`, creates a buildkit
daemon, never builds anything against a cache mount, and then asks
`harvest-cache` for `smoke-cache`:

```text
[harvest] 172.17.0.1:28372 reports NO cache mounts at all
[harvest] no cache id "smoke-cache" on this daemon - skipping.
Error: harvest-cache: every pair failed
```

The tool is right and the test is wrong - it asks a fresh daemon for a cache
that cannot exist. With `set -e`, that failure takes the rest of the job with
it. Selftest has 0 green runs in its last 100, 65 of them outright failures,
and the reason has been the same every time.

So the picture is not "seeding was tried and did not pay". It is **seeding
has never been exercised anywhere** - gated off in the fleet workflow behind
a `-seed` branch suffix nobody has used, and unreachable in the selftest
behind a step that cannot pass.

The dead step's only real assertion - that a harvest yields a pullable
reference rather than a bare digest - is already a unit test
(`a_bare_digest_becomes_something_a_daemon_can_pull`). Deleted rather than
repaired.

### Why this matters more than it looks

From the checkpoint run: 359 of 414 leads name a cache mount, and those leads
average **23.7s against 9.2s** for the 55 that do not. A 2.6x difference
between two populations of lead, on the target that is furthest from its own
ceiling.

That is not proof that mounts cause the gap - cache-touching leads are also
the substantial ones, and the causation could run entirely the other way.
But it is the largest structural split visible in the data, and the mechanism
aimed at it has never been switched on.

**Order of work:** let the selftest answer "does seeding work at all" in
thirty seconds. Only if it does is a `-seed` fleet run worth half an hour.

## Seeding works. Measured, twice, before spending a fleet run

The selftest fix let `check-seeding` run for the first time. It passed on
x86 Linux CI, and the same rig passed locally on arm64. Write a marker into
cache A, harvest A, seed a cold cache B from that harvest, read the marker
back out of B - different ids, so it cannot pass by meeting A's own warm
mount.

Local rig, `scripts/seed-check.sh`, both fills:

| phase | 0 MiB | 200 MiB | who pays |
| --------- | ----- | ------- | ------------------------- |
| write | 334ms | 872ms | once, setup only |
| harvest | 369ms | 8921ms | ONCE, on one machine |
| seed+read | 248ms | **794ms** | EVERY worker, every mount |

The asymmetry is the finding. Harvest scales with the cache - 8.9s for 200
MiB, and the fleet's daemon has been observed holding 1.71 GB under these
ids, so a full harvest is plausibly a minute or more. But it is paid once,
serially, before the leg. **Seeding is 794ms for 200 MiB** and is what every
worker pays.

### The prediction, written before the run

From the checkpoint: `cold p50 5677ms` across 359 mount arms, 9,033s of lead
time, occupancy 8.19, a 1113s leg.

If a seeded arm lands near 1s where a cold arm takes 5.7s, that is ~4.7s off
each of 359 leads - about 1,690s of lead time, 19% of the total. At 8.19
concurrency that is roughly **200s off the leg: 1113s -> ~910s**.

Three ways this fails, all worth naming now so the result cannot be
retrofitted:

- **The harvest is serial and uncounted.** A 1.7 GB harvest at the local
  rate is ~75s added to the coordinator before the leg starts. If the leg
  only drops 200s, a third of the win is eaten at the front.
- **`cold p50 5677ms` may not be arming.** It is the same family of metric
  as `mounts_ms`, which lied this evening. If it is really "time in leads
  that armed a mount", the 4.7s saving is imaginary.
- **A seeded mount reads 0.0 MiB in the harvest table** - its size is the
  copy-on-write diff over the seed, already documented in `harvest-cache`.
  So the obvious "did it work" check reports the same thing as total
  failure, and the arms line is the only honest read.

The measurement to trust is `mount arms : seeded p50 ... (n=N)` with **both
arms non-empty**. Every run so far has printed `n=0` on the seeded side.

## `-bcast`: narrowing by elimination, not a fourth hypothesis

Three candidate mechanisms for `unexpected media type application/octet-stream`
have been refuted. Rather than propose a fourth, the error string was traced
to its source. It is containerd's, in
`vendor/github.com/containerd/containerd/v2/core/images/image.go:242`:

```go
return nil, fmt.Errorf("unexpected media type %v for %v: %w",
    desc.MediaType, desc.Digest, errdefs.ErrNotFound)
```

Reached only after the descriptor failed both `IsManifestType` and
`IsIndexType`. So the failure is not a fetch going wrong: **buildkit already
held a descriptor typed `application/octet-stream` and expected a manifest
or an index.** The question is only where that descriptor was minted.

Two families can now be eliminated outright:

- **Our manifest endpoint cannot produce it.** Every `/v2/*/manifests/*`
  GET and HEAD goes through `media_type_of`, whose worst case is
  `oci.image.manifest.v1+json`. The three `application/octet-stream`
  literals in `registry.rs` are all on `/blobs/`, where octet-stream is
  correct OCI.
- **We author no manifests.** Outside tests, nothing in `src/` constructs
  manifest JSON; `solve.rs:574` only reads `layers` out of one. Every
  manifest in the mesh was written by buildkit's own exporter.

That leaves the descriptor arriving through buildkit's own metadata - a
lazy ref remembering a descriptor from when it was created - which is
consistent with the error appearing during `unlazy force execution` and not
during a pull.

Still open, and deliberately not guessed at. But the search space is two
families smaller, and the next person does not have to re-refute the
registry.

## The seeding chicken-and-egg, stated as a constraint

Reading the code while the first `-seed` run was in flight, the ordering is
the problem and it is structural rather than a bug.

A harvest can only read the cache directory earthly actually wrote if it
presents the same mount INPUT. From `harvest_graph_with`:

> `getRefCacheDir` keys on the input's ref, so a digest that differs by a
> platform field reads an empty directory and reports a cold cache.

Reconstructing that input has been measured wrong. The correct inputs come
from observing real graphs, which the proxy does and writes to
`REBUCK2_CACHE_INPUTS_FILE`. The proxy's own comment says the rest:

> The proxy sees graphs during the fleet leg, and the harvest currently runs
> BEFORE it - so this file is for the NEXT run, carried by the bank.

So on a run whose bank carries no inputs file:

1. harvest runs, has no observed inputs, reconstructs them,
2. reconstruction reads a directory nothing wrote,
3. the harvest is empty and the fleet is seeded with nothing,
4. the leg then runs and finally records the correct inputs - for next time.

The baseline cannot break the cycle either. It was deliberately moved onto
its own daemon (`base-bk`) so both legs start equally cold, which also means
it does not pass through the proxy, so its graphs are never observed.

### What the first run's timings already say

Step 16 - docker pull, tag and push of busybox, a `buildctl du -v`, a full
`check-seeding` round trip, and four harvests - completed in **8 seconds**.
Locally, harvesting a single 200 MiB cache takes 8.9s on its own.

That is not a cheap harvest. It is consistent with four harvests that found
nothing, which is branch 1-3 above. The log will say which; recorded here
because the prediction was written before the run and the arithmetic was
available before the evidence.

### Three ways out, none taken yet

- **Harvest after the leg, seed on the next run.** Honest, needs two runs to
  show anything, and is the warm-CI case rather than a simulation of it.
- **Observe the baseline.** Requires it to pass through something that
  records graphs without dispatching them.
- **Read the inputs out of the baseline daemon's history.** The vertices are
  there; `watch-vertices` already reads them. This is the only option that
  costs one run, and it shares machinery with the `-vtx` step currently
  gated off under suspicion.

### Why you cannot just read the cache key off the daemon

The obvious escape from the ordering problem: the daemon knows which
directory it wrote, so ask it. `buildctl du -v` names every cache mount, the
harvest already parses those names, and if the name carried the full key the
harvest could mount it directly and need no observed input at all.

It does not. From `solver/llbsolver/mounts/mount.go` in the earthbuild fork:

```go
name := fmt.Sprintf("cached mount %s from %s", m.Dest, mm.managerName)
if id != m.Dest {
    name += fmt.Sprintf(" with id %q", id)      // <- id, not key
}
...
key := id
if ref != nil { key += ":" + ref.ID() }         // <- what lookup uses
...
sis, err := SearchCacheDir(ctx, g.cm, key, false)
```

The description is built from **`id`**; the persisted record is found by
**`key`**. They differ by `":" + ref.ID()`, and `ref.ID()` is
`identity.NewID()` - random at record creation, not derived from content,
and not exposed by any API the harvest can reach.

So `du -v` can tell you a mount called `go-mod` exists and how big it is,
and cannot tell you which directory it is. The only way to name that
directory is to present the same mount INPUT and let buildkit hand back the
same cached ref, with the same random id, from its own records.

Which is what the existing design does, and why it needs inputs observed
from a real graph. The ordering problem is therefore not an implementation
shortcut to be routed around - it is forced.

That leaves exactly two routes: carry inputs between runs in the bank
(already built, and until this evening defeated by a truncating write), or
observe the baseline by putting something in front of its daemon that
records graphs without dispatching them.

### The clobber, reproduced and fixed locally

Staged the exact fleet scenario against the local rig: a carried inputs file
holding one real `go-mod` entry, as the bank restores it, then
`check-seeding` run against it - which is the order the fleet's harvest step
uses.

```text
[check] wrote 1 observed input(s) to .../cache-inputs.tsv, keeping 1 already there
go-mod               /cache  AAECAw==
seedcheck-src-29540  /cache  IiUSIwj///////////8BEP...
```

Both lines present. Before the fix the file held only the second, and
`harvest-cache` - looking for `go-mod` a few seconds later - found nothing
and fell back to the reconstruction.

So the bank route is intact after all. It was built, it carried the right
bytes, and a diagnostic that ran three seconds before the consumer deleted
them. Nothing about the mechanism was wrong.

**Expected sequence from here:** the run in flight writes real inputs into
the bank at the end of its leg. The next `-seed` run restores them, merges
rather than clobbers, and is the first run in this project's history whose
harvest can present the input earthly actually sent.

### The seed ids match where the time is

Before spending another run on seeding, the question worth asking is whether
the four configured `seed_ids` are the ids this target actually spends its
seconds behind. From the checkpoint run's cache-cost table (rows overlap - a
lead counts under every id it names):

| lead ms | leads | id |
| --------- | ----- | --------------- |
| 8,370,361 | 358 | `go-mod` |
| 8,288,021 | 353 | `go-build` |
| 3,153,880 | 97 | `//go/pkg/mod` |
| 3,153,880 | 97 | `//root/.cache` |

`go-mod` and `go-build` cover 358 and 353 of the 359 cache-touching leads,
and both are in the default `seed_ids`. So a working harvest reaches
essentially every lead that has a mount at all - the mechanism is aimed at
the right thing.

Two details worth writing down:

- **The bare-path ids carry a DOUBLE leading slash** - `//go/pkg/mod`, not
  `/go/pkg/mod`. `resolve_cache_id` survives it by matching on the tail
  (`h.ends_with("/go/pkg/mod")`), which is luck rather than design; an exact
  comparison would have missed both rows. The configured
  `/root/.cache/golangci_lint` does NOT match `//root/.cache` and will be
  skipped, which is correct - it is a different directory.
- **The two 97-lead rows are identical to the millisecond** because they are
  the same 97 leads counted twice, once under each id they name. That is
  what the header warns about, and it is the kind of row that gets summed
  into a headline by accident.

So the remaining risk in the seeding prediction is not aim. It is whether
the harvest can present the right input - which is the bank question, now
that the truncating write is gone.

## Where the 9,033 seconds go, and why it points at mounts

The checkpoint run's phase split, which had never been read against the
amplification figure:

```text
[wire] lead phases : placing 0s (0%) waiting 4115s (46%) building 4917s (54%)
```

| quantity | value |
| ------------------------------- | ------------------ |
| lead time total | 9,033s |
| of which placing | 0s (0%) |
| of which waiting (worker busy) | 4,115s (46%) |
| of which building | 4,917s (54%) |
| baseline, whole target | 201s |
| leg | 1,113s |
| workers | 6 |

Three things follow, and the third is the useful one.

**Placing is zero, but not for the reason it looks.** `placing_ms` is
`offered_ms`, which starts at 0 and is stamped ONLY in the decline path
(`driver.rs:2202`), on every re-offer, so what survives is the last one. A
lead accepted by its first candidate keeps zero forever. So the field
measures **re-offer round trips after a decline**, not the cost of
deciding, and 0s across 414 leads means *no lead was ever declined* - a
different and narrower statement. The conclusion "placement is not the
problem" survives; the reason had to be corrected. See the correction under
the `-seed` run below.

**Waiting is not waste, and it is NOT an argument for more machines.**
4,115s of leads sitting behind other leads on a busy worker. The obvious
reading - 46% queueing, therefore buy machines - is wrong, and worth killing
before it costs a run.

Building alone is 4,917s. Spread perfectly over six workers that is 820s,
against a 201s baseline. To get build time under the baseline on
per-unit costs like today's you would need **25 machines**, and Amdahl caps
the whole thing at 5.67x however many are present. More machines make the
fleet faster than the fleet; they cannot make it faster than one machine
while each unit of work costs fourteen times what it costs there.

So `-w12` stays cancelled, now for a second and better reason than the
ceiling arithmetic that cancelled it the first time.

**The amplification lives in `building`** - but the size of it was
overstated, twice, and the arithmetic is worth doing properly.

`lead_ms` and its `building` share are a SUM OVER LEADS THAT RUN
CONCURRENTLY, both across the six workers and within each one. The whole
leg identity in this file depends on that (`lead_ms / occupancy = leg`, 9033
/ 8.19 = 1103s against a measured 1113s). So dividing 4,917 lead-seconds by
a 201-second wall clock is not a work ratio, and the 24.5x it produces is
not a quantity.

Converting to worker-wall-seconds at the measured occupancy:

| quantity | value |
| ------------------------------ | ------- |
| building, summed over leads | 4,917s |
| leads in flight per worker | 1.36 |
| building as worker-wall-seconds | 3,602s |
| per worker, in a 1,113s leg | 600s = **54% busy** |
| against a 201s baseline wall | **17.9x** |

Two corrections fall out. The utilisation figure I gave as 74% is **54%** -
I divided lead-seconds by machine-seconds without dividing out the overlap.
And the amplification is 17.9x rather than 24.5x, which after `dup=1.8`
leaves **~10x** rather than 13.6x.

Even 17.9x is not yet apples to apples: the baseline's 201s is one wall
clock over work buildkit parallelises internally across the runner's cores,
while a dispatched subtree is small and parallelises less. A defensible
comparison needs both sides in the same units - wall or CPU - and this file
does not currently have the baseline's CPU time. So **the honest figure is
"between about 6x and 18x, and the instrument to narrow it does not exist
yet"**, which is a good deal less than the number this section opened with.

### Why this points at cache mounts

A cold cache mount is precisely a mechanism that makes identical work cost
more on a worker: the baseline fills `/go/pkg/mod` once and reuses it across
every target, while each worker starts empty and runs `go mod download`
again. It inflates `building` and nothing else - not placing, not transfer,
not the queue.

And the population split is consistent: leads with a cache mount average
23.7s, leads without average 9.2s.

This is a converging argument, not a proof. Cache-touching leads are also
the substantial ones, so the 2.6x could run the other way, and 13.6x is a
lot to hang on one mechanism. But every phase that is NOT building has now
been eliminated by measurement, and the one remaining candidate is the one
the seeding work already targets.

**If seeding works and `building` does not move, the 13.6x is something
else and this whole line of attack is wrong.** That is the falsifiable form,
and the arms line plus the phase split will answer it in one run.

### The second candidate, and why it is not the answer

13.6x is a lot to hang on cold cache mounts alone, so the other structural
difference deserves naming: **the baseline exports nothing, and the fleet
exports everything.**

`solve_request` attaches `publish_attrs(...)` to every dispatched subtree, so
each of the 414 leads serialises its result, compresses it, and pushes it to
the mesh registry - inside the same `solve()` call the worker times as
`build_ms`. So `building` is really build + export + compress + push. One
machine building the same target does none of it.

That is real, and it is structural rather than a bug: it is the price of
making a result travel, and principle 10 already says the trees have to
travel.

**But it cannot be the dominant term.** A fleet run has been observed
serving ~25.6 GiB. Taking that as the traffic:

| assumption | cost | across 6 workers |
| ---------------------------- | ------ | ---------------- |
| transfer at ~100 MiB/s | ~260s | ~43s each |
| compression at ~50 MiB/s/core | ~520s | ~87s each |

Against 4,917s of building, export accounts for something on the order of
5-10%, not 1,360%. The assumptions are coarse - deliberately, per principle
13 - but they are wrong by a factor of two, not by two orders of magnitude,
and that is all this needs to decide.

So export is named, bounded, and set aside. The per-unit cost is still
unexplained, and cold cache mounts remain the only candidate on the table
that is the right SIZE.

## The first `-seed` run: every prediction confirmed, and a 60% tax measured

Run `31552464169`, `+test-ast`, balanced, seeding on. It is a negative
result and a good one: it confirms all five mechanical reasons in one pass
and prices the failure mode.

**The daemon had the caches.** 1.6 GB of them:

```text
[harvest]  586.3 MiB  cached mount /root/.cache ... with id "//root/.cache"
[harvest]  554.9 MiB  cached mount /root/.cache/go-build ... with id "go-build"
[harvest]  456.1 MiB  cached mount /go/pkg/mod ... with id "go-mod"
```

**And read none of them.**

```text
[harvest] 1 observed cache-mount input(s)
[harvest] go-mod: RECONSTRUCTING earthly's input, which has been measured wrong
[harvest] WARNING: go-mod harvested under 64 KiB
REBUCK2_CACHE_SEEDS=go-mod=...@sha256:001f3dff...
```

`1 observed input` is the clobber, caught. The proxy's own line from the
same run says `cache inputs : 2 written`, and the bank restored them - so
the file held two real entries until `check-seeding` truncated it to its own
probe, three seconds before `harvest-cache` read it.

`go-mod` and `//go/pkg/mod` harvested to the **same digest**, `001f3dff...`,
which is what two empty directories look like.

All three empty harvests were emitted as seeds anyway, and the fleet applied
them to 360 mounts.

### What an empty seed costs

| | checkpoint | `-seed` | change |
| ------------- | ---------- | -------- | ------ |
| leg | 1,113s | **1,783s** | +60% |
| lead time | 9,033s | 12,254s | +36% |
| placing | 0s (0%) | **1,872s (15%)** | from nothing |
| waiting | 4,115s | 3,727s | -9% |
| building | 4,917s | 6,653s | +35% |
| routed / home | 412 / 0 | 393 / 19 | |
| dup | 1.8 | 1.5 | |
| cut_prefix | 2 | 113 | |

**`placing` is the surprise, and my first reading of it was wrong.** I wrote
that the 1,872s was the cost of rewriting every graph. It is not.
`placing_ms` is only ever non-zero for a lead that was **declined and
re-offered** - `offered_ms` is initialised to 0 and stamped nowhere but the
decline path.

So the real statement is sharper: **seeding made workers refuse leads.**
1,872 seconds of decline-and-re-offer round trips, where the checkpoint had
none at all. The same run corroborates it from two other directions -
`home` went 0 -> 19 and `routed` 412 -> 393, which is exactly what refusals
look like on the other side of the ledger.

Why a seeded lead gets refused is now the open question, and it is a better
one than "graph rewriting is slow". `cut_prefix` jumping 2 -> 113 is a
separate consequence: rewritten graphs have different shapes, so the prefix
finder sees many more distinct roots.

**This is shape 6 in `how-this-lies.md`** - a plausible stand-in for the
quantity you want. `placing` reads like "time spent placing" and is
"time spent re-placing". It has been in the phase-split line since it was
built, and both of tonight's readings of it were wrong until the definition
was checked.

So an empty seed is not neutral. It costs **+670s of wall clock**, split
between rewriting graphs that gain nothing and pulling images that contain
nothing.

### Both fixes were already in before the numbers landed

`merge_cache_inputs` stops the clobber; `worth_seeding` stops an empty
harvest becoming a seed. Either alone prevents this run. They were written
from the code and the 8-second harvest timing, before the leg finished -
which is the only reason the next run could be fired immediately.

### The arms line still compares nothing

```text
[wire] mount arms : seeded p50 4448ms (n=289), cold p50 0ms (n=0)
```

Everything was seeded, so there is no cold arm - the mirror image of every
previous run, where nothing was seeded and there was no seeded arm. **The
within-run comparison needs a SUBSET seeded**, which is what the next run
gets for free: the bank carries inputs for only some ids, and the empty ones
are now skipped rather than emitted.

## The declines were build failures, and the seed address is why

Chasing "why does a seeded lead get refused" through run `31552464169`'s
worker logs. The refusal is not `Saturated` and not `Undispatchable` - the
three reasons `consider()` can give. It is not a refusal at all:

```text
worker declined subtree job N: build failed: solve: failed to load cache key:
  failed to copy: httpReadSeeker: failed open: could not fetch content
  descriptor sha256:4642b241... (application/vnd.docker.container.image.v1+json)
  from remote: not found
```

**The worker took the lead, tried to build it, and could not fetch an image
config blob.** 274 of these.

### The address means something different on every machine

The harvest publishes seeds with `--registry 172.17.0.1:15000` and emits
references like `172.17.0.1:15000/rebuck2/subtree@sha256:...`. On the
coordinator that is correct - `172.17.0.1` is the docker bridge, and the
coordinator's registry is on the other side of it.

But every worker runs **its own** registry at exactly the same address:

```yaml
--registry-bind 0.0.0.0:15000 \
--registry-addr 172.17.0.1:15000
```

So the seed reference is shipped verbatim to six machines where the same
string names a *different registry* - the worker's own, which has never held
the coordinator's harvest. The fetch 404s, the build fails, the lead is
declined and re-offered, and `placing` fills up.

This is the same class as the busybox note already in the workflow ("Pushed
to 127.0.0.1, pulled from 172.17.0.1. Same registry, two addresses"), except
here the two addresses are on two different HOSTS and nothing said so.

### What this corrects

The `-seed` run's +670s is **not an empty-seed tax**. The seeds were empty,
but that is not what cost the time: the workers could not fetch them at all.
Rewriting the graph, failing the build, declining, and re-offering is the
expense, and it would be paid identically by a seed full of real bytes.

### The prediction for the run in flight

`worth_seeding` stops an EMPTY harvest becoming a seed, which would have
spared this run. It does nothing about the address. So:

- if run B's bank yields real inputs and the harvest produces real seeds,
  **it should fail the same way**, because a real seed carries the same
  unreachable reference;
- if the bank yields too little and every harvest is empty, no seeds are
  emitted at all and the leg should look like the checkpoint's 1,113s.

Written before it lands. Either outcome is informative, and neither is the
seeding measurement - that needs the address fixed first.

## Run B: the harvest finally read something

`31554856118`, same target and branch, first run with `merge_cache_inputs`.

| step | run A | run B |
| ----------------- | ----- | --------- |
| baseline | 212s | 216s |
| harvest (step 16) | **8s** | **65s** |

Run A's eight seconds covered a docker pull, a `du -v`, a full
`check-seeding` round trip and four harvests - which is only possible if
every harvest read an empty directory. Run B spends 57 seconds more on the
same steps, and the local rig puts a 200 MiB harvest at 8.9s, so ~65s is the
right order for the 1.6 GB the daemon holds.

**So the clobber was the whole of it.** The bank had been carrying the right
bytes; `check-seeding` truncated the file three seconds before
`harvest-cache` read it, and merging instead of overwriting was sufficient
to make a harvest that had never worked start working.

Whether the resulting seeds can be USED is a separate question, and this run
predates the broadcast fix, so the prediction filed earlier still stands:
real seeds carry the same delivery problem as empty ones, and 1-in-N
splitting of a blob every worker needs immediately is what produced 274
`could not fetch content descriptor` failures. Large seeds should make that
worse, not better.

### Arguing against my own hypothesis, before the measurement

"Cold cache mounts are the only candidate of the right size" is probably
wrong, and the reasoning is available without waiting for the `du -v`.

A cache mount is keyed `id + ":" + ref.ID()` and persists on the daemon that
made it. The baseline proves reuse works: after building the whole target it
holds **six** mounts, not one per exec.

On an UNSEEDED run - which is where the 13.6x was measured - each lead
carries earthly's own mkdir as the mount input, identical across leads. Same
key, same directory. A worker taking ~69 of the 414 leads fills `go-mod`
once and reuses it for the other 68.

So cold mounts should cost about **one fill per worker**, six fills across
the fleet. The baseline fills all 1.6 GB somewhere inside its whole 201s, so
six fills is a few hundred seconds at the very most - against **4,716s** of
excess building to explain. Off by an order of magnitude.

The mechanism that WOULD explain it is a per-lead key: if every lead got a
fresh empty directory, the refill is paid 359 times rather than 6.

**And nothing produces one.** I wrote here that seeding introduces it,
because `seed_cache_mounts` rewrites the mount input by design. Reading that
function settles it the other way: the input it substitutes is a `Source` op
built purely from the seed's image reference, which is the same string for
every lead in the run. Same bytes, same digest, same ref, same key. The
first lead on a worker materialises the mount from the seed and every later
lead reuses it - which is the behaviour you would want, and the opposite of
what I claimed.

So there is no per-lead key in either arm, seeded or not, and the `du -v`
should show a handful of mounts per worker whichever way run C goes.

**So the honest state is: I do not know what the 13.6x is.** Export is
bounded at 5-10%. Duplication is measured at 1.8x. Cold mounts look an order
of magnitude too small once reuse is accounted for. The `du -v` will settle
the last of those - a handful of mounts per worker confirms the reuse and
kills the hypothesis; hundreds resurrects it.

Written now because the measurement is already queued and it would be easy,
afterwards, to remember having expected whichever answer arrived.

## Run B: the harvest works, the delivery does not, and `-bcast` is explained

`31554856118`. Both earlier fixes confirmed by their own log lines.

```text
[harvest] 3 observed cache-mount input(s)
[harvest] go-mod: using the input observed from a real graph
[harvest] NOT seeding //go/pkg/mod: 491 bytes is under the 65536 byte floor
```

`using the input observed from a real graph` has never appeared before -
every previous harvest printed `RECONSTRUCTING earthly's input, which has
been measured wrong`. And `worth_seeding` fired on exactly the case it was
written for: one id whose reconstruction produced 491 bytes, skipped instead
of shipped.

Two real seeds, harvested in 28s and 33s.

| | checkpoint | A (empty seeds) | B (real seeds) |
| ------- | ---------- | --------------- | -------------- |
| leg | 1,113s | 1,783s | **1,699s** |
| placing | 0s | 1,872s | 995s |
| building | 4,917s | 6,653s | 7,035s |
| home | 0 | 19 | 9 |
| seeds | off | 3/3, empty | **2/2, real** |

And the arms line has both arms for the first time:

```text
[wire] mount arms : seeded p50 5017ms (n=294), cold p50 74511ms (n=1)
```

One cold sample is not a comparison, so this still does not price seeding.
What it does say is that a seeded arm is 5.0s where the checkpoint's cold
arms were 5.7s - a 12% difference, against a mechanism that is costing 53%
overall. Seeding is not yet paying for its delivery.

### The declines name the next two bugs

192 of them, in three groups:

| count | reason |
| ----- | ------------------------------------------------------- |
| 125 | `could not fetch content descriptor ... not found` |
| 60 | `.../rebuck2/subtree@sha256:...: not found` |
| 4 | `unexpected media type application/octet-stream for ...` |

The 60 are their own defect. `manifest_blobs` returns config and layers -
everything the manifest POINTS AT, never the manifest itself - so a
pre-positioned image left every worker needing the one blob a puller reads
first, fetched at lead time. Fixed by `manifest_dig`.

**The 4 are the `-bcast` failure.** That error has been open since `-bcast`
was measured, with three hypotheses refuted and two families eliminated by
tracing it to containerd's `images.Manifest()`. It appears here in a run
with no broadcast at all, alongside 185 other fetch failures, which settles
it: it was never a `-bcast` mechanism. It is the same unfetchable-content
family, and broadcast only changed how often it was reached.

### What run C carries

Five fixes, all of them "make seeding able to work at all" rather than
competing mechanisms:

| fix | what it addresses |
| ------------------- | ------------------------------------------------ |
| `merge_cache_inputs` | the clobber - confirmed fixed in B |
| `worth_seeding` | empty harvests shipped as seeds - confirmed in B |
| seed broadcast | a blob every machine needs, split 1-in-N |
| two-phase peer bounds | 5s around dial AND transfer |
| `manifest_dig` | the 60 |

Plus two instruments: each worker's ending cache-mount count, which decides
whether cold mounts can be the amplification at all, and CPU on both sides,
which is the missing half of every amplification figure in this file.

### What run C should show, written before it does

Five fixes and two instruments. Each has a distinct observable, so a partial
result is still readable:

| observable | fix it tests | failure reads as |
| ------------------------------------ | ---------------- | ------------------------- |
| `prefetched N/N` on all six workers | seed broadcast | `N < M` on five of six |
| no `[registry] MISS` lines | all delivery | a digest, and who was asked |
| declines near zero | delivery | the three-way breakdown again |
| `mount arms` with both arms non-tiny | the whole chain | one arm at `n<=1` |
| `cache mounts: <n>` per worker | nothing - it is a QUESTION | see below |
| `baseline cpu` / `worker cpu` | nothing - also a question | |

The last two are not tests of anything. They settle arguments this file has
had with itself:

- **a handful of cache mounts per worker** means mounts are reused across
  leads, cold mounts cost ~6 fills, and they cannot be the amplification;
  **hundreds** means every lead got a fresh directory and they can be.
- **CPU on both sides** is the first like-for-like amplification number this
  project will have had. Every previous one divided a sum over concurrent
  leads by a single wall clock, which is shape 22.

And the honest possibility: seeding may still not pay. A seeded arm was
5.0s against a cold 5.7s in run B - a 12% saving per arm. If delivery
becomes free and that is all seeding is worth, the mechanism is a wash and
the finding is that **the cache mounts were never the expensive part**,
which sends the whole investigation back to `building` with one fewer
candidate.

### The vertex join is superseded, not forgotten

Two instruments were built to price the amplification. The **join** - the
baseline daemon's per-vertex times against each worker's, matched on the
vertex digest - cost 15% when its worker half was switched on, confounded
the run it measured, and never produced a number. The **CPU counters** cost
nothing, are read once at teardown, and answer the headline question
directly.

So the join is not the priority any more, and this is written down so it is
not resurrected out of momentum. What it would still add is per-vertex
detail: *which* vertices cost more on a worker, not just that they do. That
is worth having once there is a figure to explain, and not before.

`REBUCK2_WORKER_VERTICES` and the `-vtx` baseline history step stay gated
off. Both were suspects in two parity failures, and the checkpoint that
cleared HEAD ran with them off.

### What the timeout split changes, including the part that is worse

Splitting `PEER_BLOB_TIMEOUT` into a handshake bound and a bulk bound fixes
the case it was written for - a 456 MiB seed can now cross the mesh - but it
also introduces a bound where the worker previously had none, and that is
worth stating rather than discovering.

**Before:** the worker's `fetch_by_hash_from` waited forever. A slow peer
always eventually delivered; a dead one hung the worker's registry, and
therefore its daemon's blob GET, until something further up gave up.

**After:** a peer that cannot ANSWER within five seconds is abandoned and
the walk moves on - to the next peer, then to the driver. So under heavy
contention, where a busy worker might genuinely take longer than five
seconds to reply to a blob request, more traffic lands on the coordinator
than before.

That is the right direction to fail: the driver is the backstop and holds
everything the mirror has, so the fetch still succeeds, just less
peer-to-peer than intended. It is a load shift, not a failure mode. But if a
future run shows the coordinator serving more than it used to, this is the
first thing to check, and `hits_local / hits_peer / hits_driver` on each
worker is the counter that says so.

The bulk bound has no such trade-off: it only ever permits transfers that
were previously cut off.

## Run C: the first like-for-like amplification, and cold mounts are dead

`31557310760`. Parity green. Leg 1,625s against a 221s baseline.

### Cold cache mounts are not the amplification

Each worker's daemon, at teardown:

```text
cache mounts: 2  4  2  2  0  2
```

A handful, exactly as predicted before the run. **Mounts are reused across
leads** - a worker fills `go-mod` once and keeps it for its other sixty-odd
leads - so cold mounts cost about one fill per worker, not one per lead. The
hypothesis that carried this investigation for a week is dead, and it was
killed by a single `du -v` that took ten lines of YAML.

The one worker at `0` took no cache-touching lead at all, which is its own
small finding about spread.

### The amplification, in CPU, at last

| | CPU | wall | implied |
| ------------------ | ------- | ---- | ------------------- |
| baseline (`base-bk`) | **604s** | 221s | 2.73x internal parallelism |
| six workers, summed | **3,900s** | - | 551 1316 160 821 591 461 |

**3,900 / 604 = 6.5x**, and after `dup=1.4`, roughly **4.6x unexplained.**

Every previous figure in this file was larger: 24.5x, then 17.9x, then
13.6x. All of them divided a sum over concurrent leads by a single wall
clock. The measured 2.73x internal parallelism is most of why: the baseline
was never doing 221 seconds of work, it was doing 604 in 221.

Two caveats, both making 6.5x an UNDER-estimate of the fleet's cost:

- the coordinator's own daemon is not counted, and it built 91 solves at
  home in this run;
- `dup=1.4` is ops SENT to more than one worker, which is an upper bound on
  ops rebuilt.

So the honest headline is **the fleet spends somewhere around 6-7x the CPU
one machine spends on the same target**, which is a real and serious cost -
and a quarter of what this file claimed yesterday.

### The load spread nobody had measured

Worker CPU ran from **160s to 1,316s: an 8.2x spread** across six machines
given the same fleet and the same target. `-balance` was built to even out
lead COUNTS; it does not even out work. That is a new finding and probably
the cheapest remaining win, because the busiest worker sets the leg.

### The identity refused to be trusted, correctly

`scripts/leg-arithmetic.sh` printed:

```text
identity check : lead/occupancy = 1017s against a measured 1625s leg  MISMATCH
```

Which is right: `home=91` this run, and work built at home is not a lead, so
`lead_ms` no longer accounts for the leg. The script written two hours ago
to stop me dividing badly caught the first case where the division would
have been wrong for a completely different reason.

### Seeding, still not paying

| | checkpoint | A | B | **C** |
| --------- | ---------- | ----- | ----- | --------- |
| leg | 1,113s | 1,783s | 1,699s | **1,625s** |
| lead time | 9,033s | 12,254s | 12,979s | **8,058s** |
| building | 4,917s | 6,653s | 7,035s | **4,763s** |
| routed/home | 412/0 | 393/19 | 403/9 | **321/91** |

Lead time and building are both now BELOW the unseeded checkpoint - the
delivery fixes worked, and the fleet is doing less work than it was. But 91
solves fell back to home, and the leg is still 46% worse than not seeding at
all.

`mount arms: seeded p50 6037ms (n=213), cold p50 8040ms (n=1)` - one cold
sample again, so the per-arm comparison still has nothing to say.

### Broadcast works, and one blob in three does not arrive

Run C's worker logs settle what the seed announcement actually did.

The driver announced **3 blobs** per seed - two blobs plus the manifest, so
`manifest_dig` is working - and twelve worker lines report a share of three,
which is exactly two seeds times six workers. **That is the broadcast
signature**: under the split, six workers divide three blobs and most get
zero or one.

But of those twelve:

```text
  2 prefetched 3/3 of my share (3 announced)
 10 prefetched 2/3 of my share (3 announced)
```

**Ten of twelve fetched two of their three blobs.** The prefetch reached
every worker, every worker tried to take the whole seed, and one blob in
three did not arrive - after which 733 leads failed on content this prefetch
was supposed to have placed. That is up from 192 in run B, which fits: more
workers now attempt the fetch, so more of them can fail it.

**And nothing said which blob, or why.** The loop was

```rust
if exec::Blobs::get(&*blobs, &d).await.is_ok() { got += 1; }
```

with the comment "Failures are dropped - a blob that does not arrive now
arrives lazily later", which was true when a prefetch was advisory and is
not true for a seed: a seed that does not arrive fails the build that needed
it.

The `[registry] MISS` line added earlier tonight cannot cover this either -
prefetch takes the mesh path directly and never enters the HTTP handler,
which is why `MISS: 0` appeared in every log while 733 leads were failing.
Two diagnostics, one blind spot between them, and the blind spot is exactly
where the mechanism lives.

Now named: `[worker] prefetch MISS <hash> (<size> bytes): <error>`. The next
run says which blob and what the mesh answered, which is the one fact this
investigation has never had.

### `-balance` evens the counts, and the counts are not the work

Run C, per worker:

| | spread |
| ----------- | ------------------------------- |
| leads taken | 122 to 239 = **1.96x** |
| CPU spent | 160s to 1,316s = **8.2x** |

(The two lists cannot be paired - the CPU figures come out in job order and
the lead counts by worker number - so this compares DISTRIBUTIONS, not
per-worker ratios. The spreads stand without pairing.)

`-balance` was built to stop affinity piling leads onto one machine, and by
its own measure it works: a factor of two across six workers is a reasonable
spread for a scheduler that cannot see the future. But **the work inside a
lead varies four times more than the count of leads does**, so evening the
counts leaves an 8x imbalance in the thing that matters.

The busiest worker sets the leg. On this run one machine spent 1,316 CPU-
seconds and another 160, which means five machines were waiting on one for
much of the leg - and `waiting 1542s (19%)` is that, seen from the other
side.

This is the cheapest remaining structural win, and it needs no new
mechanism: `offer_score` already exists and already takes a queue-depth
term. What it lacks is any notion of how big a lead is. `analyse` computes
the op count of every cut before the offer goes out, and the driver already
carries `lead_ms` per worker from completed leads - either would be a better
weight than a count.

Worth naming the trap before anyone acts on it: op count is a measure of
SIZE, and `-minops` already proved size is not cost - a 20-op dispatch floor
made the fleet five times slower while improving every per-lead number.
Observed `lead_ms` per worker is the honest signal, and it is already
recorded.

#### The wiring, specified but not done

The shape is settled and the code is NOT in the tree, deliberately.

`work_ahead(done_ms, mean_ms)` turns "this worker is ahead of the fleet on
work" into virtual queue slots, so the brake in `offer_score` carries it and
no second weight has to be tuned:

- brake the busy only. A worker below the mean is not rewarded here, because
  warmth already prefers it, and paying twice for one fact is how `-imports`
  cost 425s.
- `mean_ms == 0` is NO SIGNAL, not a penalty: before any lead completes
  nothing is known and nothing should be reordered.
- bounded, around six slots. A worker that has done twice the fleet's work
  may also be the only one holding the ops for the next subtree, and
  excluding it outright trades a known transfer for an unknown queue.

It was written, tested, and then **removed again** rather than committed
unwired. This file records "third mechanism on this branch built and left
unconnected" as a recurring failure, and an eight-line function behind an
`#[allow(dead_code)]` is that failure with a tidier hat on. The thinking is
here, where it does not rot; the code is two minutes' work when there is a
run to measure it with.

What remains is plumbing, and it is deliberately not done at the same time
as a seeding measurement:

1. `offer_order_warm` takes a second closure `work: &dyn Fn(u64) -> usize`,
   mirroring the `warm` closure it already has. The simple caller passes
   `&|_| 0`.
2. The driver accumulates completed `ms` per worker - it already has the
   value at `driver.rs:929`, where `all_lead_ms` is incremented - and
   supplies the closure as `work_ahead(done[id], mean)`.
3. Its own switch, default off, counted as `balance_work`, because
   `-balance` is already on in every recent run and a change that rides
   inside it cannot be attributed.

The placement path is where `-imports` and `-minops` each cost a run, so it
gets its own experiment with nothing else moving.

### The leading hypothesis for the missing blob, filed before run D

Run C, worker 1, at teardown:

```text
[worker] 14 blobs over 1MiB: 590 MiB distinct, 3981 MiB served (7x re-served)
[worker] served 4005 MiB in 104435ms (38.3 MB/s from this registry)
[worker] of that, 0 MiB went to a client on this box and 4005 MiB left it
```

Broadcast is doing exactly what it was asked to. 590 MiB of distinct content
left this machine seven times over - every other worker fetching the same
seed blobs from whoever had them first - and none of it was for a local
client. Six workers at that rate is roughly **24 GiB across the mesh** in
one leg.

**And that is the likely cause of the missing third blob.** A worker
saturated serving four gigabytes cannot necessarily answer a new blob
request within `PEER_BLOB_TIMEOUT`, which is five seconds. The walk then
abandons it, tries the next peer - also busy - and falls through to the
driver, which is serving everyone at once.

This is not a new discovery so much as a bill coming due. When the handshake
bound was split from the bulk bound earlier tonight, the entry written then
said:

> under heavy contention, where a busy worker might genuinely take longer
> than five seconds to reply to a blob request, more traffic lands on the
> coordinator than before... if a future run shows the coordinator serving
> more than it used to, this is the first thing to check.

Broadcast is what created the contention that makes the bound bite, and the
two changes shipped in the same run.

**If run D's `prefetch MISS` lines say "did not answer for ... in time",
that is this**, and the fix is not a longer timeout - it is not asking six
machines to fetch the same 456 MiB simultaneously. The split existed for
this reason; the mistake was treating "wanted by everyone" as "must be
fetched by everyone at once" rather than "must reach everyone before it is
needed", which a staggered or chained distribution also satisfies.

If instead they say `not found`, the bytes were never reachable and the
contention is a red herring.

### A comment that describes a mechanism the code skips

`share_of`, on the broadcast path, says:

> Splitting is still right for the herd it was written against, and
> `seeder_for` spreads the SOURCE per blob either way, so a broadcast pulls
> each blob from a different peer rather than stampeding one.

The code underneath it:

```rust
pub fn share_of(hashes: &[String], ids: &[String], me: &str, broadcast: bool) -> Vec<String> {
    if broadcast || ids.len() <= 1 {
        return hashes.to_vec();          // <- seeder_for is never reached
    }
    hashes.iter().filter(|h| seeder_for(h, ids).as_deref() == Some(me)) ...
}
```

Broadcast returns before `seeder_for` is consulted. So the claim is
aspirational: it describes what the SPLIT path does, on the branch that does
not take it. Under broadcast every worker asks for every blob at once, no
peer holds any of them yet, and all six go to the driver together - which is
precisely the stampede the sentence says cannot happen.

This is shape 13 with the polarity reversed. Usually a mechanism is wired,
documented and inert; here the documentation describes a mechanism the code
deliberately skips, and the sentence is reassuring enough that nobody
re-reads the three lines above it. I wrote the broadcast path tonight and
read that comment as support for it.

**The fix, if run D confirms the contention, is an ordering rather than a
policy.** In broadcast mode a worker should fetch its `seeder_for` share
FIRST and the remainder after: each blob then has exactly one machine
pulling it from the driver, and the other five find it on that peer via the
bloom moments later. Same total set, one source per blob, no stampede - the
thing the comment already promised.

### The 1,113s reference is now stale, and the next comparison must not use it

Every seeded run tonight has been measured against the checkpoint's 1,113s
unseeded leg. That reference predates two changes which are **not** gated by
`-seed`:

| change | affects |
| ----------------------- | ---------------------------------------------- |
| `manifest_dig` | EVERY prefetch, not just seeds - base images and cut prefixes now announce their manifest too |
| the peer timeout split | every peer blob fetch on both driver and worker |

Both are believed to be improvements and neither has been measured on its
own. The seeder-first ordering is genuinely seed-only (it lives on the
broadcast branch, and only seeds broadcast), and the rest of the night's
work is diagnostics.

So `1,113s` is a number from a different binary. Comparing the next seeded
leg to it would attribute the difference to seeding when part of it belongs
to a prefetch that now carries one more blob per image and a fetch that no
longer abandons large transfers.

**Before any further seeding conclusion: fire a plain `-ast-balance` run on
current HEAD and re-establish the unseeded band.** It costs half an hour and
it is the difference between a measurement and an anecdote. The same
discipline caught `-bcast` riding in with the prefetch fix, and the
checkpoint that cleared HEAD after 640 commits.

Written now because the temptation, with run D landing shortly, is to read
its leg against 1,113s and call it progress or regress. Neither reading
would be sound.

### A third candidate, and it explains the lead failures rather than the misses

Run C, worker 1: **554 prefetch announcements**. `REBUCK2_PREFETCH_LANES` is
not set in the workflow, so `prefetch_permits` defaults to **1**, and the
lane is taken for the whole of an announcement's loop:

```rust
let _lane = prefetch_gate().acquire().await;
for d in mine { ... Blobs::get(&*blobs, &d) ... }
```

So every worker processes 554 announcements strictly one at a time, and a
seed is two of them. That suggested two consequences in opposite directions,
and **checking the logs killed the first one immediately**:

- ~~**the seed waits**, behind however many announcements precede it~~ -
  **no.** The seed announcement was **#2 of 554** on every worker checked.
  `usable_seeds` runs on the first dispatch, so the seed is at the front of
  the queue by construction. It is not late, and the `2/3` is therefore a
  genuine fetch failure rather than a fetch that had not happened yet.
- **the seed blocks.** This half stands. When the seed's loop runs it holds
  the ONE lane while fetching up to 456 MiB, and the other 552 announcements
  wait behind it - so the lane delays every OTHER pre-position in the run,
  including the base images and cut prefixes that leads also need.

That is a real cost and a different one from the seed's own failure. It also
explains a shape nobody had accounted for: prefetch is supposed to remove
transfer from the critical path, and a single lane behind a half-gigabyte
fetch puts most of it back.

Checked before run D landed, with data already on disk. The hypothesis was
worth an hour of CI and cost two minutes of grep.

The gate was written to stop six announcements interleaving into six
concurrent pulls on the coordinator, which was right when announcements were
rare. At 554 per worker it is a queue with one server in front of everything
the fleet needs pre-positioned, and the thing most needed first has no way
to say so.

Not changed tonight: it is a third variable, run D is in flight, and the
diagnostic that distinguishes it from the other two lands in ten minutes.

### 87 of the 91 home builds are ONE blob

`read-run.sh`, once it read artifacts, printed the `not routed` map whole:

```text
"not routed": {"considered": 412,
  "...could not fetch content descriptor sha256:ef2f6c8d...: not found": 87,
  "...sha256:311a81dd...": 1, "...sha256:430ab33d...": 1,
  "...sha256:af33e497...": 1, "...sha256:c4db4dc1...": 1}
```

Run C fell back to home 91 times, and **87 of those are the same digest**.
Four other blobs failed once each.

This is not diffuse contention. It is one object that eighty-seven separate
leads needed and none could obtain, and the four singletons are noise beside
it.

**And it is a CONFIG blob**, of media type
`application/vnd.docker.container.image.v1+json`, which is a couple of
kilobytes. So the size theory does not survive it:
whatever stops this blob arriving, it is not that 456 MiB is hard to move
across a shared network in five seconds.

That reframes the whole delivery investigation. The question is no longer
"why is the mesh slow under broadcast" but **"why is this one small object
unreachable"**, which is a much more tractable thing to ask - and the
`prefetch MISS` line, if that blob was announced, will answer it in run D
with the error the mesh actually returned.

Two candidates worth holding, neither yet tested:

- it was never announced. `manifest_blobs` returns config and layers for a
  reference the driver can resolve; a subtree RESULT reaches the announce
  path only through `prefetch_results`, which is off by default because I
  gated it earlier tonight.
- it was announced and the fetch genuinely fails, in which case run D names
  the error.

Found by running the tool I had just changed, against a run I had already
downloaded and grepped by hand twice. The map was in the step log the whole
time; I had been reading `declined` lines and never the `not routed` field
that aggregates them by cause.

## Run D, and the correction it forces: one run does not measure the amplification

Run `31559656955`. Between it and run C, **nothing functional changed** - the
diff is the `prefetch MISS` line, a shortfall warning, a size-attribution fix
in a diagnostic, and documentation.

| | run C | run D |
| ---------------- | ------ | ------ |
| worker CPU total | 3,900s | 5,155s |
| baseline CPU | 604s | 592s |
| **amplification** | **6.5x** | **8.7x** |
| leg | 1,625s | 1,396s |
| placing | 1,751s | 443s |
| home | 91 | 7 |
| CPU spread | 8.2x | 5.9x |

**The CPU cost rose 34% while the leg fell 14%, with no functional change.**

So the 6.5x I published two hours ago as "the first like-for-like
amplification" is not a measurement, it is a sample. Two samples of the same
binary give 6.5x and 8.7x. The honest statement is **"between about 6x and
9x, on six shared CI runners, with run-to-run variance of a third"**, and
anything narrower needs repetition this file has never done.

That is the same error as the 24.5x, one level up. There the arithmetic was
wrong; here the arithmetic is right and the sample size is one. I corrected a
figure by replacing it with a better-computed figure and did not ask how
stable it was.

**It also undermines the run-to-run story tonight.** A 91-to-7 fall in `home`
and a 1751s-to-443s fall in `placing` looked like the delivery fixes landing.
Nothing functional changed. Either those numbers swing by an order of
magnitude between identical runs, or the failing content differs per run -
and run C's 87-of-91 single blob says the latter is at least partly true.

### What survives

**Cold cache mounts are still eliminated.** C: 2 4 2 2 0 2. D: 2 4 2 4 4 4. A
handful both times, so mounts are reused across a worker's leads in both, and
one fill per worker cannot be a 6-9x amplification. That conclusion rests on
a count that is stable across runs rather than on a duration that is not.

**The load spread is real but its size is not settled.** 8.2x in C, 5.9x in
D. Both large, neither precise.

**325 prefetch misses in run D, and all of them are mine** - the manifest
digests announced without checking the CAS held them, fixed in the commit
above. Run D therefore measures a fleet carrying a regression I introduced,
which is a further reason its numbers are not a baseline for anything.

### What this changes about method

Every comparison in this file between two runs is a comparison of single
samples. Where the effect was large - prefetch 1773s to 1240s, `-balance`
1240s to 1050s - that is probably still safe. Where it was tens of percent,
it may be noise, and the entries claiming those should be read with that in
mind rather than rewritten now on the strength of one more sample.

The cheap remedy for anything that matters from here: run it twice.

### The next two runs, and why they are the same run twice

Fired `giles-dispatch-ci-ast-balance` on current HEAD - plain, unseeded, with
the manifest regression fixed. It does three jobs at once:

1. **re-establishes the unseeded band**, which `1,113s` no longer represents:
   `manifest_dig` and the peer-timeout split both changed the default path.
2. **measures the manifest fix**, since every prefetch in runs C and D was
   announcing digests the coordinator did not hold.
3. **starts the pair.** Run it again, unchanged, and the difference between
   the two IS the noise band - the number every comparison in this file has
   been missing.

The third is the one that matters most and is the cheapest thing this
project has never done. Two identical runs cost an hour and make every
subsequent tens-of-percent claim interpretable; without them, a 14%
improvement and a 34% regression are indistinguishable from what happened
between runs C and D, which was nothing.

Stated as a rule going in, so it is not negotiated afterwards: **a
difference smaller than the spread between two identical runs is not a
finding.** If the pair comes back 1,400s and 1,700s, then `-balance`'s
1240-to-1050 and prefetch's 1773-to-1240 survive comfortably and most of
tonight's seeded comparisons do not.

### Why the cold arm is always `n=1`, and why subset-seeding will not fix it

The within-run arm comparison is the right instrument for a fleet with this
much run-to-run variance: it compares seeded leads against cold ones inside a
single leg, so machine speed, queueing and CI weather cancel. It has reported
`n=1` on the cold side in every seeded run, and the obvious response - seed
only SOME of the cache ids and leave the rest cold - does not work.

A lead is assigned by `any`, not `all`:

```rust
if caches.iter().any(|id| seeds.contains_key(id)) { seeded } else { cold }
```

and the comment above it is correct about why: a lead naming a seeded id
alongside an unseeded one is not a control, so it cannot be counted cold.

Now the arithmetic. On `+test-ast`, of 359 cache-bearing leads, **358 mount
`go-mod` and 353 mount `go-build`**. Seeding either id puts essentially every
lead in the seeded arm. The cold arm can only hold leads that mount SOME
cache and none of the seeded ones, and there is at most one such lead in the
target.

So `n=1` is not a sampling accident to be fixed by seeding a subset. It is
what this target's mount structure permits, and it would be `n=1` however the
ids were chosen, because the two big ones co-occur on almost every lead.

**The instrument is fine and the target is wrong for it.** Making the arms
line say something needs either a target whose cache ids do NOT co-occur, or
per-MOUNT attribution of a lead's duration - and a lead's duration cannot
honestly be split between the mounts it holds.

Recorded so the next person does not spend a run on `REBUCK2_SEED_IDS` with
one id in it, which is the obvious next move and would produce the same
`n=1` for a third reason.

### There is no target where the fleet wins

The coverage ledger read `+lint-all` as "88s vs 252s", which parses as
fleet-then-baseline and makes it the one target the fleet beats. The detail
table further down the same file has it the other way:

```text
| baseline | 92s  | 88s  |
| fleet    | 628s | 252s |
```

Baseline **88s**, fleet **252s**. The row was transposed, and it turned the
fleet's second-worst ratio into its only win.

Caught while about to ask "what is different about `+lint-all` that lets the
fleet win there?" - a line of inquiry entirely built on a number written
backwards, and one that would have taken a while to bottom out because the
premise looked like data.

Consolidating what the targets actually say, on wall clock:

| target | baseline | fleet | ratio |
| ---------------- | -------- | ------ | ----------- |
| `+lint-all` | 88s | 252s | 2.9x slower |
| `+test-ast` | ~210s | ~1400-1800s | 7-8x slower |
| `+all-binaries` | 712s | 262s | **2.7x faster** |
| `+all-buildkitd` | 1188s | 1161s | level |

So there IS a target the fleet wins on, and it is not the one the ledger
claimed: **`+all-binaries`**, five genuinely independent cross-compiles off
one shared stem. That is the shape a build farm is for, and it is the only
entry here with that shape.

The pattern across the four is not about cache mounts or docker. It is
whether the target has independent work of real size. `+all-binaries` has
five minutes-long leaves; `+lint-all` has three cheap ones where per-lead
overhead dominates; `+test-ast` has 412 small solves where it dominates
utterly; `+all-buildkitd` is half undispatchable.

Which sharpens the open question. The per-unit cost of 6-9x is not a tax the
fleet pays everywhere - `+all-binaries` pays it too and still wins, because
its leads are large enough to absorb it. **The fleet is not slow. Its leads
are too small**, and every target where that is false is a target it beats.

### The mechanism that addresses "the leads are too small" exists, and has no callers

If the fleet is not slow but its leads are too small, the fix is to stop
dispatching the small ones. That mechanism is written, documented, tested,
and connected to nothing:

```rust
/// Is this subtree big enough to be worth sending anywhere?
#[allow(dead_code)] // driver line, not the proxy - see lease.rs
pub fn worth_offering(
    est_p90: Option<std::time::Duration>,
    running_for: std::time::Duration,
) -> bool {
    est_p90.unwrap_or(running_for) > STALL
}
```

`grep -rn worth_offering src/` returns its definition and five assertions in
its own test. **No production caller anywhere** - not the proxy, and not the
driver line the `allow(dead_code)` names. That comment is shape 23: it
explains where the function is used, and it is used nowhere.

What makes this the most consequential of the four unconnected mechanisms
found here is that it is aimed exactly at tonight's sharpest finding, and
aimed better than the thing that was tried. `-minops` gated dispatch on op
COUNT, measured five times slower, and this file already records why: op
count is size, and size is not cost. `worth_offering` gates on **the timing
store's p90 for this target**, which is cost, observed, per target, with the
stall as the fallback when there is no history - so no cold-start path is
needed.

The doc comment on the neighbouring `min_ops` puts the case better than I
can:

> `+test-ast` dispatched 412 solves to run a 204-second build and took
> 1773s: the median lead ran 2.6 seconds to do a `jq` and a `diff`.

A 2.6-second lead cannot repay a placement round trip, a graph rewrite, an
export, a push and a pull. Four hundred of them cannot. `+all-binaries` wins
with five leads of minutes each, and it is the same fleet.

**This is the next experiment, and it is a better one than any seeding run.**
Wire `worth_offering` into the gateway's dispatch decision, default off,
measure on `+test-ast`. If the median lead stops travelling and the leg falls
towards the baseline, the fleet's problem was never per-unit cost - it was
that it was paying the toll four hundred times for work worth seconds.

#### And the estimate it needs exists too

I under-sold this a moment ago by assuming `worth_offering` had no source
for its `est_p90`. It has one, built and banked:

| piece | where | state |
| ------------------------- | ---------------------------- | -------- |
| the decision | `dispatch::worth_offering` | written, tested, no callers |
| p90 per target | `bank::timings::Stats.p90_ms` | written, banked between runs |
| observations that fill it | `bank::logstream::samples` | written, parses earthly's log |
| solve to target name | `dispatch::describe_root` | written, already used for `job_names` |

Every part is present. The store is keyed on "target ref plus the build args
that reach it - never the cache key, never the content", and its module doc
argues that choice exactly right for this use:

> a cache key must be exact, or a follower gets someone else's layer; an
> estimate must be STABLE, because being 20% wrong costs a slightly worse
> schedule while having no entry at all costs no schedule. Key an estimate
> on content and it is perfect and useless - every commit empties the table.

So the experiment is a wiring job after all, and the shape of it is:

1. the gateway already calls `describe_root` on every dispatched subtree to
   name it; use that name as the timings `Key`,
2. read `stats(key).p90_ms` out of the banked store, which the coordinator
   already restores as `~/.cache/rebuck2/coord`,
3. gate the offer on `worth_offering(Some(p90), elapsed)`, behind its own
   switch, counted,
4. first run of a target has no entry and dispatches as today, which is what
   `est_p90.unwrap_or(running_for)` already means.

The one real unknown is whether `describe_root`'s name and the timings
`Key`'s target ref are the same string. If they are not, that is the whole
job: a mapping, or a second key.

Recorded at this depth because "wire up worth_offering" is the kind of task
that reads as trivial and turns out to be a missing join - and because the
opposite happened tonight with `manifest_dig`, which read as trivial, was
trivial, and shipped a regression anyway.

#### The unknown resolves: the two names are not the same string

Checked, and the join named in step 1 does not hold.

`describe_root` returns the LLB vertex's `llb.customname` - earthly's
DISPLAY name - while `timings::Key` is built from an earthly TARGET REF plus
build args. Different things, and the difference is already recorded in this
repo as having cost someone six attempts:

> earthly ABBREVIATES the directory in its display names and in the target
> names it puts in OTLP spans, so the traces say
> `./t/integration-base+test-base` and there is no such path. Six warm-ups
> failed identically on "No Earthfile nor build.earth file found" because
> the target was copied out of a log.

So the mapping is not just absent, it is **lossy in the direction that
matters**: display name to target ref cannot be done by string manipulation,
because `./t/` could have been `./tests/` or `./tools/`.

Three ways out, and the choice is a real design decision rather than a
detail:

- **map it anyway**, by listing the Earthfile's targets once at startup and
  matching display names against them. Cheap, and it fails closed - an
  unmatched name simply has no estimate and dispatches as today.
- **key the estimate on the graph** instead of the target: the proxy already
  sees the terminal op digest and could bank observed `lead_ms` against it.
  The timings module argues against exactly this - "key an estimate on
  content and it is perfect and useless, every commit empties the table" -
  but a SUBTREE's digest is stable across commits that do not touch it,
  which is most commits for most subtrees. Worth measuring rather than
  assuming.
- **take the name from the traces**, which the workflow already collects
  with `rebuck2.target` resource attributes - and which carry the same
  abbreviation, so this is the first option wearing a hat.

Recorded because the previous entry called this "the one real unknown" and
it took four minutes to answer. The answer makes the experiment bigger than
it looked, which is exactly what wanted knowing before someone started it at
the end of a long night.

#### Measured: the abbreviation is not lossy on this repo

The entry above says display-name-to-target-ref "cannot be done by string
manipulation, because `./t/` could have been `./tests/` or `./tools/`". That
was reasoning, and it is wrong. Measured against the actual Earthfiles:

| | |
| ------------------------------- | ---------- |
| Earthfiles | 192 |
| targets | 961 |
| target directories | 191 |
| **ambiguous abbreviations** | **0** |

Earthly abbreviates only the FIRST path component - `./tests/integration-base`
becomes `./t/integration-base`, not `./t/i-b` - which the recorded example
shows and my first model did not. Expanding it back is a lookup against the
enumerated directories, and on this repo it is exact.

The top-level directories holding Earthfiles are `buildkitd examples
inputgraph internal release scripts tests util`, and only `i` is shared -
by `inputgraph` and `internal`. Even those do not collide, because no
subpath exists under both. So the fragility is real and one shared
subdirectory away, which a lookup that fails closed handles for free: no
match, no estimate, dispatch as today.

**So the join holds after all**, and `worth_offering` is a wiring job of the
size it first appeared to be. Option 1 is not a compromise, it is exact.

Two models, both guesses, before one measurement settled it - first that all
components are abbreviated (37 ambiguous), then that the first one is (0
ambiguous). The repo was on disk the whole time.

### A missed prefetch is not a failed lead, and the "one blob" was a one-off

Run D's `not routed` map has seven failure buckets, **one occurrence each**,
matching its `home=7`. Run C's had five buckets and one of them accounted
for **87 of 91**. So the single-blob concentration is not a structural
pattern - it happened once, to one object, and did not recur.

Which makes the 91-to-7 fall between the runs mostly the absence of that one
object rather than any fix, and it is another case where two identical
binaries produced numbers an order of magnitude apart.

**The more useful number is the pair.** Run D had:

| | |
| --------------- | --- |
| prefetch misses | 325 |
| lead failures | 7 |

Three hundred and twenty-five blobs that a prefetch announced and failed to
fetch, and seven leads that failed. So **a missed prefetch overwhelmingly
does not fail a lead** - the lazy path picks it up, exactly as the design
intends, and the comment that has been in the prefetch loop all along is
right:

> Failures are dropped - a blob that does not arrive now arrives lazily
> later, which is what happens today.

I quoted that comment earlier tonight as an example of a claim that "was
true when a prefetch was advisory and is not true for a seed". The 325-to-7
ratio says it is still true, seeds included. The comment was right and my
correction of it was wrong.

**So the delivery investigation has been chasing the wrong quantity.**
Prefetch misses are loud, numerous, and mostly harmless; lead failures are
rare and were once dominated by a single unlucky blob. Neither is the 6-9x.

What remains worth fixing from that thread is narrow and already done: the
manifest regression that produced most of those 325 misses, and the seeder-
first ordering that stops six machines asking one for the same bytes. What
does NOT deserve another run is the theory that delivery failure is why the
fleet is slow. It is not, and the arithmetic was available in this run.

### The small leads are half the count and at most 15% of the time

Before "the leads are too small" is allowed to stand as THE hypothesis, the
distribution. Run D, from the workflow's own line:

```text
n=519 min=659ms p50=5738ms p90=87367ms max=302059ms mean=19608ms
```

| | |
| ---------------------------------- | ------------------- |
| total lead time (n x mean) | 10,177s, against a reported 10,058s |
| bottom HALF of leads, upper bound | **1,489s = 15%** |
| top 10% of leads, lower bound | **4,534s = 45%** |

The bound on the bottom half is generous by construction - it assumes every
lead below the median took exactly the median. So **eliminating every
short lead entirely removes at most 15% of lead time**, and less than that
in practice, because the work still has to happen somewhere: keeping it home
saves the toll, not the build.

Meanwhile **the top tenth of leads holds at least 45% of the time**, and
those are leads of 87 to 302 seconds - exactly the size that ought to
dispatch well.

So the honest version of the hypothesis is narrower than the one I put at
the top of the open list twenty minutes ago:

- `worth_offering` is still worth wiring. Fifteen percent is a real number
  and it is nearly free to take, and 260 fewer round trips is 260 fewer
  chances for the delivery faults this file has spent a night on.
- but it **cannot be the 6-9x**. The amplification lives in the big leads,
  which are already the size a build farm wants, and something is making
  those cost several times what the same work costs on one machine.

`+all-binaries` still wins with five large leads, so large leads CAN be
efficient. The question that survives is why `+test-ast`'s large ones are
not - and that is a different question from lead size, asked of a population
that a size filter would not touch.

## The big leads are not big work. A `diff` took 173 seconds

The leads that own run D's critical path, from the workflow's own ranking:

```text
302059ms  job 89   ./internal/earthfile/tests+base depends on +earthly
197246ms  job 90   RUN jq -S . ./actual.json >./actual.pretty.json
172685ms  job 176  RUN diff ./actual.pretty.json ./expected.pretty.json
170874ms  job 125  COPY ./oom-adjust.sh.template /bin/oom-adjust.sh.template
165048ms  job 186  RUN diff ./actual.pretty.json ./expected.pretty.json
```

A `jq` on one JSON file: **197 seconds**. A `diff` of two files: **173
seconds**, twice. Copying one template into `/bin`: **171 seconds**. On one
machine these are milliseconds.

**So the previous entry's framing was wrong in an instructive way.** I split
the leads into "small" and "big" by duration and concluded the amplification
must live in the big ones because that is where the time is. It does - and
the big ones are not big. They are trivial commands wearing minutes of
overhead.

This is principle 25 - "a lead's cost barely depends on what is in it" - at
its limit, and it is the clearest statement of the fleet's problem yet:

> The fleet's per-lead toll is not tens of seconds. On the leads that decide
> the leg it is **one to five minutes**, for work worth milliseconds.

### And it is a trap for the fix

`worth_offering(est_p90, ...)` gates on **observed duration**. These leads
have an observed p90 of hundreds of seconds, so it would dispatch them
enthusiastically - they look like the biggest jobs in the build. The one
mechanism aimed at "stop sending work that is not worth sending" would send
these first.

Worse, if the estimate is fed from the FLEET's own observations it is shape
7, a feedback signal the controller moves: the fleet is slow on a lead, so
the estimate rises, so the fleet keeps sending it. `bank::timings` is filled
by `bank::logstream` from earthly's log, and **which leg's log it parses
decides whether the mechanism works or chases itself.** It must be the
baseline's.

That is now the first question to answer before wiring anything, and it was
invisible an hour ago when the wiring looked like a lookup.

### What to measure next

The overhead is per-lead and enormous, so the thing to price is a single
lead's fixed cost, decomposed. `lead_split` already reports placing, waiting
and building; on these leads `building` will be nearly all of it, and
`building` includes the worker's own fetch, unpack, export and push. That
decomposition does not exist yet and is the one that would say which of the
four to attack.

### What a 173-second `diff` is actually doing

`lead_split` puts run D at placing 443s (4%), waiting 3,802s (38%), building
5,812s (58%). So a 173-second `diff` spends roughly a minute queued and two
minutes in `building` - and `building` is one `solve()` call, which is:

1. buildkit materialises the graph's INPUTS on that worker,
2. runs the command,
3. exports the result,
4. pushes it to the mesh registry.

Step 2 is the `diff`. Steps 1, 3 and 4 are the toll, and step 1 is the one
that scales with the target rather than with the lead: **a `diff` whose
ancestry is a 600 MiB image costs whatever it costs to pull and unpack 600
MiB**, however small the diff.

On one machine that ancestry is already present - the previous target built
it - and costs nothing. In the fleet each worker materialises it again.

**This is the first hypothesis that fits everything measured tonight:**

| observation | explained |
| ------------------------------------------ | ---------------------------- |
| 6-9x CPU against one machine | ancestry materialised per worker, not once |
| a `diff` at 173s, a `jq` at 197s | their ancestry, not their command |
| `dup` only 1.4-1.8 | dup counts ops SENT, not ancestry unpacked |
| cold cache mounts eliminated | it was never the mounts, it is the layers |
| `+all-binaries` wins with five leads | five ancestries, amortised over minutes of real compute each |
| `+test-ast` loses with 519 | the same ancestry paid hundreds of times |
| a lead's cost barely depends on its content | principle 25, and this is why |

It also explains why every delivery fix tonight moved nothing: prefetch,
seeding and broadcast all pre-position CONTENT, and the cost is not fetching
the bytes, it is unpacking them into a snapshot on every machine that runs a
lead. The worker's own line says so plainly and has all along:

> served 4005 MiB in 104435ms (38.3 MB/s from this registry - **the REST of
> a lead's time is unpack**)

**Not proven.** It is a hypothesis that fits, assembled from figures already
in this file, and the measurement that would settle it does not exist:
`building` needs splitting into materialise / run / export, which buildkit's
own status stream distinguishes and `lead_split` does not.

That split is now the single most valuable instrument this project could
add, and it is worth more than any further seeding or placement experiment.

### The ancestry hypothesis predicts the amplification EQUALS the worker count

buildkit caches a materialised snapshot by vertex digest, so a worker that
has unpacked an ancestry once does not unpack it again for the next lead
that needs it. The cost is therefore per (ancestry x worker), not per lead.

Which gives a number rather than a story:

> If materialising ancestry is the amplification, then the fleet's CPU cost
> over the baseline's should be **approximately the number of workers that
> touch a given ancestry** - because each of them does once what one machine
> did once.

Six workers. Measured amplification 6.5x and 8.7x. That is a closer
agreement than this file has any right to expect from a hypothesis assembled
after the fact, and it is exactly the kind of coincidence that has misled it
before - so it wants a test, not a celebration.

**The test is cheap and decisive: change the worker count.**

| run | prediction if ancestry dominates | prediction if it does not |
| --- | -------------------------------- | ------------------------- |
| 3 workers | CPU amplification falls to ~3x | stays ~6-9x |
| 6 workers | ~6x (measured: 6.5x, 8.7x) | - |
| 12 workers | rises to ~12x | stays ~6-9x |

Nothing else on the table predicts that shape. Per-lead overhead, export
cost, cold mounts and delivery failure are all indifferent to how many
machines are present; only a cost paid once per machine scales with the
machine count.

And it inverts the usual reading of a fleet result. If it holds, **adding
machines makes the fleet cost more CPU while making the leg shorter** - the
wall clock improves and the bill rises, which is a real trade and not a bug,
but only if it is known.

The machinery exists: the plan job already parses `-w<N>` anywhere in the
branch name, and `giles-dispatch-ci-tests-balance-w12-nobase` proves the
pattern works. It needs one branch added to the trigger list and two runs.

Worth doing before anything else, including the `building` split - the split
says WHERE the time goes, and this says whether the answer scales with
machines, which is the difference between a tuning problem and a structural
one.

## The unseeded band is intact, and the noise is not where I said it was

Run `31561556653` - plain `-ast-balance` on current HEAD, unseeded, with the
manifest regression fixed.

| | |
| --------------- | ---------------------- |
| leg | **1,092s** |
| baseline | 216s wall, **607s CPU** |
| worker CPU | 5,575s |
| amplification | **9.2x** |
| placing / home | 0s / 0, 412 of 412 routed |

**The 1,113s reference survives.** I retired it two hours ago on the grounds
that `manifest_dig` and the peer-timeout split had moved the default path.
They had not, at least not measurably: 1,092s against 1,113s is 1.9% apart.
The caution was right and the conclusion was wrong, which is the cheaper way
round.

### Where the variance actually lives

Splitting the samples by what they measure:

| quantity | samples | spread |
| ----------------- | -------------------------- | ------ |
| baseline CPU | 604, 592, 607 | **2.5%** |
| unseeded leg | 1113, 1092 | **1.9%** |
| seeded legs | 1783, 1699, 1625, 1396 | 28% |
| worker CPU total | 3900, 5155, 5575 | **43%** |

So "a third of run-to-run variance" was too broad. **The leg is
reproducible to about 2%. The CPU total is not, at 43%.** They are different
kinds of number and I lumped them.

That makes sense once said: the leg is set by the critical path, so work
that shifts between machines does not move it. The CPU total sums everything
every machine did, including whatever ancestry each happened to
materialise - so it moves with placement, which changes every run.

**Two consequences.**

- Tonight's leg comparisons are sounder than I feared. 1783 -> 1396 across
  the seeded runs is well outside 2%, and prefetch's 1773 -> 1240 and
  `-balance`'s 1240 -> 1050 are far outside it.
- The CPU amplification is the number that needs repetition, and it is
  exactly the number the ancestry hypothesis predicts should track the
  worker count. Testing that at 43% noise needs the effect to be large -
  which, at 3 workers against 6, it should be.

### And seeding is worse than it looked

The unseeded leg is **1,092s**. The best seeded leg all night was 1,396s.
That gap is fifteen times the leg's own noise, so it is real: **seeding
costs about 300 seconds on this target and returns nothing measurable.**

Its CPU amplification, meanwhile, is no worse than unseeded - 6.5x and 8.7x
seeded against 9.2x unseeded. So seeding does not add work so much as
lengthen the critical path, which is what waiting for a seed that has not
arrived would do.

### Ancestry duplication caps at 6x. We measure 9.2x

The arithmetic, which `leg-arithmetic.sh` now prints as `per worker` and
which I should have done before writing the hypothesis up:

Let the baseline's CPU be `A + W` - ancestry materialised once, plus the
real work. If every worker materialises the whole ancestry and the work is
merely split between them, the fleet's CPU is `6A + W`, and

```text
amplification = (6A + W) / (A + W) = 1 + 5f      where f = A / (A + W)
```

`f` is a fraction, so **with six workers this cannot exceed 6.0x**, and it
reaches 6.0 only if the baseline is ALL ancestry and no work.

| measured | implied f | verdict |
| -------- | --------- | -------------- |
| 6.5x | 1.10 | impossible |
| 8.7x | 1.54 | impossible |
| 9.2x | 1.64 | impossible |

**So ancestry duplication cannot be the whole of it, and not by a little.**
Even the most generous reading leaves 1.5x per worker unaccounted for.

The hypothesis is not dead - a 6x ceiling that we are pressed against is
still a strong claim, and nothing else on the table explains even that much.
But something ADDS to it, and the candidates are the ones a worker does that
the baseline never does:

- **export and push** every subtree result as an image. Bounded earlier at
  5-10% of `building`, on traffic figures - which now looks like an
  underestimate worth redoing on CPU rather than bytes.
- **materialise MORE than the baseline's ancestry.** A dispatched subtree is
  a portable rewrite naming every input by digest; the baseline builds
  incrementally and may never materialise some of what the rewrite forces.
  This one would not be a duplication of the baseline's work at all, but
  work the baseline never does.

The second is the more interesting and has never been looked at. It also
survives the `-w3` test differently: pure ancestry duplication scales with
the worker count, but "each dispatched graph forces more materialisation
than the baseline needed" is per-LEAD and would not fall at three workers.

So the test now separates three things rather than two, which makes it
better rather than worse.

### The `-w3` test, stated as a ratio rather than a level

"Amplification should be about the worker count" was the wrong form, because
the measured 9.2x is already above the 6.0x ceiling that model allows - so
comparing a level against a broken model proves nothing. The RATIO between
two worker counts survives the model being incomplete:

| ancestry share of baseline CPU | CPU at 6 / CPU at 3 |
| ------------------------------ | ------------------- |
| 90% | 1.96x |
| 70% | 1.88x |
| 50% | 1.75x |
| 30% | 1.56x |
| 10% | 1.25x |

Whatever the extra 1.5x per worker turns out to be, it is per-LEAD and the
lead count does not change with the machine count - so it contributes to
both runs equally and cancels in the ratio. That is the whole reason to
measure a ratio here.

**The test:** run `-w3`, take the worker CPU total, and divide ref1's 5,575s
by it.

- near **2.0** - ancestry duplication dominates, and the fix is to place
  leads so that fewer machines touch a given ancestry, not to make transfers
  faster.
- near **1.0** - it does not, the cost is per-lead, and `worth_offering` plus
  the `building` split are the way in.

### The pair is doing double duty

`ref1` and `ref2` differ in nothing at all, so their spread is the noise
band for BOTH quantities: the leg (expected ~2%, from 1113 against 1092) and
the worker CPU total (unknown, and the thing that decides whether one `-w3`
run is enough).

If same-config CPU agrees to ~10%, one run at three workers settles it. If
it agrees to ~40%, the `-w3` test needs repeating too, and that is worth
knowing before firing it rather than after.

### Better than `-w3`: one worker isolates the per-lead cost

Writing the model down properly gives a cleaner first run than the ratio
test:

```text
fleet CPU at n workers  ~  n*A  +  W  +  leads*P
                           |       |     |
                           |       |     per-lead overhead (export, push,
                           |       |     over-materialisation)
                           |       the real work, split between machines
                           ancestry, materialised once per machine
```

At **n = 1** there is no duplication at all: one machine materialises the
ancestry once, exactly as the baseline does. So

```text
fleet CPU at 1 worker  -  baseline CPU  =  leads * P
```

which is the per-lead overhead **measured directly**, with no model fitting
and no assumption about how much of the baseline is ancestry. Subtract it
from the six-worker figure and what is left is `5A`.

| run | gives |
| --- | ------------------------------------------- |
| `-w1` | `leads * P` directly, as the excess over baseline CPU |
| `-w6` | already measured: 5,575s CPU, 9.2x |
| the two together | `A` and `P` separately, which is the whole question |
| `-w3` | a check that the model is linear, not the measurement |

It is also the cheapest run in the series - two runners - and the one most
likely to be misread as pointless, because a "fleet" of one machine sounds
like a null experiment. It is the opposite: it is the only configuration
where the ancestry term vanishes and the per-lead term stands alone.

Both branches added to the trigger list, parsing verified (`-w1` strips to
workload `ast`, `W=1`).

One caveat to record before the run rather than after: with a single worker
the leg will be long and occupancy near 1, so **the leg from this run means
nothing** and only the CPU figures should be read. A fleet of one is not a
fleet; it is an instrument.

### What tonight shipped that has never run

Worth stating plainly, because "built and left unconnected" is a failure
this file names four times and there is a fifth shape of it: built, wired,
and never executed.

| change | exercised by |
| ----------------------------- | ------------------------------------ |
| `merge_cache_inputs` | runs B, C, D - confirmed by its log line |
| `worth_seeding` | run B - skipped a 491-byte harvest |
| `manifest_dig` + the CAS check | run D found the bug, the FIX has not run |
| two-phase peer timeouts | runs C, D, ref1 - and selftest |
| **seeder-first ordering** | **its unit test, and nothing else** |
| `prefetch MISS` + counters | run D printed the lines, the COUNTER has not |
| `held_for`, `harvest_is_short` | neither has run |

The seeder-first ordering is the one that matters. It only takes effect on
the broadcast branch, only seeds broadcast, and every seeded run predates
it - so its first execution will be whenever someone next runs `-seed`. And
seeding has now been measured as costing 300 seconds and returning nothing,
so that may not be soon.

`check-seeding` does not cover it either: it runs against a single daemon
with no fleet, so `share_of` sees `ids.len() <= 1` and returns before the
broadcast branch is reached.

That is not an argument for reverting it - it makes the code do what its own
comment has always claimed, and it is covered by a test that asserts the
seeder share is a PREFIX rather than merely present. But a reader should
know that between "committed" and "observed working" there is a gap here,
and it is wider for this change than for anything else tonight.

### 81% of a worker's fetches are already local, which is what the hypothesis needs

Run D's per-worker `[cas] fetches` counters, at the end of the leg:

| worker | local | peer | driver |
| ------ | ----- | ---- | ------ |
| 1 | 2,039 | 185 | 116 |
| 2 | 980 | 179 | 117 |
| 3 | 652 | 193 | 107 |
| 4 | 1,936 | 184 | 226 |
| 5 | 1,913 | 278 | 91 |
| 6 | 1,054 | 220 | 128 |
| **total** | **8,574 (81%)** | **1,239 (12%)** | **785 (7%)** |

Two things follow, and the second is the useful one.

**Transfer is not the cost.** Four fetches in five never leave the machine,
and of the ones that do, more come from a peer than from the coordinator -
so the mesh is working and is barely needed. That is the fourth independent
line of evidence against the delivery story, after the 325-to-7 miss ratio,
the elimination of cold mounts, and seeding costing 300s for nothing.

**Within-worker caching works, so any duplication is strictly across
machines.** An 81% local hit rate is what it looks like when a worker
materialises something once and reuses it for its remaining leads. That is
precisely the premise the ancestry hypothesis rests on - and it had been
assumed rather than checked.

So the shape is confirmed even though the magnitude is not: each machine
pays a set-up cost once, and six machines pay it six times where one machine
paid it once. What remains unmeasured is how big that cost is relative to
the real work, which is exactly what `-w1` will say.

Worth noting the spread too: 652 to 2,039 local fetches, a 3.1x range that
tracks the 6.9x CPU spread. The busiest machine is not just doing more
leads, it is touching more distinct content - which is what an
ancestry-dominated cost looks like when placement is uneven, and another
reason the `-balance` work should weigh content rather than counts.

## 32 GiB to distribute 700 MiB: the ancestry, measured

Run D's artifacts hold the number this hypothesis needed, and it had not
been looked at.

| quantity | run D |
| ------------------------------------ | ------------- |
| content that LEFT the workers | **32,924 MiB** |
| distinct content, summed per worker | 4,232 MiB |
| distinct per worker | **705 MiB** |
| the coordinator's registry served | **742 MiB** |

The coordinator served **742 MiB** - almost exactly one worker's distinct
set. It seeded the content once and the peers redistributed it among
themselves, moving **32 GiB** to do so. The mesh is doing precisely its job
and the job should not exist.

**Each worker ends up holding about 705 MiB of distinct content.** If those
six sets are largely the same content - and they must be, or the coordinator
would have had to serve far more than one set's worth - then the fleet
materialises the SAME ~700 MiB on every machine.

That is the ancestry term, measured rather than inferred:

```text
A  ~  705 MiB per machine
baseline unpacks it once ;  six workers unpack it six times
```

### What it does and does not settle

**Settles the mechanism.** Transfer is not the cost (81% of fetches are
local, the coordinator serves one set), but DISTRIBUTION plus per-machine
unpacking of a common 700 MiB is real, is paid six times, and is invisible
in every counter this project had before tonight.

**Does not settle the magnitude.** Whether unpacking 705 MiB accounts for
the 9.2x depends on what unpacking costs in CPU against the 607s the
baseline spends, and nothing here measures that. `-w1` does: with one worker
the 705 MiB is materialised once, like the baseline, and any amplification
left over is the per-lead term.

It also explains the 43% run-to-run variance in worker CPU while the leg
holds to 2%: which machine ends up materialising which part of the ancestry
depends on placement, and placement changes every run - but the critical
path does not care who did it.

### Portability may be the 1.5x that ancestry duplication cannot explain

Ancestry duplication caps at `N` and we measure 9.2x on six machines, so
something adds roughly 1.5x per worker. This file already contains a
candidate, filed months ago under a different question:

> Warming cannot work, because portability changes the cache keys. Making a
> graph portable rewrites the ops that name a local context or a base image,
> every digest downstream of them changes, and buildkit's cache is keyed on
> exactly those digests. A warm worker cannot key-match a dispatched graph.
> One measured warm worker still fetched 354 MiB.

That was written about warming. It says something larger.

**The fleet does not build the baseline's graph.** `make_portable` rewrites
every source, so every digest downstream of a rewritten op differs from the
one the baseline computed. The two legs therefore materialise DIFFERENT
content, and the fleet's set is not a copy of the baseline's - it is a
parallel universe of the same build with different addresses.

Two consequences that the ancestry model does not capture:

- **Nothing is shared between the legs.** Whatever the baseline warmed, the
  fleet cannot use, in either direction. That is already known and is why
  warming was abandoned.
- **The fleet may materialise MORE than the baseline needs.** A rewritten
  graph names every input by digest; the baseline builds incrementally and
  can skip materialising an intermediate it already holds under a different
  identity. This has never been checked, and it is exactly the shape of a
  per-lead term that does not scale with the machine count.

So the `-w1` run separates three things rather than two, and the third now
has a name and a mechanism rather than being "something else":

| term | scales with | isolated by |
| ------------------- | ------------- | ---------------- |
| ancestry | machine count | `-w6` minus `-w1` |
| per-lead toll | lead count | `-w1` minus baseline |
| portability overhead | lead count | inside the above, and only a `building` split separates it |

The connection was available all along - the warming finding is 2,500 lines
above tonight's measurements in the same file. It took measuring the
ancestry to notice that a paragraph about warm workers was really about
what a dispatched graph costs.

### A registry mirror would remove half of `make_portable`

If rewriting the graph is what breaks digest equality with the baseline,
the question is whether the rewrite is necessary. Half of it may not be.

`make_portable` does two things:

1. **local contexts** become published images - unavoidable, a sessionless
   peer genuinely cannot read the client's filesystem;
2. **base images** are repointed at our mirror - `docker.io/library/x` to
   `172.17.0.1:15000/library/x`.

The second exists because a sessionless peer cannot authenticate to Docker
Hub. But buildkit already solves that without touching the graph, and the
fork we run documents it:

```toml
[registry."docker.io"]
  mirrors = ["yourmirror.local:5000"]
  http = true
  insecure = true
```

The daemon then serves `docker.io/library/x` from our registry while the
GRAPH still says `docker.io/library/x`. Same identifier, same op bytes,
**same digest** - so a worker's cache and the baseline's would key-match for
everything downstream of a base image, which today they never do.

The workflow already writes a `[registry."172.17.0.1:15000"]` stanza into
every daemon through `EARTHLY_ADDITIONAL_BUILDKIT_CONFIG`. Adding a
`mirrors` line to a `[registry."docker.io"]` stanza is the same mechanism,
one block further down.

**What it would be worth, and what it would not.** It cannot remove the
ancestry term - every machine still materialises what its leads need. It
would remove the portability term, make warming possible for the first
time, and let the baseline and fleet legs share content instead of building
parallel universes of the same target.

**And it has a hard prerequisite, found while checking it.** A registry
acting as a mirror for `docker.io` must serve MANIFESTS for tags it has
never seen. Ours cannot:

```rust
Ok(None) => err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", &key),
```

There is no upstream consulted on the manifest path, and the workflow says
so in two places already - "the registry's pull-through is BLOB-only: a
manifest tag it does not hold is a 404, with no upstream consulted". That is
coherent for a mesh mirror filled deliberately by `mirror_image`, and
disqualifying for a `docker.io` mirror, which is asked for tags by
definition.

So the order is: **manifest pull-through first, mirror stanza second, and
only then does the graph stop being rewritten.** Each is small; together
they are a project, and the first one changes a component this file has
already recorded three faults in.

**A design note rather than a finding**, and doubly so now. The rewrite may
serve purposes its comment does not list, and a mirror stanza changes who
fetches from where at exactly the moment credentials matter - the area where
this project has produced `no active sessions` twice. It wants reading
before it wants doing.

### Grafting is the mechanism principle 30 asks for, and it is off

Principle 30 says the fleet's cost is `N x ancestry`, and that no scheduler
can help because every machine that runs a lead needs that lead's ancestry.
There is one mechanism here that does not try to schedule around it and
instead changes what "needing the ancestry" costs.

**Grafting** imports a published prefix as an image rather than rebuilding
it from its ops. Same content on the machine either way, but a pull-and-
unpack instead of running every `RUN` and `COPY` in the chain. Against an
ancestry that is mostly build steps, those are very different prices - and
`cut_prefix` already publishes the prefixes for it to import, 113 of them in
run D.

The audit above records its status honestly and the entry now reads
differently in the light of tonight:

> `graft` - off. Measured ON one shape (+107s) and never re-measured against
> a warm bank. Its +107s was measured within a single run, where the
> ancestor has to be built before it can be imported. Across runs the bank
> already holds it.

So the one measurement grafting has is of the case where it cannot win: in a
cold run the prefix must be built once before anyone can import it, and the
importer pays a pull on top. Everything tonight says the cost being attacked
is real and large, and that the measurement which condemned grafting was
taken in the configuration least favourable to it.

**That does not make it a good idea; it makes the existing evidence
inadmissible.** With the bank warm the prefix is already published, and
grafting becomes "pull one image" against "rebuild the chain on six
machines" - which is exactly the `N x ancestry` term.

Priority against the other candidates: `-w1` first, because it says how big
the ancestry term is and everything else is guesswork until then. But if
`-w1` confirms it, grafting against a warm bank is the cheapest attack on
it, needs no new code, and its only contrary measurement is one this file
already calls unrepresentative.

## The pair: worker CPU is reproducible to 0.1%, and that changes everything

Two runs, identical code, identical configuration, back to back.

| | ref1 | ref2 | spread |
| ------------------- | ------ | ------ | ------ |
| leg | 1,092s | 1,039s | 5.1% |
| baseline CPU | 607s | 604s | 0.5% |
| **worker CPU total** | **5,575s** | **5,579s** | **0.1%** |
| amplification | 9.2x | 9.2x | - |

**The worker CPU total is reproducible to one part in a thousand.** Four
seconds apart on five and a half thousand.

So "43% run-to-run variance in CPU" was wrong, and wrong in the way that
matters: I measured the spread across runs with DIFFERENT configurations -
seeded, seeded-with-a-regression, unseeded - and attributed it to noise.
Same configuration twice gives 0.1%.

### What that unlocks

Every CPU comparison tonight is now admissible, and they say something the
leg alone did not:

| run | amplification | configuration |
| ----- | ------------- | ----------------------------- |
| C | 6.5x | seeded |
| D | 8.7x | seeded, plus my manifest regression |
| ref1 | 9.2x | unseeded, fixed |
| ref2 | 9.2x | unseeded, fixed |

**Seeding lowers the CPU amplification from 9.2x to 6.5x.** It is doing real
work: a filled cache mount means a worker skips work it would otherwise do,
and that shows up as 29% less total CPU.

**And it raises the leg from ~1,065s to 1,396-1,783s.**

So seeding is not useless, which is what the leg alone said all night. It
trades **critical path for total work**, and this fleet is critical-path
bound, so the trade loses. On a fleet that was throughput-bound - many
targets queued, machines saturated - the same mechanism would win.

That is a genuinely different conclusion from "seeding does not pay", and it
was invisible until the noise band existed.

### What is still not reproducible

The per-worker split. ref1 `240 742 878 985 1084 1646`, ref2
`243 360 674 1284 1293 1725` - the same total, distributed differently every
time. Placement is where the randomness lives, and it moves the leg (5.1%)
while leaving the total untouched (0.1%).

Which is a clean statement of what the scheduler can and cannot do: **it
decides who does the work, not how much there is.** Every mechanism that
tried to reduce total work by placing differently was attempting something
placement cannot do, and principle 30 says why - the ancestry is needed by
whoever runs the lead, wherever that is.
