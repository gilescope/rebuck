# dist-buildkit - handover

State as of 2026-08-01. Written to survive a context reset: what is built, what
is proven, what is not, and the traps that cost the most.

## Shipped and pushed

**buildkit fork** -- `EarthBuild/buildkit`, branch `giles-rebuck-single-flight`:

- `2de4c15b0` P2.1, buildkit identity in the lease key
- `f65523902` `01ff5ffdf` `cd0ace5a3` `64e2a74d5` `41fdfe313` `a8af577e1` P4a,
  resolution coordination

**rebuck2** -- `gilescope/rebuck`, branch `giles-dist-buildkit`:

- `c70349c` driver reports `resolve_led` / `resolve_merged`
- `323098a` preflight reachability assertion
- `d1bf37d` `b7a8895` x86 runner notes and the A/B
- `910e993` `3a7797e` plan P2.1/P4/P4b, principles 8 and 9
- `2737a1b` `75b066f` CI workflows

Not pushed, on the box only: the earthbuild changes (below).

## Measured, and trustworthy

- **P4a works cross-machine.** `leases_merged=32` across two GitHub runners,
  matching the box exactly. Later, with attribution added: `resolve_led=12`,
  `resolve_merged=6` -- six registry round trips skipped.
- **The shared stem is 32 vertices / 94s.** `./tests+base` and
  `+earthbuild-integration-test-base` both measure 32, and that is the ENTIRE
  group1-group2 intersection. So `merged=32` was already optimal; the graph
  shares ~12% of its vertices, not the coordinator underperforming.
- **Single-flight is not faster on an idle box.** Pair 373s coordinated vs 342s
  uncoordinated; solo 361s vs 328s. The gap is the same size as the run-to-run
  noise (SF-ON solo measured 329s and 361s), so the honest reading is *no
  detectable difference*, not "9% slower".
- **Consolidation removes per-test daemons.** On a real group: peak buildkitd
  **6 without forwarding, 2 with**. Both arms measured on the same box.
- **A single test sees no speedup.** `parser-smoke-test` 124s consolidated
  against a 120-126s baseline. Expected: one daemon saved out of one.

## Not proven

- **Whether consolidation is faster on a group.** The box-local control is
  **329s** (`+test-no-qemu-group1`, commit `32308f78`, our fork, no
  consolidation). Under consolidation the same target FAILS, so there is no
  paired number. Do not use the CI figures (483s/471s) as a control -- different
  hardware and a stock buildkitd.
- **P4b (serve image layers from the mesh).** Designed in `buildkit-plan.md`,
  not built. The agent already serves fleet-wide (`worker.rs:703` falls through
  to `fetch_over_mesh`); the gap is POPULATION -- a blob nobody holds 404s and
  buildkit fetches it upstream, so the bytes never enter the fleet.
- **Inner-buildkit single-flight.** Never answered; the rig could not isolate it.

## The blocker, precisely

Consolidation makes group1 fail:

```text
32308f78, no forwarding   PASS 329s
32308f78, + forwarding    Error: some/subdir/Earthfile:7:1
                          function recipes must start with FUNCTION
```

Same commit, same box, same fork -- so forwarding causes it. Mechanism: once the
inner earthly stops starting its own daemon it is coupled to the OUTER daemon's
frontend, and earthbuild's fixtures are version-sensitive on purpose.
`tests/command-to-function-rename/command.earth` declares `VERSION 0.7` and
contains a `FUNCTION` recipe; which side parses it changes the answer.

**Next step**: reinstate `force_internal_buildkit` (it existed, then was removed
during a cleanup) and set it on `command-to-function-rename` and the
`autocompletion` version tests. A handful of targets. Then group1 should
complete under consolidation and the 329s comparison becomes available.

## earthbuild changes (box only, unpushed)

Two keepers, applied as patches in `/tmp/0001-*.patch` and `/tmp/0002-*.patch`:

1. **`config`: TLS settable by env** via `env.Lookup("TLS_ENABLED")`. Every other
   buildkit setting already had an env binding; TLS was config-file only, and a
   file cannot reach a nested container. 6 tests.
2. **`earthfile2llb`: forward buildkit settings into RUN.** Explicit allowlist
   (`BUILDKIT_HOST`, `TLS_ENABLED`) as suffixes, collected CLI-side, threaded
   through `ConvertOpt.ForwardedBuildkitEnv`. 3 tests guard that the list stays
   short, carries nothing credential-shaped, and holds suffixes not full names.

Plus one fix found late: `applyFromImage` resets env from the image config, so
forwarding must be re-applied there or a target doing `FROM +base` loses it.

### Worktrees on the box

| dir           | base        | state                                          |
| ------------- | ----------- | ---------------------------------------------- |
| `earthbuild`  | main        | the user's, leave alone                        |
| `earthbuild5` | recent main | 10 experiment commits + a stash. Scrap         |
| `earthbuild6` | `202cf3206` | patches + sandbox fix. group1 fails (FUNCTION) |
| `earthbuild7` | `32308f78`  | patches + sandbox fix. **Measurement env**     |

`earthbuild7` is the one to keep: its base is the commit where group1 passes on
this box at 329s.

## Traps that cost the most

- **Observe, do not infer.** Every multi-attempt thread ended the moment
  something was looked at: `ps` (which daemon), `strings` (was the binary
  rebuilt), `env` (what a RUN sees), dialling an address, `git checkout`. Four
  wrong attributions came from reasoning about mechanisms instead.
- **Remove a failed fix before trying the next.** Five patches accumulated in
  `RUN_EARTH`, all fighting over two variables, with the worst address winning.
  Reverting the file to pristine took 30 seconds and fixed what layering could
  not.
- **Build a fast loop early.** Debugging a shell-escaping bug through a
  three-minute rig run. A 20-line Earthfile answered the same questions in 30
  seconds.
- **Know which machine and which checkout.** Edited on the Mac, compiled on the
  box, and the binary contained none of the change. `strings` on the artifact
  caught it. Later, `format-patch` exported a fix that only ever existed in a
  working tree.
- **`ISOLATE=1` builds from the COMMIT.** Uncommitted Earthfile edits are
  invisible to the rig -- the target simply "does not exist".
- **`pipefail` is not on by default in GitHub's shell.** `script | tee` reports
  tee's status; a run that died in 26s went green carrying a plausible number.
- **Escaping through nested interpreters.** Python to ssh to Earthfile
  `RUN echo "..."` to shell: a `\n` in `printf` survived as two literal
  characters and produced an unparseable config that earthly ignored in silence.
  Prefer a file and `COPY`.
- **A probe can be less representative than the thing it stands for.** `FROM
  alpine` has no base image to reset env from, so it passed where `FROM +base`
  failed.

## Cleanup owed

- `converter.go` carries `if false {` disabling the sandbox-address branch. The
  cache-key concern behind it is real: a machine-specific address in a forwarded
  RUN makes that vertex unmergeable. `buildkitsandbox` is a buildkit constant and
  would be identical everywhere -- but it only resolves to the daemon under host
  networking, and this rig uses CNI.
- Probe targets committed into `earthbuild5`/`6`/`7` Earthfiles
  (`sf-reach-test`, `fwd-env-test`, `bk-reach-test`, `fwd-dial-test`,
  `fwd-probe`).
- `~/data/rebuck2-perf/*.log` and `/tmp/rebuck2-load.*` on the box hold every
  run's evidence. `/tmp` survives reboots on NixOS but not indefinitely.
