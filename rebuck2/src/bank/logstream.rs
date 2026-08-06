//! Reading earthly's own log stream, so nothing has to be forked to
//! learn how long a target takes.
//!
//! `earthly --logstream-debug-file=X` writes protojson deltas, one per
//! line, and `TargetManifest` already carries everything the timing
//! store's key needs: `canonicalName`, `overrideArgs` (already in the
//! `k=v` form [`super::timings::Key`] takes), start and end stamps, and
//! `dependsOn`. The plan expected to record per-VERTEX `Started` /
//! `Completed` from buildkit's status stream; per-target is both
//! coarser and closer to what principle 13 asks for, and it needs no
//! change to earthbuild or buildkit at all.
//!
//! Fields arrive as DELTAS - `endedAtUnixNanos` lands in a later line
//! than `startedAtUnixNanos` - so a target is only whole once the file
//! has been read to the end.
//!
//! # Spans NEST, and that is the trap
//!
//! A target's span is wall-clock from start to end, which INCLUDES
//! waiting on its dependencies. Measured on a three-target build:
//! `+test` 2995ms contains `+build` 2945ms contains `+deps` 469ms.
//! Summing spans triple-counts the same seconds, and a bin-packer fed
//! those numbers is confidently wrong. What a scheduler wants is SELF
//! time: the span minus what its dependencies were occupying.

use std::collections::BTreeMap;

use anyhow::Result;

use super::timings::{Key, Sample};

/// One target, assembled from every delta that mentioned it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub args: Vec<String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub deps: Vec<String>,
    pub success: bool,
}

impl Target {
    /// Wall-clock, dependencies included.
    pub fn span_ms(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns) / 1_000_000
    }

    /// Did this target run to completion, forwards in time?
    ///
    /// An end before the start is a clock that moved, not a negative
    /// duration - and a wrapping subtraction would put an
    /// 18-exasecond target at the head of every schedule.
    fn usable(&self) -> bool {
        self.success && self.start_ns > 0 && self.end_ns > self.start_ns
    }

    pub fn key(&self) -> Key {
        Key::new(&self.name, self.args.iter().map(String::as_str))
    }
}

