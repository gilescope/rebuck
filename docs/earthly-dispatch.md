# What stops an earthly build from being distributed

Measured: 1 solve in 12 dispatches on a real `earthly +all`, against 100% on
plain LLB through the same proxy. The cause is not the user's Earthfile and
not a limit of the mechanism. It is four lines of debugger plumbing that
earthbuild attaches to every non-`LOCALLY` `RUN`, whether or not anyone is
debugging.

Upstream revision inspected: `earthbuild@36fa573` (main, 2026-08-07).

## The four hazards

All in `earthfile2llb/converter.go`, all inside one `if !opts.Locally` block
that starts at line 2707 and is guarded by nothing else:

| line | what it adds | why a peer cannot take it |
| ---- | ------------ | ------------------------- |
| 2721 | `llb.SocketTarget("earthly_interactive", ...)` | needs the client session's sshforward |
| 2722 | `llb.SocketTarget("earthly_save_file", ...)` | same |
| 2791 | `llb.AddSecret("/run/secrets/earthly_debugger_settings", ...)` | needs the session's secret store |
| 2792 | `pllb.AddMount(..., llb.HostBind(), ...)` | binds a path on the client's host |

A dispatched subtree is solved on a peer through that peer's own
`Control.Solve`, with no session - its inputs are digests. Any of the four
grounds the whole subtree (principle 10: exclusions propagate upward), so
every `RUN` in an earthly build is undispatchable by construction.

The proxy reports it accurately, naming the secret rather than its kind:

```text
[wire] not routed : {"considered": 12, "excluded: Secret": 11}
  mount /run/secrets/earthly_debugger_settings
    id=name=da39a3ee5e6b4b0d3255bfef95601890afd80709&org=&project=&v=1
```

## The flag that exists and does not help

`c.opt.InteractiveDebuggerEnabled` (from `builder.go:333`,
`b.opt.InteractiveDebugging`) already says whether anyone wants a debugger.
It is consulted at line 2712 - only to decide whether a missing
`CapExecMountSock` is a hard error - and again at 2769, where it sets
`Enabled` *inside the settings payload*. The mounts themselves are attached
either way. So the graph carries the machinery for a debugger that is
switched off.

## The change

Guard the attachments with the flag that already exists, so the plumbing
appears when someone is debugging and not otherwise:

```go
if c.opt.InteractiveDebuggerEnabled || isInteractive {
    runOpts = append(runOpts, debuggerSecretMount, debuggerMount)
}
```

and the same condition around the two `llb.SocketTarget` calls.

Behaviour when debugging is unchanged. When not debugging, every `RUN`
becomes an ordinary exec - dispatchable, and with a cache key that no longer
depends on a debugger socket path or a host bind.

## Why this is not fixable in the proxy

Two options were considered and both are wrong:

- **Strip the mounts.** They are part of the graph the client asked for, and
  removing them changes the digest, so the result would not match what
  buildkit will look for next time. Identity is what buildkit matches on.
- **Forward the session.** A peer could service the secret and the sockets if
  it had the client's session - but that means demultiplexing a gRPC
  connection tunnelled inside the session stream, and the session is bound to
  the daemon the client dialled.

The `HostBind` is not fixable at all from outside: it names a path on the
client's machine, and no other machine has it.

## What it would unblock

Plain LLB through the same proxy dispatches 100% of solves, and a
two-machine fleet takes 24 CPU-bound builds from 64s to 32s. Earthly is
currently capped at 1 in 12 for this reason alone.

Raising it needs a change to earthbuild, not to rebuck2 - which also settles
what this product is: a distributed BuildKit works today for any client that
builds its own LLB, and a distributed earthly additionally needs this one
upstream patch.

## Measured: 0 of 6, and the reason names itself

A real earthly build, through the gateway, on 2026-08-09:

```text
earthly +code            SUCCESS in 91s
[wire] gateway solves : 6
[wire] ops total      : 64
[wire] sources        : 6 registry, 14 local, 0 other
[wire] solves routed  : 0 to other daemons
[wire] built at home  : 6
[wire] not routed     : {"considered": 6,
  "excluded: Secret [\"mount /run/secrets/earthly_debugger_settings
   id=name=da39a3ee...&org=&project=&v=1\"]": 6}
```

**Zero of six**, every one for the same reason, and the reason is this
document. Not "1 in 12" for this target - none at all.

