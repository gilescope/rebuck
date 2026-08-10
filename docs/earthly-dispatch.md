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

## Where `+test-no-qemu` actually stands

Four fixes later, on the target M5 cares about:

| run | routed | at home | context unmirrored | git unportable |
| ----- | -----: | ------: | -----------------: | -------------: |
| tnq5 | 269 | 673 | 69 | 407 |
| tnq8 | 323 | 311 | 686 | 397 |
| tnq9 | 537 | 238 | 0 | 0 |
| tnq10 | 498 | 241 | 0 | 0 |

Blob provenance on tnq9: **peer 205, driver 39**. The mesh moves 84% of
everything that crosses a machine boundary.

What still refuses to travel is supposed to:

```text
170  excluded: Insecure            privileged exec - a trust decision
 68  excluded: UnknownMount(100)   WITH DOCKER's host bind - this host
  2  base unmirrored
```

**The build is still red, and not for a reason the fleet can fix.**
`+test-no-qemu` is earthbuild's integration suite: it reaches for an
ssh-agent, clones `test-remote` over git matchers, and pushes to a registry.

```text
./tests+reject-privileged-import-test | failed to match earthly reference
    test-remote/privileged with any git matchers
./tests+command | failed to connect to ssh-agent ... dial unix: missing address
```

118 targets pass; the suite then cancels. Getting the remainder green is a
question about credentials and network on the machine running it, not about
distribution, and no amount of proxy work will change it. The honest claim is
the dispatch column: two thirds of a 54,000-op graph built on other daemons,
with every refusal accounted for.

The two largest blockers this week were both ours, and neither was an
architectural limit:

- a context named `./buildkitd` is not a legal OCI tag, so 686 pushes failed
  with `invalid reference format`
- a mirror keyed by `git://host/repo` looked up by `host/repo`, so 397
  rewrites silently found nothing

Both reported as capability limits of the fleet - "context unmirrored", "not
portable" - and both were format strings.

## Parity, which is the only "green" this machine can show

`+test-no-qemu-group1`, upstream's own invocation (`--ci -P`, with
`EARTHLY_VERSION_FLAG_OVERRIDES`), run twice: once against a bare daemon,
once through three workers.

```text
baseline (no proxy):  1 failed target
through the fleet:    1 failed target - THE SAME ONE
                      286 solves, 248 routed, 28 built at home
```

The failure is `./t/autocompletion+test-no-parent-at-root-from-home`, which
diffs a directory listing and disagrees about `../run/`. It fails with or
without any of this.

**Correction.** This was first written up as arm64-sensitive. It is not: the
same target fails in the BASELINE on ubuntu/amd64 CI runners too. Whatever it
depends on - `$HOME`, the container rootfs, how the runner mounts things - is
not the architecture, and calling it that was a guess dressed as a finding
because macOS was the only place it had been seen.

**That is the result, and chasing an absolute green here would have been
chasing someone else's bug.** The baseline was measured first precisely
because "the build is red through the proxy" is worthless without knowing
whether it is red without one.

### What absolute green actually needs

1. **Per GROUP, not the aggregate.** Upstream runs
   `+test-no-qemu-group1` .. `group12` as twelve separate jobs and never
   builds `+test-no-qemu` itself. The aggregate cancels every sibling when
   one target fails, which is why earlier runs of it reported 118 passes and
   then stopped.
2. **`--ci -P` and the flag overrides.** `--ci` changes output and strictness;
   `-P` is what makes earthly request `security.insecure`, without which
   WITH DOCKER dies as `failed to load LLB`. `EARTHLY_VERSION_FLAG_OVERRIDES`
   comes from `.earthly_version_flag_overrides` in the repo root - thirteen
   feature flags the tests assume.
3. ~~**ubuntu/amd64.** The autocompletion group is arm64-sensitive.~~
   Wrong - it fails in the baseline on amd64 runners too. Environment, not
   architecture.
4. **Registry credentials.** Some groups push; upstream's reusable-test
   workflow logs in to GHCR and Docker Hub first. Those groups cannot pass on
   a laptop with no tokens, and should be excluded rather than pretended at.

