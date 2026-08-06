//! Bank primitives: the per-item hot paths of the CI store bank.
//!
//! These used to live in `ci/cas-bank-tool` in the buck2-fixups repo, which
//! meant the store format had two owners in two repos - and a dependency-free
//! tool there had to hand-roll SHA-256 and a protobuf varint reader that this
//! crate already has properly. They belong next to [`crate::store`], which
//! defines the layout they walk.
//!
//! Everything here is deterministic file munging. Artifact I/O (lookup,
//! download, upload) is deliberately NOT here.
//!
//! Verbs, all also callable as `rebuck2 bank <verb>`:
//!   index <store>               blob\tpath\tbytes for cas/xx/<hash>, blob-sorted
//!   ac-index <store>            path\tsha256\tbytes for ac/ + acn/, path-sorted
//!   tar <store> <batch> <out>   deterministic USTAR of batch's relative paths
//!   link <store> <paths> <dst>  hardlink (copy fallback) paths into dst
//!   purge-failures <dir>        drop AC rows caching a non-zero exit
//!   timings <verb> ...          record/read coarse per-target estimates
//!   gen-store/gen-ac/gen-segments <dir> <n>   synthetic corpora for tests

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use prost::Message;

use crate::store::sha256_hex;

pub mod ac;
pub mod cas;
pub mod dice;
pub mod manifest;
pub mod pack;
pub mod publish;
pub mod timings;

