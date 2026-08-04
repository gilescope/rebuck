//! CAS bank restore and publish: the federated blob bank's choreography.
//!
//! Eight ranges, one manifest each, written only by that range's PRIMARY
//! owner - so there is no banker job and no lap-wide failure mode. A node
//! banks its own range and SPILLS everything else for the owners to absorb
//! on a later restore, where the ordinary diff banks it properly.
//!
//! See `ci/cas-bank-design.md`. The shell this replaces is the reason the
//! design doc has a "gotchas already paid for" section.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::manifest::Manifest;
use super::publish as policy;
use super::{pack, zstd};
use crate::github::Client;

/// Working state a restore leaves for the matching publish.
pub struct Work {
    pub dir: PathBuf,
}

impl Work {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }
    pub fn blobs(&self) -> PathBuf {
        self.dir.join("bank-blobs.txt")
    }
    pub fn own_range(&self) -> PathBuf {
        self.dir.join("own-range")
    }
    pub fn own_unknown(&self) -> PathBuf {
        self.dir.join(".own-range-unknown")
    }
    pub fn oldest_container(&self) -> PathBuf {
        self.dir.join(".oldest-container")
    }
    pub fn delta_secs(&self) -> PathBuf {
        self.dir.join(".delta-restore-secs")
    }
    fn parent_manifest(&self, shard: u8) -> PathBuf {
        self.dir.join(format!("parent-manifest-r{shard}.json"))
    }
}

/// Hex prefixes a shard owns: shard n owns 2n and 2n+1.
pub fn owned_prefixes(shard: u8) -> String {
    format!("{:x}{:x}", shard * 2, shard * 2 + 1)
}

fn manifest_name(lineage: &str, shard: u8) -> String {
    format!("cas-manifest-{lineage}-r{shard}")
}

pub struct Restore<'a> {
    pub store: &'a Path,
    /// `None` = blob-list only (driver/co-worker: they own no range).
    pub shard: Option<u8>,
    pub lineage: &'a str,
    pub parent: Option<&'a str>,
    pub absorb_spills: bool,
}

