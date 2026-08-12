#!/usr/bin/env bash
# Derive the honest figures from a `[wire] verdict` line.
#
# This exists because I got the division wrong under time pressure and put a
# 24.5x amplification at the top of docs/fleet-findings.md for weeks. See
# shape 22 in docs/how-this-lies.md: `lead_ms` and its phase shares are
# summed over leads that RUN AT THE SAME TIME, so dividing them by a wall
# clock is not a ratio of anything.
#
# The conversion is one line and it is always the same line, so it belongs in
# a script rather than in whoever is reading the log at the time.
#
#   scripts/leg-arithmetic.sh --verdict '<the [wire] verdict line>' \
#     --leg 1113 --baseline 201 --workers 6 [--building 4917]
#
# `--building` is optional: without it only the lead-time figures are given.
# Every output states its units, because "4917s" meant three different
# quantities in one evening.
set -euo pipefail

verdict=""; leg=""; base=""; workers=6; building=""; basecpu=""; wcpu=""
while [ $# -gt 0 ]; do
  case "$1" in
    --verdict)      verdict=$2; shift 2 ;;
    --leg)          leg=$2; shift 2 ;;
    --baseline)     base=$2; shift 2 ;;
    --workers)      workers=$2; shift 2 ;;
    --building)     building=$2; shift 2 ;;
    --baseline-cpu) basecpu=$2; shift 2 ;;
    --worker-cpu)   wcpu=$2; shift 2 ;;   # comma-separated, one per worker
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# CPU FIRST when it is available, because it is the only like-for-like
# comparison here: wall clock on the fleet side is a critical path and on
# the baseline side is one machine's whole run, and dividing them has
# produced three wrong headline figures in this project.
#
# The amplification is also the quantity the ancestry hypothesis makes a
# prediction about - it should track the WORKER COUNT - so this prints the
# ratio against the count rather than leaving it to be eyeballed.
if [ -n "$basecpu" ] && [ -n "$wcpu" ]; then
  echo "$wcpu" | tr ',' '\n' | awk -v b="$basecpu" -v w="$workers" '
    { n++; t += $1; if (mn=="" || $1 < mn) mn=$1; if ($1 > mx) mx=$1 }
    END {
      printf "worker CPU     : %ds across %d worker(s), spread %d-%d = %.1fx\n", t, n, mn, mx, mx/(mn?mn:1)
      printf "baseline CPU   : %ds\n", b
      printf "AMPLIFICATION  : %.1fx  (CPU against CPU - the only like-for-like one)\n", t/b
      printf "per worker     : %.1fx\n", (t/b)/w
      printf "\nancestry test  : if materialising ancestry dominates, AMPLIFICATION\n"
      printf "                 tracks the worker count, so per-worker stays near 1.0.\n"
      printf "                 This run: %.2f\n\n", (t/b)/w
    }'
fi

field() { printf '%s' "$verdict" | grep -oE "$1=[0-9.]+" | head -1 | cut -d= -f2; }

lead_ms=$(field lead_ms)
occ=$(field occupancy)
[ -n "${lead_ms:-}" ] && [ -n "${occ:-}" ] || {
  echo "could not read lead_ms and occupancy out of --verdict" >&2; exit 1; }
[ -n "$leg" ] && [ -n "$base" ] || { echo "--leg and --baseline are required" >&2; exit 1; }

awk -v lead_ms="$lead_ms" -v occ="$occ" -v leg="$leg" -v base="$base" \
    -v w="$workers" -v building="${building:-}" '
BEGIN {
  lead = lead_ms / 1000
  printf "lead time      : %.0fs, summed over leads THAT OVERLAP\n", lead
  printf "occupancy      : %.2f leads in flight fleet-wide (%.2f per worker)\n", occ, occ / w

  # The identity this file trusts. If it does not hold, one of the inputs is
  # from a different run and nothing below means anything.
  pred = lead / occ
  printf "identity check : lead/occupancy = %.0fs against a measured %.0fs leg", pred, leg
  if (leg > 0 && (pred - leg) / leg < 0.05 && (leg - pred) / leg < 0.05)
    printf "  OK\n"
  else
    printf "  MISMATCH - do not trust anything below\n"

  cap = w * leg
  if (building != "") {
    b = building + 0
    bw = b / (occ / w)          # lead-seconds -> worker-wall-seconds
    printf "\nbuilding       : %.0fs of lead time = %.0fs of worker wall time\n", b, bw
    printf "utilisation    : %.0f%% of %d workers x %.0fs\n", 100 * bw / cap, w, leg
    printf "amplification  : %.1fx worker-wall against a %.0fs baseline WALL\n", bw / base, base
    printf "                 (%.1fx is the WRONG figure - lead-seconds over wall)\n", b / base
    printf "\nstill not like-for-like: the baseline wall covers work buildkit\n"
    printf "parallelises across its cores. Use `baseline cpu` and `worker cpu`\n"
    printf "for a comparison that is one.\n"
  }
}'
