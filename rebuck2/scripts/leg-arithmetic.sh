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

verdict=""; leg=""; base=""; workers=6; building=""
while [ $# -gt 0 ]; do
  case "$1" in
    --verdict)  verdict=$2; shift 2 ;;
    --leg)      leg=$2; shift 2 ;;
    --baseline) base=$2; shift 2 ;;
    --workers)  workers=$2; shift 2 ;;
    --building) building=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

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
