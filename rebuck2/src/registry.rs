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
    /// Small, known-bounded values only (manifests). Layers must stream.
    async fn blob_put(&self, bytes: &[u8]) -> Result<String>;
    async fn upload_begin(&self) -> Result<Upload>;
    async fn upload_finish(&self, up: Upload, expected: Option<&str>) -> Result<String>;
    /// `<repo>:<ref>` -> manifest digest.
    async fn tag_get(&self, key: &str) -> Option<String>;
    async fn tag_put(&self, key: &str, manifest_hash: &str) -> Result<()>;

    /// Cross-machine single-flight, if this backend has a fleet to coordinate.
    /// A bare store does not (there is nobody to race); a driver does.
    fn leases(&self) -> Option<&crate::lease::Leases> {
        None
    }
}

#[async_trait::async_trait]
impl RegistryStore for Store {
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
    fn leases(&self) -> Option<&crate::lease::Leases> {
        Some(self.lease_table())
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
}

struct Reg<S> {
    store: Arc<S>,
    bw: Bandwidth,
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
        let Some(hex) = parse_digest(reference) else {
            return err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", reference);
        };
        match method {
            // HEAD needs the length but not the bytes — do not read the blob
            // just to throw it away; a cache probe storm would read the store
            // end to end.
            Method::HEAD => match reg.store.blob_size(hex).await {
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
                Ok(None) => err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", reference),
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
    body: Body,
) -> Response {
    if method != Method::POST {
        return err(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "POST only");
    }
    let Some(leases) = reg.store.leases() else {
        return err(
            StatusCode::NOT_IMPLEMENTED,
            "UNSUPPORTED",
            "this registry has no fleet to coordinate",
        );
    };
    let Some((op, key)) = path.split_once('/') else {
        return err(StatusCode::NOT_FOUND, "UNSUPPORTED", &path);
    };
    if key.is_empty() {
        return err(StatusCode::BAD_REQUEST, "UNSUPPORTED", "empty lease key");
    }

    match op {
        "claim" => match leases.claim_local(key) {
            crate::lease::Claim::Leader => {
                (StatusCode::OK, [("X-Rebuck-Lease", "leader")]).into_response()
            }
            crate::lease::Claim::Follower(rx) => match rx.await {
                Ok(crate::lease::Outcome::Done(result)) => {
                    (StatusCode::OK, [("X-Rebuck-Lease", "follower")], result).into_response()
                }
                Ok(crate::lease::Outcome::Failed(e)) => {
                    err(StatusCode::BAD_GATEWAY, "LEASE_FAILED", &e)
                }
                // The leader died. Never a hang: the caller re-claims and one
                // of the waiters becomes the new leader.
                Ok(crate::lease::Outcome::Retry) | Err(_) => err(
                    StatusCode::CONFLICT,
                    "LEASE_RETRY",
                    "leader vanished; re-claim",
                ),
            },
        },
        "heartbeat" => {
            // Local holders are drop-guarded and need no heartbeat; this exists
            // for symmetry with the mesh path and always succeeds for a live key.
            if leases.heartbeat_local(key) {
                (StatusCode::OK, "alive").into_response()
            } else {
                err(StatusCode::CONFLICT, "LEASE_RETRY", "you no longer hold it")
            }
        }
        // A leader that FAILED must free its followers to rebuild, not hand them
        // a bogus result. Without this they would wait out the whole TTL for a
        // result that is never coming.
        "abandon" => {
            leases.abandon_local(key);
            (StatusCode::OK, "abandoned").into_response()
        }
        "release" => {
            let bytes = match axum::body::to_bytes(body, MAX_MANIFEST).await {
                Ok(b) => b,
                Err(e) => return err(StatusCode::PAYLOAD_TOO_LARGE, "UNSUPPORTED", &e.to_string()),
            };
            leases.release(key, None, crate::lease::Outcome::Done(bytes.to_vec()));
            (StatusCode::OK, "released").into_response()
        }
        _ => err(StatusCode::NOT_FOUND, "UNSUPPORTED", op),
    }
}

/// `GET /_rebuck/stats` — what crossed the coordinator, in JSON. The e2e reads
/// this to answer "is the coordinator a bottleneck?" with a number.
async fn stats_handle<S: RegistryStore>(State(reg): State<Arc<Reg<S>>>) -> Response {
    let leases = reg.store.leases();
    let body = serde_json::json!({
        "uploads":      reg.bw.uploaded.load(Ordering::Relaxed),
        "upload_bytes": reg.bw.upload_bytes.load(Ordering::Relaxed),
        "serves":       reg.bw.served.load(Ordering::Relaxed),
        "serve_bytes":  reg.bw.serve_bytes.load(Ordering::Relaxed),
        "leases_led":       leases.map(|l| l.led.load(Ordering::Relaxed)),
        "leases_merged":    leases.map(|l| l.merged.load(Ordering::Relaxed)),
        "leases_abandoned": leases.map(|l| l.abandoned.load(Ordering::Relaxed)),
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

pub fn router<S: RegistryStore>(store: Arc<S>) -> Router {
    let reg = Arc::new(Reg {
        store,
        bw: Bandwidth::default(),
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

        let (st, _, _) = call(&r, post("/_rebuck/lease/claim/k")).await;
        assert_eq!(st, StatusCode::OK);

        let r2 = r.clone();
        let follower = tokio::spawn(async move { call(&r2, post("/_rebuck/lease/claim/k")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The leader vanishes without publishing.
        d.lease_table().abandon_local("k");

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

        let r2 = r.clone();
        let follower = tokio::spawn(async move { call(&r2, post("/_rebuck/lease/claim/k")).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (st, _, _) = call(&r, post("/_rebuck/lease/abandon/k")).await;
        assert_eq!(st, StatusCode::OK);

        let (st, _, _) = follower.await.unwrap();
        assert_eq!(
            st,
            StatusCode::CONFLICT,
            "the follower must be told to rebuild"
        );
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
