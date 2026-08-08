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
    /// `None` = publication IN FLIGHT, `Some` = done.
    ///
    /// The in-flight state is load-bearing. Gateway solves run
    /// CONCURRENTLY, so a plain "is it there yet" check is check-then-act:
    /// eleven of twelve solves read an empty map before the first insert
    /// landed and each did the whole filesync and push again. Claiming the
    /// key under the same lock is what makes it once.
    published: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<(String, String), Option<String>>>,
    >,
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
            {
                let mut map = self.published.lock().expect("published");
                match map.get(&key) {
                    Some(Some(had)) => {
                        println!("[proxy] context {name:?} already published as {had}");
                        continue;
                    }
                    Some(None) => continue, // someone else is doing it
                    None => {
                        map.insert(key.clone(), None);
                    }
                }
            }
            match crate::solve::publish_context(&mirror.buildkit, &mirror.registry, &session, &name)
                .await
            {
                Ok(reference) => {
                    println!("[proxy] context {name:?} published as {reference}");
                    self.wire.lock().expect("wire").contexts_published += 1;
                    self.published
                        .lock()
                        .expect("published")
                        .insert(key, Some(reference));
                }
                Err(e) => {
                    // Release the claim: a failure must not make the
                    // context permanently unpublishable for this build.
                    self.published.lock().expect("published").remove(&key);
                    println!("[proxy] context {name:?} not published: {e:#}");
                }
            }
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
pub async fn serve(addr: std::net::SocketAddr, upstream: String) -> anyhow::Result<()> {
    println!("[proxy] buildkit control on {addr} -> {upstream}");
    let mut proxy = Proxy::connect(upstream.clone()).await?;
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
        self.gw().solve(Request::from_parts(meta, ext, req)).await
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