Two things it settles that reading `converter.go` could not.

The build SUCCEEDS. 64 ops through the proxy, correct output, the gateway
analysing every graph and then getting out of the way. Fail-open is not a
claim about a fixture any more.

And the secret is unresolvable by construction, not merely absent. The id is
`name=<sha>&org=&project=&v=1` - generated inside earthly's process per
build. `REBUCK2_SERVE_SECRETS` cannot help: there is nothing outside that
process which could answer, which is why the lift exists and does not apply
here.

It also settles the ORDER of the ceiling below. The 25 cache mounts and 9
explicit secrets are all behind this one: no solve gets far enough for them
to matter, so #784 is not the first of several obstacles, it is the only
reachable one.

## What the fix buys, measured by applying it

The gate from #784, applied locally to `main` (3380dc20) and re-run against
the same fleet and the same target:

```text
                      before            after
build                 SUCCESS 91s       SUCCESS 87s
gateway solves        6                 6
ops total             64                64
blocking exclusion    Secret x6         CacheMount x5
dispatchable          0                 1
```

Identical graph either side, so it is like for like. The debugger secret
stops blocking anything and one solve becomes genuinely dispatchable - held
at home only because home had room, not because it could not travel.

**And the issue as filed is wrong: it is three sites, not two.** Gating the
secret and the host bind leaves `prependDebugger := !opts.Locally` (~2806)
prefixing every command with `/usr/bin/earth_debugger`, which exists only
because of the bind. Every build then dies on its first RUN:

```text
/bin/sh: /usr/bin/earth_debugger: not found
RUN apk add --no-cache git   did not complete successfully. Exit code 127
```

Nothing in that error names the debugger. Anyone attempting the change from
the issue text alone would hit an inscrutable failure on `apk add` and could
reasonably conclude the change is unsafe - which is a good argument for
measuring a proposal by applying it before asking someone else to.

A draft comment carrying these numbers upstream is written and NOT posted;
posting under a maintainer-visible identity is the repo owner's call.

## You cannot put a proxy on loopback

Discovered the hard way, and it applies to anything that wants to sit between
earthly and a buildkit - not only this project.

`containerutil.IsLocal` counts `127.0.0.1`, `localhost` and `::1` as **a
buildkit earthly MANAGES**. Point `EARTHLY_BUILDKIT_HOST` at a proxy on
loopback and earthly compares the settings hash of its own container, decides
they do not match, and restarts it:

```text
buildkitd | Found buildkit daemon as docker container (earthly-dev-buildkitd)
buildkitd | Settings do not match. Restarting buildkit daemon with updated settings...
Error: ... TLS CA file ".../certs/ca_cert.pem" is missing: file does not exist
```

The build dies before ONE solve reaches the proxy, and the error names TLS
certificates, which have nothing to do with the cause. A routable address is
`!isLocal`, and earthly then prints `Connecting to ...` and does nothing
else: no settings hash, no restart, no container management.

So the gateway must bind something routable. On a developer machine that is
the LAN address; on a runner, `hostname -I`. Every workflow and script in
this repo used `127.0.0.1:1234` and none of them could have worked.

Two smaller ones from the same afternoon, both the same mistake - assuming a
constant where the daemon decides:

- The published port is **not 8372**. `earthbuild/buildkitd:v0.8.17` exposes
  8371 and docker maps it to a dynamic host port. Ask `docker port`.
- The image is whatever the earthly BINARY compiled in. `earthbuild/buildkitd:main`
  is not a published tag; `:v0.8.17` is. Run `earthly bootstrap` and copy
  what it chose.

## earthly's own daemon cannot be proxied

Worse than awkward - impossible, and it took an A/B to see it.

`earthly bootstrap` runs its daemon with `BUILDKIT_TCP_TRANSPORT_ENABLED=false`.
It speaks to that daemon over a unix socket; the port the container publishes
answers HTTP/1.1, so a gRPC client gets:

```text
error reading server preface: http2: frame too large, note that the
frame header looked like an HTTP/1.1 header
```

Nothing can sit in front of it, whatever address you use.

The mis-attribution is the instructive part. Pointing a proxy at it and
running earthly gave `h2 protocol error`, which looked like an earthly
problem. `buildctl` produced the SAME error, so not earthly. Loopback
produced the same error as the LAN address, so not the address. The only
remaining variable was the UPSTREAM: every green run in this repo proxies a
plain moby daemon, and this one proxied earthly's.

