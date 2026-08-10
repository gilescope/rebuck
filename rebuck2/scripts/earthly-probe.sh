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
# Overridable, because this script gets run from a FROZEN COPY: editing it
# while it runs corrupts the running shell (bash reads by byte offset), so a
# long probe is launched from a copy elsewhere - and then $0 no longer sits
# next to the binary.
BIN=${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/rebuck2}
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
BK_PORT=${BK_PORT:-28371}
OWN_BK=${OWN_BK:-rebuck2-earthly-bk}

[ -d "$EB" ] || { echo "no earthbuild checkout at $EB (set EB=)"; exit 1; }
[ -n "$LAN" ] || { echo "no non-loopback address found (set LAN=)"; exit 1; }
[ -x "$BIN" ] || { echo "build rebuck2 --release first"; exit 1; }

rm -rf "$RUN"; mkdir -p "$RUN"
pids=()
bk_names=()
cleanup() {
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
  docker rm -f "$OWN_BK" >/dev/null 2>&1 || true
  for n in "${bk_names[@]:-}"; do
    [ -n "$n" ] && docker rm -f "$n" >/dev/null 2>&1 || true
  done
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

echo "== a buildkitd we can actually proxy"
# Bootstrap to learn the IMAGE, then run our OWN container from it.
#
# "Just use the daemon earthly started" does not work, and the reason is not
# obvious: earthly runs it with BUILDKIT_TCP_TRANSPORT_ENABLED=false. It talks
# to its daemon over a unix socket, and the port it publishes answers HTTP/1.1
# - a gRPC client gets "frame too large, note that the frame header looked
# like an HTTP/1.1 header". Nothing can sit in front of that.
#
# The image still has to be earthly's: the converter emits fork-only ops
# (host binds, earthly_interactive sockets) that stock buildkit rejects. So
# bootstrap picks the image and we start it the way a proxy needs.
earthly bootstrap >/dev/null 2>&1 || true
SEED=$(docker ps -a --filter 'name=buildkitd' --format '{{.Names}}' | head -1)
[ -n "$SEED" ] || { echo "earthly started no buildkitd to copy an image from"; exit 1; }
BK_IMAGE=$(docker inspect -f '{{.Config.Image}}' "$SEED")
echo "   image: $BK_IMAGE (from $SEED)"

docker rm -f "$OWN_BK" >/dev/null 2>&1 || true
# 8372, and this one is KNOWN rather than discovered: the fork's own
# /etc/buildkitd.tcp.template sets `[grpc] address = tcp://0.0.0.0:8372`, and
# we are the ones enabling TCP. The 8371 earthly publishes on its own
# container is its embedded REGISTRY - the only port it exposes, because its
# gRPC never leaves a unix socket. Probing that gave HTTP 301s answering the
# HTTP/2 preface.
#
# Discover what someone else configured; know what you configured yourself.
docker run -d --name "$OWN_BK" --privileged \
  -p "$BK_PORT:8372" \
  -e BUILDKIT_TCP_TRANSPORT_ENABLED=true \
  -e BUILDKIT_TLS_ENABLED=false \
  -e EARTHLY_ADDITIONAL_BUILDKIT_CONFIG="[registry.\"$MIRROR_HOST:$REG_PORT\"]
  http = true
  insecure = true" \
  "$BK_IMAGE" >/dev/null
BK_ADDR="127.0.0.1:$BK_PORT"
for _ in $(seq 1 90); do
  docker exec "$OWN_BK" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$OWN_BK" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1 || {
  echo "our buildkitd never became ready:"; docker logs --tail 20 "$OWN_BK"; exit 1;
}
echo "   ours on $BK_ADDR"

echo "== coordinator"
REBUCK2_MIRROR="$MIRROR_HOST:$REG_PORT" \
  "$BIN" buildkit-proxy --listen "0.0.0.0:$PROXY_PORT" \
    --upstream "http://$BK_ADDR" \
    --registry-bind "0.0.0.0:$REG_PORT" \
    --session "$SESSION" --store "$RUN/coord" >"$RUN/proxy.log" 2>&1 &
