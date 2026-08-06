//! The timing store: how long a target takes, and how long its output
//! has stood still.
//!
//! Two statistics, one COARSE key. The key is the target ref plus the
//! build args that reach it - never the cache key, never the content.
//! That is deliberate and it is the opposite of what every other key in
//! this crate wants: a cache key must be exact, or a follower gets
//! someone else's layer; an estimate must be STABLE, because being 20%
//! wrong costs a slightly worse schedule while having no entry at all
//! costs no schedule. Key an estimate on content and it is perfect and
//! useless - every commit empties the table.
//!
//! The unit banked is an OBSERVATION, not an aggregate. One row per
//! (key, run); medians, p90s and stability are computed at read time.
//! That is what makes a delta replayable the way [`super::dice`]'s is:
//! idempotent, order-independent, conflict-free, first writer wins. An
//! aggregate would have to be merged arithmetically, and two runners
//! adding to the same mean is not order-independent at all.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Consecutive unchanged generations before a key is worth banking.
///
/// Principle 14: most of what a build produces dies young. Below this,
/// the upload is paid, the hit rate is zero, and the eviction is paid
/// again.
pub const TENURE_GENERATIONS: u32 = 3;

/// A coarse estimate key: a target ref and the args that reach it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
    /// `target` plus `k=v` args, canonicalised.
    ///
    /// Args are a SET, sorted and last-value-wins: the order the solver
    /// walked them in is not a property of the work, and letting it in
    /// would split one target's samples across two entries.
    pub fn new<'a>(target: &str, args: impl IntoIterator<Item = &'a str>) -> Self {
        let mut kv: BTreeMap<&str, &str> = BTreeMap::new();
        for a in args {
            let a = a.trim().trim_start_matches('-');
            let (k, v) = a.split_once('=').unwrap_or((a, ""));
            kv.insert(k, v);
        }
        let mut s = target.trim().to_owned();
        for (k, v) in kv {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        // A row is one line; a key that could carry one is a forged row.
        debug_assert!(!s.contains('\n'), "a key may not span lines: {s:?}");
        Key(s.replace('\n', " "))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A digest, or nothing. Non-empty ASCII hex and nothing else -
/// `-` is already spoken for as "this run reported none".
///
/// The gate lives here, in one place, because it guards two doors that
/// must agree: a row parsed from a downloaded artifact, and a sample
/// recorded locally. Guarding only the first lets the store hold a
/// digest it cannot persist, and the statistic disappears at the reload
/// that was the whole point of writing it down.
fn hex_digest(d: &str) -> Option<String> {
    (!d.is_empty() && d.bytes().all(|b| b.is_ascii_hexdigit())).then(|| d.to_owned())
}

/// One target's duration in one build, and what it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Build ordinal. Monotonic per lineage; gaps are expected.
    pub run: u64,
    pub ms: u64,
    /// Output digest, or `None` when the run did not report one.
    pub digest: Option<String>,
}

impl Sample {
    /// `<run>\t<ms>\t<digest>\t<key>` - the banked row.
    ///
    /// The key goes LAST because it is the only variable-width field:
    /// `splitn(4)` hands back the whole remainder, so a key containing a
    /// tab still round-trips.
    fn line(&self, key: &str) -> String {
        format!(
            "{}\t{}\t{}\t{}",
            self.run,
            self.ms,
            self.digest.as_deref().unwrap_or("-"),
            key
        )
    }

    /// Rows arrive from a DOWNLOADED artifact, so every field is remote
    /// input and is validated here rather than trusted.
    fn parse(line: &str) -> Option<(Key, Sample)> {
        let mut it = line.splitn(4, '\t');
        let run = it.next()?.parse().ok()?;
        let ms = it.next()?.parse().ok()?;
        let digest = match it.next()? {
            "-" => None,
            d => Some(hex_digest(d)?),
        };
        let key = it.next()?;
        if key.is_empty() || key.contains('\n') {
            return None;
        }
        Some((Key(key.to_owned()), Sample { run, ms, digest }))
    }
}

/// Median and p90 over a key's observed durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub count: usize,
    pub median_ms: u64,
    pub p90_ms: u64,
}

