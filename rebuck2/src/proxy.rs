//! rebuck2 in front of a buildkitd, changing nothing.
//!
//! Point earthly at this instead of the daemon and every `SolveRequest`
//! passes through our hands. It forwards all of them unaltered — this
//! version dispatches NOTHING — and reports what it would have offered.
//!
//! That order is deliberate. We do not yet know whether real earthbuild
//! shards contain subtrees worth shipping: principle 11 says a chain rooted
//! at `FROM <registry image>` is the best possible handover, and
//! [`crate::dispatch::analyse`] can now count them, but counting them on
//! invented graphs proves nothing. A proxy that only measures answers "is
//! there anything here to dispatch" before a line is written to exploit it,
//! and it costs one hop.
//!
//! Interposing rather than asking earthbuild for the graph is what keeps
//! this in one repo: the Control service is a stable, versioned wire, and
//! we already speak it.
//!
//! # MEASURED: this IS the right layer, once it serves the gateway too
//!
//! An earlier round concluded the opposite, and was wrong. `Control.Solve`
//! really does arrive with no definition — but the graph was not missing,
//! it was on the other service. A buildkit client drives its build through
//! `LLBBridge` created with `NewLLBBridgeClient(c.conn)`: the SAME
//! connection, a second service. The `Unimplemented` blamed on session
//! relaying was this proxy not offering it.
//!
//! Measured, with a real `buildctl` through a real daemon:
//!
//! ```text
//! [proxy] solve ...: NO definition on the wire (frontend="")
//! [proxy] GATEWAY solve: 3 ops, 0 cuts >= 4, 0 free-frontier
//! ```
//!
//! Three client shapes, and only the middle one is invisible:
//!
//! | client | where the LLB is |
//! | ------------------------------- | ---------------------------- |
//! | raw LLB (our own solve, testkit) | `Control.Solve`, definition set |
//! | frontend by NAME (`--frontend dockerfile.v0`) | nowhere — the frontend runs INSIDE the daemon |
//! | client-built LLB (earthly, `buildctl build < llb`) | `LLBBridge.Solve` — **here** |
//!
//! The invisible one costs nothing: if no graph crosses the wire there is
//! no graph to dispatch, and the daemon was always going to build it alone.
//!
//! # First real measurement: earthly, end to end
//!
//! A real `earthly +test` on a three-target Earthfile, through this proxy,
//! against earthbuild's own buildkitd. It succeeded, and it said:
//!
//! ```text
//! gateway solves : 6
//! ops per solve  : min 3 median 9 max 9
//! sources        : 6 registry, 0 local, 0 other
//! platforms      : {"linux/arm64"}
//! repeated ops   : 31 (73% seen again in a later solve)
//! distinct graphs: 3 of 6 solves (3 identical RESENDS)
//! overlap/solve  : [(0,3), (2,6), (5,9), (9,9), (6,6), (9,9)]
//! ```
//!
//! **The 73% is not what it looks like, and the first reading of it here was
//! wrong.** Half the solves are byte-identical RESENDS of a graph already
//! sent - the client driving the API, not work being shared. Read as
//! overlap it says routing whole Solves would duplicate three quarters of
//! the build, which would have been a conclusion drawn from an artefact.
//!
//! What the three DISTINCT graphs show is nesting: 3 ops, then 6 of which 2
//! are already seen, then 9 of which 5 are. Each solve extends the last, so
//! genuine overlap is 7 of 18 ops - about 39%, not 73%.
//!
//! Nesting is a sharper result than repetition would have been, and it
//! points the other way from the first note:
//!
//! - **Per-Solve routing is worse than the raw number suggested.** The
//!   graphs are not merely overlapping, they are cumulative - the last
//!   solve CONTAINS the earlier ones. Handing solve #3 to another machine
//!   asks it to rebuild everything solve #2 just did.
//! - **The natural unit is the INCREMENT between successive solves**, which
//!   is a subtree. The client hands us its subdivision already, one layer at
//!   a time; we do not have to infer a cut, only diff.
//!
//! Caveats: three targets is not a shard, and this Earthfile has no `COPY`
//! from the build context, which is why `local` sources are zero. A real
//! repo will not look like that. What this run establishes is that the
//! instrument works - and that a summary percentage was one decomposition
//! away from being a wrong answer.
//!
//! # Second measurement: shape decides everything
//!
//! The first Earthfile was a CHAIN (`test` -> `build` -> `deps`), which is
//! the worst case and was mistaken for a general result. A fan-out - four
//! independent targets over one shared base, with a `COPY` from context -
//! says something quite different:
//!
//! ```text
//! gateway solves : 12      distinct graphs: 12 of 12 (0 RESENDS)
//! ops per solve  : min 4 median 5 max 17
//! overlap/solve  : [(0,4), (1,5), (1,5), (1,5), (1,5), (1,17), ...]
//! sources        : 12 registry, 12 local
//! repeated ops   : 13 (13%)
//! ```
//!
//! **Each independent target arrives as its OWN gateway Solve, sharing
//! exactly one op with everything before it** - the common base. Overlap
//! falls from 39% on the chain to 13% here, and no graph is re-sent.
//!
//! So overlap is a property of the BUILD SHAPE, not of the client. A chain
//! yields cumulative graphs and nothing worth routing; a fan-out yields
//! independent units on a plate. Real multi-target repos - the ones dispatch
//! exists for - are fan-outs.
//!
//! That is strong for routing whole Solves, and it is the third verdict this
//! measurement has produced on the same question. Worth stating plainly: the
//! first two were drawn from one toy graph, and the honest lesson is that a
//! single build shape cannot price a mechanism.
//!
//! # The context is carried by whoever proxies the session. Measured.
//!
//! Every cut in the fan-out reports a NON-free frontier, because `COPY` puts
//! a `local://` source in every graph. The question that matters is what
//! that costs, and it is not a matter of opinion:
//!
//! | build context | session bytes, client -> daemon |
//! | ------------- | ------------------------------- |
//! | 16 bytes | 1 KiB |
//! | 32 MiB | 32,812 KiB |
//!
//! The context flows over the session, so it flows through the proxy. A
//! relaying coordinator IS on the data path for context - not for layers,
//! which still go peer to peer, but for every byte of the repository a
//! builder needs. With N peers each needing it, N times.
//!
//! **This reconciles the mechanism argument, and not in the direction the
//! previous measurement suggested.** Per-Solve routing looked strong because
//! a fan-out hands over independent Solves; but every one of those Solves
//! wanted the context, so routing any of them moves the repository through
//! the coordinator. The free-frontier requirement built for subtree dispatch
//! turns out not to be a nicety - it is the ONLY shape that dispatches
//! without putting the coordinator on the data path, which is to say it is
//! what principle 6 actually requires.
//!
//! So the options are now concrete rather than architectural taste:
//!
//! - dispatch only subtrees whose frontier is registry digests (free, §6
//!   intact, but a `COPY` anywhere in the chain disqualifies it)
//! - relay the context and accept being on the data path for it
//! - give the peer its own session to the client, which the client has no
//!   reason to offer and no protocol to be asked with
//! - make the context itself content-addressed and fetchable peer to peer,
//!   which is the mesh's existing job and the only option that both
//!   dispatches `COPY`-bearing work and keeps §6
//!
//! The last one is worth the most and is not built.
//!
//! # Pointing earthly at a proxy, which is not obvious
//!
//! Earthly MANAGES buildkitd when it thinks the address is local, and
//! `containerutil.IsLocal` matches the literal strings `127.0.0.1`,
//! `localhost` and `::1`. So a loopback address spelled differently reads as
//! remote and it connects instead:
//!
//! ```yaml
//! global:
//!   buildkit_host: tcp://[0:0:0:0:0:0:0:1]:11234   # ::1, expanded
//!   tls_enabled: false
//! ```
//!
//! The upstream must be earthbuild's OWN `earthbuild/buildkitd`, not
//! `moby/buildkit`: earthly asks for an exporter named `earthly` that only
//! its fork has. Stock buildkit gets as far as the build and then says
//! `exporter "earthly" could not be found`.
//!
//! # MEASURED: a gateway Solve cannot simply be routed
//!
//! Routing gateway Solves to a second daemon gets a long way and then
//! fails on something structural:
//!
//! ```text
//! NotFound: forwarding Solve: no such job ow8s6ghu1knxv6jox461m0xmk
//! ```
//!
//! The gateway conversation is scoped to a JOB, created by `Control.Solve`
//! on one daemon. A peer never saw that call, so it has no such job and
//! cannot accept a gateway Solve under its id. The build id in the header
//! is not a name we can forward; it is a handle into one daemon's state.
//!
//! Two earlier measurements said the same thing from different angles and
//! this completes them: refs are daemon-local (eleven `read_dir` calls
//! follow eleven solves), and now jobs are too.
//!
//! **So the peer is reached through `Control.Solve`, not the gateway.** The
//! shape that works is the one the dedup line already had a name for:
//!
//! 1. the proxy solves the portable graph on a peer, via its own
//!    `Control.Solve`, exporting the result to the mirror
//! 2. the client's gateway Solve is then answered on peer 0 with a graph
//!    that merely IMPORTS that image
//! 3. peer 0 fetches content instead of building, and returns a ref that
//!    belongs to it - so `read_dir` and `return` work unchanged
//!
//! That is adoption, not forwarding, and it is what `adoptLeaderResult`
//! does for the dedup line. The routing code below is kept because
//! everything except step 2 is right: peers, ref affinity, portability
//! rewriting and placement all stand.
//!
//! # MEASURED: a sessionless peer cannot reach Docker Hub
//!
//! Adoption works - a peer builds a portable graph through its own
//! `Control.Solve` and publishes the result - but the peer then tried to
//! pull its BASE image and said:
//!
//! ```text
//! error="no active sessions" host=registry-1.docker.io
//! panic: invalid memory address or nil pointer dereference
//!   solver/llbsolver.(*resultProxy).wrapError bridge.go:288
//! ```
//!
//! Registry auth travels over the session, and a peer has none - that was
//! the whole point of rewriting `local://` away. So a graph is only truly
//! portable when EVERY source is something the peer can fetch unauthenticated,
//! and `docker-image://docker.io/...` is not that, even for a public image.
//!
//! Principle 9 says this too, and says it about exactly this: the origin
//! registry is a fallback, not a data path; fetch once into the fleet and
//! serve peer to peer. The rewrite has to cover base images as well as
//! contexts - both become references into the mirror, and then a peer needs
//! no session, no credentials and no upstream at all.
//!
//! Also worth knowing: this buildkit PANICS rather than returning the
//! error, so the failure arrives as a dead daemon rather than a failed
//! build. A fleet must treat a peer that stops answering as a decline, not
//! wait on it.
//!
//! # IT DISTRIBUTES
//!
//! A real `earthly +all`, two daemons, and one of the build's solves ran on
//! a machine the client never spoke to:
//!
//! ```text
//! [proxy] base docker.io/library/alpine:3.20@sha256:... mirrored as .../rebuck2/base:9ae21ebe
//! [proxy] adopted from peer 1: docker-image://.../rebuck2/adopted:c3eb00fb
//! [wire]  solves routed  : 1 to other daemons
//! peer 2 cache          : Total 13.73MB
//! ```
//!
//! The chain, all of it measured into existence rather than designed up
//! front: the graph arrives at the GATEWAY; its context is published as
//! content by the daemon that holds the session; its base images are
//! copied into the mirror by the daemon that holds the credentials; the
//! rewritten graph names nothing but content; a peer builds it through its
//! own `Control.Solve` and publishes the result; and the client's solve is
//! answered here with an import, so the ref it gets back belongs to the
//! daemon holding its job.
//!
//! Solves whose sources were not all mirrored yet were NOT sent - they
//! failed the portability check and built locally. That is the system
//! working: an unportable graph is not a dispatch failure, it is a graph
//! that stays home.
//!
//! # The dispatch TAX is nil. The SPEEDUP is unmeasured, and cannot be
//! measured here
//!
//! Four concurrent CPU-bound builds (each hashing ~1.8 GB), same work both
//! ways:
//!
//! ```text
//! one daemon, no proxy : 10s
//! two daemons, proxied : 10s   (4 of 4 solves routed)
//! ```
//!
//! So the machinery is FREE at this scale: publishing a context, mirroring
//! a base image, rewriting a graph, solving on a peer, publishing the
//! result and importing it costs nothing measurable against the work
//! itself. That is worth knowing - a tax of 3x would have made capacity
//! irrelevant.
//!
//! It is not a speedup, and no arrangement of this machine could show one.
//! Both daemons are containers on one host sharing one CPU, so the fleet
//! has no capacity the single daemon lacked; 10s versus 10s is the correct
//! answer, not a disappointing one. Measuring speed needs a second
//! MACHINE.
//!
//! Note before anyone reaches for the x86 box for that: its docker0
//! firewall stops containers reaching host services, which is precisely how
//! a peer would reach the mirror. That does not fail loudly - it would zero
//! the measurement while looking like a slow fleet.
//!
//! A sleep-based workload was tried first and is useless for this: one
//! daemon serves four sleeps as fast as four daemons, so the fleet looks
//! free because nothing is contended. The work has to be CPU-bound before
//! either number means anything.
//!
//! Both numbers above, and every number below, are now produced by
//! `scripts/fleet.sh` - which exists because they were produced by hand
//! twenty-five times first, and a measurement you retype is a measurement
//! you cannot compare.
//!
//! # The fleet was OFFLOADING, not parallelising
//!
//! Same harness, after peer 0 was let back into the round robin - the
//! per-daemon cache is the honest witness, because work leaves a mark where
//! it actually ran:
//!
//! ```text
//! before  daemon 0: 0        daemon 1: 13.60MB   (4 routed, 0 home)
//! after   daemon 0: 13.60MB  daemon 1: 13.60MB   (2 routed, 2 home)
//! wall    10s direct -> 11s proxied
//! ```
//!
//! "4 of 4 solves routed" read as a triumph and was a symptom: peer 0 was
//! excluded from dispatch by arithmetic (`1 + n % (len - 1)`), so a
//! two-machine fleet ran every exec on ONE machine and shipped bytes with
//! the other. It could not have gone faster than a single daemon no matter
//! how many machines were added, because peer 0's share was always zero.
//! See `turn` for why the reasoning behind that was sound and the conclusion
//! was not.
//!
//! # Where the tax actually is, and the two guesses that were wrong
//!
//! "Two adoptions' worth of registry round-trip" was the obvious reading of
//! 10s -> 11s and it was wrong. Timing every phase instead:
//!
//! ```text
//! solve 0  total     1 = portable    0 + peer    0 + answer 1   home
//! solve 1  total     2 = portable    0 + peer    0 + answer 2   home
//! solve 2  total 10984 = portable 1555 + peer 9427 + answer 0   dispatched
//! solve 3  total 10984 = portable 1546 + peer 9437 + answer 0   dispatched
//! build ms  [10661, 10655, 11049, 11044]   what the client waits for
//! return ms [1, 0, 0, 0]
//! ```
//!
//! Three things fell out, none of them the round-trip:
//!
//! 1. `make_portable` costs ~1.6s - publishing the context and mirroring the
//!    base - and EVERY solve paid it, including the two that were never
//!    going to leave. Moving the rewrite behind the placement decision took
//!    those from 1613ms to 1ms. The cheap check (exclusions) now runs first
//!    and the expensive one (rewrite) only for graphs that are leaving. That
//!    matters most where dispatch is worst: earthly excludes eleven solves
//!    in twelve, and was mirroring a base for each of them.
//! 2. A gateway Solve is LAZY - ~1ms whether it stands for a nine-second
//!    build or a pull. Read alone it says answering is free.
//! 3. `return` is ALSO ~1ms, which was the next guess and also wrong. The
//!    client's wait lives in `Control.Solve`, which is now timed.
//!
//! What is left is ~0.4s per dispatched build (11.05s against 10.66s at
//! home) plus a one-time ~1.6s to mirror a base image, shared by every
//! solve that needs it and amortised to nothing across a real build. On a
//! real fleet that is paid once against a machine's worth of parallelism.
//!
//! # One slow machine costs the whole build, and load-awareness does not fix it
//!
//! Three daemons, six builds, the last daemon held to a quarter of a CPU
//! (`SLOW=0.25 DAEMONS=3 BUILDS=6 scripts/fleet.sh`):
//!
//! ```text
//! uniform fleet : wall  12s
//! one slow peer : wall 164s   build ms [10549, 10581, 10972, 10973, 162953, 162976]
//! ```
//!
//! Two of six solves landed on a machine four times slower, and the build
//! waits for them. Placement gave it an equal share because placement had no
//! idea it was slow.
//!
//! `least_loaded` was written for this and DOES NOT FIX IT, which is worth
//! saying plainly rather than shipping it as a win. Six solves of a fan-out
//! arrive at once, so every peer's outstanding count is zero at the moment
//! each is placed, and least-loaded degenerates to exactly round robin. It
//! earns its place for the other shape - solves arriving spread across a
//! long build, where a busy peer is visibly busy - and it is never worse.
//! But load is not capacity.
//!
//! Slot limits do not fix it either, and the arithmetic says why before the
//! experiment does: any placement that gives the slow machine even one of
//! these tasks waits 163s for it. The fix has to be a peer that can REFUSE
//! (principle 12 - refusal is backpressure) or a placer that knows roughly
//! how long the work takes and roughly how fast each machine is (principle
//! 13 - coarse estimates). That is what `bank::timings` is for, and it is
//! not wired to placement yet.
//!
//! The outputs were byte-identical anyway: a starved peer returns correct
//! bytes slowly, which is the failure mode to prefer.
//!
//! # Taking it back: 164s -> 40s
//!
//! Placement cannot fix a straggler, because the placement was CORRECT on
//! the information available - the peer was idle. The information only
//! arrives afterwards, when a normal adoption finishes and the straggler does
//! not. So the wait is bounded, and the bound is re-read while waiting:
//!
//! ```text
//! uniform fleet        : wall  12s
//! one slow peer        : wall 164s
//! one slow peer, bound : wall  40s   not routed {"peer too slow": 2}
//! ```
//!
//! Both solves on the quarter-CPU peer were withdrawn after three times the
//! observed median and built at home. The peer is not cancelled and its work
//! is not wasted if it lands: the tag is the digest of the graph, so whoever
//! pushes first wins (principle 3) and a later identical push is a no-op.
//! Outputs stayed byte-identical against a recorded baseline, including the
//! builds where home and peer were racing.
//!
//! The threshold takes no cold default on purpose. Any number between 9.5s
//! and 163s "fixes" this fixture, which is precisely why one should not be
//! chosen before the fleet has said what normal is - with nothing observed,
//! nothing is slow and the wait is unbounded, exactly as before.
//!
//! # The optimal split belongs to the WORKLOAD, not the fleet
//!
//! Two machines, twenty-four builds, the split swept at three build sizes.
//! `work` is the fixture's inner loop count and scales compute per build;
//! transfer cost per dispatched build is roughly constant.
//!
//! ```text
//! work   no fleet   12:12   16:8   20:4   22:2
//!   20        8s      12s    10s     7s     7s
//!   90       24s      18s    15s    18s      -
//!  250       64s      32s    33s    40s      -
//! ```
//!
//! Three things fall out, and the first is the one that matters:
//!
//! 1. At `work=20` THE FLEET HURTS. An even split takes 12s against 8s for
//!    not having a fleet at all - a 50% slowdown - and only by barely using
//!    the peer (22:2) does it draw level. Every dispatched build pays a
//!    constant transfer, and when the build is shorter than the transfer,
//!    shipping it is a loss.
//! 2. At `work=250` the fleet is worth 2x (64s -> 32s) and the optimum is at
//!    or beyond an even split, because the transfer has amortised away.
//! 3. The optimal dispatched SHARE rises monotonically with build size: ~8%,
//!    33%, >=50%.
//!
//! So a fleet-wide weight - declared, derived, or swept for - is answering
//! the wrong question. The 20:4 that is optimal for small builds costs 40s
//! against 32s on large ones, 25% worse. There is no single number, because
//! the number is a property of the work.
//!
//! The decision belongs PER SUBTREE: dispatch when the estimated compute
//! exceeds the transfer cost, keep it at home when it does not. That is what
//! `dispatch::worth_offering(est_p90, running_for)` was written for, and what
//! `bank::timings` estimates durations for - both still unwired, which is now
//! the largest gap between what this system knows and what it does.
//!
//! # The balance condition is real; feeding it back is unstable
//!
//! The sweep left an obvious question: can the optimum be found rather than
//! swept for? Measuring what the client waits, split by where the work went,
//! says yes - the optimum is exactly where the two sides finish together:
//!
//! ```text
//! 12:12  wall 18s   home 11425ms  away 17235ms  ratio 1.51
//! 16: 8  wall 15s   home 15228ms  away 15506ms  ratio 1.02
//! ```
//!
//! And the arithmetic on the bad split points at the good one:
//! `derive_weights(11425, 17235)` is 3:2, next door to the 16:8 that measured
//! best. That is a control law with no core counts in it.
//!
//! Feeding it back does not work, and two runs of the same experiment are how
//! that showed:
//!
//! ```text
//! adapt   24 builds x3   18 14 14s   placed 40:32
//! adapt   same again     24 17 24s   placed 34:38   <- drifted the wrong way
//! pinned  control        18 15 16s   placed 36:36
//! ```
//!
//! Service time is ENDOGENOUS - what is measured is caused by what is set.
//! Load the home side, its mean rises, the ratio drops below one, the
//! controller reads "home is the straggler" and sends more work away, which
//! raises the away mean in turn. The first run looked like a win over the
//! hand-tuned 15s; the second was worse than doing nothing.
//!
//! So it ships OFF, behind `REBUCK2_ADAPT=1`, with the measurement kept: the
//! law is worth having and this loop around it is not. A stable version has
//! to break the feedback - compare against a quantity the controller does not
//! move, or damp and converge rather than jump to the ratio each time.
//!
//! The control run earns its own line. Rounds two and three are faster
//! whatever placement does, because the peers warm their own caches; a
//! self-tuner left switched on would have taken credit for that too.
//!
//! # Capacity cannot be guessed from cores, and the guess made it WORSE
//!
//! The 50/50 split of the previous result looked like the obvious waste: the
//! peer has 32 cores against this machine's 16, so it should take twice the
//! work. Placement was made weighted to allow exactly that, and then measured.
//! Twenty-four builds, the split swept:
//!
//! ```text
//! home:peer   wall
//!  24: 0      24s    no fleet at all
//!   8:16      21s    peer weighted 2x, "because it has twice the cores"
//!  12:12      18s    flat
//!  16: 8      15s    <- best
//!  18: 6      16s
//!  20: 4      18s
//! ```
//!
//! There is an interior optimum and the informed-looking guess sits on the
//! WRONG SIDE of it - worse than the flat split it was meant to improve, and
//! only three seconds better than not having a fleet. Sending more work to
//! the bigger machine costs more, because every dispatched build pays a push
//! and a pull across the LAN and a local build pays neither. Cores measure
//! what a machine can compute; they say nothing about what it costs to give
//! it something to compute.
//!
//! So the weighting mechanism stays - it is what makes the optimum reachable
//! at all - and the DEFAULT stays 1. A weight is a statement about observed
//! end-to-end throughput, not about hardware, and setting it from a core
//! count is worse than leaving it alone. Deriving it from measurement is the
//! next thing; the sweep above is what it has to beat.
//!
//! # It is faster on two machines: 24s -> 18s
//!
//! The measurement this whole thing existed to make, and which one host could
//! never produce. Twenty-four CPU-bound builds against sixteen local cores,
//! then the same twenty-four with a 32-core x86 box across the LAN:
//!
//! ```text
//! one machine  : wall 24s
//! two machines : wall 18s   placed {home: 12, peer1: 12}
//! outputs identical to the single-machine baseline
//! ```
//!
//! Twelve builds executed on another physical machine, across an
//! ARCHITECTURE boundary, and every byte came back the same.
//!
//! 1.33x rather than 2x, and the reasons are known rather than guessed: each
//! dispatched build pays a push and a pull over the LAN, twenty-four builds
//! on sixteen cores is only mildly oversubscribed, and a flat 50/50 split
//! ignores that the peer has twice the cores and adds latency. Capacity-aware
//! placement is the next thing worth measuring, and it is now measurable.
//!
//! What made it work is that base images are mirrored FOR THE PEER
//! (`solve::mirror_image`'s `platform`, and the architecture in the tag).
//! Before that the mirror held whatever this host resolved, and the previous
//! section is the record of the x86 peer dying on an arm64 binary.
//!
//! One bug in the middle, of a shape worth naming: the mirror wrote under the
//! key `base:linux/amd64` and the rewrite read under `base`. The image was
//! copied correctly, the lookup found nothing, the graph kept naming
//! docker.io, and the solve was then refused as "base unmirrored" - the log
//! printing `base ... mirrored as ...` and `base unmirrored` one after the
//! other. A success and a failure that never meet.
//!
//! # A SECOND MACHINE, and what it refuted
//!
//! Everything above ran several daemons on one host, which can measure
//! overhead and placement but never capacity - the fleet had no more CPU than
//! the single daemon did. One daemon on this arm64 host, one on a 32-core x86
//! box across the LAN, mirror named by an address both can reach:
//!
//! ```text
//! peer 0 upstream            native linux/arm64
//! peer 1 192.168.1.137:18400 native linux/amd64
//! peer 1 could not take it: exit code: 255
//!   sources=["docker-image://192.168.1.91:15000/rebuck2/base:45ee4c56..."]
//! wall 16s (baseline 10s), all six outputs correct
//! ```
//!
//! The plumbing works: the remote peer was reached, and it PULLED the base
//! image from this machine's mirror over the LAN. What fails is the image
//! itself. `make_portable` mirrors `alpine:3.20` as resolved HERE, so the
//! mirror holds the arm64 variant and the portable graph names a
//! single-architecture base. An x86 peer pulls it and dies with `exit code:
//! 255` - a binary it cannot execute - after six seconds of downloading.
//! Fail-open recovered every build; the cost was time and an alarming log.
//!
//! This refutes what the multi-arch section below concluded. "Emulation is
//! legal and merely 5-10x slower, so deprioritise rather than refuse" is true
//! of a peer resolving a manifest LIST for itself, and false of a peer handed
//! a single-architecture copy. Until the mirror carries manifest lists, a
//! peer that is not native cannot build the graph at all, so:
//!
//! - an unpinned graph is read as pinned to whatever peer 0 resolved it as,
//!   because that is what the mirrored base actually is; and
//! - a fleet with no native away peer builds at HOME rather than offering.
//!
//! With both, the cross-architecture fleet places `{home: 6}`, wastes
//! nothing, and matches the single-machine baseline. Real speedup across
//! machines needs the mirror to carry manifest lists - that is the next
//! thing, and it was invisible from one host.
//!
//! Worth recording: the x86 box's docker0 firewall, which stops ITS
//! containers reaching ITS host services, did not bite. The mirror lives on
//! the other machine, so the traffic is ordinary LAN traffic.
//!
//! # The context path finally ran, and the fixture was the bug
//!
//! Every fixture until now sourced only from `docker-image://`, so
//! `contexts published: 0` in every single run: the machinery that unpins a
//! subtree from the machine holding the client's disk had never executed. A
//! fixture with a real `local://context`:
//!
//! ```text
//! sources 4 registry, 4 local    contexts published: 4
//! placed {home: 2, peer1: 2}     outputs identical to baseline
//! ```
//!
//! Files on the client's disk became content in the mirror, and a peer with
//! no session and no access to that disk built from them. That is the whole
//! claim of the design, executed for the first time.
//!
//! Getting there cost two fixture bugs, both of which produced WRONG BYTES
//! while every exit code stayed zero - and both of which would have read as
//! dispatch corrupting results:
//!
//! 1. All N graphs were byte-identical, because a context that differs only
//!    in CONTENT does not change the graph that names it. Buildkit correctly
//!    treats identical vertices as one build: four solves, one execution,
//!    every client handed build 0's bytes.
//! 2. With the graphs made distinct, the LOCAL SOURCE vertex was still
//!    identical across builds - so buildkit synced the first client's
//!    directory and served it to all four. Distinct graphs, distinct
//!    directories, every output still `task-0`. `local.unique` exists for
//!    exactly this, and `solve::publish_context` had been setting
//!    `local.session` for the same reason all along.
//!
//! What settled both in one command was running the fixture with NO proxy on
//! ONE daemon. All four outputs were still `task-0`, so nothing about
//! dispatch was involved. Reaching for the baseline first turned a
//! "distributed builds return wrong bytes" panic into a fixture fix.
//!
//! # A Dockerfile build dispatches NOTHING, and cannot
//!
//! The claim "any project that speaks buildkit can use this" needed a client
//! that is not earthbuild and not a hand-written graph. `buildctl build
//! --frontend dockerfile.v0` is that client, and it dispatches zero percent:
//!
//! ```text
//! solve <id>: NO definition on the wire (frontend="")
//! gateway solve with no definition: frontend="dockerfile.v0" opts=["no-cache"]
//! placed {}   routed 0
//! ```
//!
//! This is structural, not a bug to fix. Naming a frontend asks the DAEMON to
//! resolve it; the daemon runs that frontend as its own gateway client, the
//! frontend generates LLB against the daemon's internal bridge, and none of it
//! crosses the proxy. There is no wire to cut because the graph is never on a
//! wire.
//!
//! So the product line is narrower and sharper than "any buildkit client":
//!
//! | client                              | dispatches |
//! | ----------------------------------- | ---------- |
//! | earthbuild (builds its own graph)   | yes        |
//! | `buildctl build < graph.llb`        | yes        |
//! | anything driving the gateway w/ LLB | yes        |
//! | `--frontend dockerfile.v0`          | no         |
//! | `docker build` / `buildx`           | no         |
//!
//! The rule is not about the tool, it is about WHERE the graph is built: a
//! client that constructs LLB itself can be distributed, and a client that
//! asks the daemon to construct it cannot. Making `docker build` work means
//! running the dockerfile frontend client-side, which is a change to the
//! client, not to this proxy.
//!
//! The proxy now says this rather than reporting "no definition" - a number
//! that is true and gives the reader nothing to do. Kept as a harness mode
//! (`DOCKERFILE=1`) so the message stays honest.
//!
//! Worth noting what is still untested: every LLB fixture here sources from
//! `docker-image://`, so `contexts published: 0` in every run, and the
//! context-publishing path has never actually run. The Dockerfile mode was
//! meant to exercise it and cannot, for the reason above.
//!
//! # Native multi-arch, and why a platform FILTER would have done nothing
//!
//! Placement ignored platform entirely, in a system whose stated first job is
//! native multi-arch. The obvious fix - ask each peer what platforms it
//! supports and filter - is a no-op, and the daemons say so plainly. A stock
//! buildkitd on an arm64 host:
//!
//! ```text
//! linux/arm64,linux/amd64,linux/amd64/v2,linux/riscv64,linux/ppc64le,...
//! ```
//!
//! and the same image forced to amd64:
//!
//! ```text
//! linux/amd64,linux/amd64/v2,linux/amd64/v3,linux/arm64,linux/riscv64,...
//! ```
//!
//! Every daemon claims nearly every platform, because binfmt will run
//! anything. "Supports linux/amd64" is answered YES by every peer in any
//! fleet. The worker's OWN architecture is the one it lists FIRST, and that
//! is the whole distinction: an emulated build is legal and five to ten times
//! slower.
//!
//! Three daemons, the last one `--platform linux/amd64`, six arm64 builds:
//!
//! ```text
//! peer 0 upstream native linux/arm64
//! peer 1 ...:18373 native linux/arm64
//! peer 2 ...:18374 native linux/amd64
//! placed {home: 2, peer1: 4}      wall 12s, outputs identical
//! ```
//!
//! The emulated peer took none of six, against two in a uniform fleet, and
//! the build paid no emulation penalty. Emulation is deprioritised rather
//! than refused: a fleet with no native peer still builds, slowly, and if
//! that turns out ruinous the take-back catches it.
//!
//! This also caught the harness measuring something other than it claimed.
//! `IMAGE` pinned a TAG, so the daemons' architecture was whatever was in the
//! local image cache - and pulling that tag once with `--platform
//! linux/amd64` leaves an amd64 image under it, after which every daemon runs
//! under QEMU. Docker says so in one warning line on stderr and nothing else
//! changes. The harness now pins and prints the architecture.
//!
//! # Remembering, and a control that nearly was not run
//!
//! Taking work back is reactive: without memory the fleet rediscovers the
//! slow machine once per solve, paying the bound every time. A peer taken
//! back from is struck, and a strike biases placement away from it.
//!
//! Three rounds of the same six builds through ONE proxy, and a uniform
//! fleet run as a control:
//!
//! ```text
//! slow peer  wall 39 10 10s   placed {home: 6, peer1: 10, peer2: 2}
//! uniform    wall 12 10 10s   placed {home: 6, peer1:  6, peer2: 6}
//! ```
//!
//! The wall clock is NOT the evidence, and reading it as such was the near
//! miss. "Round 2 dropped to 10s, so avoidance works" is wrong: the control
//! shows a uniform fleet also drops to 10s, because rounds 2 and 3 hit the
//! peers' own caches. 10s is the cached floor, reached either way.
//!
//! The evidence is the placement counts, which had to be added to see it:
//! peer 2 took 2 of 18 placements - its two cold round-1 solves - and nothing
//! afterwards, against 6 in the control. The inference from timing does hold
//! once stated properly (peer 2 is both slow AND still cold, so a round-2
//! placement there would have cost ~160s, not 10s), but an argument that
//! subtle is a reason to count the thing directly.
//!
//! A strike is a bias, not a ban: a struck peer still wins against peers
//! holding several jobs each, because a fleet that banned machines outright
//! would shrink itself on one bad minute. And when EVERY away peer is struck,
//! the work goes home - otherwise a two-daemon fleet with one bad peer would
//! offer, wait out the bound and take it back, on every single solve.
//!
//! Two counting bugs surfaced here, both of the same kind - a number that
//! blames the wrong thing. The take-back arrived at the caller as an `Err`
//! and was counted as "peer refused", accusing a machine of refusing work it
//! was still doing. And a baseline recorded with four builds compared against
//! a six-build run reported "outputs DIFFER" when builds 0-3 were identical
//! and 4-5 merely did not exist in it.
//!
//! # The bytes are the same, which nothing had checked
//!
//! Every measurement up to here read exit codes. A distributed buildkit that
//! returns the wrong bytes is worse than a slow one, and identity is what
//! buildkit matches on (principle 4) - a result one byte off a local build
//! poisons every cache downstream of it while every log line says success.
//!
//! `scripts/fleet.sh` now exports each result and hashes it. Baseline on one
//! daemon with no proxy, then the same four builds through the fleet:
//!
//! ```text
//! build 0: 7d2d122a...   build 1: e37f56da...
//! build 2: ccaca3f5...   build 3: 5f1f6a93...
//! outputs identical to base/digests.txt
//! ```
//!
//! Builds 2 and 3 were the dispatched ones: rewritten, mirrored, built on
//! another daemon, pushed to a registry, imported back, exported to the
//! client - and byte-identical to having built them at home.
//!
//! Getting a result out at all took two corrections worth keeping. Exporting
//! the ROOTFS to a local directory fails on `lchownat proc: permission
//! denied`, so the fixture writes to a scratch mount instead. And the rootfs
//! mount must still declare `output: 0` even though nothing wants it,
//! because on an LLB mount `output` also decides writability - with `-1`,
//! runc cannot create `/etc/resolv.conf` and the command never runs.
//!
//! # 100% dispatch, on a client that is not earthly
//!
//! Four plain-LLB builds through the proxy, two stock buildkitds:
//!
//! ```text
//! [wire] gateway solves : 4
//! [wire] solves routed  : 4 to other daemons
//! [wire] not routed     : {"considered": 4}
//! build 0..3 exit=0
//! peer 2 cache          : Total 13.63MB
//! ```
//!
//! Every solve dispatched. The 1-in-12 ceiling on earthly builds was
//! entirely the debugger plumbing described below - not a limit of the
//! mechanism, not a property of build graphs, and not the user's secrets.
//!
//! It also settles what the product is. A distributed BUILDKIT works today
//! for clients that send ordinary LLB; a distributed EARTHLY additionally
//! needs one upstream change. Those are different amounts of work and the
//! difference was invisible until a non-earthly client was tried.
//!
//! # What actually limits dispatch here: earthly's DEBUGGER
//!
//! The eleven excluded solves do not carry a user secret. They carry this,
//! and every earthly `RUN` carries it:
//!
//! ```text
//! mount /run/secrets/earthly_debugger_settings
//!   id=name=da39a3ee5e6b4b0d3255bfef95601890afd80709&org=&project=&v=1
//! ```
//!
//! `earthfile2llb/converter.go` attaches a debugger settings SECRET mount
//! and a `llb.HostBind()` mount for the debugger binary to every exec, and
//! the only guard is `if !opts.Locally`. Not `--interactive`: the
//! interactive flag decides whether to ERROR when the capability is
//! missing, not whether to attach the mounts. The id is a query string
//! whose org and project are empty and whose name is the sha1 of the empty
//! string - nothing is being protected here.
//!
//! So every earthly exec is pinned to one machine twice over, by a secret
//! and a host bind, in support of a debugger nobody asked for. The
//! exclusion is CORRECT - principle 10 does not get to make exceptions
//! about secrets - and the consequence is that essentially no earthly exec
//! can be dispatched as things stand.
//!
//! Three ways out, and the first is the honest one:
//!
//! - earthbuild omits the debugger plumbing when not debugging. A small
//!   upstream change with an obvious rationale, and the only one that does
//!   not weaken a safety rule or lie about a graph.
//! - the proxy strips known-inert frontend plumbing. Tempting and wrong by
//!   default: it changes the graph the client asked for, and "inert" is a
//!   judgement about someone else's mount.
//! - target clients that do not do this. buildx and dagger graphs carry no
//!   such plumbing, so they are dispatchable today - which is an argument
//!   for the product being a distributed BUILDKIT rather than a
//!   distributed earthly.
//!
//! This is also the honest cost of principle 15. "The client must not have
//! to change" is the right claim, and this client's own plumbing prevents
//! distribution - so for earthly, either earthbuild changes or nothing
//! moves.
//!
//! # What the secret exclusion previously looked like
//!
//! With the exclusion check finally wired into placement, the twelve
//! solves of a real earthly build resolve as:
//!
//! ```text
//! [wire] solves routed : 1 to other daemons
//! [wire] not routed    : {"considered": 12, "excluded: Secret": 11}
//! ```
//!
//! Eleven carry a SECRET, and principle 10 has always said what that means:
//! shipping the spec ships the secret reference, so the subtree does not
//! travel. They were never dispatchable. What was wrong was WHERE they were
//! refused - offered to a peer, which then failed with "no active sessions"
//! because a secret needs the session's service, and the error named the
//! session rather than the secret.
//!
//! The check for this was written early and simply never consulted on the
//! path that places work. Every diagnosis that followed - the fork, the
//! capability, the mirror, the auth - was chasing an error message that was
//! true and irrelevant.
//!
//! It also says something about the WORKLOAD rather than the mechanism: for
//! earthly builds, what limits dispatch is not contexts or base images,
//! both of which are now solved, but how much of the graph touches secrets.
//! That is worth measuring on a real repo before optimising anything else.
//!
//! # The bind a peer is currently in
//!
//! Twelve solves are offered and one completes. The other eleven fail on
//! the PEER, and which way they fail depends on which daemon it is:
//!
//! | peer | failure |
//! | ----------------------- | ------------------------------------ |
//! | `earthbuild/buildkitd` | `no active sessions` - it wants a session to export |
//! | `moby/buildkit` | `unknown API capability exec.mount.sock` |
//!
//! So a peer cannot be stock buildkit, because earthly's LLB uses a
//! capability only its fork declares; and the fork will not export without
//! a session, which is exactly what a peer does not have.
//!
//! The way out is to give the peer a session of OUR making - not the
//! client's. Buildkit does not need the client's credentials here, it needs
//! SOMEBODY to ask; an empty auth service satisfies it. That means serving
//! a session to the peer, which is the tunnelled gRPC server this proxy has
//! so far avoided implementing.
//!
//! The one solve that does complete is the one whose export finds
//! everything already local, so no auth is resolved. That is a warm-cache
//! success, and it is why the number was 1 rather than 0 - not evidence
//! that placement works better than it does.
//!
//! # Where this actually got to
//!
//! A real `earthly +all` on a two-daemon fleet, and the whole chain fired:
//! the context was published as content, the graph was made portable, the
//! work was OFFERED to a peer through its own `Control.Solve`, and when the
//! peer died the build fell back and succeeded.
//!
//! ```text
//! [proxy] peer 1 could not take it: peer solve: Unknown error transport error
//! =========================== Earth Build  SUCCESS ===========================
//! ```
//!
//! That failure is principle 5 doing its job under a real crash rather than
//! a simulated one: duplicate work is always correct, so a peer that dies
//! mid-offer costs a retry on the machine that was going to build it
//! anyway. The client saw a normal build.
//!
//! What still does not work is the PEER, not the dispatch. It panics
//! reaching for Docker Hub because a sessionless daemon has no registry
//! auth, and declaring our mirror as a `mirrors` entry for `docker.io` in
//! its config did not divert it. Until a peer can obtain base images with
//! no credentials, adoption offers work that no peer can complete - so the
//! fleet is correct, and idle.
//!
//! The remaining question is therefore plumbing rather than design: make
//! the mirror answer for `docker.io` in a way buildkit honours. Everything
//! above it - portability, publication, placement, adoption, ref affinity,
//! fail-open - is built and has run.
//!
//! # What "transparent" has to mean
//!
//! All nine methods, including the streams. `Session` in particular is
//! BIDIRECTIONAL and carries filesync and credentials — a proxy that
//! forwards Solve but not Session works for exactly the builds that need no
//! local context, which is not the workload we care about.

