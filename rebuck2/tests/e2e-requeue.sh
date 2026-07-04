#!/usr/bin/env bash
# rebuck2 requeue e2e: two workers, kill the one running the action mid-flight,
# assert the driver requeues it onto the survivor and the build still succeeds.
# Worker 1 joins first so least-loaded dispatch deterministically picks it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PORT="${REBUCK2_PORT:-9957}"
SESSION="rebuck2-requeue-$$"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-requeue.XXXXXX")"
DLOG="$SCRATCH/driver.log"
ISOLATION="rebuck2rq"

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

echo "=== start driver + two workers"
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

cat > "$ROOT/test/.buckconfig.local" <<EOF
[build]
execution_platforms = root//platforms:re-exec

[buck2_re_client]
action_cache_address = grpc://127.0.0.1:$PORT
cas_address = grpc://127.0.0.1:$PORT
engine_address = grpc://127.0.0.1:$PORT
tls = false
EOF

echo "=== build //:slow in background, kill worker 1 mid-action"
(cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" clean >/dev/null 2>&1 || true)
(cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" build //:slow > "$SCRATCH/build.log" 2>&1) &
BUILD_PID=$!

wait_for "-> worker 1" "$DLOG" 120
echo "action dispatched to worker 1 — killing it"
kill -9 "$W1_PID"

wait_for "requeueing 1 job(s) from worker 1" "$DLOG" 120
echo "driver requeued the orphaned job"

wait "$BUILD_PID" || { cat "$SCRATCH/build.log" "$DLOG"; echo "FAIL: build did not survive the worker loss"; exit 1; }
grep -q "BUILD SUCCEEDED" "$SCRATCH/build.log" || { cat "$SCRATCH/build.log"; echo "FAIL: no BUILD SUCCEEDED"; exit 1; }
grep -q -- "-> worker 2" "$DLOG" || { cat "$DLOG"; echo "FAIL: job never reached worker 2"; exit 1; }
echo "PASS: worker died mid-action, job requeued to worker 2, build green"
