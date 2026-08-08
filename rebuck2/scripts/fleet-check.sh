#!/usr/bin/env bash
# Check the claims in docs/fleet-findings.md still hold.
#
# Every scenario here corresponds to a documented finding. They assert
# STRUCTURE - where work was placed, whether the bytes match, whether a build
# survived something being destroyed - and merely report timings, because
# wall clock moves for reasons that have nothing to do with this code and a
# suite that fails on a busy laptop gets switched off.
#
#   rebuck2/scripts/fleet-check.sh
#
# Local daemons only, so it needs no second machine. The cross-machine
# numbers are in the findings doc and are not checked here.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
fleet="$here/fleet.sh"
work=${WORK:-20}
builds=${BUILDS:-12}
slots=${SLOTS:-8}
base="${TMPDIR:-/tmp}/rebuck2-check-base"

pass=0
fail=0
ok() {
  printf '  \033[32mPASS\033[0m %s\n' "$1"
  pass=$((pass + 1))
}
no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  fail=$((fail + 1))
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1 (want $3, got $2)"; fi; }

run() {
  # Everything shares one work size and slot count so the expected placement
  # is arithmetic rather than a guess.
  #
  # `-u REMOTE` is load-bearing. Setting REMOTE to run the cross-machine
  # block at the bottom leaked into every LOCAL scenario, quietly turning
  # two-daemon fleets into three-peer ones - the surplus then split 2/2 and
  # the assertions failed against arithmetic that was still correct for the
  # fleet they thought they had.
  env -u REMOTE -u MIRROR_HOST \
    REBUCK2_LLB_WORK="$work" REBUCK2_LLB_PLATFORM=any \
    REBUCK2_HOME_SLOTS="$slots" BUILDS="$builds" "$@" "$fleet" 2>&1
}

# Placement for one peer index, read from the `placed` line ONLY.
#
# The first version grepped the whole output for `1: <n>` and matched a
# different line, reporting twelve builds on a peer that had four. Anchor to
# the line that means what is being asked.
placed() {
  echo "$1" | grep 'placed' | grep -o "$2: [0-9]*" | grep -o '[0-9]*$' |
    head -1
}

echo "== baseline (one daemon, no proxy)"
out=$(run NOPROXY=1 DAEMONS=1 RUN="$base")
echo "$out" | grep -E '^wall' | tr -s ' '
check "baseline builds succeed" "$(echo "$out" | grep -c '^failed  : 0')" 1

echo
echo "== two daemons: saturation places the surplus away"
out=$(run DAEMONS=2 EXPECT="$base/digests.txt")
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
check "outputs identical to baseline" \
  "$(echo "$out" | grep -c 'outputs identical')" 1
check "home takes exactly its slots" \
  "$(echo "$out" | grep -o '{0: [0-9]*' | grep -o '[0-9]*$')" "$slots"
check "the rest goes to the peer" "$(placed "$out" 1)" "$((builds - slots))"

echo
echo "== a peer destroyed mid-build"
out=$(run DAEMONS=2 KILL_AFTER=2 EXPECT="$base/digests.txt")
echo "$out" | grep -E '^wall' | tr -s ' ' | sed 's/^/  /'
check "every build still finishes" "$(echo "$out" | grep -c '^failed  : 0')" 1
check "outputs still identical" "$(echo "$out" | grep -c 'outputs identical')" 1

echo
echo "== the registry destroyed mid-build"
out=$(run DAEMONS=2 KILL_REGISTRY_AFTER=2 EXPECT="$base/digests.txt")
echo "$out" | grep -E '^wall' | tr -s ' ' | sed 's/^/  /'
check "every build still finishes" "$(echo "$out" | grep -c '^failed  : 0')" 1
check "outputs still identical" "$(echo "$out" | grep -c 'outputs identical')" 1
check "the peer is not blamed for it" \
  "$(echo "$out" | grep -c 'peer not blamed')" 1

