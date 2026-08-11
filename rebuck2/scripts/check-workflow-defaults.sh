#!/usr/bin/env bash
# Does a workflow input's default agree with the fallback beside it?
#
# `REBUCK2_GRAFT: ${{ inputs.graft || '0' }}` reads as "off unless asked
# for". It is not, if the `graft:` input declares its own default of "1":
# the value is then never empty, the `|| '0'` is unreachable, and the
# mechanism runs in every run. That is how grafting - measured at +107s -
# stayed on for a day after the commit that turned it off, and how several
# comparisons were taken with a mechanism nobody believed was running.
#
# Two places state a default and only one of them wins. This checks they
# agree.
set -euo pipefail
cd "$(dirname "$0")/../.."

python3 - "$@" <<'PY'
import re, sys, pathlib, glob
bad = 0
for path in sorted(glob.glob(".github/workflows/*.yml")):
    s = pathlib.Path(path).read_text()
    inp, cur = {}, None
    for line in s.split("\n"):
        m = re.match(r"^      ([a-z_0-9]+):\s*$", line)
        if m:
            cur = m.group(1)
            continue
        m = re.match(r"""^        default: ['"]?([^'"]*)['"]?\s*$""", line)
        if m and cur:
            inp[cur] = m.group(1)
    for m in re.finditer(r"^  ([A-Z_0-9]+): \$\{\{ inputs\.([a-z_0-9]+) \|\| '([^']*)' \}\}", s, re.M):
        env, name, fb = m.groups()
        d = inp.get(name)
        if d is not None and d != "" and d != fb:
            print(f"{path}: {env} falls back to {fb!r} but input {name} defaults to {d!r}")
            print("   the fallback is unreachable; the input default is what runs")
            bad += 1
sys.exit(1 if bad else 0)
PY
echo "workflow defaults agree"
