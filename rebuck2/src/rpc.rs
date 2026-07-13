//! The localhost REAPI gRPC surface buck2 dials: Capabilities, CAS,
//! ByteStream, ActionCache, Execution. Everything funnels into the Store and
//! (for Execute) the Driver.
//!
//! v0 scope: SHA256 digests, no compressed-blobs, no GetTree, Execute is
//! synchronous (the stream yields a single `done` Operation — buck2's OSS
//! client is fine with that; WaitExecution only matters if the stream drops).

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use bazel_remote_apis::build::bazel::semver::SemVer;
use bazel_remote_apis::google::bytestream as bs;
use bazel_remote_apis::google::longrunning::{operation, Operation};
use bazel_remote_apis::google::protobuf::Any;
use futures::Stream;
use prost::Message;
use tonic::{Request, Response, Status, Streaming};

use crate::driver::{AcLookup, Driver};
use crate::mesh::Dig;
use crate::store::Store;

const MAX_BATCH: i64 = 4 * 1024 * 1024;

/// Per-request-type accounting for the localhost gRPC surface. `served_total`
/// alone can't attribute the client's download volume (e.g. 1.1 GiB pulled on
/// a warm `materializations=none` build) - these split it by AC result
/// payloads vs CAS blob reads vs uploads, surfaced in the stats heartbeat.
#[derive(Default)]
pub struct RpcStats {
    /// `GetActionResult` served from the AC.
    pub ac_hits: AtomicU64,
    /// `GetActionResult` that returned NOT_FOUND.
    pub ac_misses: AtomicU64,
    /// Hits withheld because a referenced blob is unfetchable (evicted CAS):
    /// reported to the client as a miss so it re-executes and re-uploads.
    pub ac_unservable: AtomicU64,
    /// Encoded `ActionResult` payload bytes - hits only, misses add nothing.
    pub ac_bytes: AtomicU64,
    /// Blobs served via `BatchReadBlobs` + `ByteStream::Read` combined.
    pub blobs_read: AtomicU64,
    /// Bytes for [`Self::blobs_read`] (post-range-slice for stream reads).
    pub blob_read_bytes: AtomicU64,
    /// Bytes accepted via `BatchUpdateBlobs` + `ByteStream::Write` combined.
    pub blob_write_bytes: AtomicU64,
}

type OpStream = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send + 'static>>;

fn dig(d: &re::Digest) -> Result<Dig, Status> {
    if d.hash.len() != 64 || !d.hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Status::invalid_argument(format!(
            "expected 64-hex sha256 digest, got {:?} (is buck2 configured for SHA256?)",
            d.hash
        )));
    }
    Ok(Dig {
        hash: d.hash.clone(),
        size: d.size_bytes,
    })
}

/// `[instance/]blobs/{hash}/{size}` or `[instance/]uploads/{uuid}/blobs/{hash}/{size}[/...]`.
fn parse_resource(name: &str) -> Result<Dig, Status> {
    let segs: Vec<&str> = name.split('/').collect();
    if let Some(i) = segs.iter().position(|s| *s == "compressed-blobs") {
        let _ = i;
        return Err(Status::unimplemented("compressed-blobs not supported"));
    }
    let i = segs
        .iter()
        .position(|s| *s == "blobs")
        .ok_or_else(|| Status::invalid_argument(format!("no 'blobs' segment in {name:?}")))?;
    let hash = segs
        .get(i + 1)
        .ok_or_else(|| Status::invalid_argument(format!("no hash in {name:?}")))?;
    let size: i64 = segs
        .get(i + 2)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Status::invalid_argument(format!("no size in {name:?}")))?;
    dig(&re::Digest {
        hash: (*hash).into(),
        size_bytes: size,
    })
}

// ---------------------------------------------------------------- capabilities

pub struct Caps;

