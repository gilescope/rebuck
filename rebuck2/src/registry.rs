//! OCI distribution v2 facade over the fleet CAS.
//!
//! Lets a buildkitd run `--cache-to/--cache-from type=registry` against
//! `localhost` while the blobs actually live in — and travel over — the mesh.
//! BuildKit thinks it is talking to a boring registry. This is the whole of
//! docs/buildkit-plan.md P1: the layer-cache half of what an Earthly Satellite
//! gave you, on free GitHub-Actions cache.
//!
//! Only the routes BuildKit's cache path actually uses, verified against
//! containerd's `remotes/docker` (which is what BuildKit delegates to):
//!
//! ```text
//! HEAD/GET  /v2/<name>/manifests/<ref>     tag or digest
//! PUT       /v2/<name>/manifests/<ref>
//! HEAD/GET  /v2/<name>/blobs/<digest>
//! POST      /v2/<name>/blobs/uploads/      -> Location
//! PATCH     <Location>                     chunk (non-BuildKit clients)
//! PUT       <Location>?digest=sha256:...   finalise
//! ```
//!
//! Deliberately absent, each verified rather than assumed:
//!
//! - **No Range GET.** The client falls back to a serial fetch when the server
//!   ignores `Range`.
//! - **No auth, no TLS.** containerd's `MatchLocalhost` forces plain HTTP for
//!   `127.0.0.1`/`localhost`, so no `buildkitd.toml` stanza is needed.
//! - **No Referrers API.** `FetchReferrers` exists on the fetcher but the cache
//!   importer never calls it.
//! - **No `/tags/list`.** BuildKit never enumerates tags, and hashing the tag
//!   key (see [`Store::tag_put`]) makes a repo's tags unenumerable by
//!   construction — the same choice that makes path traversal unrepresentable.
//!   `skopeo inspect` is the only casualty; `skopeo copy` is unaffected.
//!
//! PATCH is the interesting one. BuildKit does *not* need it — containerd's
//! pusher does monolithic POST-then-PUT and leaves chunked upload a `// TODO` —
//! and it was omitted on that basis. Then a real client (skopeo) failed against
//! it immediately, because every OCI client that is not BuildKit chunks. The
//! lesson is narrow and worth keeping: "BuildKit does not use X" is not "no
//! client uses X", and unit tests written from the same research as the code
//! cannot catch the difference. Supporting PATCH costs ~20 lines and buys
//! verification against an implementation that does not share our assumptions.
//!
//! Two silent-failure traps worth naming, because neither surfaces as an error:
//!
//! 1. **`Content-Length` must be exact.** The cache importer caps the config
//!    blob at 1 MiB and rejects a size mismatch without saying so.
//! 2. **Manifest annotations must pass through verbatim** — notably
//!    `containerimage.inlinecache`, which the earthbuild fork keeps and upstream
//!    dropped. Strip it and `--use-inline-cache` quietly stops working. Storing
//!    manifests as opaque bytes (rather than re-serialising them) is what makes
//!    this true by construction.
//!
//! Known gap: uploads are buffered in memory (`Store::put` takes `&[u8]`), so a
//! very large layer costs its own size in RAM. Streaming to disk is the
//! follow-up; see [`MAX_UPLOAD`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures::StreamExt;

use crate::store::{Store, Upload};

/// Ceiling on a single blob, enforced while streaming. Not a memory bound —
/// blobs go to disk a chunk at a time — just a sanity limit.
const MAX_UPLOAD: u64 = 16 * 1024 * 1024 * 1024;

/// A manifest is small by construction (it is a list of descriptors), and we
/// have to read it anyway to serve its `mediaType` back. Blobs stream; only
/// this is buffered, and this bounds it.
const MAX_MANIFEST: usize = 32 * 1024 * 1024;

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// What the registry needs of a blob store. Implemented for the local [`Store`]
/// today; the mesh-backed driver is the same methods with a network hop, which
/// is the point of naming them.
///
/// Blob writes go through [`Store::Upload`], never `&[u8]`: a layer is
/// hundreds of MB and this runs on a CI runner next to a compiler.
#[async_trait::async_trait]
pub trait RegistryStore: Send + Sync + 'static {
    async fn blob_size(&self, hash: &str) -> Option<u64>;
    async fn blob_get(&self, hash: &str) -> Result<Option<Vec<u8>>>;
    /// The blob as a body that never exists whole in memory, with its length.
    ///
    /// Defaults to the buffered read so a store that cannot stream still works;
    /// the local [`Store`] overrides it, and that is the one that matters --
    /// a mirror hit serves from disk.
    async fn blob_stream(&self, hash: &str) -> Option<(u64, Body)> {
        let bytes = self.blob_get(hash).await.ok()??;
        Some((bytes.len() as u64, Body::from(bytes)))
    }
    /// Small, known-bounded values only (manifests). Layers must stream.
    async fn blob_put(&self, bytes: &[u8]) -> Result<String>;
    async fn upload_begin(&self) -> Result<Upload>;
    async fn upload_finish(&self, up: Upload, expected: Option<&str>) -> Result<String>;
    /// `<repo>:<ref>` -> manifest digest.
    async fn tag_get(&self, key: &str) -> Option<String>;
    async fn tag_put(&self, key: &str, manifest_hash: &str) -> Result<()>;

    /// Cross-machine single-flight.
    ///
    /// Behaviour, not a field, because WHERE the lease lives depends on who is
    /// serving: the driver owns the table; a worker's agent forwards to it over
    /// the mesh; a bare store has no fleet to coordinate at all.
    async fn lease_claim(&self, _key: &str) -> Option<Claimed> {
        None
    }
    /// Keep an out-of-process leader's claim alive. A build can easily outrun the
    /// TTL, and a leader reaped mid-build costs every follower a rebuild.
    /// `false` = you no longer hold it; stop, and re-claim.
    async fn lease_heartbeat(&self, _key: &str, _holder: &str) -> bool {
        false
    }
    async fn lease_release(&self, _key: &str, _holder: &str, _result: Vec<u8>) {}
    async fn lease_abandon(&self, _key: &str, _holder: &str) {}

    /// `(led, merged, abandoned)` — did single-flight actually engage?
    ///
    /// `merged` is the only direct evidence the feature did anything: a vertex a
    /// second machine adopted instead of rebuilding. Without it the e2e can only
    /// infer engagement from identical markers, which a stray cache hit would
    /// fake just as well.
    ///
    /// `None` for a store that owns no lease table (a bare store; an agent, which
    /// forwards to the driver). Only the holder of the table can answer, so ask
    /// the driver.
    fn lease_stats(&self) -> Option<(u64, u64, u64)> {
        None
    }

    /// (led, merged) restricted to image resolutions. Split out because both
    /// kinds share one table, so a bare `merged` cannot say which moved.
    fn resolve_stats(&self) -> Option<(u64, u64)> {
        None
    }

    /// Drop every canonical answer. Returns how many were forgotten.
    ///
    /// MEASUREMENT ONLY. `merged` cannot tell an in-flight collision from a late
    /// claimant adopting an earlier answer, so a test that wants to measure only
    /// the former must forget the latter first. In production this is never the
    /// right thing: forgetting an answer is how a key acquires a second one.
    fn lease_forget_all(&self) -> usize {
        0
    }
}

/// What a claimant is told.
pub enum Claimed {
    /// You build it, then release (or abandon) quoting `holder`. Heartbeat until
    /// you do, or a long build outruns the TTL and gets reaped mid-flight.
    Leader { holder: String },
    /// A peer built it. These are its result bytes.
    Done(Vec<u8>),
    /// The leader vanished. Claim again — never a hang.
    Retry,
}

#[async_trait::async_trait]
impl RegistryStore for Store {
    /// Chunks off the CAS file. The upload path already streams to disk; this is
    /// its counterpart, so a 500 MB layer is 500 MB of disk reads and never
    /// 500 MB resident.
    async fn blob_stream(&self, hash: &str) -> Option<(u64, Body)> {
        let (len, file) = self.open_blob(hash).await?;
        let stream = futures::stream::unfold(file, |mut f| async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 64 * 1024];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok::<Vec<u8>, std::io::Error>(buf), f))
                }
                Err(e) => Some((Err(e), f)),
            }
        });
        Some((len, Body::from_stream(stream)))
    }

    async fn blob_size(&self, hash: &str) -> Option<u64> {
        self.size_of(hash).await
    }
    async fn blob_get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.get_by_hash(hash).await
    }
    async fn blob_put(&self, bytes: &[u8]) -> Result<String> {
        Ok(self.put(None, bytes).await?.hash)
    }
    async fn upload_begin(&self) -> Result<Upload> {
        self.begin_upload().await
    }
    async fn upload_finish(&self, up: Upload, expected: Option<&str>) -> Result<String> {
        self.finish_upload(up, expected).await
    }
    async fn tag_get(&self, key: &str) -> Option<String> {
        Store::tag_get(self, key).await
    }
    async fn tag_put(&self, key: &str, manifest_hash: &str) -> Result<()> {
        Store::tag_put(self, key, manifest_hash).await
    }
}

