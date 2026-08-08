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
        // Coverage gate BEFORE validation: buck2 consults the AC before
        // Execute, so gating only dispatch left lookups racing dark
        // ranges during the join window - banked blobs read as
        // unservable and the graph roots re-executed every lap
        // (--require-shards' first lap, run 29602268246, missed this).
        self.driver.await_pool_formed().await;
        // A hit is a promise the CAS can deliver every referenced blob.
        // Cache eviction breaks that silently (reader 28932994472) - the
        // validated gate reports a miss instead, so the client re-executes
        // and the fleet re-uploads: self-healing.
        match self.driver.validated_ac_get(&dig(d)?.hash).await {
            AcLookup::Hit(result) => {
                self.stats.ac_hits.fetch_add(1, Relaxed);
                self.stats
                    .ac_bytes
                    .fetch_add(result.encoded_len() as u64, Relaxed);
                Ok(Response::new(*result))
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
                AcLookup::Hit(result) => {
                    if result.exit_code == 0 {
                        self.driver.ac_hit_ok.fetch_add(1, Relaxed);
                    } else {
                        self.driver.ac_hit_fail.fetch_add(1, Relaxed);
                    }
                    return Ok(op_stream(&d, *result, true));
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
        let cacheable = !outcome.do_not_cache
            && (outcome.action_result.exit_code == 0 || self.driver.cache_failures());
        if cacheable {
            let _ = self
                .driver
                .store
                .ac_put(&d.hash, &outcome.action_result.encode_to_vec())
                .await;
            self.driver.note_ac_written(&d.hash).await;
        }
        Ok(op_stream(&d, outcome.action_result, false))
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

/// Every REAPI service the driver serves, wired onto one router.
///
/// Extracted from `main` so a test can bind it to an ephemeral port and drive
/// it through a real client. Calling the service structs directly exercises
/// their logic but not the transport - and the transport is its own failure
/// surface: a client can connect to a listening socket and still never
/// complete a round trip (buck2-fixups run 31157521034 sat on
/// `[re_action_cache]` for four hours against a driver reporting `ac_ok=0
/// ac_fail=0` - nothing arrived, so nothing could fail).
pub fn router(
    driver: Arc<crate::driver::Driver>,
    store: Arc<Store>,
    stats: Arc<RpcStats>,
) -> tonic::transport::server::Router {
    // rustc rlibs can be chunky.
    let max = 256 * 1024 * 1024;
    tonic::transport::Server::builder()
        .add_service(
            re::capabilities_server::CapabilitiesServer::new(Caps).max_decoding_message_size(max),
        )
        .add_service(
            re::content_addressable_storage_server::ContentAddressableStorageServer::new(Cas {
                driver: driver.clone(),
                stats: stats.clone(),
            })
            .max_decoding_message_size(max),
        )
        .add_service(
            bs::byte_stream_server::ByteStreamServer::new(ByteStreamSvc {
                driver: driver.clone(),
                stats: stats.clone(),
            })
            .max_decoding_message_size(max),
        )
        .add_service(
            re::action_cache_server::ActionCacheServer::new(Ac {
                store,
                stats: stats.clone(),
                driver: driver.clone(),
            })
            .max_decoding_message_size(max),
        )
        .add_service(
            re::execution_server::ExecutionServer::new(Exec { driver, stats })
                .max_decoding_message_size(max),
        )
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
                require_shards: 0,
                local_exec: false,
                decentralized: false,
                hardlinks: true,
                cache_failures: false,
                locality: false,
                prefetch_metadata: false,
                name_independent: true,
                addr_file: None,
                finalize_file: None,
                scratch: std::env::temp_dir(),
            },
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

    // ---- transport round-trips -------------------------------------------
    //
    // The tests above call the service structs directly, which exercises their
    // logic but not the wire. A client can connect to a listening socket and
    // still never complete a round trip - that is what stranded run
    // 31157521034 for four hours, with buck2 blocked on `[re_action_cache]`
    // and the driver reporting `ac_ok=0 ac_fail=0`: nothing arrived, so
    // nothing could fail, and neither side had a deadline. Every test here
    // asserts under a timeout, so a hang is a failure rather than a hung run.

    use std::time::Duration;

    /// Ceiling for a loopback round trip. Generous for a laptop under load,
    /// still four hours short of the outage this pins.
    const RTT: Duration = Duration::from_secs(10);

    /// Serve the full router on an ephemeral port; returns its address.
    async fn serve_rig() -> (String, Arc<Store>, Arc<crate::driver::Driver>) {
        let ac = rig();
        let (store, driver) = (ac.store.clone(), ac.driver.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = router(driver.clone(), store.clone(), Arc::new(RpcStats::default()));
        tokio::spawn(async move {
            router
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
        });
        (format!("http://{addr}"), store, driver)
    }

    /// The bind-then-serve contract: once the listener exists, a client must be
    /// able to connect AND get an answer. `main` announces readiness off the
    /// same bind, and the driver action releases the build legs on that line.
    #[tokio::test]
    async fn a_client_completes_a_round_trip_against_the_bound_port() {
        let (addr, _s, _d) = serve_rig().await;
        let mut c = tokio::time::timeout(
            RTT,
            re::capabilities_client::CapabilitiesClient::connect(addr),
        )
        .await
        .expect("connect did not hang")
        .expect("connected");
        let caps = tokio::time::timeout(
            RTT,
            c.get_capabilities(Request::new(re::GetCapabilitiesRequest::default())),
        )
        .await
        .expect("GetCapabilities did not hang")
        .expect("capabilities served");
        assert!(
            caps.into_inner().cache_capabilities.is_some(),
            "a client that gets no cache_capabilities cannot decide to use the cache"
        );
    }

    /// buck2 asks the AC first and blocks on the answer. A miss must come back
    /// promptly as NOT_FOUND - silence is indistinguishable from a slow hit,
    /// and buck2 waits.
    #[tokio::test]
    async fn an_ac_miss_answers_not_found_over_the_wire() {
        let (addr, _s, _d) = serve_rig().await;
        let mut c = re::action_cache_client::ActionCacheClient::connect(addr)
            .await
            .unwrap();
        let err = tokio::time::timeout(RTT, c.get_action_result(req(&"b".repeat(64))))
            .await
            .expect("AC miss did not hang")
            .expect_err("a miss is an error status");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// FindMissingBlobs is the next call buck2 makes, and it is how it decides
    /// what to upload. An empty request must still answer.
    #[tokio::test]
    async fn find_missing_blobs_answers_over_the_wire() {
        let (addr, store, _d) = serve_rig().await;
        let d = crate::mesh::Dig {
            hash: ABC.into(),
            size: 3,
        };
        store.put(Some(&d), b"abc").await.unwrap();
        let mut c =
            re::content_addressable_storage_client::ContentAddressableStorageClient::connect(addr)
                .await
                .unwrap();
        let present = re::Digest {
            hash: ABC.into(),
            size_bytes: 3,
        };
        let absent = re::Digest {
            hash: "c".repeat(64),
            size_bytes: 7,
        };
        let resp = tokio::time::timeout(
            RTT,
            c.find_missing_blobs(Request::new(re::FindMissingBlobsRequest {
                blob_digests: vec![present.clone(), absent.clone()],
                ..Default::default()
            })),
        )
        .await
        .expect("FindMissingBlobs did not hang")
        .expect("served")
        .into_inner();
        let missing: Vec<String> = resp
            .missing_blob_digests
            .iter()
            .map(|d| d.hash.clone())
            .collect();
        assert!(
            !missing.contains(&present.hash),
            "a blob the store holds must not be reported missing - the client would re-upload it"
        );
        assert!(
            missing.contains(&absent.hash),
            "a blob the store lacks must be reported missing - otherwise the client never uploads it \
             and every later action referencing it strands"
        );
    }

    // ---- probes: edges a real client can reach -----------------------------

    /// `dig()` accepts any ASCII hex digit, so an UPPERCASE digest passes
    /// validation. If the store keys on lowercase, such a digest is accepted
    /// and then never found - a silent permanent miss rather than a rejection.
    /// REAPI does not forbid uppercase, so a client may legitimately send it.
    #[tokio::test]
    async fn an_uppercase_digest_is_not_silently_unfindable() {
        let (addr, store, _d) = serve_rig().await;
        let d = crate::mesh::Dig {
            hash: ABC.into(),
            size: 3,
        };
        store.put(Some(&d), b"abc").await.unwrap();

        let mut c =
            re::content_addressable_storage_client::ContentAddressableStorageClient::connect(addr)
                .await
                .unwrap();
        let upper = re::Digest {
            hash: ABC.to_uppercase(),
            size_bytes: 3,
        };
        let resp = tokio::time::timeout(
            RTT,
            c.find_missing_blobs(Request::new(re::FindMissingBlobsRequest {
                blob_digests: vec![upper.clone()],
                ..Default::default()
            })),
        )
        .await
        .expect("did not hang")
        .expect("served")
        .into_inner();
        assert!(
            resp.missing_blob_digests.is_empty(),
            "an uppercase spelling of a blob we hold was reported missing: either \
             normalise the hash on the way in, or reject non-lowercase at the door - \
             accepting it and never finding it is the worst of the three"
        );
    }

    /// buck2 sends empty FindMissingBlobs batches. An empty request must answer
    /// empty, not error and not hang.
    #[tokio::test]
    async fn an_empty_find_missing_batch_answers_empty() {
        let (addr, _s, _d) = serve_rig().await;
        let mut c =
            re::content_addressable_storage_client::ContentAddressableStorageClient::connect(addr)
                .await
                .unwrap();
        let resp = tokio::time::timeout(
            RTT,
            c.find_missing_blobs(Request::new(re::FindMissingBlobsRequest::default())),
        )
        .await
        .expect("did not hang")
        .expect("served")
        .into_inner();
        assert!(resp.missing_blob_digests.is_empty());
    }

    /// A digest that is not 64 hex must be REJECTED, not accepted-and-lost. The
    /// error names SHA256 so a misconfigured client learns why.
    #[tokio::test]
    async fn a_non_sha256_digest_is_rejected_with_a_legible_error() {
        let (addr, _s, _d) = serve_rig().await;
        let mut c =
            re::content_addressable_storage_client::ContentAddressableStorageClient::connect(addr)
                .await
                .unwrap();
        let err = tokio::time::timeout(
            RTT,
            c.find_missing_blobs(Request::new(re::FindMissingBlobsRequest {
                blob_digests: vec![re::Digest {
                    hash: "deadbeef".into(),
                    size_bytes: 4,
                }],
                ..Default::default()
            })),
        )
        .await
        .expect("did not hang")
        .expect_err("a 8-hex digest is not sha256");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("SHA256") || err.message().contains("sha256"),
            "the error should name the likely cause, got {:?}",
            err.message()
        );
    }

    /// ByteStream resource names carry an optional instance prefix. buck2 sends
    /// one; parsing it wrong loses every blob read.
    #[test]
    fn resource_names_parse_with_and_without_an_instance_prefix() {
        let bare = parse_resource(&format!("blobs/{ABC}/3")).expect("bare");
        let instanced = parse_resource(&format!("my-instance/blobs/{ABC}/3")).expect("instanced");
        let upload = parse_resource(&format!(
            "my-instance/uploads/550e8400-e29b-41d4-a716-446655440000/blobs/{ABC}/3"
        ))
        .expect("upload form");
        assert_eq!(bare.hash, ABC);
        assert_eq!(instanced.hash, ABC);
        assert_eq!(upload.hash, ABC);
        assert_eq!((bare.size, instanced.size, upload.size), (3, 3, 3));
    }
}
