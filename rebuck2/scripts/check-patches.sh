#!/usr/bin/env bash
# Do the patches in patches/ still apply, and do they apply WHOLE?
#
# `git apply --check` is not enough, and the difference cost a CI round. A
# hunk header carries the line counts of its own body; edit the body without
# updating the header and git reads only as many lines as the header claims,
# applies that much, and reports SUCCESS. The dropped remainder was a
# function definition, so earthbuild failed to compile with `undefined:
# sortedKeys` two steps later - nowhere near the patch.
#
# So: apply for real into a throwaway worktree, then assert that symbols the
# patch is supposed to introduce are actually there.
#
#   scripts/check-patches.sh [earthbuild-checkout]
set -euo pipefail

EB=${1:-$HOME/git/EarthBuild/earthbuild}
HERE=$(cd "$(dirname "$0")" && pwd)
PATCHES=$(cd "$HERE/../patches" && pwd)

[ -d "$EB/.git" ] || { echo "no earthbuild checkout at $EB"; exit 1; }

# A worktree, not the checkout: this must never leave the user's tree dirty,
# and `git checkout -- .` does not remove the files a patch ADDS.
work=$(mktemp -d "${TMPDIR:-/tmp}/patchcheck.XXXXXX")
# shellcheck disable=SC2329 # invoked by the trap below
cleanup() { git -C "$EB" worktree remove --force "$work" >/dev/null 2>&1 || true; }
trap cleanup EXIT
git -C "$EB" worktree add --detach --quiet "$work" HEAD

rc=0
for p in "$PATCHES"/*.patch; do
  name=$(basename "$p")
  if ! git -C "$work" apply "$p" 2>/tmp/patcherr; then
    echo "FAIL  $name does not apply"; sed 's/^/      /' /tmp/patcherr; rc=1; continue
  fi
  # Every symbol the patch introduces must survive the apply. A truncated
  # hunk applies "successfully" and leaves callers of a function the patch
  # never finished adding.
  # The DEFINITION, not any mention. Searching for the bare name finds the
  # call site the patch also adds, so a truncated hunk that drops the
  # function but keeps its caller passes - which is exactly the failure this
  # script exists to catch, and the first version of it did not.
  missing=""
  while read -r sym; do
    [ -n "$sym" ] || continue
    grep -rqF -- "func $sym(" "$work" || missing="$missing $sym"
  done <<< "$(grep -oE '^\+func [a-zA-Z]+' "$p" | sed 's/^+func //' | sort -u)"
  if [ -n "$missing" ]; then
    echo "FAIL  $name applied but these are missing:$missing"; rc=1
  else
    echo "ok    $name"
  fi
  git -C "$work" checkout -- . ; git -C "$work" clean -fdq
done
exit $rc
