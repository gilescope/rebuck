#!/usr/bin/env bash
# crate-key.sh <git-url> <rev>
# Binary-cache key for the rebuck2 engine at <rev>: the git tree sha of
# rebuck2/src plus the Cargo.toml/Cargo.lock blob shas. Keying on the
# CRATE SOURCE instead of the commit means actions-only bumps (yml under
# rebuck2/actions/, docs, unrelated dirs) reuse the compiled binary -
# a rev-keyed cache cost a ~4.5min cargo install per pin bump.
# Falls back to the rev itself if the API lookup fails (never worse than
# the old behaviour). Requires GH_TOKEN for the API calls.
set -euo pipefail

git_url="${1:?git url required}"
rev="${2:?rev required}"

repo="${git_url#*github.com/}"; repo="${repo%.git}"
key=$(
  set -e
  sub=$(gh api "repos/$repo/git/trees/$rev" \
    --jq '.tree[] | select(.path=="rebuck2") | .sha')
  gh api "repos/$repo/git/trees/$sub" \
    --jq '[.tree[] | select(.path=="src" or .path=="Cargo.toml" or .path=="Cargo.lock") | .sha] | join("-")'
) 2>/dev/null || true

if [ -z "$key" ]; then
  echo "crate-key: API lookup failed - falling back to rev" >&2
  key="$rev"
fi
echo "$key"
