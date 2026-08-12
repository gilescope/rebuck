#!/usr/bin/env bash
# Does a daemon re-fetch a base image it already holds? Locally, in a minute.
#
# A fleet run served 25,658 MiB while holding 277 MiB of distinct content
# over a megabyte. Two explanations want opposite fixes: one daemon
# re-materialising the same base per solve (a caching problem) or six
# daemons each fetching it once (a placement problem). No CI run so far
# distinguishes them, and each one costs half an hour.
#
#   scripts/reserve-check.sh [--n 8]
#
# Shares its scaffolding with seed-check.sh deliberately - same daemon, same
# hand-PUT manifest, same reason. `docker rm -f reservecheck-bk` when done.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$HERE/../target/release/rebuck2}
PORT=${PORT:-15098}
BKPORT=${BKPORT:-28398}
RUN=${RUN:-${TMPDIR:-/tmp}/rebuck2-reservecheck}
HOSTADDR=${HOSTADDR:-host.docker.internal}

[ -x "$BIN" ] || { echo "build rebuck2 --release first (or set BIN=)"; exit 1; }
mkdir -p "$RUN"

docker rm -f reservecheck-bk >/dev/null 2>&1
pkill -f "rebuck2 registry --bind 0.0.0.0:$PORT" >/dev/null 2>&1
sleep 1

REBUCK_UPSTREAM_REGISTRY=1 "$BIN" registry --bind "0.0.0.0:$PORT" \
  --store "$RUN/store" > "$RUN/registry.log" 2>&1 &
sleep 2

docker run -d --name reservecheck-bk --privileged -p "$BKPORT:8372" \
  -e EARTHLY_ADDITIONAL_BUILDKIT_CONFIG="[registry.\"$HOSTADDR:$PORT\"]
  http = true
  insecure = true" \
  -e BUILDKIT_TCP_TRANSPORT_ENABLED=true -e BUILDKIT_TLS_ENABLED=false \
  earthbuild/buildkitd:v0.8.17 >/dev/null || exit 1
for _ in $(seq 1 90); do
  docker exec reservecheck-bk buildctl --addr tcp://127.0.0.1:8372 debug workers \
    >/dev/null 2>&1 && break
  sleep 1
done

# The manifest by hand: the registry proxies blobs, not tags. Same dance as
# seed-check.sh, and the same reason it cannot be a docker push.
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

# --stats over loopback: $HOSTADDR is how the DAEMON reaches this host and
# does not necessarily resolve on the host itself.
"$BIN" check-reserve --bk "127.0.0.1:$BKPORT" --registry "$HOSTADDR:$PORT" \
  --stats "127.0.0.1:$PORT" \
  --base "$HOSTADDR:$PORT/library/busybox:1" "$@"