use std::pin::Pin;

use crate::gateway::frontend as gw;
use bollard_buildkit_proto::moby::buildkit::v1 as control;
use futures::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// Minimum subtree size worth reporting. Over half of every shard is
/// milliseconds of work, so a report that lists every single-op subtree
/// buries the ones that matter.
const MIN_CUT_OPS: usize = 4;

type Chan = tonic::transport::Channel;
type Client = control::control_client::ControlClient<Chan>;
type GwClient = gw::llb_bridge_client::LlbBridgeClient<Chan>;

/// Where a peer can pull content from, and which daemon to ask for it.
#[derive(Clone)]
pub struct Mirror {
    /// Address a PEER would use, e.g. `host.docker.internal:15000`.
    pub registry: String,
    /// The upstream daemon, which holds the client's session.
    pub buildkit: String,
}

/// One upstream daemon.
#[derive(Clone)]
pub struct Peer {
    /// Relative share of work, declared by the operator as `url*N`. Buildkit
    /// does not report capacity - `ListWorkers` gives platforms, snapshotter,
    /// executor and gc policy, and nothing about cores - so this is the only
    /// place it can come from.
    weight: usize,
    /// What this daemon says it can run, in ITS order - first is native.
    /// Empty if it would not say (a daemon that cannot answer ListWorkers is
    /// still usable; it is simply never preferred as native).
    platforms: Vec<String>,
    pub addr: String,
    channel: Chan,
}

