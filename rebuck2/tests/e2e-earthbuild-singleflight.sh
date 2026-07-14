#!/usr/bin/env bash
# THE PRODUCT: two earthbuild instances, one Earthfile, ONE build.
#
# Everything else we have proven is at the buildkitd level, driven by buildctl
# with a Dockerfile. This is the thing actually asked for: two earthbuild runs
# (the shape of two CI jobs, or two developers) building the same target at the
# same time, and the expensive target executing exactly ONCE.
#
# It also exercises what the Dockerfile path never touches: the `earthly`
# exporter, Earthfile targets, SAVE ARTIFACT.
#
# Same decisive trick: the target writes RANDOM bytes. Two machines cannot
# produce the same random bytes, so an identical artifact means one of them
# never ran the command.
#
# Usage: rebuck2/tests/e2e-earthbuild-singleflight.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FORK="${BUILDKIT_FORK:-$HOME/git/EarthBuild/buildkit-lease}"
EB_SRC="${EARTHBUILD_SRC:-$HOME/git/EarthBuild/earthbuild}"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-eb.XXXXXX")"
SESSION="rebuck2-eb-$$"
A1=5071; A2=5072          # rebuck agent per worker
# buildkitds are addressed by container IP, not a published port — see below.

# shellcheck disable=SC2329  # invoked via trap
cleanup() {
    docker rm -f eb-bk-1 eb-bk-2 >/dev/null 2>&1 || true
    kill "${W1:-}" "${W2:-}" "${DRIVER:-}" 2>/dev/null || true
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

if ! command -v docker >/dev/null || ! docker info >/dev/null 2>&1; then
    echo "SKIP: docker not running"; exit 0
fi

echo "=== build the earthbuild CLI from our branch"
( cd "$EB_SRC" && GOFLAGS=-mod=mod GOPROXY=direct GOSUMDB=off go build -o "$SCRATCH/earthbuild" ./cmd/earthly )

echo "=== earthbuild's buildkitd image (it has the \`earthly\` exporter) + our fork"
ARCH="$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/;s/x86_64/amd64/')"
( cd "$FORK" && CGO_ENABLED=0 GOOS=linux GOARCH="$ARCH" go build -o "$SCRATCH/buildkitd" ./cmd/buildkitd )
cat > "$SCRATCH/Dockerfile.img" <<'EOF'
FROM ghcr.io/earthbuild/earthbuild:buildkitd-v0.8.17-fix.5
COPY buildkitd /usr/bin/buildkitd
EOF
docker build -q -f "$SCRATCH/Dockerfile.img" -t eb-singleflight:test "$SCRATCH" >/dev/null

echo "=== build rebuck"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== driver + a worker (with its agent) per instance"
"$BIN" driver --grpc-port 9097 --session "$SESSION" \
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
curl -sf -o /dev/null "http://127.0.0.1:$A2/v2/" || { echo "FAIL: agents never came up"; tail -8 "$SCRATCH/w2.log"; exit 1; }
echo "agents up; workers on the mesh"

start_bk() { # start_bk <name> <agent-port> <host-port>
    docker run -d --name "$1" --privileged \
        -e BUILDKIT_TCP_TRANSPORT_ENABLED=true \
        -e BUILDKIT_SINGLEFLIGHT_URL="http://host.docker.internal:$2" \
        -e BUILDKIT_SINGLEFLIGHT_REGISTRY=cache \
        -e CACHE_SIZE_MB=2000 \
        -p "127.0.0.1:$3:8372" \
        eb-singleflight:test >/dev/null
}

# How earthbuild should ADDRESS a buildkitd.
#
# It treats 127.0.0.1/localhost as a daemon it MANAGES (containerutil.IsLocal)
# and would go start its own. We need a name that is NOT in that list but still
# reaches ours:
#   linux  — container IPs are routable from the host, so use them.
#   macOS  — the docker bridge is inside a VM and unreachable, so publish to
#            loopback and address it as `docker.for.mac.localhost`, which Docker
#            Desktop resolves to 127.0.0.1 and IsLocal does not recognise.
#            Nothing is exposed beyond loopback.
bk_addr() { # bk_addr <name> <host-port>
    if [ "$(uname -s)" = "Linux" ]; then
        echo "tcp://$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1"):8372"
    else
        echo "tcp://docker.for.mac.localhost:$2"
    fi
}
echo "=== two earthbuild buildkitds, each wired to the agent on its own box"
start_bk eb-bk-1 $A1 8391
start_bk eb-bk-2 $A2 8392
# earthbuild's buildkitd listens on TCP ONLY — there is no unix socket, so the
# default buildctl address does not exist in this image.
ready() { docker exec "$1" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1; }
for _ in $(seq 1 60); do
    ready eb-bk-1 && ready eb-bk-2 && break
    sleep 2
