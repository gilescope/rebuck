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

/// Is a preference firing far more often than its brake reorders anything?
///
/// A preference that feeds itself needs a counterweight, and the
/// counterweight has to grow with the number of preferences - principle 27.
/// Nothing warns you when it stops: each term is defensible, the sort still
/// compiles, and the symptom is an idle machine in a fleet reporting high
/// occupancy.
///
/// The ratio does warn you. `affinity_imports` fired 1,724 times against
/// `balance`'s 361 reorders in the run where the leg went 1050s to 1475s -
/// a preference applying five times per brake application is not being
/// traded against anything, it is winning outright with extra steps.
///
/// Four is the threshold, and it is coarse on purpose - principle 13. It
/// separates "sometimes decisive" from "in charge", and nothing finer could
/// be justified from one measurement.
pub fn outweighs(counts: &[(&str, u64)], preference: &str, brake: &str) -> Option<String> {
    let get = |n: &str| counts.iter().find(|(k, _)| *k == n).map(|(_, v)| *v);
    let (p, b) = (get(preference)?, get(brake)?);
    if b == 0 || p < b.saturating_mul(4) {
        return None;
    }
    Some(format!(
        "{preference} applied {p}x against {brake}'s {b} - ratio {:.1}, so the \
         preference is outweighing its brake rather than being traded against \
         it (principle 27)",
        p as f64 / b as f64
    ))
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
    /// The ratio that would have caught the over-weighting on the day.
    #[test]
    fn a_preference_outweighing_its_brake_says_so() {
        let real = [("affinity_imports", 1724u64), ("balance", 361u64)];
        let m = super::outweighs(&real, "affinity_imports", "balance").expect("flagged");
        assert!(m.contains("ratio 4.8"), "{m}");

        // Traded, not dominant: no complaint.
        assert!(super::outweighs(&[("p", 300), ("b", 200)], "p", "b").is_none());
        // Exactly at the threshold is not over it - 4x is the line.
        assert!(super::outweighs(&[("p", 800), ("b", 200)], "p", "b").is_some());
        assert!(super::outweighs(&[("p", 799), ("b", 200)], "p", "b").is_none());
        // A brake that never fired cannot be outweighed, only absent - and
        // saying "ratio infinity" would be noise, not a finding.
        assert!(super::outweighs(&[("p", 900), ("b", 0)], "p", "b").is_none());
        // Either side missing entirely: nothing to compare.
        assert!(super::outweighs(&[("p", 900)], "p", "b").is_none());
    }

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
mod every_switch_is_reported {
    /// The mirror of `source_consistency`, and the hole it left.
    ///
    /// That test proves every REPORTED name has a counter. It cannot prove
    /// the converse - that every switch which changes behaviour is reported -
    /// and three were not: `peer_cache_mounts`, set in every CI run and the
    /// exception that makes seven dispatches in eight legal; `fleet_cache`;
    /// and `warm`. Each acted, none appeared in the mechanisms line, and no
    /// run could say whether any of them did anything.
    ///
    /// A switch not in the summary is not automatically a bug - most of
    /// these name an address, a file or a size. So the exemptions are listed
    /// WITH a reason, and adding a new behavioural flag means either
    /// reporting it or saying here why it needs no report. Both are cheap;
    /// silently doing neither is what this exists to stop.
    #[test]
    fn a_flag_that_changes_behaviour_appears_in_the_report() {
        // NOT mechanisms: configuration, addresses, sizes, and the knobs of
        // the synthetic LLB generator used by scripts/fleet.sh.
        const CONFIG: &[&str] = &[
            "REBUCK2_MIRROR",            // where the mirror is
            "REBUCK2_TARGET",            // a label for the verdict line
            "REBUCK2_COMPRESSION",       // exporter attrs, reported by the run itself
            "REBUCK2_CONNS",             // connection pool size
            "REBUCK2_KEEPALIVE_S",       // h2 keepalive
            "REBUCK2_HOME_SLOTS",        // capacity, reported as `home peak`
            "REBUCK2_CACHE_SEEDS",       // seeds, reported as `seeds=`
            "REBUCK2_CACHE_SEEDS_FILE",  //   ditto
            "REBUCK2_CACHE_INPUTS_FILE", //   ditto
            "REBUCK2_EXEC_BASE",         // exec sandbox
            "REBUCK2_KEEP_SCRATCH",      // debugging aid
            "REBUCK2_NESTED_HOST",       // an address for local_nested
            "REBUCK2_PREFETCH_LANES",    // concurrency of a reported mechanism
            "REBUCK2_SECRET",            // a secret's value
            "REBUCK2_GATE",              // reported as `not routed`
            "REBUCK2_MIN_SIBLINGS",      // reported as min_siblings
            "REBUCK2_MIN_OPS",           // reported as min_ops
            "REBUCK2_GRAFT",             // reported on its own line
            "REBUCK2_WARMUP",            // a count, not a switch
            "REBUCK2_PREFETCH_ALL",      // reported as `prefetch_broadcast`
            "REBUCK2_FLEET_CACHE",       // three-valued, and the mode is in
            // the solve request rather than a
            // decision this proxy takes
            "REBUCK2_H", // llb generator
            "REBUCK2_LLB_BASE",
            "REBUCK2_LLB_CONTEXT",
            "REBUCK2_LLB_HOSTNET",
            "REBUCK2_LLB_N",
            "REBUCK2_LLB_OUT",
            "REBUCK2_LLB_PLATFORM",
            "REBUCK2_LLB_WORK",
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut all = String::new();
        for f in std::fs::read_dir(&dir).expect("src") {
            let f = f.expect("entry").path();
            if f.extension().is_some_and(|e| e == "rs") {
                all.push_str(&std::fs::read_to_string(&f).expect("read"));
            }
        }
        let mut vars: Vec<String> = Vec::new();
        let bytes = all.as_bytes();
        for (i, _) in all.match_indices("REBUCK2_") {
            let mut j = i + "REBUCK2_".len();
            while j < bytes.len() && (bytes[j].is_ascii_uppercase() || bytes[j] == b'_') {
                j += 1;
            }
            let v = all[i..j].trim_end_matches('_').to_owned();
            // `REBUCK2` on its own is prose - the binary's name in a comment
            // or a log line - not an environment variable.
            if v == "REBUCK2" {
                continue;
            }
            if !vars.contains(&v) {
                vars.push(v);
            }
        }
        let mut unreported = Vec::new();
        for v in vars {
            if CONFIG.contains(&v.as_str()) {
                continue;
            }
            // The counter name is the variable, lowercased and unprefixed -
            // the convention every reported mechanism already follows.
            let name = v.trim_start_matches("REBUCK2_").to_lowercase();
            if !all.contains(&format!("applied(\"{name}\")")) {
                unreported.push(format!("{v} -> expected applied(\"{name}\")"));
            }
        }
        assert!(
            unreported.is_empty(),
            "these switches change behaviour and no run reports them; either \
             count them or add them to CONFIG with a reason:\n  {}",
            unreported.join("\n  ")
        );
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