#[derive(Clone)]
pub struct Proxy {
    /// ONE channel for the whole proxy, cloned per call.
    ///
    /// Not one per call, which is what this had first and is wrong in a way
    /// that only streams notice: the `Client` owns the channel, so returning
    /// from the handler drops it and the still-running stream dies. A unary
    /// call has its response already and never notices; `Session` dies
    /// mid-build, and the daemon reports it as "healthcheck failed ... EOF"
    /// with nothing pointing at a dropped connection. Measured, by doing it
    /// the other way first.
    client: Client,
    channel: Chan,
    pub wire: std::sync::Arc<std::sync::Mutex<Wire>>,
    /// Set to publish build contexts as content. Absent = observe only.
    pub mirror: Option<Mirror>,
    /// buildID -> session id.
    ///
    /// The two facts arrive on different calls and neither carries both.
    /// `Control.Solve` has the session in its BODY; the gateway solves that
    /// follow carry only `buildkit-controlapi-buildid` in their headers. So
    /// the session has to be remembered from the first and looked up by the
    /// second - which is also how buildkit itself associates them.
    sessions: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// (session, local name) -> published ref. A context is published ONCE
    /// per build, not once per gateway solve: it is content-addressed so a
    /// repeat is correct, but it is a full filesync and an image push for
    /// an answer we already have.
    /// One cell per thing-we-publish, so concurrent solves SHARE the work
    /// instead of racing or skipping it.
    ///
    /// This started as a plain map and was wrong twice, in opposite
    /// directions. First it was check-then-act, so eleven of twelve
    /// concurrent solves each redid the whole filesync and push. Then
    /// in-flight entries were SKIPPED, which made it publish once - and
    /// left every solve that skipped holding a graph that was still
    /// unportable, so it stayed home. One routed solve out of twelve.
    ///
    /// A `OnceCell` per key is the shape that is neither: the first caller
    /// publishes, the rest AWAIT the same result and then have it.
    published: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                (String, String),
                std::sync::Arc<tokio::sync::OnceCell<Option<String>>>,
            >,
        >,
    >,
    /// Extra daemons this proxy may route work to. Peer 0 is always the
    /// upstream above - the one holding the client's session.
    peers: std::sync::Arc<Vec<Peer>>,
    /// ref -> peer index.
    ///
    /// A gateway result is a REF and a ref is daemon-local. Measured on a
    /// real build: eleven `read_dir` calls follow eleven solves. So whatever
    /// minted a ref must serve every later call naming it, or the eleventh
    /// call of a working-looking build fails with "ref not found".
    ref_home: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    /// build id -> where its work went. Written by the gateway solve, read by
    /// `Control.Solve` when it finishes, because only the gateway knows the
    /// placement and only Control knows what the client actually waited.
    went: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Option<usize>>>>,
    /// What completed adoptions have cost, in ms. The basis for calling one
    /// slow - see `hedge_after`.
    adopted_ms: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    /// How many times each peer has been taken back from, indexed by peer.
    ///
    /// Never reset. Within one proxy's life a machine that was four times
    /// slower than the fleet stays four times slower; forgetting means
    /// rediscovering it once per solve, at the cost of the bound each time.
    strikes: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>>,
    /// Adoptions currently in flight on each AWAY peer, indexed by peer.
    ///
    /// Index 0 is unused and always zero: peer 0 never adopts. Kept aligned
    /// with `peers` so a peer index means the same thing everywhere - an
    /// off-by-one here would silently overload one machine and starve
    /// another, which looks like a slow fleet, not a bug.
    outstanding: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>>,
    /// Round-robin cursor for placing new solves.
    next_peer: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Proxy {
    pub async fn connect(upstream: String) -> anyhow::Result<Self> {
        let channel = tonic::transport::Endpoint::new(upstream)?.connect().await?;
        Ok(Proxy {
            client: control::control_client::ControlClient::new(channel.clone()),
            channel,
            wire: Default::default(),
            mirror: None,
            sessions: Default::default(),
            published: Default::default(),
            peers: Default::default(),
            adopted_ms: Default::default(),
            outstanding: Default::default(),
            strikes: Default::default(),
            went: Default::default(),
            next_peer: Default::default(),
            ref_home: Default::default(),
        })
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    /// Whose turn it is next, over the WHOLE fleet.
    /// `None` means build it here. See `place`.
    fn next_place(&self, want: &crate::dispatch::Platform) -> Option<usize> {
        let cursor = self
            .next_peer
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let read = |v: &[std::sync::atomic::AtomicUsize]| -> Vec<usize> {
            v.iter()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                .collect()
        };
        // An unpinned graph is NOT native everywhere, however much it looks
        // it. Measured across two real machines: an arm64 host mirrors
        // `alpine:3.20` and gets the arm64 image, because that is what
        // resolving it here means. The portable graph then names a
        // single-architecture base, and an x86 peer that pulls it dies with
        // `exit code: 255` - a binary it cannot run - after six seconds of
        // pulling. Fail-open recovered every build, so the only cost was time
        // and a frightening log line.
        //
        // That was true while the mirror held one architecture, and it is not
        // true now: the base is mirrored FOR THE PEER (`make_portable`'s
        // `target`), so an unpinned graph is native on every peer - each
        // builds for itself against a base fetched for itself. The fallback
        // to peer 0's architecture stays deleted rather than commented out;
        // the reason it existed is gone.
        let pinned = match want {
            crate::dispatch::Platform::Pinned(p) => Some(p.clone()),
            crate::dispatch::Platform::Any => None,
            crate::dispatch::Platform::Conflict(_) => None,
        };
        let native: Vec<bool> = match &pinned {
            Some(p) => self.peers[1..]
                .iter()
                .map(|peer| native_for(&peer.platforms, p))
                .collect(),
            None => vec![true; self.peers.len().saturating_sub(1)],
        };
        // Declared weights are the starting point; observation overrides them
        // once there is any. Deliberately not the other way round - a
        // declaration is a guess about hardware, and the guess measured
        // WORSE than a flat split.
        let mut weights: Vec<usize> = self.peers.iter().map(|p| p.weight).collect();
        // OFF by default. `REBUCK2_ADAPT=1` opts in.
        //
        // The law is right and this implementation of it is not. Service time
        // is ENDOGENOUS: the thing being measured is caused by the thing being
        // set. Load the home side and its mean rises, the ratio falls below
        // one, the controller reads that as "home is the straggler" and sends
        // MORE work away, which raises the away mean, and so on. Two runs of
        // the same three rounds:
        //
        // ```text
        // adapt   24 builds x3   18 14 14s   placed 40:32
        // adapt   same again     24 17 24s   placed 34:38   <- drifted away
        // pinned  control        18 15 16s   placed 36:36
        // pinned  control        18 15 16s   placed 36:36
        // ```
        //
        // The control also shows where most of the round-two gain comes from,
        // and it is not placement: the peers warm their own caches, and a
        // self-tuner left switched on would have taken the credit.
        let adapt = std::env::var("REBUCK2_ADAPT").as_deref() == Ok("1");
        if adapt && weights.len() == 2 {
            // Two-machine fleets only, for now: with more peers the balance
            // condition is a system of equations, not a ratio, and shipping
            // the two-peer arithmetic as though it generalised would be the
            // same error as guessing from cores.
            let w = self.wire.lock().expect("wire");
            let mean = |v: &[u64]| -> u64 {
                if v.len() < MIN_SAMPLES {
                    0
                } else {
                    v.iter().sum::<u64>() / v.len() as u64
                }
            };
            let (h, a) = (mean(&w.home_ms), mean(&w.away_ms));
            drop(w);
            if h > 0 && a > 0 {
                let (wh, wa) = derive_weights(h, a, WEIGHT_CAP);
                weights = vec![wh, wa];
            }
        }
        place(
            cursor,
            &weights,
            &read(&self.outstanding[1..]),
            &read(&self.strikes[1..]),
            &native,
        )
    }

    /// Wait for a peer to build it - but not forever, once the fleet has said
    /// what "forever" means.
    ///
    /// Measured: three daemons, six equal builds, one peer held to a quarter
    /// of a CPU. It got its fair share and the whole build waited 164s
    /// instead of 12s. No placement rule fixes that, because the placement
    /// was correct on the information available - the peer was idle. The
    /// information only arrives afterwards, when a normal adoption finishes
    /// and the straggler does not.
    ///
    /// So the deadline is re-read while waiting rather than fixed at the
    /// start: the first completed adoption anywhere in the fleet is what
    /// makes the straggler measurably abnormal. Abandoning it returns
    /// `Err`, and the caller's existing fail-open path then builds at home -
    /// principle 5. The peer is not cancelled and its push is not wasted if
    /// it lands: the tag is the digest of the graph, so whoever gets there
    /// first wins (principle 3) and a later identical push is a no-op.
    async fn adopt_or_take_back(
        &self,
        peer: usize,
        addr: &str,
        registry: &str,
        portable: &bollard_buildkit_proto::pb::Definition,
        started: std::time::Instant,
    ) -> Adoption {
        /// How often to re-ask whether this has become abnormal. Coarse on
        /// purpose: the answer changes on the scale of whole builds.
        const POLL: std::time::Duration = std::time::Duration::from_millis(250);
        let mut fut = Box::pin(crate::solve::build_and_publish(
            addr,
            registry,
            portable.clone(),
        ));
        loop {
            tokio::select! {
                r = &mut fut => return match r {
                    Ok(reference) => Adoption::Done(reference),
                    Err(e) => Adoption::Refused(e),
                },
                _ = tokio::time::sleep(POLL) => {
                    let observed = self.adopted_ms.lock().expect("adopted_ms").clone();
                    let Some(limit) = hedge_after(&observed) else { continue };
                    if started.elapsed() <= limit {
                        continue;
                    }
                    println!(
                        "[proxy] taking it back from peer {peer}: {:?} elapsed, normal is {:?}",
                        started.elapsed(),
                        limit
                    );
                    *self
                        .wire
                        .lock()
                        .expect("wire")
                        .rejected
                        .entry(format!("peer {peer} too slow"))
                        .or_default() += 1;
                    self.strikes[peer].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Adoption::TookBack;
                }
            }
        }
    }

    /// The gateway rides the same channel, because the client's does.
    fn gw(&self) -> GwClient {
        gw::llb_bridge_client::LlbBridgeClient::new(self.channel.clone())
    }

    /// Add daemons to route to. Peer 0 is always this proxy's upstream.
    pub async fn with_peers(mut self, addrs: &[String]) -> anyhow::Result<Self> {
        let mut peers = vec![Peer {
            // Peer 0's own share. `REBUCK2_HOME_WEIGHT` because it has no
            // `--peer` flag to carry a `*N`.
            weight: std::env::var("REBUCK2_HOME_WEIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            platforms: platforms_of(self.channel.clone()).await,
            addr: "upstream".into(),
            channel: self.channel.clone(),
        }];
        for a in addrs {
            // `http://host:port*2` - twice the share. Split from the RIGHT
            // because a URL will not contain `*`.
            let (url, weight) = match a.rsplit_once('*') {
                Some((u, w)) => (u.to_owned(), w.parse().unwrap_or(1usize)),
                None => (a.clone(), 1usize),
            };
            let channel = tonic::transport::Endpoint::new(url.clone())?
                .connect()
                .await?;
            peers.push(Peer {
                weight: weight.max(1),
                platforms: platforms_of(channel.clone()).await,
                addr: url,
                channel,
            });
        }
        for (i, p) in peers.iter().enumerate() {
            println!(
                "[proxy] peer {i} {} native {} weight {}",
                p.addr,
                p.platforms.first().map(String::as_str).unwrap_or("unknown"),
                p.weight
            );
        }
        println!("[proxy] {} daemon(s) in the fleet", peers.len());
        self.outstanding = std::sync::Arc::new(
            (0..peers.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        );
        self.strikes = std::sync::Arc::new(
            (0..peers.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        );
        self.peers = std::sync::Arc::new(peers);
        Ok(self)
    }

    fn gw_of(&self, i: usize) -> GwClient {
        match self.peers.get(i) {
            Some(p) => gw::llb_bridge_client::LlbBridgeClient::new(p.channel.clone()),
            None => self.gw(),
        }
    }

    /// Where a ref lives. Unknown refs go to peer 0, which is where every
    /// call went before there was a fleet.
    fn home_of(&self, r: &str) -> usize {
        self.ref_home
            .lock()
            .expect("ref_home")
            .get(r)
            .copied()
            .unwrap_or(0)
    }

    /// Remember which daemon minted the refs in a result.
    fn remember(&self, result: &Option<gw::Result>, peer: usize) {
        let Some(inner) = result.as_ref().and_then(|r| r.result.as_ref()) else {
            return;
        };
        let mut map = self.ref_home.lock().expect("ref_home");
        match inner {
            gw::result::Result::Ref(r) => {
                map.insert(r.id.clone(), peer);
            }
            gw::result::Result::Refs(m) => {
                for r in m.refs.values() {
                    map.insert(r.id.clone(), peer);
                }
            }
            _ => {}
        }
    }
}

/// Re-wrap a payload in a Request carrying the ORIGINAL metadata.
///
/// Load-bearing, and its absence is invisible until it is not: buildkit
/// associates a session by headers on the request (the session UUID among
/// them). Forward the stream without them and the daemon accepts the
/// session, cannot match it to the build, and the frontend's filesync call
/// comes back Unimplemented - an error that names nothing to do with
/// metadata. Measured, by breaking it.
fn relay<T, U>(from: Request<T>, payload: U) -> Request<U> {
    let (meta, ext, _) = from.into_parts();
    Request::from_parts(meta, ext, payload)
}

/// Say what this graph offers a dispatcher, and nothing else.
fn report(req: &control::SolveRequest) {
    let Some(def) = &req.definition else {
        println!(
            "[proxy] solve {}: NO definition on the wire (frontend={:?}) - the \
             LLB is generated inside the daemon, not sent to it",
            req.r#ref, req.frontend
        );
        return;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    let biggest = free.first().map(|c| c.ops).unwrap_or(0);
    println!(
        "[proxy] solve {}: {} ops, {} cuts >= {MIN_CUT_OPS}, {} with a free \
         frontier, biggest {biggest} ops",
        req.r#ref,
        a.ops,
        a.cuts.len(),
        free.len(),
    );
}

#[tonic::async_trait]
impl control::control_server::Control for Proxy {
    async fn solve(
        &self,
        request: Request<control::SolveRequest>,
    ) -> Result<Response<control::SolveResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        report(&req);

        // Remember the session against this build, for the gateway solves
        // that follow. Only this call knows both.
        // The build id is the `ref` FIELD here, and arrives as the
        // `buildkit-controlapi-buildid` HEADER on the gateway solves that
        // follow. Same value, different place - looking for the header on
        // this call finds nothing.
        if !req.r#ref.is_empty() && !req.session.is_empty() {
            self.sessions
                .lock()
                .expect("sessions")
                .insert(req.r#ref.clone(), req.session.clone());
        }
        // Kept before the request is consumed: the client blocks on this call,
        // and the gateway solve that chose a peer names the same build id.
        let build_id = req.r#ref.clone();
        // THIS is where a build's time actually is. Both cheaper
        // candidates were tried and are near-zero: a gateway Solve returns a
        // ref in ~1ms, and `return` merely registers it. The client blocks
        // on the Control.Solve response, so timing it is the only way the
        // proxy sees a build's duration at all.
        let t = std::time::Instant::now();
        let out = self
            .client()
            .solve(Request::from_parts(meta, ext, req))
            .await;
        let ms = t.elapsed().as_millis() as u64;
        let went = self.went.lock().expect("went").get(&build_id).copied();
        let mut w = self.wire.lock().expect("wire");
        w.control_solves.push(ms);
        match went {
            Some(Some(_)) => w.away_ms.push(ms),
            Some(None) => w.home_ms.push(ms),
            // Never placed - excluded, no fleet, no mirror. Neither bucket.
            None => {}
        }
        out
    }

    async fn disk_usage(
        &self,
        request: Request<control::DiskUsageRequest>,
    ) -> Result<Response<control::DiskUsageResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.client()
            .disk_usage(Request::from_parts(meta, ext, req))
            .await
    }

    async fn list_workers(
        &self,
        request: Request<control::ListWorkersRequest>,
    ) -> Result<Response<control::ListWorkersResponse>, Status> {
        self.client().list_workers(request.into_inner()).await
    }

    async fn info(
        &self,
        request: Request<control::InfoRequest>,
    ) -> Result<Response<control::InfoResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.client()
            .info(Request::from_parts(meta, ext, req))
            .await
    }

    async fn update_build_history(
        &self,
        request: Request<control::UpdateBuildHistoryRequest>,
    ) -> Result<Response<control::UpdateBuildHistoryResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.client()
            .update_build_history(Request::from_parts(meta, ext, req))
            .await
    }

    type PruneStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::UsageRecord, Status>> + Send>>;

    async fn prune(
        &self,
        request: Request<control::PruneRequest>,
    ) -> Result<Response<Self::PruneStream>, Status> {
        let (meta, ext, req) = request.into_parts();
        let s = self
            .client()
            .prune(Request::from_parts(meta, ext, req))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }

    type StatusStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::StatusResponse, Status>> + Send>>;

    async fn status(
        &self,
        request: Request<control::StatusRequest>,
    ) -> Result<Response<Self::StatusStream>, Status> {
        let (meta, ext, req) = request.into_parts();
        let s = self
            .client()
            .status(Request::from_parts(meta, ext, req))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }

    type ListenBuildHistoryStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::BuildHistoryEvent, Status>> + Send>>;

    async fn listen_build_history(
        &self,
        request: Request<control::BuildHistoryRequest>,
    ) -> Result<Response<Self::ListenBuildHistoryStream>, Status> {
        let (meta, ext, req) = request.into_parts();
        let s = self
            .client()
            .listen_build_history(Request::from_parts(meta, ext, req))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }

    type SessionStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::BytesMessage, Status>> + Send>>;

    /// The bidirectional one, and the reason a Solve-only proxy is not
    /// enough: this carries filesync and registry credentials. Errors on
    /// the inbound half are dropped rather than forwarded, because a
    /// half-open session is what the daemon sees when a client goes away,
    /// and it already knows what to do about that.
    async fn session(
        &self,
        request: Request<Streaming<control::BytesMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        use std::sync::atomic::Ordering;
        let (meta, ext, stream) = request.into_parts();
        let up = self.wire.clone();
        let inbound = stream.filter_map(move |m| {
            if let Ok(msg) = &m {
                up.lock()
                    .expect("wire")
                    .session_to_daemon
                    .fetch_add(msg.data.len() as u64, Ordering::Relaxed);
            }
            futures::future::ready(m.ok())
        });
        let s = self
            .client()
            .session(Request::from_parts(meta, ext, inbound))
            .await?;
        let down = self.wire.clone();
        let out = s.into_inner().map(move |m| {
            if let Ok(msg) = &m {
                down.lock()
                    .expect("wire")
                    .session_to_client
                    .fetch_add(msg.data.len() as u64, Ordering::Relaxed);
            }
            m
        });
        Ok(Response::new(Box::pin(out)))
    }
}

/// Every gateway call, in order.
///
/// Before routing solves to different daemons, the question that decides
/// whether that is even possible: a gateway result is a REF, and a ref is
/// daemon-local. If the client only ever asks "did it work", refs never
/// leave the daemon that made them and routing is free. If it reads files
/// from them, or hands one solve's ref to another, then a ref that lives on
/// the wrong machine is a broken build.
fn trace(wire: &std::sync::Mutex<Wire>, call: &str) {
    wire.lock().expect("wire").calls.push(call.to_owned());
}

/// Round-robin over a fleet of `peers`, peer 0 included.
///
/// This was `1 + cursor % (peers - 1)`, which excluded peer 0 from
/// dispatched work entirely. The reasoning behind that was sound and the
/// conclusion was not: peer 0 does hold the client's job and must answer
/// the gateway Solve, so adopting onto peer 0 means building there, pushing
/// to the mirror and importing back to the same daemon - a round-trip for
/// nothing. But skipping peer 0 turns a two-machine fleet into OFFLOADING:
/// one machine builds, the other shuffles bytes, and half the grid is idle
/// by construction. Principle 1 says the grid is one machine; a machine
/// does not retire a core to hold the paperwork.
///
/// Peer 0 takes its turn and the graph is built in place, which is the
/// round-trip skipped rather than paid.
/// How many completed builds before a side's mean means anything.
const MIN_SAMPLES: usize = 2;
/// Largest weight `derive_weights` will hand out. Bounds how blocky `turn`'s
/// rotation can get.
const WEIGHT_CAP: usize = 8;

/// Turn two mean service times into a pair of small weights.
///
/// The control law, and it is measured rather than reasoned: at the split
/// that minimises wall clock, the two sides FINISH TOGETHER. Twenty-four
/// builds across two machines -
///
/// ```text
/// 12:12  wall 18s   home 11425ms  away 17235ms  ratio 1.51
/// 16: 8  wall 15s   home 15228ms  away 15506ms  ratio 1.02
/// ```
///
/// - so a ratio above one says the away side is the straggler and the share
/// should move home, and the balance point is ratio 1. Shares therefore go as
/// the INVERSE of service time, which needs no core counts and no declared
/// capacity: an operator guessing from cores got 21s, and this arithmetic on
/// the 12:12 numbers gives 3:2, which is next to the 16:8 that measured best.
///
/// Small integers, because `turn` hands out contiguous blocks: weights of
/// 15228 and 15506 would send fifteen thousand consecutive solves to one peer
/// before the other saw a single one. `cap` bounds the denominator, and the
/// approximation is the best rational within it rather than a truncation -
/// 1.51 must become 3:2, not 1:1.
fn derive_weights(home_ms: u64, away_ms: u64, cap: usize) -> (usize, usize) {
    if home_ms == 0 || away_ms == 0 {
        return (1, 1);
    }
    // home:away = away_ms:home_ms - the side that takes longer gets fewer.
    let ratio = away_ms as f64 / home_ms as f64;
    let (mut best, mut err) = ((1usize, 1usize), f64::INFINITY);
    for d in 1..=cap {
        let n = ((ratio * d as f64).round() as usize).clamp(1, cap);
        let e = (ratio - n as f64 / d as f64).abs();
        if e < err {
            err = e;
            best = (n, d);
        }
    }
    best
}

fn turn(cursor: usize, weights: &[usize]) -> usize {
    debug_assert!(!weights.is_empty(), "a fleet with no peers has no turns");
    // Weighted, because machines are not interchangeable and buildkit will
    // not say so: `ListWorkers` reports platforms, snapshotter, executor and
    // gc policy, and nothing at all about how many cores are behind them.
    // Measured on two real machines - a flat split sent half the work to a
    // 32-core box and half to a 16-core one, which is the visible waste in
    // the 24s -> 18s result.
    //
    // A weight of 2 means "twice as many turns", nothing more precise. That
    // is principle 13: a coarse estimate an operator can state is worth more
    // than a exact one nobody can obtain.
    let total: usize = weights.iter().sum();
    if total == 0 {
        return 0;
    }
    let mut at = cursor % total;
    for (i, &w) in weights.iter().enumerate() {
        if at < w {
            return i;
        }
        at -= w;
    }
    0
}

/// How an attempt to place a solve on a peer ended.
///
/// This was `Option<Result<String>>`, and the take-back arrived as an `Err` -
/// so a solve we withdrew ourselves was counted as "peer refused", blaming a
/// machine for something it did not do. Three outcomes need three names.
enum Adoption {
    /// The peer built it and published it under this reference.
    Done(String),
    /// The peer was asked and could not.
    Refused(anyhow::Error),
    /// We stopped waiting. Already counted; the peer may still finish.
    TookBack,
    /// Never offered - the graph was not portable.
    NotOffered,
}

/// A straggler is only abnormal relative to something.
///
/// Three times the median of what adoptions have actually cost, floored so a
/// fleet of half-second subtrees is not hedged on jitter. `None` means no
/// adoption has completed yet and there is no basis for calling anything
/// slow - which is deliberately today's behaviour: wait.
///
/// This exists because a fixed cold default would be a number invented to
/// make one measurement look good. The 0.25-CPU peer took 163s against a
/// normal 9.5s; any threshold between the two "works", and picking one before
/// the fleet has said what normal is means picking it for the fixture.
fn hedge_after(observed: &[u64]) -> Option<std::time::Duration> {
    /// Not two: a peer that is merely on the slow side of normal should
    /// finish, not be abandoned with the work half done (principle 12 -
    /// finishing beats starting).
    const FACTOR: u32 = 3;
    /// Below this, a "straggler" is scheduling noise.
    const FLOOR: std::time::Duration = std::time::Duration::from_secs(5);
    if observed.is_empty() {
        return None;
    }
    let mut v = observed.to_vec();
    v.sort_unstable();
    let median = v[v.len() / 2];
    Some(std::cmp::max(
        FLOOR,
        std::time::Duration::from_millis(median) * FACTOR,
    ))
}

/// Ask a daemon what it can run, in its own order.
///
/// A daemon that will not answer is not excluded - it is simply never
/// preferred as native. Failing closed here would turn one unhealthy
/// ListWorkers into a fleet of one (principle 5).
async fn platforms_of(channel: Chan) -> Vec<String> {
    match control::control_client::ControlClient::new(channel)
        .list_workers(control::ListWorkersRequest::default())
        .await
    {
        Ok(r) => r
            .into_inner()
            .record
            .first()
            .map(|w| {
                w.platforms
                    .iter()
                    .map(|p| format!("{}/{}", p.os, p.architecture))
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            println!("[proxy] a daemon would not list workers ({e}); never preferred as native");
            Vec::new()
        }
    }
}

/// Whether a peer can build this platform NATIVELY.
///
/// Measured, and it is the whole reason platform filtering is not a one-line
/// set membership test. A stock buildkitd on an arm64 host reports:
///
/// ```text
/// linux/arm64,linux/amd64,linux/amd64/v2,linux/riscv64,linux/ppc64le,...
/// ```
///
/// and the same image forced to amd64 reports:
///
/// ```text
/// linux/amd64,linux/amd64/v2,linux/amd64/v3,linux/arm64,linux/riscv64,...
/// ```
///
/// Every daemon claims nearly every platform, because binfmt/QEMU will run
/// anything. So "does this peer support linux/amd64" is answered YES by every
/// peer in any fleet and filtering on it is a no-op. The worker's OWN
/// architecture is the one it lists FIRST, and that is the distinction native
/// multi-arch is about: an emulated build is legal and five to ten times
/// slower.
///
/// Emulation is not refused here - a fleet with no native peer should still
/// build. It is deprioritised, and if it turns out ruinous the existing
/// take-back catches it.
fn native_for(worker_platforms: &[String], want: &str) -> bool {
    worker_platforms.first().is_some_and(|p| p == want)
}

/// Where this solve goes: `None` means home.
///
/// Three inputs, because each answers something the others cannot. `cursor`
/// keeps peer 0 in the rotation (its own occupancy is unobservable from here).
/// `load` is exact in-flight adoptions. `strikes` is memory: a peer taken back
/// for being abnormally slow is not merely busy, and without memory the fleet
/// rediscovers that fact once per solve.
///
/// `load` and `strikes` are indexed over AWAY peers only - peer 0 is neither
/// counted nor struck - so the returned index is offset back into fleet space
/// by the caller.
fn place(
    cursor: usize,
    // One per peer INCLUDING peer 0, so home takes a share proportional to
    // its own capacity rather than a flat 1/n.
    weights: &[usize],
    load: &[usize],
    strikes: &[usize],
    // `true` where the away peer can build the wanted platform natively.
    // All-true when the graph pins no platform, which is the common case.
    native: &[bool],
) -> Option<usize> {
    /// A struck peer is treated as though it already holds this many jobs.
    ///
    /// Deliberately the same number as `hedge_after`'s factor: the peer
    /// exceeded three times normal, so it is discounted by three times a job.
    /// This is a bias, not a ban - a peer with one strike still wins against
    /// peers holding four jobs each, and a fleet that banned machines
    /// outright would shrink itself on one bad minute.
    const STRIKE_WEIGHT: usize = 3;
    if weights.len() <= 1 || turn(cursor, weights) == 0 {
        return None;
    }
    debug_assert_eq!(load.len(), strikes.len(), "away peers counted twice over");
    debug_assert_eq!(load.len(), native.len(), "away peers counted twice over");
    // Every away peer has misbehaved: home is the fail-open answer
    // (principle 5). Without this a two-daemon fleet whose only peer is bad
    // would offer to it, wait out the bound, and take it back - every solve.
    if !strikes.is_empty() && strikes.iter().all(|&s| s > 0) {
        return None;
    }
    // No away peer can run this natively: home, not "slowly somewhere".
    //
    // This started as a BIAS, on the reasoning that emulation is legal and
    // merely five to ten times slower. Two real machines refuted it. The base
    // image in the mirror is single-architecture - an arm64 host resolving
    // `alpine:3.20` mirrors the arm64 image - so a peer of another
    // architecture does not run it slowly, it fails with `exit code: 255`
    // after pulling. Emulation would need the mirror to carry a manifest
    // LIST, which it does not yet.
    if !native.is_empty() && !native.iter().any(|&n| n) {
        return None;
    }
    /// What an emulated peer is discounted by. Larger than STRIKE_WEIGHT
    /// because emulation is a known 5-10x, where a strike is one bad
    /// observation - but finite, so a fleet with no native peer still builds
    /// rather than refusing work it can do slowly.
    const EMULATED_WEIGHT: usize = 8;
    /// Fixed-point, so load can be divided by capacity in integers: four
    /// jobs on a weight-2 machine scores the same as two on a weight-1.
    const SCALE: usize = 1024;
    let effective: Vec<usize> = load
        .iter()
        .zip(strikes)
        .zip(native)
        .zip(&weights[1..])
        .map(|(((&l, &s), &n), &w)| {
            // Load is normalised by capacity; the penalties are NOT. A strike
            // says the machine misbehaved and a foreign architecture says it
            // must emulate - neither is something a bigger machine absorbs.
            l * SCALE / w.max(1) + (s * STRIKE_WEIGHT + usize::from(!n) * EMULATED_WEIGHT) * SCALE
        })
        .collect();
    Some(1 + least_loaded(&effective, cursor))
}

/// Which away peer, given how much each is already holding.
///
/// `turn` decides home-or-away and this decides WHICH away, because the two
/// questions have different amounts of information behind them. Outstanding
/// adoptions are known exactly - the future is held across
/// `build_and_publish`, so the count is incremented before and decremented
/// after. Peer 0's occupancy is not knowable from here at all: its work is
/// done by the daemon being proxied and its gateway solve returns lazily in
/// about a millisecond, so counting it would show peer 0 permanently idle and
/// hand it everything.
///
/// The scan starts at `cursor` so that EQUAL loads round-robin instead of
/// piling on the lowest index - which is also why this degrades to exactly
/// `turn`'s behaviour when every peer is idle, and only diverges when one
/// genuinely is busier. Never worse than round robin, better whenever solves
/// arrive spread out over a build rather than all at once.
fn least_loaded(load: &[usize], cursor: usize) -> usize {
    debug_assert!(!load.is_empty(), "no away peers to choose between");
    let n = load.len().max(1);
    // `min_by_key` keeps the FIRST minimum, and the scan starts at the
    // cursor, so a tie goes to the peer after the last one picked.
    (0..n)
        .map(|i| (cursor + i) % n)
        .min_by_key(|&i| load.get(i).copied().unwrap_or(usize::MAX))
        .unwrap_or(0)
}

/// One gateway Solve, characterised.
///
/// Deliberately NOT "how many cuts could mechanism A take" - that prices one
/// design and asking it first is how you measure the wrong thing
/// convincingly. This describes the WORKLOAD, which prices every candidate
/// at once: how many Solves a build makes and how big each is (does routing
/// whole Solves have enough to route?), how much is `local://` (that part is
/// going nowhere whatever we build), and what platforms appear (is there
/// native multi-arch work here at all?).
/// Where one gateway solve spent its time, in ms.
///
/// `total` is the whole call as the client experienced it. The three parts
/// do NOT sum to it and are not meant to.
///
/// `answer` is measured and is almost always 1ms, which is not a mistake:
/// a gateway Solve is LAZY. It returns a ref, and the ref is evaluated on
/// `return` - so a nine-second build and a registry pull are both a
/// millisecond here. Look at `Wire::returns` for the other half. This was
/// nearly read as "answering is free", which would have sent the next
/// iteration optimising the wrong end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub total: u64,
    /// Rewriting the graph: publishing the context, mirroring base images.
    pub portable: u64,
    /// The peer building it and pushing the result.
    pub adopt: u64,
    /// Peer 0 answering - after adoption this is a PULL, not a build.
    pub answer: u64,
}

impl Span {
    /// What dispatch cost that a local build would not have paid.
    ///
    /// Not `total - adopt`: the peer's build replaces work peer 0 would have
    /// done anyway, so charging it as overhead double-counts the build and
    /// makes dispatch look catastrophic. Only preparation and the answer are
    /// new - and since the answer is lazy, in practice this is `portable`.
    pub fn tax(&self) -> u64 {
        self.portable + self.answer
    }
}

#[derive(Default)]
pub struct Wire {
    pub solves: u64,
    pub ops: u64,
    pub registry_sources: u64,
    pub local_sources: u64,
    pub other_sources: u64,
    pub platforms: std::collections::BTreeSet<String>,
    /// Ops per Solve, in arrival order - the balance question.
    pub per_solve: Vec<usize>,
    /// Graph digests seen, to measure how much two Solves share.
    seen_ops: std::collections::BTreeSet<String>,
    pub repeated_ops: u64,
    /// Digest of each Solve's whole op set, in order.
    ///
    /// Repetition means two very different things and the summary number
    /// cannot tell them apart: a client RE-SENDING an identical graph is an
    /// artefact of how it drives the API, while two graphs that genuinely
    /// share a prefix is shared work a fleet could exploit. Counting
    /// identical resends separately is the difference between a finding and
    /// a misreading.
    graph_ids: Vec<String>,
    /// Per solve: how many of its ops were already seen.
    pub overlap_per_solve: Vec<(usize, usize)>,
    /// Bytes relayed over the SESSION, both ways.
    ///
    /// This is the number that decides whether a coordinator can honour
    /// principle 6. Layers travel peer to peer, but the build CONTEXT has
    /// exactly one holder - the client - and it reaches a builder over the
    /// session we are proxying. If that is kilobytes it is a rounding error;
    /// if it is the repository it is the coordinator back on the data path.
    pub session_to_daemon: std::sync::atomic::AtomicU64,
    pub session_to_client: std::sync::atomic::AtomicU64,
    /// Build contexts turned into content a peer can pull.
    pub contexts_published: u64,
    /// Gateway calls in arrival order.
    pub calls: Vec<String>,
    /// Solves placed on a peer other than the upstream.
    pub routed: u64,
    /// Client-visible build times, split by where the work went.
    ///
    /// The sweep says the best split is 2:1 toward home, and a weight can
    /// only be derived from measurement if the ratio the sweep implies is
    /// actually observable. These two are what would have to predict it.
    pub home_ms: Vec<u64>,
    pub away_ms: Vec<u64>,
    /// How long each `Control.Solve` took - the call the CLIENT blocks on,
    /// and so the only honest measure of a build's duration from here.
    pub control_solves: Vec<u64>,
    /// How long each `return` took.
    ///
    /// Measured on the theory that a lazy gateway solve's work lands here.
    /// It does not - these come back in about a millisecond too. Kept
    /// because the zero is the finding: neither gateway call is where the
    /// time is, so the next person does not have to re-measure it.
    pub returns: Vec<u64>,
    /// Where each solve was placed: key 0 is home, 1.. are peers.
    ///
    /// Added because "round 2 was fast" is not evidence of avoidance - a
    /// control run showed a uniform fleet reaching the same wall clock purely
    /// from the peers' own caches. Which machine got what has to be counted,
    /// not inferred from a timing.
    pub placed: std::collections::BTreeMap<usize, u64>,
    /// Per-solve timing, in arrival order.
    pub spans: Vec<Span>,
    /// Solves whose turn fell to peer 0 and were built where they already
    /// were. Not a rejection: peer 0 is a machine like any other, and
    /// counting its share as "not routed" understates a fleet by exactly
    /// 1/n - a perfectly-balanced pair would report 50% dispatch.
    pub home: u64,
    /// Why a solve was NOT placed, counted.
    ///
    /// One routed solve out of twelve is either a fleet barely working or a
    /// fleet barely used, and the difference is not visible from the
    /// outside. Counting the reason is what turns "improve routing" into a
    /// specific thing to fix.
    pub rejected: std::collections::BTreeMap<String, u64>,
}

impl Wire {
    fn observe(&mut self, def: &bollard_buildkit_proto::pb::Definition) {
        use prost::Message;
        self.solves += 1;
        self.ops += def.def.len() as u64;
        self.per_solve.push(def.def.len());
        let mut ids: Vec<String> = def
            .def
            .iter()
            .map(|b| crate::store::sha256_hex(b))
            .collect();
        ids.sort();
        self.graph_ids
            .push(crate::store::sha256_hex(ids.join("").as_bytes()));
        let mut already = 0usize;
        for bytes in &def.def {
            let digest = crate::store::sha256_hex(bytes);
            if !self.seen_ops.insert(digest) {
                // The same op in two Solves. High overlap means routing
                // whole Solves duplicates work that dispatch would share.
                self.repeated_ops += 1;
                already += 1;
            }
            let Ok(op) = bollard_buildkit_proto::pb::Op::decode(bytes.as_slice()) else {
                continue;
            };
            if let Some(p) = &op.platform {
                self.platforms
                    .insert(format!("{}/{}", p.os, p.architecture));
            }
            if let Some(bollard_buildkit_proto::pb::op::Op::Source(src)) = &op.op {
                match src.identifier.split_once("://").map(|(s, _)| s) {
                    Some("docker-image") => self.registry_sources += 1,
                    Some("local") => self.local_sources += 1,
                    _ => self.other_sources += 1,
                }
            }
        }
        self.overlap_per_solve.push((already, def.def.len()));
    }

    /// The characterisation, as one block. Printed on shutdown because the
    /// interesting numbers are about the BUILD, not any one Solve.
    pub fn report(&self) {
        let mut sizes = self.per_solve.clone();
        sizes.sort_unstable();
        let median = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
        println!("[wire] ---- what this build looked like ----");
        println!("[wire] gateway solves : {}", self.solves);
        println!("[wire] ops total      : {}", self.ops);
        println!(
            "[wire] ops per solve  : min {} median {} max {}",
            sizes.first().copied().unwrap_or(0),
            median,
            sizes.last().copied().unwrap_or(0),
        );
        println!(
            "[wire] sources        : {} registry, {} local, {} other",
            self.registry_sources, self.local_sources, self.other_sources
        );
        println!("[wire] platforms      : {:?}", self.platforms);
        println!(
            "[wire] repeated ops   : {} ({}% of all ops seen again in a later solve)",
            self.repeated_ops,
            if self.ops > 0 {
                self.repeated_ops * 100 / self.ops
            } else {
                0
            }
        );

        // The distinction that decides what the repetition MEANS. A client
        // RE-SENDING an identical graph is an artefact of how it drives the
        // API; two graphs sharing a prefix is work a fleet could share. The
        // summary percentage cannot tell them apart, and reading one as the
        // other is how a measurement becomes a wrong conclusion.
        let mut uniq = self.graph_ids.clone();
        uniq.sort();
        uniq.dedup();
        let resends = self.graph_ids.len().saturating_sub(uniq.len());
        println!(
            "[wire] distinct graphs: {} of {} solves ({resends} identical RESENDS)",
            uniq.len(),
            self.graph_ids.len(),
        );
        println!(
            "[wire] overlap/solve  : {:?} (already-seen / total)",
            self.overlap_per_solve
        );
        let up = self
            .session_to_daemon
            .load(std::sync::atomic::Ordering::Relaxed);
        let down = self
            .session_to_client
            .load(std::sync::atomic::Ordering::Relaxed);
        println!("[wire] contexts published: {}", self.contexts_published);
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        for c in &self.calls {
            *counts.entry(c.as_str()).or_default() += 1;
        }
        println!("[wire] gateway calls  : {counts:?}");
        println!("[wire] solves routed  : {} to other daemons", self.routed);
        println!("[wire] built at home  : {} (peer 0's own share)", self.home);
        println!("[wire] placed         : {:?} (0 = home)", self.placed);
        let mean = |v: &[u64]| -> u64 {
            if v.is_empty() {
                0
            } else {
                v.iter().sum::<u64>() / v.len() as u64
            }
        };
        let (h, a) = (mean(&self.home_ms), mean(&self.away_ms));
        println!(
            "[wire] service ms     : home {h} ({}) away {a} ({}) ratio {:.2}",
            self.home_ms.len(),
            self.away_ms.len(),
            if h == 0 { 0.0 } else { a as f64 / h as f64 }
        );
        for (i, s) in self.spans.iter().enumerate() {
            println!(
                "[wire] solve {i} ms     : total {} = portable {} + peer {} + answer {} \
                 (tax {})",
                s.total,
                s.portable,
                s.adopt,
                s.answer,
                s.tax()
            );
        }
        println!(
            "[wire] build ms       : {:?} (Control.Solve - what the client waits for)",
            self.control_solves
        );
        println!(
            "[wire] return ms      : {:?} (near-zero; not where work lands)",
            self.returns
        );
        println!("[wire] not routed     : {:?}", self.rejected);
        println!("[wire] call order     : {}", self.calls.join(" "));
        println!(
            "[wire] session bytes  : {} KiB client->daemon, {} KiB daemon->client",
            up / 1024,
            down / 1024
        );
        if resends > 0 {
            println!(
                "[wire] NOTE: {resends} solve(s) re-sent a graph already seen - that is the \
                 client driving the API, not shared work."
            );
        }
    }
}

impl Proxy {
    /// The cell for a key, created if absent.
    fn cell(
        &self,
        key: &(String, String),
    ) -> std::sync::Arc<tokio::sync::OnceCell<Option<String>>> {
        self.published
            .lock()
            .expect("published")
            .entry(key.clone())
            .or_default()
            .clone()
    }

    /// What a key resolved to, if it has resolved and succeeded.
    fn resolved(&self, key: &(String, String)) -> Option<String> {
        self.published
            .lock()
            .expect("published")
            .get(key)
            .and_then(|c| c.get().cloned().flatten())
    }

    /// The session behind this gateway call, if we learned one.
    fn session_for(&self, meta: &tonic::metadata::MetadataMap) -> String {
        meta.get("buildkit-controlapi-buildid")
            .and_then(|v| v.to_str().ok())
            .and_then(|b| self.sessions.lock().expect("sessions").get(b).cloned())
            .unwrap_or_default()
    }

    /// Swap `local://` sources for the contexts we published, so the graph
    /// depends on content rather than on one machine's disk.
    async fn make_portable(
        &self,
        def: &bollard_buildkit_proto::pb::Definition,
        session: &str,
        mirror: &Mirror,
        // The architecture the graph is going TO. Base images are mirrored
        // for it, not for ours - see `solve::mirror_image`.
        target: Option<&str>,
    ) -> bollard_buildkit_proto::pb::Definition {
        let out = crate::dispatch::rewrite_local_sources(def, &|name| {
            // Not published: leave it alone. The graph stays pinned to peer
            // 0, which is correct, rather than pointing a peer at content
            // nobody has.
            self.resolved(&(session.to_owned(), name.to_owned()))
        });

        // And the BASE images. A sessionless peer has no registry auth, so
        // `docker-image://docker.io/...` is as unreachable for it as the
        // client's disk. Copy each through peer 0, which does have
        // credentials, and point the graph at the copy.
        let mut refs: std::collections::BTreeSet<String> = Default::default();
        for bytes in &out.def {
            use prost::Message;
            if let Ok(op) = bollard_buildkit_proto::pb::Op::decode(bytes.as_slice()) {
                if let Some(bollard_buildkit_proto::pb::op::Op::Source(src)) = &op.op {
                    if let Some(r) = src.identifier.strip_prefix("docker-image://") {
                        // Already ours: copying it again would be a loop.
                        if !r.starts_with(&mirror.registry) {
                            refs.insert(r.to_owned());
                        }
                    }
                }
            }
        }
        for r in refs {
            // The target architecture is part of the key as well as the tag:
            // one OnceCell per (image, architecture), or the first peer to
            // ask would settle the answer for every other architecture.
            let key = (format!("base:{}", target.unwrap_or("default")), r.clone());
            let cell = self.cell(&key);
            let full = format!("docker-image://{r}");
            cell.get_or_init(|| async {
                match crate::solve::mirror_image(
                    &mirror.buildkit,
                    &mirror.registry,
                    session,
                    &full,
                    target,
                )
                .await
                {
                    Ok(reference) => {
                        println!("[proxy] base {r} mirrored as {reference}");
                        Some(reference)
                    }
                    Err(e) => {
                        println!("[proxy] base {r} not mirrored: {e:#}");
                        None
                    }
                }
            })
            .await;
        }

        // The FULL reference, scheme included: the rewrite replaces the
        // identifier wholesale, and buildkit rejects a bare
        // `host:port/name:tag` with "invalid".
        // The SAME key the mirror wrote under, architecture included. Writing
        // under `base:linux/amd64` and reading under `base` mirrors the image
        // correctly, finds nothing, leaves the graph naming docker.io, and
        // then rejects it as "base unmirrored" - a success and a failure that
        // never meet, with both printed.
        let key_ns = format!("base:{}", target.unwrap_or("default"));
        crate::dispatch::rewrite_registry_sources(&out, &|r| {
            self.resolved(&(key_ns.clone(), r.to_owned()))
        })
    }

    /// Materialise every `local://` source this graph names.
    ///
    /// Best effort by construction: a context we fail to publish leaves that
    /// subtree undispatchable, which is where it already was. It must never
    /// fail the build - the client asked for a build, not for dispatch.
    async fn publish_contexts(
        &self,
        mirror: &Mirror,
        def: &bollard_buildkit_proto::pb::Definition,
        meta: &tonic::metadata::MetadataMap,
    ) {
        use prost::Message;
        // The session id rides the request headers; without it the daemon
        // has no filesync to resolve `local://` through.
        let session = meta
            .get("buildkit-controlapi-buildid")
            .and_then(|v| v.to_str().ok())
            .and_then(|b| self.sessions.lock().expect("sessions").get(b).cloned());
        let Some(session) = session else {
            println!("[proxy] gateway solve with no known session - not publishing");
            return;
        };
        let mut names: std::collections::BTreeSet<String> = Default::default();
        for bytes in &def.def {
            if let Ok(op) = bollard_buildkit_proto::pb::Op::decode(bytes.as_slice()) {
                if let Some(bollard_buildkit_proto::pb::op::Op::Source(src)) = &op.op {
                    if let Some(n) = src.identifier.strip_prefix("local://") {
                        names.insert(n.to_owned());
                    }
                }
            }
        }
        for name in names {
            let key = (session.clone(), name.clone());
            let cell = self.cell(&key);
            cell.get_or_init(|| async {
                match crate::solve::publish_context(
                    &mirror.buildkit,
                    &mirror.registry,
                    &session,
                    &name,
                )
                .await
                {
                    Ok(reference) => {
                        println!("[proxy] context {name:?} published as {reference}");
                        self.wire.lock().expect("wire").contexts_published += 1;
                        Some(reference)
                    }
                    // Remembered for this build rather than retried per
                    // solve: eleven solves each re-attempting a publish
                    // that cannot work is eleven times the wait for the
                    // same answer.
                    Err(e) => {
                        println!("[proxy] context {name:?} not published: {e:#}");
                        None
                    }
                }
            })
            .await;
        }
    }
}

/// What the gateway's Solve offers a dispatcher. THIS is the graph.
fn report_gateway(wire: &std::sync::Mutex<Wire>, req: &gw::SolveRequest) {
    let Some(def) = &req.definition else {
        // Say WHY nothing can be dispatched, not merely that nothing was.
        // "no definition" is true and useless; a user who points `docker
        // build` at this proxy and sees an even split of nothing deserves the
        // reason and the remedy.
        let mut w = wire.lock().expect("wire");
        if req.frontend.is_empty() {
            *w.rejected.entry("no definition".to_owned()).or_default() += 1;
            return;
        }
        let key = format!("frontend runs in the daemon: {}", req.frontend);
        let first = !w.rejected.contains_key(&key);
        *w.rejected.entry(key).or_default() += 1;
        drop(w);
        if first {
            println!(
                "[proxy] frontend {:?} is resolved INSIDE the daemon, so the LLB it \
                 generates never crosses this proxy and no part of it can be placed \
                 on a peer.\n\
                 [proxy]   what dispatches: clients that build the graph themselves \
                 and send it - earthbuild, `buildctl build < graph.llb`, anything \
                 driving the gateway with a Definition.\n\
                 [proxy]   what does not: `--frontend <name>`, because the daemon \
                 runs that frontend as its own gateway client and never asks us.",
                req.frontend
            );
        }
        return;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    let mut w = wire.lock().expect("wire");
    w.observe(def);
    println!(
        "[proxy] gateway solve #{}: {} ops, {} cuts >= {MIN_CUT_OPS}, {} free-frontier",
        w.solves,
        a.ops,
        a.cuts.len(),
        free.len(),
    );
}

/// Serve the Control service on `addr`, forwarding to `upstream`.
pub async fn serve(
    addr: std::net::SocketAddr,
    upstream: String,
    peers: Vec<String>,
) -> anyhow::Result<()> {
    println!("[proxy] buildkit control on {addr} -> {upstream}");
    let mut proxy = Proxy::connect(upstream.clone())
        .await?
        .with_peers(&peers)
        .await?;
    proxy.mirror = std::env::var("REBUCK2_MIRROR").ok().map(|registry| Mirror {
        registry,
        buildkit: upstream,
    });
    // The characterisation is about the BUILD, so it prints when we are
    // asked to stop rather than per Solve.
    let wire = proxy.wire.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        wire.lock().expect("wire").report();
        std::process::exit(0);
    });
    tonic::transport::Server::builder()
        .add_service(control::control_server::ControlServer::new(proxy.clone()))
        .add_service(gw::llb_bridge_server::LlbBridgeServer::new(proxy))
        .serve(addr)
        .await?;
    Ok(())
}

/// The GATEWAY, which is where the graph is.
///
/// A buildkit client drives its build through `LLBBridge` on the SAME
/// connection it speaks Control on, so these calls land here beside the
/// Control ones — and `Solve` carries the `Definition` that `Control.Solve`
/// does not. Forwarded unchanged; only `solve` is looked at on the way
/// through.
#[tonic::async_trait]
impl gw::llb_bridge_server::LlbBridge for Proxy {
    async fn solve(
        &self,
        request: Request<gw::SolveRequest>,
    ) -> Result<Response<gw::SolveResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        trace(&self.wire, "solve");
        report_gateway(&self.wire, &req);
        // Where the time goes. The tax was invisible until peer 0 rejoined
        // the round robin (10s -> 11s); "attack the round-trip" is a guess
        // until it is split into publish / peer build / answer, because two
        // of those three are not round-trips at all.
        let t_solve = std::time::Instant::now();
        let mut t_portable = 0u64;
        let mut t_adopt = 0u64;
        // Publish any build context this graph needs, so the subtree stops
        // being pinned to the one machine holding the client's disk. The
        // build itself is untouched and still goes upstream: publishing is
        // preparation for dispatch, not dispatch.
        if let (Some(mirror), Some(def)) = (&self.mirror, &req.definition) {
            self.publish_contexts(mirror, def, &meta).await;
        }