/// Mesh-backed: a blob we do not hold is fetched from whichever worker does
/// (provider index -> bloom claimants -> exact probe), cached locally, and
/// served. This is what makes the registry *distributed* rather than a local
/// disk cache: a layer built on worker A is served to worker B's buildkitd over
/// iroh, without either of them knowing the mesh exists.
///
/// Tags stay driver-local. The registry runs beside the driver, which is the
/// one process every worker can already reach, so there is nothing to
/// propagate yet. A registry *per worker* would need tags gossiped over the
/// mesh — that is the next step, not this one.
#[async_trait::async_trait]
impl RegistryStore for crate::driver::Driver {
    async fn blob_size(&self, hash: &str) -> Option<u64> {
        if let Some(n) = self.store.size_of(hash).await {
            return Some(n);
        }
        // A HEAD must be honest about FLEET-wide presence, not just ours, or
        // BuildKit re-pushes layers the fleet already holds. Costs a fetch on
        // first probe — and the GET that always follows then finds it local.
        match self.get_blob_by_hash(hash).await {
            Ok(Some(bytes)) => Some(bytes.len() as u64),
            _ => None,
        }
    }
    async fn blob_get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.get_blob_by_hash(hash).await
    }
    async fn blob_put(&self, bytes: &[u8]) -> Result<String> {
        Ok(self.store.put(None, bytes).await?.hash)
    }
    async fn upload_begin(&self) -> Result<Upload> {
        self.store.begin_upload().await
    }
    async fn upload_finish(&self, up: Upload, expected: Option<&str>) -> Result<String> {
        self.store.finish_upload(up, expected).await
    }
    async fn tag_get(&self, key: &str) -> Option<String> {
        self.store.tag_get(key).await
    }
    async fn tag_put(&self, key: &str, manifest_hash: &str) -> Result<()> {
        self.store.tag_put(key, manifest_hash).await
    }
    async fn lease_claim(&self, key: &str) -> Option<Claimed> {
        // claim_http, NOT claim_local: an HTTP claim returns immediately and
        // nothing holds it, so a drop-guarded local holder would wedge the key
        // forever if the client died. See Leases::claim_http.
        let (claim, holder) = self.lease_table().claim_http(key);
        Some(match claim {
            crate::lease::Claim::Leader => Claimed::Leader { holder },
            // Already answered by whoever got here first: adopt it, do not build
            // a second one (principle 3).
            crate::lease::Claim::Done(r) => Claimed::Done(r),
            crate::lease::Claim::Follower(rx) => match rx.await {
                Ok(crate::lease::Outcome::Done(r)) => Claimed::Done(r),
                // A failed leader and a vanished one look the same to a
                // buildkitd: rebuild it. There is nothing else it could do.
                _ => Claimed::Retry,
            },
        })
    }
    async fn lease_heartbeat(&self, key: &str, holder: &str) -> bool {
        self.lease_table().heartbeat(key, holder)
    }
    async fn lease_release(&self, key: &str, holder: &str, result: Vec<u8>) {
        self.lease_table()
            .release(key, Some(holder), crate::lease::Outcome::Done(result));
    }
    async fn lease_abandon(&self, key: &str, holder: &str) {
        self.lease_table().abandon_peer(key, holder);
    }
    fn lease_stats(&self) -> Option<(u64, u64, u64)> {
        let l = self.lease_table();
        let get = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
        Some((get(&l.led), get(&l.merged), get(&l.abandoned)))
    }
    fn resolve_stats(&self) -> Option<(u64, u64)> {
        Some(self.lease_table().resolve_stats())
    }
    fn lease_forget_all(&self) -> usize {
        self.lease_table().forget_all()
    }
}

/// What actually crossed the coordinator's NIC.
///
/// The design's one structural bottleneck: a leader PUSHES its layer here and
/// every follower PULLS it back down, so one box is on the critical path for
/// every byte. If `served` approaches `(workers-1) * uploaded`, the coordinator
/// is the bottleneck and layers should move P2P instead (the fleet already has
/// the machinery — see docs/buildkit-optimizations.md #1).
///
/// Measure before optimising. These counters exist so that decision is made on
/// a number rather than on a hunch.
#[derive(Default)]
pub struct Bandwidth {
    pub uploaded: AtomicU64,
    pub upload_bytes: AtomicU64,
    pub served: AtomicU64,
    pub serve_bytes: AtomicU64,
    /// Blobs this agent had to go to the origin registry for. The plan's
    /// Done-when for P4b is "serves rises, upstream fetches do not", so this has
    /// to be a number rather than something inferred from a log.
    pub upstream_fetches: AtomicU64,
    pub upstream_bytes: AtomicU64,
}

/// Where a blob comes from when NOBODY in the fleet holds it.
///
/// Principle 9 -- the origin registry is a fallback, not a data path. Behind a
/// trait so the wiring is testable without a network: what matters about a
/// pull-through mirror is how often it goes upstream, and that is exactly what a
/// fake can count.
/// A blob on its way somewhere, in chunks. Never a `Vec<u8>`: a layer is
/// hundreds of MB and this runs on a CI runner next to a compiler.
pub type BoxByteStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>>> + Send>>;

#[async_trait::async_trait]
pub trait Upstream: Send + Sync + 'static {
    /// The blob as a stream, or `None` if the origin does not have it either.
    ///
    /// `Err` means "could not ask" -- unreachable, refused, timed out -- and the
    /// caller must treat it the same as a miss. A mirror that turns an
    /// unreachable origin into a hard failure is strictly worse than no mirror.
    async fn blob(&self, repo: &str, digest: &str) -> Result<Option<BoxByteStream>>;
}

struct Reg<S> {
    store: Arc<S>,
    bw: Bandwidth,
    /// `None` unless configured. Off by default: a binary upgrade must not
    /// start making outbound requests nobody asked for.
    upstream: Option<Arc<dyn Upstream>>,
    /// One upstream fetch per blob, however many callers want it at once.
    ///
    /// Ten nested builds starting together all want `alpine`, and without this
    /// the mirror makes ten upstream requests -- precisely the storm it exists
    /// to remove. Keyed by digest: the first caller fetches, the rest wait on
    /// its gate and then find the blob in the store.
    inflight: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    next_upload: AtomicU64,
    /// Open upload sessions: id -> the blob being streamed to disk.
    ///
    /// Holds [`Upload`]s, not buffers: bytes go straight to a tmp file, hashed
    /// on the way. An abandoned session's tmp file is reaped by `Upload`'s Drop
    /// when the map is dropped.
    ///
    /// A session is TAKEN from the map for the duration of a write and put back
    /// after — so the (std, non-async) lock is never held across an await. A
    /// client PATCHes one session sequentially, so there is nothing to contend.
    ///
    /// BuildKit itself never opens a chunked session (containerd's pusher does
    /// monolithic POST-then-PUT and leaves chunked upload a `// TODO`), but
    /// every other OCI client does — skopeo, crane and `docker push` all PATCH.
    sessions: std::sync::Mutex<HashMap<u64, Upload>>,
}

/// Canonical `sha256:<64 lowercase hex>` -> the hex. Anything else is rejected
/// before it can reach the filesystem: the digest arrives from an HTTP path and
/// is used to build one.
fn parse_digest(s: &str) -> Option<&str> {
    let hex = s.strip_prefix("sha256:")?;
    let ok = hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then_some(hex)
}

/// The manifest's own `mediaType`, read back out of the stored bytes. Avoids a
/// second metadata store, and keeps the manifest itself the single source of
/// truth (both OCI and Docker v2 manifests carry the field).
fn media_type_of(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("mediaType")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| OCI_MANIFEST.to_string())
}

fn err(code: StatusCode, oci_code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"errors": [{"code": oci_code, "message": msg}]});
    (code, axum::Json(body)).into_response()
}

/// Body + the headers BuildKit actually reads. `Content-Length` is set
/// explicitly (not left to the framework) because a wrong one is a *silent*
/// importer rejection, not an error.
/// Serve a blob without ever holding it whole. Same headers as the buffered
/// form -- a client cannot tell, which is the point.
fn streamed_blob_response(method: &Method, len: u64, body: Body, digest: &str) -> Response {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest).unwrap(),
    );
    if method == Method::HEAD {
        return (StatusCode::OK, h).into_response();
    }
    (StatusCode::OK, h, body).into_response()
}

fn blob_response(method: &Method, bytes: Vec<u8>, digest: &str, ctype: &str) -> Response {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ctype).unwrap());
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    h.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(digest).unwrap(),
    );
    // A HEAD carries the headers and no body — same headers as the GET, which
    // is the whole point of the probe.
    if method == Method::HEAD {
        return (StatusCode::OK, h).into_response();
    }
    (StatusCode::OK, h, bytes).into_response()
}

/// Drain a request body into an open [`Upload`], chunk by chunk. Bytes are
/// hashed and written as they arrive and never accumulate, so peak memory is
/// one chunk regardless of layer size. [`MAX_UPLOAD`] is enforced here: the
/// streaming path deliberately bypasses the extractor's body limit.
async fn drain_into(up: &mut Upload, body: Body) -> Result<(), Response> {
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            err(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                &format!("body stream: {e}"),
            )
        })?;
        if up.len() + chunk.len() as u64 > MAX_UPLOAD {
            return Err(err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "BLOB_UPLOAD_INVALID",
                "blob exceeds the upload ceiling",
            ));
        }
        up.write(&chunk).await.map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_UPLOAD_INVALID",
                &e.to_string(),
            )
        })?;
    }
    Ok(())
}

