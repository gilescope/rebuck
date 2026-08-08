#!/usr/bin/env bash
# Stand up a distributed buildkit and measure it.
#
# One registry (the peer-to-peer mirror), N buildkitd daemons, one rebuck2
# proxy in front of daemon 0, and a fan-out of plain-LLB builds pushed
# through the proxy with buildctl. Prints the wire report, which is the
# measurement: how many solves were routed to another daemon, how many were
# peer 0's own share, and the reason for every one that stayed put.
#
# This existed as twenty-five hand-typed shell lines and twenty-five log
# files with names like reg9.log before it existed as a script, and every
# re-run risked measuring a slightly different thing. Reproducibility is
# the point: same inputs, same fleet, comparable numbers.
#
#   rebuck2/scripts/fleet.sh            # 2 daemons, 4 builds
#   DAEMONS=3 BUILDS=6 rebuck2/scripts/fleet.sh
#   NOPROXY=1 rebuck2/scripts/fleet.sh  # baseline: one daemon, client talks direct
#
# Needs docker. buildctl is used from the host if present and borrowed from
# the buildkit image otherwise. Leaves nothing running.
set -euo pipefail

DAEMONS=${DAEMONS:-2}
BUILDS=${BUILDS:-4}
NOPROXY=${NOPROXY:-}
IMAGE=${IMAGE:-moby/buildkit:latest}
RUN=${RUN:-${TMPDIR:-/tmp}/rebuck2-fleet}
BASE_PORT=${BASE_PORT:-18372}
REG_PORT=${REG_PORT:-15000}
PROXY_PORT=${PROXY_PORT:-11234}

crate=$(cd "$(dirname "$0")/.." && pwd)
rm -rf "$RUN"
mkdir -p "$RUN/llb" "$RUN/store"

