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
    /// "No." Refusal IS the backpressure (principle 12): a driver that
    /// cannot place work has learned the fleet is saturated without needing
    /// a metric to tell it, and a protocol where the coordinator ASSIGNS
    /// cannot express this and would have to rediscover it as a load
    /// signal, later and worse.
    Decline {
        job: u64,
        why: String,
    },
    /// A subtree was built and published. `image_ref` is what the requester
    /// pulls - from the BUILDER's mirror, not from the driver, which is
    /// principle 6 in one field.
    ///
    /// Not `Done`: that carries a REAPI `ActionResult`, and a subtree's
    /// result is an image. Squeezing one into the other would make the
    /// fleet's reply type mean two things.
    Led {
        job: u64,
        image_ref: String,
    },
    /// "Place this for me." A worker that has subdivided its tree asks the
    /// driver to find a peer for one branch. The driver arbitrates; the
    /// subtree and its result never pass through it.
    Offer {
        subtree: Vec<u8>,
        frontier: Vec<Dig>,
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
    /// Liveness heartbeat (every 20s). Workers treat a long heartbeat gap
    /// as a dead driver and exit: a SIGTERM'd/crashed driver sends no QUIC
    /// close, and a worker blocked in recv_frame otherwise idles until its
    /// CI timeout cap (win worker 1, run 29194749613: 5h of dead runner).
    /// `vitals` (Some once a minute): driver memory/disk readout, printed
    /// by workers so the driver's last-known vitals SURVIVE a driver-box
    /// death - run 29232220897's OOM verdict was circumstantial because
    /// the evidence died with the runner.
    Ping {
        vitals: Option<String>,
    },
    /// Orderly shutdown: exit now (driver teardown, no shard assignment).
    Exit,
    /// Build this subtree on someone else's behalf - an OFFER, never an
    /// assignment. Answer with [`W2D::Decline`] to refuse it.
    ///
    /// `subtree` is a serialised buildkit `pb.Definition`; `frontier` is the
    /// blobs the peer needs to start, which it fetches over the mesh. The
    /// driver arbitrates and carries NEITHER (principle 6) - the frontier
    /// and the result travel peer to peer, and the test for that is blunt:
    /// after the build, look at the driver's disk.
    Lead {
        job: u64,
        subtree: Vec<u8>,
        frontier: Vec<Dig>,
    },
    /// Your offered subtree was built by a peer; pull it from there.
    Placed {
        job: u64,
        image_ref: String,
    },
    /// Nobody took it. Build it yourself - which is what you would have
    /// done without dispatch, so this is the normal ending and not an
    /// error. `why` is for the log, not for a decision.
    Unplaced {
        job: u64,
        why: String,
    },
    /// These blobs will be wanted. Fetch them NOW, before anyone asks.
    ///
    /// Distribution is otherwise entirely lazy, and the trace timeline says
    /// what that costs: workers fetch nothing for the whole 284s baseline
    /// leg, then nothing again until 227s into the fleet leg - the bulk
    /// transfer lands exactly when the base chain finishes and the fan-out
    /// wants it. The layer that finished five minutes earlier sat on one
    /// machine until somebody asked.
    ///
    /// So the driver says so as soon as an image exists, while its consumer
    /// is still building on top of it. Combined with `seeder_for`, six
    /// workers take six different sixths off the origin at once, and the
    /// fan-out waits only for the layer that was genuinely not ready.
    ///
    /// Advisory, always: a worker that ignores this, or fails every fetch,
    /// builds exactly what it would have built anyway, one lazy pull later.
    ///
    /// APPENDED LAST, and it has to be: postcard encodes enum variants by
    /// INDEX, so inserting one anywhere else silently renumbers every
    /// variant after it and a worker built from the other commit reads a
    /// Lead as a Ping.
    Prefetch {
        digests: Vec<Dig>,
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
    /// Fetch knowing only the hash.
    ///
    /// Every other request carries a `Dig`, because REAPI always knows the
    /// size. A REGISTRY does not: buildkit asks for
    /// `/v2/<repo>/blobs/sha256:<hex>` and the size is what the answer is
    /// supposed to tell it. Without this a mesh-backed registry cannot ask
    /// the fleet for anything, which is why there was not one.
    ///
    /// LAST, not inserted: postcard encodes a variant by index, so a new one
    /// in the middle would reinterpret every later variant on a mixed-version
    /// fleet. A peer that predates this replies `Err`, which the caller
    /// treats as "not here" - the same as a miss.
    GetByHash(String),
    /// Resolve a TAG the fleet may hold, when this registry does not.
    ///
    /// Content moves by digest everywhere it can, and three separate fixes
    /// went into making that true - a tag is a name in one machine's
    /// namespace and the fleet has no namespace. But buildkit's own registry
    /// CACHE is addressed by tag and nothing else (`type=registry,ref=...`),
    /// and warm caches are the measured reason six machines are slower than
    /// one: go-mod and go-build cost ~24s per lead, paid once by a single
    /// machine and once PER WORKER by a fleet.
    ///
    /// So the tag namespace has to be shared after all, for this one purpose.
    /// Resolution, not replication: the answer is a manifest hash, and the
    /// manifest and its blobs then travel by content as everything else does.
    ///
    /// LAST, for the same reason `GetByHash` is: postcard encodes a variant
    /// by index, so inserting in the middle reinterprets every later variant
    /// on a mixed-version fleet.
    TagGet(String),
    /// `GetByHash`, but saying who is asking.
    ///
    /// The driver keeps every worker's bloom and rebroadcasts them, but a
    /// worker acts on the copy it last received. On a cold fleet all workers
    /// need the same base layers at once, nobody holds them yet, and every
    /// one of them asks the DRIVER - measured at driver=47 against peer=5 per
    /// worker, 9.4 GiB served, with the coordinator squarely on the data path
    /// it is supposed to stay off.
    ///
    /// The driver's view is fresher than any worker's. Told who is asking, it
    /// can answer `Provider` and name a peer that has since acquired the blob,
    /// instead of sending the bytes a third time.
    ///
    /// Appended last: postcard encodes by index.
    GetByHashAs {
        hash: String,
        me: String,
    },
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
    /// Reply to TagGet: the manifest hash this tag names, if the peer has it.
    ///
    /// AFTER `Err`, not before it. Appending means appending: postcard
    /// encodes by index, and putting this one variant above `Err` renumbers
    /// `Err` for every peer that has not been restarted - so an old node's
    /// error frame would decode as a tag answer. Written as "appended last"
    /// and placed second-to-last on the first attempt, which is precisely
    /// how that mistake gets made.
    Tag(Option<String>),
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

    #[test]
    fn prefetch_is_the_last_variant_and_stays_there() {
        use super::D2W;

        // postcard encodes an enum variant by its INDEX. Insert one anywhere
        // but the end and every variant after it silently renumbers, so a
        // worker built from one commit reads a Lead as a Ping - no error, no
        // mismatch, just a build doing the wrong thing. Append-only is the
        // whole contract and nothing else enforces it.
        let last = D2W::Prefetch { digests: vec![] };
        let bytes = postcard::to_allocvec(&last).expect("encode");
        let idx = bytes[0];

        // Every other variant must encode to a LOWER index, which is the
        // machine-checkable form of "Prefetch is last".
        for other in [
            D2W::Welcome {
                decentralized: false,
            },
            D2W::Exit,
            D2W::Ping { vitals: None },
            D2W::Blooms { peers: vec![] },
            D2W::Finalize { shard: 0, of: 1 },
        ] {
            let b = postcard::to_allocvec(&other).expect("encode");
            assert!(
                b[0] < idx,
                "{other:?} encodes at {} but Prefetch is {idx} - a variant was inserted, not appended",
                b[0]
            );
        }

        // And it survives a round trip with a payload, since an empty vec
        // would pass even if the fields were wrong.
        let sent = D2W::Prefetch {
            digests: vec![super::Dig {
                hash: "abc".into(),
                size: 7,
            }],
        };
        let back: D2W =
            postcard::from_bytes(&postcard::to_allocvec(&sent).expect("encode")).expect("decode");
        match back {
            D2W::Prefetch { digests } => {
                assert_eq!(digests.len(), 1);
                assert_eq!(digests[0].hash, "abc");
                assert_eq!(digests[0].size, 7);
            }
            other => panic!("decoded as {other:?}"),
        }
    }
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
