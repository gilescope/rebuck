//! Digest-keyed disk store: CAS blobs + ActionCache results.
//!
//! Layout: `<root>/cas/<hh>/<hash>` and `<root>/ac/<hash>`. Writes go through
//! a tmp file + rename so a crashed process never leaves a truncated blob
//! behind a valid digest.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::mesh::Dig;

/// sha256 of the empty string — REAPI clients assume it exists without upload.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Process-wide tmp-file sequence — see the uniqueness note in [`Store::put`].
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How a blob reached its destination — decides who owns exec-bit duty.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(target_os = "macos", allow(dead_code))] // Linked is a non-mac path
pub enum Materialized {
    /// Shared inode (hardlink): perms already correct, never chmod.
    Linked,
    /// Private inode (clone or copy): caller may chmod freely.
    Private,
}

pub struct Store {
    root: PathBuf,
    /// Block-clone probe: 0 unknown, 1 works, 2 refused (NTFS/ext4) or
    /// disabled via --no-reflink. One failed ioctl per process, not per file.
    clone_state: std::sync::atomic::AtomicU8,
    /// Bytes newly persisted to the CAS (disk-pressure signal).
    pub stored_bytes: std::sync::atomic::AtomicU64,
    /// Bytes read out of the CAS (egress-saturation proxy — mesh Gets and
    /// gRPC reads all funnel through here).
    pub read_bytes: std::sync::atomic::AtomicU64,
}

