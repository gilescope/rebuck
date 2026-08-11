#!/usr/bin/env python3
"""Is every REBUCK2_ flag set in the job that READS it?

A flag set in the coordinator's `env:` reaches the coordinator only. If the
code that acts on it lives in worker.rs, the branch name says the mechanism
is on and the process that would act on it never sees the variable - and the
counter then reads `never needed`, which looks like "it did not matter"
rather than "it never ran".

That happened once, to REBUCK2_PREFETCH_ALL: my_share calls
prefetch_broadcast(), my_share runs on the worker, and the flag sat beside
REBUCK2_BALANCE in the coordinator job where it correctly belongs for the
driver's flags and not for that one.

Deliberately a script and not a cargo test. The module-to-job mapping below
is a judgement about which process runs which file, and a test that fails
because someone moved a function is a test that gets deleted. Run it when
adding a flag; that is the moment it pays.

    rebuck2/scripts/check-flag-jobs.py
"""
import re
import sys
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
WF = ROOT / ".github/workflows/earthfile-fleet-multi.yml"
SRC = ROOT / "rebuck2/src"
# Files whose code runs in the worker job. `dispatch.rs` is shared by both
# and so proves nothing either way; it is left out rather than guessed at.
WORKER_SIDE = {"worker", "exec"}

owner: dict[str, set] = {}
job = None
for line in WF.read_text().split("\n"):
    m = re.match(r"^  ([a-z][a-z0-9_-]*):\s*$", line)
    if m:
        job = m.group(1)
    for v in re.findall(r"REBUCK2_[A-Z_]+", line):
        owner.setdefault(v, set()).add(job)

reads: dict[str, set] = {}
for f in SRC.glob("*.rs"):
    for v in set(re.findall(r"REBUCK2_[A-Z_]+", f.read_text())):
        reads.setdefault(v, set()).add(f.stem)

bad = [
    (v, sorted(reads.get(v, set())))
    for v, jobs in sorted(owner.items())
    if jobs == {"coordinator"} and (reads.get(v, set()) & WORKER_SIDE)
]
for v, r in bad:
    print(f"{v}: set in the coordinator job only, read by {r}", file=sys.stderr)
print(f"{len(owner)} flag(s) in the workflow, {len(bad)} in the wrong job")
sys.exit(1 if bad else 0)
