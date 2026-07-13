#!/usr/bin/env bash
# rebuck2 lease e2e: two machines, one key, ONE build.
#
# Two independent processes race the same cache key across a real iroh mesh.
# One is granted the lease and builds; the other BLOCKS and is handed the
# leader's result without ever building it. That is the property a single
# buildkitd gets free from its own solver, and which N ephemeral daemons lose —
# the whole point of docs/buildkit-plan.md P2.
#
# A leader HOLDS its connection while building: a lease lives exactly as long
# as its holder does. `rebuck2 claim` models that — it prints "leader", then
# blocks on stdin (stdin standing in for the build), and publishes whatever
# stdin yields. So a fifo lets us decide precisely when the leader finishes.
#
# Then the failure that actually matters: a leader that DIES must not strand
# its follower. A hang is a worse bug than the duplicate work we set out to
# prevent, so the follower must be told to re-claim (exit 75), not left waiting
# forever on a result nobody is computing. A hard-killed leader sends no QUIC
# close frame, so it is detected by SILENCE: a follower's worst-case stall is the
# lease TTL (--lease-ttl-secs; 90s by default, 8s here to keep the test quick).
#
# Usage: rebuck2/tests/e2e-lease.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SESSION="rebuck2-lease-e2e-$$"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/rebuck2-lease.XXXXXX")"

cleanup() {
    kill "${DRIVER_PID:-}" "${A_PID:-}" "${B_PID:-}" "${C_PID:-}" "${D_PID:-}" 2>/dev/null || true
    echo "logs kept in $SCRATCH"
}
trap cleanup EXIT

echo "=== build rebuck2"
cargo build --release --manifest-path "$ROOT/rebuck2/Cargo.toml"
BIN="$ROOT/rebuck2/target/release/rebuck2"

echo "=== start driver (the fleet's single coordinator)"
# Short TTL: a leader killed with -9 sends no QUIC close frame, so the driver
# can only learn of its death by SILENCE — the TTL IS the detection latency. The
# 90s default would make this test take two minutes to prove a property that
# does not depend on the number.
"$BIN" driver --grpc-port 9977 --session "$SESSION" --lease-ttl-secs 8 \
    --store "$SCRATCH/driver-store" > "$SCRATCH/driver.log" 2>&1 &
DRIVER_PID=$!
for _ in $(seq 1 30); do
    grep -q 'REAPI listening' "$SCRATCH/driver.log" 2>/dev/null && break
    sleep 1
done
grep -q 'REAPI listening' "$SCRATCH/driver.log" || { echo "FAIL: driver never started"; exit 1; }

wait_for() { # wait_for <file> <text> <secs>
    for _ in $(seq 1 "$3"); do grep -q "$2" "$1" 2>/dev/null && return 0; sleep 1; done
    return 1
}

# ---------------------------------------------------------------- build once
KEY="sha256:$(printf 'a%.0s' $(seq 1 64))"
FIFO="$SCRATCH/a.fifo"; mkfifo "$FIFO"

echo
echo "=== machine A claims the key and HOLDS it while 'building'"
"$BIN" claim --session "$SESSION" --key "$KEY" < "$FIFO" > "$SCRATCH/a.out" 2>&1 &
A_PID=$!
exec 3>"$FIFO"   # hold the write end open so A blocks rather than seeing EOF.
                 # Every process spawned after this inherits fd 3 and would ALSO
                 # hold the fifo open — hence the 3>&- on the followers below.
wait_for "$SCRATCH/a.out" leader 15 || { echo "FAIL: A never became leader"; cat "$SCRATCH/a.out"; exit 1; }
echo "A -> leader (holding the lease, building)"

echo "=== machine B claims the SAME key — it must WAIT, not rebuild"
"$BIN" claim --session "$SESSION" --key "$KEY" 3>&- > "$SCRATCH/b.out" 2>&1 &
B_PID=$!
sleep 3
kill -0 "$B_PID" 2>/dev/null || { echo "FAIL: B exited — it did not wait for the leader"; cat "$SCRATCH/b.out"; exit 1; }
echo "B -> blocked (it is NOT rebuilding)"

echo "=== A finishes and publishes"
printf 'the-one-true-layer' >&3
exec 3>&-        # EOF: A releases and exits
wait "$A_PID"

echo "=== B must now receive A's result, having built nothing"
wait "$B_PID" || { echo "FAIL: B exited non-zero"; cat "$SCRATCH/b.out"; exit 1; }
GOT="$(cat "$SCRATCH/b.out")"
[ "$GOT" = "the-one-true-layer" ] || { echo "FAIL: B got '$GOT'"; exit 1; }
echo "B -> got the leader's result over the mesh. Built ONCE."

# ------------------------------------------------------------- dead leader
echo
echo "=== the failure that matters: a leader that DIES mid-build"
KEY2="sha256:$(printf 'b%.0s' $(seq 1 64))"
FIFO2="$SCRATCH/c.fifo"; mkfifo "$FIFO2"

"$BIN" claim --session "$SESSION" --key "$KEY2" < "$FIFO2" > "$SCRATCH/c.out" 2>&1 &
C_PID=$!
exec 4>"$FIFO2"
wait_for "$SCRATCH/c.out" leader 15 || { echo "FAIL: C never became leader"; exit 1; }
echo "C -> leader (holding the lease)"

"$BIN" claim --session "$SESSION" --key "$KEY2" 3>&- 4>&- > "$SCRATCH/d.out" 2>&1 &
D_PID=$!
sleep 3
kill -0 "$D_PID" 2>/dev/null || { echo "FAIL: D did not wait"; exit 1; }
echo "D -> blocked on C"

echo "=== C is killed. D must be told to re-claim, NOT left hanging."
kill -9 "$C_PID"; exec 4>&-
set +e
# Bounded: if D is still waiting after this, it has hung — the exact bug the
# lease exists to prevent.
# Generous against the 8s TTL + 5s reap tick, but far short of a hang.
( sleep 40; kill -9 "$D_PID" 2>/dev/null ) & KILLER=$!
wait "$D_PID"; D_CODE=$?
kill "$KILLER" 2>/dev/null
set -e

[ "$D_CODE" != "137" ] || { echo "FAIL: D HUNG on a dead leader — a stranded follower"; exit 1; }
[ "$D_CODE" = "75" ] || { echo "FAIL: expected 75 (re-claim), got $D_CODE"; cat "$SCRATCH/d.out"; exit 1; }
echo "D -> exit 75 (re-claim). The dead leader stranded nobody."

echo
echo "PASS: two machines, one key, ONE build — and a dead leader strands nobody"
