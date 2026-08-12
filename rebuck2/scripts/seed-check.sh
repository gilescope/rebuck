#!/usr/bin/env bash
# Does cache-mount seeding work? Locally, in about a minute.
#
# Thirteen separate faults stood between writing `seed_cache_mounts` and
# seeing it work, and none of them was the mechanism. The first four cost a
# twenty-five minute CI run each. A five-minute smoke job took the next
# several. This rig - a buildkitd in docker with our registry beside it -
# took the last two in about four minutes, and one of those had already
# consumed three CI runs.
#
# The lesson is priced: build the cheapest instrument first.
#
#   scripts/seed-check.sh [--fill-mb N]
#
# Leaves the daemon running for a second pass; `docker rm -f seedcheck-bk`
# when done.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$HERE/../target/release/rebuck2}
PORT=${PORT:-15099}
BKPORT=${BKPORT:-28399}
RUN=${RUN:-${TMPDIR:-/tmp}/rebuck2-seedcheck}

# The host, as the DAEMON sees it. Docker Desktop does not share the host's
# network namespace the way a Linux runner does, so `172.17.0.1` is a Linux
# answer and `host.docker.internal` is the portable one. The host itself may
# not resolve that name, which is why the base check downstream only warns.
HOSTADDR=${HOSTADDR:-host.docker.internal}

[ -x "$BIN" ] || { echo "build rebuck2 --release first (or set BIN=)"; exit 1; }
mkdir -p "$RUN"

docker rm -f seedcheck-bk >/dev/null 2>&1
pkill -f "rebuck2 registry --bind 0.0.0.0:$PORT" >/dev/null 2>&1
sleep 1

# Pull-through ON: the blobs of the base image come through it. The MANIFEST
# does not - the registry proxies blobs and not tags - which is why the tag
# is PUT by hand below.
REBUCK_UPSTREAM_REGISTRY=1 "$BIN" registry --bind "0.0.0.0:$PORT" \
  --store "$RUN/store" > "$RUN/registry.log" 2>&1 &
sleep 2

docker run -d --name seedcheck-bk --privileged -p "$BKPORT:8372" \
  -e EARTHLY_ADDITIONAL_BUILDKIT_CONFIG="[registry.\"$HOSTADDR:$PORT\"]
  http = true
  insecure = true" \
  -e BUILDKIT_TCP_TRANSPORT_ENABLED=true -e BUILDKIT_TLS_ENABLED=false \
  earthbuild/buildkitd:v0.8.17 >/dev/null || exit 1
for _ in $(seq 1 90); do
  docker exec seedcheck-bk buildctl --addr tcp://127.0.0.1:8372 debug workers \
    >/dev/null 2>&1 && break
  sleep 1
done

# The base image, by hand. `docker push` cannot reach a host port from
# inside Docker Desktop's VM, and a sessionless solve cannot reach Docker
# Hub - so the manifest is fetched with a token and PUT straight in, and the
# blobs arrive through pull-through when the daemon asks.
arch=$(docker version --format '{{.Server.Arch}}' 2>/dev/null || echo arm64)
tok=$(curl -s "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/busybox:pull" | jq -r .token)
acc_list="application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.index.v1+json"
acc_one="application/vnd.docker.distribution.manifest.v2+json,application/vnd.oci.image.manifest.v1+json"
dig=$(curl -s -H "Authorization: Bearer $tok" -H "Accept: $acc_list" \
        https://registry-1.docker.io/v2/library/busybox/manifests/1 \
      | jq -r --arg a "$arch" '.manifests[]|select(.platform.os=="linux" and .platform.architecture==$a)|.digest' | head -1)
[ -n "$dig" ] || { echo "no linux/$arch busybox manifest"; exit 1; }
man=$(curl -s -H "Authorization: Bearer $tok" -H "Accept: $acc_one" \
        "https://registry-1.docker.io/v2/library/busybox/manifests/$dig")
ct=$(echo "$man" | jq -r '.mediaType // "application/vnd.docker.distribution.manifest.v2+json"')
echo "$man" | curl -sf -X PUT -H "Content-Type: $ct" --data-binary @- \
  "http://127.0.0.1:$PORT/v2/library/busybox/manifests/1" -o /dev/null \
  || { echo "could not seed the base image into the registry"; exit 1; }

# "$@" forwarded, so `--fill-mb 200` reaches the binary. Without this the
# script silently ignored it and two runs at different sizes came back with
# identical timings, which reads as "200 MiB is free" rather than as "the
# flag did nothing".
"$BIN" check-seeding --bk "127.0.0.1:$BKPORT" --registry "$HOSTADDR:$PORT" \
  --base "$HOSTADDR:$PORT/library/busybox:1" "$@"
rc=$?
echo "== check-seeding rc=$rc  (logs in $RUN)"
exit $rc
