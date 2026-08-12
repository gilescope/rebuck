#!/usr/bin/env bash
# Pull a fleet run's logs and print the lines that decide something.
#
# Written while a run was in flight, because the last four analyses were
# ad-hoc greps and two of them read a counter as the wrong quantity. The
# lines below are chosen: each one answers a question that is currently open.
set -uo pipefail
run=${1:?usage: read-run.sh <run-id> [dir]}
dir=${2:-run-$run}
rm -rf "$dir" && mkdir -p "$dir"
gh api "repos/gilescope/rebuck/actions/runs/$run/logs" > "$dir.zip" 2>/dev/null || {
  echo "no logs for $run (still running?)"; exit 1; }
unzip -oq "$dir.zip" -d "$dir"

# ARTIFACTS TOO. The header below used to say "nothing here reads
# artifacts", and that was a real hole: `proxy.log` and every `worker-N.log`
# are uploaded rather than printed, and they hold the decline reasons, the
# per-worker fetch summaries and the prefetch MISS lines. Reading a run
# without them found `MISS: 0` and 733 silent failures in the same log.
#
# Best effort - a run whose artifacts have expired, or one still going, is
# still worth reading for its step logs.
gh run download "$run" -R gilescope/rebuck -D "$dir/artifacts" 2>/dev/null \
  && echo "(artifacts: $(find "$dir/artifacts" -type f | wc -l | tr -d ' ') file(s))" \
  || echo "(no artifacts - expired, or the run is still going)"

strip() { sed 's/^[^ ]* //'; }

# STEP LOGS AND ARTIFACTS. The coordinator uploads proxy.log as an artifact and
# twenty-one [wire] lines are printed into it that the summary step never
# greps - arrivals, contexts published, op duplication, peer solo ms and the
# rest. Those are now downloaded above and every grep below sees them, which
# it did not for the first several analyses.
#
# Said here rather than discovered again: I spent part of a session assuming
# a line had not printed when it had, in a file I was not reading - and then
# a second session finding `MISS: 0` in step logs while the artifact held
# 733 failures.

echo "── verdict ─────────────────────────────────"
grep -rah "wire\] verdict\|^amplification\|^baseline:\|^one machine\|^PARITY" "$dir" | strip | sort -u

echo; echo "── did work stay home ──────────────────────"
grep -rah "wire\] home vertices\|wire\] service ms\|wire\] not routed" "$dir" | strip | sort -u

echo; echo "── repetition: distinct vs served ──────────"
grep -rah "blobs over 1MiB" "$dir" | strip | sort | uniq -c
grep -rah "went to a client on this box" "$dir" | strip | sort | uniq -c

echo; echo "── prefetch ────────────────────────────────"
grep -rah "prefetch: \|prefetched .* of my share\|no manifest URL for" "$dir" \
  | strip | sed -E 's,[^ ]*sha256:[0-9a-f]+,<img>,g' | sort | uniq -c | sort -rn | head -8

echo; echo "── did prefetch broadcast, or split? ────────"
# N == M on every worker means broadcast; N < M means the one-blob-one-worker
# split. The mech counter cannot answer this: prefetch_broadcast is
# incremented in the WORKER's process and mech::APPLIED is per-process, so
# the coordinator's report never sees it. The evidence is here instead.
grep -rah "prefetched [0-9]*/[0-9]* of my share" "$dir" | strip \
  | sed -E 's/.*prefetched ([0-9]+)\/([0-9]+) of my share \(([0-9]+) announced\)/\1 of \3/' \
  | sort | uniq -c | sort -rn | head -6

