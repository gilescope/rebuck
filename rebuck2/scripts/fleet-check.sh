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
skipped=0
IMAGE_PROBE=${IMAGE:-moby/buildkit:latest}
ok() {
  printf '  \033[32mPASS\033[0m %s\n' "$1"
  pass=$((pass + 1))
}
no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  fail=$((fail + 1))
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1 (want $3, got $2)"; fi; }

# A run that was rate limited proves nothing either way. Skipping is not
# leniency: its builds failed for a reason outside this repo, and calling
# that a product failure sends the next person hunting a regression that is
# not there. It cost me an iteration to learn that.
limited() { echo "$1" | grep -q "^ratelimited: 1"; }

# `grep -c` EXITS 1 when it counts zero, so under `set -e` the first
# genuinely-failing scenario killed the whole suite before it could report
# anything. A suite that cannot survive a failure cannot describe one.
count() { echo "$1" | grep -c "$2" || true; }

# Presence, not count. Counting occurrences of a message makes an assertion
# brittle to unrelated output: adding the idle-fleet diagnosis, which quotes
# the most common rejection reason, turned one "excluded: Secret" line into
# two and failed a test about something else entirely.
present() { if echo "$2" | grep -q "$3"; then ok "$1"; else no "$1 (no match for: $3)"; fi; }
absent() { if echo "$2" | grep -q "$3"; then no "$1 (unexpected: $3)"; else ok "$1"; fi; }

# Wrap the presence checks so a rate-limited run skips rather than fails.
#
# Call as `guard ... && check ... || true`. The trailing `|| true` is not
# decoration: a skipping guard returns 1, and under `set -e` that ends the
# whole suite after the first skip - which is how the first version of this
# printed one line and stopped.
guard() {
  if limited "$2"; then
    printf '  \033[33mSKIP\033[0m %s (Docker Hub rate limited)\n' "$1"
    skipped=$((skipped + 1))
    return 1
  fi
  return 0
}

# NOTE the `|| true` on every `out=$(...)` below. fleet.sh exits with its
# failure COUNT, so capturing a scenario that legitimately fails aborts the
# suite at the assignment under `set -e` - before it can report the failure
# it was written to detect.
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

# Blobs the shared mirror ended up holding. Counted, not sized: `du` rounds
# each 400-byte manifest up to a block, which flatters and then panics.
blobs() { echo "$1" | sed -n 's/^store blobs: \([0-9]*\) .*/\1/p' | head -1; }

echo "== baseline (one daemon, no proxy)"
out=$(run NOPROXY=1 DAEMONS=1 RUN="$base" || true)
echo "$out" | grep -E '^wall' | tr -s ' '
check "baseline builds succeed" "$(count "$out" '^failed  : 0')" 1

echo
echo "== two daemons: saturation places the surplus away"
out=$(run DAEMONS=2 EXPECT="$base/digests.txt" || true)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
present "outputs identical to baseline" "$out" 'outputs identical'
check "home takes exactly its slots" \
  "$(echo "$out" | grep -o '{0: [0-9]*' | grep -o '[0-9]*$')" "$slots"
check "the rest goes to the peer" "$(placed "$out" 1)" "$((builds - slots))"
absent "and a healthy fleet says nothing alarming" "$out" 'no solve completed on a peer'

echo
echo "== a peer destroyed mid-build"
out=$(run DAEMONS=2 KILL_AFTER=2 EXPECT="$base/digests.txt" || true)
echo "$out" | grep -E '^wall' | tr -s ' ' | sed 's/^/  /'
guard "every build still finishes" "$out" && check "every build still finishes" "$(count "$out" '^failed  : 0')" 1 || true
present "outputs still identical" "$out" 'outputs identical'
# The counterpart to the registry scenario below. Here the peer really is
# the one that failed, so it MUST be struck - otherwise `struck: {}` there
# proves nothing, being what an unused counter says too.
guard "and this time the peer IS struck" "$out" &&
  present "and this time the peer IS struck" "$out" 'struck *: {1:' || true

echo
echo "== more than one peer"
# Everything else here runs two daemons, so until now `place` chose between
# home and a single peer and the multi-peer half - rotation, least-loaded,
# strike deprioritisation - was only ever exercised by unit tests.
out=$(run DAEMONS=3 || true)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
guard "the surplus splits across BOTH peers" "$out" && {
  p1=$(placed "$out" 1)
  p2=$(placed "$out" 2)
  # 12 builds, 8 slots: 8 at home and 4 away, 2 each. Asserting the split
  # rather than the total, because one peer taking all 4 is also "4 away"
  # and is the failure this scenario exists to catch.
  check "the surplus splits across BOTH peers" "${p1:-0}/${p2:-0}" "2/2"
} || true

