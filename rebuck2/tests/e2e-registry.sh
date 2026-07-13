#!/usr/bin/env bash
# rebuck2 registry e2e: the OCI facade serves a blob it does NOT have.
#
# The blob is seeded into the WORKER's CAS only. The driver's store never sees
# it until an HTTP GET arrives on the OCI port, at which point the only way it
# can answer is by fetching from the worker over the iroh mesh. A 200 with the
# right bytes therefore proves the thing P1 actually claims: a layer built on
# one machine is served to another machine's buildkitd, P2P, with neither end
# knowing the mesh exists.
#
# Usage: rebuck2/tests/e2e-registry.sh   (from anywhere; cargo on PATH)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
REG_PORT="${REBUCK2_REG_PORT:-9966}"
GRPC_PORT="${REBUCK2_PORT:-9967}"
SESSION="rebuck2-reg-e2e-$$"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-reg.XXXXXX")"
DRIVER_STORE="$SCRATCH/driver-store"
WORKER_STORE="$SCRATCH/worker-store"

cleanup() {
    kill "${DRIVER_PID:-}" "${WORKER_PID:-}" 2>/dev/null || true
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

echo "=== build rebuck2"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== seed a 'layer' into the WORKER's CAS only"
mkdir -p "$DRIVER_STORE" "$WORKER_STORE/cas" "$WORKER_STORE/ac" "$WORKER_STORE/tmp" "$WORKER_STORE/tags"
LAYER="$SCRATCH/layer.bin"
# Big enough to be a real transfer, not a single frame.
head -c 5000000 /dev/urandom > "$LAYER"
HASH="$(sha256 "$LAYER")"
mkdir -p "$WORKER_STORE/cas/${HASH:0:2}"
cp "$LAYER" "$WORKER_STORE/cas/${HASH:0:2}/$HASH"
echo "layer sha256:$HASH ($(wc -c < "$LAYER") bytes) - worker only"

echo "=== start worker"
"$BIN" worker --session "$SESSION" --store "$WORKER_STORE" --slots 1 \
    > "$SCRATCH/worker.log" 2>&1 &
WORKER_PID=$!

echo "=== start driver (OCI registry on :$REG_PORT), waiting for the worker"
"$BIN" driver --grpc-port "$GRPC_PORT" --registry-port "$REG_PORT" \
    --session "$SESSION" --store "$DRIVER_STORE" --min-workers 1 \
    > "$SCRATCH/driver.log" 2>&1 &
DRIVER_PID=$!

echo "=== wait for the mesh to form"
for _ in $(seq 1 60); do
    if grep -q "worker" "$SCRATCH/driver.log" 2>/dev/null && \
       curl -sf -o /dev/null "http://127.0.0.1:$REG_PORT/v2/" 2>/dev/null; then
        break
    fi
    sleep 1
done
curl -sf -o /dev/null "http://127.0.0.1:$REG_PORT/v2/" || { echo "FAIL: registry never came up"; exit 1; }

# The driver must not be holding it yet - otherwise we would be testing nothing.
if [ -e "$DRIVER_STORE/cas/${HASH:0:2}/$HASH" ]; then
    echo "FAIL: driver already has the blob; the test proves nothing"; exit 1
fi
echo "=== confirmed: driver's CAS does NOT hold the blob"

echo "=== GET the blob from the driver's OCI registry (must cross the mesh)"
GOT="$SCRATCH/got.bin"
CODE=$(curl -s -o "$GOT" -w '%{http_code}' \
    "http://127.0.0.1:$REG_PORT/v2/cache/blobs/sha256:$HASH")
[ "$CODE" = "200" ] || { echo "FAIL: GET returned $CODE"; tail -30 "$SCRATCH/driver.log"; exit 1; }

GOT_HASH="$(sha256 "$GOT")"
[ "$GOT_HASH" = "$HASH" ] || { echo "FAIL: served sha256:$GOT_HASH, wanted sha256:$HASH"; exit 1; }
echo "=== served $(wc -c < "$GOT") bytes, digest matches - fetched over iroh"

echo "=== and it was cached back into the driver's CAS"
[ -e "$DRIVER_STORE/cas/${HASH:0:2}/$HASH" ] || { echo "FAIL: not cached back"; exit 1; }

echo "=== HEAD now reports the right length"
LEN=$(curl -sI "http://127.0.0.1:$REG_PORT/v2/cache/blobs/sha256:$HASH" \
    | tr -d '\r' | awk 'tolower($1)=="content-length:"{print $2}')
[ "$LEN" = "$(wc -c < "$LAYER" | tr -d ' ')" ] || { echo "FAIL: Content-Length $LEN"; exit 1; }

echo "=== a blob nobody holds must 404, not hang or 500"
ABSENT=$(printf 'b%.0s' $(seq 1 64))
CODE=$(curl -s -o /dev/null -w '%{http_code}' \
    "http://127.0.0.1:$REG_PORT/v2/cache/blobs/sha256:$ABSENT")
[ "$CODE" = "404" ] || { echo "FAIL: absent blob returned $CODE, wanted 404"; exit 1; }

echo
echo "PASS: OCI registry served a worker-held layer over the mesh"
