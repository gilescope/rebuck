//! iroh mesh plumbing shared by driver and worker.
//!
//! Rendezvous is keyless (same trick as experiments/punch): both sides derive
//! the driver's keypair from the shared session string (GITHUB_RUN_ID), so
//! workers know the driver's EndpointId a priori and N0 discovery does the
//! rest. Workers use ephemeral keys — only the driver needs to be findable.
//!
//! Wire format: 4-byte LE length + postcard frame. Blob payloads follow their
//! header frame raw (no re-framing) to keep large transfers zero-copy-ish.

use anyhow::{bail, Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"rebuck2/0";
/// Frames are control messages, not blobs — anything huge is a bug.
const MAX_FRAME: u32 = 64 * 1024 * 1024;

pub fn secret(session: &str, role: &str) -> SecretKey {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"rebuck2-v1\0");
    h.update(session.as_bytes());
    h.update(b"\0");
    h.update(role.as_bytes());
    let seed: [u8; 32] = h.finalize().into();
    SecretKey::from_bytes(&seed)
}

pub fn driver_id(session: &str) -> EndpointId {
    secret(session, "driver").public()
}

/// REAPI digest, serde-friendly (the prost type isn't).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dig {
    pub hash: String,
    pub size: i64,
}

impl From<&bazel_remote_apis::build::bazel::remote::execution::v2::Digest> for Dig {
    fn from(d: &bazel_remote_apis::build::bazel::remote::execution::v2::Digest) -> Self {
        Self {
            hash: d.hash.clone(),
            size: d.size_bytes,
        }
    }
}

impl Dig {
    pub fn to_proto(&self) -> bazel_remote_apis::build::bazel::remote::execution::v2::Digest {
        bazel_remote_apis::build::bazel::remote::execution::v2::Digest {
            hash: self.hash.clone(),
            size_bytes: self.size,
        }
    }
}

/// Space-efficient "what my store holds" summary, gossiped between peers.
/// Blob hashes are uniform (sha256), so probe positions are sliced straight
/// from the hex — no hash functions needed. k=4 at ~12 bits/entry ≈ 0.6% FP;
/// a false positive costs one refused Get, never correctness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bloom {
    pub k: u8,
    pub bits: Vec<u8>,
}

impl Bloom {
    pub fn with_capacity(n: usize) -> Self {
        let mbits = (n.max(64) * 12).next_power_of_two();
        Bloom {
            k: 4,
            bits: vec![0; mbits / 8],
        }
    }

    fn idx(&self, hash: &str, i: u8) -> Option<usize> {
        let start = (i as usize) * 8;
        let h = u64::from_str_radix(hash.get(start..start + 8)?, 16).ok()?;
        Some((h as usize) & (self.bits.len() * 8 - 1))
    }

    pub fn insert(&mut self, hash: &str) {
        for i in 0..self.k {
            if let Some(b) = self.idx(hash, i) {
                self.bits[b / 8] |= 1 << (b % 8);
            }
        }
    }

    pub fn contains(&self, hash: &str) -> bool {
        (0..self.k).all(|i| {
            self.idx(hash, i)
                .map(|b| self.bits[b / 8] & (1 << (b % 8)) != 0)
                .unwrap_or(false)
        })
    }
}

/// Worker → driver, on the control stream.
#[derive(Debug, Serialize, Deserialize)]
pub enum W2D {
    Hello {
        os: String,
        arch: String,
        slots: u32,
        /// CI shard this worker restored before joining (see the sweep
        /// workflows). Finalize assigns it the SAME shard back - its store
        /// is rich in exactly that range; join-order round-robin repacked
        /// ranges the assignee barely held, thinning the pool every lap.
        preloaded_shard: Option<u8>,
    },
    /// prost-encoded ActionResult. `stored` lists blob hashes this action
    /// persisted on the worker — the driver's provider index in
    /// decentralized mode (empty when outputs were uploaded).
    Done {
        job: u64,
        action_result: Vec<u8>,
        stored: Vec<String>,
    },
    Failed {
        job: u64,
        msg: String,
    },
    /// Periodic summary of the worker's store (bloom gossip).
    Holdings {
        bloom: Bloom,
    },
    /// Shard sync complete; this worker's job will save the shard entry.
    Finalized {
        shard: u8,
    },
}