echo
echo "== one peer of two destroyed mid-build"
# The fleet must route around it rather than collapse back to home, and the
# machine that died must be the only one that pays for it.
out=$(run DAEMONS=3 KILL_AFTER=2 REBUCK2_HOME_SLOTS=4 EXPECT="$base/digests.txt" || true)
echo "$out" | grep -E '^wall|placed|struck' | tr -s ' ' | sed 's/^/  /'
guard "every build still finishes" "$out" && check "every build still finishes" "$(count "$out" '^failed  : 0')" 1 || true
check "outputs still identical" "$(count "$out" 'outputs identical')" 1
guard "the surviving peer keeps taking work" "$out" && {
  if [ "$(placed "$out" 1)" -gt 0 ]; then ok "the surviving peer keeps taking work"
  else no "the surviving peer keeps taking work (nothing placed on peer 1)"; fi
} || true
# Names the peer, so a run that struck EVERYONE cannot pass this.
present "and only the dead one is struck" "$out" 'struck *: {2:'
absent "and only the dead one is struck (peer 1 spared)" "$out" 'struck *: {1:'

echo
echo "== a peer that is slow rather than dead"
# Step 5 of the placement algorithm - a bounded wait, past which an adoption
# is withdrawn and built at home - and nothing exercised it end to end. A
# machine that dies is easy; one that merely crawls holds a slot open and is
# the case the hedge exists for.
#
# --cpus 0.15 on the last daemon. The peer is NOT cancelled when withdrawn,
# so both copies race and whoever publishes first wins.
out=$(run DAEMONS=3 SLOW=0.15 ROUNDS=2 REBUCK2_HOME_SLOTS=4 EXPECT="$base/digests.txt" || true)
echo "$out" | grep -E '^wall|placed|struck|not routed' | tr -s ' ' | sed 's/^/  /'
guard "every build still finishes" "$out" && check "every build still finishes" "$(count "$out" '^failed  : 0')" 1 || true
check "outputs still identical" "$(count "$out" 'outputs identical')" 1
# The mechanism, not the symptom: a run where the straggler simply finished
# in time would pass the two checks above having tested nothing.
present "the straggler is withdrawn from" "$out" 'too slow'
present "and it is the SLOW one that is struck" "$out" 'struck *: {2:'

echo
echo "== a named frontend cannot be distributed"
# Documented as structural rather than a gap, and asserted nowhere.
# `--frontend dockerfile.v0` is resolved INSIDE the daemon, so its LLB never
# crosses the proxy and there is nothing to place. The point of the check is
# that this stays a clean no-op: the build must still work.
out=$(run DAEMONS=2 DOCKERFILE=1 || true)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
guard "the build still succeeds" "$out" && check "the build still succeeds" "$(count "$out" '^failed  : 0')" 1 || true
guard "and nothing was placed on a peer" "$out" &&
  check "and nothing was placed on a peer" "$(placed "$out" 1)" "" || true
# An empty `placed` is ALSO what a completely broken fleet produces, so the
# check above would pass for one. The reason has to be the structural one.
present "for the structural reason, not a broken fleet" "$out" \
  'frontend runs in the daemon: dockerfile.v0'

echo
echo "== the registry destroyed mid-build"
out=$(run DAEMONS=2 KILL_REGISTRY_AFTER=2 EXPECT="$base/digests.txt" || true)
echo "$out" | grep -E '^wall' | tr -s ' ' | sed 's/^/  /'
guard "every build still finishes" "$out" && check "every build still finishes" "$(count "$out" '^failed  : 0')" 1 || true
check "outputs still identical" "$(count "$out" 'outputs identical')" 1
# The claim is that a dead MIRROR costs no peer its standing. Asserting the
# message 'peer not blamed' tested one of the two ways that happens and
# flaked at about 1 in 3: if the registry dies before the base is mirrored,
# `make_portable` fails first, the graph is never offered, and no peer
# failure exists to attribute - reported as "base unmirrored" instead. Both
# are correct, so assert the thing they have in common.
present "the peer is not blamed for it" "$out" 'struck *: {}'

