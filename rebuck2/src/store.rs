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

pub struct Store {
    root: PathBuf,
    /// Bytes newly persisted to the CAS (disk-pressure signal).
    pub stored_bytes: std::sync::atomic::AtomicU64,
    /// Bytes read out of the CAS (egress-saturation proxy — mesh Gets and
    /// gRPC reads all funnel through here).
    pub read_bytes: std::sync::atomic::AtomicU64,
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        for sub in ["cas", "ac", "tmp"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self {
            root,
            stored_bytes: std::sync::atomic::AtomicU64::new(0),
            read_bytes: std::sync::atomic::AtomicU64::new(0),
        })
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
}

#[cfg(test)]
mod tests {
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
