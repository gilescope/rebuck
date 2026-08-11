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
    for (name, on) in enabled {
        if !on {
            continue;
        }
        match counts.get(name) {
            Some(n) => out.push(format!("{name}={n}")),
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
