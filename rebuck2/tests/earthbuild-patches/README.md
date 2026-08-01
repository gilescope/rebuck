# earthbuild consolidation patches

The daemon-consolidation change, as patches against `EarthBuild/earthbuild`
commit `32308f78` -- the commit where `+test-no-qemu-group1` passes on the x86
box at 329s, so the box control and any CI arm measure the same tree.

Vendored here rather than referenced as a branch on purpose. A `ref:` in a
workflow points at something that can move or be deleted while the numbers it
produced stay in a summary; a patch file cannot. `git am` also fails loudly if
the base drifts, where a branch checkout would quietly measure something else.

| patch | what it does |
| ----- | ------------ |
| 0001  | `config`: TLS settable by env. A config file lives in one container and cannot reach a nested build; only an env var travels. |
| 0002  | `earthfile2llb`: forward `BUILDKIT_HOST`/`TLS_ENABLED` into every RUN, so a nested earthly finds the daemon that started it. Explicit allowlist -- `EARTH_*` on a build host carries credentials. |
| 0003  | forward the CLI's address, not `tcp://buildkitsandbox:8372`. The sandbox constant only resolves under host networking; this rig uses CNI. |
| 0004  | `earthly-entrypoint.sh`: decide internal-vs-external on the address **earthly** reads (`EARTH_` first, `EARTHLY_` fallback), not the bare `BUILDKIT_HOST`. **This is the one that makes consolidation real** -- 0001-0003 alone start a daemon that earthly then ignores, which is worse than not forwarding at all. |
| 0005  | `force_internal_buildkit`, set on the five call sites whose inner Earthfile contains `LOCALLY`. `LOCALLY` runs on the *daemon's* host, so a shared daemon writes those files on the wrong machine. |

Measured on the box, `./tests/command-to-function-rename+all`, counting the
internal branch's own `running under pid=` line:

| tree                  | nested daemons | result |
| --------------------- | -------------- | ------ |
| `32308f78` pristine   | 4              | PASS   |
| + 0001-0003           | 5              | FAIL   |
| + 0004                | 0              | PASS   |

To refresh after further work on the box:

```sh
git -C ~/git/EarthBuild/earthbuild7 format-patch -o /tmp/ebpatches \
    --no-signature 32308f78..HEAD
```
