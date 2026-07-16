#!/usr/bin/env bash
# A REAL load: earthbuild's own examples, through dist-buildkit.
#
# Everything we have proven so far ran a toy Earthfile whose "build" was a sleep
# and some urandom. That exercises the lease and the layer transfer, and nothing
# else. It says nothing about a real dependency graph: many vertices, base images
# with real toolchains, COPY between targets, SAVE ARTIFACT, cache reuse.
#
# earthbuild's `examples` targets are exactly that load, and they are already
# maintained and known-good. So: point two earthbuild instances at two
# buildkitds wired to our mesh, build the SAME example on both at once, and see
# what the fleet does with a graph it did not have handed to it.
#
# What to look at:
#   led    = vertices this fleet actually built
#   merged = vertices a second instance ADOPTED instead of rebuilding
# On a real graph most vertices are shared (same base image, same deps layer), so
# merged should be a large fraction. That is the number the synthetic rig could
# never produce, because it only ever had one interesting vertex.
#
# Usage: rebuck2/tests/load-earthbuild-examples.sh [target ...]
#   e.g. rebuck2/tests/load-earthbuild-examples.sh ./examples/go+build
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FORK="${BUILDKIT_FORK:-$HOME/git/EarthBuild/buildkit-lease}"
EB_SRC="${EARTHBUILD_SRC:-$HOME/git/EarthBuild/earthbuild}"
# earthbuild's OWN buildkitd image: it carries the `earthly` exporter, which
# stock moby/buildkit does not have and which every Earthfile target needs.
EB_BK_IMAGE="${EB_BK_IMAGE:-earthbuild/buildkitd:v0.8.17}"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-load.XXXXXX")"
SESSION="rebuck2-load-$$"
A1=5091; A2=5092
DP=5090            # the driver's registry port: it OWNS the lease table, so it
                   # is the only one that can report led/merged/abandoned.