Only (1) and (2) were ours to get wrong, and both were. (3) and (4) are why
this measurement belongs on a runner.

## What is actually expensive, measured rather than counted

`+test-no-qemu-group1`, three workers, `--ci -P`:

```text
[wire] cache cost ms  : 9335980 total, worst first
   2423490ms   144 leads  go-mod
   2307286ms   138 leads  go-build
   2302602ms   137 leads  //go/pkg/mod
   2302602ms   137 leads  //root/.cache
```

Go module download and Go build are the whole story. `//go/pkg/mod` and
`//root/.cache` are the same leads again under earthly's path-derived cache
ids, not separate costs.

**The total is not wall time.** A lead naming four cache ids adds its
milliseconds to all four rows, so they overlap by construction. The ranking
is sound and the per-lead figure is sound - about 16.8s behind `go-mod` -
the sum is not.

Counting the Earthfile would have pointed elsewhere: `npm` and `go-build`
appear eighteen times each and `go-mod` eleven, which ranks `npm` joint-first
on frequency and last on cost. Frequency in the source says nothing about
seconds.

### What this does and does not justify

A cache mount PERSISTS on a worker's daemon across leads, so within one run
the first lead pays and the rest are warm. The figure above is an average
over both, which means it is NOT a cold-start penalty and should not be sold
as one.

The cost that seeding would remove is the one paid at the start of every
FRESH run - a CI runner begins with nothing, and three of them each download
the Go module graph before the first test executes. That is a
between-generations problem, which is what the bank is for, and it is not
addressed by anything in this repo yet.

Measured on one machine and one group. Before building a seeding mechanism
the same table should come off a runner, because a laptop with a warm
`~/.cache` is the environment least able to see this cost.

## Group by group, against a baseline

Upstream runs `+test-no-qemu` as twelve separate jobs, so that is how it is
measured here. `scripts/parity.sh` runs each group twice - once against a bare
daemon, once through three workers - and compares the SET of failed targets.

| group | baseline | fleet | routed / solves |
| ------ | -------- | -------- | --------------: |
| group1 | 1 failed | 1 failed, the same one | 248 / 286 |
| group2 | green | **green** | 84 / 123 |
| group3 | green | **green** | 88 / 136 |
| group4 | green | **green** | 215 / 341 |

Three groups pass through the distributed builder outright. group1's single
failure is `./t/autocompletion+test-no-parent-at-root-from-home`, which diffs
a directory listing and disagrees about `../run/` on arm64 - it fails
identically with no proxy at all.

The comparison is what makes this worth anything. "The build is green" would
have been a claim about this laptop; "the fleet did not change the answer" is
a claim about the fleet, and it is the only one a distributed builder can
support. It also catches the opposite error: a target failing only WITHOUT
the fleet means the two runs were not like-for-like and the comparison proves
nothing.

## On runners, at last

`+code` through the fleet on a GitHub runner, with the same parity
comparison the local harness uses:

```text
workers joined : 2/2
PARITY         : the same 0 target(s) failed either way
solves routed  : 6 of 6      worker 1: 4, worker 2: 2
wall           : baseline 12s, fleet 68s
```

Getting there took eight CI rounds, and every failure was environmental
rather than a defect in the fleet:

| round | failure | cause |
| ----- | ------------------------------- | ----------------------------------- |
| 1 | `manifest unknown` | bootstrap discovered an unpublished image |
| 2 | daemon never ready | `$(printf)` ate the newline between TOML stanzas |
| 3 | 4 of 51 checks reported | `set -e` killed the reporter, not the tests |
| 4 | `TLS CA file not found` | tls_enabled defaults true, in both workflows |
| 5 | `BK0: unbound variable` | `$GITHUB_ENV` does not reach its own step |
| 6 | green, `routed: 0` | the two policy env vars were never set |
| 7 | `workers joined: 3/2` and `0/2` | matrix jobs shared one rendezvous |
| 8 | group1 permanently red | CI demanded green instead of parity |

Two are worth keeping.

**Round 7** is the only bug in this list that a laptop could never have
found. `SESSION="earthfile-${GITHUB_RUN_ID}"` is unique per run and NOT per
matrix job, and the mesh derives the driver's identity from it - so four
fleets on four runners advertised the same driver and iroh connected them
across job boundaries, exactly as designed. One job reported three workers
when two were started; another reported none. It presents as a discovery
bug, a firewall, or a flaky barrier.

