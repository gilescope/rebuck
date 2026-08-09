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
