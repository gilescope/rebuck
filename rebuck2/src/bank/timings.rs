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

/// Nearest-rank median of an unsorted slice; 0 for nothing at all.
fn median(v: &[u64]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut v = v.to_vec();
    v.sort_unstable();
    v[(v.len() - 1) / 2]
}

/// An assignment of keys to runners, and what it expects each to cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub bins: Vec<Vec<Key>>,
    pub totals: Vec<u64>,
    /// How many keys were placed on the fallback prior rather than on
    /// their own samples. A plan that is mostly guesses is one.
    pub guessed: usize,
}

impl Plan {
    /// The makespan this plan predicts: the worst bin, since they run in
    /// parallel and the build is not done until the last one is.
    pub fn makespan_ms(&self) -> u64 {
        self.totals.iter().copied().max().unwrap_or(0)
    }
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

    /// The newest run's digest for a key, if it reported one. What an
    /// ingest carries forward when a target was left alone.
    pub fn newest_digest(&self, key: &Key) -> Option<&str> {
        self.by_key
            .get(key)?
            .values()
            .next_back()?
            .digest
            .as_deref()
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

    /// Drop all but the newest `n` observations of each key.
    ///
    /// The table would otherwise grow one row per key per build forever,
    /// and the old rows answer nothing: both statistics read from the
    /// newest end. Pruning is not idempotent ACROSS machines - a peer
    /// that still holds run 1 re-adds it on the next merge - but that is
    /// harmless, because the row is dropped again and the estimate it
    /// perturbs is coarse by construction.
    pub fn retain_recent(&mut self, n: usize) -> usize {
        let mut dropped = 0;
        for runs in self.by_key.values_mut() {
            while runs.len() > n {
                let oldest = *runs.keys().next().expect("non-empty");
                runs.remove(&oldest);
                dropped += 1;
            }
        }
        dropped
    }

    /// Split `keys` across `bins` runners, longest-processing-time-first.
    ///
    /// The standard makespan heuristic, and the reason M1 waited for
    /// this module: the static per-target proxy it would otherwise use
    /// correlates r=0.734 and is 10x wrong on groups with few targets.
    ///
    /// A key with no samples is costed at the MEDIAN of the keys that
    /// have them. Zero would pile every unknown into one bin, and
    /// refusing to place it is not an option - it still has to be built.
    /// `Plan::guessed` says how many were placed that way, because a
    /// plan that is mostly guesses should be read as one.
    pub fn bin_pack(&self, keys: &[Key], bins: usize) -> Plan {
        let mut plan = Plan {
            bins: vec![Vec::new(); bins],
            totals: vec![0; bins],
            guessed: 0,
        };
        if bins == 0 {
            return plan;
        }

        let known: Vec<u64> = keys
            .iter()
            .filter_map(|k| self.stats(k))
            .map(|s| s.median_ms)
            .collect();
        let prior = median(&known);
        // Descending cost, ties on the key: same input, same plan, on
        // every machine (an unstable order here would make two runners
        // disagree about which group they are building).
        let mut costed: Vec<(u64, &Key, bool)> = keys
            .iter()
            .map(|k| match self.stats(k) {
                Some(s) => (s.median_ms, k, false),
                None => (prior, k, true),
            })
            .collect();
        costed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

        for (ms, key, guessed) in costed {
            // Lightest bin - LPT's greedy step. Ties break on how many
            // keys the bin already holds, THEN on index: with a cold
            // table every key costs the same nothing, and a tie-break on
            // index alone would put all of them in bin 0. Same hazard
            // for genuinely instant targets, of which this build has
            // hundreds.
            let (i, _) = plan
                .totals
                .iter()
                .enumerate()
                .min_by_key(|(i, t)| (**t, plan.bins[*i].len(), *i))
                .expect("bins > 0");
            plan.bins[i].push(key.clone());
            plan.totals[i] += ms;
            plan.guessed += usize::from(guessed);
        }
        plan
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

/// How many observations of each key survive a publish.
///
/// Both statistics read from the newest end, so this is the whole
/// history anything consults. It also bounds the artifact: the table
/// would otherwise grow one row per key per build, forever.
const KEEP_SAMPLES: usize = 8;

/// `timings-<lineage>-<role>` - ONE artifact name per role per lineage.
///
/// Not per run: [`crate::github::Client::by_prefix`] already keeps the
/// newest artifact of each name, so a stable name gives a self-cleaning
/// head per role and a restore that downloads N artifacts for an
/// N-machine fleet rather than N per lap.
///
/// The lineage is in the name because artifact names are not namespaced
/// and provenance is checked against it - drop it and a branch feeds the
/// trunk's schedule. The role is in the name because two roles on one
/// run may not upload the same artifact name.
pub fn artifact_name(lineage: &str, role: &str) -> String {
    format!("timings-{lineage}-{role}")
}

pub struct Restore<'a> {
    pub table: &'a Path,
    pub lineage: &'a str,
    pub parent: Option<&'a str>,
}

/// Merge every role's banked table into the local one.
///
/// Returns the rows gained, or `None` when nothing was found - a cold
/// table is a normal first lap, and the caller falls back to structural
/// order rather than to a made-up number.
pub async fn restore(r: Restore<'_>, work: &Path) -> Result<Option<usize>> {
    let gh = crate::github::Client::from_env()?;
    std::fs::create_dir_all(work)?;

    let mut arts = gh
        .by_prefix(&format!("timings-{}-", r.lineage), r.lineage)
        .await?;
    // A branch with no table of its own inherits the trunk's. The
    // estimate is coarse by construction, so the trunk's numbers are
    // exactly as good a prior as this branch's would have been.
    if arts.is_empty() {
        if let Some(parent) = r.parent.filter(|p| *p != r.lineage) {
            arts = gh.by_prefix(&format!("timings-{parent}-"), parent).await?;
            if !arts.is_empty() {
                println!("[timings] inheriting {parent}'s table");
            }
        }
    }
    if arts.is_empty() {
        println!("[timings] no banked table - cold");
        return Ok(None);
    }

    let mut store = Store::load(r.table);
    let mut gained = 0;
    for a in crate::github::newest_first(arts) {
        let dir = work.join("timings-in");
        // One role's table failing to download costs its samples, not
        // the restore: an estimate may only feed decisions where being
        // wrong is cheap, and that includes being absent.
        if let Err(e) = gh.download_to(a.id, &dir).await {
            eprintln!("[timings] {} unreadable, skipping: {e}", a.name);
            continue;
        }
        gained += store.merge(
            std::fs::read_to_string(dir.join("timings.txt"))
                .unwrap_or_default()
                .lines(),
        );
    }
    store.retain_recent(KEEP_SAMPLES);
    store.save(r.table)?;
    println!(
        "[timings] merged {gained} rows; {} keys tenured",
        store.tenured_keys().len()
    );
    Ok(Some(gained))
}

pub struct Publish<'a> {
    pub table: &'a Path,
    pub lineage: &'a str,
    pub role: &'a str,
}

/// Stage the whole table for upload. Returns false when there is
/// nothing to bank.
///
/// The WHOLE table, not this lap's delta: it is a few thousand lines
/// after pruning, and self-contained means one artifact bootstraps a
/// cold machine. `bank/dice.rs` deltas because it is millions of rows;
/// copying that machinery here would be ceremony for a text file.
pub fn publish(p: Publish<'_>, work: &Path) -> Result<bool> {
    let mut store = Store::load(p.table);
    if store.to_lines().is_empty() {
        println!("[timings] nothing recorded this lap");
        return Ok(false);
    }
    store.retain_recent(KEEP_SAMPLES);
    let out = work.join("timings-out");
    std::fs::create_dir_all(&out)?;
    store.save(&out.join("timings.txt"))?;
    println!(
        "[timings] staged {} rows as {}",
        store.to_lines().len(),
        artifact_name(p.lineage, p.role)
    );
    Ok(true)
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
        // M1: twelve groups of equal expected cost. Prints one line per
        // bin, `<expected ms>\t<key>[\t<key>...]`, so the caller can
        // hand bin N to runner N without re-deriving anything.
        ["plan", file, bins, targets @ ..] => {
            let s = Store::load(Path::new(file));
            let keys: Vec<Key> = targets.iter().map(|t| Key::new(t, [])).collect();
            let bins: usize = bins.parse().context("bins must be a runner count")?;
            let plan = s.bin_pack(&keys, bins);
            for (total, keys) in plan.totals.iter().zip(&plan.bins) {
                let names: Vec<&str> = keys.iter().map(Key::as_str).collect();
                println!("{total}\t{}", names.join("\t"));
            }
            eprintln!(
                "[timings] makespan {}ms, {} of {} keys costed by prior",
                plan.makespan_ms(),
                plan.guessed,
                keys.len()
            );
            Ok(())
        }
        // The ingest: `earthly --logstream-debug-file=X` already emits
        // every target's name, args, span and dependencies, so nothing
        // needs forking to learn what a build cost.
        ["ingest", file, run, log] => {
            super::logstream::ingest(
                Path::new(file),
                run.parse().context("run must be a build ordinal")?,
                Path::new(log),
            )?;
            Ok(())
        }
        // Computed ONCE, from one build's graph, because it answers a
        // question nothing else does: the N at which adding workers
        // stops paying.
        ["critical", log] => {
            let targets = super::logstream::parse(&std::fs::read_to_string(log)?);
            let cp = super::logstream::critical_path(&targets);
            for name in &cp.path {
                println!("{name}");
            }
            eprintln!(
                "[timings] critical path {}ms of {}ms total work; \
                 saturates at {} runners",
                cp.ms,
                cp.total_ms,
                cp.saturation_n()
            );
            Ok(())
        }
        ["publish", file, lineage, role] => {
            let staged = publish(
                Publish {
                    table: Path::new(file),
                    lineage,
                    role,
                },
                &super::bank_work(),
            )?;
            if staged {
                if let Ok(out) = std::env::var("GITHUB_OUTPUT") {
                    use std::io::Write;
                    // NOT `have` - cas-publish already owns that key in
                    // the same step's output, and the two uploads would
                    // gate on each other.
                    writeln!(
                        std::fs::OpenOptions::new().append(true).open(out)?,
                        "timings-have=1\ntimings-name={}",
                        artifact_name(lineage, role)
                    )?;
                }
            }
            Ok(())
        }
        ["prune", file, keep] => {
            let path = Path::new(file);
            let mut s = Store::load(path);
            let dropped = s.retain_recent(keep.parse().context("keep must be a sample count")?);
            s.save(path)?;
            println!("{dropped}");
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
             \x20      bank timings ingest <file> <run> <logstream-debug-file>\n\
             \x20      bank timings stats <file> <target> [args...]\n\
             \x20      bank timings merge <file> <delta>...\n\
             \x20      bank timings plan <file> <bins> <target>...\n\
             \x20      bank timings prune <file> <keep>\n\
             \x20      bank timings tenured <file>\n\
             \x20      bank timings critical <logstream-debug-file>"
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

    #[test]
    fn the_table_does_not_grow_without_bound() {
        // One row per key per build, forever, is not a statistics table -
        // it is a log. Both statistics read from the newest end, so the
        // old rows answer nothing.
        let k = Key::new("+deps", []);
        let mut s = Store::new();
        for run in 1..=10 {
            s.record(k.clone(), sample(run, run * 10, "beef"));
        }
        assert_eq!(s.retain_recent(3), 7);
        assert_eq!(s.stats(&k).unwrap().count, 3);
        assert_eq!(
            s.to_lines(),
            vec![
                "8\t80\tbeef\t+deps",
                "9\t90\tbeef\t+deps",
                "10\t100\tbeef\t+deps"
            ],
            "the NEWEST runs must survive, not the first three seen"
        );
        assert_eq!(s.retain_recent(3), 0, "pruning twice drops nothing");

        // A peer that still holds the old rows re-adds them on merge.
        // Harmless: they are dropped again, and the estimate they
        // perturb is coarse by construction.
        let mut peer = Store::new();
        peer.record(k.clone(), sample(1, 10, "beef"));
        s.merge(peer.to_lines());
        assert_eq!(s.stats(&k).unwrap().count, 4);
        s.retain_recent(3);
        assert_eq!(s.stats(&k).unwrap().count, 3);
    }

    #[test]
    fn longest_first_packs_the_groups_m1_could_not() {
        // M1's actual job: twelve groups of equal expected cost. The
        // static proxy it would otherwise use correlates r=0.734 and is
        // 10x wrong on small groups.
        let mut s = Store::new();
        let keys: Vec<Key> = [500u64, 400, 300, 300, 200, 100]
            .iter()
            .enumerate()
            .map(|(i, ms)| {
                let k = Key::new(&format!("+t{i}"), []);
                s.record(k.clone(), sample(1, *ms, "beef"));
                k
            })
            .collect();

        let plan = s.bin_pack(&keys, 3);
        assert_eq!(plan.guessed, 0);
        assert_eq!(plan.totals.iter().sum::<u64>(), 1800, "no work may vanish");
        assert_eq!(
            plan.bins.iter().map(Vec::len).sum::<usize>(),
            keys.len(),
            "no key may be dropped or duplicated"
        );
        // LPT on 500/400/300/300/200/100 into 3 gives 600 each. The
        // point of the exercise: the naive split sets makespan by the
        // worst bin, and this one has no worst bin.
        assert_eq!(plan.totals, vec![600, 600, 600]);
        assert_eq!(plan.makespan_ms(), 600);

        // Determinism: two runners must derive the SAME plan, or they
        // disagree about which group each is building.
        assert_eq!(plan, s.bin_pack(&keys, 3));
        let shuffled: Vec<Key> = keys.iter().rev().cloned().collect();
        assert_eq!(
            plan.totals,
            s.bin_pack(&shuffled, 3).totals,
            "input order must not change the plan"
        );

        // A key with no samples costs the median of those that have
        // them - zero would pile every unknown into one bin, and the
        // work still has to be built somewhere.
        let cold = Key::new("+brand-new", []);
        let mut with_cold = keys.clone();
        with_cold.push(cold);
        let p = s.bin_pack(&with_cold, 3);
        assert_eq!(p.guessed, 1);
        assert_eq!(p.totals.iter().sum::<u64>(), 1800 + 300);

        // A wholly cold table still produces a plan - round-robin by
        // construction, since every key costs the same nothing.
        let empty = Store::new();
        let p = empty.bin_pack(&keys, 3);
        assert_eq!(p.guessed, 6);
        assert_eq!(
            p.bins.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![2, 2, 2]
        );

        // Degenerate shapes must not panic: they are reached on the
        // first build of a new lineage, not in a test only.
        assert_eq!(s.bin_pack(&keys, 0).bins.len(), 0);
        assert_eq!(s.bin_pack(&[], 3).totals, vec![0, 0, 0]);
        assert_eq!(
            s.bin_pack(&keys, 12)
                .bins
                .iter()
                .filter(|b| b.is_empty())
                .count(),
            6
        );
    }

    #[test]
    fn the_artifact_name_isolates_lineage_and_role() {
        // Artifact names are not namespaced, and provenance is checked
        // against the lineage IN the name - drop it and a branch feeds
        // the trunk's schedule with its own numbers.
        assert_ne!(
            artifact_name("main", "driver"),
            artifact_name("feature", "driver")
        );
        // Two roles on one run may not upload the same artifact name,
        // and one runner hosts several (the driver box also runs a
        // co-worker).
        assert_ne!(
            artifact_name("main", "driver"),
            artifact_name("main", "worker")
        );
        // The prefix a restore lists on must match what a publish
        // writes, or the bank is silently write-only.
        assert!(artifact_name("main", "driver").starts_with("timings-main-"));
    }

    #[test]
    fn the_action_uploads_under_the_name_the_restore_lists() {
        // The bank is silently WRITE-ONLY if these drift: publish uploads
        // `timings-<lineage>-<role>`, restore lists the prefix
        // `timings-<lineage>-`, and nothing anywhere fails if the two stop
        // agreeing - the table just never comes back. So the workflow's
        // string is checked against the function that defines it.
        let yaml = include_str!("../../actions/bank-publish/action.yml");
        let expected = "name: timings-${{ steps.pack.outputs.lineage }}-${{ inputs.role }}";
        assert!(
            yaml.contains(expected),
            "bank-publish must upload as {expected:?}, matching artifact_name"
        );
        assert_eq!(artifact_name("LIN", "ROLE"), "timings-LIN-ROLE");
        // And the gate may not be `have`, which cas-publish already owns in
        // the same step - the two uploads would gate on each other.
        assert!(yaml.contains("steps.pack.outputs.timings-have == '1'"));
    }

    #[test]
    fn a_publish_stages_a_bounded_self_contained_table() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("timings.txt");
        let work = dir.path().join("work");

        // Nothing recorded is not a failure - it is a lap that built
        // only cache hits.
        assert!(!publish(
            Publish {
                table: &table,
                lineage: "main",
                role: "driver"
            },
            &work
        )
        .unwrap());

        let k = Key::new("+deps", []);
        let mut s = Store::new();
        for run in 1..=20 {
            s.record(k.clone(), sample(run, 90_000 + run, "beef"));
        }
        s.save(&table).unwrap();
        assert!(publish(
            Publish {
                table: &table,
                lineage: "main",
                role: "driver"
            },
            &work
        )
        .unwrap());

        // Bounded: the artifact must not grow one row per key per build
        // forever, and the newest samples are the ones that answer.
        let staged = Store::load(&work.join("timings-out/timings.txt"));
        assert_eq!(staged.stats(&k).unwrap().count, KEEP_SAMPLES);
        assert_eq!(staged.stats(&k).unwrap().median_ms, 90_000 + 16);
        // Self-contained: a cold machine restoring ONE role's artifact
        // has a usable table, which is why the whole table travels
        // rather than a delta.
        assert!(staged.tenured(&k));
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