/// Every observation, keyed coarsely, one row per (key, run).
#[derive(Debug, Default, Clone)]
pub struct Store {
    by_key: BTreeMap<Key, BTreeMap<u64, Sample>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// First writer wins, like every other key in the bank. Returns
    /// whether the row was new.
    pub fn record(&mut self, key: Key, mut s: Sample) -> bool {
        // Fail open: an unreadable digest costs us the survival claim
        // for this run, never the duration and never the build. NOT a
        // debug_assert - the value comes from whatever the build tool
        // printed, so this is untrusted input, not a broken invariant.
        s.digest = s.digest.and_then(|d| hex_digest(&d));
        let runs = self.by_key.entry(key).or_default();
        match runs.entry(s.run) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(s);
                true
            }
            std::collections::btree_map::Entry::Occupied(_) => false,
        }
    }

    /// `None` for a key never observed - a cold start is survivable, not
    /// special-cased.
    pub fn stats(&self, key: &Key) -> Option<Stats> {
        let runs = self.by_key.get(key).filter(|r| !r.is_empty())?;
        let mut ms: Vec<u64> = runs.values().map(|s| s.ms).collect();
        ms.sort_unstable();
        // Nearest rank, so an outlier moves p90 and leaves the median
        // alone - which is the reason to keep a distribution at all.
        let at = |pct: u64| ms[((ms.len() as u64 * pct).div_ceil(100).max(1) - 1) as usize];
        Some(Stats {
            count: ms.len(),
            median_ms: at(50),
            p90_ms: at(90),
        })
    }

    /// Consecutive most-recent runs whose output digest matched the
    /// newest one. Zero when the newest run reported no digest.
    ///
    /// Read newest-first, so a row arriving late cannot lengthen a chain
    /// it sits in the middle of. Gaps in run numbers do NOT break the
    /// chain: we can only count builds we were told about, and demanding
    /// contiguity would make a missed upload look like a change.
    pub fn stability(&self, key: &Key) -> u32 {
        let Some(runs) = self.by_key.get(key) else {
            return 0;
        };
        let mut newest_first = runs.values().rev();
        let Some(newest) = newest_first.next().and_then(|s| s.digest.as_deref()) else {
            return 0;
        };
        1 + newest_first
            .take_while(|s| s.digest.as_deref() == Some(newest))
            .count() as u32
    }

    /// Principle 14: promote by survival, never by size.
    pub fn tenured(&self, key: &Key) -> bool {
        self.stability(key) >= TENURE_GENERATIONS
    }

    /// Deterministic text form: sorted by key, then by run.
    pub fn to_lines(&self) -> Vec<String> {
        self.by_key
            .iter()
            .flat_map(|(k, runs)| runs.values().map(|s| s.line(k.as_str())))
            .collect()
    }

    /// Load a table, treating a missing or unreadable file as empty.
    ///
    /// An estimate may only feed decisions where being wrong is cheap
    /// (principle 13), so a table we cannot read costs a worse schedule
    /// and never a wrong answer. Failing the build over it would be the
    /// one way to make a coarse statistic expensive.
    pub fn load(path: &Path) -> Self {
        let mut s = Self::new();
        s.merge(std::fs::read_to_string(path).unwrap_or_default().lines());
        s
    }

    /// Write the table, deterministically. Identical tables produce
    /// identical bytes, so an unchanged lap uploads an identical
    /// artifact rather than a fresh one.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut out = self.to_lines().join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        std::fs::write(path, out).with_context(|| format!("writing timings to {}", path.display()))
    }

    /// Every key whose output has stood still long enough to be worth
    /// banking, longest-surviving first.
    pub fn tenured_keys(&self) -> Vec<(&Key, u32)> {
        let mut v: Vec<(&Key, u32)> = self
            .by_key
            .keys()
            .map(|k| (k, self.stability(k)))
            .filter(|(_, gen)| *gen >= TENURE_GENERATIONS)
            .collect();
        // Ties break on the key, so the order is the table's, not the
        // map's iteration accident.
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// Replay rows. Returns how many were new.
    ///
    /// Idempotent and order-independent, so a restore may apply several
    /// generations without knowing which it already holds. Unparseable
    /// rows are dropped, not fatal: a corrupt sample costs a slightly
    /// worse schedule, and refusing the whole delta costs all of them.
    pub fn merge<S: AsRef<str>>(&mut self, lines: impl IntoIterator<Item = S>) -> usize {
        lines
            .into_iter()
            .filter_map(|l| Sample::parse(l.as_ref()))
            .filter(|(k, s)| self.record(k.clone(), s.clone()))
            .count()
    }
}

