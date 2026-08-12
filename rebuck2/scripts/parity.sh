#!/usr/bin/env bash
# Does the fleet change the ANSWER?
#
# Not "is the build green" - that is a question about the machine, its
# credentials and its architecture, and it was red here before any of this
# existed. The only question a distributed builder can be held to is whether
# it produces the same result as not using it.
#
# Measured on `+test-no-qemu-group1`: one failed target either way, the same
# one, an autocompletion test that diffs a directory listing and disagrees
# about `../run/` on arm64. Chasing that to green would have been chasing
# someone else's bug; not measuring the baseline first would have meant not
# knowing that.
#
#   scripts/parity.sh [target] [workers]
#
# Baseline-then-bisect, from CLAUDE.md: reproduce a known state, confirm it,
# then move ONE variable. Here the variable is the fleet.
set -euo pipefail

TARGET=${1:-+test-no-qemu-group1}
WORKERS=${2:-3}
EB=${EB:-$HOME/git/EarthBuild/earthbuild}
RUN=${RUN:-${TMPDIR:-/tmp}/rebuck2-parity}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$(cd "$HERE/.." && pwd)/target/release/rebuck2}
EARTHLY_BIN=${EARTHLY_BIN:-earthly}
BASE_BK=${BASE_BK:-rebuck2-parity-bk}
BASE_PORT=${BASE_PORT:-29371}

[ -d "$EB" ] || { echo "no earthbuild checkout at $EB (set EB=)"; exit 1; }
[ -x "$BIN" ] || { echo "build rebuck2 --release first (or set BIN=)"; exit 1; }

# The flags upstream uses, and they are not optional.
#
#   --ci  changes output mode and strictness
#   -P    is what makes earthly request the security.insecure entitlement;
#         without it every WITH DOCKER target dies as
#         `failed to load LLB: security.insecure is not allowed`, which reads
#         like the proxy stripped something and does not
#
# EARTHLY_VERSION_FLAG_OVERRIDES is thirteen feature flags the tests assume.
# Running without it does not fail loudly - it fails as individual tests
# behaving differently, which is indistinguishable from a fleet bug.
FLAGS=${FLAGS:---ci -P}
OVERRIDES=$(tr -d '\n' < "$EB/.earthly_version_flag_overrides")

# Traces, if somewhere is listening for them.
#
# earthly emits OTLP spans through the standard autoexport, and they carry
# per-target timing directly - the thing this script otherwise infers from a
# wall clock and a failed-target set. The collector on the x86 box keeps them
# across runs, which a CI artifact cannot.
#
# OFF unless OTLP_ENDPOINT is set, and that is not tidiness: pointed at a
# collector that is not there, earthly retries every export and logs
# `traces export: exporter export timeout` until it gives up, which slows the
# very thing being measured.
if [ -n "${OTLP_ENDPOINT:-}" ]; then
  export OTEL_TRACES_EXPORTER=otlp
  export OTEL_EXPORTER_OTLP_PROTOCOL=grpc
  export OTEL_EXPORTER_OTLP_ENDPOINT="$OTLP_ENDPOINT"
  # Self-signed on the box: encrypt the token and the span contents without
  # pretending to authenticate the server.
  export OTEL_EXPORTER_OTLP_INSECURE_SKIP_VERIFY=true
  [ -z "${OTLP_TOKEN:-}" ] || export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer ${OTLP_TOKEN}"
  # Which leg a span belongs to, or the two are indistinguishable in the
  # collector and the comparison this script exists for cannot be made.
  export OTEL_SERVICE_NAME="earthly"
  echo "== traces -> $OTLP_ENDPOINT"
fi