        // Place the work by ADOPTION, not forwarding. A peer cannot accept
        // this gateway solve - jobs are daemon-local - so the peer builds
        // the portable graph through its own Control.Solve and publishes
        // the result, and the client's solve is then answered here with a
        // graph that merely imports it. Peer 0 fetches content instead of
        // building, and the ref it returns is its own.
        let mut req = req;
        {
            let mut w = self.wire.lock().expect("wire");
            let key = match (
                self.peers.len() > 1,
                self.mirror.is_some(),
                req.definition.is_some(),
            ) {
                (false, _, _) => "no fleet",
                (_, false, _) => "no mirror",
                (_, _, false) => "no definition",
                _ => "considered",
            };
            *w.rejected.entry(key.to_owned()).or_default() += 1;
        }
        if self.peers.len() > 1 {
            if let (Some(mirror), Some(def)) = (&self.mirror, req.definition.clone()) {
                // The EXCLUSIONS, checked FIRST because they are free.
                // Principle 10 - one cache mount, secret, ssh agent or
                // privileged exec anywhere grounds the whole subtree - and
                // this ran on the REWRITTEN graph until the rewrite was
                // timed at 1.6s a solve. Earthly excludes eleven solves in
                // twelve, so eleven rewrites were published, mirrored and
                // thrown away. Hazards live on ExecOps and rewriting only
                // touches source identifiers, so the original graph gives
                // the same verdict for nothing.
                let verdict = crate::dispatch::inspect(&def);
                let allowed = verdict.dispatchable();
                if !allowed {
                    // Name the secret, not just its kind. A build that
                    // declares none can still be full of them: a frontend
                    // may attach its own, and "excluded: Secret" then reads
                    // as the user's fault.
                    use prost::Message;
                    let mut detail: Vec<String> = Vec::new();
                    for b in &def.def {
                        if let Some(bollard_buildkit_proto::pb::op::Op::Exec(e)) =
                            bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                                .ok()
                                .and_then(|o| o.op)
                        {
                            for se in &e.secretenv {
                                detail.push(format!("env {}={}", se.name, se.id));
                            }
                            for m in &e.mounts {
                                if let Some(so) = &m.secret_opt {
                                    detail.push(format!("mount {} id={}", m.dest, so.id));
                                }
                            }
                        }
                    }
                    detail.sort();
                    detail.dedup();
                    let why = verdict
                        .exclusions
                        .first()
                        .map(|(_, e)| format!("excluded: {e:?} {detail:?}"))
                        .unwrap_or_else(|| "excluded: platform".to_owned());
                    *self
                        .wire
                        .lock()
                        .expect("wire")
                        .rejected
                        .entry(why)
                        .or_default() += 1;
                }
                // Draw ONCE, and only among solves that COULD move. Two
                // calls advance the cursor twice, so the peer that gets the
                // work is not the peer whose turn it was; and letting an
                // excluded solve consume a turn means an earthly build burns
                // eleven turns and hands its one movable solve to whichever
                // machine the arithmetic lands on.
                let peer = if allowed {
                    self.next_place(&verdict.platform)
                } else {
                    None
                };
                if allowed {
                    *self
                        .wire
                        .lock()
                        .expect("wire")
                        .placed
                        .entry(peer.unwrap_or(0))
                        .or_default() += 1;
                    if let Some(id) = meta
                        .get("buildkit-controlapi-buildid")
                        .and_then(|v| v.to_str().ok())
                    {
                        self.went.lock().expect("went").insert(id.to_owned(), peer);
                    }
                }
                if allowed && peer.is_none() {
                    // Peer 0's turn. It already holds the job, the session
                    // and the graph, so its share is served by falling
                    // through to the ordinary solve below - no publish, no
                    // import, no adoption. This is not merely the cheaper
                    // route: peer 0's `addr` is the sentinel "upstream", so
                    // it is the only route.
                    self.wire.lock().expect("wire").home += 1;
                } else if let Some(peer) = peer {
                    let addr = self.peers[peer].addr.clone();
                    debug_assert_ne!(
                        addr, "upstream",
                        "peer 0 has no dialable address; its turn is the home path"
                    );
                    // Only NOW is the rewrite worth its 1.6s: this graph is
                    // leaving. Publishing a context and mirroring a base for
                    // a solve that stays home buys nothing at all.
                    let session = self.session_for(&meta);
                    let t = std::time::Instant::now();
                    let target = self.peers[peer].platforms.first().cloned();
                    let portable = self
                        .make_portable(&def, &session, mirror, target.as_deref())
                        .await;
                    t_portable = t.elapsed().as_millis() as u64;
                    // Portable means EVERY source is something a sessionless
                    // peer can fetch: nothing local, and every image already
                    // in our mirror. Checking only `local == 0` let a graph
                    // whose base was still being mirrored go out anyway -
                    // concurrent solves skip an in-flight copy - and the peer
                    // then reached for Docker Hub with no credentials and
                    // panicked.
                    let local_clear = crate::dispatch::analyse(&portable, 1)
                        .cuts
                        .first()
                        .is_none_or(|c| c.frontier.local == 0);
                    let bases_clear = portable.def.iter().all(|b| {
                        use prost::Message;
                        match bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                            .ok()
                            .and_then(|o| o.op)
                        {
                            Some(bollard_buildkit_proto::pb::op::Op::Source(src)) => src
                                .identifier
                                .strip_prefix("docker-image://")
                                .is_none_or(|r| r.starts_with(&mirror.registry)),
                            _ => true,
                        }
                    });
                    // Counting the reason is not the same as ACTING on it:
                    // an unportable graph must not be sent, or the peer
                    // reaches for Docker Hub with no credentials. Moving the
                    // rewrite in here turned a `movable` conjunction into a
                    // count-and-continue, which is how a check becomes
                    // decoration.
                    let adopted = if local_clear && bases_clear {
                        // Held across the await, so the count is exact -
                        // this is the whole reason `least_loaded` can be
                        // trusted where peer 0's occupancy cannot.
                        self.outstanding[peer].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let t = std::time::Instant::now();
                        let r = self
                            .adopt_or_take_back(peer, &addr, &mirror.registry, &portable, t)
                            .await;
                        t_adopt = t.elapsed().as_millis() as u64;
                        self.outstanding[peer].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        // Only a COMPLETED adoption tells us what normal
                        // costs. Recording a take-back would fold our own
                        // impatience into the threshold that produced it.
                        if matches!(r, Adoption::Done(_)) {
                            self.adopted_ms.lock().expect("adopted_ms").push(t_adopt);
                        }
                        r
                    } else {
                        let why = match (local_clear, bases_clear) {
                            (false, false) => "context and base unmirrored",
                            (false, true) => "context unmirrored",
                            (true, false) => "base unmirrored",
                            (true, true) => unreachable!(),
                        };
                        *self
                            .wire
                            .lock()
                            .expect("wire")
                            .rejected
                            .entry(why.to_owned())
                            .or_default() += 1;
                        Adoption::NotOffered
                    };
                    match adopted {
                        // Nothing was offered, so nothing refused. Reporting
                        // "peer refused" here would blame a machine that was
                        // never asked.
                        Adoption::NotOffered | Adoption::TookBack => {}
                        Adoption::Done(reference) => {
                            println!("[proxy] adopted from peer {peer}: {reference}");
                            self.wire.lock().expect("wire").routed += 1;
                            req.definition = Some(crate::dispatch::import_graph(&reference));
                        }
                        // Fail open: build it here, exactly as we would
                        // have without a fleet.
                        Adoption::Refused(e) => {
                            use prost::Message;
                            let srcs: Vec<String> = portable
                                .def
                                .iter()
                                .filter_map(|b| {
                                    match bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                                        .ok()?
                                        .op?
                                    {
                                        bollard_buildkit_proto::pb::op::Op::Source(s) => {
                                            Some(s.identifier)
                                        }
                                        _ => None,
                                    }
                                })
                                .collect();
                            println!(
                                "[proxy] peer {peer} could not take it: {e:#} | sources={srcs:?}"
                            );
                            // Count the FAILURES too. `routed` counts only
                            // successes, so a fleet attempting twelve
                            // adoptions and completing one looked identical
                            // to a fleet attempting one - which sent me
                            // hunting a placement bug that did not exist.
                            *self
                                .wire
                                .lock()
                                .expect("wire")
                                .rejected
                                .entry("peer refused".to_owned())
                                .or_default() += 1;
                        }
                    }
                }
            }
        }

