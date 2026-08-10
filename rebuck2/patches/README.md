# Patches applied to third-party trees for measurement

Not fixes for this repo. These change something ELSE so that a measurement
becomes possible at all, and each one is here so a CI run reproduces what was
measured on a laptop rather than approximating it.

## `earthbuild-784-four-sites.patch`

Gates earthly's interactive debugger behind the flag that already exists for
it (`InteractiveDebuggerEnabled || isInteractive`), at four sites in
`earthfile2llb/converter.go`.

Without it, **nothing dispatches**. Every non-`LOCALLY` `RUN` gets the
debugger's secret, its host bind, and two fork-only sockets attached
unconditionally, and all four pin the solve to the machine holding earthly's
session. Measured on `earthly +code`: 0 of 6 solves dispatchable as shipped,
6 of 6 with this applied.

Four sites, and they were found one at a time, each by the failure of the
previous version:

| site | what it attaches | how it announced itself |
| ---- | ------------------------------- | ----------------------------- |
| ~2721 | `earthly_interactive`, `earthly_save_file` sockets | worker: `no active sessions` |
| ~2795 | debugger secret + host bind | `inspect`: excluded on `Secret` |
| ~2806 | `prependDebugger` | `earth_debugger: not found`, exit 127 |

The `prependDebugger` line is the one to notice: gating the mounts without it
leaves every command prefixed with `/usr/bin/earth_debugger`, which existed
only because of the host bind. The build then dies on its first `RUN` with an
error naming no debugger.

`wsl_v5` in earthbuild's own `golangci-lint` rejects the blank-line-free
version of the ~2795 hunk. `earthly +lint` is green with this as it stands -
which is only known because the fleet ran that target.

Upstream: [EarthBuild/earthbuild#784](https://github.com/EarthBuild/earthbuild/issues/784).
Issue only. No PR has been offered and none should be without asking first.

Applies to earthbuild `main` at 3380dc20. If it stops applying, re-derive it
rather than forcing it: the condition to gate on is whatever the surrounding
code already uses for the debugger.
