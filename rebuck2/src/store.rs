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

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        for sub in ["cas", "ac", "tmp"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self { root })
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
            Ok(b) => Ok(Some(b)),
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
            let tmp = self
                .root
                .join("tmp")
                .join(format!("{hash}.{}", std::process::id()));
            tokio::fs::write(&tmp, bytes).await?;
            // rename is atomic; a concurrent identical write wins harmlessly
            tokio::fs::rename(&tmp, &dest).await?;
        }
        Ok(Dig {
            hash,
            size: bytes.len() as i64,
        })
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