/// Returns segments seeded, or `None` for a cold bank.
pub async fn restore(r: Restore<'_>, work: &Work) -> Result<Option<u64>> {
    let gh = Client::from_env()?;
    let _ = std::fs::remove_file(work.own_unknown());

    let mut union: Vec<String> = Vec::new();
    let (mut found, mut inherited) = (0u32, 0u32);
    let mut own_created: Option<String> = None;
    let scratch = work.dir.join(".m");

    for n in 0..8u8 {
        let is_own = r.shard == Some(n);
        let name = manifest_name(r.lineage, n);
        let row = match gh.by_name(&name, r.lineage).await {
            Ok(v) => v.into_iter().next(),
            Err(e) if is_own => {
                // For the OWN range a lookup ERROR must not read as
                // "absent": the publish would stage a thin manifest and
                // newest-wins would clobber the fat one - monotonicity
                // broken by a network flake.
                println!(
                    "[bank] WARN own-range manifest lookup FAILED ({e}) - publish will spill-only"
                );
                std::fs::write(work.own_unknown(), b"")?;
                continue;
            }
            Err(_) => None,
        };
        let Some(art) = row else { continue };
        gh.download_to(art.id, &scratch).await?;
        let mf = scratch.join("manifest.json");
        if !mf.is_file() {
            continue;
        }
        std::fs::copy(&mf, work.dir.join(format!("bank-manifest-r{n}.json")))?;
        let list = scratch.join("blobs.txt.zst");
        if list.is_file() {
            union.extend(zstd::read_lines(&list)?);
        }
        found += 1;
        if is_own {
            own_created = Some(art.created_at.clone());
            let head = work.own_range();
            let _ = std::fs::remove_dir_all(&head);
            std::fs::create_dir_all(&head)?;
            std::fs::copy(&mf, head.join("manifest.json"))?;
            if list.is_file() {
                std::fs::copy(&list, head.join("blobs.txt.zst"))?;
            }
        }
    }

    // Parent lineage: read-only. Its blobs join the union so this lap
    // never re-banks them and its segments seed the store, but every
    // publish still goes to the CHILD's manifest. On merge the trunk
    // re-derives under its own trust.
    if let Some(parent) = r.parent.filter(|p| *p != r.lineage) {
        for n in 0..8u8 {
            let Some(art) = gh
                .by_name(&manifest_name(parent, n), parent)
                .await?
                .into_iter()
                .next()
            else {
                continue;
            };
            gh.download_to(art.id, &scratch).await?;
            let mf = scratch.join("manifest.json");
            if !mf.is_file() {
                continue;
            }
            std::fs::copy(&mf, work.parent_manifest(n))?;
            let list = scratch.join("blobs.txt.zst");
            if list.is_file() {
                union.extend(zstd::read_lines(&list)?);
            }
            inherited += 1;
        }
        if inherited > 0 {
            println!("[bank] parent lineage {parent}: {inherited} manifests inherited");
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    if found + inherited == 0 {
        println!("[bank] no range manifests for {} - cold bank", r.lineage);
        return Ok(None);
    }
    union.sort();
    union.dedup();
    std::fs::write(work.blobs(), union.join("\n") + "\n")?;
    println!(
        "[bank] {found} own + {inherited} inherited manifests, union {} blobs",
        union.len()
    );

    let Some(shard) = r.shard else {
        return Ok(Some(0));
    };
    let owned = owned_prefixes(shard);

    let mut seeded = 0u64;
    std::fs::create_dir_all(r.store)?;
    // The parent's range first (the branch's warm base), then this
    // lineage's own on top. Content-addressed, so order only decides who
    // does the bulk fetch.
    for m in [
        work.parent_manifest(shard),
        work.own_range().join("manifest.json"),
    ] {
        if m.is_file() {
            seeded += seed_from_manifest(&gh, &m, &owned, r.store, work).await?;
        }
    }
    println!("[bank] seeded {seeded} segments for range {owned}");

    if r.absorb_spills {
        absorb_spills(
            &gh,
            r.lineage,
            &owned,
            r.store,
            work,
            own_created.as_deref(),
        )
        .await?;
    }
    heal_exec_bits(r.store)?;
    Ok(Some(seeded))
}

/// Fetch each container named by `manifest` once and seed its segments.
async fn seed_from_manifest(
    gh: &Client,
    manifest: &Path,
    owned: &str,
    store: &Path,
    work: &Work,
) -> Result<u64> {
    let m = Manifest::read(manifest)?;
    let needed = m.segments_to_fetch(owned);
    if needed.is_empty() {
        return Ok(0);
    }
    let mut by_container: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for s in &needed {
        if let Some(a) = s.artifact.as_deref() {
            by_container.entry(a).or_default().push(&s.name);
        }
    }

    let mut seeded = 0;
    let seg_dir = work.dir.join(".seg");
    for (container, segments) in by_container {
        // Autotune input: wall seconds spent fetching DELTA containers.
        // Full packs are the post-compaction steady state, so their cost
        // is not what compaction can reclaim.
        let start = std::time::Instant::now();
        let is_full = m
            .segments
            .iter()
            .filter(|s| s.artifact.as_deref() == Some(container))
            .all(|s| s.full == Some(true));

        // Containers carry no provenance of their own - the manifest
        // naming them is the trust anchor, and the store is hash-verified
        // after seeding anyway.
        let Some(art) = gh.by_name(container, "-").await?.into_iter().next() else {
            // Referenced but missing: degrade to re-execution (those
            // actions miss the cache) rather than failing the lap.
            println!("[bank] WARN container {container} missing - its blobs will re-derive");
            continue;
        };
        // Oldest referenced container feeds publish's rewarm check.
        let older = std::fs::read_to_string(work.oldest_container())
            .ok()
            .is_none_or(|o| art.created_at.as_str() < o.trim());
        if older {
            std::fs::write(work.oldest_container(), &art.created_at)?;
        }

        gh.download_to(art.id, &seg_dir).await?;
        let dirs: Vec<PathBuf> = segments
            .iter()
            .map(|n| seg_dir.join(n))
            .filter(|d| d.is_dir())
            .collect();
        seeded += pack::seed_store(store, &dirs)?;

        if !is_full {
            let prev: u64 = std::fs::read_to_string(work.delta_secs())
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            std::fs::write(
                work.delta_secs(),
                (prev + start.elapsed().as_secs()).to_string(),
            )?;
        }
    }
    let _ = std::fs::remove_dir_all(&seg_dir);
    Ok(seeded)
}

/// Absorb recent spills into this range (primaries only).
///
/// Out-of-range blobs other nodes produced land in `cas-spill-*` until
/// their owner seeds them; the owner's next publish then diffs them as new
/// and banks them properly. Absorption is a side effect of the ordinary
/// pack, not extra machinery.
async fn absorb_spills(
    gh: &Client,
    lineage: &str,
    owned: &str,
    store: &Path,
    work: &Work,
    since: Option<&str>,
) -> Result<()> {
    let cutoff = since.unwrap_or("1970-01-01T00:00:00Z");
    let mut spills: Vec<_> = gh
        .by_prefix(&format!("cas-spill-{lineage}-"), lineage)
        .await?
        .into_iter()
        .filter(|a| a.created_at.as_str() > cutoff)
        .collect();
    spills.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    spills.truncate(40);

    let dir = work.dir.join(".spill");
    let staging = work.dir.join(".spill-x");
    let mut absorbed = 0u64;
    for art in spills {
        gh.download_to(art.id, &dir).await?;
        for seg in std::fs::read_dir(&dir)?.flatten() {
            let tarball = seg.path().join("bulk.tar.zst");
            if !tarball.is_file() {
                continue;
            }
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)?;
            zstd::extract_tar(&tarball, &staging)?;
            // Only this range moves into the store: seeding foreign
            // prefixes would make a spill-only node re-spill them.
            let cas = staging.join("cas");
            if !cas.is_dir() {
                continue;
            }
            for d in std::fs::read_dir(&cas)?.flatten() {
                let name = d.file_name().to_string_lossy().into_owned();
                // Store dirs are TWO hex chars; the range is the first.
                if !name.chars().next().is_some_and(|c| owned.contains(c)) {
                    continue;
                }
                let dest = store.join("cas").join(&name);
                std::fs::create_dir_all(&dest)?;
                for f in std::fs::read_dir(d.path())?.flatten() {
                    std::fs::copy(f.path(), dest.join(f.file_name()))?;
                }
                absorbed += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&staging);
    println!("[bank] absorbed own-range dirs from {absorbed} spill segments since {cutoff}");
    Ok(())
}

/// Heal store files packed before the tar writer stamped 0755.
///
/// rebuck2 hardlinks store files into exec dirs, so a 0644 build script
/// dies with EACCES - lap 29507595376's 29 linux "failures" were that one
/// bit. Matches nothing once pre-fix segments compact away.
fn heal_exec_bits(store: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let cas = store.join("cas");
        if !cas.is_dir() {
            return Ok(());
        }
        for d in std::fs::read_dir(&cas)?.flatten() {
            if !d.file_type()?.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(d.path())?.flatten() {
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() || meta.permissions().mode() & 0o111 != 0 {
                    continue;
                }
                let mut p = meta.permissions();
                p.set_mode(meta.permissions().mode() | 0o111);
                let _ = std::fs::set_permissions(f.path(), p);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = store;
    Ok(())
}

pub struct Publish<'a> {
    pub store: &'a Path,
    pub role: &'a str,
    /// The range this node is PRIMARY for; `None` = spill-only.
    pub shard: Option<u8>,
    pub lineage: &'a str,
    pub parent: Option<&'a str>,
    pub run: &'a str,
}

#[derive(Debug, Default)]
pub struct Staged {
    pub container: bool,
    pub manifest: bool,
    pub spill: bool,
}

pub fn publish(p: Publish<'_>, work: &Work) -> Result<Staged> {
    let container_name = format!("cas-segs-{}-{}-{}", p.lineage, p.run, p.role);
    let run_num: u64 = p.run.parse().unwrap_or(0);
    let segs = work.dir.join("bank-segs");
    let container_dir = work.dir.join("bank-container");
    let manifest_dir = work.dir.join("bank-manifest-out");
    let spill_segs = work.dir.join("bank-spill-segs");
    let spill_dir = work.dir.join("bank-spill");
    for d in [
        &segs,
        &container_dir,
        &manifest_dir,
        &spill_segs,
        &spill_dir,
    ] {
        let _ = std::fs::remove_dir_all(d);
    }

    // A failed own-manifest lookup at restore must not let this lap stage
    // a thin manifest that newest-wins would put over the fat one: demote
    // to spill-only and let the owner re-bank from spill next lap.
    let mut shard = p.shard;
    if work.own_unknown().is_file() && shard.is_some() {
        println!(
            "[bank] {} r{}: own manifest state unknown - spill-only lap",
            p.role,
            shard.unwrap()
        );
        shard = None;
    }

    let banked = super::read_list(&work.blobs())?;
    let mut out = Staged::default();

    if let Some(n) = shard {
        let owned = owned_prefixes(n);
        let head_dir = work.own_range();
        let head = head_dir
            .join("manifest.json")
            .is_file()
            .then(|| Manifest::read(&head_dir.join("manifest.json")))
            .transpose()?;

        let evidence = policy::RestoreEvidence {
            delta_restore_secs: std::fs::read_to_string(work.delta_secs())
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            oldest_container: std::fs::read_to_string(work.oldest_container())
                .ok()
                .map(|s| s.trim().to_owned()),
        };
        let pol = policy::CompactPolicy::from_env();
        let mut compacting = pol.decide(head.as_ref(), &evidence, policy::now_secs()?);
        if let Some(c) = &compacting {
            println!("[bank] {} r{n}: COMPACTING ({})", p.role, c.reason());
        }

        let base: &[String] = if compacting.is_some() { &[] } else { &banked };
        let mut names = pack::pack_segments(p.store, base, &segs, &owned, compacting.is_some())?;

        // Monotonicity gate: a full re-pack that would SHED blobs (a
        // referenced container failed to restore) falls back to a delta,
        // which can only add.
        if compacting.is_some() && !names.is_empty() {
            let old_list = head_dir.join("blobs.txt.zst");
            if old_list.is_file() {
                let old = zstd::read_lines(&old_list)?.len();
                let mut new: Vec<String> = Vec::new();
                for n in &names {
                    new.extend(zstd::read_lines(&segs.join(n).join("blobs.txt.zst"))?);
                }
                new.sort();
                new.dedup();
                if policy::would_shed(old, new.len()) {
                    println!(
                        "[bank] {} r{n}: compact would shed blobs ({old} -> {}) - delta instead",
                        p.role,
                        new.len()
                    );
                    compacting = None;
                    let _ = std::fs::remove_dir_all(&segs);
                    names = pack::pack_segments(p.store, &banked, &segs, &owned, false)?;
                }
            }
        }

        if names.is_empty() {
            println!("[bank] {} r{n}: nothing new in range {owned}", p.role);
        } else {
            std::fs::create_dir_all(&container_dir)?;
            for name in &names {
                let from = segs.join(name);
                let to = container_dir.join(name);
                std::fs::create_dir_all(&to)?;
                std::fs::rename(from.join("bulk.tar.zst"), to.join("bulk.tar.zst"))?;
                let mut seg: super::manifest::Segment =
                    serde_json::from_str(&std::fs::read_to_string(from.join("meta.json"))?)?;
                seg.artifact = Some(container_name.clone());
                seg.full = compacting.is_some().then_some(true);
                std::fs::write(from.join("meta.json"), serde_json::to_string(&seg)? + "\n")?;
            }
            super::manifest::write_manifest(
                p.lineage,
                &format!("{}-1", p.run),
                p.parent,
                head.as_ref().map(|h| h.generation.as_str()),
                run_num,
                (compacting.is_none() && head.is_some()).then_some(head_dir.as_path()),
                &segs,
                &manifest_dir,
            )?;
            println!(
                "[bank] {} r{n}: {} segments in {container_name}; manifest gen {}-1 staged",
                p.role,
                names.len(),
                p.run
            );
            out.container = true;
            out.manifest = true;
        }
        let _ = std::fs::remove_dir_all(&segs);
    }

    // Everything outside the owned range spills for its owners to absorb.
    let spillset: String = "0123456789abcdef"
        .chars()
        .filter(|c| shard.is_none_or(|n| !owned_prefixes(n).contains(*c)))
        .collect();
    let spilled = pack::pack_segments(p.store, &banked, &spill_segs, &spillset, false)?;
    if spilled.is_empty() {
        println!("[bank] {}: nothing to spill", p.role);
    } else {
        std::fs::create_dir_all(&spill_dir)?;
        for n in &spilled {
            std::fs::rename(spill_segs.join(n), spill_dir.join(n))?;
        }
        println!(
            "[bank] {}: {} spill segments (out-of-range, owners absorb)",
            p.role,
            spilled.len()
        );
        out.spill = true;
    }
    let _ = std::fs::remove_dir_all(&spill_segs);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shard_owns_two_adjacent_prefixes() {
        assert_eq!(owned_prefixes(0), "01");
        assert_eq!(owned_prefixes(3), "67");
        assert_eq!(owned_prefixes(7), "ef");
        // Every prefix is owned exactly once across the eight ranges.
        let all: String = (0..8u8).map(owned_prefixes).collect();
        let mut chars: Vec<char> = all.chars().collect();
        chars.sort_unstable();
        assert_eq!(chars.iter().collect::<String>(), "0123456789abcdef");
    }

    #[test]
    fn spill_is_exactly_the_complement_of_the_owned_range() {
        for n in 0..8u8 {
            let owned = owned_prefixes(n);
            let spill: String = "0123456789abcdef"
                .chars()
                .filter(|c| !owned.contains(*c))
                .collect();
            assert_eq!(spill.len(), 14, "range {n}");
            assert!(
                !spill.chars().any(|c| owned.contains(c)),
                "a blob must never be both banked and spilled"
            );
        }
    }
}
