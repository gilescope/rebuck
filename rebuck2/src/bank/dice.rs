//! Dice bank: the value store is a CAS in sqlite clothing, so it banks
//! like one.
//!
//! `pagable.{0..15}.db` hold one table of content-addressed 128-bit keys
//! with `INSERT OR IGNORE` writes (shard = `key_lo & 15`). That makes
//! deltas exportable as deterministic text and replayable idempotently:
//! order-independent, conflict-free, and diffable on the key alone.
//!
//! The manifest name carries a hash of the fork-rev + snapshot seed.
//! Banked rows are only valid within one reuse gate, so a rev bump
//! orphans them and retention reaps - there is no way for a stale row to
//! be served under a new graph.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::manifest::{Manifest, Segment};
use super::zstd;
use crate::store::sha256_hex;

/// Default raw split size for a row segment.
const SEG_RAW_MB_DEFAULT: u64 = 256;

/// `sha256(seed)[..8]` - the reuse gate, embedded in the manifest name.
pub fn seed8(seed: &str) -> String {
    sha256_hex(seed.as_bytes())[..8].to_owned()
}

pub fn manifest_name(lineage: &str, seed: &str) -> String {
    format!("cas-manifest-{lineage}-dice-{}", seed8(seed))
}

/// One exported row: `<shard> <key_hi> <key_lo> <hex value>`.
struct Row {
    shard: u8,
    key_hi: i64,
    key_lo: i64,
    value_hex: String,
}

impl Row {
    fn parse(line: &str) -> Option<Self> {
        let mut it = line.splitn(4, ' ');
        Some(Row {
            shard: it.next()?.parse().ok()?,
            key_hi: it.next()?.parse().ok()?,
            key_lo: it.next()?.parse().ok()?,
            value_hex: it.next()?.to_owned(),
        })
    }
    fn key(&self) -> String {
        format!("{} {}", self.key_hi, self.key_lo)
    }
    fn line(&self) -> String {
        format!(
            "{} {} {} {}",
            self.shard, self.key_hi, self.key_lo, self.value_hex
        )
    }
}