/// A diff base: one id per line, or nothing at all. `/dev/null` and a
/// missing file both mean "cold bank" - the callers pass either.
fn read_list(path: &Path) -> Result<Vec<String>> {
    Ok(fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Staging dir shared by a restore and the publish that follows it.
fn bank_work() -> PathBuf {
    std::env::var("BANK_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("bank"))
}

/// Dispatch for `rebuck2 bank <verb> [args...]`.
pub async fn run(args: &[String]) -> Result<()> {
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    match strs.as_slice() {
        ["index", store] => index(Path::new(store)),
        ["ac-index", store] => ac_index(Path::new(store)),
        ["tar", store, batch, out] => tar(Path::new(store), Path::new(batch), Path::new(out)),
        ["link", store, paths, dst] => link(Path::new(store), Path::new(paths), Path::new(dst)),
        ["purge-failures", dir] => purge_failures(Path::new(dir)),
        // Async, so it cannot live in `timings::cli` with the rest.
        // Exit 3 = cold, as every other restore in here reports it.
        ["timings", "restore", file, lineage, rest @ ..] => {
            let got = timings::restore(
                timings::Restore {
                    table: Path::new(file),
                    lineage,
                    parent: rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                },
                &bank_work(),
            )
            .await?;
            if got.is_none() {
                std::process::exit(3);
            }
            Ok(())
        }
        ["timings", rest @ ..] => timings::cli(rest),
        ["fetch-list", m, owned] => {
            let m = manifest::Manifest::read(Path::new(m))?;
            for s in m.segments_to_fetch(owned) {
                println!("{}", s.name);
            }
            Ok(())
        }
        // AC row lists union line-wise, so a mutated row leaves both its
        // old and new (path, hash) pairs behind; collapse to newest-per-
        // path so the list tracks the row set, not its history.
        ["pack", store, banked, out, rest @ ..] => {
            let only = rest.first().copied().unwrap_or("*");
            let full = rest.contains(&"--full");
            for n in pack::pack_segments(
                Path::new(store),
                &read_list(Path::new(banked))?,
                Path::new(out),
                only,
                full,
            )? {
                println!("{n}");
            }
            Ok(())
        }
        ["ac-pack", store, banked, out, rest @ ..] => {
            for n in pack::ac_pack(
                Path::new(store),
                &read_list(Path::new(banked))?,
                Path::new(out),
                rest.contains(&"--full"),
            )? {
                println!("{n}");
            }
            Ok(())
        }
        ["compact", store, out] => {
            for n in pack::compact(Path::new(store), Path::new(out))? {
                println!("{n}");
            }
            Ok(())
        }
        ["seed", store, segs @ ..] => {
            let dirs: Vec<std::path::PathBuf> = segs.iter().map(Into::into).collect();
            pack::seed_store(Path::new(store), &dirs)?;
            Ok(())
        }
        // Artifact lookups: GITHUB_TOKEN over the REST API, the same
        // calls the workflow made with `gh api`. Prints "id<TAB>name".
        // The whole AC restore: lookup, order, seed, purge, banked list.
        // Exit 3 = cold bank, which callers treat as "nothing to inherit".
        ["ac-restore", store, role, mode, lineage, rest @ ..] => {
            let work = ac::Work::new(
                std::env::var("BANK_WORK")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| std::env::temp_dir().join("bank")),
            )?;
            let parent = rest.first().copied().filter(|p| !p.is_empty() && *p != "-");
            let seeded = ac::restore(
                ac::Restore {
                    store: Path::new(store),
                    role,
                    all_roles: *mode == "all",
                    lineage,
                    parent,
                },
                &work,
            )
            .await?;
            if seeded.is_none() {
                std::process::exit(3);
            }
            Ok(())
        }
        ["ac-publish", store, role, lineage, run, rest @ ..] => {
            let work = ac::Work::new(
                std::env::var("BANK_WORK")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| std::env::temp_dir().join("bank")),
            )?;
            let staged = ac::publish(
                ac::Publish {
                    store: Path::new(store),
                    role,
                    lineage,
                    parent: rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                    run,
                },
                &work,
            )?;
            if let Some(s) = staged {
                // The upload steps are gated on this: no container, no
                // manifest, and a lap with nothing new stages neither.
                if let Ok(out) = std::env::var("GITHUB_OUTPUT") {
                    use std::io::Write;
                    writeln!(
                        fs::OpenOptions::new().append(true).open(out)?,
                        "ac-have=1\nac-segments={}",
                        s.segments
                    )?;
                }
            }
            Ok(())
        }
        // The composite verbs: one call for the whole warm path, so the
        // actions stay one line and cannot drift from each other.
        ["restore", store, role, mode, shard, lineage, rest @ ..] => {
            let warm = restore_all(
                Path::new(store),
                role,
                *mode == "all",
                shard.parse().ok(),
                lineage,
                rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                rest.get(1).copied() != Some("--no-verify"),
            )
            .await?;
            if !warm {
                std::process::exit(3);
            }
            Ok(())
        }
        ["publish", store, role, shard, lineage, run, rest @ ..] => {
            let (cas_staged, ac_staged) = publish_all(
                Path::new(store),
                role,
                shard.parse().ok(),
                lineage,
                rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                run,
                rest.get(1).copied() != Some("--no-ac"),
            )?;
            if let Ok(out) = std::env::var("GITHUB_OUTPUT") {
                use std::io::Write;
                let mut f = fs::OpenOptions::new().append(true).open(out)?;
                for (k, v) in [
                    ("have", cas_staged.container),
                    ("manifest", cas_staged.manifest),
                    ("spill", cas_staged.spill),
                    ("ac-have", ac_staged),
                ] {
                    if v {
                        writeln!(f, "{k}=1")?;
                    }
                }
            }
            Ok(())
        }
        ["cas-restore", store, shard, lineage, rest @ ..] => {
            let work = cas::Work::new(bank_work())?;
            let seeded = cas::restore(
                cas::Restore {
                    store: Path::new(store),
                    shard: shard.parse().ok(),
                    lineage,
                    parent: rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                    absorb_spills: std::env::var("ABSORB_SPILLS").as_deref() == Ok("1"),
                },
                &work,
            )
            .await?;
            if seeded.is_none() {
                std::process::exit(3);
            }
            Ok(())
        }
        ["cas-publish", store, role, shard, lineage, run, rest @ ..] => {
            let work = cas::Work::new(bank_work())?;
            let staged = cas::publish(
                cas::Publish {
                    store: Path::new(store),
                    role,
                    shard: shard.parse().ok(),
                    lineage,
                    parent: rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                    run,
                },
                &work,
            )?;
            if let Ok(out) = std::env::var("GITHUB_OUTPUT") {
                use std::io::Write;
                let mut f = fs::OpenOptions::new().append(true).open(out)?;
                for (k, v) in [
                    ("have", staged.container),
                    ("manifest", staged.manifest),
                    ("spill", staged.spill),
                ] {
                    if v {
                        writeln!(f, "{k}=1")?;
                    }
                }
            }
            Ok(())
        }
        ["dice-restore", dice_dir, lineage, seed, rest @ ..] => {
            let work = bank_work();
            let merged = dice::restore(
                dice::Restore {
                    dice_dir: Path::new(dice_dir),
                    lineage,
                    parent: rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                    seed,
                },
                &work,
            )
            .await?;
            if merged.is_none() {
                std::process::exit(3);
            }
            Ok(())
        }
        ["dice-publish", dice_dir, lineage, seed, run, rest @ ..] => {
            let work = bank_work();
            let staged = dice::publish(
                dice::Publish {
                    dice_dir: Path::new(dice_dir),
                    lineage,
                    parent: rest.first().copied().filter(|p| !p.is_empty() && *p != "-"),
                    seed,
                    run,
                },
                &work,
            )?;
            if staged {
                if let Ok(out) = std::env::var("GITHUB_OUTPUT") {
                    use std::io::Write;
                    writeln!(fs::OpenOptions::new().append(true).open(out)?, "have=1")?;
                }
            }
            Ok(())
        }
        // The manifest artifact name embeds the seed hash, and the
        // workflow needs it to name an upload - so the engine is the one
        // place that computes it.
        ["dice-seed8", seed] => {
            println!("{}", dice::seed8(seed));
            Ok(())
        }
        ["dice-keys", db] => {
            for k in dice::keys(Path::new(db))? {
                println!("{k}");
            }
            Ok(())
        }
        ["dice-pack", db, banked, out] => {
            for n in dice::pack(
                Path::new(db),
                &read_list(Path::new(banked))?,
                Path::new(out),
            )? {
                println!("{n}");
            }
            Ok(())
        }
        ["dice-merge", db, segs @ ..] => {
            let dirs: Vec<PathBuf> = segs.iter().map(Into::into).collect();
            dice::merge(Path::new(db), &dirs)?;
            Ok(())
        }
        ["gh-list", name, lineage] => {
            let c = crate::github::Client::from_env()?;
            for a in c.by_name(name, lineage).await? {
                println!("{}\t{}\t{}", a.id, a.name, a.created_at);
            }
            Ok(())
        }
        ["gh-list-prefix", prefix, lineage] => {
            let c = crate::github::Client::from_env()?;
            for a in c.by_prefix(prefix, lineage).await? {
                println!("{}\t{}\t{}", a.id, a.name, a.created_at);
            }
            Ok(())
        }
        ["gh-download", id, dest] => {
            let c = crate::github::Client::from_env()?;
            c.download_to(id.parse()?, Path::new(dest)).await
        }
        ["collapse-rows", file] => {
            let lines: Vec<String> = std::fs::read_to_string(file)?
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
            for l in manifest::collapse_rows(&lines) {
                println!("{l}");
            }
            Ok(())
        }
        ["needs-compaction", m] => {
            let m = manifest::Manifest::read(Path::new(m))?;
            println!(
                "{}",
                manifest::needs_compaction(&m, &manifest::CompactCfg::from_env())
            );
            Ok(())
        }
        ["write-manifest", lineage, gen, parent, parent_gen, run, head, segs, out] => {
            manifest::write_manifest(
                lineage,
                gen,
                Some(parent),
                Some(parent_gen),
                run.parse().unwrap_or(0),
                (*head != "-").then(|| Path::new(*head)),
                Path::new(segs),
                Path::new(out),
            )
        }
        ["gen-store", dir, n] => gen_store(Path::new(dir), n.parse()?),
        ["gen-ac", dir, n] => gen_ac(Path::new(dir), n.parse()?),
        ["gen-segments", dir, n] => gen_segments(Path::new(dir), n.parse()?),
        _ => bail!(
            "usage: rebuck2 bank index <store> | ac-index <store> \
             | tar <store> <batch> <out> | link <store> <paths> <dst> \
             | purge-failures <dir> | fetch-list <manifest> <prefixes> \
             | needs-compaction <manifest> \
             | write-manifest <lineage> <gen> <parent|-> <parent-gen|-> <run> \
                              <head|-> <segs-dir> <out-dir> \
             | ac-restore <store> <role> <all|own> <lineage> [parent] \
             | ac-publish <store> <role> <lineage> <run> [parent] \
             | restore <store> <role> <all|own> <shard|-> <lineage> [parent] \
             | publish <store> <role> <shard|-> <lineage> <run> [parent] \
             | dice-restore <dice-dir> <lineage> <seed> [parent] \
             | dice-publish <dice-dir> <lineage> <seed> <run> [parent] \
             | dice-pack <db> <banked> <out> | dice-merge <db> <seg>... \
             | dice-keys <db> \
             | cas-restore <store> <shard|-> <lineage> [parent] \
             | cas-publish <store> <role> <shard|-> <lineage> <run> [parent] \
             | gh-list <name> <lineage> | gh-list-prefix <prefix> <lineage> \
             | gh-download <id> <dest> \
             | gen-store <dir> <n> | gen-ac <dir> <n> | gen-segments <dir> <n>"
        ),
    }
}

/// Print `blob\tcas/xx/blob\tbytes`, sorted by blob (byte-lexical, so
/// `comm`/`join` against C-sorted lists agree).
fn index(store: &Path) -> Result<()> {
    let cas = store.join("cas");
    let mut rows: BTreeMap<String, (String, u64)> = BTreeMap::new();
    if cas.is_dir() {
        for d in fs::read_dir(&cas)? {
            let d = d?;
            if !d.file_type()?.is_dir() {
                continue;
            }
            let dname = d.file_name().to_string_lossy().into_owned();
            for f in fs::read_dir(d.path())? {
                let f = f?;
                let meta = f.metadata()?;
                if !meta.is_file() {
                    continue;
                }
                let blob = f.file_name().to_string_lossy().into_owned();
                rows.insert(blob.clone(), (format!("cas/{dname}/{blob}"), meta.len()));
            }
        }
    }
    let mut w = BufWriter::new(std::io::stdout().lock());
    for (blob, (rel, size)) in rows {
        writeln!(w, "{blob}\t{rel}\t{size}")?;
    }
    Ok(w.flush()?)
}

/// Every AC row under `store`, as `path\tsha256\tbytes`, path-sorted.
///
/// Two layouts, both live: `ac/<digest>` is FLAT and `acn/<xx>/<key>` is one
/// level deep (see [`crate::store`]). Paths are store-relative so the same
/// lines feed [`tar`]'s batch file directly. The content hash is the diff
/// key: AC rows are name-stable but content-MUTABLE, so the name alone
/// cannot tell a re-executed action's new result from its old one.
fn ac_index(store: &Path) -> Result<()> {
    let mut w = BufWriter::new(std::io::stdout().lock());
    for (path, hash, size) in ac_rows(store)? {
        writeln!(w, "{path}\t{hash}\t{size}")?;
    }
    Ok(w.flush()?)
}

/// Every AC row as `(store-relative path, sha256(content), bytes)`,
/// path-sorted. See [`ac_index`] for why both layouts are walked.
pub fn ac_rows(store: &Path) -> Result<Vec<(String, String, u64)>> {
    let mut rows: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for (sub, depth) in [("ac", 0u32), ("acn", 1)] {
        let dir = store.join(sub);
        if !dir.is_dir() {
            continue;
        }
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let ft = e.file_type()?;
            if ft.is_file() {
                let bytes = fs::read(e.path())?;
                rows.insert(
                    format!("{sub}/{name}"),
                    (sha256_hex(&bytes), bytes.len() as u64),
                );
            } else if ft.is_dir() && depth > 0 {
                for f in fs::read_dir(e.path())? {
                    let f = f?;
                    if !f.file_type()?.is_file() {
                        continue;
                    }
                    let leaf = f.file_name().to_string_lossy().into_owned();
                    let bytes = fs::read(f.path())?;
                    rows.insert(
                        format!("{sub}/{name}/{leaf}"),
                        (sha256_hex(&bytes), bytes.len() as u64),
                    );
                }
            }
        }
    }
    Ok(rows.into_iter().map(|(p, (h, n))| (p, h, n)).collect())
}

