//! AC bank restore: the whole choreography, not just its primitives.
//!
//! Every node banks the rows it AUTHORED into its own role manifest, so a
//! restore is a union across roles - and, for a branch lineage, across its
//! parent too. See `ci/ac-bank-plan.md`.
//!
//! This exists in Rust rather than shell because it is the same on every
//! runner. The bash it replaces needed `cygpath` for store paths, a
//! `timeout`/`gtimeout` fork for macOS, `tr -d '\r'` after every `jq.exe`
//! call, and real temp files because process substitution cannot cross
//! into a native windows binary. None of that is a property of the
//! problem; all of it is a property of writing it in shell.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::manifest::Manifest;
use super::pack;
use crate::github::Client;

/// Where a restore leaves its working state for the matching publish.
pub struct Work {
    pub dir: PathBuf,
}

impl Work {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }
    pub fn banked_rows(&self) -> PathBuf {
        self.dir.join("ac-banked-rows.txt")
    }
    pub fn own_head(&self) -> PathBuf {
        self.dir.join("own-ac")
    }
    pub fn own_unknown(&self) -> PathBuf {
        self.dir.join(".ac-own-unknown")
    }
    pub fn oldest_container(&self) -> PathBuf {
        self.dir.join(".ac-oldest-container")
    }
}

/// One segment to lay down, with everything the ordering needs.
struct Planned {
    rank: u8,
    run: u64,
    role_key: String,
    container: String,
    segment: String,
}

/// Rows are name-stable but content-MUTABLE, so the apply order must be
/// TOTAL and deterministic.
///
/// `(lineage, run, role)` with the parent lineage FIRST and the driver
/// LAST within a run: the driver's row is the normalized one and it is the
/// only node that serves. Ordering by run alone would let a trunk that
/// published later win over the branch built on top of it.
fn sort_key(p: &Planned) -> (u8, u64, &str, &str) {
    (p.rank, p.run, p.role_key.as_str(), p.segment.as_str())
}

fn role_of(name: &str) -> String {
    name.rsplit_once("-ac-")
        .map(|(_, r)| r.to_owned())
        .unwrap_or_default()
}

/// Sorts the driver last within a generation.
fn role_key(role: &str) -> String {
    if role == "driver" {
        "zzzz-driver".into()
    } else {
        role.into()
    }
}

pub struct Restore<'a> {
    pub store: &'a Path,
    pub role: &'a str,
    /// `true` = lay down every role's rows (the driver, the only reader).
    /// `false` = this role's own history only, which is all a worker needs
    /// for a compaction re-pack - and inheriting rows it did not author
    /// would invite it to re-bank the trunk.
    pub all_roles: bool,
    pub lineage: &'a str,
    pub parent: Option<&'a str>,
}