/// sqlite3 via the CLI, for the same reason zstd and tar are: every
/// runner has it, the shell layer used it on these exact files, and
/// rusqlite would drag a C build into the engine for one table.
fn sqlite(db: &Path, sql: &str, readonly: bool) -> Result<String> {
    let mut c = Command::new("sqlite3");
    if readonly {
        c.arg("-readonly");
    }
    let out = c
        .arg(db)
        .arg(sql)
        .output()
        .with_context(|| format!("sqlite3 {}", db.display()))?;
    if !out.status.success() {
        bail!(
            "sqlite3 {}: {}",
            db.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every row in the store, shard-ordered.
fn export(db: &Path) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for i in 0..16 {
        let f = db.join(format!("pagable.{i}.db"));
        if !f.is_file() {
            continue;
        }
        let sql = "SELECT printf('%d %d %d ', key_lo & 15, key_hi, key_lo) || hex(value) \
                   FROM pagable_data ORDER BY key_hi, key_lo;";
        rows.extend(sqlite(&f, sql, true)?.lines().filter_map(Row::parse));
    }
    Ok(rows)
}

/// Sorted `"<key_hi> <key_lo>"` for every row - the banked-set shape.
pub fn keys(db: &Path) -> Result<Vec<String>> {
    let mut v: Vec<String> = export(db)?.iter().map(Row::key).collect();
    v.sort();
    Ok(v)
}

/// Pack rows the bank does not hold into deterministic text segments.
///
/// Segment name is the sha256 of the raw rows file, so identical deltas
/// pack to identical names whatever the compressor does.
pub fn pack(db: &Path, banked: &[String], out: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(out)?;
    let have: std::collections::HashSet<&str> = banked.iter().map(String::as_str).collect();
    let max = std::env::var("DICE_SEG_RAW_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(SEG_RAW_MB_DEFAULT)
        * 1024
        * 1024;

    let mut parts: Vec<Vec<Row>> = Vec::new();
    let mut cur: Vec<Row> = Vec::new();
    let mut bytes = 0u64;
    for r in export(db)? {
        if have.contains(r.key().as_str()) {
            continue;
        }
        let n = r.line().len() as u64 + 1;
        if bytes >= max && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += n;
        cur.push(r);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }

    let mut names = Vec::new();
    for part in parts {
        let raw: String = part
            .iter()
            .map(|r| r.line() + "\n")
            .collect::<Vec<_>>()
            .join("");
        let sha = sha256_hex(raw.as_bytes());
        let dir = out.join(format!("cas-seg-{sha}"));
        std::fs::create_dir_all(&dir)?;

        let mut ids: Vec<String> = part.iter().map(Row::key).collect();
        ids.sort();
        zstd::write_lines(&dir.join("blobs.txt.zst"), &ids)?;

        let plain = dir.join("rows.txt");
        std::fs::write(&plain, &raw)?;
        zstd::compress_file(&plain, &dir.join("rows.txt.zst"), 8)?;
        std::fs::remove_file(&plain)?;

        let seg = Segment {
            name: format!("cas-seg-{sha}"),
            bytes: std::fs::metadata(dir.join("rows.txt.zst"))?.len(),
            blobs: ids.len() as u64,
            prefixes: "*".into(),
            artifact: None,
            run: None,
            role: None,
            full: None,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_string(&seg)? + "\n")?;
        names.push(seg.name);
    }
    Ok(names)
}

/// Replay segments into the sharded dbs.
///
/// `INSERT OR IGNORE` on content-addressed keys: idempotent,
/// order-independent, conflict-free - which is what lets a restore apply
/// several generations without caring which came first.
pub fn merge(db: &Path, segments: &[PathBuf]) -> Result<u64> {
    std::fs::create_dir_all(db)?;
    let mut by_shard: Vec<Vec<String>> = vec![Vec::new(); 16];
    let mut merged = 0;
    for d in segments {
        let rows = d.join("rows.txt.zst");
        if !rows.is_file() {
            continue;
        }
        for line in zstd::read_lines(&rows)? {
            if let Some(r) = Row::parse(&line) {
                // The value is hex from sqlite's own hex(), so it cannot
                // carry a quote - but bind it as a blob literal anyway.
                by_shard[(r.shard & 15) as usize].push(format!(
                    "INSERT OR IGNORE INTO pagable_data VALUES({},{},X'{}');",
                    r.key_lo, r.key_hi, r.value_hex
                ));
            }
        }
        merged += 1;
    }
    for (i, stmts) in by_shard.iter().enumerate() {
        let f = db.join(format!("pagable.{i}.db"));
        sqlite(
            &f,
            "CREATE TABLE IF NOT EXISTS pagable_data (
               key_lo INTEGER NOT NULL, key_hi INTEGER NOT NULL,
               value BLOB NOT NULL, UNIQUE(key_hi, key_lo));",
            false,
        )?;
        if stmts.is_empty() {
            continue;
        }
        // One transaction per shard: a statement-at-a-time replay of a
        // 4.6M-row bank is minutes of fsync.
        let mut child = Command::new("sqlite3")
            .arg(&f)
            .stdin(Stdio::piped())
            .spawn()?;
        {
            let mut si = child.stdin.take().expect("piped");
            writeln!(si, "BEGIN;")?;
            for s in stmts {
                writeln!(si, "{s}")?;
            }
            writeln!(si, "COMMIT;")?;
        }
        if !child.wait()?.success() {
            bail!("sqlite3 {}: replay failed", f.display());
        }
    }
    Ok(merged)
}

/// The graph skeleton rides the manifest whole: byte-stable, and it must
/// be atomic with the row index that references it.
fn graph_meta(dice_dir: &Path) -> PathBuf {
    dice_dir.join("graph.meta")
}

pub struct Restore<'a> {
    pub dice_dir: &'a Path,
    pub lineage: &'a str,
    pub parent: Option<&'a str>,
    pub seed: &'a str,
}

/// Returns the number of segments merged, or `None` for a cold bank.
pub async fn restore(r: Restore<'_>, work: &Path) -> Result<Option<u64>> {
    let gh = crate::github::Client::from_env()?;
    std::fs::create_dir_all(work)?;
    let name = manifest_name(r.lineage, r.seed);

    let mut art = gh.by_name(&name, r.lineage).await?.into_iter().next();
    // A branch with no dice bank of its own inherits the trunk's. The
    // seed hash already gates validity, so an inherited bank is only ever
    // read when it is the SAME graph.
    if art.is_none() {
        if let Some(parent) = r.parent.filter(|p| *p != r.lineage) {
            art = gh
                .by_name(&manifest_name(parent, r.seed), parent)
                .await?
                .into_iter()
                .next();
            if art.is_some() {
                println!("[dice-bank] inheriting {parent}'s dice bank");
            }
        }
    }
    let Some(art) = art else {
        println!("[dice-bank] no {name} - cold dice bank");
        return Ok(None);
    };

    let head = work.join("dice-head");
    gh.download_to(art.id, &head).await?;
    let m = Manifest::read(&head.join("manifest.json"))?;
    let banked = zstd::read_lines(&head.join("blobs.txt.zst"))?;
    println!(
        "[dice-bank] manifest {}@{}: {} segments, {} rows",
        seed8(r.seed),
        m.generation,
        m.segments.len(),
        banked.len()
    );
    std::fs::write(work.join("dice-banked-keys.txt"), banked.join("\n") + "\n")?;

    let skeleton = head.join("graph.meta.zst");
    if skeleton.is_file() {
        std::fs::create_dir_all(r.dice_dir)?;
        let out = graph_meta(r.dice_dir);
        let bytes = Command::new("zstd")
            .args(["-dqc"])
            .arg(&skeleton)
            .output()?;
        std::fs::write(&out, bytes.stdout)?;
    }

    let mut containers: Vec<&str> = m
        .segments
        .iter()
        .filter_map(|s| s.artifact.as_deref())
        .collect();
    containers.sort();
    containers.dedup();
    let seg_dir = work.join("dice-seg");
    let mut merged = 0;
    for c in containers {
        let Some(a) = gh.by_name(c, "-").await?.into_iter().next() else {
            // An incomplete value store is worse than none: hydrating a
            // missing DataKey is an engine error, not a cache miss.
            println!("[dice-bank] container {c} missing - cold dice load");
            let _ = std::fs::remove_dir_all(r.dice_dir.join("db"));
            let _ = std::fs::remove_file(graph_meta(r.dice_dir));
            return Ok(None);
        };
        gh.download_to(a.id, &seg_dir).await?;
        let dirs: Vec<PathBuf> = std::fs::read_dir(&seg_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        merged += merge(&r.dice_dir.join("db"), &dirs)?;
    }
    let _ = std::fs::remove_dir_all(&seg_dir);
    println!(
        "[dice-bank] merged {merged} segments into {}/db",
        r.dice_dir.display()
    );
    Ok(Some(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seed_gate_is_the_manifest_name() {
        // A fork-rev or seed change must orphan the bank rather than
        // serve rows derived under a different graph.
        let a = manifest_name("lin", "rev1-sweep-treehash1");
        let b = manifest_name("lin", "rev2-sweep-treehash1");
        assert_ne!(a, b, "a seed change must change the manifest name");
        assert!(a.starts_with("cas-manifest-lin-dice-"));
        assert_eq!(seed8("rev1-sweep-treehash1").len(), 8);
    }

    #[test]
    fn rows_round_trip_through_their_text_form() {
        let r = Row::parse("3 -200 19 0BADF00D").unwrap();
        assert_eq!((r.shard, r.key_hi, r.key_lo), (3, -200, 19));
        assert_eq!(r.value_hex, "0BADF00D");
        assert_eq!(r.line(), "3 -200 19 0BADF00D");
        assert_eq!(r.key(), "-200 19", "the diff key is (key_hi, key_lo)");
        // A negative key_hi is real: sqlite stores these as signed i64 and
        // awk's arithmetic could not be trusted with them.
        assert!(Row::parse("bad").is_none());
    }
}