**Round 6** is the one to be embarrassed about. Three targets went green with
`solves routed: 0`, because `REBUCK2_PEER_CACHE_MOUNTS=1` and
`REBUCK2_HOME_SLOTS=0` are in every local probe command in this repo and had
never reached the workflow. A green run that measures nothing is worse than a
red one: it looks like the answer and is not the question.

The wall clock is worse through the fleet, and will stay worse while workers
share a runner with the coordinator. Two workers on four cores is contention
plus real transfer; `routed` is the number this measures.

## Four machines, one build

`+code`, one coordinator runner and three worker runners, each a separate
GitHub-hosted machine:

```text
workers joined : 3/3
solves routed  : 6 of 6      built at home: 0     not routed: {}
spread         : worker 1: 3, worker 2: 2, worker 3: 1
blobs          : worker 3  local=0 peer=3 driver=12
wall           : 68s
```

Every solve of a real earthbuild target built on a machine other than the one
that asked for it, and three of the blobs moved worker-to-worker without
touching the coordinator.

Getting here needed one idea applied three times, and the first two
applications did not reveal the third:

| what crosses a machine | was named by | now |
| ---------------------- | ------------ | ------- |
| subtree results | tag | digest |
| base images | tag | digest |
| contexts | tag | digest |

Each fix exposed the next, because a graph stops at the first thing it cannot
pull. And **none of them is visible on one machine**: there,
`172.17.0.1:15000` is the same registry for every participant and every tag
resolves. A single-host fleet cannot test the property that makes a fleet
worth having.

> Anything a peer must fetch is named by CONTENT. A tag is a name in one
> machine's namespace, and a fleet has no namespace.

The driver still serves most blobs - 12 against 3 on the worker that used the
mesh at all - because the fleet is cold and the bloom filters have almost
nothing in them on a first run. That ratio is the thing to watch as runs
accumulate, and it is the argument for banking between generations rather
than a claim about it.

68s of wall on `+code` against 12s for the baseline. Distribution still costs
more than it saves at this size, which is what a six-solve target should be
expected to show.

## Six machines, a real test group

`+test-no-qemu-group2` - 123 solves - across one coordinator runner and six
worker runners, all separate GitHub-hosted machines:

```text
workers joined : 6/6
solves routed  : 84        built at home: 39
not routed     : {"considered": 123, "excluded: Insecure []": 39}
spread         : 38 / 23 / 10 / 5 / 6 / 2
blobs          : worker 1  peer=61 driver=14
                 worker 2  peer=64 driver=5
                 worker 3  peer=37 driver=30
wall           : 773s
```

84 routed plus 39 refused is 123: **everything that could legitimately move,
moved**, and every refusal is `Insecure` - privileged exec, which is a trust
decision and never lifted.

### The ratio inverts as the fleet grows

| machines | peer fetches | driver fetches | driver's share |
| -------: | -----------: | -------------: | -------------: |
| 3 | 3 | 12 | 80% |
| 6 | 162 | 49 | 23% |

This is the claim the whole design rests on - the driver arbitrates and
carries as little as possible - and it is the first evidence that the
carrying part improves rather than degrades with fleet size. A coordinator
that served every blob would be the bottleneck that makes nineteen workers
pointless.

Not proof that it scales to nineteen. It is two points, on one target, and
the second is far more favourable than the first partly because six cold
workers give each other more to find.

### What the spread says

38 / 23 / 10 / 5 / 6 / 2 is not balance. `offer_order` sorts emptiest-first
and offers one peer at a time, so the worker that answers earliest keeps
winning while the others are still starting their daemons. On a longer target
that self-corrects as load accrues; on a 123-solve group it does not get the
chance.

Worth fixing only if it costs wall clock, which needs the baseline this
workflow does not yet run.

## The fleet cache made it worse

Six machines, `+test-no-qemu-group2`, one variable:

| fleet cache | baseline | fleet | delta |
| ----------- | -------: | ----: | ----: |
| off | 307s | 590s | +283s |
| on | 321s | 755s | +434s |