/// Driver → worker, on the control stream.
#[derive(Debug, Serialize, Deserialize)]
pub enum D2W {
    /// First frame after Hello: session-wide mode flags.
    Welcome {
        decentralized: bool,
    },
    Run {
        job: u64,
        action: Dig,
    },
    /// Rebroadcast of every peer's holdings: (endpoint id, bloom).
    Blooms {
        peers: Vec<(String, Bloom)>,
    },
    /// Post-build: sync + own snapshot shard `shard` (of `of`), then exit.
    Finalize {
        shard: u8,
        of: u8,
    },
}

/// Worker → driver, each on a fresh bi-stream (header, then raw bytes for Put).
#[derive(Debug, Serialize, Deserialize)]
pub enum BlobReq {
    Get(Dig),
    Put(Dig),
    /// Exact presence check for a batch — the honesty layer over bloom
    /// gossip (blooms route, HasMany confirms; FindMissingBlobs must never
    /// lie to buck2).
    HasMany(Vec<Dig>),
    /// All store hashes whose shard (first hex nibble / 2 when of=8) is
    /// `shard`. Used by workers syncing their assigned snapshot shard.
    ListShard {
        shard: u8,
        of: u8,
    },
    /// Batch fetch: the reply is one `BlobResp` frame PER digest, in request
    /// order (`Found` frames followed immediately by that blob's raw bytes),
    /// all on the same stream. One round-trip where `Get` cost one per blob —
    /// sequential per-file staging at ~12 RTT-bound fetches/s was a 20-minute
    /// pre-rustc stall on the big crate forests (run 29160244348).
    GetMany(Vec<Dig>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BlobResp {
    /// Raw bytes follow on the same stream.
    Found {
        size: u64,
    },
    Missing,
    /// Decentralized mode: the bytes live on this peer — fetch direct.
    Provider {
        endpoint: String,
    },
    PutOk,
    /// Reply to HasMany, same order as the request.
    HaveMany(Vec<bool>),
    /// Reply to ListShard.
    HashList(Vec<Dig>),
    Err(String),
}

pub async fn send_frame<T: Serialize>(s: &mut SendStream, v: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(v)?;
    let len: u32 = bytes.len().try_into().context("frame > 4 GiB")?;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    s.write_all(&len.to_le_bytes()).await?;
    s.write_all(&bytes).await?;
    Ok(())
}

/// None on clean EOF before a frame starts.
pub async fn recv_frame<T: serde::de::DeserializeOwned>(r: &mut RecvStream) -> Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(_) => return Ok(None), // stream finished/reset — treat as EOF
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)
        .await
        .context("frame body truncated")?;
    Ok(Some(postcard::from_bytes(&buf)?))
}

/// Read exactly `size` raw bytes following a header frame.
pub async fn recv_raw(r: &mut RecvStream, size: u64) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; usize::try_from(size).context("blob > usize")?];
    r.read_exact(&mut buf)
        .await
        .context("blob body truncated")?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_no_false_negatives_and_sane_fp() {
        // Production hashes are sha256 — uniform across all hex positions.
        let fake = |i: u32| crate::store::sha256_hex(&i.to_le_bytes());
        let n = 5000;
        let mut b = Bloom::with_capacity(n);
        for i in 0..n as u32 {
            b.insert(&fake(i));
        }
        for i in 0..n as u32 {
            assert!(b.contains(&fake(i)), "false negative at {i}");
        }
        let fps = (n as u32..3 * n as u32)
            .filter(|i| b.contains(&fake(*i)))
            .count();
        let rate = fps as f64 / (2.0 * n as f64);
        assert!(rate < 0.05, "false-positive rate too high: {rate}");
    }
}