echo
echo "== a build context leaves the client's machine"
# The claim with the most moving parts: files on the client's disk become
# content in the mirror, and a peer with no session and no access to that
# disk builds from them. It had never once run before a fixture existed that
# used `local://`, and every other scenario here reports zero contexts.
ctxbase="${TMPDIR:-/tmp}/rebuck2-check-ctx"
out=$(run CONTEXT=1 NOPROXY=1 DAEMONS=1 RUN="$ctxbase" || true)
check "context baseline succeeds" "$(count "$out" '^failed  : 0')" 1
out=$(run CONTEXT=1 DAEMONS=2 EXPECT="$ctxbase/digests.txt" || true)
echo "$out" | grep -E '^wall|contexts published|placed' | tr -s ' ' | sed 's/^/  /'
present "outputs identical to the context baseline" "$out" 'outputs identical'
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
out=$(run SECRET=1 DAEMONS=2 REBUCK2_SERVE_SECRETS=1 || true)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
guard "every build finishes" "$out" && check "every build finishes" "$(count "$out" '^failed  : 0')" 1 || true
check "the peer took the surplus" "$(placed "$out" 1)" "$((builds - slots))"

echo
echo "== a secret the PROXY cannot resolve"
# The earthly shape: a secret the client can produce and nothing outside it
# can. Nothing may be offered, or every dispatched solve fails on the peer
# and fail-open rebuilds it at home having paid for the trip.
out=$(run SECRET=1 UNRESOLVABLE=1 DAEMONS=2 REBUCK2_SERVE_SECRETS=1 || true)
echo "$out" | grep -E '^wall|placed|not routed' | tr -s ' ' | sed 's/^/  /'
guard "every build still finishes" "$out" && check "every build still finishes" "$(count "$out" '^failed  : 0')" 1 || true
check "nothing is offered to the peer" "$(placed "$out" 1)" ""
present "and the refusal names the secret" "$out" 'excluded: Secret'

echo
echo "== a fleet that is silently doing nothing"
# The failure that reads as success: daemons that will not pull from an HTTP
# mirror. Every peer refuses, dispatch falls back home, the BUILD SUCCEEDS.
# The proxy has to say so, because nothing else will.
out=$(run NO_REGISTRY_TRUST=1 DAEMONS=2 || true)
check "the build still succeeds" "$(count "$out" '^failed  : 0')" 1
present "but the proxy says the fleet did nothing" "$out" 'no solve completed on a peer'
present "and names the likely cause" "$out" 'cannot PULL from the mirror'

echo
echo "== repeating a build costs the mirror nothing"
# Buildkit stamps wall clock into the image config and real mtimes into the
# layer, so an unchanged graph used to republish as fresh bytes, move its
# tag, and orphan whatever the tag named before. 8 rounds of 4 builds left
# 75 blobs where 1 round left 19.
#
# HOME_SLOTS=0 on BOTH runs, so every build is adopted every round and the
# two runs see the SAME set of distinct graphs. At the default slot count
# which builds travel depends on saturation timing, so a later round can
# adopt a graph an earlier one built at home - the blob count would then
# differ for a reason that has nothing to do with dedup.
one=$(run DAEMONS=2 ROUNDS=1 REBUCK2_HOME_SLOTS=0 || true)
many=$(run DAEMONS=2 ROUNDS=3 REBUCK2_HOME_SLOTS=0 || true)
b1=$(blobs "$one")
b3=$(blobs "$many")
printf '  1 round: %s blobs · 3 rounds: %s blobs\n' "$b1" "$b3"
guard "three rounds add nothing to the store" "$many" &&
  check "three rounds add nothing to the store" "$b3" "$b1" || true
# Vacuous if nothing was published at all - 0 equals 0.
guard "and there was something in it to begin with" "$one" &&
  { [ "${b1:-0}" -gt 0 ] && ok "and there was something in it to begin with" ||
    no "and there was something in it to begin with (store empty)"; } || true

echo
echo "== a graph with a CACHE MOUNT"
# The peer builds with its own, colder cache mount. The claim is that this
# changes nothing about the output, which is the same contract that makes a
# cache mount safe on one machine - so the digests are the assertion.
cachebase="${TMPDIR:-/tmp}/rebuck2-check-cache"
out=$(run CACHE=1 NOPROXY=1 DAEMONS=1 RUN="$cachebase" || true)
check "cache baseline succeeds" "$(count "$out" '^failed  : 0')" 1
out=$(run CACHE=1 DAEMONS=2 EXPECT="$cachebase/digests.txt" || true)
check "excluded while the flag is off" "$(placed "$out" 1)" ""
out=$(run CACHE=1 DAEMONS=2 REBUCK2_PEER_CACHE_MOUNTS=1 EXPECT="$cachebase/digests.txt" || true)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
check "the peer takes it with the flag on" "$(placed "$out" 1)" "$((builds - slots))"
present "and its colder cache changes nothing" "$out" 'outputs identical'

