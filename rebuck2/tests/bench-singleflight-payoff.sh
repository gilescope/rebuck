#!/usr/bin/env bash
# Does single-flight PAY? (docs/buildkit-optimizations.md #10)
#
# #10 frames the payoff as a ratio: expensive-to-build + small-output = win,
# cheap-to-build + large-output = lose. That framing has a hole: it assumes the
# follower's alternative is to build. It is, but IN PARALLEL, and that changes
# the sign.
#
# For a follower that arrives WHILE the leader is still building:
#
#   SF on   follower waits out the leader's remaining build, THEN downloads
#           => t ~= (build_secs - stagger) + T_xfer
#   SF off  follower just builds it itself, concurrently
#           => t ~= build_secs
#
# So single-flight costs the follower ~T_xfer of LATENCY and saves the fleet one
# whole build of CPU. The payoff is governed by ARRIVAL STAGGER -- a variable #10
# does not mention -- and not by the cost/size ratio alone. This measures it:
# sweep the stagger from "together" to "leader already done" and print the sign.
#
# WHAT THIS BENCH CANNOT SEE, and why its numbers must not decide anything:
#
# An earlier version of this header concluded "so it is a THROUGHPUT
# optimisation, not a latency one", and #10 went on to propose gating the lease
# on fleet contention -- skip it when idle, since parallel building is free. The
# arithmetic is right and the conclusion is wrong. Single-flight is a CONSISTENCY
# mechanism (docs/dist-buildkit-principles.md, 1-3): without it, machine A takes
# its apt state from 10:00 and B from 10:05 and the artifact is stitched from
# both -- a build no single machine would produce. Gating on idleness buys a
# little latency by making the grid non-deterministic.
#
# So read this as: what does consistency COST, and where does it also happen to
# pay? Never as: should the lease run? It runs unconditionally.
#
# Prints a table. Asserts nothing about the sign: this is a MEASUREMENT, and the
# point is to find out, not to confirm. (Three of #10's claims were overturned by
# looking, including one this bench produced.)
#
# Usage: rebuck2/tests/bench-singleflight-payoff.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FORK="${BUILDKIT_FORK:-$HOME/git/EarthBuild/buildkit-lease}"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-bench.XXXXXX")"
SESSION="rebuck2-bench-$$"
A1=5081   # worker 1's agent
A2=5082   # worker 2's agent
DP=5080   # the driver's registry: it OWNS the lease table, so it is the only
          # endpoint that can report led/merged/abandoned. An agent forwards, and
          # would report nothing.

BUILD_SECS="${BUILD_SECS:-20}"
LAYER_MB="${LAYER_MB:-50}"
STAGGERS="${STAGGERS:-0 10 20}"   # 0 = together; ~BUILD_SECS = leader already done

# shellcheck disable=SC2329  # invoked via trap
cleanup() {
    docker rm -f bench-bk-1 bench-bk-2 >/dev/null 2>&1 || true
    kill "${W1:-}" "${W2:-}" "${DRIVER:-}" 2>/dev/null || true
    # Bin the heavy artefacts, keep the logs -- the CAS stores hold every layer a
    # leader pushed, so a few runs of this is gigabytes of scratch nobody reads.
    rm -rf "$SCRATCH/driver-store" "$SCRATCH/w1" "$SCRATCH/w2" \
           "$SCRATCH/buildkitd" 2>/dev/null || true
    echo "logs kept in $SCRATCH ($(du -sm "$SCRATCH" 2>/dev/null | cut -f1) MB)"
}
trap cleanup EXIT

if ! command -v docker >/dev/null || ! docker info >/dev/null 2>&1; then
    echo "SKIP: docker not running"; exit 0
fi

now() { python3 -c 'import time; print(f"{time.time():.3f}")'; }
since() { python3 -c "import sys; print(f'{float(sys.argv[2])-float(sys.argv[1]):.1f}')" "$1" "$2"; }

echo "=== build the forked buildkitd + image"
ARCH="$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/;s/x86_64/amd64/')"
( cd "$FORK" && CGO_ENABLED=0 GOOS=linux GOARCH="$ARCH" go build -o "$SCRATCH/buildkitd" ./cmd/buildkitd )
printf 'FROM moby/buildkit:v0.18.2\nCOPY buildkitd /usr/bin/buildkitd\n' > "$SCRATCH/Dockerfile.img"
docker build -q -f "$SCRATCH/Dockerfile.img" -t bk-singleflight:test "$SCRATCH" >/dev/null

echo "=== build rebuck"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== driver + two workers, each with its own agent (the P2P topology)"
"$BIN" driver --grpc-port 9096 --session "$SESSION" --registry-port $DP \
    --store "$SCRATCH/driver-store" > "$SCRATCH/driver.log" 2>&1 &
DRIVER=$!
sleep 2
"$BIN" worker --session "$SESSION" --slots 1 --store "$SCRATCH/w1" \
    --registry-port $A1 --registry-bind 0.0.0.0 > "$SCRATCH/w1.log" 2>&1 &
W1=$!
"$BIN" worker --session "$SESSION" --slots 1 --store "$SCRATCH/w2" \
    --registry-port $A2 --registry-bind 0.0.0.0 > "$SCRATCH/w2.log" 2>&1 &
W2=$!
for _ in $(seq 1 40); do
    curl -sf -o /dev/null "http://127.0.0.1:$A1/v2/" 2>/dev/null && \
    curl -sf -o /dev/null "http://127.0.0.1:$A2/v2/" 2>/dev/null && break
    sleep 1
done
curl -sf -o /dev/null "http://127.0.0.1:$A2/v2/" || { echo "FAIL: agents never came up"; exit 1; }
echo "agents up"