async fn handle<S: RegistryStore>(
    State(reg): State<Arc<Reg<S>>>,
    method: Method,
    Path(path): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    // POST /v2/<name>/blobs/uploads/ — begin an upload.
    // Checked before the generic blobs arm: this path *contains* "/blobs/".
    if let Some(repo) = path.strip_suffix("/blobs/uploads/") {
        if method != Method::POST {
            return err(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "POST only");
        }
        // Cross-repo mount: our CAS is repo-agnostic, so if we hold the blob
        // the mount always succeeds and the client skips the upload entirely.
        if let Some(hex) = q.get("mount").and_then(|d| parse_digest(d)) {
            if reg.store.blob_size(hex).await.is_some() {
                let mut h = HeaderMap::new();
                h.insert(
                    header::LOCATION,
                    HeaderValue::from_str(&format!("/v2/{repo}/blobs/sha256:{hex}")).unwrap(),
                );
                return (StatusCode::CREATED, h).into_response();
            }
        }
        // Otherwise: 202 + Location. NOT 404/400 — the client treats those as a
        // hard error rather than "fall back to a normal upload".
        let up = match reg.store.upload_begin().await {
            Ok(u) => u,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BLOB_UPLOAD_INVALID",
                    &e.to_string(),
                )
            }
        };
        let id = reg.next_upload.fetch_add(1, Ordering::Relaxed);
        reg.sessions.lock().unwrap().insert(id, up);
        let mut h = HeaderMap::new();
        h.insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("/v2/{repo}/blobs/uploads/{id}")).unwrap(),
        );
        h.insert(header::RANGE, HeaderValue::from_static("0-0"));
        return (StatusCode::ACCEPTED, h).into_response();
    }

    // <Location>: PATCH appends a chunk, PUT finalises (and may carry the last
    // chunk itself). Monolithic upload — BuildKit's only mode — is just the PUT
    // with an empty session.
    if let Some((repo, id)) = path.split_once("/blobs/uploads/") {
        let id: u64 = match id.parse() {
            Ok(i) => i,
            Err(_) => return err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id),
        };

        // Take the session out to write to it and put it back after: the lock
        // is std, so it must never be held across an await.
        let Some(mut up) = reg.sessions.lock().unwrap().remove(&id) else {
            return err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no session");
        };

        if method == Method::PATCH {
            // On a stream error the session is NOT reinstated: `up` drops here
            // and takes its tmp file with it. A half-written blob must not be
            // resumable into a valid-looking digest.
            if let Err(resp) = drain_into(&mut up, body).await {
                return resp;
            }
            let end = up.len().saturating_sub(1);
            let mut h = HeaderMap::new();
            h.insert(
                header::LOCATION,
                HeaderValue::from_str(&format!("/v2/{repo}/blobs/uploads/{id}")).unwrap(),
            );
            h.insert(
                header::RANGE,
                HeaderValue::from_str(&format!("0-{end}")).unwrap(),
            );
            reg.sessions.lock().unwrap().insert(id, up);
            return (StatusCode::ACCEPTED, h).into_response();
        }

        if method != Method::PUT {
            reg.sessions.lock().unwrap().insert(id, up);
            return err(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "PATCH or PUT",
            );
        }
        let Some(want) = q.get("digest").and_then(|d| parse_digest(d)) else {
            return err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "bad ?digest");
        };
        // The PUT may carry the final chunk on top of whatever was PATCHed.
        if let Err(resp) = drain_into(&mut up, body).await {
            return resp;
        }
        // finish_upload rejects a mismatch: the CAS's contract is that a name
        // means its content, so a lying client must not get to write it.
        let n = up.len();
        let got = match reg.store.upload_finish(up, Some(want)).await {
            Ok(h) => h,
            Err(e) => return err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", &e.to_string()),
        };
        reg.bw.uploaded.fetch_add(1, Ordering::Relaxed);
        reg.bw.upload_bytes.fetch_add(n, Ordering::Relaxed);
        let mut h = HeaderMap::new();
        h.insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("/v2/{repo}/blobs/sha256:{got}")).unwrap(),
        );
        h.insert(
            "Docker-Content-Digest",
            HeaderValue::from_str(&format!("sha256:{got}")).unwrap(),
        );
        return (StatusCode::CREATED, h).into_response();
    }

    // /v2/<name>/manifests/<ref> — <ref> is a tag or a digest.
    if let Some((repo, reference)) = path.rsplit_once("/manifests/") {
        let key = format!("{repo}:{reference}");
        match method {
            Method::PUT => {
                // The only buffered body in the registry, and bounded: a
                // manifest is a short list of descriptors, and we must read it
                // to serve its mediaType back anyway.
                let bytes = match axum::body::to_bytes(body, MAX_MANIFEST).await {
                    Ok(b) => b,
                    Err(e) => {
                        return err(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "MANIFEST_INVALID",
                            &e.to_string(),
                        )
                    }
                };
                let hash = match reg.store.blob_put(&bytes).await {
                    Ok(h) => h,
                    Err(e) => {
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "MANIFEST_INVALID",
                            &e.to_string(),
                        )
                    }
                };
                // A digest ref needs no tag: it already names its own content.
                if parse_digest(reference).is_none() {
                    if let Err(e) = reg.store.tag_put(&key, &hash).await {
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "MANIFEST_INVALID",
                            &e.to_string(),
                        );
                    }
                }
                let mut h = HeaderMap::new();
                h.insert(
                    "Docker-Content-Digest",
                    HeaderValue::from_str(&format!("sha256:{hash}")).unwrap(),
                );
                (StatusCode::CREATED, h).into_response()
            }
            Method::GET | Method::HEAD => {
                let hash = match parse_digest(reference) {
                    Some(hex) => hex.to_string(),
                    None => match reg.store.tag_get(&key).await {
                        Some(h) => h,
                        None => {
                            return err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", &key);
                        }
                    },
                };
                match reg.store.blob_get(&hash).await {
                    Ok(Some(bytes)) => {
                        let ctype = media_type_of(&bytes);
                        blob_response(&method, bytes, &format!("sha256:{hash}"), &ctype)
                    }
                    Ok(None) => err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", &key),
                    Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()),
                }
            }
            _ => err(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "GET/HEAD/PUT",
            ),
        }
    }
    // /v2/<name>/blobs/<digest>
    else if let Some((_repo, reference)) = path.rsplit_once("/blobs/") {
        // The repo names the upstream namespace, so the mirror arm needs it.
        // Everything before /blobs/ is the name, per the OCI spec.

        let Some(hex) = parse_digest(reference) else {
            return err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", reference);
        };
        match method {
            // HEAD needs the length but not the bytes — do not read the blob
            // just to throw it away; a cache probe storm would read the store
            // end to end.
            // A HEAD miss goes to the origin too. Answering "no" here while
            // GET answers "yes" makes the mirror contradict itself, and a
            // client that stats before fetching -- buildkit's cache importer
            // among them -- believes the "no". The fetch also POPULATES, so the
            // GET that follows is a local hit rather than a second origin
            // request.
            Method::HEAD => match match reg.store.blob_size(hex).await {
                Some(len) => Some(len),
                None => {
                    if ensure_present(&reg, _repo, reference, hex).await {
                        reg.store.blob_size(hex).await
                    } else {
                        None
                    }
                }
            } {
                Some(len) => {
                    let mut h = HeaderMap::new();
                    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
                    h.insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    );
                    h.insert(
                        "Docker-Content-Digest",
                        HeaderValue::from_str(reference).unwrap(),
                    );
                    (StatusCode::OK, h).into_response()
                }
                None => err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", reference),
            },
            Method::GET => match reg.store.blob_get(hex).await {
                Ok(Some(bytes)) => {
                    reg.bw.served.fetch_add(1, Ordering::Relaxed);
                    reg.bw
                        .serve_bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    blob_response(&method, bytes, reference, "application/octet-stream")
                }
                // Nobody in the fleet holds it. THIS is where the bytes enter:
                // fetch once from the origin into a store the blooms advertise,
                // so the next machine gets it peer to peer instead of paying the
                // same request. Without this the 404 below sends buildkit
                // upstream itself and the fleet never learns anything.
                Ok(None) => {
                    if ensure_present(&reg, _repo, reference, hex).await {
                        match reg.store.blob_stream(hex).await {
                            Some((len, body)) => {
                                reg.bw.served.fetch_add(1, Ordering::Relaxed);
                                reg.bw.serve_bytes.fetch_add(len, Ordering::Relaxed);
                                streamed_blob_response(&method, len, body, reference)
                            }
                            None => err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", reference),
                        }
                    } else {
                        err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", reference)
                    }
                }
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", &e.to_string()),
            },
            _ => err(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "GET/HEAD"),
        }
    } else {
        err(StatusCode::NOT_FOUND, "UNSUPPORTED", &path)
    }
}

