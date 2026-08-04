#!/usr/bin/env bash
# rebuck2 local e2e: driver + worker as separate processes (real iroh mesh),
# buck2 builds test/ with local execution DISABLED — success means every
# action executed on the worker and round-tripped through the driver's CAS.
#
# Usage: rebuck2/tests/e2e-local.sh   (from the repo root; buck2 + cargo on PATH)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PORT="${REBUCK2_PORT:-9955}"
SESSION="rebuck2-e2e-$$"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-e2e.XXXXXX")"
DRIVER_LOG="$SCRATCH/driver.log"
WORKER_LOG="$SCRATCH/worker.log"
ISOLATION="rebuck2e2e"

cleanup() {
    kill "${DRIVER_PID:-}" "${WORKER_PID:-}" 2>/dev/null || true
    (cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" clean 2>/dev/null) || true
    rm -f "$ROOT/test/.buckconfig.local"
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

echo "=== build rebuck2"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== start driver (no local exec — worker or bust)"
"$BIN" driver --grpc-port "$PORT" --session "$SESSION" \
    --store "$SCRATCH/driver-store" --min-workers 1 --no-local-exec \
    >"$DRIVER_LOG" 2>&1 &
DRIVER_PID=$!

echo "=== start worker"
"$BIN" worker --session "$SESSION" --store "$SCRATCH/worker-store" \
    >"$WORKER_LOG" 2>&1 &
WORKER_PID=$!

for _ in $(seq 1 60); do
    grep -q "worker 1 joined" "$DRIVER_LOG" 2>/dev/null && break
    kill -0 "$DRIVER_PID" || { cat "$DRIVER_LOG"; echo "driver died"; exit 1; }
    kill -0 "$WORKER_PID" || { cat "$WORKER_LOG"; echo "worker died"; exit 1; }
    sleep 1
done
grep -q "worker 1 joined" "$DRIVER_LOG" || { cat "$DRIVER_LOG" "$WORKER_LOG"; echo "worker never joined"; exit 1; }
echo "worker joined the mesh"

echo "=== point buck2 at the driver (remote-exec-only platform)"
cat > "$ROOT/test/.buckconfig.local" <<EOF
[build]
execution_platforms = root//platforms:re-exec

[buck2_re_client]
action_cache_address = grpc://127.0.0.1:$PORT
cas_address = grpc://127.0.0.1:$PORT
engine_address = grpc://127.0.0.1:$PORT
tls = false
EOF

echo "=== buck2 build"
(cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" clean >/dev/null 2>&1 || true)
(cd "$ROOT/test" && buck2 --isolation-dir "$ISOLATION" build //:hello)

echo "=== assertions"
grep -q -- "-> worker 1" "$DRIVER_LOG" || { echo "FAIL: no job was dispatched to the worker"; tail -50 "$DRIVER_LOG"; exit 1; }
JOBS=$(grep -c -- "-> worker 1" "$DRIVER_LOG")
echo "PASS: build succeeded; $JOBS action(s) executed remotely on the worker"
