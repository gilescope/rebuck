#!/usr/bin/env bash
# Single-flight with an agent per worker: the layer must NEVER touch the driver.
#
# The central-registry design measured ~1.0x amplification per follower — every
# byte a leader built went up to the coordinator and back down to each follower,
# so 8 workers would put ~1 GB through one NIC.
#
# Here each worker runs its own rebuck agent, and each buildkitd talks only to
# the agent on its own box:
#
#   leader   pushes a layer -> its OWN agent          (loopback, ~free)
#   follower pulls  a layer -> its own agent -> mesh -> the LEADER's agent
#
# The measurement is deliberately blunt and hard to fake: after the build, look
# at what is ON THE DRIVER'S DISK. If the driver is out of the data path, the
# 150 MB layer is simply not there.
#
# Usage: rebuck2/tests/e2e-buildkit-p2p.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FORK="${BUILDKIT_FORK:-$HOME/git/EarthBuild/buildkit-lease}"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-p2p.XXXXXX")"
SESSION="rebuck2-p2p-$$"
A1=5061   # worker 1's agent
A2=5062   # worker 2's agent
LAYER_MB=150

# shellcheck disable=SC2329  # invoked via trap
cleanup() {
    docker rm -f bk-p2p-1 bk-p2p-2 >/dev/null 2>&1 || true
    kill "${W1:-}" "${W2:-}" "${DRIVER:-}" 2>/dev/null || true
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

if ! command -v docker >/dev/null || ! docker info >/dev/null 2>&1; then
    echo "SKIP: docker not running"; exit 0
fi

echo "=== build the forked buildkitd + image"
ARCH="$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/;s/x86_64/amd64/')"
( cd "$FORK" && CGO_ENABLED=0 GOOS=linux GOARCH="$ARCH" go build -o "$SCRATCH/buildkitd" ./cmd/buildkitd )
printf 'FROM moby/buildkit:v0.18.2\nCOPY buildkitd /usr/bin/buildkitd\n' > "$SCRATCH/Dockerfile.img"
docker build -q -f "$SCRATCH/Dockerfile.img" -t bk-singleflight:test "$SCRATCH" >/dev/null

echo "=== build rebuck"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

# Decentralized is now the DEFAULT: the driver REDIRECTS a fetch to whoever holds
# the blob rather than relaying the bytes through itself. (--centralized-cas
# restores the old read-through behaviour, and would put us back where we
# started.)
echo "=== driver (coordinator only — it must never see a layer)"
"$BIN" driver --grpc-port 9098 --session "$SESSION" \
    --store "$SCRATCH/driver-store" > "$SCRATCH/driver.log" 2>&1 &
DRIVER=$!
sleep 2

echo "=== two workers, each with its own agent"
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
curl -sf -o /dev/null "http://127.0.0.1:$A1/v2/" || { echo "FAIL: agent 1 never came up"; tail -10 "$SCRATCH/w1.log"; exit 1; }
curl -sf -o /dev/null "http://127.0.0.1:$A2/v2/" || { echo "FAIL: agent 2 never came up"; tail -10 "$SCRATCH/w2.log"; exit 1; }
echo "both agents up; workers joined the mesh"

mkdir -p "$SCRATCH/ctx"
cat > "$SCRATCH/ctx/Dockerfile" <<EOF
FROM alpine:3.20
RUN head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \\n' > /marker.txt \\
 && head -c ${LAYER_MB}m /dev/urandom > /big.bin \\
 && sleep 8
EOF

start() { # start <name> <agent-port>
    docker run -d --name "$1" --privileged \
        -e BUILDKIT_SINGLEFLIGHT_URL="http://host.docker.internal:$2" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache \
        -v "$SCRATCH/ctx:/ctx:ro" \
        bk-singleflight:test --addr unix:///run/buildkit/buildkitd.sock >/dev/null
}
build() { # build <name> <out>
    docker exec "$1" buildctl build --frontend dockerfile.v0 \
        --local context=/ctx --local dockerfile=/ctx \
        --output type=local,dest=/out > "$SCRATCH/$1.log" 2>&1
    docker cp "$1:/out/marker.txt" "$2" >/dev/null 2>&1
}

echo "=== each buildkitd talks ONLY to the agent on its own box"
start bk-p2p-1 $A1
start bk-p2p-2 $A2
sleep 4

build bk-p2p-1 "$SCRATCH/m1.txt" & P1=$!
sleep 1
build bk-p2p-2 "$SCRATCH/m2.txt" & P2=$!
wait $P1 || { echo "FAIL: build 1"; tail -20 "$SCRATCH/bk-p2p-1.log"; exit 1; }
wait $P2 || { echo "FAIL: build 2"; tail -20 "$SCRATCH/bk-p2p-2.log"; exit 1; }

M1="$(cat "$SCRATCH/m1.txt" 2>/dev/null || echo MISSING)"
M2="$(cat "$SCRATCH/m2.txt" 2>/dev/null || echo MISSING)"
echo "  marker 1: $M1"
echo "  marker 2: $M2"
[ "$M1" = "$M2" ] && [ "$M1" != "MISSING" ] || {
    echo "FAIL: the step did not single-flight (markers differ)"; exit 1; }
echo "single-flight still works: the step executed ONCE"

echo
echo "=== THE MEASUREMENT: did the layer touch the driver?"
du_mb() { du -sm "$1" 2>/dev/null | cut -f1; }
D=$(du_mb "$SCRATCH/driver-store")
S1=$(du_mb "$SCRATCH/w1")
S2=$(du_mb "$SCRATCH/w2")
echo "  driver store:   ${D} MiB"
echo "  worker 1 store: ${S1} MiB"
echo "  worker 2 store: ${S2} MiB"
echo
echo "  worker 2's fetch sources (local / peer / driver):"
grep -h '\[cas\] fetches' "$SCRATCH/w2.log" | tail -1 || echo "    (none logged)"

# The layer is 150 MiB. If the driver is out of the data path its store cannot
# be holding it. Allow slack for the lease/AC bookkeeping.
if [ "${D:-0}" -lt 50 ]; then
    echo
    echo "PASS: the driver's store is ${D} MiB — the ${LAYER_MB} MiB layer never went through it."
    echo "      Leader pushed to loopback; the follower pulled from the LEADER, peer to peer."
    exit 0
fi

echo
echo "FAIL: the driver's store holds ${D} MiB — the layer IS being relayed through it."
echo "      Expected < 50 MiB. The driver is still on the data path."
exit 1
