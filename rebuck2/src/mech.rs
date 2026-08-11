//! Did the mechanism you switched on actually do anything?
//!
//! Three mechanisms on this branch were built, switched on, measured, and
//! found to have no effect - because none of them was connected to a
//! decision:
//!
//! * `MIN_SIBLINGS` gated a diagnostic counter instead of the dispatch test,
//!   so the flag changed a tally and nothing else.
//! * the seed split computed each worker's share from its own stale gossip,
//!   so the shares neither partitioned nor became visible in time.
//! * `PREFETCH` had a consumer gate with no caller: base images and prefix
//!   cuts were announced, subtree results - where the 65 MiB layers are -
//!   reached no firing site at all.
//!
//! Each would have been recorded as "the idea does not work". The defence
//! that keeps working is an observable signature stated BEFORE the run, and
//! this is that defence made structural: a mechanism counts the times it
//! changed an outcome, and the report says so. On means nothing; applied is
//! the claim.
//!
//! Deliberately not a metrics framework. It is a map of counters printed at
//! the end, because anything heavier would not have been added.

use std::collections::BTreeMap;
use std::sync::Mutex;

static APPLIED: std::sync::LazyLock<Mutex<BTreeMap<&'static str, u64>>> =
    std::sync::LazyLock::new(Default::default);

/// This mechanism just changed what would otherwise have happened.
///
/// Call it at the point of the DECISION, not where the flag is read - the
/// whole failure mode is a flag that is read and then not acted on.
pub fn applied(name: &'static str) {
    if let Ok(mut m) = APPLIED.lock() {
        *m.entry(name).or_default() += 1;
    }
}

/// What each mechanism actually did, for the end-of-run report.
pub fn report() -> Vec<(&'static str, u64)> {
    APPLIED
        .lock()
        .map(|m| m.iter().map(|(k, v)| (*k, *v)).collect())
        .unwrap_or_default()
}

/// A line naming every mechanism that is ON, and how often it mattered.
///
/// `enabled` is what the environment asked for. A name that is enabled and
/// absent from the counters is the exact bug this module exists for, and it
/// is called out rather than left to be noticed.
pub fn summary(enabled: &[(&'static str, bool)]) -> String {
    let counts: BTreeMap<&str, u64> = report().into_iter().collect();
    let mut out = Vec::new();
    for (listed, on) in enabled {
        if !on {
            continue;
        }
        // A GUARD is spelled with a leading `?` in the enabled list. The
        // marker is for this function, never for the reader.
        let guard = listed.starts_with('?');
        let name = listed.strip_prefix('?').unwrap_or(listed);
        match counts.get(name) {
            Some(n) => out.push(format!("{name}={n}")),
            // ZERO is not always a bug. `read_retry` and
            // `verdict_stops_retry` are guards: the good day is the one
            // where neither fires, and shouting ON BUT NEVER APPLIED at them
            // teaches a reader to skim past the phrase - which is fatal,
            // because the phrase exists for `cut_prefix` sitting silently
            // disconnected, and that reading has been needed once already.
            None if guard => out.push(format!("{name}=0 (never needed)")),
            None => out.push(format!("{name}=ON BUT NEVER APPLIED")),
        }
    }
    if out.is_empty() {
        "none enabled".to_owned()
    } else {
        out.join(" ")
    }
}

#[cfg(test)]
mod summary_shape {
    /// Zero is not always a bug, and the report has to say which it is.
    ///
    /// `read_retry` and `verdict_stops_retry` are GUARDS - the good day is
    /// the one where neither fires. Shouting ON BUT NEVER APPLIED at them
    /// teaches a reader to skim past the phrase, which is fatal: it exists
    /// for `cut_prefix` sitting silently disconnected, and that reading has
    /// already been needed once.
    #[test]
    fn a_guard_that_never_fired_is_not_a_mechanism_that_never_ran() {
        super::applied("ran_once");
        let s = super::summary(&[
            ("ran_once", true),
            ("?a_guard", true),
            ("never_wired", true),
            ("switched_off", false),
        ]);
        assert!(s.contains("ran_once=1"), "{s}");
        assert!(s.contains("a_guard=0 (never needed)"), "{s}");
        assert!(!s.contains("?"), "the marker is not for the reader: {s}");
        assert!(s.contains("never_wired=ON BUT NEVER APPLIED"), "{s}");
        assert!(!s.contains("switched_off"), "{s}");
    }
}

#[cfg(test)]
mod source_consistency {
    /// Every name the report can print must have somewhere that counts it.
    ///
    /// The guard gave a false positive on its first real run: `cut_prefix`
    /// was added to the summary list while the `applied()` call beside it
    /// silently failed to land, so the report said ON BUT NEVER APPLIED for
    /// a mechanism that had published three prefixes. A guard that can cry
    /// wolf is worse than none - it was built precisely to be believed.
    ///
    /// Source-level because there is no way to reach every call site from a
    /// test: they are inside async paths that need a fleet.
    #[test]
    fn every_reported_mechanism_has_a_counter() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut all = String::new();
        for f in std::fs::read_dir(&dir).expect("src") {
            let f = f.expect("entry").path();
            if f.extension().is_some_and(|e| e == "rs") {
                all.push_str(&std::fs::read_to_string(&f).expect("read"));
            }
        }
        // Names the report claims to know about. Only the FIRST string of
        // each ("name", condition) tuple - the condition contains string
        // literals of its own, and the first version of this test tripped
        // over the "1" in `Ok("1")`.
        let mut listed: Vec<String> = Vec::new();
        for (i, _) in all.match_indices("mech::summary(&[") {
            let Some((block, _)) = all[i..].split_once("])") else {
                continue;
            };
            for (j, _) in block.match_indices('(') {
                // Skip whitespace: rustfmt puts multi-line tuples as
                // `(\n    "name",` and the first version of this only
                // matched the single-line ones - so it checked three names
                // and silently skipped the very one that was broken.
                let rest = block[j + 1..].trim_start();
                if !rest.starts_with('"') {
                    continue;
                }
                if let Some(end) = rest[1..].find('"') {
                    // The `?` marks a guard - "zero is fine here" - and
                    // is not part of the name a counter must exist for.
                    let name = rest[1..1 + end].trim_start_matches('?');
                    if name.chars().all(|c| c.is_ascii_lowercase() || c == '_') && !name.is_empty()
                    {
                        listed.push(name.to_owned());
                    }
                }
            }
        }
        assert!(!listed.is_empty(), "found no summary call to check");

        for name in listed {
            assert!(
                all.contains(&format!("applied(\"{name}\")")),
                "{name} is reported but nothing calls applied(\"{name}\") - \
                 the report would say ON BUT NEVER APPLIED for a mechanism \
                 that may be working perfectly"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_mechanism_that_is_on_and_never_fires_says_so() {
        // The three bugs this exists for all looked identical from outside:
        // the flag was on, the run completed, the numbers did not move. What
        // distinguishes "tried and did not help" from "never ran" is whether
        // the decision point was ever reached.
        super::applied("wired");
        super::applied("wired");

        let s = super::summary(&[("wired", true), ("unwired", true), ("off", false)]);
        assert!(s.contains("wired=2"), "{s}");
        assert!(s.contains("unwired=ON BUT NEVER APPLIED"), "{s}");
        assert!(
            !s.contains("off"),
            "a mechanism that is off is not reported: {s}"
        );
    }
}
