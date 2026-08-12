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
    /// Were all of this target's EXEC commands served from cache?
    /// `None` when it ran none - `+base` is a `FROM` and nothing else,
    /// and "no work" is not evidence either way.
    ///
    /// Not "all commands": structural ones (`FROM +base`, `SAVE
    /// ARTIFACT`) report uncached even on an identical rerun, so that
    /// predicate is never true. Measured.
    pub execs_cached: Option<bool>,
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
    // cmdID -> (targetID, is an exec, was cached). Commands arrive in
    // deltas too, and `isCached` may land in a different line from the
    // name that says whether it is an exec at all.
    let mut cmds: BTreeMap<String, (String, bool, bool)> = BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(fields) = v.get("deltaManifest").and_then(|d| d.get("fields")) else {
            continue;
        };
        for (id, c) in fields
            .get("commands")
            .and_then(|c| c.as_object())
            .into_iter()
            .flatten()
        {
            let e = cmds.entry(id.clone()).or_default();
            if let Some(t) = c.get("targetId").and_then(|t| t.as_str()) {
                e.0 = t.to_owned();
            }
            if let Some(n) = c.get("name").and_then(|n| n.as_str()) {
                // An EXEC is what actually does work. `FROM` and `SAVE`
                // are structural and report uncached on an identical
                // rerun, so counting them makes the predicate constant.
                e.1 = n.starts_with("RUN") || n.starts_with("COPY");
            }
            if let Some(c) = c.get("isCached").and_then(serde_json::Value::as_bool) {
                e.2 = c;
            }
        }
        let Some(targets) = fields.get("targets").and_then(|t| t.as_object()) else {
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
    // One uncached exec is enough: a target is unchanged only if all
    // of its work was.
    for (tid, _, cached) in cmds.into_values().filter(|c| c.1) {
        if let Some(t) = by_id.get_mut(&tid) {
            t.execs_cached = Some(t.execs_cached.unwrap_or(true) && cached);
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

/// The identity that answers "has this target moved".
///
/// The log stream carries no output digest, so one is SYNTHESISED: a
/// cached target keeps the digest it had, an uncached one is given a
/// fresh one. `Store::stability` then counts consecutive builds that
/// left a target alone, without knowing the difference.
///
/// The proxy lies only in the SAFE direction, which is the whole
/// argument for using it. Measured on a real build:
///
/// | case | reports | truth | cost |
/// | ---- | ------- | ----- | ---- |
/// | unchanged rerun | cached | unchanged | correct |
/// | source changed | uncached | changed | correct |
/// | unchanged, COLD cache | uncached | unchanged | a lost tenure |
///
/// The last row is the fresh-runner case and it under-tenures: we fail
/// to bank something we could have. The dangerous direction - claiming
/// unchanged when it moved - needs buildkit to report a cache hit on
/// different inputs, which is principle 7's determinism bound and is
/// already accepted everywhere else in this system.
fn stability_digest(
    prev: Option<&str>,
    cached: Option<bool>,
    key: &Key,
    run: u64,
) -> Option<String> {
    match cached? {
        // Left alone: keep the identity it had. A first sighting has
        // none to keep, so it starts a chain rather than joining one.
        true => Some(prev.map_or_else(|| mint(key, run), str::to_owned)),
        // Rebuilt: a fresh identity, keyed on the run so it breaks the
        // chain exactly once and is stable if the ingest is replayed.
        false => Some(mint(key, run)),
    }
}

/// A synthesised identity for one (key, run). Hex, because that is what
/// [`super::timings`] accepts, and short because nothing compares it to
/// anything but itself.
fn mint(key: &Key, run: u64) -> String {
    crate::store::sha256_hex(format!("{}\0{run}", key.as_str()).as_bytes())[..16].to_owned()
}

/// Turn one build's log stream into samples for `run`.
///
/// Only successes teach: a failed target's duration is how long it took
/// to fail, which is not what it costs to build. No digest is reported,
/// and inventing one would tenure things into the bank on a fiction -
/// so stability stays unknown until something can answer it.
pub fn samples(targets: &[Target], run: u64, prev: &super::timings::Store) -> Vec<(Key, Sample)> {
    let self_ms = self_ms(targets);
    targets
        .iter()
        .filter(|t| t.usable())
        .map(|t| {
            let key = t.key();
            let digest = stability_digest(prev.newest_digest(&key), t.execs_cached, &key, run);
            (
                key,
                Sample {
                    run,
                    ms: self_ms.get(&t.id).copied().unwrap_or(0),
                    digest,
                },
            )
        })
        .collect()
}

/// The longest chain of dependent work, and how long it takes.
///
/// The plan says to compute this ONCE rather than continuously, because
/// it answers a question nothing else does: **the N at which adding
/// workers stops paying**. Below that N the fleet is work-bound and
/// batch efficiency dominates; above it, runners are being bought to
/// wait on a chain.
///
/// Weighted by SELF time, so a parent that only waits on its child
/// contributes nothing of its own - otherwise the nesting would be
/// counted twice here as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriticalPath {
    /// Target names, root-most last.
    pub path: Vec<String>,
    pub ms: u64,
    /// Every target's self time added up: the work a fleet must do.
    pub total_ms: u64,
}

impl CriticalPath {
    /// Runners beyond which the chain, not the work, sets makespan.
    ///
    /// `total / path`, rounded up: with fewer runners than this the
    /// fleet is work-bound; with more, the extra ones wait.
    pub fn saturation_n(&self) -> u64 {
        if self.ms == 0 {
            return 0;
        }
        self.total_ms.div_ceil(self.ms)
    }
}

pub fn critical_path(targets: &[Target]) -> CriticalPath {
    let self_ms = self_ms(targets);
    let by_id: BTreeMap<&str, &Target> = targets.iter().map(|t| (t.id.as_str(), t)).collect();
    let cost = |id: &str| self_ms.get(id).copied().unwrap_or(0);

    // Longest path by memoised descent. A build graph is acyclic, but
    // the input is a file that anything could have written, so `on_stack`
    // makes a cycle terminate at zero rather than recurse forever.
    fn longest<'a>(
        id: &'a str,
        by_id: &BTreeMap<&'a str, &'a Target>,
        cost: &dyn Fn(&str) -> u64,
        memo: &mut BTreeMap<&'a str, (u64, Vec<String>)>,
        on_stack: &mut std::collections::BTreeSet<&'a str>,
    ) -> (u64, Vec<String>) {
        if let Some(hit) = memo.get(id) {
            return hit.clone();
        }
        // A dependency naming a target this stream never described - it
        // may have come from another earth process - contributes
        // nothing rather than losing the path that reaches it.
        let Some(t) = by_id.get(id) else {
            return (0, Vec::new());
        };
        if !on_stack.insert(id) {
            return (0, Vec::new());
        }
        // `None` until a dependency RESOLVES, because a zero-cost one
        // is still on the chain - `+base` costs nothing and dropping it
        // leaves the path looking disconnected from its own root.
        let mut best: Option<(u64, Vec<String>)> = None;
        let mut deps: Vec<&String> = t.deps.iter().collect();
        deps.sort();
        for d in deps {
            let got = longest(d.as_str(), by_id, cost, memo, on_stack);
            if got.1.is_empty() {
                continue;
            }
            // Longest wins; ties break on the names, so two machines
            // computing this from the same graph name the same path.
            best = match best {
                Some(b) if (b.0, &b.1) >= (got.0, &got.1) => Some(b),
                _ => Some(got),
            };
        }
        on_stack.remove(id);
        let (base_ms, mut path) = best.unwrap_or((0, Vec::new()));
        path.push(t.name.clone());
        let out = (base_ms + cost(id), path);
        memo.insert(id, out.clone());
        out
    }

    let mut memo = BTreeMap::new();
    let mut best = (0, Vec::new());
    for t in targets {
        let got = longest(
            t.id.as_str(),
            &by_id,
            &cost,
            &mut memo,
            &mut Default::default(),
        );
        if got.0 > best.0 {
            best = got;
        }
    }
    CriticalPath {
        path: best.1,
        ms: best.0,
        total_ms: self_ms.values().sum(),
    }
}

/// Ingest a log stream into a timing table. Returns rows added.
pub fn ingest(table: &std::path::Path, run: u64, log: &std::path::Path) -> Result<usize> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let targets = parse(&text);
    let mut store = super::timings::Store::load(table);
    let mut added = 0;
    for (k, s) in samples(&targets, run, &store) {
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
    use super::super::timings::Store;
    use super::*;

    /// Real output of `earthly --logstream-debug-file`, trimmed to the
    /// lines that carry a target. Captured rather than written: the
    /// field names, the casing and the delta-per-line shape are all
    /// things we would otherwise be guessing at.
    const NESTED: &str = include_str!("../../tests/fixtures/logstream-nested.jsonl");

    /// The SAME build, rerun with nothing changed: every exec served
    /// from cache.
    const CACHED: &str = include_str!("../../tests/fixtures/logstream-cached.jsonl");

    /// The same build after editing what a RUN does: no exec cached.
    const CHANGED: &str = include_str!("../../tests/fixtures/logstream-changed.jsonl");

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
        let got = samples(&ts, 7, &super::super::timings::Store::new());
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
    fn cachedness_is_read_from_execs_not_from_every_command() {
        // Structural commands report uncached even on an identical
        // rerun - measured: FROM +base and SAVE ARTIFACT are false
        // while the RUN beside them is true. "All commands cached" is
        // therefore never true, and a predicate that is never true is
        // not a signal.
        let cached = parse(CACHED);
        for name in ["+build", "+deps"] {
            assert_eq!(
                by_name(&cached, name).execs_cached,
                Some(true),
                "{name} was rerun unchanged"
            );
        }
        for name in ["+build", "+deps"] {
            assert_eq!(
                by_name(&parse(CHANGED), name).execs_cached,
                Some(false),
                "{name}'s work was edited"
            );
        }
        // A target that runs no execs is not evidence either way.
        assert_eq!(by_name(&cached, "+base").execs_cached, None);
    }

    #[test]
    fn a_left_alone_target_keeps_its_identity_and_tenures() {
        // The whole point: with no output digest in the stream, one is
        // synthesised so Store::stability can count consecutive builds
        // that left a target alone.
        let mut store = Store::new();
        let deps = Key::new("+deps", []);

        // Run 1 is the first sighting - it starts a chain.
        for (k, s) in samples(&parse(CHANGED), 1, &store) {
            store.record(k, s);
        }
        let first = store.newest_digest(&deps).map(str::to_owned);
        assert!(first.is_some(), "a first sighting must start a chain");
        assert_eq!(store.stability(&deps), 1);

        // Runs 2 and 3 change nothing, so the identity carries and the
        // target tenures at three generations (principle 14).
        for run in 2..=3 {
            for (k, s) in samples(&parse(CACHED), run, &store) {
                store.record(k, s);
            }
        }
        assert_eq!(store.newest_digest(&deps).map(str::to_owned), first);
        assert_eq!(store.stability(&deps), 3);
        assert!(store.tenured(&deps), "three untouched builds is the stem");

        // An edit breaks the chain, and tenure with it - which is the
        // rule that stops us banking what changes every commit.
        for (k, s) in samples(&parse(CHANGED), 4, &store) {
            store.record(k, s);
        }
        assert_ne!(store.newest_digest(&deps).map(str::to_owned), first);
        assert_eq!(store.stability(&deps), 1);
        assert!(!store.tenured(&deps));

        // The synthesised identity must be stable for one (key, run) -
        // an ingest replayed twice may not look like a change.
        assert_eq!(
            stability_digest(None, Some(false), &deps, 9),
            stability_digest(None, Some(false), &deps, 9)
        );
        // ...and must differ per run, or an uncached rebuild would
        // silently extend the chain it is supposed to break.
        assert_ne!(
            stability_digest(None, Some(false), &deps, 9),
            stability_digest(None, Some(false), &deps, 10)
        );
        // No execs, no claim.
        assert_eq!(stability_digest(Some("abc"), None, &deps, 1), None);
    }

    #[test]
    fn the_critical_path_says_when_more_runners_stop_paying() {
        // The real build is a pure chain: +test -> +build -> +deps ->
        // +base. Every millisecond of it is on the critical path, so no
        // number of runners helps and saturation is ONE. A fleet bought
        // for this shape would sit idle, which is exactly the answer
        // this computation exists to give.
        let cp = critical_path(&parse(NESTED));
        assert_eq!(cp.path, vec!["+base", "+deps", "+build", "+test"]);
        assert_eq!(cp.ms, cp.total_ms, "a chain has no parallel work");
        assert_eq!(cp.saturation_n(), 1);

        // Widen it: two independent 100ms leaves under one root. Now
        // there are 200ms of work on a 100ms chain, so a second runner
        // pays and a third does not.
        // A start of zero means "no stamp arrived", so these need real
        // ones or self_ms treats them as unmeasured and declines to
        // subtract their overlap.
        let t = |id: &str, ms: u64, deps: Vec<&str>| Target {
            id: id.into(),
            name: format!("+{id}"),
            start_ns: 1_000_000,
            end_ns: (1 + ms) * 1_000_000,
            deps: deps.into_iter().map(Into::into).collect(),
            success: true,
            ..Default::default()
        };
        // The root's own span covers both leaves, so its SELF time is
        // what is left after they are subtracted.
        let mut root = t("root", 100, vec!["a", "b"]);
        root.deps = vec!["a".into(), "b".into()];
        let wide = vec![t("a", 100, vec![]), t("b", 100, vec![]), root];
        let cp = critical_path(&wide);
        assert_eq!(cp.total_ms, 200, "two leaves of work");
        assert_eq!(cp.ms, 100, "either leaf is the whole chain");
        assert_eq!(cp.saturation_n(), 2);

        // Degenerate shapes are reached on real builds, not just here.
        assert_eq!(critical_path(&[]).saturation_n(), 0);
        assert_eq!(critical_path(&[]).ms, 0);
        // A dependency naming a target that is not in the stream (a
        // target from another earth process) must not lose the path.
        let orphan = vec![t("x", 50, vec!["gone"])];
        assert_eq!(critical_path(&orphan).ms, 50);
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
        assert!(samples(&parse(unfinished), 1, &Store::new()).is_empty());

        // An end BEFORE the start is a clock that moved, not a negative
        // duration. Dropping it costs one estimate; a wrapped u64 would
        // put an 18-exasecond target at the head of every schedule.
        let backwards = r#"{"deltaManifest":{"fields":{"targets":{"a":{"name":"+x","canonicalName":"+x","startedAtUnixNanos":"200","endedAtUnixNanos":"100","status":"RUN_STATUS_SUCCESS"}}}}}"#;
        assert!(samples(&parse(backwards), 1, &Store::new()).is_empty());

        // A failed target's duration is the time it took to fail, which
        // is not what it costs to build. Only successes teach.
        let failed = r#"{"deltaManifest":{"fields":{"targets":{"a":{"name":"+x","canonicalName":"+x","startedAtUnixNanos":"100","endedAtUnixNanos":"200","status":"RUN_STATUS_FAILURE"}}}}}"#;
        assert!(samples(&parse(failed), 1, &Store::new()).is_empty());
    }
}