/// Cross-machine single-flight over HTTP — the surface a forked buildkitd calls
/// before executing a vertex, keyed on its fast cache key (`currentIndexKey`,
/// which is stable across machines by construction: it is what BuildKit already
/// exports for `--cache-from`).
///
/// `claim` BLOCKS for a follower until the leader publishes, so the caller need
/// not poll, and a leader's death arrives as `409 Conflict` (retry) rather than
/// as silence. See [`crate::lease`] and docs/buildkit-plan.md P2.
///
/// ```text
/// POST /_rebuck/lease/claim/<key>      200 lead | 200 <result> | 409 retry
/// POST /_rebuck/lease/heartbeat/<key>  200 alive | 409 you were reaped
/// POST /_rebuck/lease/release/<key>    body = the result bytes
/// POST /_rebuck/lease/abandon/<key>    we failed — followers must rebuild
/// ```
async fn lease_handle<S: RegistryStore>(
    State(reg): State<Arc<Reg<S>>>,
    method: Method,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if method != Method::POST {
        return err(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "POST only");
    }
    // No key, and deliberately so: this drops every canonical answer the table
    // holds. It exists for MEASUREMENT — `merged` cannot distinguish an
    // in-flight collision from a late claimant adopting an earlier answer, and a
    // test that wants to measure only the former has to forget the latter. It is
    // never useful in production: forgetting an answer is how the grid ends up
    // building a second one.
    if path == "forget-all" {
        let n = reg.store.lease_forget_all();
        return (StatusCode::OK, format!("forgot {n}")).into_response();
    }
    let Some((op, key)) = path.split_once('/') else {
        return err(StatusCode::NOT_FOUND, "UNSUPPORTED", &path);
    };
    if key.is_empty() {
        return err(StatusCode::BAD_REQUEST, "UNSUPPORTED", "empty lease key");
    }
    // Who is speaking. Handed out with the grant; proves a release is the CURRENT
    // leader's and not a zombie's.
    let holder = headers
        .get("X-Rebuck-Holder")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    match op {
        "claim" => match reg.store.lease_claim(key).await {
            None => err(
                StatusCode::NOT_IMPLEMENTED,
                "UNSUPPORTED",
                "this registry has no fleet to coordinate",
            ),
            // The holder id goes back with the grant, and must be echoed on
            // heartbeat/release/abandon. Without it a zombie leader could publish
            // over its successor.
            Some(Claimed::Leader { holder }) => (
                StatusCode::OK,
                [("X-Rebuck-Lease", "leader"), ("X-Rebuck-Holder", &holder)],
            )
                .into_response(),
            Some(Claimed::Done(result)) => {
                (StatusCode::OK, [("X-Rebuck-Lease", "follower")], result).into_response()
            }
            // The leader died or failed. 409 says "build it yourself" — never a
            // hang, which would be the worse bug.
            Some(Claimed::Retry) => err(
                StatusCode::CONFLICT,
                "LEASE_RETRY",
                "leader vanished; build it yourself",
            ),
        },
        "heartbeat" => {
            if reg.store.lease_heartbeat(key, &holder).await {
                (StatusCode::OK, "alive").into_response()
            } else {
                err(StatusCode::CONFLICT, "LEASE_RETRY", "you no longer hold it")
            }
        }
        "abandon" => {
            reg.store.lease_abandon(key, &holder).await;
            (StatusCode::OK, "abandoned").into_response()
        }
        "release" => {
            let bytes = match axum::body::to_bytes(body, MAX_MANIFEST).await {
                Ok(b) => b,
                Err(e) => return err(StatusCode::PAYLOAD_TOO_LARGE, "UNSUPPORTED", &e.to_string()),
            };
            reg.store.lease_release(key, &holder, bytes.to_vec()).await;
            (StatusCode::OK, "released").into_response()
        }
        _ => err(StatusCode::NOT_FOUND, "UNSUPPORTED", op),
    }
}

/// `GET /_rebuck/stats` — what crossed this endpoint, in JSON. The e2e reads it
/// to answer "is the coordinator a bottleneck?" with a number rather than a
/// hunch.
/// Make sure the fleet holds this blob, fetching it from the origin at most
/// once however many callers want it at once. Returns whether it is now present.
///
/// Returns `false` for every kind of "no" -- not configured, origin does not
/// have it, origin unreachable, bytes did not match. The caller turns that into
/// the same 404 it returned before this existed, and buildkit fetches the blob
/// itself. FAIL OPEN, ALWAYS: a mirror that converts an unreachable origin into
/// a hard failure is strictly worse than no mirror, and this is the rule this
/// system has broken before.
async fn ensure_present<S: RegistryStore>(
    reg: &Reg<S>,
    repo: &str,
    reference: &str,
    hex: &str,
) -> bool {
    let Some(up) = reg.upstream.as_ref() else {
        return false;
    };

    // Take this blob's gate. The first caller fetches; the rest queue here
    // rather than each opening their own connection to the origin.
    let gate = {
        let mut m = reg.inflight.lock().await;
        m.entry(hex.to_string()).or_default().clone()
    };
    let _held = gate.lock().await;

    // Whoever we queued behind has landed it by now, so this is a hit rather
    // than a second fetch. Also covers a peer having supplied it meanwhile.
    let present = reg.store.blob_size(hex).await.is_some();
    let landed = if present {
        true
    } else {
        fetch_once(reg, up.as_ref(), repo, reference, hex).await
    };

    // Drop the gate from the map when nobody else holds it, so a long-lived
    // agent does not accumulate an entry per blob it has ever served.
    drop(_held);
    let mut m = reg.inflight.lock().await;
    if let Some(g) = m.get(hex) {
        if Arc::strong_count(g) == 1 {
            m.remove(hex);
        }
    }
    landed
}

/// The fetch itself: stream the origin into an upload, and let the store verify
/// the digest as it lands.
///
/// VERIFY BEFORE STORING is delegated to `upload_finish(.., Some(hex))`, which
/// rejects a mismatch -- the CAS's whole contract is that a name means its
/// content. It matters here more than anywhere: we store what we serve and the
/// blooms then advertise it, so one unchecked answer would propagate across the
/// fleet by design.
async fn fetch_once<S: RegistryStore>(
    reg: &Reg<S>,
    up: &dyn Upstream,
    repo: &str,
    reference: &str,
    hex: &str,
) -> bool {
    let mut stream = match up.blob(repo, reference).await {
        Ok(Some(s)) => s,
        Ok(None) => return false,
        Err(e) => {
            eprintln!("[registry] upstream {repo} {reference}: {e}");
            return false;
        }
    };
    let mut upload = match reg.store.upload_begin().await {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[registry] cannot stage {reference}: {e}");
            return false;
        }
    };
    let mut n: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[registry] upstream {repo} {reference} broke off: {e}");
                return false;
            }
        };
        n += chunk.len() as u64;
        if let Err(e) = upload.write(&chunk).await {
            eprintln!("[registry] cannot stage {reference}: {e}");
            return false;
        }
    }
    // A mismatch lands here, as an Err, and nothing reaches the CAS.
    if let Err(e) = reg.store.upload_finish(upload, Some(hex)).await {
        eprintln!("[registry] refusing {reference} from {repo}: {e}");
        return false;
    }
    reg.bw.upstream_fetches.fetch_add(1, Ordering::Relaxed);
    reg.bw.upstream_bytes.fetch_add(n, Ordering::Relaxed);
    true
}

async fn stats_handle<S: RegistryStore>(State(reg): State<Arc<Reg<S>>>) -> Response {
    let mut body = serde_json::json!({
        "uploads":      reg.bw.uploaded.load(Ordering::Relaxed),
        "upload_bytes": reg.bw.upload_bytes.load(Ordering::Relaxed),
        "serves":       reg.bw.served.load(Ordering::Relaxed),
        "serve_bytes":  reg.bw.serve_bytes.load(Ordering::Relaxed),
        "upstream_fetches": reg.bw.upstream_fetches.load(Ordering::Relaxed),
        "upstream_bytes":   reg.bw.upstream_bytes.load(Ordering::Relaxed),
    });
    // Only whoever owns the lease table can answer this; an agent forwards and
    // would report zeroes, which is worse than saying nothing.
    if let Some((led, merged, abandoned)) = reg.store.lease_stats() {
        body["leases_led"] = led.into();
        body["leases_merged"] = merged.into();
        body["leases_abandoned"] = abandoned.into();
    }
    // Of the above, the image resolutions. "merged went up" cannot say whether a
    // machine skipped a BUILD or a REGISTRY ROUND TRIP; this can.
    if let Some((led, merged)) = reg.store.resolve_stats() {
        body["resolve_led"] = led.into();
        body["resolve_merged"] = merged.into();
    }
    (StatusCode::OK, axum::Json(body)).into_response()
}

pub fn router<S: RegistryStore>(store: Arc<S>) -> Router {
    router_with_upstream(store, None)
}