So the working shape is a hybrid, and both halves matter:

- the IMAGE must be earthly's fork - `earthly bootstrap`, then
  `docker inspect -f '{{.Config.Image}}'`. Stock buildkit rejects the graph.
- the CONTAINER must be ours - `docker run` with
  `BUILDKIT_TCP_TRANSPORT_ENABLED=true`, so it serves gRPC on a port a proxy
  can reach.

Registry trust then goes on our container as
`EARTHLY_ADDITIONAL_BUILDKIT_CONFIG`, where it cannot disturb earthly's
settings hash - which is what made the loopback restart so hard to attribute.

## The ceiling after #784, counted from the Earthfile

Fixing #784 does not make earthbuild's own root Earthfile fully
dispatchable, and it is worth knowing by how much BEFORE the patch, so the
patch is not credited with more than it buys. Counted on the 1060-line
Earthfile at the root of earthbuild:

| construct | RUN steps | what it does to dispatch |
| ------------------------- | --------: | ------------------------ |
| total `RUN` | 65 | |
| `--mount type=cache` | 25 | excluded unless `REBUCK2_PEER_CACHE_MOUNTS=1` |
| `--secret` | 9 | excluded; no session reaches a worker |
| `LOCALLY` | 1 | never dispatchable, by definition |
| `--privileged`, `--ssh`, host network | 0 | nothing to lift |

Four cache ids do the work: `go-mod`, `go-build`, `npm`,
`littleredcorvette-id`. That is a Go and Node build keeping its module and
compiler caches warm, which is exactly what a cache mount is FOR - so this is
not misuse to be tidied away upstream, it is the shape of the build.

So the ladder, in the order the numbers say to climb it:

1. **#784** - the debugger's secret and host bind on every non-`LOCALLY`
   `RUN`. Until this lands, nothing dispatches and the rest is unmeasurable.
2. **Cache mounts, 38% of RUNs.** `REBUCK2_PEER_CACHE_MOUNTS=1` already lifts
   them, on the argument that a cache mount is scratch a peer has its own of.
   That argument is sound and untested against a build that actually depends
   on one being warm - a cold `go-mod` on a worker is correct and slow, and
   slow enough may be worse than not dispatching.
3. **Secrets, 9 RUNs.** These need a session on the worker side, which the
   mesh path does not have. Smallest of the three and the most work.

None of that is proxy work. The distributed BuildKit is not what limits
earthbuild's Earthfile.

