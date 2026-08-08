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
//! # MEASURED: this is the wrong layer, and here is the evidence
//!
//! It works, and it sees nothing worth seeing. A real `buildctl` build
//! through this proxy reports:
//!
//! ```text
//! [proxy] solve ...: NO definition on the wire (frontend="")
//! ```
//!
//! For any FRONTEND or GATEWAY build - which is what buildctl and earthly
//! both do - the LLB is generated INSIDE the daemon, or exchanged over the
//! session as gateway calls. It never crosses `Control.Solve`, so a
//! Control-layer proxy cannot see the graph it exists to analyse.
//!
//! Kept because the finding is worth more than the code: it cost one
//! afternoon and it rules out an approach that looked obviously right. To
//! see earthly's LLB the options are now (a) proxy the gateway service
//! INSIDE the session, which is a much deeper interposition, (b) have
//! earthbuild hand the definition over, or (c) read it back out of
//! buildkit's build history.
//!
//! # KNOWN INCOMPLETE: the session relay
//!
//! Unary methods forward correctly. `Session` does not: the daemon reports
//! `healthcheck failed ... EOF` and the solve is cancelled. Buildkit's
//! session is a TUNNELLED REVERSE connection - the daemon dials back
//! through it to call services the CLIENT implements - and relaying that
//! faithfully at the gRPC message layer is not the same as relaying bytes.
//! Left unfixed on purpose: the finding above means nobody should build on
//! this until the layer question is settled.
//!
//! # What "transparent" has to mean
//!
//! All nine methods, including the streams. `Session` in particular is
//! BIDIRECTIONAL and carries filesync and credentials — a proxy that
//! forwards Solve but not Session works for exactly the builds that need no
//! local context, which is not the workload we care about.

use std::pin::Pin;

use bollard_buildkit_proto::moby::buildkit::v1 as control;
use futures::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// Minimum subtree size worth reporting. Over half of every shard is
/// milliseconds of work, so a report that lists every single-op subtree
/// buries the ones that matter.
const MIN_CUT_OPS: usize = 4;

type Chan = tonic::transport::Channel;
type Client = control::control_client::ControlClient<Chan>;

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
}

impl Proxy {
    pub async fn connect(upstream: String) -> anyhow::Result<Self> {
        Ok(Proxy {
            client: control::control_client::ControlClient::connect(upstream).await?,
        })
    }

    fn client(&self) -> Client {
        self.client.clone()
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

/// Serve the Control service on `addr`, forwarding to `upstream`.
pub async fn serve(addr: std::net::SocketAddr, upstream: String) -> anyhow::Result<()> {
    println!("[proxy] buildkit control on {addr} -> {upstream}");
    let proxy = Proxy::connect(upstream).await?;
    tonic::transport::Server::builder()
        .add_service(control::control_server::ControlServer::new(proxy))
        .serve(addr)
        .await?;
    Ok(())
}