Sharing a buildkit registry cache across the fleet cost a further **165s**.

The reasoning behind it was sound and is worth keeping, because the numbers
that motivated it have not changed: `go-mod` costs ~24.2s per lead and
`go-build` ~24.7s, a cache mount does not travel, and six machines therefore
pay six times what one machine pays once. That is still where the 283s goes.

What was wrong was the remedy. `mode=max` exports the whole layer set after
EVERY solve, and there were 84 of them: eighty-four uploads of a cache that
grows as it goes, plus eighty-four imports of a manifest that grows with it.
The cache costs more to move than the download it saves.

Things this rules out, and things it does not:

- **Ruled out**: export-per-solve to a shared ref. The write amplification is
  proportional to the number of leads, which is the number this fleet is
  trying to increase.
- **Not ruled out**: import-only, from a cache somebody else populated once.
  A worker that reads a warm `go-mod` and never writes one pays the transfer
  a single time.
- **Not ruled out**: `mode=min`, which caches far less and might move fast
  enough to be worth it.
- **Not addressed at all**: between-generation seeding. Every run here starts
  from nothing; the cost of a FRESH runner is the one the bank was meant for,
  and nothing above touches it.

Defaulted off before measuring, which is the only reason this is a finding
rather than a regression. A speedup that ships on cannot be distinguished
from one that does not work.

## Two corrections, and where the time actually is not

**The graph is parallel.** Peak concurrency on `group2` across six machines
was **9 subtrees in flight at once** - more than the six workers available.
The build is not a chain, the fleet is not starved of work, and the 383s gap
is not a critical path. That was worth ruling out and it is now ruled out.

**"go-mod costs 24.2s per lead" was an overstatement, and mine.** The cache
table attributes each lead's ENTIRE duration to every cache id the subtree
names. It says "leads mentioning go-mod average 24.2s", not "24.2s is spent
on go-mod". Those are different claims and only the first is measured. A
subtree naming four cache ids contributes its whole duration to all four -
the overlap was flagged when the table was built, and then quietly dropped
when the number got quoted.

So the cache is a suspect, not a finding, and the cache A/B was aimed by a
figure that could not support it.

### What is actually known

| | |
| ---------------------------- | ------------------------------- |
| baseline, one machine | 277s |
| fleet, six machines | 660s |
| leads dispatched | 84 |
| peak concurrent subtrees | 9 |
| average lead duration | ~24s |

84 leads at ~24s each, run about six-wide, is ~336s of worker time - which
is most of the gap on its own. The same work costs the baseline 277s in
total, so **a unit of work costs far more on a worker than at home**, and
the multiplier is the thing to explain. Candidates, none measured:

- the mirror hop: a worker pulls base and context from a registry rather
  than reading its own store
- cold FS cache per worker for content the baseline touched once
- earthly's own per-solve overhead paid 84 times instead of inline

The next measurement is a breakdown of one lead - fetch versus build - not
another remedy. Three remedies have now been tried against a number that did
not mean what it was taken to mean.

## Fewer machines are faster, which is the whole diagnosis

`+test-no-qemu-group2`, same everything, only the fleet size changed:

| workers | baseline | fleet | penalty |
| ------: | -------: | ----: | ------: |
| 6 | 277s | 660s | +383s |
| 2 | 299s | 401s | +102s |

A parallel graph that gets SLOWER with more machines is not a scheduling
problem. It is work being multiplied.

The lead durations say it again:

```text
n=40  min=918ms  p50=21679ms  p90=96000ms  max=159877ms  mean=30155ms
```

A 170x spread between the fastest lead and the slowest. The early leads on
each worker take up to 160 seconds; once that worker is warm they drop below
a second.

### What is actually happening

Every solve's LLB graph is a DAG rooted at its target and contains its whole
ancestor chain. Dispatched subtrees measure ~106-109 ops out of a 123-solve
build, and every lead reports `0 frontier blobs` - nothing is handed over
pre-built, so the worker executes the chain from the base image up.

BuildKit dedupes within one daemon, so a worker pays that prefix ONCE and its
remaining leads are nearly free. Which means:

