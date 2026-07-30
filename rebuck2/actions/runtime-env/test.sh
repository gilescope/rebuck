#!/usr/bin/env bash
# Local tests for the runtime-env action - no runner, no network.
#   rebuck2/actions/runtime-env/test.sh
set -euo pipefail
cd "$(dirname "$0")"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass=0
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }

node --check index.js || fail "index.js does not parse"
ok "index.js parses"

# ── exports what it finds, masks only the token ─────────────────────
: > "$T/env1"
out=$(GITHUB_ENV="$T/env1" \
  ACTIONS_RUNTIME_TOKEN="tok-secret-value" \
  ACTIONS_RESULTS_URL="https://results.example/" \
  node index.js)
grep -qx 'ACTIONS_RUNTIME_TOKEN=tok-secret-value' "$T/env1" \
  || fail "token not exported: $(cat "$T/env1")"
grep -qx 'ACTIONS_RESULTS_URL=https://results.example/' "$T/env1" \
  || fail "results url not exported"
printf '%s\n' "$out" | grep -qx '::add-mask::tok-secret-value' \
  || fail "token was not masked before export"
printf '%s\n' "$out" | grep -q 'tok-secret-value' \
  && [ "$(printf '%s\n' "$out" | grep -c 'tok-secret-value')" -eq 1 ] \
  || fail "token value appears outside the mask directive"
printf '%s\n' "$out" | grep -q 'ACTIONS_RUNTIME_URL: unset' \
  || fail "absent names should be reported, not exported"
ok "exports present names, masks the token, skips absent ones"

# ── a newline in a value cannot forge further assignments ───────────
: > "$T/env2"
GITHUB_ENV="$T/env2" \
  ACTIONS_RESULTS_URL="$(printf 'https://a/\nEVIL=1')" \
  node index.js > /dev/null
# EVIL=1 appears in the file either way - the question is whether it is
# DATA inside a heredoc or an assignment at the top level. Assert the
# frame: first line opens the delimiter, last line closes it.
delim=$(head -1 "$T/env2" | sed -n 's/^ACTIONS_RESULTS_URL<<//p')
[ -n "$delim" ] \
  || fail "multiline value did not use the delimiter form: $(cat "$T/env2")"
[ "$(tail -1 "$T/env2")" = "$delim" ] \
  || fail "delimiter not closed - EVIL=1 escapes as an assignment"
ok "multiline values use the heredoc form - no assignment injection"

# ── absent credentials: warn by default, fail when required ─────────
: > "$T/env3"
out=$(GITHUB_ENV="$T/env3" node index.js)
printf '%s\n' "$out" | grep -q '::warning::no runtime credentials' \
  || fail "missing credentials should warn by default"
[ ! -s "$T/env3" ] || fail "nothing should be exported when nothing is found"
rc=0
GITHUB_ENV="$T/env3" INPUT_REQUIRED=true node index.js > "$T/out4" || rc=$?
[ "$rc" -eq 1 ] || fail "required=true should exit 1, got $rc"
grep -q '::error::no runtime credentials' "$T/out4" \
  || fail "required=true should emit an error annotation"
ok "absent credentials: warns by default, fails when required"

# ── no GITHUB_ENV at all is a loud error, not a silent no-op ────────
rc=0
env -u GITHUB_ENV ACTIONS_RUNTIME_TOKEN=x node index.js > /dev/null 2>&1 || rc=$?
[ "$rc" -ne 0 ] || fail "missing GITHUB_ENV should fail"
ok "missing GITHUB_ENV fails loudly"

echo "PASS: $pass groups"