mkdir -p "$SCRATCH/ctx"
# SALT makes every measurement a genuine miss: no cross-run cache hits, so an
# "off" run really does build. The PAIR shares a salt so they collide.
cat > "$SCRATCH/ctx/Dockerfile" <<'EOF'
FROM alpine:3.20
ARG SALT
ARG BUILD_SECS
ARG LAYER_MB
RUN echo "$SALT" > /salt.txt \
 && head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > /marker.txt \
 && head -c ${LAYER_MB}m /dev/urandom > /big.bin \
 && sleep ${BUILD_SECS}
EOF

start_daemons() { # start_daemons <on|off>
    docker rm -f bench-bk-1 bench-bk-2 >/dev/null 2>&1 || true
    local u1="http://host.docker.internal:$A1" u2="http://host.docker.internal:$A2"
    [ "$1" = "off" ] && { u1=""; u2=""; }
    docker run -d --name bench-bk-1 --privileged -e BUILDKIT_SINGLEFLIGHT_URL="$u1" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache -v "$SCRATCH/ctx:/ctx:ro" \
        bk-singleflight:test --addr unix:///run/buildkit/buildkitd.sock >/dev/null
    docker run -d --name bench-bk-2 --privileged -e BUILDKIT_SINGLEFLIGHT_URL="$u2" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache -v "$SCRATCH/ctx:/ctx:ro" \
        bk-singleflight:test --addr unix:///run/buildkit/buildkitd.sock >/dev/null
    sleep 5
}

build() { # build <container> <salt> <outfile>
    docker exec "$1" buildctl build --frontend dockerfile.v0 \
        --local context=/ctx --local dockerfile=/ctx \
        --opt build-arg:SALT="$2" \
        --opt build-arg:BUILD_SECS="$BUILD_SECS" \
        --opt build-arg:LAYER_MB="$LAYER_MB" \
        --output type=local,dest=/out > "$3" 2>&1
}

# One measurement: leader starts, follower joins <stagger> seconds later. We time
# the FOLLOWER, because the follower is the only one whose fate single-flight
# changes -- the leader builds either way.
measure() { # measure <mode> <stagger> -> "follower_secs leader_secs"
    local mode="$1" stagger="$2"
    local salt t0 t1 t2 t3
    salt="s-${mode}-${stagger}-$(date +%s)-$RANDOM"
    t0="$(now)"
    build bench-bk-1 "$salt" "$SCRATCH/leader-${mode}-${stagger}.log" & local LP=$!
    sleep "$stagger"
    t1="$(now)"
    build bench-bk-2 "$salt" "$SCRATCH/follower-${mode}-${stagger}.log" & local FP=$!
    wait $FP; local frc=$?
    t2="$(now)"
    wait $LP; local lrc=$?
    t3="$(now)"
    [ $frc -eq 0 ] || { echo "FAIL: follower build failed (${mode}/${stagger})"; tail -15 "$SCRATCH/follower-${mode}-${stagger}.log"; exit 1; }
    [ $lrc -eq 0 ] || { echo "FAIL: leader build failed (${mode}/${stagger})"; tail -15 "$SCRATCH/leader-${mode}-${stagger}.log"; exit 1; }
    echo "$(since "$t1" "$t2") $(since "$t0" "$t3")"
}

echo
echo "=== sweep: build=${BUILD_SECS}s layer=${LAYER_MB}MB, staggers: $STAGGERS"
echo "    (follower wall-clock; the leader builds either way)"
declare -A ON OFF

echo
echo "--- single-flight ON"
start_daemons on
for s in $STAGGERS; do
    read -r f l <<<"$(measure on "$s")"
    ON[$s]="$f"; echo "    stagger=${s}s -> follower ${f}s (leader ${l}s)"
done

echo
echo "--- single-flight OFF (control: each daemon builds its own)"
start_daemons off
for s in $STAGGERS; do
    read -r f l <<<"$(measure off "$s")"
    OFF[$s]="$f"; echo "    stagger=${s}s -> follower ${f}s (leader ${l}s)"
done

echo
echo "=== RESULT: follower wall-clock, single-flight ON vs OFF"
printf '  %-10s %10s %10s %10s   %s\n' stagger ON OFF delta verdict
for s in $STAGGERS; do
    d="$(python3 -c "print(f'{float('${ON[$s]}')-float('${OFF[$s]}'):+.1f}')")"
    v="$(python3 -c "print('SF WINS' if float('${ON[$s]}') < float('${OFF[$s]}') - 1 else ('SF LOSES' if float('${ON[$s]}') > float('${OFF[$s]}') + 1 else 'wash'))")"
    printf '  %-10s %9ss %9ss %9ss   %s\n' "${s}s" "${ON[$s]}" "${OFF[$s]}" "$d" "$v"
done

echo
echo "=== what the fleet spent (leases: led = built, merged = adopted)"
curl -s "http://127.0.0.1:$DP/_rebuck/stats" 2>/dev/null | python3 -c '
import json,sys
try: s = json.load(sys.stdin)
except Exception: print("  (driver stats unavailable)"); sys.exit()
print("  driver  led={} merged={} abandoned={}".format(
    s.get("leases_led"), s.get("leases_merged"), s.get("leases_abandoned")))
' || true

echo
echo "Read the delta column, not the vibes: a NEGATIVE delta means single-flight"
echo "made the follower faster. If it is positive at stagger=0 and negative as"
echo "the stagger approaches the build time, then the payoff is governed by WHEN"
echo "the follower arrives -- and #10's cost/size framing is incomplete."