TARGETS=("$@")
[ ${#TARGETS[@]} -eq 0 ] && TARGETS=("./examples/go+build")

# shellcheck disable=SC2329  # invoked via trap
cleanup() {
    # KEEP=1 leaves the daemons up so their logs can be read after the fact --
    # the buildkitd log is the only place a per-vertex lease key appears.
    if [ -n "${KEEP:-}" ]; then
        for c in load-bk-1 load-bk-2; do
            docker logs "$c" > "$SCRATCH/$c.dockerlog" 2>&1 || true
        done
        echo "KEEP=1: daemons left up; their logs saved to $SCRATCH/*.dockerlog"
    else
        docker rm -f load-bk-1 load-bk-2 >/dev/null 2>&1 || true
    fi
    kill "${W1:-}" "${W2:-}" "${DRIVER:-}" 2>/dev/null || true
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

if ! command -v docker >/dev/null || ! docker info >/dev/null 2>&1; then
    echo "SKIP: docker not running"; exit 0
fi

echo "=== earthbuild CLI"
( cd "$EB_SRC" && GOFLAGS=-mod=mod GOPROXY=direct GOSUMDB=off \
    go build -o "$SCRATCH/earthbuild" ./cmd/earthly )

echo "=== our buildkitd, baked into earthbuild's buildkitd image"
ARCH="$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/;s/x86_64/amd64/')"
( cd "$FORK" && CGO_ENABLED=0 GOOS=linux GOARCH="$ARCH" go build -o "$SCRATCH/buildkitd" ./cmd/buildkitd )
cat > "$SCRATCH/Dockerfile.img" <<EOF
FROM $EB_BK_IMAGE
COPY buildkitd /usr/bin/buildkitd
EOF
docker build -q -f "$SCRATCH/Dockerfile.img" -t eb-load:test "$SCRATCH" >/dev/null

echo "=== rebuck"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== driver + a worker (with its agent) per instance"
"$BIN" driver --grpc-port 9095 --session "$SESSION" --registry-port $DP \
    --store "$SCRATCH/driver-store" > "$SCRATCH/driver.log" 2>&1 &
DRIVER=$!
sleep 2
"$BIN" worker --session "$SESSION" --slots 4 --store "$SCRATCH/w1" \
    --registry-port $A1 --registry-bind 0.0.0.0 > "$SCRATCH/w1.log" 2>&1 &
W1=$!
"$BIN" worker --session "$SESSION" --slots 4 --store "$SCRATCH/w2" \
    --registry-port $A2 --registry-bind 0.0.0.0 > "$SCRATCH/w2.log" 2>&1 &
W2=$!
for _ in $(seq 1 40); do
    curl -sf -o /dev/null "http://127.0.0.1:$A1/v2/" 2>/dev/null && \
    curl -sf -o /dev/null "http://127.0.0.1:$A2/v2/" 2>/dev/null && break
    sleep 1
done
curl -sf -o /dev/null "http://127.0.0.1:$A2/v2/" || { echo "FAIL: agents never came up"; tail -8 "$SCRATCH/w2.log"; exit 1; }
echo "agents up"

start_bk() { # start_bk <name> <agent-port> <host-port>
    # ALWAYS remove first. A leftover container (KEEP=1 from a previous run)
    # makes `docker run --name` fail, and the readiness probe then passes against
    # the STALE daemon -- so the run silently tests the old binary. Cost me two
    # runs and an "impossible" log before I spotted it.
    docker rm -f "$1" >/dev/null 2>&1 || true
    docker run -d --name "$1" --privileged \
        -e BUILDKIT_TCP_TRANSPORT_ENABLED=true \
        -e BUILDKIT_SINGLEFLIGHT_URL="http://host.docker.internal:$2" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache \
        -e CACHE_SIZE_MB=8000 \
        -e BUILDKIT_MAX_PARALLELISM=8 \
        -p "127.0.0.1:$3:8372" \
        eb-load:test >/dev/null
}
# 127.0.0.1 would make earthbuild think it MANAGES the daemon and start its own
# (containerutil.IsLocal). docker.for.mac.localhost resolves to loopback but is
# not in that list. On linux the container IP is routable directly.
bk_addr() { # bk_addr <name> <host-port>
    if [ "$(uname -s)" = "Linux" ]; then
        echo "tcp://$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1"):8372"
    else
        echo "tcp://docker.for.mac.localhost:$2"
    fi
}

echo "=== two buildkitds, each wired to the agent on its own box"
start_bk load-bk-1 $A1 8395
start_bk load-bk-2 $A2 8396
ready() { docker exec "$1" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1; }
for _ in $(seq 1 60); do ready load-bk-1 && ready load-bk-2 && break; sleep 2; done
ready load-bk-2 || { echo "FAIL: buildkitds not ready"; docker logs load-bk-1 2>&1 | tail -20; exit 1; }
BK1="$(bk_addr load-bk-1 8395)"; BK2="$(bk_addr load-bk-2 8396)"
echo "both up ($BK1, $BK2)"

export EARTHLY_CONFIG="$SCRATCH/earthbuild.yml"
cat > "$EARTHLY_CONFIG" <<'EOF'
global:
  tls_enabled: false
EOF

run_eb() { # run_eb <label> <bk-addr> <target>
    ( cd "$EB_SRC" && EARTHLY_BUILDKIT_HOST="$2" \
        "$SCRATCH/earthbuild" --no-output "$3" ) > "$SCRATCH/$1.log" 2>&1
}

stats() { # stats <port> — bandwidth; plus leases iff this endpoint owns the table
    curl -s "http://127.0.0.1:$1/_rebuck/stats" 2>/dev/null | python3 -c '
import json,sys
try: s = json.load(sys.stdin)
except Exception: print("(unavailable)"); sys.exit()
bw = "uploads={} ({:.1f} MiB)  serves={} ({:.1f} MiB)".format(
    s.get("uploads",0), s.get("upload_bytes",0)/1048576,
    s.get("serves",0), s.get("serve_bytes",0)/1048576)
if "leases_merged" in s:
    print("led={} MERGED={} abandoned={}  {}".format(
        s["leases_led"], s["leases_merged"], s["leases_abandoned"], bw))
else:
    print(bw)
' || echo "(unavailable)"
}

for T in "${TARGETS[@]}"; do
    echo
    echo "############ $T"
    echo "=== instance 1 alone (cold): does a real graph even build on our fork?"
    SECONDS=0
    if run_eb "solo-1" "$BK1" "$T"; then
        echo "PASS: $T built on dist-buildkit in ${SECONDS}s"
    else
        echo "FAIL: $T did not build. tail:"
        tail -30 "$SCRATCH/solo-1.log"
        exit 1
    fi

    echo
    # The solo run above left instance 1 with a WARM cache. Racing against a
    # warm daemon proves nothing: it never execs, so it never claims, so there is
    # nothing for instance 2 to merge with -- the collision we are trying to
    # measure cannot happen. (Measured: led=4 merged=0, four solo claims and not
    # one race.) Restart both daemons so the pair starts genuinely cold.
    echo "=== restarting both buildkitds: a warm daemon cannot collide"
    docker rm -f load-bk-1 load-bk-2 >/dev/null 2>&1 || true
    start_bk load-bk-1 $A1 8395
    start_bk load-bk-2 $A2 8396
    for _ in $(seq 1 60); do ready load-bk-1 && ready load-bk-2 && break; sleep 2; done
    ready load-bk-2 || { echo "FAIL: buildkitds did not come back"; exit 1; }

    echo "=== now BOTH instances build $T at once, both COLD (the real load)"
    BEFORE="$(stats $DP)"
    SECONDS=0
    run_eb "pair-1" "$BK1" "$T" & P1=$!
    run_eb "pair-2" "$BK2" "$T" & P2=$!
    wait $P1 || { echo "FAIL: instance 1"; tail -25 "$SCRATCH/pair-1.log"; exit 1; }
    wait $P2 || { echo "FAIL: instance 2"; tail -25 "$SCRATCH/pair-2.log"; exit 1; }
    echo "both finished in ${SECONDS}s"
    echo "  driver before: $BEFORE"
    echo "  driver after:  $(stats $DP)"
    echo "  agent1: $(stats $A1)"
    echo "  agent2: $(stats $A2)"
    echo "  (MERGED must rise during the pair run, or single-flight did nothing)"
done

echo
echo "=== driver store (the layers must not be travelling through it)"
du -sm "$SCRATCH/driver-store" 2>/dev/null | cut -f1 | sed 's/$/ MiB/'
echo
echo "DONE. 'merged' is the headline: vertices a second instance adopted"
echo "rather than rebuilt, on a graph nobody hand-crafted for us."