pids=()
containers=()
# shellcheck disable=SC2329  # invoked by the EXIT trap, not by name
cleanup() {
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
  for c in "${containers[@]:-}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

say() { printf '\n=== %s\n' "$*"; }

# buildctl, from wherever it can be had. The buildkit image ships it, so a
# host without it is not a blocker - but `--network host` on Docker Desktop
# does NOT reach the host's loopback, so borrowed buildctl must dial
# host.docker.internal. Rewriting the address here rather than at every call
# site is the whole reason this is a function.
#
# It also decides how far the fleet has to be published. A host buildctl
# reaches loopback, so ports stay on 127.0.0.1. A borrowed one runs in a
# container, and `host-gateway` resolves to the host's routable address -
# which a service bound to loopback will not answer. So borrowing costs an
# all-interfaces bind on the daemons. Say so rather than defaulting to it:
# these are PRIVILEGED daemons that will run anything they are handed.
if command -v buildctl >/dev/null 2>&1; then
  BIND=127.0.0.1
  bctl() { buildctl "$@"; }
else
  BIND=0.0.0.0
  echo "note: no host buildctl; publishing daemons on $BIND so a borrowed one can reach them"
  bctl() {
    local args=()
    for a in "$@"; do args+=("${a//127.0.0.1/host.docker.internal}"); done
    docker run --rm -i --add-host host.docker.internal:host-gateway \
      --entrypoint buildctl "$IMAGE" "${args[@]}"
  }
fi

say "build"
(cd "$crate" && cargo build --quiet)
bin="$crate/target/debug/rebuck2"

# The LLB. Four independent CPU-bound execs on a common base: the shape
# where a fleet can win and a single daemon cannot fake it. A sleep-based
# workload measures nothing - one daemon serves four sleeps as fast as four
# daemons do, so the fleet looks free when it is not.
say "generate llb"
(cd "$crate" && REBUCK2_LLB_OUT="$RUN/llb" REBUCK2_LLB_N="$BUILDS" \
  cargo test --quiet --bin rebuck2 write_fanout_llb -- --ignored >/dev/null)
printf "%s\n" "$RUN"/llb/*.llb

say "registry on 0.0.0.0:$REG_PORT (daemons reach it as host.docker.internal)"
"$bin" registry --bind "0.0.0.0:$REG_PORT" --store "$RUN/store" >"$RUN/registry.log" 2>&1 &
pids+=($!)

# Daemons trust the mirror over plain HTTP. Without this a peer with no
# session cannot pull what another daemon published, and the failure looks
# like a portability bug rather than a config one.
cat >"$RUN/buildkitd.toml" <<TOML
root = "/var/lib/buildkit"
[grpc]
  address = [ "tcp://0.0.0.0:8372" ]
[worker.oci]
  enabled = true
  max-parallelism = 20
[registry."host.docker.internal:$REG_PORT"]
  http = true
  insecure = true
TOML

peers=()
for i in $(seq 0 $((DAEMONS - 1))); do
  port=$((BASE_PORT + i))
  name="rebuck2-fleet-$i"
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --privileged \
    -p "$BIND:$port:8372" \
    -v "$RUN/buildkitd.toml:/etc/buildkit/buildkitd.toml:ro" \
    --add-host host.docker.internal:host-gateway \
    "$IMAGE" >/dev/null
  containers+=("$name")
  say "daemon $i on 127.0.0.1:$port ($name)"
  # `if`, not `[ ] &&`: a false test is the loop body's last command, and
  # under `set -e` that ends the script on daemon 0.
  if [ "$i" -gt 0 ]; then peers+=(--peer "http://127.0.0.1:$port"); fi
done

# Daemons are not ready when `docker run` returns; ListWorkers is the only
# honest readiness signal.
for i in $(seq 0 $((DAEMONS - 1))); do
  port=$((BASE_PORT + i))
  for _ in $(seq 1 60); do
    bctl --addr "tcp://127.0.0.1:$port" debug workers >/dev/null 2>&1 && break
    sleep 1
  done
done

if [ -n "$NOPROXY" ]; then
  addr="tcp://127.0.0.1:$BASE_PORT"
  say "baseline: client -> daemon 0 direct"
else
  addr="tcp://127.0.0.1:$PROXY_PORT"
  # peers holds --peer and its value, so its length is twice the count.
  say "proxy on $addr -> daemon 0 plus $((${#peers[@]} / 2)) peer(s)"
  REBUCK2_MIRROR="host.docker.internal:$REG_PORT" \
    "$bin" buildkit-proxy --listen "$BIND:$PROXY_PORT" \
    --upstream "http://127.0.0.1:$BASE_PORT" "${peers[@]:-}" >"$RUN/proxy.log" 2>&1 &
  proxy_pid=$!
  pids+=("$proxy_pid")
  for _ in $(seq 1 30); do
    bctl --addr "$addr" debug workers >/dev/null 2>&1 && break
    sleep 1
  done
fi

say "$BUILDS builds through $addr"
start=$(date +%s)
n=0
builds=()
for f in "$RUN"/llb/*.llb; do
  if [ "$n" -ge "$BUILDS" ]; then break; fi
  bctl --addr "$addr" build --no-cache <"$f" >"$RUN/build-$n.log" 2>&1 &
  builds+=($!)
  n=$((n + 1))
done
# Wait on the BUILD pids only. A bare `wait` also waits on the registry and
# the proxy, neither of which ever exits, so the harness hangs forever
# having already finished the measurement.
fail=0
for p in "${builds[@]}"; do
  wait "$p" || fail=1
done
elapsed=$(($(date +%s) - start))

say "result"
echo "daemons : $DAEMONS"
echo "builds  : $BUILDS"
echo "wall    : ${elapsed}s"
echo "failed  : $fail"

if [ -z "$NOPROXY" ]; then
  # The wire report prints on SIGINT, so ask for it before the trap kills
  # everything with SIGTERM.
  kill -INT "$proxy_pid" 2>/dev/null || true
  sleep 2
  grep -E '^\[wire\]|^\[proxy\] adopted' "$RUN/proxy.log" || true
fi

say "per-daemon cache (work leaves a mark where it ran)"
for i in $(seq 0 $((DAEMONS - 1))); do
  port=$((BASE_PORT + i))
  printf 'daemon %s: ' "$i"
  bctl --addr "tcp://127.0.0.1:$port" du 2>/dev/null | tail -1 || echo "?"
done

echo
echo "logs in $RUN"
exit "$fail"
