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

/// Worker → driver, on the control stream.
#[derive(Debug, Serialize, Deserialize)]
pub enum W2D {
    Hello {
        os: String,
        arch: String,
        slots: u32,
    },
    /// prost-encoded ActionResult.
    Done {
        job: u64,
        action_result: Vec<u8>,
    },
    Failed {
        job: u64,
        msg: String,
    },
}

/// Driver → worker, on the control stream.
#[derive(Debug, Serialize, Deserialize)]
pub enum D2W {
    Run { job: u64, action: Dig },
}

/// Worker → driver, each on a fresh bi-stream (header, then raw bytes for Put).
#[derive(Debug, Serialize, Deserialize)]
pub enum BlobReq {
    Get(Dig),
    Put(Dig),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BlobResp {
    /// Raw bytes follow on the same stream.
    Found {
        size: u64,
    },
    Missing,
    PutOk,
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
