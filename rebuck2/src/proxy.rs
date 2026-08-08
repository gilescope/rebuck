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
//! # First real measurement: earthly, end to end
//!
//! A real `earthly +test` on a three-target Earthfile, through this proxy,
//! against earthbuild's own buildkitd. It succeeded, and it said:
//!
//! ```text
//! gateway solves : 6
//! ops per solve  : min 3 median 9 max 9
//! sources        : 6 registry, 0 local, 0 other
//! platforms      : {"linux/arm64"}
//! repeated ops   : 31 (73% seen again in a later solve)
//! distinct graphs: 3 of 6 solves (3 identical RESENDS)
//! overlap/solve  : [(0,3), (2,6), (5,9), (9,9), (6,6), (9,9)]
//! ```
//!
//! **The 73% is not what it looks like, and the first reading of it here was
//! wrong.** Half the solves are byte-identical RESENDS of a graph already
//! sent - the client driving the API, not work being shared. Read as
//! overlap it says routing whole Solves would duplicate three quarters of
//! the build, which would have been a conclusion drawn from an artefact.
//!
//! What the three DISTINCT graphs show is nesting: 3 ops, then 6 of which 2
//! are already seen, then 9 of which 5 are. Each solve extends the last, so
//! genuine overlap is 7 of 18 ops - about 39%, not 73%.
//!
//! Nesting is a sharper result than repetition would have been, and it
//! points the other way from the first note:
//!
//! - **Per-Solve routing is worse than the raw number suggested.** The
//!   graphs are not merely overlapping, they are cumulative - the last
//!   solve CONTAINS the earlier ones. Handing solve #3 to another machine
//!   asks it to rebuild everything solve #2 just did.
//! - **The natural unit is the INCREMENT between successive solves**, which
//!   is a subtree. The client hands us its subdivision already, one layer at
//!   a time; we do not have to infer a cut, only diff.
//!
//! Caveats: three targets is not a shard, and this Earthfile has no `COPY`
//! from the build context, which is why `local` sources are zero. A real
//! repo will not look like that. What this run establishes is that the
//! instrument works - and that a summary percentage was one decomposition
//! away from being a wrong answer.
//!
//! # Second measurement: shape decides everything
//!
//! The first Earthfile was a CHAIN (`test` -> `build` -> `deps`), which is
//! the worst case and was mistaken for a general result. A fan-out - four
//! independent targets over one shared base, with a `COPY` from context -
//! says something quite different:
//!
//! ```text
//! gateway solves : 12      distinct graphs: 12 of 12 (0 RESENDS)
//! ops per solve  : min 4 median 5 max 17
//! overlap/solve  : [(0,4), (1,5), (1,5), (1,5), (1,5), (1,17), ...]
//! sources        : 12 registry, 12 local
//! repeated ops   : 13 (13%)
//! ```
//!
//! **Each independent target arrives as its OWN gateway Solve, sharing
//! exactly one op with everything before it** - the common base. Overlap
//! falls from 39% on the chain to 13% here, and no graph is re-sent.
//!
//! So overlap is a property of the BUILD SHAPE, not of the client. A chain
//! yields cumulative graphs and nothing worth routing; a fan-out yields
//! independent units on a plate. Real multi-target repos - the ones dispatch
//! exists for - are fan-outs.
//!
//! That is strong for routing whole Solves, and it is the third verdict this
//! measurement has produced on the same question. Worth stating plainly: the
//! first two were drawn from one toy graph, and the honest lesson is that a
//! single build shape cannot price a mechanism.
//!
//! # The context is carried by whoever proxies the session. Measured.
//!
//! Every cut in the fan-out reports a NON-free frontier, because `COPY` puts
//! a `local://` source in every graph. The question that matters is what
//! that costs, and it is not a matter of opinion:
//!
//! | build context | session bytes, client -> daemon |
//! | ------------- | ------------------------------- |
//! | 16 bytes | 1 KiB |
//! | 32 MiB | 32,812 KiB |
//!
//! The context flows over the session, so it flows through the proxy. A
//! relaying coordinator IS on the data path for context - not for layers,
//! which still go peer to peer, but for every byte of the repository a
//! builder needs. With N peers each needing it, N times.
//!
//! **This reconciles the mechanism argument, and not in the direction the
//! previous measurement suggested.** Per-Solve routing looked strong because
//! a fan-out hands over independent Solves; but every one of those Solves
//! wanted the context, so routing any of them moves the repository through
//! the coordinator. The free-frontier requirement built for subtree dispatch
//! turns out not to be a nicety - it is the ONLY shape that dispatches
//! without putting the coordinator on the data path, which is to say it is
//! what principle 6 actually requires.
//!
//! So the options are now concrete rather than architectural taste:
//!
//! - dispatch only subtrees whose frontier is registry digests (free, §6
//!   intact, but a `COPY` anywhere in the chain disqualifies it)
//! - relay the context and accept being on the data path for it
//! - give the peer its own session to the client, which the client has no
//!   reason to offer and no protocol to be asked with
//! - make the context itself content-addressed and fetchable peer to peer,
//!   which is the mesh's existing job and the only option that both
//!   dispatches `COPY`-bearing work and keeps §6
//!
//! The last one is worth the most and is not built.
//!
//! # Pointing earthly at a proxy, which is not obvious
//!
//! Earthly MANAGES buildkitd when it thinks the address is local, and
//! `containerutil.IsLocal` matches the literal strings `127.0.0.1`,
//! `localhost` and `::1`. So a loopback address spelled differently reads as
//! remote and it connects instead:
//!
//! ```yaml
//! global:
//!   buildkit_host: tcp://[0:0:0:0:0:0:0:1]:11234   # ::1, expanded
//!   tls_enabled: false
//! ```
//!
//! The upstream must be earthbuild's OWN `earthbuild/buildkitd`, not
//! `moby/buildkit`: earthly asks for an exporter named `earthly` that only
//! its fork has. Stock buildkit gets as far as the build and then says
//! `exporter "earthly" could not be found`.
//!
//! # MEASURED: a gateway Solve cannot simply be routed
//!
//! Routing gateway Solves to a second daemon gets a long way and then
//! fails on something structural:
//!
//! ```text
//! NotFound: forwarding Solve: no such job ow8s6ghu1knxv6jox461m0xmk
//! ```
//!
//! The gateway conversation is scoped to a JOB, created by `Control.Solve`
//! on one daemon. A peer never saw that call, so it has no such job and
//! cannot accept a gateway Solve under its id. The build id in the header
//! is not a name we can forward; it is a handle into one daemon's state.
//!
//! Two earlier measurements said the same thing from different angles and
//! this completes them: refs are daemon-local (eleven `read_dir` calls
//! follow eleven solves), and now jobs are too.
//!
//! **So the peer is reached through `Control.Solve`, not the gateway.** The
//! shape that works is the one the dedup line already had a name for:
//!
//! 1. the proxy solves the portable graph on a peer, via its own
//!    `Control.Solve`, exporting the result to the mirror
//! 2. the client's gateway Solve is then answered on peer 0 with a graph
//!    that merely IMPORTS that image
//! 3. peer 0 fetches content instead of building, and returns a ref that
//!    belongs to it - so `read_dir` and `return` work unchanged
//!
//! That is adoption, not forwarding, and it is what `adoptLeaderResult`
//! does for the dedup line. The routing code below is kept because
//! everything except step 2 is right: peers, ref affinity, portability
//! rewriting and placement all stand.
//!
//! # MEASURED: a sessionless peer cannot reach Docker Hub
//!
//! Adoption works - a peer builds a portable graph through its own
//! `Control.Solve` and publishes the result - but the peer then tried to
//! pull its BASE image and said:
//!
//! ```text
//! error="no active sessions" host=registry-1.docker.io
//! panic: invalid memory address or nil pointer dereference
//!   solver/llbsolver.(*resultProxy).wrapError bridge.go:288
//! ```
//!
//! Registry auth travels over the session, and a peer has none - that was
//! the whole point of rewriting `local://` away. So a graph is only truly
//! portable when EVERY source is something the peer can fetch unauthenticated,
//! and `docker-image://docker.io/...` is not that, even for a public image.
//!
//! Principle 9 says this too, and says it about exactly this: the origin
//! registry is a fallback, not a data path; fetch once into the fleet and
//! serve peer to peer. The rewrite has to cover base images as well as
//! contexts - both become references into the mirror, and then a peer needs
//! no session, no credentials and no upstream at all.
//!
//! Also worth knowing: this buildkit PANICS rather than returning the
//! error, so the failure arrives as a dead daemon rather than a failed
//! build. A fleet must treat a peer that stops answering as a decline, not
//! wait on it.
//!
//! # IT DISTRIBUTES
//!
//! A real `earthly +all`, two daemons, and one of the build's solves ran on
//! a machine the client never spoke to:
//!
//! ```text
//! [proxy] base docker.io/library/alpine:3.20@sha256:... mirrored as .../rebuck2/base:9ae21ebe
//! [proxy] adopted from peer 1: docker-image://.../rebuck2/adopted:c3eb00fb
//! [wire]  solves routed  : 1 to other daemons
//! peer 2 cache          : Total 13.73MB
//! ```
//!
//! The chain, all of it measured into existence rather than designed up
//! front: the graph arrives at the GATEWAY; its context is published as
//! content by the daemon that holds the session; its base images are
//! copied into the mirror by the daemon that holds the credentials; the
//! rewritten graph names nothing but content; a peer builds it through its
//! own `Control.Solve` and publishes the result; and the client's solve is
//! answered here with an import, so the ref it gets back belongs to the
//! daemon holding its job.
//!
//! Solves whose sources were not all mirrored yet were NOT sent - they
//! failed the portability check and built locally. That is the system
//! working: an unportable graph is not a dispatch failure, it is a graph
//! that stays home.
//!
//! # The bind a peer is currently in
//!
//! Twelve solves are offered and one completes. The other eleven fail on
//! the PEER, and which way they fail depends on which daemon it is:
//!
//! | peer | failure |
//! | ----------------------- | ------------------------------------ |
//! | `earthbuild/buildkitd` | `no active sessions` - it wants a session to export |
//! | `moby/buildkit` | `unknown API capability exec.mount.sock` |
//!
//! So a peer cannot be stock buildkit, because earthly's LLB uses a
//! capability only its fork declares; and the fork will not export without
//! a session, which is exactly what a peer does not have.
//!
//! The way out is to give the peer a session of OUR making - not the
//! client's. Buildkit does not need the client's credentials here, it needs
//! SOMEBODY to ask; an empty auth service satisfies it. That means serving
//! a session to the peer, which is the tunnelled gRPC server this proxy has
//! so far avoided implementing.
//!
//! The one solve that does complete is the one whose export finds
//! everything already local, so no auth is resolved. That is a warm-cache
//! success, and it is why the number was 1 rather than 0 - not evidence
//! that placement works better than it does.
//!
//! # Where this actually got to
//!
//! A real `earthly +all` on a two-daemon fleet, and the whole chain fired:
//! the context was published as content, the graph was made portable, the
//! work was OFFERED to a peer through its own `Control.Solve`, and when the
//! peer died the build fell back and succeeded.
//!
//! ```text
//! [proxy] peer 1 could not take it: peer solve: Unknown error transport error
//! =========================== Earth Build  SUCCESS ===========================
//! ```
//!
//! That failure is principle 5 doing its job under a real crash rather than
//! a simulated one: duplicate work is always correct, so a peer that dies
//! mid-offer costs a retry on the machine that was going to build it
//! anyway. The client saw a normal build.
//!
//! What still does not work is the PEER, not the dispatch. It panics
//! reaching for Docker Hub because a sessionless daemon has no registry
//! auth, and declaring our mirror as a `mirrors` entry for `docker.io` in
//! its config did not divert it. Until a peer can obtain base images with
//! no credentials, adoption offers work that no peer can complete - so the
//! fleet is correct, and idle.
//!
//! The remaining question is therefore plumbing rather than design: make
//! the mirror answer for `docker.io` in a way buildkit honours. Everything
//! above it - portability, publication, placement, adoption, ref affinity,
//! fail-open - is built and has run.
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