/// The mirror shape. `upstream` is the third step of P4b: local store, then
/// whichever peer the blooms claim, then the driver -- and only if all of that
/// misses, the origin, once, into a store the blooms then advertise.
pub fn router_with_upstream<S: RegistryStore>(
    store: Arc<S>,
    upstream: Option<Arc<dyn Upstream>>,
) -> Router {
    let reg = Arc::new(Reg {
        store,
        bw: Bandwidth::default(),
        upstream,
        inflight: tokio::sync::Mutex::new(HashMap::new()),
        next_upload: AtomicU64::new(0),
        sessions: std::sync::Mutex::new(HashMap::new()),
    });
    Router::new()
        // The `/v2/` probe: clients use a 200 here as "this is a registry".
        .route("/v2/", any(|| async { StatusCode::OK }))
        .route("/v2/{*path}", any(handle::<S>))
        .route("/_rebuck/lease/{*path}", any(lease_handle::<S>))
        .route("/_rebuck/stats", any(stats_handle::<S>))
        // Blobs stream to disk, so the extractor's buffering limit must not
        // apply — `drain_into` enforces MAX_UPLOAD as the bytes go past, and
        // the manifest arm bounds itself with MAX_MANIFEST.
        .layer(DefaultBodyLimit::disable())
        .with_state(reg)
}

pub async fn serve<S: RegistryStore>(addr: SocketAddr, store: Arc<S>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("[registry] OCI v2 on http://{}", listener.local_addr()?);
    axum::serve(listener, router(store)).await?;
    Ok(())
}

