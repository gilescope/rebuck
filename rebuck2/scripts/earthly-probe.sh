#!/usr/bin/env bash
# How much of a REAL earthly build can this fleet actually take?
#
# Every dispatch number in this repo comes from a fixture built to be
# dispatchable: three ops, one registry source, no secrets, no host binds. An
# earthly graph is none of those things - `earthly-dispatch.md` predicts one
# solve in twelve survives, because the converter attaches the interactive
# debugger's secret, two sockets and a host bind to EVERY non-LOCALLY RUN.
#
# That prediction has never been checked against a real target. This checks
# it, locally, before anything is asked of CI.
#
#   scripts/earthly-probe.sh [target] [workers]
#
# Leaves ~/.earthly alone: earthly reads EARTHLY_CONFIG, so the registry trust
# this needs goes in a scratch file and the developer's own config is not
# touched.
set -euo pipefail

TARGET=${1:-+code}
WORKERS=${2:-1}
EB=${EB:-$HOME/git/EarthBuild/earthbuild}
RUN=${RUN:-${TMPDIR:-/tmp}/earthly-probe}
BIN=$(cd "$(dirname "$0")/.." && pwd)/target/release/rebuck2
SESSION="probe-$$"
# Ports above the fleet rig's, so this can run while that does.
PROXY_PORT=${PROXY_PORT:-21234}
REG_PORT=${REG_PORT:-25000}

[ -d "$EB" ] || { echo "no earthbuild checkout at $EB (set EB=)"; exit 1; }
[ -x "$BIN" ] || { echo "build rebuck2 --release first"; exit 1; }

rm -rf "$RUN"; mkdir -p "$RUN"
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

# Docker Desktop resolves this; a container reaches the host by it.
MIRROR_HOST=${MIRROR_HOST:-host.docker.internal}

# Registry trust, in a scratch config. Without it the daemon pushes fine and
# cannot pull, so every lead declines and the build succeeds at home having
# distributed nothing - the silent failure, and the one that would make this
# probe report "earthly dispatches nothing" for the wrong reason.
cat >"$RUN/earthly.yml" <<YML
global:
  buildkit_additional_config: |
    [registry."$MIRROR_HOST:$REG_PORT"]
      http = true
      insecure = true
YML
export EARTHLY_CONFIG="$RUN/earthly.yml"

echo "== earthly's own buildkitd"
# Its converter emits fork-only ops, so the daemon must be the one earthly
# picked, not moby's.
earthly bootstrap >/dev/null 2>&1 || true
BK=$(docker ps --filter 'name=buildkitd' --format '{{.Names}}' | head -1)
[ -n "$BK" ] || { echo "earthly started no buildkitd"; exit 1; }
BK_ADDR=$(docker port "$BK" 8372 2>/dev/null | head -1)
[ -n "$BK_ADDR" ] || { echo "$BK publishes no 8372 - cannot proxy it"; exit 1; }
echo "   $BK on $BK_ADDR"

echo "== coordinator"
REBUCK2_MIRROR="$MIRROR_HOST:$REG_PORT" \
  "$BIN" buildkit-proxy --listen "127.0.0.1:$PROXY_PORT" \
    --upstream "http://$BK_ADDR" \
    --registry-bind "0.0.0.0:$REG_PORT" \
    --session "$SESSION" --store "$RUN/coord" >"$RUN/proxy.log" 2>&1 &
pids+=("$!")

for i in $(seq 1 "$WORKERS"); do
  "$BIN" worker --session "$SESSION" \
    --store "$RUN/worker-$i" \
    --buildkit-addr "http://$BK_ADDR" \
    --registry-addr "$MIRROR_HOST:$REG_PORT" >"$RUN/worker-$i.log" 2>&1 &
  pids+=("$!")
done

for _ in $(seq 1 40); do
  [ "$(grep -c joined "$RUN/proxy.log" 2>/dev/null || echo 0)" -ge "$WORKERS" ] && break
  sleep 1
done
echo "   workers joined: $(grep -c joined "$RUN/proxy.log" 2>/dev/null || echo 0)/$WORKERS"

echo "== earthly $TARGET through the proxy"
start=$SECONDS
( cd "$EB" && EARTHLY_BUILDKIT_HOST="tcp://127.0.0.1:$PROXY_PORT" \
    earthly "$TARGET" >"$RUN/earthly.log" 2>&1 ) && ok=yes || ok=no
echo "   build: $ok in $((SECONDS - start))s"

kill -INT "${pids[0]}" 2>/dev/null || true
for _ in $(seq 1 20); do
  grep -q "^\[wire\] placed" "$RUN/proxy.log" && break
  sleep 1
done

echo
echo "== what the graph looked like"
grep -E '^\[wire\] (gateway solves|ops total|sources|platforms)' "$RUN/proxy.log" || true
echo "== how much could move, and why not"
grep -E '^\[wire\] (solves routed|built at home|placed|not routed)' "$RUN/proxy.log" || true
echo "== which worker took what"
grep -oE -- '-> worker [0-9]+' "$RUN/proxy.log" | sort | uniq -c || true
echo
echo "logs in $RUN (earthly.log has the build itself)"