        // Always peer 0: it holds the client's job, and after adoption the
        // graph is a fetch rather than a build.
        let t_answer = std::time::Instant::now();
        let out = self.gw().solve(Request::from_parts(meta, ext, req)).await?;
        let answer = t_answer.elapsed().as_millis() as u64;
        self.wire.lock().expect("wire").spans.push(Span {
            total: t_solve.elapsed().as_millis() as u64,
            portable: t_portable,
            adopt: t_adopt,
            answer,
        });
        let out = out.into_inner();
        self.remember(&out.result, 0);
        Ok(Response::new(out))
    }

    async fn resolve_image_config(
        &self,
        request: Request<gw::ResolveImageConfigRequest>,
    ) -> Result<Response<gw::ResolveImageConfigResponse>, Status> {
        trace(&self.wire, "resolve_image_config");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .resolve_image_config(Request::from_parts(meta, ext, req))
            .await
    }
    async fn resolve_source_meta(
        &self,
        request: Request<gw::ResolveSourceMetaRequest>,
    ) -> Result<Response<gw::ResolveSourceMetaResponse>, Status> {
        trace(&self.wire, "resolve_source_meta");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .resolve_source_meta(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_file(
        &self,
        request: Request<gw::ReadFileRequest>,
    ) -> Result<Response<gw::ReadFileResponse>, Status> {
        trace(&self.wire, "read_file");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_file(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_dir(
        &self,
        request: Request<gw::ReadDirRequest>,
    ) -> Result<Response<gw::ReadDirResponse>, Status> {
        trace(&self.wire, "read_dir");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_dir(Request::from_parts(meta, ext, req))
            .await
    }
    async fn stat_file(
        &self,
        request: Request<gw::StatFileRequest>,
    ) -> Result<Response<gw::StatFileResponse>, Status> {
        trace(&self.wire, "stat_file");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .stat_file(Request::from_parts(meta, ext, req))
            .await
    }
    async fn evaluate(
        &self,
        request: Request<gw::EvaluateRequest>,
    ) -> Result<Response<gw::EvaluateResponse>, Status> {
        trace(&self.wire, "evaluate");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .evaluate(Request::from_parts(meta, ext, req))
            .await
    }
    async fn ping(
        &self,
        request: Request<gw::PingRequest>,
    ) -> Result<Response<gw::PongResponse>, Status> {
        trace(&self.wire, "ping");
        let (meta, ext, req) = request.into_parts();
        self.gw().ping(Request::from_parts(meta, ext, req)).await
    }
    async fn inputs(
        &self,
        request: Request<gw::InputsRequest>,
    ) -> Result<Response<gw::InputsResponse>, Status> {
        trace(&self.wire, "inputs");
        let (meta, ext, req) = request.into_parts();
        self.gw().inputs(Request::from_parts(meta, ext, req)).await
    }
    async fn new_container(
        &self,
        request: Request<gw::NewContainerRequest>,
    ) -> Result<Response<gw::NewContainerResponse>, Status> {
        trace(&self.wire, "new_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .new_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn release_container(
        &self,
        request: Request<gw::ReleaseContainerRequest>,
    ) -> Result<Response<gw::ReleaseContainerResponse>, Status> {
        trace(&self.wire, "release_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .release_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_file_container(
        &self,
        request: Request<gw::ReadFileRequest>,
    ) -> Result<Response<gw::ReadFileResponse>, Status> {
        trace(&self.wire, "read_file_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_file_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_dir_container(
        &self,
        request: Request<gw::ReadDirRequest>,
    ) -> Result<Response<gw::ReadDirResponse>, Status> {
        trace(&self.wire, "read_dir_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_dir_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn stat_file_container(
        &self,
        request: Request<gw::StatFileRequest>,
    ) -> Result<Response<gw::StatFileResponse>, Status> {
        trace(&self.wire, "stat_file_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .stat_file_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn warn(
        &self,
        request: Request<gw::WarnRequest>,
    ) -> Result<Response<gw::WarnResponse>, Status> {
        trace(&self.wire, "warn");
        let (meta, ext, req) = request.into_parts();
        self.gw().warn(Request::from_parts(meta, ext, req)).await
    }

    /// `return` is a keyword here and a method name there.
    async fn r#return(
        &self,
        request: Request<gw::ReturnRequest>,
    ) -> Result<Response<gw::ReturnResponse>, Status> {
        trace(&self.wire, "return");
        let (meta, ext, req) = request.into_parts();
        // Timed, because `solve` is LAZY. A gateway Solve hands back a ref
        // in about a millisecond whether it stands for a nine-second build
        // or a registry pull; the work happens when the ref is evaluated,
        // which is here. Measuring only `solve` said the answer cost 1ms
        // and made a pull look free.
        let t = std::time::Instant::now();
        let out = self
            .gw()
            .r#return(Request::from_parts(meta, ext, req))
            .await;
        self.wire
            .lock()
            .expect("wire")
            .returns
            .push(t.elapsed().as_millis() as u64);
        out
    }

    type ExecProcessStream =
        Pin<Box<dyn futures::Stream<Item = Result<gw::ExecMessage, Status>> + Send>>;

    async fn exec_process(
        &self,
        request: Request<Streaming<gw::ExecMessage>>,
    ) -> Result<Response<Self::ExecProcessStream>, Status> {
        let (meta, ext, stream) = request.into_parts();
        let inbound = stream.filter_map(|m| futures::future::ready(m.ok()));
        let s = self
            .gw()
            .exec_process(Request::from_parts(meta, ext, inbound))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }
}

#[cfg(test)]
mod tests {
    use super::turn;

    /// The regression: peer 0 must get its share.
    ///
    /// Under the old `1 + cursor % (peers - 1)` this fails on the first
    /// assert - peer 0 never came up, however many solves arrived, so a
    /// two-machine fleet ran on one machine and called it dispatch.
    #[test]
    fn every_peer_in_the_fleet_gets_a_turn() {
        for peers in 1..6usize {
            let mut seen = vec![0usize; peers];
            for cursor in 0..peers * 3 {
                seen[turn(cursor, &vec![1usize; peers])] += 1;
            }
            assert_eq!(seen[0], 3, "peer 0 skipped in a fleet of {peers}");
            assert!(
                seen.iter().all(|&n| n == 3),
                "uneven over {peers} peers: {seen:?}"
            );
        }
    }

    /// Native beats emulated, but emulated still beats nowhere.
    ///
    /// Every buildkitd claims nearly every platform because binfmt will run
    /// anything, so a set-membership filter is answered YES by every peer and
    /// changes nothing. The worker's own architecture is the one it lists
    /// FIRST.
    #[test]
    fn a_native_peer_is_preferred_and_an_emulated_one_is_not_refused() {
        let arm = vec!["linux/arm64".to_owned(), "linux/amd64".to_owned()];
        let x86 = vec!["linux/amd64".to_owned(), "linux/arm64".to_owned()];
        assert!(super::native_for(&arm, "linux/arm64"));
        assert!(
            !super::native_for(&arm, "linux/amd64"),
            "emulation is not native"
        );
        assert!(super::native_for(&x86, "linux/amd64"));
        // Peer 1 emulates, peer 2 is native: every away turn goes to peer 2.
        let away: Vec<usize> = (0..8)
            .filter_map(|c| super::place(c, &[1, 1, 1], &[0, 0], &[0, 0], &[false, true]))
            .collect();
        assert!(!away.is_empty());
        assert!(
            away.iter().all(|&p| p == 2),
            "emulated peer chosen: {away:?}"
        );
        // Nobody native: HOME. This asserted the opposite until two real
        // machines were involved - emulation is not merely slow here, because
        // the mirrored base is single-architecture and a foreign peer fails
        // with `exit code: 255` after pulling it.
        let away: Vec<usize> = (0..8)
            .filter_map(|c| super::place(c, &[1, 1, 1], &[0, 0], &[0, 0], &[false, false]))
            .collect();
        assert!(
            away.is_empty(),
            "offered an arm64 base to a peer that cannot execute it: {away:?}"
        );
    }

    /// A daemon that will not answer ListWorkers is usable, just never
    /// preferred - failing closed would turn one bad health check into a
    /// fleet of one.
    #[test]
    fn a_silent_daemon_is_not_native_and_not_excluded() {
        assert!(!super::native_for(&[], "linux/arm64"));
    }

    /// A struck peer stops getting an equal share.
    #[test]
    fn a_peer_taken_back_from_is_avoided() {
        // Two away peers, peer 2 struck once. Every away turn goes to peer 1.
        let load = [0usize, 0];
        let strikes = [0usize, 1];
        let away: Vec<usize> = (0..12)
            .filter_map(|c| super::place(c, &[1, 1, 1], &load, &strikes, &[true, true]))
            .collect();
        assert!(!away.is_empty(), "no away turns at all");
        assert!(
            away.iter().all(|&p| p == 1),
            "struck peer 2 still got work: {away:?}"
        );
    }

    /// A bias, not a ban. A struck peer beats peers that are genuinely
    /// swamped - otherwise one bad minute permanently shrinks the fleet.
    #[test]
    fn a_struck_peer_still_beats_a_swamped_one() {
        let away: Vec<usize> = (0..6)
            .filter_map(|c| super::place(c, &[1, 1, 1], &[9, 0], &[0, 1], &[true, true]))
            .collect();
        assert!(
            away.iter().all(|&p| p == 2),
            "peer 1 holding nine jobs was preferred to a once-struck idle peer: {away:?}"
        );
    }

    /// Nowhere good to send it is not a reason to send it somewhere bad.
    ///
    /// Without this a two-daemon fleet whose only peer is slow would offer,
    /// wait out the bound and take it back on EVERY solve - paying the
    /// straggler tax forever to learn something it already knew.
    #[test]
    fn a_wholly_struck_fleet_builds_at_home() {
        for cursor in 0..8 {
            assert_eq!(super::place(cursor, &[1, 1], &[0], &[1], &[true]), None);
            assert_eq!(
                super::place(cursor, &[1, 1, 1], &[0, 0], &[2, 1], &[true, true]),
                None
            );
        }
    }

    /// A lone daemon has nowhere to send anything.
    #[test]
    fn a_fleet_of_one_never_dispatches() {
        assert_eq!(super::place(0, &[1], &[], &[], &[]), None);
        assert_eq!(super::place(7, &[1], &[], &[], &[]), None);
    }

    /// With nothing observed, nothing is slow.
    ///
    /// The alternative - a cold default - is a number chosen to make one
    /// fixture look good. Any value between 9.5s and 163s "fixes" the slow
    /// peer measurement, which is exactly why none of them should be picked
    /// before the fleet has said what normal is.
    #[test]
    fn a_cold_fleet_calls_nothing_a_straggler() {
        assert_eq!(super::hedge_after(&[]), None);
    }

    /// Three times the median, and the median is not the mean: one 163s
    /// straggler among normal work must not drag the threshold up to meet
    /// itself.
    #[test]
    fn the_threshold_is_not_moved_by_the_straggler_it_judges() {
        let normal = [9_400u64, 9_500, 9_600];
        let with_straggler = [9_400u64, 9_500, 9_600, 163_000];
        assert_eq!(
            super::hedge_after(&normal),
            Some(std::time::Duration::from_millis(28_500))
        );
        // Median of the 4-element list is 9600, so the threshold moves by
        // 300ms, not by two and a half minutes. A mean would have put it at
        // over four minutes and never fired.
        assert_eq!(
            super::hedge_after(&with_straggler),
            Some(std::time::Duration::from_millis(28_800))
        );
    }

    /// Fast work is not hedged on jitter.
    #[test]
    fn a_fleet_of_fast_subtrees_has_a_floor() {
        assert_eq!(
            super::hedge_after(&[80, 90, 100]),
            Some(std::time::Duration::from_secs(5)),
            "270ms would abandon peers over scheduling noise"
        );
    }

    /// The control law reproduces the measured optimum.
    ///
    /// These are the real numbers from two machines. The flat split's
    /// observation must point AT the split that measured best, or the law is
    /// just an equation that happens to run.
    #[test]
    fn observed_service_times_point_at_the_measured_optimum() {
        // 12:12, wall 18s - away is the straggler.
        assert_eq!(super::derive_weights(11_425, 17_235, 8), (3, 2));
        // 16:8, wall 15s - balanced, so stay put.
        assert_eq!(super::derive_weights(15_228, 15_506, 8), (1, 1));
    }

    /// 1.51 must become 3:2, not 1:1. Truncating the ratio would report
    /// "balanced" for a fleet that is half again slower on one side.
    #[test]
    fn a_ratio_between_whole_numbers_is_not_truncated() {
        assert_eq!(super::derive_weights(1_000, 1_510, 8), (3, 2));
        assert_eq!(super::derive_weights(1_000, 1_250, 8), (5, 4));
        assert_eq!(super::derive_weights(1_000, 2_000, 8), (2, 1));
    }

    /// No samples, no opinion.
    #[test]
    fn a_side_with_no_time_gets_an_even_split() {
        assert_eq!(super::derive_weights(0, 5_000, 8), (1, 1));
        assert_eq!(super::derive_weights(5_000, 0, 8), (1, 1));
    }

    /// A declared weight buys proportionally more turns.
    ///
    /// Buildkit will not say how big a machine is, so an operator saying
    /// "twice the share" is the only capacity signal there is. Measured on
    /// two real machines: a flat split sent half the work to a 32-core box
    /// and half to a 16-core one.
    #[test]
    fn a_heavier_peer_takes_proportionally_more() {
        // home 1, peer1 2, peer2 1 - six turns is one whole cycle.
        let w = [1usize, 2, 1];
        let picks: Vec<usize> = (0..8).map(|c| super::turn(c, &w)).collect();
        assert_eq!(picks, vec![0, 1, 1, 2, 0, 1, 1, 2]);
    }

    /// Load is compared PER UNIT of capacity, so a big machine holding four
    /// jobs is no busier than a small one holding two.
    #[test]
    fn load_is_measured_against_capacity() {
        // peer 1 has weight 2 and holds 3; peer 2 has weight 1 and holds 2.
        // 3/2 > 2/1 is false, so peer 1 is the less loaded of the two.
        let away: Vec<usize> = (0..4)
            .filter_map(|c| super::place(c, &[0, 2, 1], &[3, 2], &[0, 0], &[true, true]))
            .collect();
        assert!(
            !away.is_empty(),
            "home weight 0 should send everything away"
        );
        assert!(
            away.iter().all(|&p| p == 1),
            "the roomier machine was passed over: {away:?}"
        );
    }

    /// Equal load must round-robin, not pile on peer 1.
    ///
    /// `min_by_key` keeps the first minimum, so scanning from 0 would send
    /// every away solve to the same machine and the extra peers would sit
    /// idle - the offloading bug again, one level down. Starting the scan at
    /// the cursor is what prevents it.
    #[test]
    fn an_idle_fleet_still_takes_turns() {
        let idle = [0usize, 0, 0];
        let picks: Vec<usize> = (0..6).map(|c| super::least_loaded(&idle, c)).collect();
        assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
    }

    /// A busy peer is skipped even when it is its turn.
    #[test]
    fn a_loaded_peer_is_passed_over() {
        // Peer 1 holds three adoptions; peers 0 and 2 hold none.
        let load = [0usize, 3, 0];
        for cursor in 0..6 {
            assert_ne!(
                super::least_loaded(&load, cursor),
                1,
                "cursor {cursor} chose the loaded peer"
            );
        }
        // And it comes back into rotation once it drains.
        assert_eq!(super::least_loaded(&[0, 0, 0], 1), 1);
    }

    /// The whole fleet busy is not a reason to pick nobody, and not a reason
    /// to always pick the same one.
    #[test]
    fn a_uniformly_busy_fleet_still_rotates() {
        let load = [2usize, 2, 2];
        let picks: Vec<usize> = (0..3).map(|c| super::least_loaded(&load, c)).collect();
        assert_eq!(picks, vec![0, 1, 2]);
    }

    /// The peer's build is not overhead.
    ///
    /// A dispatched solve whose peer took 9s and whose own preparation and
    /// pull took 300ms and 700ms has cost a second, not ten. Charging the
    /// peer's build as tax makes every dispatch look ruinous and would have
    /// argued for switching dispatch off.
    #[test]
    fn tax_excludes_the_work_the_peer_did_instead_of_us() {
        let dispatched = super::Span {
            total: 10_000,
            portable: 300,
            adopt: 9_000,
            answer: 700,
        };
        assert_eq!(dispatched.tax(), 1_000);
        // A solve that stayed home pays no tax, however long it took.
        let home = super::Span {
            total: 10_000,
            portable: 0,
            adopt: 0,
            answer: 9_990,
        };
        assert_eq!(home.tax(), 9_990);
    }

    /// A cursor that has wrapped `usize` still lands in the fleet. Round
    /// robin over a long-lived proxy is the only user of this and it counts
    /// up forever.
    #[test]
    fn a_wrapped_cursor_still_names_a_peer() {
        for peers in 1..6usize {
            assert!(turn(usize::MAX, &vec![1usize; peers]) < peers);
        }
    }
}
