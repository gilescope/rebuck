//! Packing: store contents in, sealed segments out.
//!
//! A segment is `cas-seg-<sha256 of the raw tar>/{bulk.tar.zst,
//! blobs.txt.zst, meta.json}`. Naming by the RAW tar is what lets the
//! compressor change version without forking every name in the bank.
//!
//! The shell original was O(n·forks): sizing each blob inside the batching
//! loop cost an awk scan plus a `wc` fork per item and stalled all eleven
//! workers for 30 minutes at fleet scale (run 29435672672). Everything here
//! is one pass over an index built once.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::manifest::Segment;
use super::{tar_to, zstd};
use crate::store::sha256_hex;

/// Default segment size target. Small enough that a torn upload loses
/// little, big enough that the artifact count stays sane.
pub const SEG_MAX_MB_DEFAULT: u64 = 64;

fn seg_max_bytes() -> u64 {
    std::env::var("SEG_MAX_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(SEG_MAX_MB_DEFAULT)
        * 1024
        * 1024
}

/// One item to pack: where it lives, how big it is, and the line that
/// identifies it in `blobs.txt`.
struct Item {
    rel: String,
    size: u64,
    id: String,
}

/// Seal one batch into a segment directory, returning its name.
fn seal(store: &Path, out: &Path, batch: &[Item], full: bool) -> Result<String> {
    let paths: Vec<String> = batch.iter().map(|i| i.rel.clone()).collect();
    let tmp = out.join(".seg-building");
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;

    let raw = tmp.join("bulk.tar");
    tar_to(store, &paths, &raw)?;
    let sha = sha256_hex(&fs::read(&raw)?);
    zstd::compress_file(&raw, &tmp.join("bulk.tar.zst"), 8)?;
    fs::remove_file(&raw)?;

    let mut ids: Vec<String> = batch.iter().map(|i| i.id.clone()).collect();
    ids.sort();
    zstd::write_lines(&tmp.join("blobs.txt.zst"), &ids)?;

    // The bitmap is the first hex char of every blob id, deduped. For AC
    // rows the id is a path, which has no prefix meaning - those segments
    // are marked '*' because the AC restore is whole-fetch.
    let prefixes: String = if batch.iter().any(|i| i.id.contains(' ')) {
        "*".into()
    } else {
        let mut p: Vec<char> = ids.iter().filter_map(|b| b.chars().next()).collect();
        p.sort_unstable();
        p.dedup();
        p.into_iter().collect()
    };

    let seg = Segment {
        name: format!("cas-seg-{sha}"),
        bytes: fs::metadata(tmp.join("bulk.tar.zst"))?.len(),
        blobs: ids.len() as u64,
        prefixes,
        artifact: None,
        run: None,
        role: None,
        full: full.then_some(true),
    };
    fs::write(tmp.join("meta.json"), serde_json::to_string(&seg)? + "\n")?;
    let dest = out.join(&seg.name);
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    fs::rename(&tmp, &dest)?;
    Ok(seg.name)
}

/// Greedy split by cumulative size, then seal each batch.
fn pack_items(store: &Path, out: &Path, items: Vec<Item>, full: bool) -> Result<Vec<String>> {
    fs::create_dir_all(out)?;
    let max = seg_max_bytes();
    let mut names = Vec::new();
    let mut batch: Vec<Item> = Vec::new();
    let mut bytes = 0u64;
    for it in items {
        if !batch.is_empty() && bytes + it.size > max {
            names.push(seal(store, out, &batch, full)?);
            batch.clear();
            bytes = 0;
        }
        bytes += it.size;
        batch.push(it);
    }
    if !batch.is_empty() {
        names.push(seal(store, out, &batch, full)?);
    }
    Ok(names)
}

/// Every CAS blob in the store, as `hash -> (relative path, size)`.
fn index_cas(store: &Path) -> Result<BTreeMap<String, (String, u64)>> {
    let mut rows = BTreeMap::new();
    let cas = store.join("cas");
    if !cas.is_dir() {
        return Ok(rows);
    }
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
    Ok(rows)
}

/// Pack blobs the bank does not already hold.
///
/// `banked` is the union blob list (may be empty for a cold bank).
/// `only` filters by first hex char - a range owner packs its own
/// prefixes and everything else spills for the owner to absorb later.
pub fn pack_segments(
    store: &Path,
    banked: &[String],
    out: &Path,
    only: &str,
    full: bool,
) -> Result<Vec<String>> {
    let have: std::collections::HashSet<&str> = banked.iter().map(String::as_str).collect();
    let items: Vec<Item> = index_cas(store)?
        .into_iter()
        .filter(|(hash, _)| !have.contains(hash.as_str()))
        .filter(|(hash, _)| only == "*" || hash.chars().next().is_some_and(|c| only.contains(c)))
        .map(|(hash, (rel, size))| Item {
            rel,
            size,
            id: hash,
        })
        .collect();
    pack_items(store, out, items, full)
}

/// Pack AC rows whose `(path, content-hash)` pair the bank does not hold.
///
/// The diff key is the PAIR: rows are name-stable but content-mutable, so
/// a re-executed action overwrites its row under the same name and must
/// re-bank.
pub fn ac_pack(store: &Path, banked: &[String], out: &Path, full: bool) -> Result<Vec<String>> {
    let have: std::collections::HashSet<&str> = banked.iter().map(String::as_str).collect();
    let mut items = Vec::new();
    for (path, hash, size) in super::ac_rows(store)? {
        let line = format!("{path} {hash}");
        if have.contains(line.as_str()) {
            continue;
        }
        items.push(Item {
            rel: path,
            size,
            id: line,
        });
    }
    items.sort_by(|a, b| a.rel.cmp(&b.rel));
    pack_items(store, out, items, full)
}

/// Re-bin the whole store into prefix-grouped full packs.
///
/// Compaction is publish-with-no-diff-base plus prefix binning: the owner's
/// store already holds its range's full view, so this is not a merge, it is
/// a re-pack. Binning by prefix pays down the over-fetch the bitmap's
/// "presence is a maybe" allows.
pub fn compact(store: &Path, out: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(out)?;
    let all = index_cas(store)?;
    let mut names = Vec::new();
    for p in "0123456789abcdef".chars() {
        let items: Vec<Item> = all
            .iter()
            .filter(|(hash, _)| hash.starts_with(p))
            .map(|(hash, (rel, size))| Item {
                rel: rel.clone(),
                size: *size,
                id: hash.clone(),
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        names.extend(pack_items(store, out, items, true)?);
    }
    Ok(names)
}

/// Untar segments into the store, in the order given.
///
/// Order is the caller's business and it matters for the AC: rows are
/// content-mutable, so later segments must overwrite earlier ones.
pub fn seed_store(store: &Path, segments: &[PathBuf]) -> Result<u64> {
    fs::create_dir_all(store)?;
    let mut seeded = 0;
    for d in segments {
        let tarball = d.join("bulk.tar.zst");
        if !tarball.is_file() {
            continue;
        }
        zstd::extract_tar(&tarball, store)?;
        seeded += 1;
    }
    Ok(seeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkblob(store: &Path, hash: &str, bytes: usize) {
        let d = store.join("cas").join(&hash[..2]);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(hash), vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn packs_only_what_the_bank_lacks_and_names_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        mkblob(&store, "1111aaaa", 100);
        mkblob(&store, "2222bbbb", 200);
        mkblob(&store, "99ffcccc", 300);

        let names = pack_segments(&store, &[], &dir.path().join("segs"), "*", false).unwrap();
        assert_eq!(names.len(), 1, "600 bytes is one segment");
        let meta: super::super::manifest::Segment = serde_json::from_str(
            &fs::read_to_string(dir.path().join("segs").join(&names[0]).join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta.prefixes, "129", "bitmap is the deduped first chars");
        assert_eq!(meta.blobs, 3);

        // Same content, fresh store -> same name. The whole bank depends
        // on this: a name that drifts re-uploads everything.
        let store2 = dir.path().join("s2");
        mkblob(&store2, "1111aaaa", 100);
        mkblob(&store2, "2222bbbb", 200);
        mkblob(&store2, "99ffcccc", 300);
        let again = pack_segments(&store2, &[], &dir.path().join("segs2"), "*", false).unwrap();
        assert_eq!(
            names, again,
            "identical content must pack to identical names"
        );

        // Banked blobs are skipped entirely.
        let none = pack_segments(
            &store,
            &["1111aaaa".into(), "2222bbbb".into(), "99ffcccc".into()],
            &dir.path().join("segs3"),
            "*",
            false,
        )
        .unwrap();
        assert!(none.is_empty(), "nothing new must pack nothing");
    }

    #[test]
    fn prefix_filter_splits_range_from_spill() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        mkblob(&store, "1111aaaa", 10);
        mkblob(&store, "99ffcccc", 10);
        mkblob(&store, "eeee0123", 10);

        let names = pack_segments(&store, &[], &dir.path().join("own"), "9e", false).unwrap();
        let ids = zstd::read_lines(&dir.path().join("own").join(&names[0]).join("blobs.txt.zst"))
            .unwrap();
        assert_eq!(ids, ["99ffcccc", "eeee0123"], "only the owned prefixes");
    }

    #[test]
    fn seed_round_trips_bytes_and_the_exec_bit() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        mkblob(&store, "ab120001", 40);
        // Lap 29507595376's 29 linux "failures" were one bug: 0644 in the
        // USTAR header stripped exec bits, and rebuck2 hardlinks store
        // files into exec dirs, so bank-seeded build scripts died EACCES.
        let names = pack_segments(&store, &[], &dir.path().join("segs"), "*", false).unwrap();
        let restored = dir.path().join("r");
        let n = seed_store(&restored, &[dir.path().join("segs").join(&names[0])]).unwrap();
        assert_eq!(n, 1);
        let out = restored.join("cas/ab/ab120001");
        assert_eq!(fs::read(&out).unwrap(), vec![b'x'; 40]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                fs::metadata(&out).unwrap().permissions().mode() & 0o111 != 0,
                "exec bit lost through pack/seed"
            );
        }
    }

    #[test]
    fn ac_pack_keys_on_content_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        let ac = store.join("ac");
        fs::create_dir_all(&ac).unwrap();
        fs::write(ac.join("a".repeat(64)), b"v1").unwrap();

        let names = ac_pack(&store, &[], &dir.path().join("s1"), false).unwrap();
        let rows =
            zstd::read_lines(&dir.path().join("s1").join(&names[0]).join("blobs.txt.zst")).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("ac/aaaa"), "id is the path + hash");

        // Unchanged: nothing packs. Same name, new content: it re-banks.
        let banked = rows.clone();
        assert!(
            ac_pack(&store, &banked, &dir.path().join("s2"), false)
                .unwrap()
                .is_empty(),
            "an unchanged row must not re-bank"
        );
        fs::write(ac.join("a".repeat(64)), b"v2").unwrap();
        assert_eq!(
            ac_pack(&store, &banked, &dir.path().join("s3"), false)
                .unwrap()
                .len(),
            1,
            "same name + new content must re-bank"
        );
    }

    #[test]
    fn compaction_preserves_the_blob_set_and_marks_full() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s");
        for (i, h) in ["0aaa1111", "0bbb2222", "9ccc3333", "eddd4444"]
            .iter()
            .enumerate()
        {
            mkblob(&store, h, 10 * (i + 1));
        }
        let names = compact(&store, &dir.path().join("packs")).unwrap();
        assert!(!names.is_empty());

        let mut got: Vec<String> = Vec::new();
        for n in &names {
            let meta: super::super::manifest::Segment = serde_json::from_str(
                &fs::read_to_string(dir.path().join("packs").join(n).join("meta.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(meta.full, Some(true), "compaction packs are marked full");
            got.extend(
                zstd::read_lines(&dir.path().join("packs").join(n).join("blobs.txt.zst")).unwrap(),
            );
        }
        got.sort();
        assert_eq!(
            got,
            ["0aaa1111", "0bbb2222", "9ccc3333", "eddd4444"],
            "compaction must not shed blobs"
        );
    }
}
