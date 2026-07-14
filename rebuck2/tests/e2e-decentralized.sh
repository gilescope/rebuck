#!/usr/bin/env bash
# Decentralized CAS e2e (now the DEFAULT): driver + 2 workers. The rustc
# compile lands on one worker, the link needs its rlib — with outputs staying
# local to producers, the consumer must fetch peer-to-peer (or the driver
# read-through must kick in for buck2's own materialization). PASS requires
# the build green AND evidence that outputs were NOT uploaded to the driver.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PORT="${REBUCK2_PORT:-9958}"
SESSION="rebuck2-dcas-$$"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-dcas.XXXXXX")"
DLOG="$SCRATCH/driver.log"
ISOLATION="rebuck2dcas"

cleanup() {
    kill "${DRIVER_PID:-}" "${W1_PID:-}" "${W2_PID:-}" 2>/dev/null || true
    (cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" clean 2>/dev/null) || true
    rm -f "$ROOT/test/.buckconfig.local"
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

wait_for() { # pattern file tries
    for _ in $(seq 1 "${3:-60}"); do
        grep -q -e "$1" "$2" 2>/dev/null && return 0
        sleep 1
    done
    echo "TIMEOUT waiting for: $1"; cat "$2" 2>/dev/null; return 1
}

echo "=== build rebuck2"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== start driver (decentralized) + two workers"
"$BIN" driver --grpc-port "$PORT" --session "$SESSION" \
    --store "$SCRATCH/driver-store" --min-workers 2 --no-local-exec \
    >"$DLOG" 2>&1 &
DRIVER_PID=$!
"$BIN" worker --session "$SESSION" --store "$SCRATCH/w1-store" >"$SCRATCH/w1.log" 2>&1 &
W1_PID=$!
wait_for "worker 1 joined" "$DLOG" 90
"$BIN" worker --session "$SESSION" --store "$SCRATCH/w2-store" >"$SCRATCH/w2.log" 2>&1 &
W2_PID=$!
wait_for "worker 2 joined" "$DLOG" 90
grep -q "decentralized CAS" "$SCRATCH/w1.log" || { echo "FAIL: worker 1 not in decentralized mode"; exit 1; }

cat > "$ROOT/test/.buckconfig.local" <<EOF
[build]
execution_platforms = root//platforms:re-exec

[buck2_re_client]
action_cache_address = grpc://127.0.0.1:$PORT
cas_address = grpc://127.0.0.1:$PORT
engine_address = grpc://127.0.0.1:$PORT
tls = false
EOF

echo "=== buck2 build (compile + link land on workers; outputs stay there)"
(cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" clean >/dev/null 2>&1 || true)
(cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" build //:hello 2>&1 | tee "$SCRATCH/build.log")
grep -q "BUILD SUCCEEDED" "$SCRATCH/build.log" || { echo "FAIL: build failed"; exit 1; }

echo "=== assertions"
# Outputs must NOT have been uploaded: driver store holds only buck2's own
# uploads (sources/actions), so the rustc-produced binary blob must live in a
# worker store and not the driver's.
BIN_BLOBS_DRIVER=$(find "$SCRATCH/driver-store/cas" -type f -size +200k | wc -l | tr -d ' ')
BIN_BLOBS_WORKERS=$(find "$SCRATCH/w1-store/cas" "$SCRATCH/w2-store/cas" -type f -size +200k | wc -l | tr -d ' ')
echo "large blobs: driver=$BIN_BLOBS_DRIVER workers=$BIN_BLOBS_WORKERS"
[ "$BIN_BLOBS_WORKERS" -ge 1 ] || { echo "FAIL: no large output blob on any worker"; exit 1; }
grep -q -- "-> worker " "$DLOG" || { echo "FAIL: nothing dispatched"; exit 1; }
echo "PASS: decentralized build green; outputs live on workers, driver kept the index"
