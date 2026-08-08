//! rebuck2 in front of a buildkitd, changing nothing.
//!
//! Point earthly at this instead of the daemon and every `SolveRequest`
//! passes through our hands. It forwards all of them unaltered — this
//! version dispatches NOTHING — and reports what it would have offered.
//!
//! That order is deliberate. We do not yet know whether real earthbuild
//! shards contain subtrees worth shipping: principle 11 says a chain rooted
//! at `FROM <registry image>` is the best possible handover, and
//! [`crate::dispatch::analyse`] can now count them, but counting them on
//! invented graphs proves nothing. A proxy that only measures answers "is
//! there anything here to dispatch" before a line is written to exploit it,
//! and it costs one hop.
//!
//! Interposing rather than asking earthbuild for the graph is what keeps
//! this in one repo: the Control service is a stable, versioned wire, and
//! we already speak it.
//!
//! # MEASURED: this IS the right layer, once it serves the gateway too
//!
//! An earlier round concluded the opposite, and was wrong. `Control.Solve`
//! really does arrive with no definition — but the graph was not missing,
//! it was on the other service. A buildkit client drives its build through
//! `LLBBridge` created with `NewLLBBridgeClient(c.conn)`: the SAME
//! connection, a second service. The `Unimplemented` blamed on session
//! relaying was this proxy not offering it.
//!
//! Measured, with a real `buildctl` through a real daemon:
//!
//! ```text
//! [proxy] solve ...: NO definition on the wire (frontend="")
//! [proxy] GATEWAY solve: 3 ops, 0 cuts >= 4, 0 free-frontier
//! ```
//!
//! Three client shapes, and only the middle one is invisible:
//!
//! | client | where the LLB is |
//! | ------------------------------- | ---------------------------- |
//! | raw LLB (our own solve, testkit) | `Control.Solve`, definition set |
//! | frontend by NAME (`--frontend dockerfile.v0`) | nowhere — the frontend runs INSIDE the daemon |
//! | client-built LLB (earthly, `buildctl build < llb`) | `LLBBridge.Solve` — **here** |
//!
//! The invisible one costs nothing: if no graph crosses the wire there is
//! no graph to dispatch, and the daemon was always going to build it alone.
//!
//! # What "transparent" has to mean
//!
//! All nine methods, including the streams. `Session` in particular is
//! BIDIRECTIONAL and carries filesync and credentials — a proxy that
//! forwards Solve but not Session works for exactly the builds that need no
//! local context, which is not the workload we care about.

use std::pin::Pin;

use crate::gateway::frontend as gw;
use bollard_buildkit_proto::moby::buildkit::v1 as control;
use futures::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// Minimum subtree size worth reporting. Over half of every shard is
/// milliseconds of work, so a report that lists every single-op subtree
/// buries the ones that matter.
const MIN_CUT_OPS: usize = 4;

type Chan = tonic::transport::Channel;
type Client = control::control_client::ControlClient<Chan>;
type GwClient = gw::llb_bridge_client::LlbBridgeClient<Chan>;

#[derive(Clone)]
pub struct Proxy {
    /// ONE channel for the whole proxy, cloned per call.
    ///
    /// Not one per call, which is what this had first and is wrong in a way
    /// that only streams notice: the `Client` owns the channel, so returning
    /// from the handler drops it and the still-running stream dies. A unary
    /// call has its response already and never notices; `Session` dies
    /// mid-build, and the daemon reports it as "healthcheck failed ... EOF"
    /// with nothing pointing at a dropped connection. Measured, by doing it
    /// the other way first.
    client: Client,
    channel: Chan,
}

