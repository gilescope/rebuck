//! The localhost REAPI gRPC surface buck2 dials: Capabilities, CAS,
//! ByteStream, ActionCache, Execution. Everything funnels into the Store and
//! (for Execute) the Driver.
//!
//! v0 scope: SHA256 digests, no compressed-blobs, no GetTree, Execute is
//! synchronous (the stream yields a single `done` Operation — buck2's OSS
//! client is fine with that; WaitExecution only matters if the stream drops).

use std::pin::Pin;
use std::sync::Arc;

use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use bazel_remote_apis::build::bazel::semver::SemVer;
use bazel_remote_apis::google::bytestream as bs;
use bazel_remote_apis::google::longrunning::{operation, Operation};
use bazel_remote_apis::google::protobuf::Any;
use futures::Stream;
use prost::Message;
use tonic::{Request, Response, Status, Streaming};

use crate::driver::Driver;
use crate::mesh::Dig;
use crate::store::Store;

const MAX_BATCH: i64 = 4 * 1024 * 1024;

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
}

#[tonic::async_trait]
impl re::content_addressable_storage_server::ContentAddressableStorage for Cas {
    async fn find_missing_blobs(
        &self,
        req: Request<re::FindMissingBlobsRequest>,
    ) -> Result<Response<re::FindMissingBlobsResponse>, Status> {
        let mut missing = Vec::new();
        for d in &req.get_ref().blob_digests {
            // Provider-indexed blobs count as present (decentralized mode).
            if !self.driver.has_blob(&dig(d)?).await {
                missing.push(d.clone());
            }
        }
        Ok(Response::new(re::FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        req: Request<re::BatchUpdateBlobsRequest>,
    ) -> Result<Response<re::BatchUpdateBlobsResponse>, Status> {
        let mut responses = Vec::new();
        for r in &req.get_ref().requests {
            let Some(d) = &r.digest else { continue };
            let status = match self.driver.store.put(Some(&dig(d)?), &r.data).await {
                Ok(_) => ok_status(),
                Err(e) => rpc_status(tonic::Code::InvalidArgument, &format!("{e:#}")),
            };
            responses.push(re::batch_update_blobs_response::Response {
                digest: Some(d.clone()),
                status: Some(status),
            });
        }
        Ok(Response::new(re::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        req: Request<re::BatchReadBlobsRequest>,
    ) -> Result<Response<re::BatchReadBlobsResponse>, Status> {
        let mut responses = Vec::new();
        for d in &req.get_ref().digests {
            let (data, status) = match self.driver.get_blob(&dig(d)?).await {
                Ok(Some(bytes)) => (bytes, ok_status()),
                Ok(None) => (
                    Vec::new(),
                    rpc_status(tonic::Code::NotFound, "blob not found"),
                ),
                Err(e) => (
                    Vec::new(),
                    rpc_status(tonic::Code::Internal, &format!("{e:#}")),
                ),
            };
            responses.push(re::batch_read_blobs_response::Response {
                digest: Some(d.clone()),
                data,
                compressor: 0,
                status: Some(status),
            });
        }
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
}

#[tonic::async_trait]
impl re::action_cache_server::ActionCache for Ac {
    async fn get_action_result(
        &self,
        req: Request<re::GetActionResultRequest>,
    ) -> Result<Response<re::ActionResult>, Status> {
        let d = req.get_ref().action_digest.as_ref().ok_or_else(no_digest)?;
        match self.store.ac_get(&dig(d)?.hash).await {
            Some(bytes) => re::ActionResult::decode(bytes.as_slice())
                .map(Response::new)
                .map_err(|e| Status::internal(format!("corrupt AC entry: {e}"))),
            None => Err(Status::not_found("action not cached")),
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
        self.store
            .ac_put(&dig(d)?.hash, &result.encode_to_vec())
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;
        Ok(Response::new(result))
    }
}

// ------------------------------------------------------------------- execution

pub struct Exec {
    pub driver: Arc<Driver>,
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

        // AC short-circuit — the dedup layer.
        if !r.skip_cache_lookup {
            if let Some(bytes) = self.driver.store.ac_get(&d.hash).await {
                if let Ok(result) = re::ActionResult::decode(bytes.as_slice()) {
                    return Ok(op_stream(&d, result, true));
                }
            }
        }

        let outcome = self
            .driver
            .execute(&d)
            .await
            .map_err(|e| Status::internal(format!("{e:#}")))?;

        if outcome.action_result.exit_code == 0 && !outcome.do_not_cache {
            let _ = self
                .driver
                .store
                .ac_put(&d.hash, &outcome.action_result.encode_to_vec())
                .await;
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