done
ready eb-bk-2 || { echo "FAIL: buildkitds not ready"; docker logs eb-bk-1 2>&1 | tail -20; exit 1; }
BK_ADDR_1="$(bk_addr eb-bk-1 8391)"; BK_ADDR_2="$(bk_addr eb-bk-2 8392)"
echo "both buildkitds up ($BK_ADDR_1, $BK_ADDR_2)"

# TLS off — these daemons speak plain TCP.
export EARTHLY_CONFIG="$SCRATCH/earthbuild.yml"
cat > "$EARTHLY_CONFIG" <<'EOF'
global:
  tls_enabled: false
EOF

# A real Earthfile. The target is expensive AND nondeterministic — the sleep lets
# the second instance collide with the first, and the random marker is what tells
# us afterwards which of them actually ran the command.
mkdir -p "$SCRATCH/proj"
#
# NB: deliberately NOT `SAVE ARTIFACT ... AS LOCAL`. That panics on this
# earthbuild branch — a nil fsutil.ContentHasher, pre-existing and unrelated to
# single-flight (it reproduces with the feature OFF; see the note in the repo).
# So a cheap downstream target CATs the marker instead, and we read it from the
# build log. That also exercises COPY +target/artifact, which is the Earthfile
# mechanism a fleet actually leans on.
cat > "$SCRATCH/proj/Earthfile" <<'EOF'
VERSION 0.8
FROM alpine:3.20

expensive:
    RUN head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > /marker.txt && sleep 12
    SAVE ARTIFACT /marker.txt marker

show:
    COPY +expensive/marker /m.txt
    RUN echo "MARKER=$(cat /m.txt)"
EOF

run_eb() { # run_eb <outdir> <buildkit-addr>
    mkdir -p "$1"
    cp "$SCRATCH/proj/Earthfile" "$1/"
    ( cd "$1" && EARTHLY_BUILDKIT_HOST="$2" \
        "$SCRATCH/earthbuild" --no-cache +show ) > "$1/eb.log" 2>&1
}
marker_of() { grep -oE 'MARKER=[0-9a-f]{32}' "$1/eb.log" | head -1 | cut -d= -f2; }

echo
echo "=== both instances build the SAME target AT THE SAME TIME"
run_eb "$SCRATCH/out1" "$BK_ADDR_1" & E1=$!
sleep 2
run_eb "$SCRATCH/out2" "$BK_ADDR_2" & E2=$!
wait $E1 || { echo "FAIL: earthbuild 1"; tail -25 "$SCRATCH/out1/eb.log"; exit 1; }
wait $E2 || { echo "FAIL: earthbuild 2"; tail -25 "$SCRATCH/out2/eb.log"; exit 1; }

M1="$(marker_of "$SCRATCH/out1")"
M2="$(marker_of "$SCRATCH/out2")"
echo "  instance 1 artifact: ${M1:-MISSING}"
echo "  instance 2 artifact: ${M2:-MISSING}"

[ -n "$M1" ] && [ -n "$M2" ] || {
    echo "FAIL: an artifact never arrived"; tail -20 "$SCRATCH/out2/eb.log"; exit 1; }

if [ "$M1" = "$M2" ]; then
    echo
    echo "  driver store: $(du -sm "$SCRATCH/driver-store" | cut -f1) MiB (the layer must not be here)"
    echo
    echo "PASS: two earthbuild instances, one Earthfile, ONE build."
    echo "      Identical artifacts from a RANDOM command: one instance built it,"
    echo "      the other adopted its layer over the mesh."
    exit 0
fi

echo
echo "FAIL: artifacts differ — both instances built +expensive independently."
echo "      single-flight did not engage for earthbuild."
grep -iE 'singleflight|lease|rebuck' "$SCRATCH/out2/eb.log" | head -5 || true
docker logs eb-bk-2 2>&1 | grep -iE 'single-flight|rebuck' | head -5 || true
exit 1