impl Proxy {
    pub async fn connect(upstream: String) -> anyhow::Result<Self> {
        let channel = tonic::transport::Endpoint::new(upstream)?.connect().await?;
        Ok(Proxy {
            client: control::control_client::ControlClient::new(channel.clone()),
            channel,
        })
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    /// The gateway rides the same channel, because the client's does.
    fn gw(&self) -> GwClient {
        gw::llb_bridge_client::LlbBridgeClient::new(self.channel.clone())
    }
}

/// Re-wrap a payload in a Request carrying the ORIGINAL metadata.
///
/// Load-bearing, and its absence is invisible until it is not: buildkit
/// associates a session by headers on the request (the session UUID among
/// them). Forward the stream without them and the daemon accepts the
/// session, cannot match it to the build, and the frontend's filesync call
/// comes back Unimplemented - an error that names nothing to do with
/// metadata. Measured, by breaking it.
fn relay<T, U>(from: Request<T>, payload: U) -> Request<U> {
    let (meta, ext, _) = from.into_parts();
    Request::from_parts(meta, ext, payload)
}

/// Say what this graph offers a dispatcher, and nothing else.
fn report(req: &control::SolveRequest) {
    let Some(def) = &req.definition else {
        println!(
            "[proxy] solve {}: NO definition on the wire (frontend={:?}) - the \
             LLB is generated inside the daemon, not sent to it",
            req.r#ref, req.frontend
        );
        return;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    let biggest = free.first().map(|c| c.ops).unwrap_or(0);
    println!(
        "[proxy] solve {}: {} ops, {} cuts >= {MIN_CUT_OPS}, {} with a free \
         frontier, biggest {biggest} ops",
        req.r#ref,
        a.ops,
        a.cuts.len(),
        free.len(),
    );
}

#[tonic::async_trait]
impl control::control_server::Control for Proxy {
    async fn solve(
        &self,
        request: Request<control::SolveRequest>,
    ) -> Result<Response<control::SolveResponse>, Status> {
        let req = request.into_inner();
        report(&req);
        self.client().solve(req).await
    }

    async fn disk_usage(
        &self,
        request: Request<control::DiskUsageRequest>,
    ) -> Result<Response<control::DiskUsageResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.client()
            .disk_usage(Request::from_parts(meta, ext, req))
            .await
    }

    async fn list_workers(
        &self,
        request: Request<control::ListWorkersRequest>,
    ) -> Result<Response<control::ListWorkersResponse>, Status> {
        self.client().list_workers(request.into_inner()).await
    }

    async fn info(
        &self,
        request: Request<control::InfoRequest>,
    ) -> Result<Response<control::InfoResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.client()
            .info(Request::from_parts(meta, ext, req))
            .await
    }

    async fn update_build_history(
        &self,
        request: Request<control::UpdateBuildHistoryRequest>,
    ) -> Result<Response<control::UpdateBuildHistoryResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.client()
            .update_build_history(Request::from_parts(meta, ext, req))
            .await
    }

    type PruneStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::UsageRecord, Status>> + Send>>;

    async fn prune(
        &self,
        request: Request<control::PruneRequest>,
    ) -> Result<Response<Self::PruneStream>, Status> {
        let (meta, ext, req) = request.into_parts();
        let s = self
            .client()
            .prune(Request::from_parts(meta, ext, req))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }

    type StatusStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::StatusResponse, Status>> + Send>>;

    async fn status(
        &self,
        request: Request<control::StatusRequest>,
    ) -> Result<Response<Self::StatusStream>, Status> {
        let (meta, ext, req) = request.into_parts();
        let s = self
            .client()
            .status(Request::from_parts(meta, ext, req))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }

    type ListenBuildHistoryStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::BuildHistoryEvent, Status>> + Send>>;

    async fn listen_build_history(
        &self,
        request: Request<control::BuildHistoryRequest>,
    ) -> Result<Response<Self::ListenBuildHistoryStream>, Status> {
        let (meta, ext, req) = request.into_parts();
        let s = self
            .client()
            .listen_build_history(Request::from_parts(meta, ext, req))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }

    type SessionStream =
        Pin<Box<dyn futures::Stream<Item = Result<control::BytesMessage, Status>> + Send>>;

    /// The bidirectional one, and the reason a Solve-only proxy is not
    /// enough: this carries filesync and registry credentials. Errors on
    /// the inbound half are dropped rather than forwarded, because a
    /// half-open session is what the daemon sees when a client goes away,
    /// and it already knows what to do about that.
    async fn session(
        &self,
        request: Request<Streaming<control::BytesMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let (meta, ext, stream) = request.into_parts();
        let inbound = stream.filter_map(|m| futures::future::ready(m.ok()));
        let s = self
            .client()
            .session(Request::from_parts(meta, ext, inbound))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }
}

