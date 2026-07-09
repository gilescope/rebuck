#!/usr/bin/env bash
# install-buck2.sh <version> <os>-<arch> [dest]
# Install a pinned facebook/buck2 release. Pinned because action digests
# must be stable - a floating buck2 invalidates every warm cache hit.
set -euo pipefail

version="${1:?buck2 release tag required (pin it)}"
platform="${2:?runner platform required (os-arch)}"
dest="${3:-$HOME/bin}"

case "$platform" in
  Linux-X64)   triple=x86_64-unknown-linux-musl; exe="" ;;
  Linux-ARM64) triple=aarch64-unknown-linux-gnu; exe="" ;;
  macOS-ARM64) triple=aarch64-apple-darwin;      exe="" ;;
  macOS-X64)   triple=x86_64-apple-darwin;       exe="" ;;
  Windows-X64) triple=x86_64-pc-windows-msvc;    exe=".exe" ;;
  *) echo "unsupported runner platform: $platform" >&2; exit 1 ;;
esac

mkdir -p "$dest"
curl -fsSL "https://github.com/facebook/buck2/releases/download/${version}/buck2-${triple}${exe}.zst" \
  | zstd -d > "$dest/buck2$exe"
chmod +x "$dest/buck2$exe"
echo "$dest" >> "$GITHUB_PATH"
"$dest/buck2$exe" --version
