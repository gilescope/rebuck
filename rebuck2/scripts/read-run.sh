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
strip() { sed 's/^[^ ]* //'; }

# STEP LOGS ONLY. The coordinator also uploads proxy.log as an artifact, and
# twenty-one [wire] lines are printed into it that the summary step never
# greps - arrivals, contexts published, op duplication, peer solo ms and the
# rest. They are not lost, they are in the artifact, and nothing here reads
# artifacts. If a line you expect is missing, that is where it is:
#
#   gh run download <run-id> -n <artifact> && grep '\[wire\]' proxy.log
#
# Said here rather than discovered again: I spent part of a session assuming
# a line had not printed when it had, in a file I was not reading.

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
