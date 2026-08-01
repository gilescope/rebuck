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
- `34b38e7` rig sets TLS by env, so it reaches the nested build

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
- **Consolidation removes per-test daemons, and costs nothing measurable.**
  `+test-no-qemu-group1`, same box, same fork, same base commit `32308f78`:

  | arm                          | solo | pair | nested daemons |
  | ---------------------------- | ---- | ---- | -------------- |
  | no consolidation (control)   | 329s | 373s | one per test   |
  | consolidated                 | 311s | 389s | **none**       |

  ALL TARGETS PASSED, `MERGED=30`, `resolve_MERGED=22`. The 18s is n=1 against a
  cell whose own spread is ~10% (SF-ON solo has measured 329s and 361s), so read
  it as *no detectable difference*, not as a win. Peak buildkitd during the run
  was 2 -- the rig's own daemons and nothing else.
- **Counting daemons is the honest instrument.** `earthly-entrypoint.sh` prints
  `running under pid=` only on its internal-buildkit branch, so grepping for it
  counts nested daemons exactly. On `./tests/command-to-function-rename+all`:
  pristine **4**, forwarding alone **5**, forwarding + the entrypoint fix **0**.
  Forwarding on its own was worse than not doing it, and only that count says so.
- **A single test sees no speedup.** `parser-smoke-test` 124s consolidated
  against a 120-126s baseline. Expected: one daemon saved out of one.

## Not proven

- **That consolidation is *faster*.** It is not slower, which is the claim the
  numbers support; every cell is n=1 and the effect is inside the noise. Three
  to five repeats per cell before either number means anything. Do not use the
  CI figures (483s/471s) as a control -- different hardware, stock buildkitd.
- **P4b (serve image layers from the mesh).** Designed in `buildkit-plan.md`,
  not built. The agent already serves fleet-wide (`worker.rs:703` falls through
  to `fetch_over_mesh`); the gap is POPULATION -- a blob nobody holds 404s and
  buildkit fetches it upstream, so the bytes never enter the fleet.
- **Inner-buildkit single-flight.** Moot under consolidation -- there is no inner
  buildkit left to coordinate. It becomes a question again only for the five
  `force_internal_buildkit` call sites.

## The blocker, resolved

Recorded as: *forwarding couples the inner earthly to the outer daemon's
frontend, and version-sensitive fixtures then parse on the wrong side.* That was
wrong on both halves.

**The error was not the failure.** `function recipes must start with FUNCTION` is
the *correct* output of a `should_fail=true` test sitting beside the real one in
interleaved parallel output. The failure was eleven lines further down:

```text
ln: /etc/ca.pem: File exists
```

**The mechanism was a disagreement about a variable name.**
`earthly-entrypoint.sh` decided "external if given, internal otherwise" on the
bare `BUILDKIT_HOST`; earthly resolves `EARTH_BUILDKIT_HOST` first and falls back
to `EARTHLY_` (`internal/env.Lookup`). Forwarding writes the prefixed form. So
the container started a daemon *and* earthly ignored it and used the caller's --
and the internal branch's cert bootstrap, which no longer writes a cert once TLS
is off, left `/etc/ca.pem` a dangling symlink. `[ ! -f ]` is true for a dangling
symlink, so the next `ln` in the same chained RUN died on "File exists", under
`set -e`.

Fixed by making the entrypoint decide on the same set earthly reads. The genuine
opt-out turned out to be a different thing entirely: `LOCALLY` runs on the
DAEMON's host, so a nested `LOCALLY` under a shared daemon writes on the outer
machine. Five call sites -- `locally-in-command`, `locally-in-function`, and the
`if`/`for`/`first-command` fixtures -- set `force_internal_buildkit=true`.
`command-to-function-rename` and `autocompletion`, the two the old note named,
need nothing.

## earthbuild changes (box only, unpushed)

On `earthbuild7`, branch `giles-fwd-on-329`, base `32308f78`:

1. **`config`: TLS settable by env** via `env.Lookup("TLS_ENABLED")`. Every other
   buildkit setting already had an env binding; TLS was config-file only, and a
   file cannot reach a nested container. 6 tests.
2. **`earthfile2llb`: forward buildkit settings into RUN.** Explicit allowlist
   (`BUILDKIT_HOST`, `TLS_ENABLED`) as suffixes, collected CLI-side, threaded
   through `ConvertOpt.ForwardedBuildkitEnv`. 3 tests guard that the list stays
   short, carries nothing credential-shaped, and holds suffixes not full names.
3. **`earthly-entrypoint.sh`: decide on the address earthly will use.** The
   entrypoint read the bare `BUILDKIT_HOST`; earthly reads `EARTH_BUILDKIT_HOST`
   first and falls back to `EARTHLY_`. Forwarding writes the prefixed form, so
   the two disagreed and the container started a daemon that earthly then
   ignored. **This is the commit that makes consolidation real** -- 1 and 2 alone
   made it worse.
4. **`tests`: `force_internal_buildkit`,** set on the five call sites whose inner
   Earthfile contains `LOCALLY`.

Plus one fix found late: `applyFromImage` resets env from the image config, so
forwarding must be re-applied there or a target doing `FROM +base` loses it.

### Worktrees on the box

| dir           | base        | state                                          |
| ------------- | ----------- | ---------------------------------------------- |
| `earthbuild`  | main        | the user's, leave alone                        |
| `earthbuild5` | recent main | 10 experiment commits + a stash. Scrap         |
| `earthbuild6` | `202cf3206` | patches + sandbox fix. Superseded by 7. Scrap  |
| `earthbuild7` | `32308f78`  | 5 commits. **Measurement env**                 |

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
- **An expected failure is not a cause.** The consolidation blocker was recorded
  as `function recipes must start with FUNCTION`, which is the *correct* output
  of a `should_fail=true` test that happened to sit next to the real one. The
  actual failure was `ln: /etc/ca.pem: File exists`, eleven lines down. A
  parallel build interleaves several tests' output, so the loudest error line is
  not reliably the failing one -- find the target marked `*failed*` first, then
  read only its lines.

## Next

1. **Push the earthbuild work.** Four commits on `earthbuild7` are the only
   unpublished piece, and 3 (the entrypoint) is the one that carries the result.
2. **Repeat the cells.** 3-5 runs of control and consolidated before quoting
   311s against 329s as anything but "not slower".
3. **P4b.** Now the only unbuilt item on the plan.

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