echo; echo "── fetches that found nothing anywhere ─────"
# The end of the whole lookup chain: local store, peers by bloom, the
# driver, then upstream. Each one of these is a 404 the caller sees, and a
# caller that was resolving an image fails its build on it - which arrives
# as `worker declined ... could not fetch content descriptor`, 274 times in
# one run, with nothing on our side saying which digest or who was asked.
# TWO strings, in two files, for two paths. The registry's MISS is the end
# of the HTTP lookup chain; the worker's is the end of the MESH one, which
# prefetch takes directly and never enters the handler. Run 31557310760
# printed `MISS: 0` in every log while 733 leads failed on content a
# prefetch had silently not fetched - one diagnostic covering one path, and
# the mechanism living in the gap between them.
grep -rah "\[registry\] MISS\|\[worker\] prefetch MISS" "$dir" | strip \
  | sed -E 's/sha256:[0-9a-f]{8}[0-9a-f]*/<digest>/g; s/[0-9a-f]{40,}/<digest>/g' \
  | sort | uniq -c | sort -rn | head -10
echo "  (none is the good answer)"

echo; echo "── why a lead was declined ─────────────────"
# A decline is not always a refusal. `consider()` gives three reasons -
# saturated, undispatchable, wrong platform - and anything else here is the
# worker having TAKEN the lead and failed to build it, which is a different
# problem with a different fix.
grep -rah "declined subtree" "$dir" | strip \
  | sed -E 's/.*declined subtree job [0-9]+: //; s/sha256:[0-9a-f]+/<digest>/g' \
  | cut -c1-100 | sort | uniq -c | sort -rn | head -6

echo; echo "── seeding ─────────────────────────────────"
# THE ONLY HONEST READ, and it needs its own block rather than a line inside
# `mechanisms`. A seeded mount reports 0.0 MiB in the harvest table - its
# size is the copy-on-write diff over the seed, not the total - so "how big
# is the cache" says the same thing after a perfect seed as after a total
# failure. The arms comparison is what distinguishes them.
grep -rah "\[harvest\]" "$dir" | strip \
  | sed -E 's,[^ ]*sha256:[0-9a-f]{8}[0-9a-f]*,<ref>,g' | sort -u | head -12
arms=$(grep -rah "wire\] mount arms" "$dir" | strip | sort -u | tail -1)
echo "${arms:-  (no mount arms line - the run printed none)}"
# n=0 on the seeded arm has been the outcome of EVERY run so far, for four
# different reasons. Saying so here stops the next reader concluding that
# seeding was measured and did not pay.
case "$arms" in
  *"seeded p50 0ms (n=0)"*)
    echo "  ^ SEEDED ARM EMPTY: nothing was seeded. This is not a result about"
    echo "    seeding - it is the mechanism not firing. Check the harvest lines"
    echo "    above and REBUCK2_CACHE_SEEDS in the fleet step." ;;
esac

echo; echo "── affinity and mechanisms ─────────────────"
grep -rah "wire\] mechanisms\|op duplication\|mount arms" "$dir" | strip | sort -u

echo; echo "── placement spread ────────────────────────"
# The metric -balance exists to move. Three runs with six workers available
# placed 223/125/63/3, 46/37/10 and 153/142/81/3 - two or three machines
# never took a lead, and 65% of lead time was leads queued behind others on
# the machine affinity kept choosing.
grep -rah -- "-> worker" "$dir" | strip | grep -vE "grep|echo" | sort -u \
  | awk '{n++; c[n]=$1; t+=$1}
         END {if(!n){print "  none"; exit}
              printf "  %d worker(s) took work, %d placements:", n, t
              for(i=1;i<=n;i++) printf " %d", c[i]; print ""
              if (n < 6) printf "  %d of 6 took NOTHING\n", 6-n}'

echo; echo "── leads ───────────────────────────────────"
# macOS awk has no 3-argument match(), so the fields are cut with sed first
# rather than parsed in awk - the portable half of a two-line script beats a
# gawk dependency nobody has on this laptop.
grep -rah "KiB fetched" "$dir" | strip | sort -u \
  | sed -E 's/.*took ([0-9]+)ms \(([0-9]+) ops, ([0-9]+) KiB.*/\1 \2 \3/' \
  | awk '{n++; ms+=$1; ops+=$2; kib+=$3}
         END {if(!n){print "  none"; exit}
              printf "  %d leads, %.0f MiB served to their daemons, %.0fs of lead time, %.0f ops\n",
                     n, kib/1024, ms/1000, ops}'
