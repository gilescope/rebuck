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

- Found: 2026-08-08. Not raised upstream; needs consent before any push or PR.