echo
echo "== a graph with an SSH AGENT mount"
# Needs a real agent on this machine, which CI does not have. Skipped rather
# than failed, and skipped for a stated reason: no agent is a property of the
# machine, not of the code.
if [ -z "${SSH_AUTH_SOCK:-}" ]; then
  printf '  \033[33mSKIP\033[0m no SSH_AUTH_SOCK on this machine\n'
  skipped=$((skipped + 1))
else
  # HOME_SLOTS=0 forces every build away: the client cannot forward its own
  # agent into a borrowed buildctl container on every platform, so home
  # builds are not the thing under test here - dispatch is.
  out=$(run SSHM=1 DAEMONS=2 REBUCK2_HOME_SLOTS=0 || true)
  check "excluded while the flag is off" "$(placed "$out" 1)" ""
  out=$(run SSHM=1 DAEMONS=2 REBUCK2_HOME_SLOTS=0 REBUCK2_FORWARD_AGENT=1 || true)
  echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
  # The exec runs `ssh-add -l; test $? -ne 2`, so a build that reaches no
  # agent fails. Finishing means the peer talked to ours.
  guard "every build finishes" "$out" && check "every build finishes" "$(count "$out" '^failed  : 0')" 1 || true
  check "the peer takes it with the flag on" "$(placed "$out" 1)" "$builds"
fi

echo
echo "== a foreign-architecture peer, on a PINNED graph"
# Needs emulation for the foreign platform, which a bare CI runner does not
# have until someone installs binfmt. Skipped rather than failed: a suite
# that goes red because of the machine it is on teaches people to ignore it.
foreign=${FOREIGN_PLATFORM:-linux/amd64}
native="linux/$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')"
if [ "$foreign" = "$native" ]; then
  foreign=linux/arm64
fi
# `--entrypoint`, or this runs `buildkitd true` and starts a daemon that
# never exits - the probe then hangs instead of answering.
if ! timeout 60 docker run --rm --platform "$foreign" \
  --entrypoint /bin/true "$IMAGE_PROBE" >/dev/null 2>&1; then
  printf '  \033[33mSKIP\033[0m no emulation for %s on this machine\n' "$foreign"
  skipped=$((skipped + 1))
else
# No REBUCK2_LLB_PLATFORM=any here, and that is the whole scenario. An
# unpinned graph is native on every peer, because the base is mirrored for
# whichever architecture the peer runs - so testing emulation with one asks
# nothing. The first version of this check did exactly that, and passed while
# the emulated peer took a third of the work.
out=$(env -u REMOTE -u MIRROR_HOST REBUCK2_LLB_WORK="$work" \
  REBUCK2_HOME_SLOTS="$slots" \
  BUILDS="$builds" DAEMONS=2 FOREIGN="$foreign" "$fleet" 2>&1 || true)
echo "$out" | grep -E '^wall|placed' | tr -s ' ' | sed 's/^/  /'
# An emulated privileged buildkitd is the one scenario here with a real
# environmental dependency: it can fail to START rather than fail an
# assertion. Those are different facts and must not share a verdict - a
# daemon that never came up says nothing about placement. No wire report
# means the scenario did not run.
if ! echo "$out" | grep -q 'placed'; then
  printf '  \033[33mSKIP\033[0m the emulated daemon did not come up\n'
  skipped=$((skipped + 1))
else
  guard "every build still finishes" "$out" && check "every build still finishes" "$(count "$out" '^failed  : 0')" 1 || true
  check "the emulated peer is given nothing" "$(placed "$out" 1)" ""
fi
fi

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
  guard "every build finishes" "$out" && check "every build finishes" "$(count "$out" '^failed  : 0')" 1 || true
  present "outputs identical across the network" "$out" 'outputs identical'
  check "the remote peer took the surplus" "$(placed "$out" 1)" "$((builds - slots))"
fi

echo
printf '\n%s passed, %s failed, %s skipped\n' "$pass" "$fail" "$skipped"
[ "$fail" -eq 0 ]
