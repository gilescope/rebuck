#!/usr/bin/env bash
# resolve-rev.sh <git-url> <rev> <branch> <action-ref>
# Resolve the rebuck2 engine rev to install: explicit rev > branch tip >
# the sha the calling action was pinned at. Prints the sha.
set -euo pipefail

git_url="${1:?git url required}"
rev="${2:-}"
branch="${3:-}"
action_ref="${4:-}"

if [ -z "$rev" ] && [ -n "$branch" ]; then
  rev=$(git ls-remote "$git_url" "refs/heads/$branch" | cut -f1)
  [ -n "$rev" ] || { echo "branch $branch not found on $git_url" >&2; exit 1; }
fi
rev="${rev:-$action_ref}"
case "$rev" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) ;;
  *) echo "rev '$rev' is not a sha - pin the action by full sha or pass rev/branch" >&2; exit 1 ;;
esac
echo "$rev"