/// Where a peer can pull content from, and which daemon to ask for it.
#[derive(Clone)]
pub struct Mirror {
    /// Address a PEER would use, e.g. `host.docker.internal:15000`.
    pub registry: String,
    /// The upstream daemon, which holds the client's session.
    pub buildkit: String,
}

/// One upstream daemon.
#[derive(Clone)]
pub struct Peer {
    pub addr: String,
    channel: Chan,
}

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
    pub wire: std::sync::Arc<std::sync::Mutex<Wire>>,
    /// Set to publish build contexts as content. Absent = observe only.
    pub mirror: Option<Mirror>,
    /// buildID -> session id.
    ///
    /// The two facts arrive on different calls and neither carries both.
    /// `Control.Solve` has the session in its BODY; the gateway solves that
    /// follow carry only `buildkit-controlapi-buildid` in their headers. So
    /// the session has to be remembered from the first and looked up by the
    /// second - which is also how buildkit itself associates them.
    sessions: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// (session, local name) -> published ref. A context is published ONCE
    /// per build, not once per gateway solve: it is content-addressed so a
    /// repeat is correct, but it is a full filesync and an image push for
    /// an answer we already have.
    /// One cell per thing-we-publish, so concurrent solves SHARE the work
    /// instead of racing or skipping it.
    ///
    /// This started as a plain map and was wrong twice, in opposite
    /// directions. First it was check-then-act, so eleven of twelve
    /// concurrent solves each redid the whole filesync and push. Then
    /// in-flight entries were SKIPPED, which made it publish once - and
    /// left every solve that skipped holding a graph that was still
    /// unportable, so it stayed home. One routed solve out of twelve.
    ///
    /// A `OnceCell` per key is the shape that is neither: the first caller
    /// publishes, the rest AWAIT the same result and then have it.
    published: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                (String, String),
                std::sync::Arc<tokio::sync::OnceCell<Option<String>>>,
            >,
        >,
    >,
    /// Extra daemons this proxy may route work to. Peer 0 is always the
    /// upstream above - the one holding the client's session.
    peers: std::sync::Arc<Vec<Peer>>,
    /// ref -> peer index.
    ///
    /// A gateway result is a REF and a ref is daemon-local. Measured on a
    /// real build: eleven `read_dir` calls follow eleven solves. So whatever
    /// minted a ref must serve every later call naming it, or the eleventh
    /// call of a working-looking build fails with "ref not found".
    ref_home: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    /// Round-robin cursor for placing new solves.
    next_peer: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Proxy {
    pub async fn connect(upstream: String) -> anyhow::Result<Self> {
        let channel = tonic::transport::Endpoint::new(upstream)?.connect().await?;
        Ok(Proxy {
            client: control::control_client::ControlClient::new(channel.clone()),
            channel,
            wire: Default::default(),
            mirror: None,
            sessions: Default::default(),
            published: Default::default(),
            peers: Default::default(),
            next_peer: Default::default(),
            ref_home: Default::default(),
        })
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    /// The gateway rides the same channel, because the client's does.
    fn gw(&self) -> GwClient {
        gw::llb_bridge_client::LlbBridgeClient::new(self.channel.clone())
    }

    /// Add daemons to route to. Peer 0 is always this proxy's upstream.
    pub async fn with_peers(mut self, addrs: &[String]) -> anyhow::Result<Self> {
        let mut peers = vec![Peer {
            addr: "upstream".into(),
            channel: self.channel.clone(),
        }];
        for a in addrs {
            peers.push(Peer {
                addr: a.clone(),
                channel: tonic::transport::Endpoint::new(a.clone())?
                    .connect()
                    .await?,
            });
        }
        println!("[proxy] {} daemon(s) in the fleet", peers.len());
        self.peers = std::sync::Arc::new(peers);
        Ok(self)
    }

    fn gw_of(&self, i: usize) -> GwClient {
        match self.peers.get(i) {
            Some(p) => gw::llb_bridge_client::LlbBridgeClient::new(p.channel.clone()),
            None => self.gw(),
        }
    }

    /// Where a ref lives. Unknown refs go to peer 0, which is where every
    /// call went before there was a fleet.
    fn home_of(&self, r: &str) -> usize {
        self.ref_home
            .lock()
            .expect("ref_home")
            .get(r)
            .copied()
            .unwrap_or(0)
    }

    /// Remember which daemon minted the refs in a result.
    fn remember(&self, result: &Option<gw::Result>, peer: usize) {
        let Some(inner) = result.as_ref().and_then(|r| r.result.as_ref()) else {
            return;
        };
        let mut map = self.ref_home.lock().expect("ref_home");
        match inner {
            gw::result::Result::Ref(r) => {
                map.insert(r.id.clone(), peer);
            }
            gw::result::Result::Refs(m) => {
                for r in m.refs.values() {
                    map.insert(r.id.clone(), peer);
                }
            }
            _ => {}
        }
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
        let (meta, ext, req) = request.into_parts();
        report(&req);

        // Remember the session against this build, for the gateway solves
        // that follow. Only this call knows both.
        // The build id is the `ref` FIELD here, and arrives as the
        // `buildkit-controlapi-buildid` HEADER on the gateway solves that
        // follow. Same value, different place - looking for the header on
        // this call finds nothing.
        if !req.r#ref.is_empty() && !req.session.is_empty() {
            self.sessions
                .lock()
                .expect("sessions")
                .insert(req.r#ref.clone(), req.session.clone());
        }
        self.client()
            .solve(Request::from_parts(meta, ext, req))
            .await
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
        use std::sync::atomic::Ordering;
        let (meta, ext, stream) = request.into_parts();
        let up = self.wire.clone();
        let inbound = stream.filter_map(move |m| {
            if let Ok(msg) = &m {
                up.lock()
                    .expect("wire")
                    .session_to_daemon
                    .fetch_add(msg.data.len() as u64, Ordering::Relaxed);
            }
            futures::future::ready(m.ok())
        });
        let s = self
            .client()
            .session(Request::from_parts(meta, ext, inbound))
            .await?;
        let down = self.wire.clone();
        let out = s.into_inner().map(move |m| {
            if let Ok(msg) = &m {
                down.lock()
                    .expect("wire")
                    .session_to_client
                    .fetch_add(msg.data.len() as u64, Ordering::Relaxed);
            }
            m
        });
        Ok(Response::new(Box::pin(out)))
    }
}

/// Every gateway call, in order.
///
/// Before routing solves to different daemons, the question that decides
/// whether that is even possible: a gateway result is a REF, and a ref is
/// daemon-local. If the client only ever asks "did it work", refs never
/// leave the daemon that made them and routing is free. If it reads files
/// from them, or hands one solve's ref to another, then a ref that lives on
/// the wrong machine is a broken build.
fn trace(wire: &std::sync::Mutex<Wire>, call: &str) {
    wire.lock().expect("wire").calls.push(call.to_owned());
}

/// One gateway Solve, characterised.
///
/// Deliberately NOT "how many cuts could mechanism A take" - that prices one
/// design and asking it first is how you measure the wrong thing
/// convincingly. This describes the WORKLOAD, which prices every candidate
/// at once: how many Solves a build makes and how big each is (does routing
/// whole Solves have enough to route?), how much is `local://` (that part is
/// going nowhere whatever we build), and what platforms appear (is there
/// native multi-arch work here at all?).
#[derive(Default)]
pub struct Wire {
    pub solves: u64,
    pub ops: u64,
    pub registry_sources: u64,
    pub local_sources: u64,
    pub other_sources: u64,
    pub platforms: std::collections::BTreeSet<String>,
    /// Ops per Solve, in arrival order - the balance question.
    pub per_solve: Vec<usize>,
    /// Graph digests seen, to measure how much two Solves share.
    seen_ops: std::collections::BTreeSet<String>,
    pub repeated_ops: u64,
    /// Digest of each Solve's whole op set, in order.
    ///
    /// Repetition means two very different things and the summary number
    /// cannot tell them apart: a client RE-SENDING an identical graph is an
    /// artefact of how it drives the API, while two graphs that genuinely
    /// share a prefix is shared work a fleet could exploit. Counting
    /// identical resends separately is the difference between a finding and
    /// a misreading.
    graph_ids: Vec<String>,
    /// Per solve: how many of its ops were already seen.
    pub overlap_per_solve: Vec<(usize, usize)>,
    /// Bytes relayed over the SESSION, both ways.
    ///
    /// This is the number that decides whether a coordinator can honour
    /// principle 6. Layers travel peer to peer, but the build CONTEXT has
    /// exactly one holder - the client - and it reaches a builder over the
    /// session we are proxying. If that is kilobytes it is a rounding error;
    /// if it is the repository it is the coordinator back on the data path.
    pub session_to_daemon: std::sync::atomic::AtomicU64,
    pub session_to_client: std::sync::atomic::AtomicU64,
    /// Build contexts turned into content a peer can pull.
    pub contexts_published: u64,
    /// Gateway calls in arrival order.
    pub calls: Vec<String>,
    /// Solves placed on a peer other than the upstream.
    pub routed: u64,
    /// Why a solve was NOT placed, counted.
    ///
    /// One routed solve out of twelve is either a fleet barely working or a
    /// fleet barely used, and the difference is not visible from the
    /// outside. Counting the reason is what turns "improve routing" into a
    /// specific thing to fix.
    pub rejected: std::collections::BTreeMap<String, u64>,
}

impl Wire {
    fn observe(&mut self, def: &bollard_buildkit_proto::pb::Definition) {
        use prost::Message;
        self.solves += 1;
        self.ops += def.def.len() as u64;
        self.per_solve.push(def.def.len());
        let mut ids: Vec<String> = def
            .def
            .iter()
            .map(|b| crate::store::sha256_hex(b))
            .collect();
        ids.sort();
        self.graph_ids
            .push(crate::store::sha256_hex(ids.join("").as_bytes()));
        let mut already = 0usize;
        for bytes in &def.def {
            let digest = crate::store::sha256_hex(bytes);
            if !self.seen_ops.insert(digest) {
                // The same op in two Solves. High overlap means routing
                // whole Solves duplicates work that dispatch would share.
                self.repeated_ops += 1;
                already += 1;
            }
            let Ok(op) = bollard_buildkit_proto::pb::Op::decode(bytes.as_slice()) else {
                continue;
            };
            if let Some(p) = &op.platform {
                self.platforms
                    .insert(format!("{}/{}", p.os, p.architecture));
            }
            if let Some(bollard_buildkit_proto::pb::op::Op::Source(src)) = &op.op {
                match src.identifier.split_once("://").map(|(s, _)| s) {
                    Some("docker-image") => self.registry_sources += 1,
                    Some("local") => self.local_sources += 1,
                    _ => self.other_sources += 1,
                }
            }
        }
        self.overlap_per_solve.push((already, def.def.len()));
    }

    /// The characterisation, as one block. Printed on shutdown because the
    /// interesting numbers are about the BUILD, not any one Solve.
    pub fn report(&self) {
        let mut sizes = self.per_solve.clone();
        sizes.sort_unstable();
        let median = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
        println!("[wire] ---- what this build looked like ----");
        println!("[wire] gateway solves : {}", self.solves);
        println!("[wire] ops total      : {}", self.ops);
        println!(
            "[wire] ops per solve  : min {} median {} max {}",
            sizes.first().copied().unwrap_or(0),
            median,
            sizes.last().copied().unwrap_or(0),
        );
        println!(
            "[wire] sources        : {} registry, {} local, {} other",
            self.registry_sources, self.local_sources, self.other_sources
        );
        println!("[wire] platforms      : {:?}", self.platforms);
        println!(
            "[wire] repeated ops   : {} ({}% of all ops seen again in a later solve)",
            self.repeated_ops,
            if self.ops > 0 {
                self.repeated_ops * 100 / self.ops
            } else {
                0
            }
        );

        // The distinction that decides what the repetition MEANS. A client
        // RE-SENDING an identical graph is an artefact of how it drives the
        // API; two graphs sharing a prefix is work a fleet could share. The
        // summary percentage cannot tell them apart, and reading one as the
        // other is how a measurement becomes a wrong conclusion.
        let mut uniq = self.graph_ids.clone();
        uniq.sort();
        uniq.dedup();
        let resends = self.graph_ids.len().saturating_sub(uniq.len());
        println!(
            "[wire] distinct graphs: {} of {} solves ({resends} identical RESENDS)",
            uniq.len(),
            self.graph_ids.len(),
        );
        println!(
            "[wire] overlap/solve  : {:?} (already-seen / total)",
            self.overlap_per_solve
        );
        let up = self
            .session_to_daemon
            .load(std::sync::atomic::Ordering::Relaxed);
        let down = self
            .session_to_client
            .load(std::sync::atomic::Ordering::Relaxed);
        println!("[wire] contexts published: {}", self.contexts_published);
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        for c in &self.calls {
            *counts.entry(c.as_str()).or_default() += 1;
        }
        println!("[wire] gateway calls  : {counts:?}");
        println!("[wire] solves routed  : {} to other daemons", self.routed);
        println!("[wire] not routed     : {:?}", self.rejected);
        println!("[wire] call order     : {}", self.calls.join(" "));
        println!(
            "[wire] session bytes  : {} KiB client->daemon, {} KiB daemon->client",
            up / 1024,
            down / 1024
        );
        if resends > 0 {
            println!(
                "[wire] NOTE: {resends} solve(s) re-sent a graph already seen - that is the \
                 client driving the API, not shared work."
            );
        }
    }
}

impl Proxy {
    /// The cell for a key, created if absent.
    fn cell(
        &self,
        key: &(String, String),
    ) -> std::sync::Arc<tokio::sync::OnceCell<Option<String>>> {
        self.published
            .lock()
            .expect("published")
            .entry(key.clone())
            .or_default()
            .clone()
    }

    /// What a key resolved to, if it has resolved and succeeded.
    fn resolved(&self, key: &(String, String)) -> Option<String> {
        self.published
            .lock()
            .expect("published")
            .get(key)
            .and_then(|c| c.get().cloned().flatten())
    }

    /// The session behind this gateway call, if we learned one.
    fn session_for(&self, meta: &tonic::metadata::MetadataMap) -> String {
        meta.get("buildkit-controlapi-buildid")
            .and_then(|v| v.to_str().ok())
            .and_then(|b| self.sessions.lock().expect("sessions").get(b).cloned())
            .unwrap_or_default()
    }

    /// Swap `local://` sources for the contexts we published, so the graph
    /// depends on content rather than on one machine's disk.
    async fn make_portable(
        &self,
        def: &bollard_buildkit_proto::pb::Definition,
        session: &str,
        mirror: &Mirror,
    ) -> bollard_buildkit_proto::pb::Definition {
        let out = crate::dispatch::rewrite_local_sources(def, &|name| {
            // Not published: leave it alone. The graph stays pinned to peer
            // 0, which is correct, rather than pointing a peer at content
            // nobody has.
            self.resolved(&(session.to_owned(), name.to_owned()))
        });

        // And the BASE images. A sessionless peer has no registry auth, so
        // `docker-image://docker.io/...` is as unreachable for it as the
        // client's disk. Copy each through peer 0, which does have
        // credentials, and point the graph at the copy.
        let mut refs: std::collections::BTreeSet<String> = Default::default();
        for bytes in &out.def {
            use prost::Message;
            if let Ok(op) = bollard_buildkit_proto::pb::Op::decode(bytes.as_slice()) {
                if let Some(bollard_buildkit_proto::pb::op::Op::Source(src)) = &op.op {
                    if let Some(r) = src.identifier.strip_prefix("docker-image://") {
                        // Already ours: copying it again would be a loop.
                        if !r.starts_with(&mirror.registry) {
                            refs.insert(r.to_owned());
                        }
                    }
                }
            }
        }
        for r in refs {
            let key = ("base".to_owned(), r.clone());
            let cell = self.cell(&key);
            let full = format!("docker-image://{r}");
            cell.get_or_init(|| async {
                match crate::solve::mirror_image(&mirror.buildkit, &mirror.registry, session, &full)
                    .await
                {
                    Ok(reference) => {
                        println!("[proxy] base {r} mirrored as {reference}");
                        Some(reference)
                    }
                    Err(e) => {
                        println!("[proxy] base {r} not mirrored: {e:#}");
                        None
                    }
                }
            })
            .await;
        }

        // The FULL reference, scheme included: the rewrite replaces the
        // identifier wholesale, and buildkit rejects a bare
        // `host:port/name:tag` with "invalid".
        crate::dispatch::rewrite_registry_sources(&out, &|r| {
            self.resolved(&("base".to_owned(), r.to_owned()))
        })
    }

    /// Materialise every `local://` source this graph names.
    ///
    /// Best effort by construction: a context we fail to publish leaves that
    /// subtree undispatchable, which is where it already was. It must never
    /// fail the build - the client asked for a build, not for dispatch.
    async fn publish_contexts(
        &self,
        mirror: &Mirror,
        def: &bollard_buildkit_proto::pb::Definition,
        meta: &tonic::metadata::MetadataMap,
    ) {
        use prost::Message;
        // The session id rides the request headers; without it the daemon
        // has no filesync to resolve `local://` through.
        let session = meta
            .get("buildkit-controlapi-buildid")
            .and_then(|v| v.to_str().ok())
            .and_then(|b| self.sessions.lock().expect("sessions").get(b).cloned());
        let Some(session) = session else {
            println!("[proxy] gateway solve with no known session - not publishing");
            return;
        };
        let mut names: std::collections::BTreeSet<String> = Default::default();
        for bytes in &def.def {
            if let Ok(op) = bollard_buildkit_proto::pb::Op::decode(bytes.as_slice()) {
                if let Some(bollard_buildkit_proto::pb::op::Op::Source(src)) = &op.op {
                    if let Some(n) = src.identifier.strip_prefix("local://") {
                        names.insert(n.to_owned());
                    }
                }
            }
        }
        for name in names {
            let key = (session.clone(), name.clone());
            let cell = self.cell(&key);
            cell.get_or_init(|| async {
                match crate::solve::publish_context(
                    &mirror.buildkit,
                    &mirror.registry,
                    &session,
                    &name,
                )
                .await
                {
                    Ok(reference) => {
                        println!("[proxy] context {name:?} published as {reference}");
                        self.wire.lock().expect("wire").contexts_published += 1;
                        Some(reference)
                    }
                    // Remembered for this build rather than retried per
                    // solve: eleven solves each re-attempting a publish
                    // that cannot work is eleven times the wait for the
                    // same answer.
                    Err(e) => {
                        println!("[proxy] context {name:?} not published: {e:#}");
                        None
                    }
                }
            })
            .await;
        }
    }
}

/// What the gateway's Solve offers a dispatcher. THIS is the graph.
fn report_gateway(wire: &std::sync::Mutex<Wire>, req: &gw::SolveRequest) {
    let Some(def) = &req.definition else {
        return;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    let mut w = wire.lock().expect("wire");
    w.observe(def);
    println!(
        "[proxy] gateway solve #{}: {} ops, {} cuts >= {MIN_CUT_OPS}, {} free-frontier",
        w.solves,
        a.ops,
        a.cuts.len(),
        free.len(),
    );
}

/// Serve the Control service on `addr`, forwarding to `upstream`.
pub async fn serve(
    addr: std::net::SocketAddr,
    upstream: String,
    peers: Vec<String>,
) -> anyhow::Result<()> {
    println!("[proxy] buildkit control on {addr} -> {upstream}");
    let mut proxy = Proxy::connect(upstream.clone())
        .await?
        .with_peers(&peers)
        .await?;
    proxy.mirror = std::env::var("REBUCK2_MIRROR").ok().map(|registry| Mirror {
        registry,
        buildkit: upstream,
    });
    // The characterisation is about the BUILD, so it prints when we are
    // asked to stop rather than per Solve.
    let wire = proxy.wire.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        wire.lock().expect("wire").report();
        std::process::exit(0);
    });
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
        trace(&self.wire, "solve");
        report_gateway(&self.wire, &req);
        // Publish any build context this graph needs, so the subtree stops
        // being pinned to the one machine holding the client's disk. The
        // build itself is untouched and still goes upstream: publishing is
        // preparation for dispatch, not dispatch.
        if let (Some(mirror), Some(def)) = (&self.mirror, &req.definition) {
            self.publish_contexts(mirror, def, &meta).await;
        }

        // Place the work by ADOPTION, not forwarding. A peer cannot accept
        // this gateway solve - jobs are daemon-local - so the peer builds
        // the portable graph through its own Control.Solve and publishes
        // the result, and the client's solve is then answered here with a
        // graph that merely imports it. Peer 0 fetches content instead of
        // building, and the ref it returns is its own.
        let mut req = req;
        {
            let mut w = self.wire.lock().expect("wire");
            let key = match (
                self.peers.len() > 1,
                self.mirror.is_some(),
                req.definition.is_some(),
            ) {
                (false, _, _) => "no fleet",
                (_, false, _) => "no mirror",
                (_, _, false) => "no definition",
                _ => "considered",
            };
            *w.rejected.entry(key.to_owned()).or_default() += 1;
        }
        if self.peers.len() > 1 {
            if let (Some(mirror), Some(def)) = (&self.mirror, req.definition.clone()) {
                let session = self.session_for(&meta);
                let portable = self.make_portable(&def, &session, mirror).await;
                // Portable means EVERY source is something a sessionless
                // peer can fetch: nothing local, and every image already in
                // our mirror. Checking only `local == 0` let a graph whose
                // base was still being mirrored go out anyway - concurrent
                // solves skip an in-flight copy - and the peer then reached
                // for Docker Hub with no credentials and panicked.
                let local_clear = crate::dispatch::analyse(&portable, 1)
                    .cuts
                    .first()
                    .is_none_or(|c| c.frontier.local == 0);
                let bases_clear = portable.def.iter().all(|b| {
                    use prost::Message;
                    match bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                        .ok()
                        .and_then(|o| o.op)
                    {
                        Some(bollard_buildkit_proto::pb::op::Op::Source(src)) => src
                            .identifier
                            .strip_prefix("docker-image://")
                            .is_none_or(|r| r.starts_with(&mirror.registry)),
                        _ => true,
                    }
                });
                let movable = local_clear && bases_clear;
                if !movable {
                    let why = match (local_clear, bases_clear) {
                        (false, false) => "context and base unmirrored",
                        (false, true) => "context unmirrored",
                        (true, false) => "base unmirrored",
                        (true, true) => unreachable!(),
                    };
                    *self
                        .wire
                        .lock()
                        .expect("wire")
                        .rejected
                        .entry(why.to_owned())
                        .or_default() += 1;
                }
                if movable {
                    let peer = 1 + self
                        .next_peer
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        % (self.peers.len() - 1);
                    let addr = self.peers[peer].addr.clone();
                    match crate::solve::build_and_publish(&addr, &mirror.registry, portable).await {
                        Ok(reference) => {
                            println!("[proxy] adopted from peer {peer}: {reference}");
                            self.wire.lock().expect("wire").routed += 1;
                            req.definition = Some(crate::dispatch::import_graph(&reference));
                        }
                        // Fail open: build it here, exactly as we would
                        // have without a fleet.
                        Err(e) => {
                            println!("[proxy] peer {peer} could not take it: {e:#}");
                            // Count the FAILURES too. `routed` counts only
                            // successes, so a fleet attempting twelve
                            // adoptions and completing one looked identical
                            // to a fleet attempting one - which sent me
                            // hunting a placement bug that did not exist.
                            *self
                                .wire
                                .lock()
                                .expect("wire")
                                .rejected
                                .entry("peer refused".to_owned())
                                .or_default() += 1;
                        }
                    }
                }
            }
        }

        // Always peer 0: it holds the client's job, and after adoption the
        // graph is a fetch rather than a build.
        let out = self.gw().solve(Request::from_parts(meta, ext, req)).await?;
        let out = out.into_inner();
        self.remember(&out.result, 0);
        Ok(Response::new(out))
    }

    async fn resolve_image_config(
        &self,
        request: Request<gw::ResolveImageConfigRequest>,
    ) -> Result<Response<gw::ResolveImageConfigResponse>, Status> {
        trace(&self.wire, "resolve_image_config");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .resolve_image_config(Request::from_parts(meta, ext, req))
            .await
    }
    async fn resolve_source_meta(
        &self,
        request: Request<gw::ResolveSourceMetaRequest>,
    ) -> Result<Response<gw::ResolveSourceMetaResponse>, Status> {
        trace(&self.wire, "resolve_source_meta");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .resolve_source_meta(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_file(
        &self,
        request: Request<gw::ReadFileRequest>,
    ) -> Result<Response<gw::ReadFileResponse>, Status> {
        trace(&self.wire, "read_file");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_file(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_dir(
        &self,
        request: Request<gw::ReadDirRequest>,
    ) -> Result<Response<gw::ReadDirResponse>, Status> {
        trace(&self.wire, "read_dir");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_dir(Request::from_parts(meta, ext, req))
            .await
    }
    async fn stat_file(
        &self,
        request: Request<gw::StatFileRequest>,
    ) -> Result<Response<gw::StatFileResponse>, Status> {
        trace(&self.wire, "stat_file");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .stat_file(Request::from_parts(meta, ext, req))
            .await
    }
    async fn evaluate(
        &self,
        request: Request<gw::EvaluateRequest>,
    ) -> Result<Response<gw::EvaluateResponse>, Status> {
        trace(&self.wire, "evaluate");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .evaluate(Request::from_parts(meta, ext, req))
            .await
    }
    async fn ping(
        &self,
        request: Request<gw::PingRequest>,
    ) -> Result<Response<gw::PongResponse>, Status> {
        trace(&self.wire, "ping");
        let (meta, ext, req) = request.into_parts();
        self.gw().ping(Request::from_parts(meta, ext, req)).await
    }
    async fn inputs(
        &self,
        request: Request<gw::InputsRequest>,
    ) -> Result<Response<gw::InputsResponse>, Status> {
        trace(&self.wire, "inputs");
        let (meta, ext, req) = request.into_parts();
        self.gw().inputs(Request::from_parts(meta, ext, req)).await
    }
    async fn new_container(
        &self,
        request: Request<gw::NewContainerRequest>,
    ) -> Result<Response<gw::NewContainerResponse>, Status> {
        trace(&self.wire, "new_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .new_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn release_container(
        &self,
        request: Request<gw::ReleaseContainerRequest>,
    ) -> Result<Response<gw::ReleaseContainerResponse>, Status> {
        trace(&self.wire, "release_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .release_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_file_container(
        &self,
        request: Request<gw::ReadFileRequest>,
    ) -> Result<Response<gw::ReadFileResponse>, Status> {
        trace(&self.wire, "read_file_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_file_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn read_dir_container(
        &self,
        request: Request<gw::ReadDirRequest>,
    ) -> Result<Response<gw::ReadDirResponse>, Status> {
        trace(&self.wire, "read_dir_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .read_dir_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn stat_file_container(
        &self,
        request: Request<gw::StatFileRequest>,
    ) -> Result<Response<gw::StatFileResponse>, Status> {
        trace(&self.wire, "stat_file_container");
        let (meta, ext, req) = request.into_parts();
        self.gw()
            .stat_file_container(Request::from_parts(meta, ext, req))
            .await
    }
    async fn warn(
        &self,
        request: Request<gw::WarnRequest>,
    ) -> Result<Response<gw::WarnResponse>, Status> {
        trace(&self.wire, "warn");
        let (meta, ext, req) = request.into_parts();
        self.gw().warn(Request::from_parts(meta, ext, req)).await
    }

    /// `return` is a keyword here and a method name there.
    async fn r#return(
        &self,
        request: Request<gw::ReturnRequest>,
    ) -> Result<Response<gw::ReturnResponse>, Status> {
        trace(&self.wire, "return");
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