#[tonic::async_trait]
impl re::capabilities_server::Capabilities for Caps {
    async fn get_capabilities(
        &self,
        _req: Request<re::GetCapabilitiesRequest>,
    ) -> Result<Response<re::ServerCapabilities>, Status> {
        let sha256 = re::digest_function::Value::Sha256 as i32;
        Ok(Response::new(re::ServerCapabilities {
            cache_capabilities: Some(re::CacheCapabilities {
                digest_functions: vec![sha256],
                action_cache_update_capabilities: Some(re::ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                max_batch_total_size_bytes: MAX_BATCH,
                symlink_absolute_path_strategy: re::symlink_absolute_path_strategy::Value::Allowed
                    as i32,
                ..Default::default()
            }),
            execution_capabilities: Some(re::ExecutionCapabilities {
                digest_function: sha256,
                digest_functions: vec![sha256],
                exec_enabled: true,
                ..Default::default()
            }),
            low_api_version: Some(SemVer {
                major: 2,
                ..Default::default()
            }),
            high_api_version: Some(SemVer {
                major: 2,
                minor: 3,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

// ------------------------------------------------------------------------ cas

pub struct Cas {
    pub driver: Arc<Driver>,
    pub stats: Arc<RpcStats>,
}

#[tonic::async_trait]
impl re::content_addressable_storage_server::ContentAddressableStorage for Cas {
    async fn find_missing_blobs(
        &self,
        req: Request<re::FindMissingBlobsRequest>,
    ) -> Result<Response<re::FindMissingBlobsResponse>, Status> {
        let digs: Vec<crate::mesh::Dig> = req
            .get_ref()
            .blob_digests
            .iter()
            .map(dig)
            .collect::<Result<_, _>>()?;
        // Mesh-wide presence: local + provider index + bloom-routed exact
        // verification against workers (shard-seeded stores count).
        let have = self.driver.has_blobs(&digs).await;
        let missing = req
            .get_ref()
            .blob_digests
            .iter()
            .zip(&have)
            .filter(|(_, h)| !**h)
            .map(|(d, _)| d.clone())
            .collect();
        Ok(Response::new(re::FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        req: Request<re::BatchUpdateBlobsRequest>,
    ) -> Result<Response<re::BatchUpdateBlobsResponse>, Status> {
        // Concurrent puts (store tmp-names are collision-free per call);
        // `buffered` keeps REAPI reply order matching the request. Futures
        // built eagerly, streamed lazily: mapping an async closure over
        // borrowed items trips HRTB inference inside async trait methods.
        use futures::StreamExt;
        let futs: Vec<_> = req
            .get_ref()
            .requests
            .iter()
            .map(|r| async move {
                let Some(d) = &r.digest else { return Ok(None) };
                self.stats
                    .blob_write_bytes
                    .fetch_add(r.data.len() as u64, Relaxed);
                let status = match self.driver.store.put(Some(&dig(d)?), &r.data).await {
                    Ok(_) => ok_status(),
                    Err(e) => rpc_status(tonic::Code::InvalidArgument, &format!("{e:#}")),
                };
                Ok::<_, Status>(Some(re::batch_update_blobs_response::Response {
                    digest: Some(d.clone()),
                    status: Some(status),
                }))
            })
            .collect();
        let responses: Vec<_> = futures::stream::iter(futs).buffered(16).collect().await;
        let responses = responses
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Response::new(re::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        req: Request<re::BatchReadBlobsRequest>,
    ) -> Result<Response<re::BatchReadBlobsResponse>, Status> {
        // Concurrent reads: a cold blob is a full mesh fetch, and buck2
        // batches these on the hot path. `buffered` (not `buffer_unordered`)
        // keeps reply order; get_blob's mesh_fetches permit is acquired
        // inside each future, so a big batch can't starve the fleet.
        // Futures built eagerly for the same HRTB reason as above.
        use futures::StreamExt;
        let futs: Vec<_> = req
            .get_ref()
            .digests
            .iter()
            .map(|d| async move {
                let (data, status) = match self.driver.get_blob(&dig(d)?).await {
                    Ok(Some(bytes)) => {
                        self.stats.blobs_read.fetch_add(1, Relaxed);
                        self.stats
                            .blob_read_bytes
                            .fetch_add(bytes.len() as u64, Relaxed);
                        (bytes, ok_status())
                    }
                    Ok(None) => (
                        Vec::new(),
                        rpc_status(tonic::Code::NotFound, "blob not found"),
                    ),
                    Err(e) => (
                        Vec::new(),
                        rpc_status(tonic::Code::Internal, &format!("{e:#}")),
                    ),
                };
                Ok::<_, Status>(re::batch_read_blobs_response::Response {
                    digest: Some(d.clone()),
                    data,
                    compressor: 0,
                    status: Some(status),
                })
            })
            .collect();
        let responses: Vec<_> = futures::stream::iter(futs).buffered(32).collect().await;
        let responses = responses.into_iter().collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(re::BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream =
        Pin<Box<dyn Stream<Item = Result<re::GetTreeResponse, Status>> + Send + 'static>>;

    async fn get_tree(
        &self,
        _req: Request<re::GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("GetTree"))
    }

    async fn split_blob(
        &self,
        _req: Request<re::SplitBlobRequest>,
    ) -> Result<Response<re::SplitBlobResponse>, Status> {
        Err(Status::unimplemented("SplitBlob"))
    }

    async fn splice_blob(
        &self,
        _req: Request<re::SpliceBlobRequest>,
    ) -> Result<Response<re::SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("SpliceBlob"))
    }
}

// ------------------------------------------------------------------ bytestream

pub struct ByteStreamSvc {
    pub driver: Arc<Driver>,
    pub stats: Arc<RpcStats>,
}

#[tonic::async_trait]
impl bs::byte_stream_server::ByteStream for ByteStreamSvc {
    type ReadStream =
        Pin<Box<dyn Stream<Item = Result<bs::ReadResponse, Status>> + Send + 'static>>;

    async fn read(
        &self,
        req: Request<bs::ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let r = req.get_ref();
        let d = parse_resource(&r.resource_name)?;
        let bytes = self
            .driver
            .get_blob(&d)
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?
            .ok_or_else(|| Status::not_found(format!("blob {} not found", d.hash)))?;
        let start = usize::try_from(r.read_offset.max(0))
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let end = if r.read_limit > 0 {
            (start + r.read_limit as usize).min(bytes.len())
        } else {
            bytes.len()
        };
        self.stats.blobs_read.fetch_add(1, Relaxed);
        self.stats
            .blob_read_bytes
            .fetch_add((end - start) as u64, Relaxed);
        let chunks: Vec<Result<bs::ReadResponse, Status>> = bytes[start..end]
            .chunks(1024 * 1024)
            .map(|c| Ok(bs::ReadResponse { data: c.to_vec() }))
            .collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn write(
        &self,
        req: Request<Streaming<bs::WriteRequest>>,
    ) -> Result<Response<bs::WriteResponse>, Status> {
        let mut stream = req.into_inner();
        let mut expected: Option<Dig> = None;
        let mut data = Vec::new();
        while let Some(msg) = stream.message().await? {
            if expected.is_none() && !msg.resource_name.is_empty() {
                let d = parse_resource(&msg.resource_name)?;
                data.reserve(d.size as usize);
                expected = Some(d);
            }
            data.extend_from_slice(&msg.data);
            if msg.finish_write {
                break;
            }
        }
        let expected = expected.ok_or_else(|| Status::invalid_argument("no resource_name"))?;
        let committed = data.len() as i64;
        self.stats
            .blob_write_bytes
            .fetch_add(data.len() as u64, Relaxed);
        self.driver
            .store
            .put(Some(&expected), &data)
            .await
            .map_err(|e| Status::invalid_argument(format!("{e:#}")))?;
        Ok(Response::new(bs::WriteResponse {
            committed_size: committed,
        }))
    }

    async fn query_write_status(
        &self,
        _req: Request<bs::QueryWriteStatusRequest>,
    ) -> Result<Response<bs::QueryWriteStatusResponse>, Status> {
        // "Nothing committed" makes the client restart the upload — always safe.
        Ok(Response::new(bs::QueryWriteStatusResponse {
            committed_size: 0,
            complete: false,
        }))
    }
}

// ---------------------------------------------------------------- action cache

pub struct Ac {
    pub store: Arc<Store>,
    pub stats: Arc<RpcStats>,
    pub driver: Arc<Driver>,
}

#[tonic::async_trait]
impl re::action_cache_server::ActionCache for Ac {
    async fn get_action_result(
        &self,
        req: Request<re::GetActionResultRequest>,
    ) -> Result<Response<re::ActionResult>, Status> {
        let d = req.get_ref().action_digest.as_ref().ok_or_else(no_digest)?;
        // A hit is a promise the CAS can deliver every referenced blob.
        // Cache eviction breaks that silently (reader 28932994472) - the
        // validated gate reports a miss instead, so the client re-executes
        // and the fleet re-uploads: self-healing.
        match self.driver.validated_ac_get(&dig(d)?.hash).await {
            AcLookup::Hit(bytes) => {
                self.stats.ac_hits.fetch_add(1, Relaxed);
                self.stats.ac_bytes.fetch_add(bytes.len() as u64, Relaxed);
                // The fleet ships opaque bytes; decoding is the frontend's job.
                let result = re::ActionResult::decode(bytes.as_slice())
                    .map_err(|e| Status::internal(format!("corrupt cached result: {e}")))?;
                Ok(Response::new(result))
            }
            AcLookup::Unservable => {
                self.stats.ac_unservable.fetch_add(1, Relaxed);
                Err(Status::not_found(
                    "cached result references unfetchable blobs",
                ))
            }
            AcLookup::Miss => {
                self.stats.ac_misses.fetch_add(1, Relaxed);
                Err(Status::not_found("action not cached"))
            }
        }
    }

    async fn update_action_result(
        &self,
        req: Request<re::UpdateActionResultRequest>,
    ) -> Result<Response<re::ActionResult>, Status> {
        let r = req.get_ref();
        let d = r.action_digest.as_ref().ok_or_else(no_digest)?;
        let result = r
            .action_result
            .clone()
            .ok_or_else(|| Status::invalid_argument("no result"))?;
        let hash = dig(d)?.hash;
        self.store
            .ac_put(&hash, &result.encode_to_vec())
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;
        self.driver.note_ac_written(&hash).await;
        Ok(Response::new(result))
    }
}

// ------------------------------------------------------------------- execution

pub struct Exec {
    pub driver: Arc<Driver>,
    pub stats: Arc<RpcStats>,
}

#[tonic::async_trait]
impl re::execution_server::Execution for Exec {
    type ExecuteStream = OpStream;
    type WaitExecutionStream = Self::ExecuteStream;

    async fn execute(
        &self,
        req: Request<re::ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let r = req.get_ref();
        let d = dig(r.action_digest.as_ref().ok_or_else(no_digest)?)?;

        // AC short-circuit — the dedup layer. Same validated gate as the
        // GetActionResult endpoint: an unvalidated door here served 17k
        // blob-less results after cache eviction (writer 28935304124).
        if !r.skip_cache_lookup {
            match self.driver.validated_ac_get(&d.hash).await {
                AcLookup::Hit(bytes) => {
                    let result = re::ActionResult::decode(bytes.as_slice())
                        .map_err(|e| Status::internal(format!("corrupt cached result: {e}")))?;
                    if result.exit_code == 0 {
                        self.driver.ac_hit_ok.fetch_add(1, Relaxed);
                    } else {
                        self.driver.ac_hit_fail.fetch_add(1, Relaxed);
                    }
                    return Ok(op_stream(&d, result, true));
                }
                AcLookup::Unservable => {
                    self.stats.ac_unservable.fetch_add(1, Relaxed);
                }
                AcLookup::Miss => {}
            }
        }

        let outcome = self
            .driver
            .execute(&d)
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;

        if outcome.do_not_cache {
            self.driver
                .dnc_exec
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // The fleet handed back opaque bytes; this frontend owns the meaning.
        let result = re::ActionResult::decode(outcome.result.as_slice())
            .map_err(|e| Status::internal(format!("worker returned a bad result: {e}")))?;
        let cacheable =
            !outcome.do_not_cache && (result.exit_code == 0 || self.driver.cache_failures());
        if cacheable {
            let _ = self.driver.store.ac_put(&d.hash, &outcome.result).await;
            self.driver.note_ac_written(&d.hash).await;
        }
        Ok(op_stream(&d, result, false))
    }

    async fn wait_execution(
        &self,
        _req: Request<re::WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        // Execute completes inline, so there is never an operation to re-join.
        Err(Status::not_found("operation already completed or unknown"))
    }
}

fn op_stream(action: &Dig, result: re::ActionResult, cached: bool) -> Response<OpStream> {
    let response = re::ExecuteResponse {
        result: Some(result),
        cached_result: cached,
        status: Some(ok_status()),
        server_logs: Default::default(),
        message: String::new(),
    };
    let op = Operation {
        name: format!("operations/{}", action.hash),
        done: true,
        metadata: None,
        result: Some(operation::Result::Response(Any {
            type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".into(),
            value: response.encode_to_vec(),
        })),
    };
    Response::new(Box::pin(tokio_stream::once(Ok(op))))
}

fn no_digest() -> Status {
    Status::invalid_argument("missing action_digest")
}

fn ok_status() -> bazel_remote_apis::google::rpc::Status {
    bazel_remote_apis::google::rpc::Status::default()
}

fn rpc_status(code: tonic::Code, msg: &str) -> bazel_remote_apis::google::rpc::Status {
    bazel_remote_apis::google::rpc::Status {
        code: code as i32,
        message: msg.into(),
        details: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Driver, DriverCfg};
    use crate::lease;
    use tonic::Request;

    /// sha256("abc") - the store verifies content digests on put.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn rig() -> Ac {
        let dir = tempfile::tempdir().unwrap().keep();
        let store = Arc::new(Store::new(dir).unwrap());
        let driver = Driver::new(
            store.clone(),
            DriverCfg {
                session: "test".into(),
                min_workers: 0,
                local_exec: false,
                decentralized: false,
                hardlinks: true,
                cache_failures: false,
                locality: false,
                prefetch_metadata: false,
                addr_file: None,
                finalize_file: None,
                scratch: std::env::temp_dir(),
                lease_ttl: lease::DEFAULT_LEASE_TTL,
            },
            Arc::new(crate::payload::reapi::Reapi),
        );
        Ac {
            store,
            stats: Arc::new(RpcStats::default()),
            driver,
        }
    }

    fn req(hash: &str) -> Request<re::GetActionResultRequest> {
        Request::new(re::GetActionResultRequest {
            action_digest: Some(re::Digest {
                hash: hash.into(),
                size_bytes: 1,
            }),
            ..Default::default()
        })
    }

    /// An AC hit whose output blob is unfetchable must report NOT_FOUND -
    /// serving it strands the client on blobs nobody can deliver (the
    /// evicted-shards outage, run 28932994472). Present blobs serve normally.
    #[tokio::test]
    async fn ac_hit_validates_blob_presence() {
        use re::action_cache_server::ActionCache;
        let ac = rig();
        let key = "a".repeat(64);
        let result = re::ActionResult {
            output_files: vec![re::OutputFile {
                path: "out".into(),
                digest: Some(re::Digest {
                    hash: ABC.into(),
                    size_bytes: 3,
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        ac.store
            .ac_put(&key, &result.encode_to_vec())
            .await
            .unwrap();

        let err = ac.get_action_result(req(&key)).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(ac.stats.ac_unservable.load(Relaxed), 1);

        // Store the referenced blob; the unservable verdict is memoized
        // until the entry is rewritten - drive the real invalidation via
        // the driver's note_ac_written (what UpdateActionResult calls).
        let d = crate::mesh::Dig {
            hash: ABC.into(),
            size: 3,
        };
        ac.store.put(Some(&d), b"abc").await.unwrap();
        ac.driver.note_ac_written(&key).await;
        ac.get_action_result(req(&key)).await.expect("now servable");
        assert_eq!(ac.stats.ac_hits.load(Relaxed), 1);
    }
}
