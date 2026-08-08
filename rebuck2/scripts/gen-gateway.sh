#!/usr/bin/env bash
# Regenerate the buildkit GATEWAY bindings (moby.buildkit.v1.frontend).
#
# The output is COMMITTED under src/generated/, and there is deliberately no
# build.rs: generation needs a buildkit checkout and a hand-built include
# tree, and a build that depends on either is neither hermetic nor
# reproducible on a fresh machine.
#
# Run this only when the gateway wire changes. Requires protoc and a
# buildkit checkout.
set -euo pipefail

BUILDKIT="${BUILDKIT:-$HOME/git/gilescope/buildkit}"
RES="$(find "$HOME/.cargo/registry/src" -maxdepth 2 -type d -name 'bollard-buildkit-proto-*' | head -1)/resources"
INC="$(mktemp -d)"

# gateway.proto imports by full `github.com/...` path, so the include tree
# has to look like a GOPATH. bollard already vendors everything buildkit's
# own checkout does not (google/rpc, fsutil, vtproto), so the tree is three
# symlinks rather than a second vendor dir.
mkdir -p "$INC/github.com/moby" "$INC/github.com/tonistiigi" "$INC/github.com/planetscale"
ln -sfn "$BUILDKIT"      "$INC/github.com/moby/buildkit"
ln -sfn "$RES/fsutil"    "$INC/github.com/tonistiigi/fsutil"
ln -sfn "$RES/vtproto"   "$INC/github.com/planetscale/vtprotobuf"

echo "include tree: $INC"
echo "resources:    $RES"
echo "run: GEN_INC=$INC GEN_RES=$RES cargo run --bin gen-gateway"