pids+=("$!")

# ONE DAEMON PER WORKER, which is the whole point and was not what this did.
#
# Every worker used to drive the coordinator's buildkitd. Placement was real,
# but nothing ever crossed a daemon boundary: a subtree "handed to a peer" was
# built by the same daemon that would have built it anyway, into the same
# content store, and the requester found the result already local. That is why
# `hits_peer` has never once been non-zero in this repo.
#
# Separate daemons make the handover cost what it actually costs - a push to
# the mirror and a pull back - which is the thing being built here.
#
# Set WORKER_BK=shared to get the old behaviour for a quick placement check.
for i in $(seq 1 "$WORKERS"); do
  if [ "${WORKER_BK:-own}" = shared ]; then
    w_bk="$BK_ADDR"
  else
    w_port=$((BK_PORT + i))
    docker rm -f "$OWN_BK-$i" >/dev/null 2>&1 || true
    docker run -d --name "$OWN_BK-$i" --privileged \
      -p "$w_port:8372" \
      -e BUILDKIT_TCP_TRANSPORT_ENABLED=true \
      -e BUILDKIT_TLS_ENABLED=false \
      -e EARTHLY_ADDITIONAL_BUILDKIT_CONFIG="[registry.\"$MIRROR_HOST:$REG_PORT\"]
  http = true
  insecure = true" \
      "$BK_IMAGE" >/dev/null
    w_bk="127.0.0.1:$w_port"
    bk_names+=("$OWN_BK-$i")
  fi
  "$BIN" worker --session "$SESSION" \
    --store "$RUN/worker-$i" \
    --buildkit-addr "http://$w_bk" \
    --registry-addr "$MIRROR_HOST:$REG_PORT" >"$RUN/worker-$i.log" 2>&1 &
  pids+=("$!")
done

# Wait for every worker daemon before the barrier below: a worker whose
# daemon is still starting joins the mesh, advertises the host's platform
# because it cannot ask its daemon, and then declines everything.
for n in "${bk_names[@]:-}"; do
  [ -n "$n" ] || continue
  for _ in $(seq 1 90); do
    docker exec "$n" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1 && break
    sleep 1
  done
  docker exec "$n" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1 || {
    echo "worker daemon $n never became ready:"; docker logs --tail 20 "$n"; exit 1;
  }
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
# EARTHLY_BIN lets a PATCHED earthly be measured against the same fleet -
# the only way to price EarthBuild#784 before it lands.
# EARTHLY_ARGS carries flags the TARGET needs, not flags we prefer.
# `+test-no-qemu` uses WITH DOCKER, which is `security.insecure`, and earthly
# only requests that entitlement when --allow-privileged is passed
# (build_cmd.go: `if b.cli.Flags().AllowPrivileged`). Without it the build
# dies on `failed to load LLB: security.insecure is not allowed`, which reads
# like the proxy stripped something and does not.
# shellcheck disable=SC2086 # deliberate word-splitting: these are flags
( cd "$EB" && EARTHLY_BUILDKIT_HOST="tcp://$LAN:$PROXY_PORT" \
    "${EARTHLY_BIN:-earthly}" ${EARTHLY_ARGS:-} "$TARGET" >"$RUN/earthly.log" 2>&1 ) \
  && ok=yes || ok=no
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
echo "== where each worker's blobs came from"
# The point of separate daemons: a subtree built on worker N and consumed by
# the requester has to MOVE. local=warm cache, peer=the mesh did its job,
# driver=fell back through the coordinator.
#
# Reported per worker and not summed: one worker serving everything looks
# identical to a healthy fleet once you add the columns up.
for i in $(seq 1 "$WORKERS"); do
  line=$(grep -E '^\[cas\] fetches' "$RUN/worker-$i.log" 2>/dev/null | tail -1)
  echo "   worker $i: ${line:-no fetches (nothing crossed a boundary)}"
done
echo
echo "logs in $RUN (earthly.log has the build itself)"