/// Returns the number of segments seeded, or `None` for a cold bank.
pub async fn restore(r: Restore<'_>, work: &Work) -> Result<Option<u64>> {
    let gh = Client::from_env()?;
    let prefix = format!("cas-manifest-{}-ac-", r.lineage);

    let _ = std::fs::remove_file(work.own_unknown());

    // A lookup ERROR must not read as "absent": the publish would stage a
    // thin manifest and newest-wins would put it over the fat one.
    let own = match gh.by_name(&format!("{prefix}{}", r.role), r.lineage).await {
        Ok(v) => v.into_iter().next(),
        Err(e) => {
            println!("[ac-bank] WARN own manifest lookup FAILED ({e}) - publish will not stage");
            std::fs::write(work.own_unknown(), b"")?;
            None
        }
    };

    // (rank, artifact) pairs to read. Rank 0 is the parent lineage.
    let mut sources: Vec<(u8, crate::github::Artifact)> = Vec::new();
    if r.all_roles {
        if let Some(parent) = r.parent.filter(|p| *p != r.lineage) {
            for a in gh
                .by_prefix(&format!("cas-manifest-{parent}-ac-"), parent)
                .await?
            {
                sources.push((0, a));
            }
        }
        for a in gh.by_prefix(&prefix, r.lineage).await? {
            sources.push((1, a));
        }
    }
    if let Some(o) = own {
        if !sources.iter().any(|(_, a)| a.name == o.name) {
            sources.push((1, o));
        }
    }

    if sources.is_empty() {
        println!("[ac-bank] no AC manifests for {} - cold bank", r.lineage);
        return Ok(None);
    }

    let mut rows: Vec<String> = Vec::new();
    let mut plan: Vec<Planned> = Vec::new();
    let (mut found, mut inherited) = (0u32, 0u32);
    let scratch = work.dir.join(".acm");

    for (rank, art) in &sources {
        gh.download_to(art.id, &scratch).await?;
        let mf = scratch.join("manifest.json");
        if !mf.is_file() {
            continue;
        }
        let m = Manifest::read(&mf)?;
        let list = scratch.join("blobs.txt.zst");
        if list.is_file() {
            rows.extend(super::zstd::read_lines(&list)?);
        }
        if *rank == 0 {
            inherited += 1;
        } else {
            found += 1;
        }

        let role = role_of(&art.name);
        for s in &m.segments {
            // Segments carry the run/role that PACKED them; a manifest's
            // own generation says nothing about its inherited segments.
            plan.push(Planned {
                rank: *rank,
                run: s.run.unwrap_or(0),
                role_key: role_key(s.role.as_deref().unwrap_or(&role)),
                container: s.artifact.clone().unwrap_or_default(),
                segment: s.name.clone(),
            });
        }
        if *rank == 1 && role == r.role {
            let head = work.own_head();
            let _ = std::fs::remove_dir_all(&head);
            std::fs::create_dir_all(&head)?;
            std::fs::copy(&mf, head.join("manifest.json"))?;
            if list.is_file() {
                std::fs::copy(&list, head.join("blobs.txt.zst"))?;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    rows.sort();
    rows.dedup();
    std::fs::write(work.banked_rows(), rows.join("\n") + "\n")?;
    println!(
        "[ac-bank] {found} role manifests ({inherited} inherited), union {} rows",
        rows.len()
    );

    plan.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

    // Group consecutively by container so each is fetched once, while the
    // apply order within it is preserved.
    let mut seeded = 0u64;
    let mut oldest: Option<String> = None;
    let mut i = 0;
    let seg_dir = work.dir.join(".acseg");
    while i < plan.len() {
        let container = plan[i].container.clone();
        let mut j = i;
        while j < plan.len() && plan[j].container == container {
            j += 1;
        }
        if container.is_empty() {
            i = j;
            continue;
        }
        // Containers carry no provenance of their own - the manifest that
        // names them is the trust anchor.
        let Some(art) = gh.by_name(&container, "-").await?.into_iter().next() else {
            // Referenced but missing: those rows re-derive. A missing AC
            // row is a cache miss, never a corruption.
            println!("[ac-bank] WARN container {container} missing - its rows re-derive");
            i = j;
            continue;
        };
        if oldest.as_ref().is_none_or(|o| art.created_at < *o) {
            oldest = Some(art.created_at.clone());
        }
        gh.download_to(art.id, &seg_dir).await?;
        for p in &plan[i..j] {
            let d = seg_dir.join(&p.segment);
            if d.is_dir() {
                seeded += pack::seed_store(r.store, &[d])?;
            }
        }
        i = j;
    }
    let _ = std::fs::remove_dir_all(&seg_dir);
    if let Some(o) = oldest {
        std::fs::write(work.oldest_container(), o)?;
    }
    println!(
        "[ac-bank] seeded {seeded} segments into {} (mode {})",
        r.store.display(),
        if r.all_roles { "all" } else { "own" }
    );

    // --cache-failures writes failure rows and the read path serves them
    // unconditionally, so a banked environmental failure replays forever.
    for sub in ["ac", "acn"] {
        let d = r.store.join(sub);
        if d.is_dir() {
            super::purge_failures(&d)?;
        }
    }
    Ok(Some(seeded))
}

/// Stage this node's new/changed rows for upload.
///
/// The counterpart to [`restore`]: same `BANK_WORK`, same role. Every node
/// banks the rows it AUTHORED under its own manifest, so publishing is
/// never a merge - it can only add to, or fully re-pack, this role's own
/// history.
pub struct Publish<'a> {
    pub store: &'a Path,
    pub role: &'a str,
    pub lineage: &'a str,
    pub parent: Option<&'a str>,
    pub run: &'a str,
}

/// What a publish staged, for a caller that wants to report it. The CLI
/// only needs to know that something WAS staged, so the fields are
/// informational.
#[derive(Debug)]
pub struct Staged {
    pub segments: usize,
}

pub fn publish(p: Publish<'_>, work: &Work) -> Result<Option<Staged>> {
    use super::publish as policy;

    let container = format!("cas-ac-segs-{}-{}-{}", p.lineage, p.run, p.role);
    let run_num: u64 = p.run.parse().unwrap_or(0);
    let segs_dir = work.dir.join("ac-segs");
    let container_dir = work.dir.join("ac-container");
    let manifest_dir = work.dir.join("ac-manifest-out");
    for d in [&segs_dir, &container_dir, &manifest_dir] {
        let _ = std::fs::remove_dir_all(d);
    }

    // A failed own-manifest lookup leaves this role's banked state
    // unknown; staging anything now risks newest-wins putting a thin
    // manifest over the fat one.
    if work.own_unknown().is_file() {
        println!(
            "[ac-bank] {}: own manifest state unknown - not staging",
            p.role
        );
        return Ok(None);
    }

    // Never bank poison: --cache-failures rows are useful WITHIN a lap and
    // fatal across laps, so they are purged here as well as at seed.
    for sub in ["ac", "acn"] {
        let d = p.store.join(sub);
        if d.is_dir() {
            super::purge_failures(&d)?;
        }
    }

    let banked = super::read_list(&work.banked_rows())?;
    let head_dir = work.own_head();
    let head = head_dir
        .join("manifest.json")
        .is_file()
        .then(|| Manifest::read(&head_dir.join("manifest.json")))
        .transpose()?;

    // The AC's row set is bounded by the action graph (~22MB fleet-wide),
    // so the blob bank's 256MB floor would never fire.
    let mut pol = policy::CompactPolicy::from_env();
    pol.cfg.min_mb = std::env::var("AC_COMPACT_MIN_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    pol.cfg.max_segments = std::env::var("AC_COMPACT_MAX_SEGMENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let evidence = policy::RestoreEvidence {
        delta_restore_secs: None,
        oldest_container: std::fs::read_to_string(work.oldest_container())
            .ok()
            .map(|s| s.trim().to_owned()),
    };
    let mut compacting = pol.decide(head.as_ref(), &evidence, policy::now_secs()?);
    if let Some(c) = &compacting {
        println!("[ac-bank] {}: COMPACTING ({})", p.role, c.reason());
    }

    let diff_base: &[String] = if compacting.is_some() { &[] } else { &banked };
    let mut names = pack::ac_pack(p.store, diff_base, &segs_dir, compacting.is_some())?;

    // Monotonicity gate: a full re-pack that would SHED rows (a referenced
    // container failed to restore) falls back to a delta, which can only
    // add - newest-wins must never lose history.
    if compacting.is_some() && !names.is_empty() {
        let old_list = head_dir.join("blobs.txt.zst");
        if old_list.is_file() {
            let old = super::zstd::read_lines(&old_list)?.len();
            let new: usize = names
                .iter()
                .map(|n| {
                    super::zstd::read_lines(&segs_dir.join(n).join("blobs.txt.zst"))
                        .map(|v| v.len())
                        .unwrap_or(0)
                })
                .sum();
            if policy::would_shed(old, new) {
                println!(
                    "[ac-bank] {}: compact would shed rows ({old} -> {new}) - delta instead",
                    p.role
                );
                compacting = None;
                let _ = std::fs::remove_dir_all(&segs_dir);
                names = pack::ac_pack(p.store, &banked, &segs_dir, false)?;
            }
        }
    }

    if names.is_empty() {
        println!("[ac-bank] {}: no new or changed rows", p.role);
        let _ = std::fs::remove_dir_all(&segs_dir);
        return Ok(None);
    }

    // Move the payload into the container dir and stamp each meta with the
    // container, run and role - the restore's fetch map and sort key.
    std::fs::create_dir_all(&container_dir)?;
    let mut fresh_rows: Vec<String> = Vec::new();
    for name in &names {
        let from = segs_dir.join(name);
        let to = container_dir.join(name);
        std::fs::create_dir_all(&to)?;
        std::fs::rename(from.join("bulk.tar.zst"), to.join("bulk.tar.zst"))?;
        let mut seg: super::manifest::Segment =
            serde_json::from_str(&std::fs::read_to_string(from.join("meta.json"))?)?;
        seg.artifact = Some(container.clone());
        seg.run = Some(run_num);
        seg.role = Some(p.role.to_owned());
        seg.full = compacting.is_some().then_some(true);
        std::fs::write(from.join("meta.json"), serde_json::to_string(&seg)? + "\n")?;
        fresh_rows.extend(super::zstd::read_lines(&from.join("blobs.txt.zst"))?);
    }

    let prev_gen = head.as_ref().map(|h| h.generation.clone());
    super::manifest::write_manifest(
        p.lineage,
        &format!("{}-1", p.run),
        p.parent,
        prev_gen.as_deref(),
        run_num,
        // A compacting manifest references ONLY its fresh full packs.
        (compacting.is_none() && head.is_some()).then_some(head_dir.as_path()),
        &segs_dir,
        &manifest_dir,
    )?;

    // The union is line-wise, so a mutated row leaves both its old and new
    // (path, hash) pairs behind; collapse to newest-per-path with this
    // lap's rows last so the list tracks the row set, not its history.
    let list = manifest_dir.join("blobs.txt.zst");
    let mut all = super::zstd::read_lines(&list)?;
    all.extend(fresh_rows);
    super::zstd::write_lines(&list, &super::manifest::collapse_rows(&all))?;

    println!(
        "[ac-bank] {}: {} segments in {container}; manifest gen {}-1 staged",
        p.role,
        names.len(),
        p.run
    );
    let _ = std::fs::remove_dir_all(&segs_dir);
    Ok(Some(Staged {
        segments: names.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(rank: u8, run: u64, role: &str, seg: &str) -> Planned {
        Planned {
            rank,
            run,
            role_key: role_key(role),
            container: "c".into(),
            segment: seg.into(),
        }
    }

    #[test]
    fn apply_order_is_lineage_then_run_then_driver_last() {
        let mut p = [
            planned(1, 900, "driver", "s-child-driver"),
            planned(0, 1002, "driver", "s-parent-late"),
            planned(1, 900, "Linux-w1", "s-child-worker"),
            planned(0, 1, "Linux-w1", "s-parent-early"),
        ];
        p.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let order: Vec<&str> = p.iter().map(|x| x.segment.as_str()).collect();
        assert_eq!(
            order,
            [
                "s-parent-early",
                "s-parent-late",
                "s-child-worker",
                "s-child-driver"
            ],
            "parent first even when it published LATER (run 1002 > 900); \
             driver last within a generation"
        );
    }

    #[test]
    fn role_is_parsed_from_the_manifest_name() {
        assert_eq!(role_of("cas-manifest-main-ac-driver"), "driver");
        assert_eq!(role_of("cas-manifest-main-ac-Linux-w1"), "Linux-w1");
        assert_eq!(
            role_of("cas-manifest-giles-rebuck2-sweep-ac-co-worker"),
            "co-worker",
            "a lineage containing dashes must not confuse the split"
        );
    }
}
