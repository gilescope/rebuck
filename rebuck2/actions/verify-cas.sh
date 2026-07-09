#!/usr/bin/env bash
# verify-cas.sh <store-dir>
# CAS blobs are self-certifying: the filename IS the sha256 of the
# content. Shard artifacts cross GitHub's branch-scoping wall (any run,
# any branch, including fork PRs, shares the artifact namespace), so
# every imported blob must prove itself - a planted file with a
# legitimate digest name and poisoned bytes is deleted, degrading the
# attack to a cold shard instead of code execution.
set -euo pipefail

store="${1:?store dir required}"
[ -d "$store/cas" ] || { echo "verify-cas: no cas/ - nothing to verify"; exit 0; }

if command -v sha256sum >/dev/null 2>&1; then
  H() { sha256sum "$1" | cut -d' ' -f1; }
else
  H() { shasum -a 256 "$1" | cut -d' ' -f1; } # macOS
fi

ok=0 bad=0
while IFS= read -r f; do
  name=$(basename "$f")
  if [ "$(H "$f")" = "$name" ]; then
    ok=$((ok + 1))
  else
    echo "verify-cas: DIGEST MISMATCH - deleting $name" >&2
    rm -f "$f"
    bad=$((bad + 1))
  fi
done < <(find "$store/cas" -maxdepth 1 -type f)

echo "verify-cas: $ok verified, $bad rejected"
[ "$bad" -eq 0 ] || echo "verify-cas: WARNING - rejected blobs suggest a poisoned or corrupt shard artifact" >&2
