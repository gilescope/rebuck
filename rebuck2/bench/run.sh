#!/usr/bin/env bash
# Bench the fleet: up, wait for quorum, plant withheld blobs on worker3
# (so poisoned lookups validate through the MESH), fire, report, down.
# Usage: bench/run.sh [entries] [poisoned_pct] [concurrency] [rounds]
set -euo pipefail
cd "$(dirname "$0")/.."
ENTRIES="${1:-2000}" PCT="${2:-20}" CONC="${3:-16}" ROUNDS="${4:-3}"

docker compose -f bench/docker-compose.yml up -d --build
trap 'docker compose -f bench/docker-compose.yml down -v' EXIT

echo "[run] waiting for quorum..."
for _ in $(seq 1 60); do
  docker compose -f bench/docker-compose.yml logs driver 2>/dev/null | grep -q "worker 3 joined" && break
  sleep 2
done

PLANT=$(mktemp -d)
cargo run --release --bin rebuck2 -- bench \
  --grpc http://127.0.0.1:19092 \
  --entries "$ENTRIES" --poisoned-pct "$PCT" --plant-dir "$PLANT" \
  --concurrency "$CONC" --rounds "$ROUNDS"

echo "[run] planting withheld blobs into worker3's store (mesh path)..."
docker compose -f bench/docker-compose.yml cp "$PLANT/cas" worker3:/store/worker/cas 2>/dev/null \
  || docker cp "$PLANT/cas/." "$(docker compose -f bench/docker-compose.yml ps -q worker3)":/store/worker/cas/
echo "[run] waiting a bloom interval (30s) so worker3 announces..."
sleep 35

echo "[run] refire: poisoned entries should now be MESH-VALIDATED hits"
cargo run --release --bin rebuck2 -- bench \
  --grpc http://127.0.0.1:19092 \
  --entries "$ENTRIES" --poisoned-pct "$PCT" \
  --concurrency "$CONC" --rounds "$ROUNDS"
rm -rf "$PLANT"