/// `rebuck2 bank timings <verb> ...` - the seam a build step writes
/// through, so recording a sample costs a shell line rather than a
/// binding.
///
/// Exit 3 on a key with no samples, matching the bank's other cold
/// paths: a scheduler that cannot get an estimate must fall back to
/// structural order, not to a made-up number (principle 13).
pub fn cli(args: &[&str]) -> Result<()> {
    match args {
        ["record", file, run, target, ms, digest, rest @ ..] => {
            let path = Path::new(file);
            let mut s = Store::load(path);
            s.record(
                Key::new(target, rest.iter().copied()),
                Sample {
                    run: run.parse().context("run must be a build ordinal")?,
                    ms: ms.parse().context("ms must be a duration")?,
                    digest: (*digest != "-").then(|| (*digest).to_owned()),
                },
            );
            s.save(path)
        }
        ["stats", file, target, rest @ ..] => {
            let s = Store::load(Path::new(file));
            let k = Key::new(target, rest.iter().copied());
            let Some(st) = s.stats(&k) else {
                eprintln!("[timings] no samples for {}", k.as_str());
                std::process::exit(3);
            };
            println!(
                "{}\t{}\t{}\t{}\t{}",
                st.count,
                st.median_ms,
                st.p90_ms,
                s.stability(&k),
                s.tenured(&k)
            );
            Ok(())
        }
        ["merge", file, deltas @ ..] => {
            let path = Path::new(file);
            let mut s = Store::load(path);
            let mut new = 0;
            for d in deltas {
                new += s.merge(std::fs::read_to_string(d).unwrap_or_default().lines());
            }
            s.save(path)?;
            println!("{new}");
            Ok(())
        }
        // What is worth banking at all - the cheaper lever, since
        // nothing beats not uploading (principle 14).
        ["tenured", file] => {
            for (k, gen) in Store::load(Path::new(file)).tenured_keys() {
                println!("{gen}\t{}", k.as_str());
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "usage: bank timings record <file> <run> <target> <ms> <digest|-> [args...]\n\
             \x20      bank timings stats <file> <target> [args...]\n\
             \x20      bank timings merge <file> <delta>...\n\
             \x20      bank timings tenured <file>"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(run: u64, ms: u64, digest: &str) -> Sample {
        Sample {
            run,
            ms,
            digest: (digest != "-").then(|| digest.to_owned()),
        }
    }

    #[test]
    fn the_key_is_coarse_on_purpose() {
        // Args are a SET: the order the solver happened to walk them in
        // is not a property of the work, and letting it into the key
        // would split one target's samples across two entries.
        let a = Key::new("+deps", ["mode=0004", "arch=arm64"]);
        let b = Key::new("+deps", ["arch=arm64", "mode=0004"]);
        assert_eq!(a, b, "arg order must not split a key");

        // But a DIFFERENT arg value is different work under one name -
        // `--mode=0004` and `--mode=0777` do not take the same time.
        assert_ne!(a, Key::new("+deps", ["mode=0777", "arch=arm64"]));
        assert_ne!(a, Key::new("+test", ["mode=0004", "arch=arm64"]));

        // Leading dashes are how a user writes them and not part of the
        // identity; a repeated arg takes its last value, as the CLI does.
        assert_eq!(
            Key::new("+deps", ["--mode=0004"]),
            Key::new("+deps", ["mode=0004"])
        );
        assert_eq!(
            Key::new("+deps", ["mode=0004", "mode=0777"]),
            Key::new("+deps", ["mode=0777"]),
            "last value wins, as on the command line"
        );

        // A valueless flag is still an arg that reaches the target.
        assert_ne!(Key::new("+deps", ["ci"]), Key::new("+deps", []));

        // The point of the whole module: nothing content-addressed gets
        // in. If a cache key could reach this, every commit would empty
        // the table (principle 13).
        assert!(!a.as_str().contains("sha256"));
    }

    #[test]
    fn rows_round_trip_and_a_crafted_row_does_not() {
        let k = Key::new("+deps", ["arch=arm64"]);
        let s = sample(7, 94_000, "abc123");
        let line = s.line(k.as_str());
        let (k2, s2) = Sample::parse(&line).expect("own output must parse");
        assert_eq!((k2, s2), (k.clone(), s.clone()));

        // No digest is a legitimate observation: we still learn the
        // duration, we just cannot say anything about stability.
        let none = sample(8, 12, "-");
        assert_eq!(
            Sample::parse(&none.line(k.as_str())).unwrap().1.digest,
            None
        );

        // Remote input. The digest is the one free-text-looking field
        // that later gets compared and printed, so it is hex or nothing.
        for evil in [
            "notanumber\t1\t-\t+deps",
            "1\tnotanumber\t-\t+deps",
            "1\t1\tzz\t+deps",
            "1\t1\t-",        // truncated
            "1\t1\t-\t",      // empty key
            "1\t1\tab\t\n+d", // a newline would forge a second row
        ] {
            assert!(
                Sample::parse(evil).is_none(),
                "{evil:?} must not parse into a replayable row"
            );
        }
    }

    #[test]
    fn a_delta_replays_the_way_dice_does() {
        let k = Key::new("+deps", []);
        let mut a = Store::new();
        a.record(k.clone(), sample(1, 100, "aa"));
        a.record(k.clone(), sample(2, 200, "aa"));
        let mut b = Store::new();
        b.record(k.clone(), sample(3, 300, "bb"));

        // Order-independent: two runners' deltas compose either way.
        let mut ab = Store::new();
        assert_eq!(ab.merge(a.to_lines()), 2);
        assert_eq!(ab.merge(b.to_lines()), 1);
        let mut ba = Store::new();
        ba.merge(b.to_lines());
        ba.merge(a.to_lines());
        assert_eq!(ab.to_lines(), ba.to_lines());

        // Idempotent: a restore may apply several generations without
        // knowing which it already has.
        assert_eq!(ab.merge(a.to_lines()), 0, "replay must add nothing");
        assert_eq!(ab.to_lines(), ba.to_lines());

        // First writer wins - the same (key, run) twice is one row, and
        // which one survives must not depend on arrival order.
        let mut c = ab.clone();
        c.record(k.clone(), sample(1, 999, "cc"));
        assert_eq!(c.stats(&k), ab.stats(&k), "a later row must not overwrite");
    }

    #[test]
    fn stats_are_a_distribution_not_a_single_bit() {
        let k = Key::new("+test", []);
        let mut s = Store::new();
        assert_eq!(s.stats(&k), None, "a cold key must answer None, not 0");

        for (i, ms) in [10, 20, 30, 40, 50, 60, 70, 80, 90, 1000]
            .into_iter()
            .enumerate()
        {
            s.record(k.clone(), sample(i as u64, ms, "-"));
        }
        let got = s.stats(&k).unwrap();
        assert_eq!(got.count, 10);
        // Nearest-rank on a sorted sample: the 5th of 10 for the median,
        // the 9th for p90. The single 1000ms outlier must move p90 and
        // must NOT move the median - that is the whole reason we keep a
        // distribution instead of a mean.
        assert_eq!(got.median_ms, 50);
        assert_eq!(got.p90_ms, 90);

        // One sample is a legitimate distribution of one.
        let k1 = Key::new("+one", []);
        s.record(k1.clone(), sample(0, 42, "-"));
        assert_eq!(
            s.stats(&k1).unwrap(),
            Stats {
                count: 1,
                median_ms: 42,
                p90_ms: 42
            }
        );
    }

    #[test]
    fn stability_counts_generations_and_decides_tenure() {
        // The stem: months without moving. The prize (principle 14).
        let stem = Key::new("+deps", []);
        let mut s = Store::new();
        for run in 1..=5 {
            s.record(stem.clone(), sample(run, 94_000, "beef"));
        }
        assert_eq!(s.stability(&stem), 5);
        assert!(s.tenured(&stem));

        // Changes every commit: never bank it. Upload cost, zero hit
        // rate, and the eviction paid again.
        let churn = Key::new("+build", []);
        for run in 1..=5 {
            s.record(churn.clone(), sample(run, 20_000, &format!("d{run}")));
        }
        assert_eq!(s.stability(&churn), 1, "only the newest run matches itself");
        assert!(!s.tenured(&churn));

        // A change RESETS the count - stability is consecutive from the
        // newest, not a tally of how often it has ever agreed.
        let moved = Key::new("+toolchain", []);
        for run in 1..=4 {
            moved_record(&mut s, &moved, run, "dead");
        }
        moved_record(&mut s, &moved, 5, "beef");
        assert_eq!(s.stability(&moved), 1);
        assert!(!s.tenured(&moved));
        moved_record(&mut s, &moved, 6, "beef");
        moved_record(&mut s, &moved, 7, "beef");
        assert!(s.tenured(&moved), "three consecutive generations tenures");

        // Stability is read NEWEST-first, so an out-of-order arrival
        // (run 6 landing after run 7) cannot fake a longer chain.
        let mut t = Store::new();
        let k = Key::new("+x", []);
        t.record(k.clone(), sample(7, 1, "beef"));
        t.record(k.clone(), sample(5, 1, "beef"));
        t.record(k.clone(), sample(6, 1, "dead"));
        assert_eq!(
            t.stability(&k),
            1,
            "run 6 breaks the chain wherever it lands"
        );

        // No digest reported means we know nothing about survival, and
        // "know nothing" must never tenure.
        let blind = Key::new("+unknown", []);
        for run in 1..=5 {
            t.record(blind.clone(), sample(run, 1, "-"));
        }
        assert_eq!(t.stability(&blind), 0);
        assert!(!t.tenured(&blind));
        assert!(
            t.stats(&blind).is_some(),
            "a digestless run still teaches us the duration"
        );

        assert_eq!(t.stability(&Key::new("+never-seen", [])), 0);
    }

    #[test]
    fn nothing_can_enter_the_store_that_cannot_leave_it() {
        // save/load is the only way a statistic survives to the run that
        // needs it, so anything `record` accepts must round-trip. A
        // digest that is not a digest is downgraded to "unknown" AT THE
        // DOOR: we keep the duration and drop only the survival claim,
        // rather than persist a row that silently vanishes on reload.
        let k = Key::new("+odd", []);
        let mut s = Store::new();
        s.record(k.clone(), sample(1, 10, "not-a-digest"));
        s.record(k.clone(), sample(2, 20, "beef"));

        let mut round = Store::new();
        round.merge(s.to_lines());
        assert_eq!(round.to_lines(), s.to_lines(), "the table must be total");
        assert_eq!(round.stats(&k).map(|st| st.count), Some(2));
        assert_eq!(
            s.stability(&k),
            1,
            "an unreadable digest teaches nothing about survival"
        );
    }

    fn moved_record(s: &mut Store, k: &Key, run: u64, digest: &str) {
        s.record(k.clone(), sample(run, 1_000, digest));
    }

    #[test]
    fn a_second_run_answers_what_the_first_could_not() {
        // M2's done-when, as a test: after one build the table can say
        // how long `+deps` takes with these args, and whether it has
        // moved in the last three builds.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timings.txt");
        let deps = Key::new("+deps", ["arch=arm64"]);

        // Cold start is survivable, not special-cased: run one asks and
        // gets nothing, records, and run two is already informed.
        let mut first = Store::load(&path);
        assert_eq!(first.stats(&deps), None);
        first.record(deps.clone(), sample(1, 94_000, "beef"));
        first.save(&path).unwrap();

        let mut second = Store::load(&path);
        let st = second.stats(&deps).expect("run two inherits run one");
        assert_eq!((st.count, st.median_ms), (1, 94_000));
        assert!(!second.tenured(&deps), "one generation is not tenure");

        // Two more unchanged laps and the stem is worth banking.
        second.record(deps.clone(), sample(2, 91_000, "beef"));
        second.record(deps.clone(), sample(3, 96_000, "beef"));
        second.save(&path).unwrap();
        let third = Store::load(&path);
        assert_eq!(third.stats(&deps).unwrap().count, 3);
        assert_eq!(third.stability(&deps), 3);
        assert!(third.tenured(&deps));
        assert_eq!(
            third
                .tenured_keys()
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>(),
            vec![&deps]
        );

        // Determinism: the same table must write the same bytes, or an
        // unchanged lap uploads a fresh artifact for nothing.
        let other = dir.path().join("again.txt");
        third.save(&other).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(&other).unwrap()
        );

        // A table we cannot read costs a worse schedule, never a build.
        std::fs::write(&path, b"\xff\xfe not a table at all\n").unwrap();
        assert_eq!(Store::load(&path).stats(&deps), None);
        assert!(Store::load(dir.path().join("absent.txt").as_path())
            .to_lines()
            .is_empty());
    }
}