/// Write one octal field: zero-padded to `width - 1`, NUL-terminated.
fn octal(field: &mut [u8], val: u64) {
    let s = format!("{:0width$o}", val, width = field.len() - 1);
    field[..s.len()].copy_from_slice(s.as_bytes());
    field[s.len()] = 0;
}

/// Deterministic USTAR: fixed mode 0755, uid/gid 0, mtime 0, empty
/// uname/gname, entries sorted by path.
///
/// The segment NAME is the sha256 of this raw tar, so ANY nondeterminism
/// here forks segment names for identical content and re-uploads the whole
/// bank - hence 0755 for everything rather than preserving source modes
/// (windows has none to preserve). Spurious exec bits on data blobs are
/// harmless; a MISSING exec bit is not: rebuck2 hardlinks store files into
/// exec dirs, so a 0644 build script dies with EACCES (lap 29507595376, 29
/// targets). The byte layout is pinned by test against the shipped tool's
/// output - changing it is a bank-wide re-upload.
fn tar(store: &Path, batch: &Path, out: &Path) -> Result<()> {
    let paths: Vec<String> = BufReader::new(fs::File::open(batch)?)
        .lines()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    tar_to(store, &paths, out)
}

/// [`tar`] over an in-memory path list.
pub fn tar_to(store: &Path, paths: &[String], out: &Path) -> Result<()> {
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();

    let mut w = BufWriter::new(fs::File::create(out)?);
    let mut written: u64 = 0;
    for rel in &paths {
        let src = store.join(rel);
        let size = fs::metadata(&src)
            .with_context(|| format!("{rel}: not in the store"))?
            .len();

        let mut h = [0u8; 512];
        h[..rel.len()].copy_from_slice(rel.as_bytes()); // cas/xx/<64hex> = 71 <= 100
        octal(&mut h[100..108], 0o755); // mode: see doc comment
        octal(&mut h[108..116], 0); // uid
        octal(&mut h[116..124], 0); // gid
        octal(&mut h[124..136], size);
        octal(&mut h[136..148], 0); // mtime
        h[148..156].copy_from_slice(b"        "); // chksum: spaces while summing
        h[156] = b'0'; // typeflag: regular file
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        octal(&mut h[329..337], 0); // devmajor
        octal(&mut h[337..345], 0); // devminor
        let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        w.write_all(&h)?;
        written += 512;

        let mut f = fs::File::open(&src)?;
        let copied = std::io::copy(&mut f, &mut w)?;
        if copied != size {
            bail!("{rel}: changed size mid-pack ({size} -> {copied} bytes)");
        }
        written += copied;
        let pad = (512 - (copied % 512) % 512) % 512;
        w.write_all(&vec![0u8; pad as usize])?;
        written += pad;
    }
    // Two zero blocks, then pad to the conventional 10240 record size.
    w.write_all(&[0u8; 1024])?;
    written += 1024;
    let pad = (10240 - (written % 10240)) % 10240;
    w.write_all(&vec![0u8; pad as usize])?;
    Ok(w.flush()?)
}

