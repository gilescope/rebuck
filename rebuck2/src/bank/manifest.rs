//! Manifests: the bank's index, and every decision made from it.
//!
//! This was `jq`-in-bash, which cost us a specific run of bugs that are
//! structurally impossible here: `jq.exe` emitting CRLF into segment names
//! so every `[ -d ]` test failed but the last (run 29491383253); native jq
//! unable to open MSYS `/proc/N/fd` process-substitution paths (run
//! 29486020160); `--jq` silently accepting no `--arg`. Typed JSON in, typed
//! JSON out.
//!
//! A manifest names the segments of one lineage at one generation, plus the
//! full blob/row list of the bank at that generation. For the AC bank a
//! "blob" line is `<store-relative-path> <sha256(content)>` - rows are
//! name-stable but content-mutable, so identity is the pair.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::zstd;

/// One packed segment, as referenced by a manifest.
///
/// `run` and `role` exist for the AC bank's cross-role apply order and are
/// absent on blob-bank segments; `full` marks a compaction pack, which
/// `needs_compaction` must not count as delta or the trigger re-fires every
/// lap (run 29589478222: 70 full packs re-compacting 1.3GB).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub name: String,
    pub bytes: u64,
    pub blobs: u64,
    pub prefixes: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<bool>,
}

/// Every scalar defaults: a manifest is read from an artifact that may
/// have been written by an older generation (or, in the test suites, by
/// hand) and a missing `created_by_run` must not fail a lap when the only
/// thing being asked is which segments to fetch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub version: u32,
    pub lineage: String,
    pub generation: String,
    pub parent_lineage: Option<String>,
    pub parent_generation: Option<String>,
    pub created_by_run: u64,
    pub segments: Vec<Segment>,
}

impl Manifest {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
        // Tolerate a BOM/CRLF-mangled file from an older jq-written
        // generation rather than failing a lap on it.
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .with_context(|| format!("{}: not a manifest", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        Ok(fs::write(path, serde_json::to_string(self)? + "\n")?)
    }

    /// Segments whose prefix bitmap overlaps `owned` (e.g. "89"); `*`
    /// fetches everything.
    ///
    /// Absence in the bitmap is a guarantee (no blob in that range),
    /// presence is a maybe - over-fetch on overlap is accepted, and
    /// compaction re-bins by prefix to pay it down.
    pub fn segments_to_fetch(&self, owned: &str) -> Vec<&Segment> {
        if owned == "*" {
            return self.segments.iter().collect();
        }
        self.segments
            .iter()
            .filter(|s| s.prefixes.chars().any(|p| owned.contains(p)))
            .collect()
    }
}

/// Compaction thresholds, all tunable. Defaults are the blob bank's; the
/// AC bank passes its own (its whole row set is ~22MB, so a 256MB floor
/// would never fire).
pub struct CompactCfg {
    pub delta_pct: u64,
    pub hysteresis_pct: u64,
    pub min_mb: u64,
    pub max_segments: usize,
}

impl Default for CompactCfg {
    fn default() -> Self {
        Self {
            delta_pct: 20,
            hysteresis_pct: 5,
            min_mb: 256,
            max_segments: 64,
        }
    }
}

impl CompactCfg {
    /// Env override, so the shell callers keep the knobs they document.
    pub fn from_env() -> Self {
        let d = Self::default();
        let get = |k: &str, or: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(or)
        };
        Self {
            delta_pct: get("COMPACT_DELTA_PCT", d.delta_pct),
            hysteresis_pct: get("COMPACT_HYSTERESIS_PCT", d.hysteresis_pct),
            min_mb: get("COMPACT_MIN_MB", d.min_mb),
            max_segments: get("COMPACT_MAX_SEGMENTS", d.max_segments as u64) as usize,
        }
    }
}

