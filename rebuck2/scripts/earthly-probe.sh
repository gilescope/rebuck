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
# NOT loopback, and this is the whole trick.
#
# earthly's `IsLocal` counts 127.0.0.1, localhost and ::1 as "a buildkit I
# manage", so pointing it at a proxy on loopback makes it compare the
# settings hash of its OWN container, decide they differ, and restart it -
# which on this machine fails on TLS certs the dev install expects. The build
# then dies before a single solve reaches the proxy.
#
# A LAN address is `!isLocal`, so earthly simply connects to it. That is the
# only way to put anything in front of earthly.
LAN=${LAN:-$(ipconfig getifaddr en0 2>/dev/null || hostname -I 2>/dev/null | awk '{print $1}')}
REG_PORT=${REG_PORT:-25000}

[ -d "$EB" ] || { echo "no earthbuild checkout at $EB (set EB=)"; exit 1; }
[ -n "$LAN" ] || { echo "no non-loopback address found (set LAN=)"; exit 1; }
[ -x "$BIN" ] || { echo "build rebuck2 --release first"; exit 1; }

rm -rf "$RUN"; mkdir -p "$RUN"
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

# Docker Desktop resolves this; a container reaches the host by it.
MIRROR_HOST=${MIRROR_HOST:-host.docker.internal}

# NO config override, deliberately.
#
# The first version wrote a scratch config to add registry trust. earthly
# hashes its settings and restarts the daemon when they change, and a restart
# on a developer machine can fail for reasons that have nothing to do with
# this - here, TLS certs the dev install expects and does not have. The probe
# then reported a failed build that never reached the proxy.
#
# Trust is only needed once something actually DISPATCHES. What is being
# measured - how many of earthly's solves survive exclusion - is decided
# before any worker builds anything. So the cheap measurement needs no config
# at all, and if a solve does get dispatched the pull failure is itself worth
# seeing.
# tls_enabled defaults to TRUE, and a REMOTE buildkit is expected to present a
# certificate: earthly goes looking for ~/.earthly*/certs/ca_cert.pem and
# times out after a minute when it is not there. The gateway speaks plain
# gRPC, so this has to be off.
#
# Safe to write now, where it was not before: a config change alters the
# settings hash, and earthly only acts on that for a buildkit it MANAGES. A
# routable address is not one of those, so nothing gets restarted.
{
  echo "global:"
  echo "  tls_enabled: false"
  if [ -n "${EARTHLY_TRUST_CONFIG:-}" ]; then
    echo "  buildkit_additional_config: |"
    echo "    [registry.\"$MIRROR_HOST:$REG_PORT\"]"
    echo "      http = true"
    echo "      insecure = true"
  fi
} >"$RUN/earthly.yml"
export EARTHLY_CONFIG="$RUN/earthly.yml"

echo "== earthly's own buildkitd"
# Its converter emits fork-only ops, so the daemon must be the one earthly
# picked, not moby's.
earthly bootstrap >/dev/null 2>&1 || true
BK=$(docker ps --filter 'name=buildkitd' --format '{{.Names}}' | head -1)
[ -n "$BK" ] || { echo "earthly started no buildkitd"; exit 1; }
# Whatever it publishes, not a port we assumed. v0.8.17 uses 8371 and maps it
# to a dynamic host port; asking for 8372 got nothing and the probe exited
# with no explanation. Same class of guess as hardcoding the image.
BK_ADDR=$(docker port "$BK" | head -1 | awk -F' -> ' '{print $2}')
[ -n "$BK_ADDR" ] || {
  echo "$BK publishes no ports - cannot proxy it:"; docker port "$BK"; exit 1;
}
echo "   $BK on $BK_ADDR"

echo "== coordinator"
REBUCK2_MIRROR="$MIRROR_HOST:$REG_PORT" \
  "$BIN" buildkit-proxy --listen "0.0.0.0:$PROXY_PORT" \
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
  j=$(grep -c joined "$RUN/proxy.log" 2>/dev/null) || j=0
  [ "$j" -ge "$WORKERS" ] && break
  sleep 1
done
j=$(grep -c joined "$RUN/proxy.log" 2>/dev/null) || j=0
echo "   workers joined: $j/$WORKERS"

echo "== earthly $TARGET through the proxy"
start=$SECONDS
( cd "$EB" && EARTHLY_BUILDKIT_HOST="tcp://$LAN:$PROXY_PORT" \
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