- Found: 2026-08-08.
- Raised upstream: [EarthBuild/earthbuild#784](https://github.com/EarthBuild/earthbuild/issues/784).
  Issue only - no patch offered, and any PR needs consent first.
- Earthfile counts: 2026-08-09, at earthbuild `main`.

## Climbed: a real earthly subtree, built by a peer

2026-08-10. `earthly +code` through the gateway, one worker, all four #784
sites gated locally, `REBUCK2_PEER_CACHE_MOUNTS=1`:

```text
build: yes in 88s
gateway solves : 6      placed        : {1: 6}
solves routed  : 1      built at home : 5
                        subtree job 1 built at sha256:8b32973def46cb54...
```

One solve from earthbuild's own root Earthfile was cut out, handed to
another daemon over the mesh, published by digest, pulled back and consumed.
The other five are backpressure, not failure: one worker holds one lead.

Getting there cost four bugs, and every one of them was ours reporting
success it had not earned:

| what it claimed | what was true | how it was found |
| ---------------------------- | ------------------------------- | ------------------------- |
| `inspect`: graph is clean | two mounts of type 101 | asked the graph, not the converter |
| worker: `built at subtree:job-1` | nothing pushed under that name | the tag file was the base's |
| registry: `served 22 requests` | six of them were 404s | counted status, not shape |
| daemon: solve succeeded | it exported nothing | the fork never saw an exporter |

The last is the one worth remembering. Our `SolveRequest` named its exporter
only in `exporters`, the repeated field added in buildkit 0.13. earthly's
fork is cut from 2024-05 and has `Exporter = 3` / `ExporterAttrs = 4` and
nothing else. Protobuf drops unknown fields **silently**, so the request
arrived asking for no export, and the daemon correctly solved, exported
nothing, and returned success. No error on the wire, none in the daemon log,
and a 200 in the registry tally for the base image that mirrored fine beside
it.

An unknown protobuf field is not a compatibility warning; it is a hole with a
success code over it. The `-Deprecated` suffix on those two fields reads like
a thing to avoid, and it is the only thing a 2024 daemon can read.

## The correction this section is

The ladder above says #784 gates everything. That was right. It also implied
the rungs above it had been measured, and they had not: every earlier figure
counted `placed` - solves that cleared exclusion - as though it were
dispatch. `routed`, meaning a peer actually built it, was **0 for all of
them**. The two differ by a worker that accepts a lead and then fails, which
is exactly what was happening and exactly what nothing was counting.

The published figures were the optimistic column, and no run before today had
a non-zero one in the other.

## 6 of 6, and the three askers

2026-08-10, later the same day. `earthly +code`, four workers, all four
EarthBuild#784 sites gated, `REBUCK2_PEER_CACHE_MOUNTS=1`:

```text
build: yes in 114s
gateway solves : 6      solves routed : 6
built at home  : 0      not routed    : {}
spread         : worker 1 x4, worker 2 x2
```

Every solve of earthbuild's root Earthfile target built by a peer.

**114s against 87s for the single-worker run, and that is not a speedup.**
All four workers drive one buildkitd on one laptop, so a second worker adds
contention and no capacity. The number that matters here is `routed`; the
wall clock is waiting on hardware that can actually be in more than one
place.

Getting from 1 to 6 was not a scheduling improvement. Three components
decide whether a subtree may travel - the gateway before offering, the
driver before choosing a peer, the worker before accepting - and each had
its own answer:

| asker | asked | answered |
| ------------- | -------------------------------- | ----------------- |
| gateway | `dispatchable_when(Allow{caches})` | yes, offer it |
| driver | `consider` - which took no policy | no, undispatchable |
| worker | `consider` - the same one | no, declined |

So the gateway offered, the driver refused, and the report blamed the
workers - naming four idle machines of exactly the right platform. Fixing
the driver moved the refusal one hop to the worker, which refused the same
way for the same reason.

`consider` now requires an `Allow` and there is no policy-free version left
to call. `dispatch::policy()` is the only reader of the environment.

The general shape, which cost most of a day in three separate places: a
question with a default answer will be asked by more components than you
intended, and they will diverge silently. Delete the default.

## A bigger target: `+lint`, 9 of 9

Same fleet, same day, the next rung up:

| target | ops | solves | routed | at home | build | spread |
| --------------- | --: | -----: | -----: | ------: | --------- | ------------ |
| `+code` | 64 | 6 | 6 | 0 | yes, 114s | 4/2 |
| `+unit-test` | 113 | 8 | 8 | 0 | yes, 126s | 4/2 |
| `+lint` | 143 | 9 | 9 | 0 | yes, 149s | 6/3 |
| `+lint-all` | 349 | 20 | 20 | 0 | yes, 143s | - |
| `+all-binaries` | 638 | 33 | 33 | 0 | yes, 194s | 12/8/7/6 |

`+all-binaries` is ten times the graph of `+code` and every solve of it was
built by a peer, spread across all four workers. It cross-compiles five
platforms, which turns out to say nothing about platform pinning: earthly
cross-compiles with `GOOS`/`GOARCH` inside one `linux/arm64` image, so the
whole graph is single-platform and the placement logic is never asked the
interesting question. Worth knowing before anyone quotes this as evidence
that heterogeneous placement works - it is not.

The first `+lint` attempt failed, and the failure was worth having: two
subtrees came back with `golangci-lint ... exit code: 1`, which is a real
build failure faithfully reported through the mesh rather than anything
going wrong in the fleet. The offending line was **ours** -

```text
earthfile2llb/converter.go:2798:3: missing whitespace above this line
  (invalid statement above if) (wsl_v5)
```

- the local EarthBuild#784 patch, rejected by earthbuild's own linter. A
blank line fixed it and `+lint` went green.

Which is the argument for running a project's real targets rather than
fixtures: the fleet linted the patch that makes the fleet possible, and found
it wanting. No synthetic graph was ever going to do that.

## The first time a blob moved between two workers

2026-08-10. `earthly +code`, three workers, each with its OWN buildkitd and
its OWN registry:

```text
build: yes in 128s      solves routed: 6 of 6
worker 1: local=0 peer=0 driver=5
worker 2: no fetches
worker 3: local=0 peer=6 driver=2
```

`peer=6` is the first non-zero `hits_peer` this repo has ever recorded.

It had never been zero because the mesh was broken. Every worker was pointed
at the COORDINATOR's registry, so a subtree's layers were already sitting
where the requester would look, and the fetch path - local, then a
bloom-matched peer, then the driver - was never asked a question it could
answer with "peer". The fleet had nowhere to move anything to.

Two topology facts had to be true together, and each was a separate change:

| change | without it |
| ------------------------- | ------------------------------------------ |
| one buildkitd per worker | a "peer" build lands in the requester's own content store |
| one registry per worker | the layers are already at the address the requester pulls from |

Worker 1 still took all five of its blobs from the driver. That is the
fallback working, not the mesh failing: on a cold fleet the bloom filters
have little in them, and the driver is the correct answer when no peer
advertises the content. What matters is that `peer` is now a column that can
be non-zero, so the next question - how OFTEN it beats the driver, and
whether that improves as stores warm - is finally askable.

Every dispatch number recorded before this section was measured on a fleet
where the handover cost nothing, because nothing moved. The placement figures
stand; any inference from them about transfer cost does not.

## The mesh carrying more than the driver

`+all-binaries`, three workers, each with its own daemon and its own
registry:

```text
build: yes in 333s      solves routed: 33 of 33
worker 1: local=0 peer=11 driver=9
worker 2: local=0 peer=1  driver=5
worker 3: local=0 peer=12 driver=3
```

**24 peer fetches against 17 driver fetches.** Most of the content that had
to move between machines moved between workers, without passing through the
coordinator. That is principle 6 - the driver arbitrates and carries as
little as possible - showing up as a number for the first time rather than as
a design intention.

The wall clock went the other way, and the comparison is worth setting out
because it is easy to quote the wrong column:

| topology | wall | peer fetches | what it actually measures |
| ----------------------------- | ---: | -----------: | ------------------------- |
| 4 workers, shared daemon | 194s | n/a | placement only; no transfer exists |
| 3 workers, own daemon, shared registry | 297s | 0 | transfer, but to where the requester already looks |
| 3 workers, own daemon, own registry | 333s | 24 | the real thing |

Each row is slower than the last, and each is more honest than the last. The
first has nothing to transfer; the second transfers into a registry the
requester was going to read anyway; only the third pays what distribution
costs. On one laptop that cost buys nothing, because three cold daemons and
three registries are contending for the same cores and the same page cache.

The number to carry forward is 24:17, not 333s. Whether distribution PAYS is
a question for hardware that can genuinely be in three places, and nothing
measured on this machine can answer it.

## The biggest target, and what actually blocks it

`+test-no-qemu` is the target M5 cares about - it BUILDs all twelve test
groups. Through the gateway it is 1018 solves and 74,246 ops, roughly a
hundred times `+code`.

| run | routed | built at home | change |
| ---- | -----: | ------------: | ------------------------------- |
| tnq5 | 269 | 673 | git sources excluded |
| tnq7 | 254 | 250 | git sources MIRRORED |

Built-at-home fell by nearly two thirds. One mirror did it:
`git://github.com/EarthBuild/buildkit.git#51fe8fb` is named by many solves
and fetched once, because the `OnceCell` that dedupes base images dedupes
this too.

Three defects were found getting there, and all three had the same shape -
something reporting a state it had not verified:

1. **The mirror never ran.** `inspect` refused git-bearing graphs before
   `make_portable` could fix them. The stale assumption was in a comment one
   line above: "rewriting only touches source identifiers, so the original
   graph gives the same verdict" - true until source hazards existed.
2. **The blocker column was fiction.** The report named
   `exclusions.first()`, whichever hazard sorted earliest, INCLUDING ones the
   operator had lifted. It printed `CacheMount: 194` for graphs whose cache
   mounts were explicitly allowed.
3. **`SAVE IMAGE` failed as `Unimplemented`.** earthly's fork adds
   `rpc Export` to the gateway; upstream has none, so our generated service
   has none, so tonic refused it and every run died at the end of an
   otherwise successful build.

The third is worth keeping as a rule, because it looks like a contradiction
of the other two:

> `dispatch` fails CLOSED on anything it does not recognise. The proxy fails
> OPEN. "May this graph run on someone else's machine" must be conservative;
> "may the client talk to its own daemon" must be transparent. Each default
> is a bug in the other's place.