| | one machine | N machines |
| ----------------- | ----------- | ---------------------- |
| the shared prefix | built once | built N times |
| the leaf work | once | genuinely divided by N |
| transfer | none | base + context per worker |

The leaf work is the small remainder. Adding a machine adds a prefix rebuild
and subtracts a fraction of the remainder, and the first term is larger - so
the curve goes the wrong way, exactly as measured.

This also retires the cache investigation as a fix rather than a symptom. A
warm `go-mod` would shave part of a prefix that should not be built six times
in the first place.

The unit being distributed is the problem, not the algorithm that places it.

## The duplication, counted - and two claims of mine corrected

`+test-no-qemu-group2`, two workers:

```text
2568 ops sent / 142 distinct = 18.1x sent
238 (op,worker) pairs        =  1.7x built
```

**Correction 1: the prefix story was overstated.** "Every worker rebuilds the
shared ancestry" predicts a multiplier near the worker count. Measured
execution duplication is **1.7x against a ceiling of 2.0** - real, but 1.7x of
the work spread over two machines is close to break-even and cannot on its own
explain a 65% slowdown. The 18.1x is DISPATCH duplication, which costs almost
nothing because buildkit dedupes within a daemon. The dramatic figure was
quoted before the honest one existed.

**Correction 2: three cache configurations, three regressions.**

| configuration | baseline | fleet |
| ---------------------------- | -------: | ----: |
| no cache, 6 workers | 277s | 660s |
| no cache, 2 workers | 299s | 401s |
| read-only (no writer at all) | 307s | 658s |
| export per solve (84 writers) | 321s | 755s |
| one reference export + imports | 303s | 498s |

The last was the shape the prior art recommends, and it still lost 97s to no
cache at all - for a reason visible in the code as written: the export runs on
the CLIENT's build, inside the measured window, pushing a mode=max cache
before the run ends. **A single run cannot benefit from a cache it is itself
producing.**

Which is what this document said days ago - "between-generation seeding ...
the cost of a FRESH runner is the one the bank was meant for" - and what three
experiments have now confirmed by contradiction. Every run on a GitHub runner
starts empty. There is nothing to import, and manufacturing something to
import costs more than it returns within the same run.

### So where does the 100-380s actually go?

Not, on this evidence, mostly into rebuilt ancestry. Still unaccounted:

- 1.7x duplicated execution - real, but small
- transfer: base image and context pulled per worker
- earthly's per-solve overhead, paid 84 times through a gateway rather than
  inline
- the mirror hop: a worker reads inputs from a registry where home reads its
  own content store

The next measurement is a breakdown of one lead into fetch versus execute.
That has been the next measurement for a while, and three remedies have been
attempted ahead of it.

## How noisy is any of this? 18%

Three identical runs - same target, same three workers, same flags, nothing
changed between them:

```text
129s   154s   135s      range 25s on a 139s mean = 18%
```

Scaled to the `group2` fleet numbers (400-660s), that is 70-120 seconds of
noise. Which re-sorts every A/B reported today:

| comparison | delta | verdict |
| ---------------------------- | ----: | ------------------- |
| 6 workers vs 2 workers | 259s | holds, comfortably |
| readwrite cache vs off | 165s | probably holds |
| one reference export vs off | 97s | NOT established |
| read-only vs off | 68s | NOT established |

Two of the four regressions I reported are indistinguishable from noise. The
ARGUMENT against each still stands on its own - a run cannot import a cache it
is itself producing, and exporting after all 84 solves is 84 writes for one
read - but the numbers were presented as evidence and were not.

**The rule this earns**: on this rig, a difference under ~20% needs at least
three runs before it is written down. That is cheap for `+code` at ~2 minutes
and expensive for `group2` at ~10, which is an argument for developing against
the small target and confirming on the large one, not for skipping the
repeats.

Measured after four remedies had already been chosen and reported.

## Daemon consolidation, recovered - and it moves the goalposts

earthbuild's tests are earthly-in-earthly: `RUN_EARTH` runs
`/usr/bin/earthly-entrypoint.sh` INSIDE a container, and that entrypoint
starts its own buildkitd unless a host is given. Every test's real build
therefore ran on a nested daemon this gateway never saw - and the 39 solves
refused as `Insecure` ARE those nested runs. The fleet was distributing the
cheap 84 and leaving the whole critical path at home.

