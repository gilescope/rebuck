#!/usr/bin/env bash
# Does an estargz layer get MOUNTED rather than downloaded? Locally, in minutes.
#
# estargz is the only idea in this project aimed at the unpack term rather
# than at transfer or placement, and it has a prerequisite that makes it
# very easy to measure a lie: without the stargz snapshotter on the
# CONSUMING daemon, containerd treats an estargz layer as ordinary gzip.
# The layer is BIGGER than gzip (TOC, footer, per-file gzip members), so a
# half-configured run measures a regression and reads as "estargz is worse".
# That is shape 13 - a knob wired, documented and inert - waiting to happen.
#
#   scripts/stargz-check.sh [--mib 192]
#
# The discriminating observation is RANGE REQUESTS. A normal pull issues one
# full GET per blob and gets 200. A lazy pull issues many partial GETs and
# gets 206. Bytes alone cannot separate them - a lazy mount that happens to
# touch everything moves the same bytes - so the count of 206s is what says
# the mechanism engaged at all, and the byte total says what it bought.
#
# `docker rm -f stargz-reg stargz-src stargz-dst` when done.
set -uo pipefail
MIB=192
while [ $# -gt 0 ]; do
  case "$1" in
    --mib) MIB=$2; shift 2 ;;
    *) echo "unknown argument: $1"; exit 2 ;;
  esac
done

BK=${BK:-earthbuild/buildkitd:v0.8.17}
REGPORT=${REGPORT:-15099}
RUN=${RUN:-${TMPDIR:-/tmp}/rebuck2-stargzcheck}
# How the daemons reach the registry. Both are containers on the default
# bridge, so this is the bridge gateway and NOT `host.docker.internal` -
# that name is how a container reaches the HOST, and the host is not what
# is serving here. Getting this wrong is how `check-reserve` printed a
# clean flat line for a total instrument failure (shape 15).
GW=${GW:-172.17.0.1}

