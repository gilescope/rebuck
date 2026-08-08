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

/// A blob being streamed into the CAS: hashed as it is written, so the bytes
/// never accumulate in memory. Created by [`Store::begin_upload`], landed by
/// [`Store::finish_upload`].
///
/// An `Upload` dropped without finishing (client hung up, digest mismatch,
/// task cancelled) takes its tmp file with it — an abandoned upload must not
/// leave a turd in `tmp/`.
pub struct Upload {
    file: tokio::fs::File,
    hasher: sha2::Sha256,
    tmp: PathBuf,
    len: u64,
}

impl Upload {
    /// Append a chunk. Hashing and writing happen together so the bytes are
    /// touched once.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;
        self.hasher.update(chunk);
        self.file.write_all(chunk).await?;
        self.len += chunk.len() as u64;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    fn hash(&self) -> String {
        use sha2::Digest;
        let d = self.hasher.clone().finalize();
        let mut s = String::with_capacity(64);
        for b in d {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        // `finish_upload` takes the path when it renames; an empty path means
        // the blob landed and there is nothing to clean.
        if !self.tmp.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

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
    pub fn root_dir(&self) -> PathBuf {
        self.root.clone()
    }

    pub fn new(root: PathBuf) -> Result<Self> {
        // "tags" is the OCI ref namespace - without it every tag_put fails
        // at the rename and the registry answers 500 to a perfectly valid
        // manifest PUT.
        for sub in ["cas", "ac", "tmp", "tags"] {
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

    /// `link_out` plus the exec guarantee: when `is_executable`, the
    /// materialized file carries the exec bit. A blob that entered the
    /// store via a mode-less path (CI bank seed, external tar) violates
    /// the 0o555 invariant; on the Linked path this normalizes the SHARED
    /// inode - adding exec to immutable content is safe, write stays off,
    /// and every past and future link of the blob heals with it (run
    /// 29524645875: bank-era build scripts staged 0o100644, EACCES).
    pub async fn link_out_exec(
        &self,
        d: &Dig,
        dest: &std::path::Path,
        is_executable: bool,
    ) -> Result<Materialized> {
        let m = self.link_out(d, dest).await?;
        #[cfg(unix)]
        if is_executable {
            use std::os::unix::fs::PermissionsExt;
            let meta = tokio::fs::metadata(dest).await?;
            if meta.permissions().mode() & 0o111 == 0 {
                let mode = match m {
                    Materialized::Private => 0o755,
                    Materialized::Linked => 0o555,
                };
                tokio::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode)).await?;
            }
        }
        #[cfg(not(unix))]
        let _ = is_executable;
        Ok(m)
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
            // Case-insensitivity collision: a linux-unpacked crate tree can
            // carry files differing only in case (windows-core), and the
            // second hardlink onto NTFS/APFS hits AlreadyExists. Replace -
            // last-wins, the same semantics tar gave these trees when each
            // OS unpacked its own copy (run 29199153837, error 183). Also
            // covers staging over leftovers generally: materialization is
            // declarative, the CAS copy is the truth.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::fs::remove_file(dest).await.with_context(|| {
                    format!("replace existing {} before relink", dest.display())
                })?;
                tokio::fs::hard_link(&src, dest)
                    .await
                    .with_context(|| format!("relink {} -> {}", d.hash, dest.display()))?;
                Ok(Materialized::Linked)
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
    /// Stream a blob INTO the store: chunked read -> hash-while-writing a
    /// tmp file -> verify -> 0o555 -> rename. O(chunk) memory where `put`
    /// is O(blob) - the whole-blob Vec buffering measured 2.4GB peak for
    /// 48x48MB actions on the loopback bench. When `expected` is already
    /// present the reader is still DRAINED of exactly `expected.size`
    /// bytes: batched wire protocols (GetMany) interleave frames and
    /// payloads on one stream, so short-reading desynchronizes the peer.
    pub async fn put_stream(
        &self,
        expected: Option<&Dig>,
        reader: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> Result<Dig> {
        use sha2::{Digest as _, Sha256};
        use tokio::io::AsyncReadExt;
        if let Some(exp) = expected {
            if self.has(exp).await {
                let mut limited = reader.take(exp.size as u64);
                tokio::io::copy(&mut limited, &mut tokio::io::sink()).await?;
                return Ok(exp.clone());
            }
        }
        let tmp = self.root.join("tmp").join(format!(
            "stream.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut total: u64 = 0;
        let limit = expected.map(|e| e.size as u64);
        loop {
            let want = match limit {
                Some(l) if total >= l => 0,
                Some(l) => (l - total).min(buf.len() as u64) as usize,
                None => buf.len(),
            };
            if want == 0 {
                break;
            }
            let n = reader.read(&mut buf[..want]).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tokio::io::AsyncWriteExt::write_all(&mut file, &buf[..n]).await?;
            total += n as u64;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        drop(file);
        let d = hasher.finalize();
        let mut hash = String::with_capacity(64);
        for b in d {
            hash.push_str(&format!("{b:02x}"));
        }
        if let Some(exp) = expected {
            if exp.hash != hash || exp.size as u64 != total {
                let _ = tokio::fs::remove_file(&tmp).await;
                bail!(
                    "digest mismatch: expected {}/{}, got {hash}/{total}",
                    exp.hash,
                    exp.size
                );
            }
        }
        let dest = self.cas_path(&hash);
        if tokio::fs::metadata(&dest).await.is_ok() {
            let _ = tokio::fs::remove_file(&tmp).await;
        } else {
            tokio::fs::create_dir_all(dest.parent().context("cas path has parent")?).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555)).await?;
            }
            tokio::fs::rename(&tmp, &dest).await?;
            self.stored_bytes
                .fetch_add(total, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Dig {
            hash,
            size: total as i64,
        })
    }

    /// Stream a blob OUT of the store into `writer`. O(chunk) memory; the
    /// serve paths previously loaded whole blobs.
    pub async fn copy_out(
        &self,
        d: &Dig,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<()> {
        if d.size == 0 && d.hash == EMPTY_SHA256 {
            return Ok(());
        }
        let mut f = tokio::fs::File::open(self.cas_path(&d.hash)).await?;
        let n = tokio::io::copy(&mut f, writer).await?;
        self.read_bytes
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Streaming content digest of a file (for output ingestion - reading
    /// whole outputs to hash them was half the 2.4GB bench peak).
    pub async fn hash_file(path: &std::path::Path) -> Result<Dig> {
        use sha2::{Digest as _, Sha256};
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        let d = hasher.finalize();
        let mut hash = String::with_capacity(64);
        for b in d {
            hash.push_str(&format!("{b:02x}"));
        }
        Ok(Dig {
            hash,
            size: total as i64,
        })
    }

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

    /// Canonical (name-independent) action cache: keyed by the normalized
    /// action key from [`crate::norm`], holding normalized ActionResults.
    /// A separate namespace so a normalization-scheme change can never be
    /// confused with a digest-keyed AC row.
    pub async fn acn_get(&self, key: &str) -> Option<Vec<u8>> {
        tokio::fs::read(self.root.join("acn").join(&key[..2]).join(key))
            .await
            .ok()
    }

    pub async fn acn_put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.root.join("acn").join(&key[..2]);
        tokio::fs::create_dir_all(&dir).await?;
        let tmp = self
            .root
            .join("tmp")
            .join(format!("acn-{key}.{}", std::process::id()));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, dir.join(key)).await?;
        Ok(())
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

    /// being that this runs on a 7 GB CI runner beside a compiler.
    pub async fn begin_upload(&self) -> Result<Upload> {
        let tmp = self.root.join("tmp").join(format!(
            "up.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let file = tokio::fs::File::create(&tmp).await?;
        Ok(Upload {
            file,
            hasher: <sha2::Sha256 as sha2::Digest>::new(),
            tmp,
            len: 0,
        })
    }

    /// Land a streamed blob in the CAS under its own content hash. Rejects a
    /// digest mismatch (the CAS's whole contract is that a name means its
    /// content), and mirrors [`Store::put`]'s invariants: 0o555 from first
    /// visibility, rename-race tolerant.
    pub async fn finish_upload(&self, mut up: Upload, expected: Option<&str>) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        up.file.flush().await?;
        up.file.sync_all().await?;
        let hash = up.hash();
        if let Some(exp) = expected {
            if exp != hash {
                bail!("digest mismatch: claimed {exp}, got {hash}");
            }
        }
        let dest = self.cas_path(&hash);
        if tokio::fs::metadata(&dest).await.is_ok() {
            // Already held — the upload was redundant. Drop the tmp.
            return Ok(hash);
        }
        tokio::fs::create_dir_all(dest.parent().context("cas path has parent")?).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&up.tmp, std::fs::Permissions::from_mode(0o555)).await?;
        }
        let len = up.len;
        // Defuse the Drop cleanup: from here the tmp file is either renamed
        // into the CAS or explicitly removed below.
        let tmp = std::mem::take(&mut up.tmp);
        if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            if tokio::fs::metadata(&dest).await.is_err() {
                return Err(e).context("persist streamed blob");
            }
        } else {
            self.stored_bytes
                .fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(hash)
    }

    /// Size of blob `hash`, if present. OCI hands us a digest with no size
    /// (unlike REAPI, where every `Digest` carries one), so presence and
    /// `Content-Length` both have to come off the filesystem. Getting the
    /// length wrong is not cosmetic: BuildKit's cache importer caps the config
    /// blob at 1 MiB and rejects a size mismatch *silently*.
    pub async fn size_of(&self, hash: &str) -> Option<u64> {
        if hash == EMPTY_SHA256 {
            return Some(0);
        }
        tokio::fs::metadata(self.cas_path(hash))
            .await
            .ok()
            .map(|m| m.len())
    }

    /// Blob by hash alone. See [`Store::size_of`] for why OCI needs this.
    /// Open a blob for STREAMING, with its length -- the read counterpart of
    /// [`Store::begin_upload`]. A layer is hundreds of MB; anything that only
    /// ever exists whole in memory is a memory bug waiting for a big enough
    /// image.
    pub async fn open_blob(&self, hash: &str) -> Option<(u64, tokio::fs::File)> {
        let path = self.cas_path(hash);
        let file = tokio::fs::File::open(&path).await.ok()?;
        let len = file.metadata().await.ok()?.len();
        Some((len, file))
    }

    pub async fn get_by_hash(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if hash == EMPTY_SHA256 {
            return Ok(Some(Vec::new()));
        }
        match tokio::fs::read(self.cas_path(hash)).await {
            Ok(b) => {
                self.read_bytes
                    .fetch_add(b.len() as u64, std::sync::atomic::Ordering::Relaxed);
                Ok(Some(b))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Tag namespace: an OCI ref (`<repo>:<tag>`) -> the manifest digest it
    /// points at. The manifest bytes themselves are a CAS blob like any other;
    /// a tag is only ever a mutable pointer into it.
    ///
    /// The key is HASHED, not used as a path component: `<repo>` arrives from
    /// an HTTP path and would otherwise be a directory-traversal vector
    /// (`/v2/../../etc/passwd/manifests/x`). Hashing makes traversal
    /// unrepresentable rather than merely filtered.
    fn tag_path(&self, key: &str) -> PathBuf {
        self.root.join("tags").join(sha256_hex(key.as_bytes()))
    }

    pub async fn tag_get(&self, key: &str) -> Option<String> {
        tokio::fs::read_to_string(self.tag_path(key)).await.ok()
    }

    pub async fn tag_put(&self, key: &str, manifest_hash: &str) -> Result<()> {
        let hashed = sha256_hex(key.as_bytes());
        let tmp = self
            .root
            .join("tmp")
            .join(format!("tag-{hashed}.{}", std::process::id()));
        tokio::fs::write(&tmp, manifest_hash).await?;
        tokio::fs::rename(&tmp, self.tag_path(key)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// Case-collision / leftover tolerance: materializing onto an existing
    /// dest replaces it (last-wins - the semantics tar gave case-colliding
    /// trees when each OS unpacked its own copy). Run 29199153837: a
    /// linux-unpacked windows-core tree hit error 183 on NTFS. On macOS
    /// the copy path overwrites natively; elsewhere this exercises the
    /// AlreadyExists relink arm.
    #[tokio::test]
    async fn link_out_replaces_existing_dest() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(dir.path().join("store")).unwrap();
        let d = store.put(None, b"new contents").await.unwrap();
        let dest = dir.path().join("out/README.md");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"stale case-colliding twin").unwrap();
        store.link_out(&d, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new contents");
    }

    /// A blob that entered the store via a mode-less path (CI bank seed,
    /// external tar) violates the 0o555 invariant; the Linked
    /// materialization shares that inode, so an executable input stages
    /// as 0644 and the action dies with EACCES (run 29524645875:
    /// build_script_build mode=0o100644). link_out must normalize the
    /// shared inode when the caller demands exec.
    #[cfg(unix)]
    #[tokio::test]
    async fn link_out_heals_mode_stripped_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(dir.path().join("store")).unwrap();
        let d = store.put(None, b"#!/bin/sh\ntrue\n").await.unwrap();
        // Simulate the mode-less arrival: strip the store inode to 0644.
        let src = store.cas_path_for_test(&d.hash);
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
        let dest = dir.path().join("exec/build_script_build");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        store.link_out_exec(&d, &dest, true).await.unwrap();
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&dest).unwrap();
        let mode = meta.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "materialized executable lacks exec: {mode:o}"
        );
        // Only a SHARED inode (hardlink path) must stay unwritable; a
        // private clone/copy is the caller's to chmod (0o755 is fine).
        if meta.nlink() > 1 {
            assert!(
                mode & 0o222 == 0,
                "shared CAS inode must stay unwritable: {mode:o}"
            );
        }
    }

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

    /// A bare hash must find a blob whose size we never knew - and the
    /// size-carrying path must NOT be used for it. `has()` compares the
    /// recorded size against the file length, so a fabricated size-0 `Dig`
    /// calls a blob we are holding absent, and `put` then rejects the very
    /// bytes it already has. That is why `get_blob_by_hash` exists at all.
    #[tokio::test]
    async fn a_bare_hash_finds_what_a_sizeless_dig_would_miss() {
        let (s, _root) = tmp_store();
        let d = s.put(None, b"a layer of some size").await.unwrap();

        assert_eq!(
            s.get_by_hash(&d.hash).await.unwrap().as_deref(),
            Some(&b"a layer of some size"[..]),
            "by hash alone must find it"
        );
        assert_eq!(s.size_of(&d.hash).await, Some(20));

        // The trap, pinned: pretend we did not know the size.
        let sizeless = Dig {
            hash: d.hash.clone(),
            size: 0,
        };
        assert!(
            !s.has(&sizeless).await,
            "a size-0 Dig calls a held blob absent - this is why OCI lookups \
             may not fabricate one"
        );
        assert!(
            s.put(Some(&sizeless), b"a layer of some size")
                .await
                .is_err(),
            "and putting under one is rejected outright"
        );

        // The empty blob is the one case where size 0 is the truth.
        assert!(
            s.has(&Dig {
                hash: EMPTY_SHA256.into(),
                size: 0
            })
            .await
        );
        assert_eq!(s.get_by_hash(EMPTY_SHA256).await.unwrap(), Some(Vec::new()));
        assert_eq!(s.size_of(EMPTY_SHA256).await, Some(0));

        // A hash we have never seen is a clean miss, not an error.
        assert_eq!(s.get_by_hash(&"f".repeat(64)).await.unwrap(), None);
        assert_eq!(s.size_of(&"f".repeat(64)).await, None);
    }

    fn tmp_store() -> (Store, PathBuf) {
        let root = tempfile::tempdir().unwrap().keep();
        (Store::new(root.clone()).unwrap(), root)
    }

    fn tmp_files(root: &std::path::Path) -> Vec<PathBuf> {
        std::fs::read_dir(root.join("tmp"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    }

    #[tokio::test]
    async fn streamed_upload_matches_a_buffered_put() {
        let (s, _root) = tmp_store();
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();

        let mut up = s.begin_upload().await.unwrap();
        for chunk in payload.chunks(7919) {
            up.write(chunk).await.unwrap();
        }
        assert_eq!(up.len(), payload.len() as u64);
        let streamed = s.finish_upload(up, None).await.unwrap();

        let buffered = s.put(None, &payload).await.unwrap();
        assert_eq!(streamed, buffered.hash);
        assert_eq!(s.get_by_hash(&streamed).await.unwrap().unwrap(), payload);
    }

    /// An upload abandoned mid-flight (client hung up, task cancelled) must
    /// take its tmp file with it. Otherwise a CI runner accretes half-written
    /// layers until the disk dies — and disk death is the failure mode this
    /// whole project exists to avoid.
    #[tokio::test]
    async fn abandoned_upload_leaves_no_tmp_file() {
        let (s, root) = tmp_store();
        {
            let mut up = s.begin_upload().await.unwrap();
            up.write(b"half a layer").await.unwrap();
            assert_eq!(
                tmp_files(&root).len(),
                1,
                "tmp file should exist mid-upload"
            );
        } // dropped without finishing
        assert!(
            tmp_files(&root).is_empty(),
            "abandoned upload leaked its tmp file"
        );
    }

    /// A client that claims a digest its bytes do not have gets refused, and
    /// the rejected bytes must not linger. The CAS's contract is that a name
    /// means its content.
    #[tokio::test]
    async fn streamed_digest_mismatch_is_rejected_and_cleaned_up() {
        let (s, root) = tmp_store();
        let mut up = s.begin_upload().await.unwrap();
        up.write(b"the actual bytes").await.unwrap();

        let lie = "c".repeat(64);
        let e = s.finish_upload(up, Some(&lie)).await.unwrap_err();
        assert!(e.to_string().contains("digest mismatch"), "{e}");
        assert!(
            tmp_files(&root).is_empty(),
            "rejected upload leaked its tmp"
        );
        assert!(s.get_by_hash(&lie).await.unwrap().is_none());
    }

    /// Re-uploading a blob we already hold is a no-op, not a rewrite.
    #[tokio::test]
    async fn redundant_streamed_upload_is_a_noop() {
        let (s, root) = tmp_store();
        let first = s.put(None, b"already here").await.unwrap();

        let mut up = s.begin_upload().await.unwrap();
        up.write(b"already here").await.unwrap();
        let again = s.finish_upload(up, Some(&first.hash)).await.unwrap();

        assert_eq!(again, first.hash);
        assert!(tmp_files(&root).is_empty());
        assert_eq!(
            s.get_by_hash(&again).await.unwrap().unwrap(),
            b"already here"
        );
    }
}