/// What the gateway's Solve offers a dispatcher. THIS is the graph.
fn report_gateway(req: &gw::SolveRequest) {
    let Some(def) = &req.definition else {
        return;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    println!(
        "[proxy] GATEWAY solve: {} ops, {} cuts >= {MIN_CUT_OPS}, {} free-frontier, biggest {} ops",
        a.ops,
        a.cuts.len(),
        free.len(),
        free.first().map(|c| c.ops).unwrap_or(0),
    );
}

/// Serve the Control service on `addr`, forwarding to `upstream`.
pub async fn serve(addr: std::net::SocketAddr, upstream: String) -> anyhow::Result<()> {
    println!("[proxy] buildkit control on {addr} -> {upstream}");
    let proxy = Proxy::connect(upstream).await?;
    tonic::transport::Server::builder()
        .add_service(control::control_server::ControlServer::new(proxy.clone()))
        .add_service(gw::llb_bridge_server::LlbBridgeServer::new(proxy))
        .serve(addr)
        .await?;
    Ok(())
}

/// The GATEWAY, which is where the graph is.
///
/// A buildkit client drives its build through `LLBBridge` on the SAME
/// connection it speaks Control on, so these calls land here beside the
/// Control ones — and `Solve` carries the `Definition` that `Control.Solve`
/// does not. Forwarded unchanged; only `solve` is looked at on the way
/// through.
#[tonic::async_trait]
impl gw::llb_bridge_server::LlbBridge for Proxy {
    async fn solve(
        &self,
        request: Request<gw::SolveRequest>,
    ) -> Result<Response<gw::SolveResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        report_gateway(&req);
        self.gw().solve(Request::from_parts(meta, ext, req)).await
    }

    async fn resolve_image_config(
        &self,
        request: Request<gw::ResolveImageConfigRequest>,
    ) -> Result<Response<gw::ResolveImageConfigResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .resolve_image_config(Request::from_parts(meta, ext, req))
            .await
    }
    async fn resolve_source_meta(
        &self,
        request: Request<gw::ResolveSourceMetaRequest>,
    ) -> Result<Response<gw::ResolveSourceMetaResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .resolve_source_meta(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_file(
        &self,
        request: Request<gw::ReadFileRequest>,
    ) -> Result<Response<gw::ReadFileResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_file(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_dir(
        &self,
        request: Request<gw::ReadDirRequest>,
    ) -> Result<Response<gw::ReadDirResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_dir(Request::from_parts(meta, ext, req))
            .await
    }
    async fn stat_file(
        &self,
        request: Request<gw::StatFileRequest>,
    ) -> Result<Response<gw::StatFileResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .stat_file(Request::from_parts(meta, ext, req))
            .await
    }
    async fn evaluate(
        &self,
        request: Request<gw::EvaluateRequest>,
    ) -> Result<Response<gw::EvaluateResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .evaluate(Request::from_parts(meta, ext, req))
            .await
    }
    async fn ping(
        &self,
        request: Request<gw::PingRequest>,
    ) -> Result<Response<gw::PongResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw().ping(Request::from_parts(meta, ext, req)).await
    }
    async fn inputs(
        &self,
        request: Request<gw::InputsRequest>,
    ) -> Result<Response<gw::InputsResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw().inputs(Request::from_parts(meta, ext, req)).await
    }
    async fn new_container(
        &self,
        request: Request<gw::NewContainerRequest>,
    ) -> Result<Response<gw::NewContainerResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .new_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn release_container(
        &self,
        request: Request<gw::ReleaseContainerRequest>,
    ) -> Result<Response<gw::ReleaseContainerResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .release_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_file_container(
        &self,
        request: Request<gw::ReadFileRequest>,
    ) -> Result<Response<gw::ReadFileResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_file_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_dir_container(
        &self,
        request: Request<gw::ReadDirRequest>,
    ) -> Result<Response<gw::ReadDirResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_dir_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn stat_file_container(
        &self,
        request: Request<gw::StatFileRequest>,
    ) -> Result<Response<gw::StatFileResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .stat_file_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn warn(
        &self,
        request: Request<gw::WarnRequest>,
    ) -> Result<Response<gw::WarnResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw().warn(Request::from_parts(meta, ext, req)).await
    }

    /// `return` is a keyword here and a method name there.
    async fn r#return(
        &self,
        request: Request<gw::ReturnRequest>,
    ) -> Result<Response<gw::ReturnResponse>, Status> {
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .r#return(Request::from_parts(meta, ext, req))
            .await
    }

    type ExecProcessStream =
        Pin<Box<dyn futures::Stream<Item = Result<gw::ExecMessage, Status>> + Send>>;

    async fn exec_process(
        &self,
        request: Request<Streaming<gw::ExecMessage>>,
    ) -> Result<Response<Self::ExecProcessStream>, Status> {
        let (meta, ext, stream) = request.into_parts();
        let inbound = stream.filter_map(|m| futures::future::ready(m.ok()));
        let s = self
            .gw()
            .exec_process(Request::from_parts(meta, ext, inbound))
            .await?;
        Ok(Response::new(Box::pin(s.into_inner())))
    }
}
