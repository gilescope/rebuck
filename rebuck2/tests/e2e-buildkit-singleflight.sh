#!/usr/bin/env bash
# Two buildkitds, one coordinator, ONE build.
#
# The whole point of docs/buildkit-plan.md P2, proven with real daemons: two
# independent buildkitd instances start the same build at the same time, and the
# expensive step runs exactly ONCE.
#
# The test is decisive rather than log-scraped. The RUN writes RANDOM bytes into
# the layer:
#
#   * if single-flight works, one daemon builds and the other ADOPTS its layer,
#     so both builds end with the SAME marker;
#   * if it does not, both daemons build independently and get DIFFERENT markers.
#
# Nondeterministic content cannot be faked by a cache hit we did not ask for, and
# neither daemon is given --import-cache, so the only way the second one can
# avoid building is the lease.
#
# Usage: rebuck2/tests/e2e-buildkit-singleflight.sh   (needs docker + cargo)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FORK="${BUILDKIT_FORK:-$HOME/git/EarthBuild/buildkit-lease}"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-sf.XXXXXX")"
PORT=5055
SESSION="rebuck2-sf-$$"

# shellcheck disable=SC2329  # invoked via trap
cleanup() {
    docker rm -f bk-sf-1 bk-sf-2 >/dev/null 2>&1 || true
    kill "${DRIVER_PID:-}" 2>/dev/null || true
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

command -v docker >/dev/null || { echo "SKIP: docker not available"; exit 0; }
docker info >/dev/null 2>&1 || { echo "SKIP: docker daemon not running"; exit 0; }

echo "=== build the forked buildkitd (linux/arm64) and bake an image"
( cd "$FORK" && CGO_ENABLED=0 GOOS=linux GOARCH="$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/;s/x86_64/amd64/')" \
    go build -o "$SCRATCH/buildkitd" ./cmd/buildkitd )
cat > "$SCRATCH/Dockerfile.img" <<'EOF'
FROM moby/buildkit:v0.18.2
COPY buildkitd /usr/bin/buildkitd
EOF
docker build -q -f "$SCRATCH/Dockerfile.img" -t bk-singleflight:test "$SCRATCH" >/dev/null

echo "=== build rebuck (the coordinator)"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

# The coordinator serves BOTH surfaces the daemons need: the lease
# (/_rebuck/lease/*) and the OCI registry the leader pushes its layers to
# (/v2/*). They are the same store, which is the point.
#
# Bound to 0.0.0.0 ONLY because Docker Desktop runs buildkitd inside a VM that
# cannot reach the host's loopback. It has no auth: it is killed on exit.
echo "=== start the coordinator on :$PORT"
"$BIN" driver --grpc-port 9099 --registry-port "$PORT" --session "$SESSION" \
    --store "$SCRATCH/coord-store" > "$SCRATCH/coord.log" 2>&1 &
DRIVER_PID=$!
for _ in $(seq 1 30); do
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/v2/" 2>/dev/null && break
    sleep 1
done
curl -sf -o /dev/null "http://127.0.0.1:$PORT/v2/" || { echo "FAIL: coordinator never came up"; cat "$SCRATCH/coord.log"; exit 1; }

# The build. The RUN is expensive AND nondeterministic: the sleep gives the
# second daemon time to collide with the first, and the random marker is what
# tells us afterwards whether it built its own layer or adopted one.
mkdir -p "$SCRATCH/ctx"
cat > "$SCRATCH/ctx/Dockerfile" <<'EOF'
FROM alpine:3.20
ARG LAYER_MB
RUN head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > /marker.txt \
 && head -c ${LAYER_MB:-150}m /dev/urandom > /big.bin \
 && sleep 8
EOF

start_daemon() { # start_daemon <name> [off]
    # "off" leaves BUILDKIT_SINGLEFLIGHT_URL unset: the feature is inert and
    # ExecOp behaves exactly as upstream. This is the NEGATIVE CONTROL.
    local url="http://host.docker.internal:$PORT"
    [ "${2:-}" = "off" ] && url=""
    docker run -d --name "$1" --privileged \
        -e BUILDKIT_SINGLEFLIGHT_URL="$url" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache \
        -v "$SCRATCH/ctx:/ctx:ro" \
        bk-singleflight:test \
        --addr unix:///run/buildkit/buildkitd.sock >/dev/null
}

echo "=== start two INDEPENDENT buildkitds (separate caches, no --import-cache)"
start_daemon bk-sf-1
start_daemon bk-sf-2
sleep 4
for c in bk-sf-1 bk-sf-2; do
    docker exec "$c" buildctl debug workers >/dev/null 2>&1 || { echo "FAIL: $c not ready"; docker logs "$c" | tail -20; exit 1; }
done
echo "both daemons up"

build() { # build <container> <outdir>
    docker exec "$1" buildctl build \
        --frontend dockerfile.v0 \
        --local context=/ctx --local dockerfile=/ctx \
        --output type=local,dest=/out \
        > "$SCRATCH/$1.log" 2>&1
    docker cp "$1:/out/marker.txt" "$2" >/dev/null 2>&1
}

echo "=== both daemons build the SAME thing, at the SAME time"
SECONDS=0
build bk-sf-1 "$SCRATCH/marker1.txt" &
P1=$!
sleep 1   # let one of them win the race deterministically
build bk-sf-2 "$SCRATCH/marker2.txt" &
P2=$!
wait $P1 || { echo "FAIL: daemon 1's build failed"; tail -25 "$SCRATCH/bk-sf-1.log"; exit 1; }
wait $P2 || { echo "FAIL: daemon 2's build failed"; tail -25 "$SCRATCH/bk-sf-2.log"; exit 1; }
echo "both builds finished in ${SECONDS}s"

M1="$(cat "$SCRATCH/marker1.txt" 2>/dev/null || echo MISSING-1)"
M2="$(cat "$SCRATCH/marker2.txt" 2>/dev/null || echo MISSING-2)"
echo "  daemon 1 marker: $M1"
echo "  daemon 2 marker: $M2"

echo
# Is the coordinator a bandwidth bottleneck, or is that theory? A leader PUSHES
# its layer here and every follower PULLS it back down, so with N workers the
# coordinator serves ~(N-1)x what it stored. Print the ratio rather than assert
# on it: this is a measurement, not a pass/fail.
echo "=== what actually crossed the coordinator"
curl -s "http://127.0.0.1:$PORT/_rebuck/stats" | python3 -c '
import json,sys
s = json.load(sys.stdin)
up, srv = s["upload_bytes"], s["serve_bytes"]
mib = lambda b: b / (1024*1024)
print(f"  leases:  led={s["leases_led"]} merged={s["leases_merged"]} abandoned={s["leases_abandoned"]}")
print(f"  pushed by leaders:  {s["uploads"]:>3} blobs  {mib(up):8.2f} MiB")
print(f"  pulled by followers:{s["serves"]:>3} blobs  {mib(srv):8.2f} MiB")
if up:
    print(f"  amplification: {srv/up:.2f}x  (each follower re-downloads the leader'"'"'s layer)")
'

if [ "$M1" != "$M2" ] || [ "$M1" = "MISSING-1" ]; then
    echo "FAIL: markers differ => both daemons built it independently."
    echo "      single-flight did not engage. Daemon 1 log tail:"
    tail -20 "$SCRATCH/bk-sf-1.log"
    echo "--- daemon 2:"; tail -20 "$SCRATCH/bk-sf-2.log"
    exit 1
fi
echo "PASS(1/2): identical markers from a RANDOM command => the step executed ONCE."

# ------------------------------------------------------------ negative control
# A test that cannot fail proves nothing. Run the SAME rig with the feature OFF:
# the two daemons must now each build their own layer and get DIFFERENT markers.
# If they still match, something other than single-flight is deduplicating them
# and the result above is worthless.
echo
echo "=== NEGATIVE CONTROL: same rig, single-flight DISABLED"
docker rm -f bk-sf-1 bk-sf-2 >/dev/null 2>&1
start_daemon bk-sf-1 off
start_daemon bk-sf-2 off
sleep 4
build bk-sf-1 "$SCRATCH/ctrl1.txt" &
C1=$!
sleep 1
build bk-sf-2 "$SCRATCH/ctrl2.txt" &
C2=$!
wait $C1; wait $C2
K1="$(cat "$SCRATCH/ctrl1.txt" 2>/dev/null || echo MISSING)"
K2="$(cat "$SCRATCH/ctrl2.txt" 2>/dev/null || echo MISSING)"
echo "  daemon 1 marker: $K1"
echo "  daemon 2 marker: $K2"
if [ "$K1" = "$K2" ]; then
    echo "FAIL: markers MATCH with the feature off — something else is deduplicating."
    echo "      The pass above therefore proves nothing."
    exit 1
fi
echo "PASS(2/2): with the feature off the daemons DO build independently."
echo "           So the test can fail, and the pass above is real."

# ------------------------------------------------- the safety property
# Coalescing builds that must DIFFER is the catastrophic failure — far worse
# than failing to coalesce. `COPY payload.txt` has the SAME op digest whatever
# the file contains: the content only enters the cache key through the dependency
# chain (the slow/content key). If LeaseKey were blind to that, daemon 2 would
# adopt daemon 1's layer and bake in the WRONG SOURCE.
#
# So: same Dockerfile, DIFFERENT payloads, built concurrently. Each daemon must
# end up with its OWN content.
echo
echo "=== SAFETY: same build, DIFFERENT sources — they must NOT coalesce"
docker rm -f bk-sf-1 bk-sf-2 >/dev/null 2>&1

for n in 1 2; do
    mkdir -p "$SCRATCH/ctx$n"
    cat > "$SCRATCH/ctx$n/Dockerfile" <<'EOF'
FROM alpine:3.20
COPY payload.txt /payload.txt
RUN cat /payload.txt > /marker.txt && sleep 10
EOF
done
printf 'ALPHA' > "$SCRATCH/ctx1/payload.txt"
printf 'BETA'  > "$SCRATCH/ctx2/payload.txt"

start_ctx_daemon() { # start_ctx_daemon <name> <ctxdir>
    docker run -d --name "$1" --privileged \
        -e BUILDKIT_SINGLEFLIGHT_URL="http://host.docker.internal:$PORT" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache \
        -v "$2:/ctx:ro" \
        bk-singleflight:test \
        --addr unix:///run/buildkit/buildkitd.sock >/dev/null
}
start_ctx_daemon bk-sf-1 "$SCRATCH/ctx1"
start_ctx_daemon bk-sf-2 "$SCRATCH/ctx2"
sleep 4

build bk-sf-1 "$SCRATCH/safe1.txt" &
S1=$!
sleep 1
build bk-sf-2 "$SCRATCH/safe2.txt" &
S2=$!
wait $S1 || { echo "FAIL: safety build 1 failed"; tail -20 "$SCRATCH/bk-sf-1.log"; exit 1; }
wait $S2 || { echo "FAIL: safety build 2 failed"; tail -20 "$SCRATCH/bk-sf-2.log"; exit 1; }

A="$(cat "$SCRATCH/safe1.txt" 2>/dev/null || echo MISSING)"
B="$(cat "$SCRATCH/safe2.txt" 2>/dev/null || echo MISSING)"
echo "  daemon 1 (source ALPHA) baked: $A"
echo "  daemon 2 (source BETA)  baked: $B"

[ "$A" = "ALPHA" ] || { echo "FAIL: daemon 1 baked '$A', wanted ALPHA"; exit 1; }
[ "$B" = "BETA" ]  || {
    echo "FAIL: daemon 2 baked '$B', wanted BETA."
    echo "      The lease key is CONTENT-BLIND — a follower adopted a layer built"
    echo "      from someone else's sources. This is the wrong-artifact bug."
    exit 1
}
echo "PASS(3/3): different sources => different keys => no coalescing."
echo "           The lease key is content-addressed, as claimed."

echo
echo "PASS: two buildkitds, one coordinator, ONE build — and never the wrong one."
exit 0