echo
echo "== a build context leaves the client's machine"
# The claim with the most moving parts: files on the client's disk become
# content in the mirror, and a peer with no session and no access to that
# disk builds from them. It had never once run before a fixture existed that
# used `local://`, and every other scenario here reports zero contexts.
ctxbase="${TMPDIR:-/tmp}/rebuck2-check-ctx"
out=$(run CONTEXT=1 NOPROXY=1 DAEMONS=1 RUN="$ctxbase")
check "context baseline succeeds" "$(echo "$out" | grep -c '^failed  : 0')" 1
out=$(run CONTEXT=1 DAEMONS=2 EXPECT="$ctxbase/digests.txt")
echo "$out" | grep -E '^wall|contexts published|placed' | tr -s ' ' | sed 's/^/  /'
check "outputs identical to the context baseline" \
  "$(echo "$out" | grep -c 'outputs identical')" 1
check "every context was published" \
  "$(echo "$out" | grep -o 'contexts published: [0-9]*' | grep -o '[0-9]*$')" \
  "$builds"
check "the peer still took the surplus" "$(placed "$out" 1)" "$((builds - slots))"

echo
echo "== a secret-bearing graph"
# No EXPECT here: the exec itself is the assertion. It runs
# `test "$(cat /run/secrets/probe)" = the-value`, so a build that gets the
# wrong secret - or none - exits non-zero and fails. `failed: 0` with work
# placed away therefore means the peer got the right value, not merely that
# it started.
out=$(run SECRET=1 DAEMONS=2 REBUCK2_SERVE_SECRETS=1)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
check "every build finishes" "$(echo "$out" | grep -c '^failed  : 0')" 1
check "the peer took the surplus" "$(placed "$out" 1)" "$((builds - slots))"

echo
echo "== a secret the PROXY cannot resolve"
# The earthly shape: a secret the client can produce and nothing outside it
# can. Nothing may be offered, or every dispatched solve fails on the peer
# and fail-open rebuilds it at home having paid for the trip.
out=$(run SECRET=1 UNRESOLVABLE=1 DAEMONS=2 REBUCK2_SERVE_SECRETS=1)
echo "$out" | grep -E '^wall|placed|not routed' | tr -s ' ' | sed 's/^/  /'
check "every build still finishes" "$(echo "$out" | grep -c '^failed  : 0')" 1
check "nothing is offered to the peer" "$(placed "$out" 1)" ""
check "and the refusal names the secret" \
  "$(echo "$out" | grep -c 'excluded: Secret')" 1

echo
echo "== a foreign-architecture peer, on a PINNED graph"
# No REBUCK2_LLB_PLATFORM=any here, and that is the whole scenario. An
# unpinned graph is native on every peer, because the base is mirrored for
# whichever architecture the peer runs - so testing emulation with one asks
# nothing. The first version of this check did exactly that, and passed while
# the emulated peer took a third of the work.
out=$(env -u REMOTE -u MIRROR_HOST REBUCK2_LLB_WORK="$work" \
  REBUCK2_HOME_SLOTS="$slots" \
  BUILDS="$builds" DAEMONS=2 FOREIGN=linux/amd64 "$fleet" 2>&1)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
check "every build still finishes" "$(echo "$out" | grep -c '^failed  : 0')" 1
check "the emulated peer is given nothing" "$(placed "$out" 1)" ""

if [ -n "${REMOTE:-}" ]; then
  echo
  echo "== a peer on ANOTHER MACHINE ($REMOTE)"
  # Skipped unless REMOTE is set, because a suite that needs a second machine
  # to pass is a suite nobody runs. The claims are the same ones; what is
  # being checked is that they survive a real network and a foreign
  # architecture, where the base image has to be mirrored per target.
  out=$(env REBUCK2_LLB_WORK="$work" REBUCK2_LLB_PLATFORM=any \
    REBUCK2_HOME_SLOTS="$slots" BUILDS="$builds" DAEMONS=1 \
    REMOTE="$REMOTE" MIRROR_HOST="${MIRROR_HOST:?set MIRROR_HOST to an address both machines reach}" \
    EXPECT="$base/digests.txt" "$fleet" 2>&1)
  echo "$out" | grep -E '^wall|placed|proxy\] peer [0-9]' | tr -s ' ' | sed 's/^/  /'
  check "every build finishes" "$(echo "$out" | grep -c '^failed  : 0')" 1
  check "outputs identical across the network" \
    "$(echo "$out" | grep -c 'outputs identical')" 1
  check "the remote peer took the surplus" "$(placed "$out" 1)" "$((builds - slots))"
fi

echo
printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