Solved once already on `giles-single-buildkit-with-dist` and lost. Recovered
as four source patches plus an escape hatch, re-derived onto 3380dc20 because
0005 patches `RUN_EARTHLY`, which upstream renamed to `RUN_EARTH`.

Three things had to be true together, and only the first was in the patches:

1. **0004** - the entrypoint must decide internal-vs-external on the variable
   EARTHLY reads (`EARTH_` first, `EARTHLY_` fallback), not the bare
   `BUILDKIT_HOST`. Without it the container starts a daemon earthly then
   ignores, which is worse than not forwarding.
2. **`force_internal_buildkit`** on the six call sites whose inner Earthfile
   contains `LOCALLY` - re-derived by grep, not copied.
3. **`EARTHLY_TLS_ENABLED=false` in the ENVIRONMENT.** We had TLS off in a
   config FILE, which lives in one container and cannot travel. The nested
   earthly inherited the host and nothing else, defaulted to TLS, and exited
   6. This is what made patch 0001 look redundant.

### The result, and it is not the one expected

| | unconsolidated | consolidated |
| ------------------ | -------------: | -----------: |
| baseline, 1 machine | ~300s | **141s** |
| fleet, 3 workers | 401s | 451s |

**Consolidation more than halves the single machine and barely moves the
fleet.** It is a 2x win, and it is a win for the thing the fleet has to beat.
Removing 39 nested daemon startups helps whoever was paying for them, and on
one machine that is one process paying 39 times; spread over three workers it
was already partly amortised.

The fleet now sees the nested work - 160 gateway solves against 123, and 129
routed - so the mechanism did what it was for. It just did not pay.

### And the fleet changed the answer

`./tests+copy-test-verbose-output` passes on one machine and fails through the
fleet. That is the parity check earning its place: a test asserting on
earthly's own output is exactly the kind that a distributed build can break
without breaking anything real, and it needs diagnosis rather than a
`force_internal_buildkit` sprinkled on it to make the red go away.

## The driver is off the data path, and it changed nothing

Letting the driver redirect a blob fetch to a peer that already holds it
(`GetByHashAs` -> `Provider`):

| | before | after |
| ------------- | -----: | ----: |
| peer fetches | 5 | 36 |
| driver fetches | 38-47 | 5-7 |
| wall | 451s | 436s |

The coordinator went from serving ~130 of 145 fetches to ~19 of ~128.
Principle 6, finally true as a number: the driver arbitrates and carries
almost nothing.

**And the wall clock did not move** - 436s against 451s is inside the 18%
noise. Transfer was never the bottleneck; it was architecturally wrong, which
is a different complaint and worth fixing on its own terms. Both of those are
worth saying, and only one of them is a speedup.

### Where the time actually is

```text
104 leads, 675s of lead time
p50 4.9s   p90 14.6s
49 leads of <=10 ops -> median 521ms
```

Small leads are cheap, so this is not fixed per-solve overhead. The cost is in
the large leads - and 675s of lead-work does what the baseline does in 144s.

That is ~4.7x, and with three workers the ceiling for "every worker rebuilds
the shared ancestry" is 3x plus transfer and coordination. **The fleet is at
its duplication ceiling.** Dispatching a 106-op subtree hands a worker the
whole chain from the base image up, and buildkit dedupes only within one
daemon - so three daemons build it three times where one builds it once.

### The remaining move, and it is the one that was named at the start

Prefixes, in order of preference:

```text
0 prefixes   restored from the bank, across generations
1 prefix     built once, published, imported by every worker
N prefixes   one per worker - where we are
12 prefixes  one per CI job - where upstream is
```

Everything measured today says the gap is the step from N to 1, and nothing
else has moved it: not a shared cache (three configurations, all worse), not
peer-to-peer transfer (correct, and neutral), not scheduling (the graph is
parallel, peak 9). The subtree has to arrive with its ancestry already built
and named as content, which is this repo's own principle 10 - hand over trees,
not vertices - unimplemented at the point where it matters.