mkdir -p "$RUN"
rm -f "$RUN"/*.log

fail() { echo "✗ $*"; exit 1; }

# FUSE is not optional: the stargz snapshotter mounts a filesystem. Without
# /dev/fuse the daemon still STARTS and still reports snapshotter=stargz -
# it just silently falls back to fetching whole layers, which is precisely
# the green-but-inert outcome this script exists to catch.
#
# Probed INSIDE a container, not on this host. The mount happens wherever
# the docker daemon runs, which on macOS is a Linux VM - testing the Mac's
# /dev/fuse asks the wrong machine, and answers confidently. The first
# version of this line did exactly that and failed on a working setup.
docker run --rm --device /dev/fuse alpine:3.20 test -e /dev/fuse >/dev/null 2>&1 \
  || fail "/dev/fuse is absent in the docker daemon's kernel; stargz cannot mount"

docker rm -f stargz-reg stargz-src stargz-dst >/dev/null 2>&1
sleep 1

echo "── registry ──────"
docker run -d --name stargz-reg -p "$REGPORT:5000" registry:2 >/dev/null \
  || fail "could not start registry:2"
for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$REGPORT/v2/" >/dev/null 2>&1 && break
  sleep 1
done
curl -sf "http://127.0.0.1:$REGPORT/v2/" >/dev/null || fail "registry never came up"

REGCFG="[registry.\"$GW:$REGPORT\"]
  http = true
  insecure = true"

# Start a daemon through earthly's OWN entrypoint, not by replacing it.
#
# The entrypoint renders /etc/buildkitd.toml.template and appends
# ${EARTHLY_ADDITIONAL_BUILDKIT_CONFIG}. Overriding the entrypoint to call
# buildkitd directly skips that entirely, so the registry stanza never
# lands and the consumer tries HTTPS against a plain-HTTP registry. The
# first version of this script did exactly that.
#
# And the snapshotter CANNOT go in EARTHLY_ADDITIONAL_BUILDKIT_CONFIG: the
# template already hardcodes
#
#   [worker.oci]
#     snapshotter = "auto"
#
# and TOML forbids a duplicate table, so appending a second [worker.oci]
# is a parse error rather than an override. The template has to be edited
# in place before the entrypoint reads it - which is a real constraint on
# putting this in CI, not an artefact of testing locally.
boot() { # name, snapshotter
  local name=$1 snap=$2
  docker run -d --name "$name" --privileged --device /dev/fuse \
    -e BUILDKIT_TCP_TRANSPORT_ENABLED=true -e BUILDKIT_TLS_ENABLED=false \
    -e EARTHLY_ADDITIONAL_BUILDKIT_CONFIG="$REGCFG" \
    --entrypoint sh "$BK" -c \
    "sed -i 's|snapshotter = \"auto\"|snapshotter = \"$snap\"|' /etc/buildkitd.toml.template \
     && exec /usr/bin/entrypoint.sh buildkitd --config=/etc/buildkitd.toml" >/dev/null \
    || fail "could not start $name"
  for _ in $(seq 1 90); do
    docker exec "$name" buildctl --addr tcp://127.0.0.1:8372 debug workers \
      >/dev/null 2>&1 && break
    sleep 1
  done
  docker exec "$name" buildctl --addr tcp://127.0.0.1:8372 debug workers >/dev/null 2>&1 || {
    docker logs "$name" 2>&1 | tail -20; fail "$name never became ready"; }
  # The daemon reports the snapshotter it actually loaded. Asking it beats
  # assuming the sed matched.
  docker exec "$name" buildctl --addr tcp://127.0.0.1:8372 debug workers \
    --format '{{range .}}{{.Labels}}{{end}}' 2>/dev/null | grep -q "snapshotter:$snap" \
    || fail "$name is not running the $snap snapshotter - the template edit did not take"
  echo "◈ $name up, snapshotter=$snap"
}

echo "── producer (default snapshotter) ──────"
boot stargz-src overlayfs

# One big incompressible file so the layer cannot be gzipped away, and one
# tiny file which is ALL the consumer will read. If lazy pull works, the
# consumer fetches the tiny file's chunks and not the big file's.
cat > "$RUN/Dockerfile.src" <<EOF
FROM alpine:3.20
RUN dd if=/dev/urandom of=/big.bin bs=1M count=$MIB 2>/dev/null \\
 && echo marker-value > /small.txt
EOF
docker cp "$RUN/Dockerfile.src" stargz-src:/Dockerfile >/dev/null

IMG="$GW:$REGPORT/stargz/probe:v1"
echo "building and exporting as estargz (${MIB} MiB payload)"
docker exec stargz-src buildctl --addr tcp://127.0.0.1:8372 build \
  --frontend dockerfile.v0 --local context=/ --local dockerfile=/ \
  --output "type=image,name=$IMG,push=true,registry.insecure=true,compression=estargz,force-compression=true,oci-mediatypes=true" \
  > "$RUN/build-src.log" 2>&1 || { tail -25 "$RUN/build-src.log"; fail "producer build failed"; }

# Did it actually WRITE estargz? A codec that silently did nothing is the
# most likely way this whole test passes while measuring gzip. estargz
# stamps a TOC digest annotation on every converted layer.
MAN=$(curl -sf -H "Accept: application/vnd.oci.image.manifest.v1+json" \
        "http://127.0.0.1:$REGPORT/v2/stargz/probe/manifests/v1")
TOCS=$(echo "$MAN" | jq '[.layers[]|select(.annotations["containerd.io/snapshot/stargz/toc.digest"])]|length')
LAYER_BYTES=$(echo "$MAN" | jq '[.layers[].size]|max')
[ "$TOCS" -gt 0 ] || fail "pushed manifest carries no stargz TOC annotation - the export was NOT estargz"
echo "◈ estargz confirmed on the wire: $TOCS layer(s) annotated, largest blob $((LAYER_BYTES/1024/1024)) MiB"

echo "── consumer (stargz snapshotter) ──────"
boot stargz-dst stargz

# From here the registry log is the instrument, so mark the boundary: the
# producer's PUSH is in the same log and must not be counted as a pull.
docker logs stargz-dst >/dev/null 2>&1
MARK=$(docker logs stargz-reg 2>&1 | wc -l | tr -d ' ')

cat > "$RUN/Dockerfile.dst" <<EOF
FROM $IMG
RUN cat /small.txt
EOF
docker cp "$RUN/Dockerfile.dst" stargz-dst:/Dockerfile >/dev/null
echo "consuming: reading ONE small file out of a ${MIB} MiB image"
docker exec stargz-dst buildctl --addr tcp://127.0.0.1:8372 build \
  --frontend dockerfile.v0 --local context=/ --local dockerfile=/ \
  > "$RUN/build-dst.log" 2>&1 || { tail -25 "$RUN/build-dst.log"; fail "consumer build failed"; }

echo "── verdict ──────"
docker logs stargz-reg 2>&1 | tail -n "+$((MARK+1))" > "$RUN/reg.log"
GETS=$(grep -c '"GET /v2/stargz/probe/blobs' "$RUN/reg.log")
PARTIAL=$(grep '"GET /v2/stargz/probe/blobs' "$RUN/reg.log" | grep -c '206')
SERVED=$(grep '"GET /v2/stargz/probe/blobs' "$RUN/reg.log" \
         | grep -o '"http.response.written":[0-9]*' | cut -d: -f2 \
         | awk '{s+=$1} END {printf "%d", s+0}')

printf '  blob GETs          %s\n' "$GETS"
printf '  of those, partial  %s  (HTTP 206 - the lazy-pull signature)\n' "$PARTIAL"
printf '  bytes served       %s MiB\n' "$((SERVED/1024/1024))"
printf '  largest blob       %s MiB\n' "$((LAYER_BYTES/1024/1024))"

# A fallback to a whole-layer pull is a CORRECT build and a failed
# experiment. Say which, and never let the two print the same thing.
if grep -qi 'failed to restore remote snapshot' "$RUN/build-dst.log"; then
  echo "⚠︎ the consumer log says it FELL BACK to a normal pull - see $RUN/build-dst.log"
fi
if [ "$PARTIAL" -eq 0 ]; then
  echo "✗ NOT LAZY: zero range requests. The layer was downloaded whole."
  echo "  estargz without a working mount is strictly worse than gzip - the"
  echo "  blob is bigger and nothing is skipped."
  exit 1
fi
if [ "$SERVED" -ge "$LAYER_BYTES" ]; then
  echo "⚠︎ LAZY BUT POINTLESS: range requests fired, and it still moved the"
  echo "  whole layer. The mount works; this workload reads all of it."
  exit 0
fi
PCT=$((SERVED * 100 / LAYER_BYTES))
echo "◈ LAZY: fetched ${PCT}% of the largest layer to read one file."
echo "  This is the unpack term being skipped rather than made cheaper."
