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
# Files BOTH jobs run, so a flag read only from here is unresolved rather
# than fine. Silence was not neutral: REBUCK2_COMPRESSION sat in the
# coordinator job while `solve::build_subtree` - called from worker.rs:919 -
# exported every dispatched result from the WORKER, and this script passed
# it because `solve` is in neither set. Every dispatched export in every run
# used buildkit's default codec, and two arms measured the coordinator only.
#
# Adding `solve` to WORKER_SIDE would be wrong: the coordinator genuinely
# reads it too (publish_context, mirror_image). The defect is that a
# file-level map cannot see that ONE FUNCTION in a shared file runs on one
# side, so the honest repair is to say so rather than to guess.
SHARED = {"solve", "dispatch", "mesh", "store", "registry"}

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

# Second check, from the other side: a flag DOCUMENTED for an operator that
# nothing implements. REBUCK2_ADAPT sat in running-a-fleet.md's flag table
# and in no .rs file - an operator setting it got silence and no effect,
# which is the code-side defect this repo keeps finding, arriving from the
# documentation instead. A doc mention counts as implemented if the binary
# reads it OR the workflow's own shell expands it.
# A WORD BOUNDARY, not a list of terminators. The first version accepted
# `$X:`, `$X}` and `$X ` and so missed `--pairs "$REBUCK2_SEED_IDS"`, where
# the expansion is closed by a quote - which made a real, used flag look
# like a phantom the moment a doc mentioned it by name. The check then
# argues for deleting the mention, which is the wrong repair twice over.
shell_used = set(re.findall(r"\$\{?(REBUCK2_[A-Z_]+)\b", WF.read_text()))
phantom = []
for d in sorted((ROOT / "docs").glob("*.md")):
    text = d.read_text()
    for v in sorted(set(re.findall(r"REBUCK2_[A-Z_]+", text))):
        if v == "REBUCK2" or v in reads or v in shell_used:
            continue
        # A doc may discuss a flag it is telling you NOT to use, so a
        # sentence saying so is an acceptable answer - but PER FLAG, near
        # the mention. The first version tested the whole file, so one
        # documented removal exempted every phantom beside it, and a planted
        # REBUCK2_PHANTOM sailed through. The proof-it-can-fail step caught
        # that; the commit message written before running it did not.
        near = " ".join(ln for ln in text.split("\n") if v in ln)
        if "does not exist" in near or "gone from" in near:
            continue
        phantom.append(f"{d.name}: {v} is documented and implemented nowhere")
for line in phantom:
    print(line, file=sys.stderr)

# A check that finds nothing must prove it looked (shape 12). These are the
# ones this script CANNOT rule on, and they are printed every run - not only
# when something is wrong - because a line that appears only on failure
# cannot evidence an absence.
unresolved = [
    (v, sorted(reads.get(v, set())))
    for v, jobs in sorted(owner.items())
    if jobs == {"coordinator"}
    and not (reads.get(v, set()) & WORKER_SIDE)
    and (reads.get(v, set()) & SHARED)
]
for v, r in unresolved:
    print(
        f"{v}: coordinator-only, read from shared file(s) {r} - "
        f"check which JOB runs the function that reads it",
        file=sys.stderr,
    )

print(
    f"{len(owner)} flag(s) in the workflow, {len(bad)} in the wrong job, "
    f"{len(phantom)} documented but unimplemented, "
    f"{len(unresolved)} unresolved (shared reader)"
)
# `unresolved` does not fail the run: it is a prompt to think, and a check
# that goes red on every shared-file flag would be turned off within a week.
sys.exit(1 if (bad or phantom) else 0)