/// A protojson uint64 is a STRING, not a number - 2^53 is where a JSON
/// number stops being able to hold a nanosecond stamp, and these are
/// nearly 2^61.
fn nanos(v: Option<&serde_json::Value>) -> u64 {
    v.and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn strings(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Merge every delta in a `--logstream-debug-file` into whole targets.
///
/// A half-written last line is normal input, not corruption: the file
/// is written by a build that may be killed. Unparseable lines are
/// skipped rather than fatal.
pub fn parse(text: &str) -> Vec<Target> {
    let mut by_id: BTreeMap<String, Target> = BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(targets) = v
            .get("deltaManifest")
            .and_then(|d| d.get("fields"))
            .and_then(|f| f.get("targets"))
            .and_then(|t| t.as_object())
        else {
            continue;
        };
        for (id, t) in targets {
            let e = by_id.entry(id.clone()).or_default();
            e.id.clone_from(id);
            // Every field is a delta: absent means "unchanged", never
            // "cleared", so only ever overwrite with something present.
            for field in ["canonicalName", "name"] {
                if let Some(n) = t.get(field).and_then(|n| n.as_str()) {
                    e.name = n.to_owned();
                    break;
                }
            }
            if t.get("overrideArgs").is_some() {
                e.args = strings(t.get("overrideArgs"));
            }
            if t.get("dependsOn").is_some() {
                e.deps = strings(t.get("dependsOn"));
            }
            if let s @ 1.. = nanos(t.get("startedAtUnixNanos")) {
                e.start_ns = s;
            }
            if let s @ 1.. = nanos(t.get("endedAtUnixNanos")) {
                e.end_ns = s;
            }
            if let Some(st) = t.get("status").and_then(|s| s.as_str()) {
                e.success = st == "RUN_STATUS_SUCCESS";
            }
        }
    }
    by_id.into_values().filter(|t| !t.name.is_empty()).collect()
}

/// Each target's own cost: its span, less the parts of it its
/// dependencies were occupying.
///
/// Clipped to the parent's own span, because a dependency shared with
/// another target may have started before this one did. Saturating,
/// because a dependency reported as longer than its parent is a clock
/// artefact and a self time of zero is the honest answer.
pub fn self_ms(targets: &[Target]) -> BTreeMap<String, u64> {
    let by_id: BTreeMap<&str, &Target> = targets.iter().map(|t| (t.id.as_str(), t)).collect();
    targets
        .iter()
        .map(|t| {
            let covered: u64 = t
                .deps
                .iter()
                .filter_map(|d| by_id.get(d.as_str()))
                .filter(|d| d.usable())
                .map(|d| {
                    let lo = d.start_ns.max(t.start_ns);
                    let hi = d.end_ns.min(t.end_ns);
                    hi.saturating_sub(lo)
                })
                .sum();
            (
                t.id.clone(),
                (t.end_ns.saturating_sub(t.start_ns)).saturating_sub(covered) / 1_000_000,
            )
        })
        .collect()
}

/// Turn one build's log stream into samples for `run`.
///
/// Only successes teach: a failed target's duration is how long it took
/// to fail, which is not what it costs to build. No digest is reported,
/// and inventing one would tenure things into the bank on a fiction -
/// so stability stays unknown until something can answer it.
pub fn samples(targets: &[Target], run: u64) -> Vec<(Key, Sample)> {
    let self_ms = self_ms(targets);
    targets
        .iter()
        .filter(|t| t.usable())
        .map(|t| {
            (
                t.key(),
                Sample {
                    run,
                    ms: self_ms.get(&t.id).copied().unwrap_or(0),
                    digest: None,
                },
            )
        })
        .collect()
}

/// Ingest a log stream into a timing table. Returns rows added.
pub fn ingest(table: &std::path::Path, run: u64, log: &std::path::Path) -> Result<usize> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let targets = parse(&text);
    let mut store = super::timings::Store::load(table);
    let mut added = 0;
    for (k, s) in samples(&targets, run) {
        added += usize::from(store.record(k, s));
    }
    store.save(table)?;
    // The widest span is the root target's, i.e. the build's own wall
    // clock - worth printing beside the self times, because the two
    // differing IS the nesting this module exists to undo.
    let wall = targets.iter().map(Target::span_ms).max().unwrap_or(0);
    println!(
        "[timings] ingested {added} targets from {} ({wall}ms wall clock)",
        log.display()
    );
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real output of `earthly --logstream-debug-file`, trimmed to the
    /// lines that carry a target. Captured rather than written: the
    /// field names, the casing and the delta-per-line shape are all
    /// things we would otherwise be guessing at.
    const NESTED: &str = include_str!("../../tests/fixtures/logstream-nested.jsonl");

    fn by_name(ts: &[Target], name: &str) -> Target {
        ts.iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no {name} in {:?}",
                    ts.iter().map(|t| &t.name).collect::<Vec<_>>()
                )
            })
            .clone()
    }

    #[test]
    fn a_target_is_assembled_from_several_deltas() {
        let ts = parse(NESTED);
        assert_eq!(ts.len(), 4, "+test +build +deps +base");

        // The end stamp arrives in a LATER line than the start, so a
        // parser that treated one line as one target would record every
        // target as unfinished.
        let build = by_name(&ts, "+build");
        assert!(build.start_ns > 0 && build.end_ns > build.start_ns);
        assert_eq!(build.span_ms(), 2945);
        assert!(build.success);
        assert_eq!(build.deps.len(), 1, "+build depends on +deps");

        assert_eq!(by_name(&ts, "+test").span_ms(), 2995);
        assert_eq!(by_name(&ts, "+deps").span_ms(), 469);

        // The key falls out of the wire format: overrideArgs is already
        // the k=v form the coarse key takes.
        assert_eq!(by_name(&ts, "+deps").key(), Key::new("+deps", []));
    }

    #[test]
    fn self_time_undoes_the_nesting() {
        // The trap this module exists for. +test contains +build
        // contains +deps, so the spans sum to 6409ms of a build that
        // took 2995ms. Charging a scheduler those numbers would have it
        // believe three targets' work where there is one target's.
        let ts = parse(NESTED);
        let spans: u64 = ts.iter().map(Target::span_ms).sum();
        assert!(spans > 6000, "spans really do overlap: {spans}");

        let self_ms = self_ms(&ts);
        let total: u64 = self_ms.values().sum();
        assert!(
            total <= by_name(&ts, "+test").span_ms(),
            "self time must not exceed the wall clock it happened in: {total}"
        );

        // +test only BUILDs +build, so almost none of its span is its
        // own; +build's own work is its span less +deps'.
        let id = |n: &str| by_name(&ts, n).id;
        assert!(self_ms[&id("+test")] < 100, "+test does nothing itself");
        assert_eq!(self_ms[&id("+build")], 2945 - 469);
        assert_eq!(self_ms[&id("+deps")], 469, "a leaf's self time is its span");
    }

    #[test]
    fn samples_carry_self_time_and_no_forged_digest() {
        let ts = parse(NESTED);
        let got = samples(&ts, 7);
        assert_eq!(got.len(), 4);

        let (_, s) = got
            .iter()
            .find(|(k, _)| k == &Key::new("+build", []))
            .expect("+build");
        assert_eq!(s.run, 7);
        assert_eq!(s.ms, 2945 - 469, "the sample is SELF time, not the span");
        // The log stream reports no output digest, and inventing one
        // would tenure things into the bank on a fiction. Stability
        // stays unknown until something can actually answer it.
        assert_eq!(s.digest, None);
    }

    #[test]
    fn a_truncated_or_hostile_stream_yields_nothing_rather_than_nonsense() {
        // The file is written by a build that may be killed mid-write,
        // so a half-line is normal input, not corruption.
        assert!(parse("").is_empty());
        assert!(parse("not json at all\n").is_empty());
        assert!(parse("{\"deltaManifest\":{\"fields\":{}}}\n").is_empty());

        // A target that never ended is still running or was killed;
        // it has no duration and must not be recorded as one.
        let unfinished = r#"{"deltaManifest":{"fields":{"targets":{"a":{"name":"+x","canonicalName":"+x","startedAtUnixNanos":"100"}}}}}"#;
        assert_eq!(parse(unfinished).len(), 1);
        assert!(samples(&parse(unfinished), 1).is_empty());

        // An end BEFORE the start is a clock that moved, not a negative
        // duration. Dropping it costs one estimate; a wrapped u64 would
        // put an 18-exasecond target at the head of every schedule.
        let backwards = r#"{"deltaManifest":{"fields":{"targets":{"a":{"name":"+x","canonicalName":"+x","startedAtUnixNanos":"200","endedAtUnixNanos":"100","status":"RUN_STATUS_SUCCESS"}}}}}"#;
        assert!(samples(&parse(backwards), 1).is_empty());

        // A failed target's duration is the time it took to fail, which
        // is not what it costs to build. Only successes teach.
        let failed = r#"{"deltaManifest":{"fields":{"targets":{"a":{"name":"+x","canonicalName":"+x","startedAtUnixNanos":"100","endedAtUnixNanos":"200","status":"RUN_STATUS_FAILURE"}}}}}"#;
        assert!(samples(&parse(failed), 1).is_empty());
    }
}