/// Hardlink every store-relative path in `paths` into `dst` (copy when
/// linking fails, e.g. across filesystems).
fn link(store: &Path, paths: &Path, dst: &Path) -> Result<()> {
    for line in BufReader::new(fs::File::open(paths)?).lines() {
        let line = line?;
        let rel = line.trim();
        if rel.is_empty() {
            continue;
        }
        let to = dst.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let from = store.join(rel);
        if fs::hard_link(&from, &to).is_err() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Does this encoded `ActionResult` record a FAILURE? Conservative:
/// undecodable input reads as "not a failure" so we never delete what we
/// cannot parse.
fn is_failure_row(buf: &[u8]) -> bool {
    re::ActionResult::decode(buf).is_ok_and(|r| r.exit_code != 0)
}

/// Delete AC rows that cache FAILURES.
///
/// `--cache-failures` usefully dedupes repeated failures WITHIN a lap, but
/// the read path serves them unconditionally, so a banked environmental
/// failure (the exec-bit EACCES class, lap 29522220924) replays forever.
///
/// Walks BOTH layouts: `ac/` is flat, `acn/` is `<xx>/<key>`. A dir-only
/// walk skipped every top-level FILE, so the digest-keyed AC - where the
/// poison actually lives - was never purged at all.
fn purge_failures(dir: &Path) -> Result<()> {
    let (mut purged, mut kept) = (0u64, 0u64);
    fn sweep(f: &Path, purged: &mut u64, kept: &mut u64) -> Result<()> {
        if is_failure_row(&fs::read(f)?) {
            fs::remove_file(f)?;
            *purged += 1;
        } else {
            *kept += 1;
        }
        Ok(())
    }
    if dir.is_dir() {
        for d in fs::read_dir(dir)? {
            let d = d?;
            let ft = d.file_type()?;
            if ft.is_file() {
                sweep(&d.path(), &mut purged, &mut kept)?;
            } else if ft.is_dir() {
                for f in fs::read_dir(d.path())? {
                    let f = f?;
                    if f.file_type()?.is_file() {
                        sweep(&f.path(), &mut purged, &mut kept)?;
                    }
                }
            }
        }
    }
    println!("purged {purged} failure rows, kept {kept}");
    Ok(())
}

/// Synthetic 64-hex blob name: the reversed hex of `i`, tiled to 64 chars.
/// Reversal puts the varying nibble FIRST so names spread evenly across all
/// 16 prefixes.
fn synth_name(i: u64) -> String {
    let rev: String = format!("{i:08x}").chars().rev().collect();
    rev.repeat(8)
}

/// Test corpus: `n` small blobs laid out as a store (`cas/xx/<name>`).
fn gen_store(dir: &Path, n: u64) -> Result<()> {
    for i in 0..n {
        let name = synth_name(i);
        let d = dir.join("cas").join(&name[..2]);
        fs::create_dir_all(&d)?;
        fs::write(d.join(&name), i.to_string())?;
    }
    Ok(())
}

/// Test corpus: `n` AC rows - 3 in 4 digest-keyed (`ac/<name>`, flat), the
/// rest canonical (`acn/<xx>/<name>`), each holding a distinct payload.
fn gen_ac(dir: &Path, n: u64) -> Result<()> {
    let ac = dir.join("ac");
    fs::create_dir_all(&ac)?;
    for i in 0..n {
        let name = synth_name(i);
        let body = format!("row-{i}");
        if i % 4 == 3 {
            let d = dir.join("acn").join(&name[..2]);
            fs::create_dir_all(&d)?;
            fs::write(d.join(&name), &body)?;
        } else {
            fs::write(ac.join(&name), &body)?;
        }
    }
    Ok(())
}

/// Test corpus: `n` segment dirs (`cas-seg-<hash>/{meta.json,blobs.txt}`),
/// 20 synthetic blobs each. blobs.txt is left uncompressed for the caller.
fn gen_segments(dir: &Path, n: u64) -> Result<()> {
    for i in 0..n {
        let name = synth_name(i);
        let d = dir.join(format!("cas-seg-{name}"));
        fs::create_dir_all(&d)?;
        let mut blobs: Vec<String> = (0..20).map(|j| synth_name(1 + i * 20 + j)).collect();
        blobs.sort();
        fs::write(d.join("blobs.txt"), blobs.join("\n") + "\n")?;
        fs::write(
            d.join("meta.json"),
            format!(
                "{{\"name\":\"cas-seg-{name}\",\"bytes\":1000,\"blobs\":20,\"prefixes\":\"{}\"}}\n",
                &name[..1]
            ),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The segment name IS sha256(raw tar), so this byte layout is a wire
    /// format: if the port changed one padding byte, every segment in the
    /// live bank would get a new name and the whole 8GB would re-upload
    /// under names nothing references. Golden captured from the shipped
    /// ci/cas-bank-tool before it was deleted.
    #[test]
    fn tar_is_byte_identical_to_the_shipped_tool() {
        const GOLDEN: &str = "08bed4a580406feb30b84dfc110d128843191843ca87c2f35e6140a876b3e79a";
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        gen_store(&store, 20).unwrap();

        let mut batch = String::new();
        let mut names: Vec<String> = Vec::new();
        for i in 0..20 {
            names.push(synth_name(i));
        }
        names.sort();
        for n in &names {
            batch.push_str(&format!("cas/{}/{}\n", &n[..2], n));
        }
        let batch_file = dir.path().join("batch");
        fs::write(&batch_file, batch).unwrap();

        let out = dir.path().join("out.tar");
        tar(&store, &batch_file, &out).unwrap();
        let bytes = fs::read(&out).unwrap();
        assert_eq!(bytes.len(), 30720, "tar record padding changed");
        assert_eq!(sha256_hex(&bytes), GOLDEN, "USTAR bytes drifted");
    }

    #[test]
    fn ac_index_hashes_content_and_walks_both_layouts() {
        let dir = tempfile::tempdir().unwrap();
        gen_ac(dir.path(), 12).unwrap();

        let mut rows: BTreeMap<String, (String, u64)> = BTreeMap::new();
        for (sub, depth) in [("ac", 0u32), ("acn", 1)] {
            let d = dir.path().join(sub);
            if !d.is_dir() {
                continue;
            }
            for e in fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().unwrap().is_file() {
                    let b = fs::read(e.path()).unwrap();
                    rows.insert(format!("{sub}/{name}"), (sha256_hex(&b), b.len() as u64));
                } else if depth > 0 {
                    for f in fs::read_dir(e.path()).unwrap() {
                        let f = f.unwrap();
                        let leaf = f.file_name().to_string_lossy().into_owned();
                        let b = fs::read(f.path()).unwrap();
                        rows.insert(
                            format!("{sub}/{name}/{leaf}"),
                            (sha256_hex(&b), b.len() as u64),
                        );
                    }
                }
            }
        }
        assert_eq!(rows.len(), 12, "flat ac/ and nested acn/ both indexed");
        assert_eq!(
            rows.keys().filter(|k| k.starts_with("acn/")).count(),
            3,
            "1 row in 4 is canonical"
        );
        // The hash is of CONTENT, not of the name - the whole point of the
        // (name, hash) diff key.
        let first = rows.get("ac/0000000000000000000000000000000000000000000000000000000000000000");
        assert_eq!(
            first.map(|(h, n)| (h.as_str(), *n)),
            Some((
                "3c831eb5a23962a50dbffc0d4f37facd0f1171844d08d1a5cc93a04426f02393",
                5
            ))
        );
    }

    #[test]
    fn purge_reaches_flat_rows_and_spares_the_undecodable() {
        let dir = tempfile::tempdir().unwrap();
        let ac = dir.path().join("ac");
        let nested = ac.join("ab");
        fs::create_dir_all(&nested).unwrap();

        // ac/ is FLAT: a dir-only walk purged nothing where the poison is.
        fs::write(ac.join("failrow"), [0x20, 0x01]).unwrap(); // exit_code = 1
        fs::write(ac.join("okrow"), [0x20, 0x00]).unwrap(); // exit_code = 0
        fs::write(nested.join("nested-fail"), [0x20, 0x02]).unwrap();
        // Undecodable: keep it. Deleting what we cannot parse is worse than
        // serving it - validated_ac_get refuses corrupt rows anyway.
        fs::write(ac.join("garbage"), b"\xff\xff\xff\xff").unwrap();

        purge_failures(&ac).unwrap();

        assert!(!ac.join("failrow").exists(), "flat failure row survived");
        assert!(
            !nested.join("nested-fail").exists(),
            "nested failure row survived"
        );
        assert!(ac.join("okrow").exists(), "success row was eaten");
        assert!(ac.join("garbage").exists(), "undecodable row was eaten");
    }
}

/// zstd via the CLI, deliberately.
///
/// The `zstd` crate would drag in a C build (zstd-sys) for something every
/// runner already has on PATH and the shell layer already uses for the same
/// files - and the segment name is sha256 of the RAW tar precisely so the
/// compressor's version cannot fork it. Shelling out keeps the bytes
/// identical to what the previous generation wrote.
pub mod zstd {
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use anyhow::{bail, Result};

    pub fn read_lines(path: &Path) -> Result<Vec<String>> {
        let out = Command::new("zstd").arg("-dqc").arg(path).output()?;
        if !out.status.success() {
            bail!(
                "zstd -dqc {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Compress a file in place-adjacent form (source kept).
    pub fn compress_file(src: &Path, dst: &Path, level: u8) -> Result<()> {
        let st = Command::new("zstd")
            .args(["-q", "-f", &format!("-{level}")])
            .arg(src)
            .arg("-o")
            .arg(dst)
            .status()?;
        if !st.success() {
            bail!("zstd {}: exit {st}", src.display());
        }
        Ok(())
    }

    /// `zstd -dc <tarball> | tar -x -C <dest>`.
    pub fn extract_tar(tarball: &Path, dest: &Path) -> Result<()> {
        let z = Command::new("zstd")
            .args(["-dqc"])
            .arg(tarball)
            .stdout(Stdio::piped())
            .spawn()?;
        let st = Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(dest)
            .stdin(Stdio::from(z.stdout.expect("piped")))
            .status()?;
        if !st.success() {
            bail!("tar -x {}: exit {st}", tarball.display());
        }
        Ok(())
    }

    pub fn write_lines(path: &Path, lines: &[String]) -> Result<()> {
        let mut child = Command::new("zstd")
            .args(["-q", "-f", "-o"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()?;
        {
            let mut si = child.stdin.take().expect("piped");
            for l in lines {
                writeln!(si, "{l}")?;
            }
        }
        let st = child.wait()?;
        if !st.success() {
            bail!("zstd -o {}: exit {st}", path.display());
        }
        Ok(())
    }
}

/// The whole warm path, as one call.
///
/// Composition lives here rather than in YAML so there is exactly one
/// definition of "restore the bank": the actions, the workflow and a
/// developer running it by hand all get the same sequence. Returns false
/// for a cold bank, which is a normal first lap on a new lineage.
pub async fn restore_all(
    store: &Path,
    role: &str,
    all_roles: bool,
    shard: Option<u8>,
    lineage: &str,
    parent: Option<&str>,
    verify: bool,
) -> Result<bool> {
    let work_dir = bank_work();
    let mut warm = false;

    let cas_work = cas::Work::new(work_dir.clone())?;
    if cas::restore(
        cas::Restore {
            store,
            shard,
            lineage,
            parent,
            absorb_spills: shard.is_some(),
        },
        &cas_work,
    )
    .await?
    .is_some()
    {
        warm = true;
    }

    let ac_work = ac::Work::new(work_dir)?;
    if ac::restore(
        ac::Restore {
            store,
            role,
            all_roles,
            lineage,
            parent,
        },
        &ac_work,
    )
    .await?
    .is_some()
    {
        warm = true;
    }

    // Anything that crossed the artifact wall proves itself before the
    // build can use it: a CAS filename IS the sha256 of its content, and
    // the artifact namespace is shared by every run and branch in the
    // repo, fork PRs included.
    if verify && warm {
        let (ok, bad) = crate::store::verify_cas(store)?;
        println!("[bank] verified {ok} blobs, rejected {bad}");
        if bad > 0 {
            eprintln!("[bank] WARNING - rejected blobs suggest a poisoned or corrupt artifact");
        }
    }
    Ok(warm)
}

/// Bank everything this node has to bank, in one call.
pub fn publish_all(
    store: &Path,
    role: &str,
    shard: Option<u8>,
    lineage: &str,
    parent: Option<&str>,
    run: &str,
    with_ac: bool,
) -> Result<(cas::Staged, bool)> {
    let work_dir = bank_work();
    let cas_staged = cas::publish(
        cas::Publish {
            store,
            role,
            shard,
            lineage,
            parent,
            run,
        },
        &cas::Work::new(work_dir.clone())?,
    )?;
    let ac_staged = if with_ac {
        ac::publish(
            ac::Publish {
                store,
                role,
                lineage,
                parent,
                run,
            },
            &ac::Work::new(work_dir)?,
        )?
        .is_some()
    } else {
        false
    };
    Ok((cas_staged, ac_staged))
}