# NOT loopback, for either half. earthly's `IsLocal` counts 127.0.0.1 as "a
# buildkit I manage" and tries to (re)start its own container from the image
# compiled into the binary - which for a source build is
# `buildkitd-dev-main`, unpublished, so the run dies on `manifest unknown`
# before a single solve happens. Cost one baseline run to rediscover.
LAN=${LAN:-$(ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}')}
[ -n "$LAN" ] || { echo "no non-loopback address found (set LAN=)"; exit 1; }

rm -rf "$RUN"; mkdir -p "$RUN"
cleanup() { docker rm -f "$BASE_BK" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# `\*failed\*` is earthly's own marker for a target that failed, one line per
# occurrence; sorted and deduped it is the set of targets that did not pass.
failed_set() {
  # `|| true`, and the reason is worth stating: a build with NO failures
  # makes grep exit 1, and under `set -e` that killed this script - right
  # after the baseline came back green. The harness for measuring success
  # could only survive failure.
  # LEADING WHITESPACE is allowed, and that is not cosmetic. earthly
  # right-aligns the target column to the longest target name in the run, so
  # these lines are indented in some runs and not others - and the anchored
  # form reported ZERO failed targets for a CI run whose log plainly showed
  # `./tests+fail-test *failed*`, which then compared the wrong sets.
  grep -aoE "^[[:space:]]*[^[:space:]]+ \*failed\*" "$1" 2>/dev/null \
    | sed 's/^[[:space:]]*//;s/ \*failed\*//' | sort -u || true
}

echo "== baseline: $TARGET with no fleet at all"
export OTEL_RESOURCE_ATTRIBUTES="rebuck2.leg=baseline,rebuck2.target=$TARGET"
docker rm -f "$BASE_BK" >/dev/null 2>&1 || true
BK_IMAGE=${BK_IMAGE:-earthbuild/buildkitd:v0.8.17}
docker run -d --name "$BASE_BK" --privileged -p "$BASE_PORT:8372" \
  -e BUILDKIT_TCP_TRANSPORT_ENABLED=true -e BUILDKIT_TLS_ENABLED=false \
  "$BK_IMAGE" >/dev/null
for _ in $(seq 1 90); do
  docker exec "$BASE_BK" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1 && break
  sleep 1
done
printf 'global:\n  tls_enabled: false\n' > "$RUN/earthly.yml"
start=$SECONDS
# shellcheck disable=SC2086 # deliberate word-splitting: FLAGS is a flag list
# TLS off by ENV, not only by config file, for the SAME reason the fleet leg
# needs it: under daemon consolidation a nested earthly inherits its settings
# through the forwarded environment, and a config file lives in one container.
# Without this the baseline's nested builds dial the shared daemon, default to
# TLS, and exit 6 - so the baseline fails while the fleet leg passes and the
# comparison measures the difference between two rigs rather than one variable.
( cd "$EB" && EARTHLY_CONFIG="$RUN/earthly.yml" \
    EARTHLY_BUILDKIT_HOST="tcp://$LAN:$BASE_PORT" \
    EARTHLY_TLS_ENABLED=false EARTH_TLS_ENABLED=false \
    EARTHLY_VERSION_FLAG_OVERRIDES="$OVERRIDES" \
    "$EARTHLY_BIN" $FLAGS "$TARGET" >"$RUN/baseline.log" 2>&1 ) && b_ok=yes || b_ok=no
echo "   baseline: $b_ok in $((SECONDS - start))s"
failed_set "$RUN/baseline.log" > "$RUN/baseline.failed"
docker rm -f "$BASE_BK" >/dev/null 2>&1 || true

echo "== the same target, through the fleet"
export OTEL_RESOURCE_ATTRIBUTES="rebuck2.leg=fleet,rebuck2.target=$TARGET"
start=$SECONDS
RUN="$RUN/fleet" BIN="$BIN" EARTHLY_BIN="$EARTHLY_BIN" \
  EARTHLY_TRUST_CONFIG=1 EARTHLY_ARGS="$FLAGS" \
  EARTHLY_VERSION_FLAG_OVERRIDES="$OVERRIDES" \
  REBUCK2_PEER_CACHE_MOUNTS=1 REBUCK2_HOME_SLOTS=0 \
  "$HERE/earthly-probe.sh" "$TARGET" "$WORKERS" >"$RUN/fleet.out" 2>&1 || true
echo "   fleet: $((SECONDS - start))s"
failed_set "$RUN/fleet/earthly.log" > "$RUN/fleet.failed"

echo
echo "== did the fleet change the answer?"
if diff -u "$RUN/baseline.failed" "$RUN/fleet.failed" > "$RUN/parity.diff"; then
  n=$(wc -l < "$RUN/baseline.failed" | tr -d ' ')
  echo "   PARITY: the same $n target(s) failed either way"
  [ "$n" = 0 ] || sed 's/^/     /' "$RUN/baseline.failed"
else
  # A DIFFERENCE either way is a finding. Targets that fail only with the
  # fleet are the obvious bug; targets that fail only WITHOUT it mean the
  # comparison is not like-for-like and the run proves nothing.
  echo "   DIFFERENT - the fleet changed the outcome:"
  sed 's/^/     /' "$RUN/parity.diff"
fi

echo
echo "== and how much of it moved"
grep -E '^\[wire\] (gateway solves|solves routed|built at home)' "$RUN/fleet.out" || true
echo
echo "logs in $RUN"