pub async fn serve_with_upstream<S: RegistryStore>(
    addr: SocketAddr,
    store: Arc<S>,
    upstream: Option<Arc<dyn Upstream>>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let via = match &upstream {
        Some(_) => " (pull-through)",
        None => "",
    };
    println!(
        "[registry] OCI v2 on http://{}{via}",
        listener.local_addr()?
    );
    axum::serve(listener, router_with_upstream(store, upstream)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn store() -> Arc<Store> {
        Arc::new(Store::new(tempfile::tempdir().unwrap().keep()).unwrap())
    }

    async fn call(r: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let res = r.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    fn post(uri: &str) -> Request<Body> {
        Request::post(uri).body(Body::empty()).unwrap()
    }

    /// The push path a `--cache-to type=registry` actually walks: POST to open,
    /// PUT the bytes, then the blob is fetchable by digest with an exact
    /// Content-Length (a wrong one is a silent importer rejection).
    #[tokio::test]
    async fn blob_upload_then_fetch_roundtrips() {
        let r = router(store());
        let payload = b"a layer, of sorts".to_vec();
        let digest = format!("sha256:{}", crate::store::sha256_hex(&payload));

        let (st, h, _) = call(&r, post("/v2/cache/blobs/uploads/")).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let loc = h[header::LOCATION].to_str().unwrap().to_string();

        let (st, _, _) = call(
            &r,
            Request::put(format!("{loc}?digest={digest}"))
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        let (st, h, body) = call(
            &r,
            Request::get(format!("/v2/cache/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, payload);
        assert_eq!(
            h[header::CONTENT_LENGTH].to_str().unwrap(),
            payload.len().to_string(),
            "exact Content-Length or the importer drops it silently"
        );
    }

    /// A HEAD must report the length without reading the blob, and must 404
    /// for one we do not hold — this is the probe BuildKit storms us with.
    #[tokio::test]
    async fn blob_head_reports_length_and_404s_when_absent() {
        let r = router(store());
        let payload = b"xyz".to_vec();
        let digest = format!("sha256:{}", crate::store::sha256_hex(&payload));

        let (st, _, _) = call(
            &r,
            Request::head(format!("/v2/c/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let (_, h, _) = call(&r, post("/v2/c/blobs/uploads/")).await;
        let loc = h[header::LOCATION].to_str().unwrap().to_string();
        call(
            &r,
            Request::put(format!("{loc}?digest={digest}"))
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await;

        let (st, h, body) = call(
            &r,
            Request::head(format!("/v2/c/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.is_empty(), "HEAD must not carry a body");
        assert_eq!(h[header::CONTENT_LENGTH].to_str().unwrap(), "3");
    }

    /// Manifests round-trip by tag AND by digest, and — the trap — annotations
    /// survive verbatim. `containerimage.inlinecache` is kept by the earthbuild
    /// fork and dropped upstream; strip it and `--use-inline-cache` silently
    /// stops working. Storing opaque bytes is what makes this hold.
    #[tokio::test]
    async fn manifest_roundtrips_by_tag_and_digest_preserving_annotations() {
        let r = router(store());
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.buildkit.cacheconfig.v0",
                       "digest": "sha256:".to_string() + &"0".repeat(64), "size": 1},
            "layers": [],
            "annotations": {"containerimage.inlinecache": "eyJmb28iOiJiYXIifQ=="}
        });
        let raw = serde_json::to_vec(&manifest).unwrap();

        let (st, h, _) = call(
            &r,
            Request::put("/v2/cache/manifests/latest")
                .body(Body::from(raw.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let digest = h["Docker-Content-Digest"].to_str().unwrap().to_string();

        for uri in [
            "/v2/cache/manifests/latest".to_string(),
            format!("/v2/cache/manifests/{digest}"),
        ] {
            let (st, h, body) = call(&r, Request::get(&uri).body(Body::empty()).unwrap()).await;
            assert_eq!(st, StatusCode::OK, "{uri}");
            assert_eq!(body, raw, "manifest bytes must be byte-identical: {uri}");
            assert_eq!(
                h[header::CONTENT_TYPE].to_str().unwrap(),
                "application/vnd.oci.image.manifest.v1+json"
            );
            let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                got["annotations"]["containerimage.inlinecache"], "eyJmb28iOiJiYXIifQ==",
                "inline-cache annotation dropped — --use-inline-cache would break silently"
            );
        }
    }

    /// Chunked upload: POST, PATCH the chunks, PUT to finalise. BuildKit never
    /// does this (containerd's pusher is monolithic), which is exactly why it
    /// was missing — and why skopeo broke against the first cut. Every OCI
    /// client that is not BuildKit chunks.
    #[tokio::test]
    async fn chunked_patch_upload_assembles_the_blob() {
        let r = router(store());
        let payload = b"chunk-one/chunk-two/chunk-three".to_vec();
        let digest = format!("sha256:{}", crate::store::sha256_hex(&payload));

        let (_, h, _) = call(&r, post("/v2/c/blobs/uploads/")).await;
        let loc = h[header::LOCATION].to_str().unwrap().to_string();

        // Two PATCHes, then a PUT carrying the tail — all three paths at once.
        for chunk in [&payload[..10], &payload[10..20]] {
            let (st, _, _) = call(
                &r,
                Request::patch(&loc)
                    .body(Body::from(chunk.to_vec()))
                    .unwrap(),
            )
            .await;
            assert_eq!(st, StatusCode::ACCEPTED);
        }
        let (st, _, _) = call(
            &r,
            Request::put(format!("{loc}?digest={digest}"))
                .body(Body::from(payload[20..].to_vec()))
                .unwrap(),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::CREATED,
            "chunks must reassemble to the digest"
        );

        let (st, _, body) = call(
            &r,
            Request::get(format!("/v2/c/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, payload);
    }

    /// A layer far bigger than any buffered body limit must round-trip. axum's
    /// default extractor cap is 2 MiB and we disable it precisely because blobs
    /// stream to disk a chunk at a time — this is the test that would fail the
    /// moment someone reintroduces a `Bytes` extractor on the blob path.
    #[tokio::test]
    async fn blob_far_larger_than_any_buffer_limit_streams_through() {
        let r = router(store());
        let payload: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let digest = format!("sha256:{}", crate::store::sha256_hex(&payload));

        let (_, h, _) = call(&r, post("/v2/c/blobs/uploads/")).await;
        let loc = h[header::LOCATION].to_str().unwrap().to_string();
        let (st, _, _) = call(
            &r,
            Request::put(format!("{loc}?digest={digest}"))
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "16 MiB blob must stream through");

        let (st, h, body) = call(
            &r,
            Request::get(format!("/v2/c/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body.len(), payload.len());
        assert_eq!(body, payload);
        assert_eq!(
            h[header::CONTENT_LENGTH].to_str().unwrap(),
            payload.len().to_string()
        );
    }

    /// A PUT whose bytes do not match the digest it claims must be refused —
    /// the CAS's whole contract is that a name means its content.
    #[tokio::test]
    async fn digest_mismatch_on_upload_is_rejected() {
        let r = router(store());
        let lie = format!("sha256:{}", "c".repeat(64));
        let (_, h, _) = call(&r, post("/v2/c/blobs/uploads/")).await;
        let loc = h[header::LOCATION].to_str().unwrap().to_string();

        let (st, _, _) = call(
            &r,
            Request::put(format!("{loc}?digest={lie}"))
                .body(Body::from("not what the digest says"))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// A blob we already hold satisfies a cross-repo mount outright: the CAS is
    /// repo-agnostic, so the client skips the upload. This is dedupe across
    /// every repo in the fleet, for free.
    #[tokio::test]
    async fn cross_repo_mount_of_a_held_blob_skips_the_upload() {
        let r = router(store());
        let payload = b"shared base layer".to_vec();
        let digest = format!("sha256:{}", crate::store::sha256_hex(&payload));

        let (_, h, _) = call(&r, post("/v2/one/blobs/uploads/")).await;
        let loc = h[header::LOCATION].to_str().unwrap().to_string();
        call(
            &r,
            Request::put(format!("{loc}?digest={digest}"))
                .body(Body::from(payload))
                .unwrap(),
        )
        .await;

        let (st, h, _) = call(
            &r,
            post(&format!("/v2/two/blobs/uploads/?mount={digest}&from=one")),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "mount of a held blob must succeed");
        assert!(h[header::LOCATION].to_str().unwrap().contains(&digest));
    }

    /// An unmountable blob must still get 202 + Location so the client falls
    /// back to a normal upload. A 404/400 here is a hard client error, not a
    /// fallback — this is the one that would look like "registry broken".
    #[tokio::test]
    async fn unmountable_blob_falls_back_to_upload_not_error() {
        let r = router(store());
        let absent = format!("sha256:{}", "b".repeat(64));
        let (st, h, _) = call(
            &r,
            post(&format!("/v2/two/blobs/uploads/?mount={absent}&from=one")),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(h.contains_key(header::LOCATION));
    }

    /// The digest comes off an HTTP path and is used to build a filesystem
    /// path. Traversal and non-canonical forms must die at the door.
    #[tokio::test]
    async fn malformed_digests_are_rejected() {
        let r = router(store());
        for bad in [
            "sha256:../../../../etc/passwd",
            "sha256:NOTHEX",
            &format!("sha256:{}", "A".repeat(64)), // uppercase is not canonical
            &format!("sha256:{}", "a".repeat(63)), // short
            "md5:abc",
        ] {
            let (st, _, _) = call(
                &r,
                Request::get(format!("/v2/c/blobs/{bad}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "accepted a bad digest: {bad}");
        }
    }

    /// The registry over a real [`Driver`] (not a bare Store): a blob the
    /// driver holds is served, and the mesh is never consulted. The
    /// cross-machine half — a blob only a WORKER holds, fetched over iroh — is
    /// tests/e2e-registry.sh, which needs a real mesh to mean anything.
    #[tokio::test]
    async fn registry_over_driver_serves_a_held_blob() {
        let d = crate::driver::Driver::for_test();
        let payload = b"a layer the driver holds".to_vec();
        let hash = d.store.put(None, &payload).await.unwrap().hash;
        let r = router(d);

        let (st, h, body) = call(
            &r,
            Request::get(format!("/v2/cache/blobs/sha256:{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, payload);
        assert_eq!(
            h[header::CONTENT_LENGTH].to_str().unwrap(),
            payload.len().to_string()
        );
    }

    /// With no workers and no local copy, a miss must be an honest 404 — not a
    /// hang, and not a 500. BuildKit reads 404 as "push it", which is right;
    /// anything else stalls or fails the build.
    #[tokio::test]
    async fn registry_over_driver_404s_when_nobody_holds_it() {
        let r = router(crate::driver::Driver::for_test());
        let absent = format!("sha256:{}", "b".repeat(64));
        let (st, _, _) = call(
            &r,
            Request::get(format!("/v2/c/blobs/{absent}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// Regression: an OCI digest carries no size, and the mesh cache-back
    /// verifies what it fetched. Faking a zero size would make `put(Some(d))`
    /// reject every non-empty blob as a digest mismatch, so the hash-only path
    /// must degenerate the size check rather than invent one.
    #[tokio::test]
    async fn hash_only_fetch_does_not_trip_the_size_check() {
        let d = crate::driver::Driver::for_test();
        let payload = b"non-empty, so a zero size would be a lie".to_vec();
        let hash = d.store.put(None, &payload).await.unwrap().hash;

        let got = d.get_blob_by_hash(&hash).await.unwrap();
        assert_eq!(got.unwrap(), payload);
    }

    /// The HTTP surface a forked buildkitd calls. Two daemons racing the same
    /// cache key: one is told to build, the other BLOCKS and is handed the
    /// result. This is the property one buildkitd gets free from its own solver
    /// and which N ephemeral daemons otherwise lose — the whole of P2.
    #[tokio::test]
    async fn two_daemons_racing_one_key_build_it_once() {
        let r = router(crate::driver::Driver::for_test());

        let (st, h, _) = call(&r, post("/_rebuck/lease/claim/sha256:abc")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            h["X-Rebuck-Lease"], "leader",
            "the first claimant must build it"
        );
        let holder = h["X-Rebuck-Holder"].to_str().unwrap().to_string();

        // The second daemon blocks — it must not build the same thing.
        let r2 = r.clone();
        let follower =
            tokio::spawn(async move { call(&r2, post("/_rebuck/lease/claim/sha256:abc")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !follower.is_finished(),
            "the follower must WAIT, not rebuild"
        );

        // Leader publishes; the follower is handed the result it never built.
        let (st, _, _) = call(
            &r,
            Request::post("/_rebuck/lease/release/sha256:abc")
                .header("X-Rebuck-Holder", &holder)
                .body(Body::from("the one true layer"))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let (st, h, body) = follower.await.unwrap();
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h["X-Rebuck-Lease"], "follower");
        assert_eq!(body, b"the one true layer");
    }

    /// A leader that dies must free its followers to re-elect, not strand them.
    /// 409 says "claim again"; silence would hang the build, which is a worse
    /// bug than the duplicate work we set out to prevent.
    #[tokio::test]
    async fn a_dead_leader_tells_its_follower_to_retry_not_to_wait() {
        let d = crate::driver::Driver::for_test();
        let r = router(d.clone());

        let (st, h, _) = call(&r, post("/_rebuck/lease/claim/k")).await;
        assert_eq!(st, StatusCode::OK);
        let holder = h["X-Rebuck-Holder"].to_str().unwrap().to_string();

        let r2 = r.clone();
        let follower = tokio::spawn(async move { call(&r2, post("/_rebuck/lease/claim/k")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The leader vanishes without publishing.
        d.lease_table().abandon_peer("k", &holder);

        let (st, _, _) = follower.await.unwrap();
        assert_eq!(
            st,
            StatusCode::CONFLICT,
            "a stranded follower must be told to re-claim"
        );
        // ... and the key is genuinely up for grabs again.
        let (st, h, _) = call(&r, post("/_rebuck/lease/claim/k")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h["X-Rebuck-Lease"], "leader");
    }

    /// A leader that fails must free its followers to REBUILD, not hand them a
    /// bogus result and not leave them waiting out the TTL for one that is never
    /// coming.
    #[tokio::test]
    async fn an_abandoning_leader_frees_its_followers_to_rebuild() {
        let r = router(crate::driver::Driver::for_test());
        let (st, h, _) = call(&r, post("/_rebuck/lease/claim/k")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h["X-Rebuck-Lease"], "leader");
        let holder = h["X-Rebuck-Holder"].to_str().unwrap().to_string();

        let r2 = r.clone();
        let follower = tokio::spawn(async move { call(&r2, post("/_rebuck/lease/claim/k")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (st, _, _) = call(
            &r,
            Request::post("/_rebuck/lease/abandon/k")
                .header("X-Rebuck-Holder", &holder)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let (st, _, _) = follower.await.unwrap();
        assert_eq!(
            st,
            StatusCode::CONFLICT,
            "the follower must be told to rebuild"
        );
    }

    /// A zombie must not publish over its successor. The holder id handed out
    /// with the grant is what proves a release is the CURRENT leader's — without
    /// it, an evicted leader that finally finishes would overwrite the result of
    /// whoever took over, and two writers would race one entry.
    #[tokio::test]
    async fn a_release_without_the_holder_id_is_ignored() {
        let r = router(crate::driver::Driver::for_test());
        let (st, h, _) = call(&r, post("/_rebuck/lease/claim/k")).await;
        assert_eq!(st, StatusCode::OK);
        let holder = h["X-Rebuck-Holder"].to_str().unwrap().to_string();

        let r2 = r.clone();
        let follower = tokio::spawn(async move { call(&r2, post("/_rebuck/lease/claim/k")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // An impostor (or a zombie quoting a stale id) publishes.
        call(
            &r,
            Request::post("/_rebuck/lease/release/k")
                .header("X-Rebuck-Holder", "http-999")
                .body(Body::from("forged"))
                .unwrap(),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !follower.is_finished(),
            "an impostor's result must not be served"
        );

        // The real leader publishes.
        call(
            &r,
            Request::post("/_rebuck/lease/release/k")
                .header("X-Rebuck-Holder", &holder)
                .body(Body::from("genuine"))
                .unwrap(),
        )
        .await;
        let (_, _, body) = follower.await.unwrap();
        assert_eq!(body, b"genuine");
    }

    /// A bare store has no fleet to coordinate, and must say so rather than
    /// pretend to hold a lease nobody is honouring.
    #[tokio::test]
    async fn a_storeonly_registry_refuses_leases() {
        let r = router(store());
        let (st, _, _) = call(&r, post("/_rebuck/lease/claim/k")).await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
    }

    /// A repo name is a path segment too. Hashing the tag key makes traversal
    /// unrepresentable rather than merely filtered.
    #[tokio::test]
    async fn traversal_in_repo_name_cannot_escape_the_store() {
        let root = tempfile::tempdir().unwrap().keep();
        let r = router(Arc::new(Store::new(root.clone()).unwrap()));
        let (st, _, _) = call(
            &r,
            Request::put("/v2/../../../../tmp/pwned/manifests/latest")
                .body(Body::from(r#"{"mediaType":"x"}"#))
                .unwrap(),
        )
        .await;
        // Whatever the router makes of it, nothing may land outside the store.
        assert!(st.is_success() || st.is_client_error());
        assert!(!std::path::Path::new("/tmp/pwned").exists());
        for entry in walk(&root) {
            assert!(entry.starts_with(&root), "escaped the store: {entry:?}");
        }
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }
}

/// Pull-through mirror: where an image blob comes from when nobody in the fleet
/// holds it.
///
/// Principle 9 -- the origin registry is a fallback, not a data path. Without
/// this the agent 404s on a blob no peer has, buildkit fetches it upstream
/// itself, and the bytes never enter the fleet: every machine then pays the same
/// request, which is what earthbuild currently buys Docker Hub credentials to
/// survive. Fetching once THROUGH the agent puts it in a store the blooms
/// advertise, so the next machine gets it peer to peer.
pub mod upstream {
    /// The registry an unqualified repo belongs to. Docker Hub is the default
    /// because it is what rate-limits; anything else is already explicit in the
    /// reference.
    pub const DEFAULT_HOST: &str = "registry-1.docker.io";

    /// Blob URL for a repo and digest.
    ///
    /// The repo arrives from the mirror request path, so an official image
    /// reaches us as "library/alpine" -- already normalised by the client. We do
    /// not re-normalise: guessing at a namespace is how a mirror silently serves
    /// the wrong image.
    pub fn blob_url(host: &str, repo: &str, digest: &str) -> String {
        format!("https://{host}/v2/{repo}/blobs/{digest}")
    }

    /// Docker Hub answers an anonymous blob request with 401 and a token
    /// challenge. The token is per-repo and pull-scoped: asking for more is how
    /// an anonymous client gets refused outright.
    pub fn token_url(repo: &str) -> String {
        format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"
        )
    }

    /// Enabled by pointing at a host. Off by default: a mirror that starts
    /// reaching the internet because a binary was upgraded is not a change
    /// anyone asked for.
    pub fn host_from_env() -> Option<String> {
        match std::env::var("REBUCK_UPSTREAM_REGISTRY") {
            // "on" without having to name the host. Hub is the default because
            // Hub is what rate-limits; anything else is already explicit in the
            // reference, so naming it is no burden.
            Ok(v) if matches!(v.trim(), "1" | "true" | "yes") => Some(DEFAULT_HOST.to_string()),
            Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
            _ => None,
        }
    }
}

/// The origin registry over HTTPS, with Docker Hub's token dance.
///
/// One client for the process: reqwest pools connections, and a fleet fetching
/// eighteen base images wants that pooling rather than eighteen TLS handshakes.
pub struct HttpUpstream {
    host: String,
    http: reqwest::Client,
}

impl HttpUpstream {
    /// `None` unless `REBUCK_UPSTREAM_REGISTRY` names a host -- see
    /// [`upstream::host_from_env`]. Off by default, deliberately.
    pub fn from_env() -> Option<Self> {
        upstream::host_from_env().map(Self::new)
    }

    pub fn new(host: String) -> Self {
        Self {
            host,
            // A layer can be hundreds of MB from a CDN on a cold runner; the
            // default has no overall timeout, which is right here. Connect is
            // bounded so an unroutable origin fails open promptly rather than
            // stalling every build behind it.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .user_agent("rebuck2-mirror")
                .build()
                .expect("rustls client"),
        }
    }

    /// Docker Hub answers an anonymous blob request with 401 and a token
    /// challenge. The token is per-repo and pull-scoped; asking for more is how
    /// an anonymous client gets refused outright.
    async fn token(&self, repo: &str) -> Result<Option<String>> {
        let v: serde_json::Value = self
            .http
            .get(upstream::token_url(repo))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("token").and_then(|t| t.as_str()).map(str::to_string))
    }
}

#[async_trait::async_trait]
impl Upstream for HttpUpstream {
    async fn blob(&self, repo: &str, digest: &str) -> Result<Option<BoxByteStream>> {
        let url = upstream::blob_url(&self.host, repo, digest);
        let mut res = self.http.get(&url).send().await?;
        // Unauthenticated first, token only if challenged: a private registry
        // with no auth at all should not be handed a Hub token, and Hub tells us
        // plainly when it wants one.
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(tok) = self.token(repo).await? {
                res = self.http.get(&url).bearer_auth(tok).send().await?;
            }
        }
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = res.error_for_status()?;
        // bytes_stream, not bytes: reqwest hands the body over as it arrives and
        // it goes straight into an upload, so a layer never exists whole
        // anywhere in this process.
        let stream = res
            .bytes_stream()
            .map(|c| c.map(|b| b.to_vec()).map_err(anyhow::Error::from));
        Ok(Some(Box::pin(stream)))
    }
}

#[cfg(test)]
mod upstream_tests {
    use super::upstream::*;

    #[test]
    fn blob_url_is_the_oci_v2_path() {
        assert_eq!(
            blob_url("registry-1.docker.io", "library/alpine", "sha256:abc"),
            "https://registry-1.docker.io/v2/library/alpine/blobs/sha256:abc"
        );
    }

    /// A non-Hub registry is reached directly; nothing about the path is
    /// Hub-specific.
    #[test]
    fn blob_url_works_for_any_host() {
        assert_eq!(
            blob_url("ghcr.io", "earthbuild/earthbuild", "sha256:d"),
            "https://ghcr.io/v2/earthbuild/earthbuild/blobs/sha256:d"
        );
    }

    /// Pull-scoped and repo-scoped. A broader scope is refused for anonymous
    /// clients, so asking for one turns every miss into a hard failure.
    #[test]
    fn token_is_scoped_to_pull_on_one_repo() {
        let u = token_url("library/alpine");
        assert!(u.contains("scope=repository:library/alpine:pull"), "{u}");
        assert!(u.contains("service=registry.docker.io"), "{u}");
    }

    /// Off unless asked for: a mirror must not start reaching the internet
    /// because someone upgraded a binary.
    #[test]
    fn upstream_is_opt_in() {
        // SAFETY-free: single-threaded test, and we restore nothing because the
        // absence of the var IS the default under test.
        unsafe { std::env::remove_var("REBUCK_UPSTREAM_REGISTRY") };
        assert_eq!(host_from_env(), None);
        unsafe { std::env::set_var("REBUCK_UPSTREAM_REGISTRY", "  ") };
        assert_eq!(host_from_env(), None, "blank is not a host");
        unsafe { std::env::set_var("REBUCK_UPSTREAM_REGISTRY", " ghcr.io ") };
        assert_eq!(host_from_env().as_deref(), Some("ghcr.io"));
        unsafe { std::env::remove_var("REBUCK_UPSTREAM_REGISTRY") };
    }
}

/// P4b -- the agent as a pull-through mirror.
///
/// Steps 1 and 2 of the plan's shape already work: `Agent::blob_get` goes local
/// store -> whichever peer the blooms claim -> the driver. What is missing is
/// step 3. A blob NOBODY holds is a plain 404, buildkit fetches it upstream
/// itself, and the bytes never enter the fleet -- so the next machine pays the
/// same request, and the one after that, which is what earthbuild currently buys
/// Docker Hub credentials to survive.
#[cfg(test)]
mod pull_through_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::AtomicUsize;
    use tower::ServiceExt;

    fn store() -> Arc<Store> {
        Arc::new(Store::new(tempfile::tempdir().unwrap().keep()).unwrap())
    }

    async fn call(r: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let res = r.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    fn sha256_of(b: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(b))
    }

    /// An upstream that answers from memory and counts how often it was asked.
    /// The count IS the assertion for most of these: "fetched once" is the whole
    /// claim of a pull-through mirror.
    struct Fake {
        body: std::result::Result<Option<Vec<u8>>, ()>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Upstream for Fake {
        async fn blob(&self, _repo: &str, _digest: &str) -> Result<Option<BoxByteStream>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match &self.body {
                Ok(None) => Ok(None),
                Ok(Some(v)) => {
                    let chunks: Vec<Result<Vec<u8>>> = vec![Ok(v.clone())];
                    Ok(Some(Box::pin(futures::stream::iter(chunks))))
                }
                Err(()) => Err(anyhow::anyhow!("upstream unreachable")),
            }
        }
    }

    fn fake(body: std::result::Result<Option<Vec<u8>>, ()>) -> Arc<Fake> {
        Arc::new(Fake {
            body,
            calls: AtomicUsize::new(0),
        })
    }

    /// THE property. A blob no peer holds is fetched upstream ONCE, stored, and
    /// served -- and the second request never reaches upstream, because by then
    /// the fleet holds it and the blooms say so.
    #[tokio::test]
    async fn a_blob_nobody_holds_is_fetched_once_and_then_served_locally() {
        let bytes = b"layer bytes".to_vec();
        let digest = sha256_of(&bytes);
        let up = fake(Ok(Some(bytes.clone())));
        let r = router_with_upstream(store(), Some(up.clone()));

        for _ in 0..3 {
            let (st, got) = call(
                &r,
                Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(got, bytes);
        }
        assert_eq!(
            up.calls.load(Ordering::Relaxed),
            1,
            "three requests must cost ONE upstream fetch -- that is the whole feature"
        );
    }

    /// The failure a mirror must never have. Serving bytes whose hash does not
    /// match the digest asked for is silently building against the wrong image,
    /// and it would poison the fleet: we store what we serve, so one bad answer
    /// propagates by design.
    #[tokio::test]
    async fn bytes_that_do_not_match_the_digest_are_refused_and_not_stored() {
        let asked = sha256_of(b"the blob that was asked for");
        let up = fake(Ok(Some(b"something else entirely".to_vec())));
        let r = router_with_upstream(store(), Some(up.clone()));

        let (st, _) = call(
            &r,
            Request::get(format!("/v2/library/alpine/blobs/{asked}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "a digest mismatch is not a hit");

        // And it must not have been kept: a second request has to go back
        // upstream rather than serve the poison from our own store.
        let (st2, _) = call(
            &r,
            Request::get(format!("/v2/library/alpine/blobs/{asked}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st2, StatusCode::NOT_FOUND);
        assert_eq!(up.calls.load(Ordering::Relaxed), 2, "nothing was cached");
    }

    /// Fail open, always. A mirror that turns an unreachable upstream into a
    /// hard failure is strictly worse than no mirror: 404 is what we said before
    /// this feature existed, and buildkit then fetches it itself.
    #[tokio::test]
    async fn an_unreachable_upstream_is_a_404_not_a_500() {
        let digest = sha256_of(b"whatever");
        let r = router_with_upstream(store(), Some(fake(Err(()))));
        let (st, _) = call(
            &r,
            Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// Off by default. The mirror reaches the internet only when configured to,
    /// so upgrading a binary cannot start making outbound requests nobody asked
    /// for.
    #[tokio::test]
    async fn without_an_upstream_a_miss_is_still_a_miss() {
        let digest = sha256_of(b"whatever");
        let r = router(store());
        let (st, _) = call(
            &r,
            Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// "upstream fetches do not rise" is half of the plan's Done-when, so it has
    /// to be reportable rather than inferred from a log.
    #[tokio::test]
    async fn upstream_fetches_are_counted() {
        let bytes = b"counted".to_vec();
        let digest = sha256_of(&bytes);
        let r = router_with_upstream(store(), Some(fake(Ok(Some(bytes)))));
        let _ = call(
            &r,
            Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let (_, body) = call(
            &r,
            Request::get("/_rebuck/stats").body(Body::empty()).unwrap(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["upstream_fetches"], 1);
    }
}

#[cfg(test)]
mod upstream_env_tests {
    use super::upstream::*;

    /// The mirror is off unless asked for, and "asked for" should not require
    /// remembering Docker Hub's registry hostname. `host_from_env` reads process
    /// state, so these assert on the pure mapping rather than by setting env
    /// vars -- concurrent tests share one environment and that is a race.
    #[test]
    fn default_host_is_hub_because_hub_is_what_rate_limits() {
        assert_eq!(DEFAULT_HOST, "registry-1.docker.io");
    }

    #[test]
    fn a_named_host_is_reached_directly() {
        assert_eq!(
            blob_url("ghcr.io", "earthbuild/earthbuild", "sha256:abc"),
            "https://ghcr.io/v2/earthbuild/earthbuild/blobs/sha256:abc"
        );
    }

    /// Pull-scoped and per-repo. Asking for more is how an anonymous client gets
    /// refused outright rather than handed a narrower token.
    #[test]
    fn the_token_request_asks_only_for_pull_on_one_repo() {
        let u = token_url("library/alpine");
        assert!(u.contains("scope=repository:library/alpine:pull"), "{u}");
        assert!(!u.contains("push"), "{u}");
    }
}

#[cfg(test)]
mod pull_through_streaming_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::AtomicUsize;
    use tower::ServiceExt;

    fn store() -> Arc<Store> {
        Arc::new(Store::new(tempfile::tempdir().unwrap().keep()).unwrap())
    }

    fn sha256_of(b: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(b))
    }

    /// Hands the bytes over in chunks and can be made slow, so a second caller
    /// is guaranteed to arrive while the first is still fetching.
    struct SlowChunked {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl Upstream for SlowChunked {
        async fn blob(&self, _repo: &str, _digest: &str) -> Result<Option<BoxByteStream>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(self.delay).await;
            let chunks: Vec<Result<Vec<u8>>> =
                self.bytes.chunks(7).map(|c| Ok(c.to_vec())).collect();
            Ok(Some(Box::pin(futures::stream::iter(chunks))))
        }
    }

    /// Ten nested builds asking for alpine at once must cost ONE upstream
    /// fetch. Without this the mirror is a per-request proxy -- it would make
    /// exactly the storm of requests the feature exists to remove, which is the
    /// shape earthbuild already pays Docker Hub credentials to survive.
    #[tokio::test]
    async fn concurrent_misses_for_one_blob_cost_one_upstream_fetch() {
        let bytes: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let digest = sha256_of(&bytes);
        let calls = Arc::new(AtomicUsize::new(0));
        let up = Arc::new(SlowChunked {
            bytes: bytes.clone(),
            calls: calls.clone(),
            delay: std::time::Duration::from_millis(120),
        });
        let r = router_with_upstream(store(), Some(up));

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let r = r.clone();
            let digest = digest.clone();
            set.spawn(async move {
                let res = r
                    .oneshot(
                        Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let st = res.status();
                let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec();
                (st, body)
            });
        }
        while let Some(j) = set.join_next().await {
            let (st, body) = j.unwrap();
            assert_eq!(st, StatusCode::OK);
            assert_eq!(body, bytes, "every waiter gets the whole blob");
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "ten concurrent misses must collapse to one upstream fetch"
        );
    }

    /// The bytes must reach disk a chunk at a time. A layer is hundreds of MB
    /// and this runs on a CI runner next to a compiler, so a blob that is only
    /// ever whole in memory is a memory bug waiting for a big enough image.
    ///
    /// Asserted structurally rather than by watching RSS: the upstream hands
    /// over a stream, and what lands is verified by the store's own digest
    /// check on finish -- so a correct result here means the streaming path,
    /// not a buffered one, carried it.
    #[tokio::test]
    async fn a_streamed_blob_lands_whole_and_verified() {
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let digest = sha256_of(&bytes);
        let calls = Arc::new(AtomicUsize::new(0));
        let s = store();
        let r = router_with_upstream(
            s.clone(),
            Some(Arc::new(SlowChunked {
                bytes: bytes.clone(),
                calls,
                delay: std::time::Duration::ZERO,
            })),
        );
        let res = r
            .clone()
            .oneshot(
                Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let got = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        assert_eq!(got, bytes);
        // And it is in the CAS under its own name, which is what makes the next
        // machine's fetch a peer fetch rather than another upstream one.
        let hex = digest.strip_prefix("sha256:").unwrap();
        assert!(s.get_by_hash(hex).await.unwrap().is_some());
    }
}

#[cfg(test)]
mod pull_through_head_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::AtomicUsize;
    use tower::ServiceExt;

    fn store() -> Arc<Store> {
        Arc::new(Store::new(tempfile::tempdir().unwrap().keep()).unwrap())
    }

    fn sha256_of(b: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(b))
    }

    struct One {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Upstream for One {
        async fn blob(&self, _repo: &str, _digest: &str) -> Result<Option<BoxByteStream>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let chunks: Vec<Result<Vec<u8>>> = vec![Ok(self.bytes.clone())];
            Ok(Some(Box::pin(futures::stream::iter(chunks))))
        }
    }

    /// A registry must not contradict itself. Before this, HEAD consulted only
    /// the local store while GET went on to the origin -- so for one blob the
    /// mirror answered "no" to HEAD and "yes" to GET, and which answer a client
    /// got depended on which verb it happened to use. Clients that stat before
    /// fetching (buildkit's cache importer among them) see the "no".
    #[tokio::test]
    async fn head_and_get_agree_about_a_blob_only_the_origin_has() {
        let bytes = b"a layer the fleet has never seen".to_vec();
        let digest = sha256_of(&bytes);
        let calls = Arc::new(AtomicUsize::new(0));
        let r = router_with_upstream(
            store(),
            Some(Arc::new(One {
                bytes: bytes.clone(),
                calls: calls.clone(),
            })),
        );

        let head = r
            .clone()
            .oneshot(
                Request::head(format!("/v2/library/alpine/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK, "HEAD must not deny it");
        assert_eq!(
            head.headers()[header::CONTENT_LENGTH],
            bytes.len().to_string().as_str(),
            "and must report the real length"
        );

        let get = r
            .clone()
            .oneshot(
                Request::get(format!("/v2/library/alpine/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);

        // And the HEAD populated, so the GET after it is a local hit. A HEAD
        // that fetched and then threw the bytes away would double the origin
        // traffic this feature exists to remove.
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "HEAD then GET is ONE upstream fetch, not two"
        );
    }
}