/// Hash-verify every CAS blob under `dir` (filename IS the content
/// sha256), deleting mismatches. Native replacement for the shell
/// verify-cas: one sequential msys hasher took 31min over 95k blobs on
/// windows, and parallel shell hashers interleaved their stdout and
/// "mismatch"-deleted good files (run 29082052036's mac fleet).
/// Returns (verified, rejected).
pub fn verify_cas(dir: &std::path::Path) -> anyhow::Result<(u64, u64)> {
    let cas = dir.join("cas");
    let (mut ok, mut bad) = (0u64, 0u64);
    if !cas.is_dir() {
        return Ok((0, 0));
    }
    for sub in std::fs::read_dir(&cas)?.flatten() {
        if !sub.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        for f in std::fs::read_dir(sub.path())?.flatten() {
            let p = f.path();
            if !p.is_file() {
                continue;
            }
            let name = f.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(&p)?;
            if sha256_hex(&bytes) == name {
                ok += 1;
            } else {
                eprintln!("verify-store: DIGEST MISMATCH - deleting {name}");
                let _ = std::fs::remove_file(&p);
                bad += 1;
            }
        }
    }
    Ok((ok, bad))
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        for sub in ["cas", "ac", "tmp"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self {
            root,
            clone_state: std::sync::atomic::AtomicU8::new(0),
            stored_bytes: std::sync::atomic::AtomicU64::new(0),
            read_bytes: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Force the old pre-clone behaviour (--no-reflink).
    pub fn disable_clone(&self) {
        self.clone_state
            .store(2, std::sync::atomic::Ordering::Relaxed);
    }

    /// ReFS block clone / FICLONE via reflink-copy: CoW, private inode, no
    /// link ceiling. Returns false when unsupported here (probe-once).
    async fn try_clone(&self, src: &std::path::Path, dest: &std::path::Path) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if cfg!(target_os = "macos") {
            return false; // fs::copy already clones on APFS
        }
        let state = self.clone_state.load(Relaxed);
        if state == 2 {
            return false;
        }
        let (s, d) = (src.to_path_buf(), dest.to_path_buf());
        let ok = tokio::task::spawn_blocking(move || reflink_copy::reflink(&s, &d).is_ok())
            .await
            .unwrap_or(false);
        if ok {
            self.clone_state.store(1, Relaxed);
        } else if state == 0 {
            self.clone_state.store(2, Relaxed);
            println!("[store] block clone unsupported here — hardlink/copy chain in use");
        }
        ok
    }

    #[cfg(test)]
    pub fn cas_path_for_test(&self, hash: &str) -> PathBuf {
        self.cas_path(hash)
    }

    fn cas_path(&self, hash: &str) -> PathBuf {
        self.root.join("cas").join(&hash[..2]).join(hash)
    }

    pub async fn has(&self, d: &Dig) -> bool {
        if d.size == 0 && d.hash == EMPTY_SHA256 {
            return true;
        }
        match tokio::fs::metadata(self.cas_path(&d.hash)).await {
            Ok(m) => m.len() == d.size as u64,
            Err(_) => false,
        }
    }

    pub async fn get(&self, d: &Dig) -> Result<Option<Vec<u8>>> {
        if d.size == 0 && d.hash == EMPTY_SHA256 {
            return Ok(Some(Vec::new()));
        }
        match tokio::fs::read(self.cas_path(&d.hash)).await {
            Ok(b) => {
                self.read_bytes
                    .fetch_add(b.len() as u64, std::sync::atomic::Ordering::Relaxed);
                Ok(Some(b))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Store bytes; verifies against `expected` when given. Returns the digest.
    pub async fn put(&self, expected: Option<&Dig>, bytes: &[u8]) -> Result<Dig> {
        let hash = sha256_hex(bytes);
        if let Some(exp) = expected {
            if exp.hash != hash || exp.size as usize != bytes.len() {
                bail!(
                    "digest mismatch: expected {}/{}, got {}/{}",
                    exp.hash,
                    exp.size,
                    hash,
                    bytes.len()
                );
            }
        }
        let dest = self.cas_path(&hash);
        if tokio::fs::metadata(&dest).await.is_err() {
            tokio::fs::create_dir_all(dest.parent().context("cas path has parent")?).await?;
            // Tmp name must be unique per CALL: concurrent puts of the same
            // blob in one process otherwise share a path and race each
            // other's rename (ENOENT storms under sweep-scale upload load).
            let tmp = self.root.join("tmp").join(format!(
                "{hash}.{}.{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            tokio::fs::write(&tmp, bytes).await?;
            // Unix blobs are 0o555 from first visibility: no write bit means a
            // careless action writing through a hardlinked input gets EACCES
            // instead of silently poisoning the CAS; the exec bit is global
            // because REAPI exec-ness is per-reference, and a spurious +x on
            // an input is benign where a chmod on a shared inode is not.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555)).await?;
            }
            if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
                // A concurrent identical put may have won the rename; content
                // is identical by construction, so losing is fine.
                let _ = tokio::fs::remove_file(&tmp).await;
                if tokio::fs::metadata(&dest).await.is_err() {
                    return Err(e).context("persist blob");
                }
            } else {
                self.stored_bytes
                    .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(Dig {
            hash,
            size: bytes.len() as i64,
        })
    }

    /// Materialize a blob at `dest` for near-zero cost. Chain: block clone
    /// (ReFS/btrfs — private inode) -> hardlink (shared inode, 0o555-defended)
    /// -> copy on the two honest link refusals. macOS: fs::copy = APFS clone.
    pub async fn link_out(&self, d: &Dig, dest: &std::path::Path) -> Result<Materialized> {
        let src = self.cas_path(&d.hash);
        if self.try_clone(&src, dest).await {
            return Ok(Materialized::Private);
        }
        #[cfg(target_os = "macos")]
        {
            tokio::fs::copy(&src, dest)
                .await
                .with_context(|| format!("clone {} -> {}", d.hash, dest.display()))?;
            Ok(Materialized::Private)
        }
        #[cfg(not(target_os = "macos"))]
        match tokio::fs::hard_link(&src, dest).await {
            Ok(()) => Ok(Materialized::Linked),
            // The two honest refusals fall back to copy; anything else
            // surfaces rather than hiding filesystem trouble.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TooManyLinks | std::io::ErrorKind::CrossesDevices
                ) =>
            {
                tokio::fs::copy(&src, dest)
                    .await
                    .with_context(|| format!("materialize {} -> {}", d.hash, dest.display()))?;
                Ok(Materialized::Private)
            }
            Err(e) => {
                // Field forensics: which leg of the link vanished? A stat
                // burst is cheap next to a failed action, and one retry
                // distinguishes a transient race from a real absence.
                let src_exists = tokio::fs::metadata(&src).await.is_ok();
                let parent_exists = match dest.parent() {
                    Some(p) => tokio::fs::metadata(p).await.is_ok(),
                    None => false,
                };
                let dest_exists = tokio::fs::symlink_metadata(dest).await.is_ok();
                if src_exists && parent_exists && !dest_exists {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if tokio::fs::hard_link(&src, dest).await.is_ok() {
                        eprintln!(
                            "[store] hardlink transient race healed by retry: {} -> {}",
                            d.hash,
                            dest.display()
                        );
                        return Ok(Materialized::Linked);
                    }
                }
                Err(e).with_context(|| {
                    format!(
                        "hardlink {} -> {} (src_exists={src_exists} dest_parent_exists={parent_exists} dest_exists={dest_exists})",
                        d.hash,
                        dest.display()
                    )
                })
            }
        }
    }

    /// Adopt an existing file (an action output) into the CAS by clone/link
    /// instead of rewriting its bytes — the outbound twin of `link_out`.
    /// Clone path: store gets private CoW extents, source stays writable.
    /// Hardlink path: source inode goes 0o555 first on unix, so the store
    /// name is read-only from the moment it exists.
    pub async fn adopt(&self, expected: &Dig, src: &std::path::Path) -> Result<()> {
        let dest = self.cas_path(&expected.hash);
        if tokio::fs::metadata(&dest).await.is_ok() {
            return Ok(());
        }
        tokio::fs::create_dir_all(dest.parent().context("cas path has parent")?).await?;
        let tmp = self.root.join("tmp").join(format!(
            "{}.{}.{}",
            expected.hash,
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if self.try_clone(src, &tmp).await {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555)).await?;
            }
            tokio::fs::rename(&tmp, &dest).await?;
            self.stored_bytes
                .fetch_add(expected.size as u64, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(src, std::fs::Permissions::from_mode(0o555)).await?;
        }
        let copy_via_tmp = |src: std::path::PathBuf,
                            dest: std::path::PathBuf,
                            tmp: std::path::PathBuf| async move {
            tokio::fs::copy(&src, &tmp).await?;
            tokio::fs::rename(&tmp, &dest).await?;
            Ok::<(), std::io::Error>(())
        };
        #[cfg(target_os = "macos")]
        {
            copy_via_tmp(src.to_path_buf(), dest.clone(), tmp)
                .await
                .with_context(|| format!("adopt-clone {}", expected.hash))?;
        }
        #[cfg(not(target_os = "macos"))]
        match tokio::fs::hard_link(src, &dest).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TooManyLinks | std::io::ErrorKind::CrossesDevices
                ) =>
            {
                copy_via_tmp(src.to_path_buf(), dest.clone(), tmp)
                    .await
                    .with_context(|| format!("adopt-copy {}", expected.hash))?;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("adopt-link {}", expected.hash));
            }
        }
        self.stored_bytes
            .fetch_add(expected.size as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// All CAS blob hashes currently on disk (for bloom gossip). Walks the
    /// two-level fan-out; ~10k entries costs single-digit ms.
    pub fn list_hashes(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(subs) = std::fs::read_dir(self.root.join("cas")) else {
            return out;
        };
        for sub in subs.flatten() {
            if let Ok(files) = std::fs::read_dir(sub.path()) {
                for f in files.flatten() {
                    if let Some(name) = f.file_name().to_str() {
                        out.push(name.to_owned());
                    }
                }
            }
        }
        out
    }

    /// Like `list_hashes` but with sizes (for shard listings, where the
    /// fetcher needs full digests).
    /// CAS entries whose first hex nibble falls in shard `shard` of `of`.
    pub fn list_shard(&self, shard: u8, of: u8) -> Vec<crate::mesh::Dig> {
        let of = of.max(1);
        self.list_entries()
            .into_iter()
            .filter(|(hash, _)| {
                u8::from_str_radix(&hash[..1], 16)
                    .map(|n| n / (16 / of.min(16)) == shard)
                    .unwrap_or(false)
            })
            .map(|(hash, size)| crate::mesh::Dig { hash, size })
            .collect()
    }

    pub fn list_entries(&self) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        let Ok(subs) = std::fs::read_dir(self.root.join("cas")) else {
            return out;
        };
        for sub in subs.flatten() {
            if let Ok(files) = std::fs::read_dir(sub.path()) {
                for f in files.flatten() {
                    if let (Some(name), Ok(meta)) = (f.file_name().to_str(), f.metadata()) {
                        out.push((name.to_owned(), meta.len() as i64));
                    }
                }
            }
        }
        out
    }

    pub async fn ac_get(&self, action_hash: &str) -> Option<Vec<u8>> {
        tokio::fs::read(self.root.join("ac").join(action_hash))
            .await
            .ok()
    }

    pub async fn ac_put(&self, action_hash: &str, bytes: &[u8]) -> Result<()> {
        let tmp = self
            .root
            .join("tmp")
            .join(format!("ac-{action_hash}.{}", std::process::id()));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, self.root.join("ac").join(action_hash)).await?;
        Ok(())
    }

    /// Every action hash present in the AC (filenames are the hashes).
    pub fn ac_list(&self) -> Vec<String> {
        std::fs::read_dir(self.root.join("ac"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect()
    }

    pub async fn ac_delete(&self, action_hash: &str) {
        let _ = tokio::fs::remove_file(self.root.join("ac").join(action_hash)).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn verify_cas_keeps_good_deletes_bad() {
        let dir = tempfile::tempdir().unwrap();
        let good = crate::store::sha256_hex(b"abc");
        let sub = dir.path().join("cas").join(&good[..2]);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(&good), b"abc").unwrap();
        let evil = dir.path().join("cas").join("aa");
        std::fs::create_dir_all(&evil).unwrap();
        std::fs::write(evil.join("aa".repeat(32)), b"evil").unwrap();
        let (ok, bad) = crate::store::verify_cas(dir.path()).unwrap();
        assert_eq!((ok, bad), (1, 1));
        assert!(sub.join(&good).exists());
        assert!(!evil.join("aa".repeat(32)).exists());
    }

    use super::*;

    /// Regression: concurrent puts of the SAME content must all succeed.
    /// The sweep hit "os error 2" storms when thousands of uploads of a
    /// common blob shared one tmp path and raced each other's rename.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_same_blob_puts() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::new(dir.path().to_path_buf()).unwrap());
        for round in 0u32..20 {
            let bytes = vec![round as u8; 256 * 1024];
            let tasks: Vec<_> = (0..50)
                .map(|_| {
                    let store = store.clone();
                    let bytes = bytes.clone();
                    tokio::spawn(async move { store.put(None, &bytes).await })
                })
                .collect();
            for t in tasks {
                t.await.unwrap().expect("concurrent put must not race");
            }
        }
    }
}