/// "yes <reason>" or "no" - should this lineage re-pack itself in full?
///
/// The hysteresis exists so a bank hovering at the boundary does not
/// compact on alternate laps. The segment cap counts DELTA segments only:
/// a big range legitimately needs many full packs, and counting those
/// re-fired the trigger every lap.
pub fn needs_compaction(m: &Manifest, cfg: &CompactCfg) -> String {
    let is_full = |s: &&Segment| s.full == Some(true);
    let full_bytes: u64 = m.segments.iter().filter(is_full).map(|s| s.bytes).sum();
    let delta: Vec<&Segment> = m.segments.iter().filter(|s| !is_full(s)).collect();
    let delta_bytes: u64 = delta.iter().map(|s| s.bytes).sum();

    if delta.len() > cfg.max_segments {
        return format!("yes segments={}>max={}", delta.len(), cfg.max_segments);
    }
    if delta_bytes < cfg.min_mb * 1024 * 1024 {
        return "no".into();
    }
    if full_bytes == 0 {
        return format!("yes cold-bank delta={delta_bytes}B");
    }
    let threshold = full_bytes * (cfg.delta_pct + cfg.hysteresis_pct) / 100;
    if delta_bytes > threshold {
        format!(
            "yes delta={delta_bytes}B>{}%of={full_bytes}B",
            cfg.delta_pct + cfg.hysteresis_pct
        )
    } else {
        "no".into()
    }
}

/// Assemble the next generation: head's segments plus the newly packed
/// ones, and the union of their blob lists.
///
/// `head` is the unpacked previous manifest artifact (or None for a cold
/// bank / a compaction, which references only its fresh full packs).
#[allow(clippy::too_many_arguments)]
pub fn write_manifest(
    lineage: &str,
    generation: &str,
    parent_lineage: Option<&str>,
    parent_generation: Option<&str>,
    run_id: u64,
    head: Option<&Path>,
    segs_dir: &Path,
    out: &Path,
) -> Result<()> {
    let mut segments = Vec::new();
    let mut blobs: Vec<String> = Vec::new();

    if let Some(head) = head {
        let hm = head.join("manifest.json");
        if hm.is_file() {
            segments.extend(Manifest::read(&hm)?.segments);
            let list = head.join("blobs.txt.zst");
            if list.is_file() {
                blobs.extend(zstd::read_lines(&list)?);
            }
        }
    }

    // Directory order is filesystem order; sort so a manifest is a
    // function of its inputs and two nodes packing the same set agree.
    let mut new_dirs: Vec<_> = fs::read_dir(segs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("cas-seg-"))
        })
        .collect();
    new_dirs.sort();
    for d in new_dirs {
        let meta = d.join("meta.json");
        if !meta.is_file() {
            continue;
        }
        segments.push(
            serde_json::from_str(&fs::read_to_string(&meta)?)
                .with_context(|| format!("{}: not a segment meta", meta.display()))?,
        );
        let list = d.join("blobs.txt.zst");
        if list.is_file() {
            blobs.extend(zstd::read_lines(&list)?);
        }
    }

    fs::create_dir_all(out)?;
    Manifest {
        version: 1,
        lineage: lineage.into(),
        generation: generation.into(),
        parent_lineage: parent_lineage.filter(|s| *s != "-").map(Into::into),
        parent_generation: parent_generation.filter(|s| *s != "-").map(Into::into),
        created_by_run: run_id,
        segments,
    }
    .write(&out.join("manifest.json"))?;

    blobs.sort();
    blobs.dedup();
    zstd::write_lines(&out.join("blobs.txt.zst"), &blobs)
}

/// Collapse an AC row list to newest-per-path.
///
/// `write_manifest` unions line-wise, so a mutated row leaves both
/// `(path, old-hash)` and `(path, new-hash)` behind and the list grows with
/// history rather than with the row set. Later lines win, so the caller
/// appends this lap's rows last.
pub fn collapse_rows(lines: &[String]) -> Vec<String> {
    let mut newest: std::collections::BTreeMap<&str, &str> = Default::default();
    for l in lines {
        if let Some((path, hash)) = l.split_once(' ') {
            newest.insert(path, hash);
        }
    }
    newest
        .into_iter()
        .map(|(p, h)| format!("{p} {h}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(name: &str, bytes: u64, prefixes: &str, full: bool) -> Segment {
        Segment {
            name: name.into(),
            bytes,
            blobs: 1,
            prefixes: prefixes.into(),
            artifact: None,
            run: None,
            role: None,
            full: full.then_some(true),
        }
    }

    fn manifest(segments: Vec<Segment>) -> Manifest {
        Manifest {
            version: 1,
            lineage: "lin-a".into(),
            generation: "gen-1".into(),
            parent_lineage: None,
            parent_generation: None,
            created_by_run: 1,
            segments,
        }
    }

    #[test]
    fn prefix_bitmap_skip_is_certain_overlap_fetches() {
        let m = manifest(vec![
            seg("cas-seg-1", 10, "129", false),
            seg("cas-seg-2", 10, "e", false),
        ]);
        let names = |owned: &str| {
            m.segments_to_fetch(owned)
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("9"), ["cas-seg-1"]);
        assert_eq!(names("e"), ["cas-seg-2"]);
        assert!(names("45").is_empty(), "no overlap must fetch nothing");
        assert_eq!(names("*").len(), 2, "wildcard fetches all");
    }

    #[test]
    fn compaction_triggers_floor_cold_bank_and_delta_only_cap() {
        let m = manifest(vec![seg("a", 10, "0", false), seg("b", 10, "1", false)]);

        let cfg = CompactCfg {
            min_mb: 0,
            ..Default::default()
        };
        assert!(
            needs_compaction(&m, &cfg).starts_with("yes cold-bank"),
            "no full packs yet: any delta above the floor compacts"
        );

        let cfg = CompactCfg {
            min_mb: 999_999,
            ..Default::default()
        };
        assert_eq!(needs_compaction(&m, &cfg), "no", "floor suppresses");

        let cfg = CompactCfg {
            min_mb: 999_999,
            max_segments: 1,
            ..Default::default()
        };
        assert!(
            needs_compaction(&m, &cfg).starts_with("yes segments="),
            "the segment cap fires regardless of the byte floor"
        );

        // 70 FULL packs are the compacted steady state, not churn: counting
        // them re-fired the trigger every lap and re-packed 1.3GB.
        let all_full = manifest(
            (0..70)
                .map(|i| seg(&format!("f{i}"), 1000, "0", true))
                .collect(),
        );
        let cfg = CompactCfg {
            min_mb: 999_999,
            max_segments: 64,
            ..Default::default()
        };
        assert_eq!(
            needs_compaction(&all_full, &cfg),
            "no",
            "full packs must never count toward the delta cap"
        );
    }

    #[test]
    fn hysteresis_keeps_a_boundary_bank_from_alternating() {
        // delta exactly at 20% of full: without hysteresis this compacts,
        // with it (25%) it must not.
        let m = manifest(vec![
            seg("full", 1000 * 1024 * 1024, "0", true),
            seg("delta", 200 * 1024 * 1024, "1", false),
        ]);
        let cfg = CompactCfg {
            min_mb: 1,
            ..Default::default()
        };
        assert_eq!(needs_compaction(&m, &cfg), "no", "20% is inside 20%+5%");
    }

    #[test]
    fn rows_collapse_to_newest_per_path() {
        let rows = vec![
            "ac/aaa hash-old".to_string(),
            "acn/bb/ccc hash-c".to_string(),
            "ac/aaa hash-new".to_string(),
        ];
        assert_eq!(
            collapse_rows(&rows),
            ["ac/aaa hash-new", "acn/bb/ccc hash-c"],
            "later lines win, output is path-sorted"
        );
    }

    #[test]
    fn manifest_round_trips_and_omits_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        let m = manifest(vec![seg("cas-seg-x", 5, "0", false)]);
        m.write(&p).unwrap();

        let text = fs::read_to_string(&p).unwrap();
        assert!(
            !text.contains("\"full\"") && !text.contains("\"artifact\""),
            "absent optional fields must not appear as nulls: {text}"
        );
        assert!(
            text.contains("\"parent_lineage\":null"),
            "parent is explicit"
        );

        let back = Manifest::read(&p).unwrap();
        assert_eq!(back.segments.len(), 1);
        assert_eq!(back.segments[0].name, "cas-seg-x");
    }
}
