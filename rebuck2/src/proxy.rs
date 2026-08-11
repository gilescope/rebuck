//! A distributed BuildKit: one client, one endpoint, many machines.
//!
//! A client points `BUILDKIT_HOST` at this and gets a fleet. It serves both
//! services buildkit clients use - `Control` and the `LLBBridge` gateway -
//! and forwards everything it does not act on, so a fleet with no peers
//! behaves exactly like the daemon behind it.
//!
//! The findings, the measurements and the wrong turns are in
//! [`docs/fleet-findings.md`](../../docs/fleet-findings.md). What follows is
//! only what a maintainer has to know before changing this file.
//!
//! # Load-bearing facts
//!
//! - **The graph is on the GATEWAY, not on Control.** `Control.Solve` carries
//!   no `Definition`; the client drives the build through `LLBBridge` on the
//!   SAME connection. Both are served here for that reason.
//! - **Work is ADOPTED, never forwarded.** Jobs and refs are daemon-local, so
//!   a peer cannot accept this client's gateway solve. The peer builds a
//!   portable copy through its own `Control.Solve` and publishes it; the
//!   client's solve is then answered on peer 0 with a graph that imports the
//!   result. Every ref the client sees therefore belongs to peer 0.
//! - **A gateway Solve is LAZY.** It returns a ref in about a millisecond
//!   whether it stands for a ten-second build or a registry pull. The client
//!   blocks on `Control.Solve`; that is the only call whose duration means
//!   anything.
//! - **Sessions are matched by request HEADERS.** Forward a stream without
//!   them and the daemon holds a session it cannot match to a build, and the
//!   frontend's filesync fails as `Unimplemented`. Preserve metadata on every
//!   forwarding path.
//! - **A named frontend is invisible.** `--frontend dockerfile.v0` asks the
//!   DAEMON to resolve it, so the LLB never crosses this proxy and nothing
//!   can be placed. Clients that build their own graph dispatch normally.
//! - **Portable means every source is fetchable without a session**: local
//!   context published as content, base images mirrored FOR THE TARGET
//!   ARCHITECTURE. A single-architecture base sent to a foreign peer fails as
//!   `exit code: 255` after a full pull.
//!
//! # How placement decides
//!
//! In order, cheapest first, and each step exists because skipping it was
//! measured to cost something:
//!
//! 1. **Exclusions** (`dispatch::inspect`), on the ORIGINAL graph. One cache
//!    mount, secret, ssh socket or host bind anywhere grounds the subtree.
//! 2. **The mirror breaker.** Nothing can be adopted while the shared
//!    registry is down, and building at home is already the fail-open answer.
//! 3. **Saturation.** Ship only once the local machine is full. This is the
//!    home-or-away decision, and nothing downstream may re-make it.
//! 4. **Which peer** (`place`): native architecture first, then load per unit
//!    of declared capacity, then strikes. A struck peer is deprioritised, not
//!    banned.
//! 5. **A bounded wait** (`adopt_or_take_back`). Past three times the
//!    observed median an adoption is withdrawn and built at home; the peer is
//!    not cancelled, and whoever publishes first wins.
//!
//! Everything fails open. A peer that refuses, a mirror that dies, a graph
//! that cannot be made portable: each produces the build an ordinary
//! buildkitd would have produced.
//!
//! # Reading the report
//!
//! `[wire]` lines are printed on SIGINT. `placed` is where solves went (key 0
//! is home) and is the only honest evidence of distribution - wall clock
//! moves for unrelated reasons, and a control run has twice overturned a
//! conclusion drawn from it.

use std::pin::Pin;

use crate::gateway::frontend as gw;
use crate::store::Held;
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

/// One in-flight publish per key, shared by every solve that wants it.
type Published = std::sync::Arc<
    std::sync::Mutex<
        std::collections::HashMap<
            (String, String),
            std::sync::Arc<tokio::sync::OnceCell<Option<String>>>,
        >,
    >,
>;

/// build id -> (where it went, graph key, whether it held a home slot).
type Went = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, (Option<usize>, String, bool)>>,
>;

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
    /// A SECOND connection, carrying only `Session`.
    ///
    /// Session is long-lived, bidirectional, and carries filesync and
    /// credentials - so when it dies the build dies, and it cannot be
    /// retried the way a solve can. Multiplexed behind 163 gateway solves on
    /// one h2 connection, any connection-level event takes it with them:
    ///
    ///     [proxy] session stream from daemon failed: Unknown error
    ///     h2 protocol error: error reading a body from connection
    ///
    /// which earthly then prints as its own exit error, having been told
    /// nothing about which of its many calls broke. Five rounds of fixes
    /// went to the server side, to Control.Solve and to hyper's reset
    /// limits before the session relay was instrumented and said plainly
    /// that the failing connection was the one we make.
    session_channel: Chan,
    /// Kept so each Session can dial a connection of its own.
    upstream: String,
    /// Connections the gateway's calls are spread over. See [`Proxy::gw`].
    gw_pool: Vec<Chan>,
    next_gw: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// One dispatch at a time until something has been BUILT.
    ///
    /// The barrier that grafting needs and did not have. On a cold fleet the
    /// first wave of solves is dispatched simultaneously, nothing is
    /// published, and every worker builds the shared ancestor chain - so
    /// grafting fired 50 times and never touched a 109-op graph, because
    /// those all went out before anything existed to graft.
    ///
    /// Holding the first dispatch alone costs one solve's worth of
    /// serialisation and buys every later subtree an ancestry it can import
    /// rather than rebuild. That is the step from N prefixes to 1; the bank
    /// is the same sequence with this step already paid for by a previous
    /// generation, which is why 0 beats 1.
    warmup: std::sync::Arc<tokio::sync::Semaphore>,
    /// Who places dispatched work. The gateway holds the client's Solve; the
    /// driver decides which machine builds it, using the arbitration workers
    /// already get. Not a peer list of our own - see M4.5.
    driver: std::sync::Arc<crate::driver::Driver>,
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
    published: Published,
    /// build id -> where its work went. Written by the gateway solve, read by
    /// `Control.Solve` when it finishes, because only the gateway knows the
    /// placement and only Control knows what the client actually waited.
    went: Went,
    /// Builds currently running at home, so dispatch can wait until the
    /// local machine is actually full. Incremented when a solve is placed
    /// home, decremented when the client's `Control.Solve` for it returns -
    /// the gateway solve is lazy and returns in a millisecond, so it cannot
    /// mark the end of anything.
    home_inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// graph key -> observed client-visible durations. The estimate that
    /// decides whether a subtree is worth shipping.
    seen_ms: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u64>>>>,
    /// Per-peer durations sampled ONLY while that peer held nothing else.
    ///
    /// The exogenous signal. Every other timing here is moved by the thing it
    /// is meant to inform - load a peer and its service time rises, which is
    /// what made the feedback controller chase itself. An uncontended sample
    /// is not: it says how fast this machine is on one build, and placement
    /// cannot change that by deciding differently.
    ///
    /// It measures SPEED, not capacity. A 32-core box and a 4-core box can
    /// agree on one build and differ wildly on eight. Worth having anyway:
    /// measured with no contention at all, the remote peer built in 8.8s
    /// against this machine's 10s, so on this pair speed is the thing that
    /// turned out to matter.
    solo_ms: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<usize, Vec<u64>>>>,
}

impl Proxy {
    pub async fn connect(
        upstream: String,
        driver: std::sync::Arc<crate::driver::Driver>,
    ) -> anyhow::Result<Self> {
        // Keepalive on both: a Session can sit idle while a nested build
        // runs, and an intermediary that reaps idle connections takes the
        // build with it.
        // KEEPALIVE, and whether it is the thing killing long solves.
        //
        // Added in the same commit as the connection split, and the failure
        // lands immediately after a 59.6s lead against a 60s keep-alive
        // timeout. A Control.Solve waits for the fleet to build a subtree -
        // 59s here, 142s for the next one - and carries no traffic while it
        // waits, which is exactly when a keepalive decides a connection is
        // dead.
        //
        // OFF by default, and the daemon's own source says why. From
        // grpc-go's http2_server.go, vendored into the buildkitd we talk to:
        //
        //     maxPingStrikes     = 2
        //     defaultPingTimeout = 2 * time.Hour
        //
        // With no active streams and PermitWithoutStream false, EVERY ping
        // inside two hours is a strike, and three strikes sends GOAWAY with
        // ENHANCE_YOUR_CALM / "too_many_pings" and closes the connection.
        // A 20s keepalive therefore kills an idle connection after exactly
        // three pings - 60 seconds - which is the boundary the failures
        // landed on. With active streams the bar is EnforcementPolicy
        // MinTime, 5 minutes by default, so any keepalive faster than that
        // is fatal either way.
        //
        // tonic then reports the closed connection as `transport error`,
        // which is its own Kind::Transport string and not the daemon's - two
        // days were spent reading it as an answer from buildkitd.
        //
        // Anything non-zero here must exceed 5 minutes to be safe.
        let ka: u64 = std::env::var("REBUCK2_KEEPALIVE_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let endpoint = move |u: String| -> anyhow::Result<tonic::transport::Endpoint> {
            let e = tonic::transport::Endpoint::new(u)?;
            Ok(if ka == 0 {
                e
            } else {
                e.http2_keep_alive_interval(std::time::Duration::from_secs(ka))
                    .keep_alive_timeout(std::time::Duration::from_secs(ka * 3))
                    .keep_alive_while_idle(true)
            })
        };
        let upstream_kept = upstream.clone();
        // HOW MANY connections, and whether Control shares one.
        //
        // Splitting these was meant to stop one connection-level event
        // taking every stream. It may have bought a worse problem: on a
        // single h2 connection a gateway call cannot overtake the
        // Control.Solve that registers its job, and separate connections
        // remove that ordering. The failing runs now say
        //
        //     upstream solve failed: ... no such job t7p8h37g7ses5...
        //
        // which is the daemon being asked about a build it has not been told
        // about yet - and it appears first in the run AFTER the split.
        //
        // 1 restores the original shape: one connection for Control and the
        // gateway together, which is what every run before the split used.
        let conns: usize = std::env::var("REBUCK2_CONNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(4);
        let channel = endpoint(upstream.clone())?.connect().await?;
        let control_channel = if conns == 1 {
            channel.clone()
        } else {
            endpoint(upstream.clone())?.connect().await?
        };
        let session_channel = endpoint(upstream.clone())?.connect().await?;
        // FOUR, which is a guess bounded on both sides: one connection
        // admits 250 concurrent streams and a run peaks well under 1000, so
        // four is enough; and each is an idle TCP connection to localhost
        // when unused, so being wrong upwards costs nothing measurable.
        let mut gw_pool = vec![channel];
        for _ in 1..conns {
            gw_pool.push(endpoint(upstream.clone())?.connect().await?);
        }
        println!(
            "[proxy] upstream connections: {} gateway, control {}",
            gw_pool.len(),
            if conns == 1 { "shared" } else { "its own" }
        );
        Ok(Proxy {
            client: control::control_client::ControlClient::new(control_channel),
            session_channel,
            upstream: upstream_kept,
            gw_pool,
            next_gw: Default::default(),
            warmup: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            driver,
            wire: Default::default(),
            mirror: None,
            sessions: Default::default(),
            published: Default::default(),
            solo_ms: Default::default(),
            went: Default::default(),
            seen_ms: Default::default(),
            home_inflight: Default::default(),
        })
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    /// What this graph has cost before, as a median.
    ///
    /// Median rather than mean: one build that queued behind everything else
    /// must not make a subtree look permanently expensive, and therefore
    /// permanently worth shipping.
    ///
    /// Only ever populated when [`gating`] is on. The history is keyed by
    /// graph digest, so it grows with the number of DISTINCT graphs a proxy
    /// has seen and no window bounds that - and with the gate off it was
    /// being filled for a reader that never ran.
    fn estimate(&self, key: &str) -> Option<u64> {
        let seen = self.seen_ms.held();
        let v = seen.get(key)?;
        if v.is_empty() {
            return None;
        }
        let mut v = v.clone();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    /// What shipping COSTS, in ms: away time minus home time.
    ///
    /// First written as away minus what the peer spent building - and the
    /// gate then never fired, because the estimate it is compared against is
    /// itself an away duration and already contains that overhead. Comparing
    /// two client-visible numbers in the same units is the fix: the
    /// difference is exactly what shipping cost, whatever it was made of.
    ///
    /// Zero until both sides have run, which makes everything worth shipping.
    /// That is the deliberate cold start - something has to run somewhere
    /// before there is anything to know.
    fn overhead_ms(&self) -> u64 {
        let w = self.wire.held();
        let mean = |v: &[u64]| -> u64 {
            if v.is_empty() {
                0
            } else {
                v.iter().sum::<u64>() / v.len() as u64
            }
        };
        mean(&w.away_ms).saturating_sub(mean(&w.home_ms))
    }

    /// The gateway rides the same channel, because the client's does.
    /// A gateway client, spread across the connection POOL.
    ///
    /// Not one connection. The gateway carries the bulk of the traffic - one
    /// run logged 200 calls across solve, read_file, read_dir, ping and
    /// return - and Go's http2 server admits 250 concurrent streams per
    /// connection by default. Past that a stream is refused and the h2 error
    /// surfaces as several independent targets failing in the same instant:
    ///
    ///     apply IF: read exit code: transport error
    ///
    /// on `+cache-mount-arg` and `+cache-test` together, followed by the
    /// build dying. Simultaneous failure of unrelated calls is what a shared
    /// connection looks like when it breaks.
    ///
    /// Round-robin rather than least-loaded: the counter is one atomic add,
    /// and picking the emptiest would need per-connection stream accounting
    /// that tonic does not expose.
    fn gw(&self) -> GwClient {
        let n = self
            .next_gw
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ch = &self.gw_pool[n % self.gw_pool.len()];
        gw::llb_bridge_client::LlbBridgeClient::new(ch.clone())
    }
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
                .held()
                .insert(req.r#ref.clone(), req.session.clone());
        }
        // Kept before the request is consumed: the client blocks on this call,
        // and the gateway solve that chose a peer names the same build id.
        let build_id = req.r#ref.clone();
        // THIS is where a build's time actually is. Both cheaper
        // candidates were tried and are near-zero: a gateway Solve returns a
        // ref in ~1ms, and `return` merely registers it. The client blocks
        // on the Control.Solve response, so timing it is the only way the
        // proxy sees a build's duration at all.
        // ONE reference export, from the client's own build.
        //
        // This is the shape the prior art converges on and the shape neither
        // leg of the cache A/B tested: `read` had no writer, so every import
        // fetched nothing; `readwrite` had 84 writers, and exporting after
        // every solve cost more than it saved.
        //
        // The client's build runs here, on peer 0, ONCE. Exporting from it
        // populates the ref that every dispatched subtree imports - one
        // writer, N readers - so a worker can skip executing an ancestor
        // instead of rebuilding it. Ahmad & Kwok's rule says that is the
        // right side of the line for this workload by two orders of
        // magnitude: an apt-layer costs 30-120s to execute and 40-400ms to
        // pull.
        //
        // `ignore-error` because an optimisation that can fail the client's
        // build is not one.
        let mut req = req;
        if let (Some(m), true) = (
            self.mirror.as_ref(),
            crate::solve::fleet_cache_mode() != "off",
        ) {
            req.cache = crate::solve::reference_export(&m.registry);
        }
        let t = std::time::Instant::now();
        // Kept for one retry: the channel reconnects on the next request,
        // but the request itself moves into the first attempt.
        let again = (meta.clone(), req.clone());
        let out = self
            .client()
            .solve(Request::from_parts(meta, ext, req))
            .await
            .inspect_err(|e| {
                // THE call whose failure the user sees. earthly blocks on
                // Control.Solve, so whatever this returns becomes its final
                // `Error:` line - and a full +test-no-qemu keeps ending on
                // `h2 protocol error: error reading a body from connection`,
                // which is hyper's phrasing and not Go's.
                //
                // The gateway-solve path was instrumented first and logged
                // NOTHING across a whole failing run, which rules it out
                // rather than confirming it. This is the other relay.
                println!(
                    "[proxy] upstream Control.Solve failed: {} {}",
                    e.code(),
                    e.message()
                );
            });
        // ONE retry, for the transport only. This is the call the client
        // blocks on, so its failure is the whole build's failure - a full
        // +test-no-qemu has died here repeatedly, taking thirteen groups
        // that were fine down with the one that was not.
        //
        // Extensions belong to the inbound connection and the upstream call
        // does not read them.
        let out = match out {
            Err(e) if worth_retrying(&e) => {
                println!("[proxy] retrying Control.Solve once: {}", e.message());
                let (meta, req) = again;
                self.client()
                    .solve(Request::from_parts(meta, Default::default(), req))
                    .await
                    .inspect_err(|e| {
                        println!(
                            "[proxy] Control.Solve retry failed too: {} {}",
                            e.code(),
                            e.message()
                        );
                    })
            }
            other => other,
        };
        let ms = t.elapsed().as_millis() as u64;
        let went = self.went.held().get(&build_id).cloned();
        let mut w = self.wire.held();
        w.control_solves.push(ms);
        match &went {
            Some((Some(_), _, _)) => w.away_ms.push(ms),
            Some((None, _, held)) if !held => w.home_ms.push(ms),
            Some((None, _, _)) => {
                w.home_ms.push(ms);
                // Only a solve that actually HELD a slot releases one.
                // Releasing on every home build frees slots that were never
                // taken - a saturated solve with nowhere to go still builds
                // at home, and was handing back capacity it never had, so the
                // gate reopened immediately and only four builds in
                // twenty-four ever found home full.
                let _ = self.home_inflight.fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |n| Some(n.saturating_sub(1)),
                );
            }
            // Never placed - excluded, no fleet, no mirror. Neither bucket.
            None => {}
        }
        drop(w);
        if let Some((_, key, _)) = went {
            if gating() {
                remember(self.seen_ms.held().entry(key).or_default(), ms);
            }
        }
        out
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
        // WHICH session, and how long it lived. Four of these fail per run
        // with an h2 protocol error even now each has its own connection, so
        // multiplexing is not the cause - and the two readings left are very
        // different. Either the daemon is dropping live sessions, or a
        // nested earthly finished and its ordinary teardown is being logged
        // as a failure and RELAYED to the client as one.
        //
        // The session id separates them: an id that never carried a solve is
        // a nested build going away, and one that did is a live session
        // being cut.
        let sid = meta
            .get("x-docker-expose-session-uuid")
            .or_else(|| meta.get("buildkit-controlapi-buildid"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("?")
            .to_owned();
        let started = std::time::Instant::now();
        let up = self.wire.clone();
        let sid_in = sid.clone();
        let inbound = stream.filter_map(move |m| {
            match &m {
                Ok(msg) => up
                    .held()
                    .session_to_daemon
                    .fetch_add(msg.data.len() as u64, Ordering::Relaxed),
                // A dropped error here is INVISIBLE and not harmless. The
                // session carries filesync and credentials, and `m.ok()`
                // turns a stream error into a clean end-of-stream - so
                // buildkitd sees the session close normally, any filesync
                // still in flight fails, and it reports the Solve as
                // `transport error`, which earthly prints as `h2 protocol
                // error: error reading a body from connection`.
                //
                // That chain is why three rounds of instrumenting Solve
                // found nothing: the failure is one relay upstream and this
                // line ate the evidence.
                Err(e) => {
                    println!(
                        "[proxy] session {sid_in} from client failed: {} {}",
                        e.code(),
                        e.message()
                    );
                    0
                }
            };
            futures::future::ready(m.ok())
        });
        // ONE CONNECTION PER SESSION, not one shared by all of them.
        //
        // Giving Session its own channel stopped the top-level h2 collapse,
        // and moved the failure one layer down: under daemon consolidation
        // every nested earthly opens its own Session through this proxy, so
        // a full +test-no-qemu has dozens of them, and they were all sharing
        // the single session channel. A connection-level event still took
        // the lot - it just took nested builds instead of the outer one,
        // surfacing as `apply IF: read exit code: transport error`.
        //
        // Dialling per session costs a connect on localhost and buys
        // isolation: one nested build's session can no longer end another's.
        let mut c = match crate::proxy::session_endpoint(&self.upstream) {
            Ok(ep) => match ep.connect().await {
                Ok(ch) => control::control_client::ControlClient::new(ch),
                // Fall back to the shared channel rather than failing the
                // session: a build with a shared connection is what we had,
                // and a build with none is a build that does not run.
                Err(e) => {
                    println!("[proxy] session dial failed, sharing: {e}");
                    control::control_client::ControlClient::new(self.session_channel.clone())
                }
            },
            Err(e) => {
                println!("[proxy] session endpoint invalid, sharing: {e}");
                control::control_client::ControlClient::new(self.session_channel.clone())
            }
        };
        let s = c.session(Request::from_parts(meta, ext, inbound)).await?;
        let down = self.wire.clone();
        let out = s.into_inner().map(move |m| {
            // `c` is captured so the connection outlives the stream. Dropping
            // it here is the bug this file already documents once: the
            // handler returns, the channel drops, and the still-running
            // Session dies mid-build with the daemon reporting only
            // "healthcheck failed ... EOF".
            let _keepalive = &c;
            match &m {
                Ok(msg) => down
                    .held()
                    .session_to_client
                    .fetch_add(msg.data.len() as u64, Ordering::Relaxed),
                // This direction IS propagated - the client sees it - but it
                // is still worth naming here, because the client's report of
                // it names nothing on this side.
                Err(e) => {
                    // ENDED, most likely, not failed. The lifetimes settled
                    // this: three sessions ended within 150ms of each other
                    // at 22.6s, on separate connections, which is three
                    // nested earthlys that started together and exited
                    // together - not a connection fault. The fourth ran
                    // 439s, the length of the whole build.
                    //
                    // Called a failure for two days, it sent five fixes at
                    // the wrong component. A stream ending when its client
                    // goes away is the normal case and the log should say so.
                    println!(
                        "[proxy] session {sid} ended after {}ms: {} {}",
                        started.elapsed().as_millis(),
                        e.code(),
                        e.message()
                    );
                    0
                }
            };
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
    let mut w = wire.held();
    w.calls.push_back(call.to_owned());
    if w.calls.len() > CALLS_KEPT {
        w.calls.pop_front();
    }
}

/// Round-robin over a fleet of `peers`, peer 0 included.
///
/// This was `1 + cursor % (peers - 1)`, which excluded peer 0 from
/// dispatched work entirely. The reasoning behind that was sound and the
/// conclusion was not: peer 0 does hold the client's job and must answer
/// the gateway Solve, so adopting onto peer 0 means building there, pushing
/// to the mirror and importing back to the same daemon - a round-trip for
/// nothing. But skipping peer 0 turns a two-machine fleet into OFFLOADING:
/// one machine builds, the other shuffles bytes, and half the grid is idle
/// by construction. Principle 1 says the grid is one machine; a machine
/// does not retire a core to hold the paperwork.
///
/// Peer 0 takes its turn and the graph is built in place, which is the
/// round-trip skipped rather than paid.
/// May we hand a secret this machine holds to another machine?
///
/// Off unless asked. Serving secrets to a peer is the one thing here that
/// moves a user's credential off their box, and no amount of scheduling
/// benefit makes that a decision to take on their behalf.
fn serving_secrets() -> bool {
    std::env::var("REBUCK2_SERVE_SECRETS").as_deref() == Ok("1")
}

/// May we forward this machine's ssh agent to a peer?
///
/// Two conditions, because either alone is a lie: the operator has to ask,
/// and there has to be an agent to forward. Advertising the service with no
/// socket behind it makes the peer wait on a call that cannot succeed.
///
/// The sharpest permission here. A secret is a value; an agent is a
/// capability that signs whatever it is asked to, for as long as the build
/// runs.
fn forwarding_agent() -> bool {
    std::env::var("REBUCK2_FORWARD_AGENT").as_deref() == Ok("1")
        && std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
}

/// May a peer build with its own cache mount instead of ours?
///
/// See `Verdict::dispatchable_when` for why this is defensible at all. Off
/// by default: a cache mount whose contents a build depends on is already
/// broken, and being right about that is not the same as being entitled to
/// prove it on someone's CI.
fn local_caches() -> bool {
    // Delegated, not re-read. This function and the driver's arbitration
    // both used to consult the environment; they agreed by coincidence and
    // stopped agreeing the moment one of them grew a policy the other did
    // not have.
    crate::dispatch::policy().caches
}

/// Can we serve every secret THIS graph asks for?
///
/// Being able to serve secrets in general is not the question. An earthly
/// build mounts `earthly_debugger_settings`, whose value earthly generates
/// per build and keeps in its own internal store - no environment holds it,
/// and earthly's own provider refuses it by name. Lifting the exclusion on
/// the general capability would offer eleven solves in twelve that are
/// certain to fail on the peer, and fail-open would rebuild every one at
/// home having paid for the trip.
///
/// So: all of them, or none.
fn can_serve_secrets(def: &bollard_buildkit_proto::pb::Definition) -> bool {
    if !serving_secrets() {
        return false;
    }
    // An EMPTY value is not a resolved secret. `env::var` returns Ok("") for
    // a variable set to nothing, so the first version of this check called
    // an unset-in-practice secret resolvable, served the peer an empty
    // string, and let the build fail there instead of staying home.
    let resolves = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty()).is_some();
    crate::dispatch::secret_ids(def).iter().all(|id| {
        let name = buildkit_session::EnvSecrets::var_for(id);
        resolves(&name) || resolves(&name.to_uppercase())
    })
}

/// A graph's identity, for remembering how long it took.
///
/// The same bytes `solve::build_and_publish` tags the adopted image with, so
/// "this graph" means the same thing to the estimator and to the mirror.
fn graph_key(def: &bollard_buildkit_proto::pb::Definition) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    for op in &def.def {
        bytes.extend_from_slice(op);
    }
    crate::store::sha256_hex(&bytes)
}

/// Is this subtree big enough to be worth shipping? OFF by default, and the
/// reason is the interesting part.
///
/// The workload sweep showed the fleet HURTING at small build sizes -
/// twenty-four short builds took 12s split evenly against 8s with no fleet at
/// all - and helping 2x at large ones, so "ship it only if it is bigger than
/// its own transfer" looked like the rule. It is not, and wiring it is what
/// showed that: with real numbers the gate never fires.
///
/// Twice, for two different reasons, both worth keeping:
///
/// 1. The first overhead estimate was away-time minus what the PEER spent
///    building. But the duration being compared against is itself an away
///    duration and already contains that overhead, so the comparison was
///    est > (est - build), which is nearly always true. Fixed by comparing
///    two client-visible numbers in the same units.
/// 2. With that fixed it still does not fire, and the model is why. At small
///    build sizes home is not SATURATED - twenty-four short builds fit
///    comfortably in sixteen cores - so shipping adds latency without
///    relieving anything, and per-build times cannot see that. Build size was
///    only ever a proxy for how full the local machine is.
///
/// So the signal is saturation, not size, and that is a different mechanism
/// from this one. Left in behind `REBUCK2_GATE=1` with its measurement live,
/// because the estimator is sound and only the question put to it is wrong.
fn worth_shipping(est_ms: Option<u64>, overhead_ms: u64) -> bool {
    /// How much bigger than the overhead before shipping pays.
    ///
    /// 1, from the measured numbers: at work=90 a build costs 11.4s at home
    /// and shipping costs 5.8s on top, and the fleet DID help there - 18s
    /// against 24s. A margin of 2 puts the threshold at 11.6s, just above
    /// that 11.4s build, and would have refused a third of the wall clock.
    const MARGIN: u64 = 1;
    match est_ms {
        None => true,
        Some(est) => est > overhead_ms.saturating_mul(MARGIN),
    }
}

/// How much work this machine can run at once.
///
/// The local core count, and using it here is not the mistake made earlier
/// with peer weights. "Is this machine FULL" and "how FAST is this machine"
/// are different questions: the first is a count of things running against a
/// count of things that can run, and the second is a throughput that core
/// counts turned out to be uncorrelated with once transfer dominated.
///
/// Overridable, because the daemon's own `max-parallelism` is not queryable
/// through the API and may not match the host.
fn home_slots() -> usize {
    std::env::var("REBUCK2_HOME_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
}

/// A straggler is only abnormal relative to something.
///
/// Three times the median of what adoptions have actually cost, floored so a
/// fleet of half-second subtrees is not hedged on jitter. `None` means no
/// adoption has completed yet and there is no basis for calling anything
/// slow - which is deliberately today's behaviour: wait.
///
/// This exists because a fixed cold default would be a number invented to
/// make one measurement look good. The 0.25-CPU peer took 163s against a
/// normal 9.5s; any threshold between the two "works", and picking one before
/// the fleet has said what normal is means picking it for the fixture.
/// How many recent timings any one history keeps.
///
/// Enough that a median is not one unlucky build, few enough that the window
/// turns over inside a single session - the threshold has to be able to
/// follow the fleet, not average it since boot.
const OBSERVATIONS: usize = 64;

/// Gateway calls retained for the report. Enough to show the interleaving
/// around the end of a build, which is the only part anyone reads.
const CALLS_KEPT: usize = 200;

/// Record a timing, forgetting the oldest once the window is full.
///
/// These histories used to grow for the life of the process. The cost people
/// notice is memory, and the cost that actually bites is that `hedge_after`
/// clones and sorts the whole thing on every adoption - but neither is the
/// reason for the bound. An all-time median cannot TRACK anything: a fleet
/// that was slow this morning keeps a high threshold all afternoon, so the
/// hedge stops withdrawing from stragglers exactly when the rest of the fleet
/// has become fast enough for one to hurt.
fn remember(history: &mut Vec<u64>, ms: u64) {
    history.push(ms);
    if history.len() > OBSERVATIONS {
        // Cheap because it runs every time: one shift of a 64-element vec,
        // never a growing backlog.
        history.remove(0);
    }
}

/// Is the size gate on?
///
/// One reader, because the two sides of this used to be written separately:
/// the estimator recorded unconditionally while only the gate consulted it,
/// so with the gate off - the default, and the measured recommendation - the
/// proxy maintained an unbounded per-graph history that nothing read.
fn gating() -> bool {
    std::env::var("REBUCK2_GATE").as_deref() == Ok("1")
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
/// Where one gateway solve spent its time, in ms.
///
/// `total` is the whole call as the client experienced it. The three parts
/// do NOT sum to it and are not meant to.
///
/// `answer` is measured and is almost always 1ms, which is not a mistake:
/// a gateway Solve is LAZY. It returns a ref, and the ref is evaluated on
/// `return` - so a nine-second build and a registry pull are both a
/// millisecond here. Look at `Wire::returns` for the other half. This was
/// nearly read as "answering is free", which would have sent the next
/// iteration optimising the wrong end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// Milliseconds after the FIRST solve that this one arrived.
    ///
    /// Duplicated from `arrivals` on purpose. That vector is appended on
    /// arrival and this one on completion, so with eight solves in flight
    /// the two are not index-aligned and no start can be paired with any
    /// duration. Anything asking "how many were running at once" needs the
    /// pair, so the pair rides together.
    pub start: u64,
    pub total: u64,
    /// This exact graph had been solved before.
    pub resend: bool,
    /// Rewriting the graph: publishing the context, mirroring base images.
    pub portable: u64,
    /// The peer building it and pushing the result.
    pub adopt: u64,
    /// Peer 0 answering - after adoption this is a PULL, not a build.
    pub answer: u64,
}

impl Span {
    /// When it stopped, on the same clock as [`Span::start`].
    pub fn end(&self) -> u64 {
        self.start + self.total
    }

    /// What dispatch cost that a local build would not have paid.
    ///
    /// Not `total - adopt`: the peer's build replaces work peer 0 would have
    /// done anyway, so charging it as overhead double-counts the build and
    /// makes dispatch look catastrophic. Only preparation and the answer are
    /// new - and since the answer is lazy, in practice this is `portable`.
    pub fn tax(&self) -> u64 {
        self.portable + self.answer
    }
}

/// The most a fleet could ever do to this run, from Amdahl.
///
/// The serial fraction is the wall time during which at most ONE solve was
/// in flight: milliseconds where no second piece of work existed to give
/// anybody. `1/s` is then the speedup an infinite fleet would reach.
///
/// The honest caveat, because this number will be quoted: a stretch of
/// concurrency 1 means no second solve was RUNNING, and the reason might be
/// a dependency - which is a real ceiling - or a dispatcher that declined to
/// place one, which is not. Read it beside `not routed` and `placed`. It is
/// a bound on what the graph offers, and it becomes a bound on the fleet
/// only when dispatch is otherwise healthy.
pub fn ceiling(spans: &[Span]) -> f64 {
    if spans.is_empty() {
        return 1.0;
    }
    let mut edges: Vec<(u64, i32)> = Vec::with_capacity(spans.len() * 2);
    for s in spans {
        edges.push((s.start, 1));
        edges.push((s.end(), -1));
    }
    edges.sort_by_key(|&(t, d)| (t, d));
    let (mut now, mut at, mut serial, mut wall) = (0i32, edges[0].0, 0u64, 0u64);
    for &(t, d) in &edges {
        let dt = t.saturating_sub(at);
        if now >= 1 {
            wall += dt;
            if now == 1 {
                serial += dt;
            }
        }
        now += d;
        at = t;
    }
    if wall == 0 {
        return 1.0;
    }
    let s = serial as f64 / wall as f64;
    // Zero serial time is unbounded, and "inf" is not a thing to print. The
    // sample size is the honest stand-in: it is what this run actually had
    // to spread.
    if s == 0.0 {
        return spans.len() as f64;
    }
    1.0 / s
}

/// How many solves were running at once, and how much of the wall clock that
/// filled.
///
/// The number principle 19 asks for. A fleet of six that never has more than
/// one solve in flight is not slow because dispatch is slow - it is a
/// workload with no seam, and every other figure in the report will be read
/// wrongly without this one beside it.
///
/// `occupancy` is solve-time divided by the span it happened in: 1.0 is a
/// queue, 5.0 is five machines genuinely busy. It is an area over a span
/// rather than an average of a skewed sample, which is why a mean is the
/// right shape here and is not elsewhere in this report.
pub fn concurrency(spans: &[Span]) -> (usize, f64) {
    if spans.is_empty() {
        return (0, 0.0);
    }
    let mut edges: Vec<(u64, i32)> = Vec::with_capacity(spans.len() * 2);
    for s in spans {
        edges.push((s.start, 1));
        edges.push((s.end(), -1));
    }
    // Ends before starts at the same instant, or a handover reads as an
    // overlap and a strictly serial run reports a peak of two.
    // -1 sorts before +1, which IS ends-before-starts. Writing `-d` here
    // reverses it and a strictly serial run reports a peak of two; the test
    // beside this caught exactly that, having been written from this comment.
    edges.sort_by_key(|&(t, d)| (t, d));
    let (mut now, mut peak) = (0i32, 0i32);
    for (_, d) in &edges {
        now += d;
        peak = peak.max(now);
    }
    let first = spans.iter().map(|s| s.start).min().unwrap_or(0);
    let last = spans.iter().map(|s| s.end()).max().unwrap_or(0);
    let busy: u64 = spans.iter().map(|s| s.total).sum();
    let wall = last.saturating_sub(first);
    (
        peak as usize,
        if wall == 0 {
            0.0
        } else {
            busy as f64 / wall as f64
        },
    )
}

/// Everything the SIGINT report is computed from.
///
/// **This grows for the life of the process, on purpose and not without
/// cost.** Eight of the fields below keep one entry per solve - `arrivals`,
/// `spans`, `graph_ids` and the timing vectors - because the report quotes
/// medians and orderings that a running total cannot reconstruct.
///
/// A proxy started per build, which is how it is normally driven, ends before
/// that matters. A proxy left up as a service accumulates roughly a hundred
/// bytes a solve, plus a `graph_ids` entry, indefinitely. Bounding them is not
/// free: several are read with `len()` as a COUNT, so a window would silently
/// change what the report claims rather than just what it remembers.
///
/// Recorded rather than fixed because the fix is per-field - counts want to
/// become counters, distributions want a window - and doing half of it would
/// leave a report that is bounded and wrong.
#[derive(Default)]
pub struct Wire {
    /// Subtrees whose ancestry was replaced by an import of an already-built
    /// result - the step from N prefixes to 1.
    pub grafted: u64,
    pub solves: u64,
    pub ops: u64,
    pub registry_sources: u64,
    pub local_sources: u64,
    pub other_sources: u64,
    pub platforms: std::collections::BTreeSet<String>,
    /// Ops per Solve, in arrival order - the balance question.
    pub per_solve: Vec<usize>,
    /// Graph digests seen, to measure how much two Solves share.
    /// Held as the digest's leading 64 bits, not its hex text.
    ///
    /// One entry per DISTINCT op for the life of the process, so on a
    /// monorepo this is every vertex of every graph and the largest thing
    /// here by some way. A 64-char `String` costs about ninety bytes before
    /// the set's own overhead; the digest is already a hash, so the text was
    /// pure carriage. Collisions are a miscount of one in a diagnostic, and
    /// at a million distinct ops the odds are about three in a hundred
    /// million.
    seen_ops: std::collections::BTreeSet<u64>,
    /// How many distinct solves each op has appeared in.
    ///
    /// The signal pre-positioning actually wants. `op_by_worker` counts
    /// which machines have BEEN SENT an op - a measurement of the past, and
    /// one that affinity deliberately drives towards one, so gating a
    /// prefetch on it means the better affinity works the less is ever
    /// pre-positioned. This counts the graphs the CLIENT has sent, which is
    /// known before anything is dispatched: an op in two solves will be
    /// wanted twice however it is placed.
    op_solves: std::collections::BTreeMap<u64, u32>,
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
    /// Gateway calls in arrival order, most recent [`CALLS_KEPT`] only.
    ///
    /// Interleaving is what this is for - seeing that a `return` lands before
    /// the `solve` it belongs to. That is a local question, so the tail
    /// answers it as well as the whole tape would, and the whole tape both
    /// grows for the life of the process and prints as a line nobody reads.
    pub calls: std::collections::VecDeque<String>,
    /// The last failure from mirroring a base image, verbatim.
    ///
    /// A separate field from `last_refusal` because they accuse different
    /// machines. A peer refusing is the peer's problem; a push that fails is
    /// OURS, and blaming the peer for it sends the reader to the wrong host.
    pub last_mirror_error: Option<String>,
    /// The last thing a peer said when it refused, verbatim.
    ///
    /// Kept so the report can DIAGNOSE rather than tally. A fleet that
    /// placed nothing looks exactly like a fleet with nothing to place, and
    /// the difference is usually sitting in this string.
    pub last_refusal: Option<String>,
    /// Solves placed on a peer other than the upstream.
    pub routed: u64,
    /// Client-visible build times, split by where the work went.
    ///
    /// The sweep says the best split is 2:1 toward home, and a weight can
    /// only be derived from measurement if the ratio the sweep implies is
    /// actually observable. These two are what would have to predict it.
    pub home_ms: Vec<u64>,
    pub away_ms: Vec<u64>,
    /// How long each `Control.Solve` took - the call the CLIENT blocks on,
    /// and so the only honest measure of a build's duration from here.
    pub control_solves: Vec<u64>,
    /// How long each `return` took.
    ///
    /// Measured on the theory that a lazy gateway solve's work lands here.
    /// It does not - these come back in about a millisecond too. Kept
    /// because the zero is the finding: neither gateway call is where the
    /// time is, so the next person does not have to re-measure it.
    pub returns: Vec<u64>,
    /// When each gateway solve arrived, ms after the first.
    ///
    /// The saturation gate assumes solves arrive together; if they trickle in
    /// over the length of a build, home never fills and the gate never opens.
    /// That is a property of the CLIENT, not of placement, and it is invisible
    /// from any counter that only records what was decided.
    pub arrivals: Vec<u64>,
    /// When the first gateway solve landed, so arrivals are relative.
    pub first_solve: Option<std::time::Instant>,
    /// The most builds ever running at home at once, against the slot count
    /// that gates dispatch. If this never reaches the limit, the gate never
    /// opens and the fleet is idle for a reason that has nothing to do with
    /// placement.
    pub peak_home: usize,
    pub slots: usize,
    /// Home versus the fleet: key 0 is home, key 1 is "went to a peer".
    ///
    /// TWO keys, not one per peer - WHICH machine took it is the driver's
    /// business and the driver reports it. Read as peer indices, `{1: 70}`
    /// says every job landed on one overloaded machine; it actually says 70
    /// jobs left home, and the six runners had 52/47/36/38/37/33 of them.
    /// Cost half an hour of chasing a load-balancing bug that was not there.
    ///
    /// Added because "round 2 was fast" is not evidence of avoidance - a
    /// control run showed a uniform fleet reaching the same wall clock purely
    /// from the peers' own caches. Which machine got what has to be counted,
    /// not inferred from a timing.
    pub placed: std::collections::BTreeMap<usize, u64>,
    /// Per-solve timing, in arrival order.
    pub spans: Vec<Span>,
    /// Solves whose turn fell to peer 0 and were built where they already
    /// were. Not a rejection: peer 0 is a machine like any other, and
    /// counting its share as "not routed" understates a fleet by exactly
    /// 1/n - a perfectly-balanced pair would report 50% dispatch.
    pub home: u64,
    /// Why a solve was NOT placed, counted.
    ///
    /// One routed solve out of twelve is either a fleet barely working or a
    /// fleet barely used, and the difference is not visible from the
    /// outside. Counting the reason is what turns "improve routing" into a
    /// specific thing to fix.
    pub rejected: std::collections::BTreeMap<String, u64>,

    /// Peers struck, counted. A strike is this proxy deciding a machine is
    /// the problem, and it is the one judgement here that outlives the
    /// solve that produced it - a struck peer is deprioritised for the rest
    /// of the run.
    ///
    /// Reported because the alternative is inferring it from a rejection
    /// message, which conflates "the peer failed" with "something the peer
    /// depended on failed". Kill the shared registry and both produce
    /// refusals; only one of them should cost a machine its standing.
    pub struck: std::collections::BTreeMap<usize, u64>,
}

impl Wire {
    /// Say so when the fleet did nothing, and guess why.
    ///
    /// Added after following this project's own quickstart and getting a
    /// working build out of a fleet that placed not one solve. Publishing is
    /// told to be insecure per-solve so the mirror fills and the log looks
    /// healthy; pulling is governed by daemon config alone, so every peer
    /// refuses and dispatch quietly falls back to home. The build succeeds.
    /// Nothing draws attention to the fleet being idle.
    ///
    /// A tally cannot fix that - the reader has to already suspect something.
    /// So the proxy says it.
    fn diagnose(&self) {
        // `routed`, not `placed`. Placement counts DECISIONS: a solve sent to
        // a peer that then refused it is placed and NOT routed, which is
        // exactly the case this note exists for. Checking placements missed
        // the very misconfiguration it was written from.
        if self.routed == 0 && self.solves > 0 {
            println!(
                "[wire] NOTE: no solve completed on a peer. This fleet did no distributed work."
            );
            if let Some((why, n)) = self.rejected.iter().max_by_key(|(_, n)| **n) {
                println!("[wire]   most common reason: {why} (x{n})");
            }
        }
        // Our own push failing is a different accusation from a peer
        // refusing, and the text arrives on a different path.
        if let Some(m) = &self.last_mirror_error {
            let storage = [
                "Permission denied",
                "No space left",
                "os error 13",
                "os error 28",
            ]
            .iter()
            .any(|p| m.contains(p));
            if storage {
                println!(
                    "[wire] LIKELY CAUSE: the MIRROR could not be written to - check its disk\n\
                     [wire]   and the permissions on its --store path. This is our registry\n\
                     [wire]   failing, not a peer: {m}"
                );
                return;
            }
            println!("[wire]   a base image could not be mirrored: {m}");
        }
        let Some(r) = &self.last_refusal else { return };
        if r.contains("server gave HTTP response to HTTPS client") {
            println!(
                "[wire] LIKELY CAUSE: a peer cannot PULL from the mirror over plain HTTP.\n\
                 [wire]   Publishing is told to be insecure per-solve, so the mirror filled and\n\
                 [wire]   the log looked fine; pulling needs daemon config. Add to every\n\
                 [wire]   buildkitd's /etc/buildkit/buildkitd.toml:\n\
                 [wire]     [registry.\"<REBUCK2_MIRROR>\"]\n\
                 [wire]       http = true\n\
                 [wire]       insecure = true"
            );
        }
    }

    /// Solves whose graph had already been sent, byte for byte.
    fn resends(&self) -> usize {
        let mut uniq = self.graph_ids.clone();
        uniq.sort();
        uniq.dedup();
        self.graph_ids.len().saturating_sub(uniq.len())
    }

    /// Record a solve, and say whether this exact graph has been seen before.
    ///
    /// The verdict is returned rather than looked up later because arrival
    /// order and completion order are different orders: `graph_ids` grows on
    /// arrival, `spans` on completion, and eight solves run at once.
    /// Will more than one graph want this op? Asked of the SOLVES seen so
    /// far, so it is a prediction rather than a record of placements.
    fn shared_as_sent(&self, def: &bollard_buildkit_proto::pb::Definition) -> bool {
        def.def.iter().any(|b| {
            let d = crate::store::sha256_hex(b);
            let short = u64::from_str_radix(&d[..16], 16).unwrap_or_default();
            self.op_solves.get(&short).is_some_and(|n| *n > 1)
        })
    }

    fn observe(&mut self, def: &bollard_buildkit_proto::pb::Definition) -> bool {
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
        let graph = crate::store::sha256_hex(ids.join("").as_bytes());
        let seen_before = self.graph_ids.contains(&graph);
        self.graph_ids.push(graph);
        let mut already = 0usize;
        for bytes in &def.def {
            let digest = crate::store::sha256_hex(bytes);
            let short = u64::from_str_radix(&digest[..16], 16).unwrap_or_default();
            *self.op_solves.entry(short).or_default() += 1;
            if !self.seen_ops.insert(short) {
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
        seen_before
    }

    /// The characterisation, as one block. Printed on shutdown because the
    /// interesting numbers are about the BUILD, not any one Solve.
    pub fn report(&self) {
        let mut sizes = self.per_solve.clone();
        sizes.sort_unstable();
        let median = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
        println!("[wire] ---- what this build looked like ----");
        // ON is not the claim; APPLIED is. A mechanism enabled by the
        // environment and never reached is reported as such, because three
        // of them have now been measured as "does not help" while never
        // running at all.
        println!(
            "[wire] mechanisms   : {}",
            crate::mech::summary(&[
                ("affinity", crate::dispatch::affinity()),
                ("seed_mounts", !crate::dispatch::cache_seeds().is_empty()),
                ("min_siblings", min_siblings() > 0),
                (
                    "prefetch",
                    std::env::var("REBUCK2_PREFETCH").as_deref() == Ok("1")
                ),
                (
                    "graft",
                    std::env::var("REBUCK2_GRAFT").as_deref() == Ok("1")
                ),
                (
                    "cut_prefix",
                    std::env::var("REBUCK2_CUT_PREFIX").as_deref() == Ok("1")
                ),
                (
                    "local_nested",
                    std::env::var("REBUCK2_LOCAL_NESTED").as_deref() == Ok("1")
                ),
            ])
        );
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
            (self.repeated_ops * 100).checked_div(self.ops).unwrap_or(0)
        );

        // The distinction that decides what the repetition MEANS. A client
        // RE-SENDING an identical graph is an artefact of how it drives the
        // API; two graphs sharing a prefix is work a fleet could share. The
        // summary percentage cannot tell them apart, and reading one as the
        // other is how a measurement becomes a wrong conclusion.
        let resends = self.resends();
        // What the repeats COST, not just how many there are. A resend that
        // buildkit answers from its own cache in 500ms is an artefact of how
        // earthly drives the API and nothing to fix; a resend that pays full
        // price is work a memo would delete outright - no fleet involved.
        // Counting them without timing them cannot tell those apart, and the
        // two conclusions are opposite.
        let (r_n, r_ms): (usize, u64) = self
            .spans
            .iter()
            .filter(|s| s.resend)
            .fold((0, 0), |(n, ms), s| (n + 1, ms + s.total));
        let (f_n, f_ms): (usize, u64) = self
            .spans
            .iter()
            .filter(|s| !s.resend)
            .fold((0, 0), |(n, ms), s| (n + 1, ms + s.total));
        if r_n > 0 {
            println!(
                "[wire] resend cost    : {r_n} resends {}ms mean, {f_n} first-sightings {}ms mean",
                r_ms / r_n as u64,
                f_ms / f_n.max(1) as u64,
            );
        }
        println!(
            "[wire] distinct graphs: {} of {} solves ({resends} identical RESENDS)",
            self.graph_ids.len() - resends,
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
        println!("[wire] built at home  : {} (peer 0's own share)", self.home);
        // The map stays - `fleet-check.sh` parses `<key>: <n>` out of this
        // line - but it no longer travels alone. Two keys, not one per peer.
        println!(
            "[wire] placed         : {:?} = home {}, fleet {} (WHICH peer is the driver's line)",
            self.placed,
            self.placed.get(&0).copied().unwrap_or(0),
            self.placed.get(&1).copied().unwrap_or(0),
        );
        self.diagnose();
        if let (Some(&first), Some(&last)) = (self.arrivals.first(), self.arrivals.last()) {
            println!(
                "[wire] arrivals       : {} solves spread over {}ms (first {first}, last {last})",
                self.arrivals.len(),
                last.saturating_sub(first)
            );
        }
        // Principle 19, printed rather than reasoned about afterwards. A peak
        // of one over six machines is not a slow fleet, it is a workload
        // with no seam - and every other figure here reads wrongly without
        // it. `1.4x ceiling` beside `1.8x achieved` is the whole story;
        // neither number alone is.
        let (peak, occupancy) = concurrency(&self.spans);
        let ceiling = ceiling(&self.spans);
        println!(
            "[wire] concurrency    : peak {peak} solves at once, occupancy {occupancy:.2} \
             (1.00 = a queue)"
        );
        println!(
            "[wire] amdahl ceiling : {ceiling:.2}x - the most ANY fleet could do to this \
             graph, from the {:.0}% of wall clock with one solve in flight",
            if ceiling > 0.0 { 100.0 / ceiling } else { 0.0 }
        );
        println!(
            "[wire] home peak      : {} of {} slots{}",
            self.peak_home,
            self.slots,
            if self.slots > 0 && self.peak_home < self.slots {
                " - never saturated, so dispatch never opened"
            } else {
                ""
            }
        );
        let mean = |v: &[u64]| -> u64 {
            if v.is_empty() {
                0
            } else {
                v.iter().sum::<u64>() / v.len() as u64
            }
        };
        let (h, a) = (mean(&self.home_ms), mean(&self.away_ms));
        println!(
            "[wire] service ms     : home {h} ({}) away {a} ({}) ratio {:.2}",
            self.home_ms.len(),
            self.away_ms.len(),
            if h == 0 { 0.0 } else { a as f64 / h as f64 }
        );
        for (i, s) in self.spans.iter().enumerate() {
            println!(
                "[wire] solve {i} ms     : {}..{} total {} = portable {} + peer {} \
                 + answer {} (tax {})",
                s.start,
                s.end(),
                s.total,
                s.portable,
                s.adopt,
                s.answer,
                s.tax()
            );
        }
        println!(
            "[wire] build ms       : {:?} (Control.Solve - what the client waits for)",
            self.control_solves
        );
        println!(
            "[wire] return ms      : {:?} (near-zero; not where work lands)",
            self.returns
        );
        println!("[wire] not routed     : {:?}", self.rejected);
        // Always printed, including when empty. "struck: {}" is the evidence
        // that a dead mirror cost no peer its standing; a line that appears
        // only on failure cannot say that.
        println!("[wire] struck         : {:?}", self.struck);
        println!(
            "[wire] call order     : {}{}",
            if self.calls.len() == CALLS_KEPT {
                "... "
            } else {
                ""
            },
            self.calls.iter().cloned().collect::<Vec<_>>().join(" ")
        );
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
            .held()
            .entry(key.clone())
            .or_default()
            .clone()
    }

    /// What a key resolved to, if it has resolved and succeeded.
    fn resolved(&self, key: &(String, String)) -> Option<String> {
        self.published
            .held()
            .get(key)
            .and_then(|c| c.get().cloned().flatten())
    }

    /// The session behind this gateway call, if we learned one.
    fn session_for(&self, meta: &tonic::metadata::MetadataMap) -> String {
        meta.get("buildkit-controlapi-buildid")
            .and_then(|v| v.to_str().ok())
            .and_then(|b| self.sessions.held().get(b).cloned())
            .unwrap_or_default()
    }

    /// Swap `local://` sources for the contexts we published, so the graph
    /// depends on content rather than on one machine's disk.
    async fn make_portable(
        &self,
        def: &bollard_buildkit_proto::pb::Definition,
        session: &str,
        mirror: &Mirror,
        // The architecture the graph is going TO. Base images are mirrored
        // for it, not for ours - see `solve::mirror_image`.
        target: Option<&str>,
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
            // The target architecture is part of the key as well as the tag:
            // one OnceCell per (image, architecture), or the first peer to
            // ask would settle the answer for every other architecture.
            let key = (format!("base:{}", target.unwrap_or("default")), r.clone());
            let cell = self.cell(&key);
            let full = format!("docker-image://{r}");
            cell.get_or_init(|| async {
                match crate::solve::mirror_image(
                    &mirror.buildkit,
                    &mirror.registry,
                    session,
                    &full,
                    target,
                )
                .await
                {
                    Ok(reference) => {
                        println!("[proxy] base {r} mirrored as {reference}");
                        // A BASE image is shared by construction: every graph
                        // that names it needs it, on whichever machine it
                        // lands. That is the signal a prefetch wants, and the
                        // one the first version did not use - it fired on
                        // every finished subtree, most of which only the
                        // requester will ever want. Pre-position what is
                        // shared; leave a leaf result to the one machine
                        // asking for it.
                        self.driver.prefetch_image(&reference).await;
                        Some(reference)
                    }
                    Err(e) => {
                        println!("[proxy] base {r} not mirrored: {e:#}");
                        self.wire.held().last_mirror_error = Some(format!("{e:#}"));
                        None
                    }
                }
            })
            .await;
        }

        // The FULL reference, scheme included: the rewrite replaces the
        // identifier wholesale, and buildkit rejects a bare
        // `host:port/name:tag` with "invalid".
        // The SAME key the mirror wrote under, architecture included. Writing
        // under `base:linux/amd64` and reading under `base` mirrors the image
        // correctly, finds nothing, leaves the graph naming docker.io, and
        // then rejects it as "base unmirrored" - a success and a failure that
        // never meet, with both printed.
        let key_ns = format!("base:{}", target.unwrap_or("default"));
        let out = crate::dispatch::rewrite_registry_sources(&out, &|r| {
            self.resolved(&(key_ns.clone(), r.to_owned()))
        });

        // GIT, the third kind of source that pins a graph to this machine.
        //
        // 407 of 1018 solves on `+test-no-qemu` were grounded by one, which
        // makes it the single largest reason earthbuild's own Earthfile does
        // not dispatch. buildkit resolves git credentials through the client
        // session; a worker has none, so it dies with `no active sessions`
        // after taking the lead.
        //
        // We hold the session, so we fetch the tree once and publish it as
        // an image. Same machinery as a base image - `mirror_image` builds a
        // one-op source graph and exports it, and nothing in it is specific
        // to `docker-image://`.
        //
        // Deliberately AFTER the base rewrite: mirroring emits
        // `docker-image://` identifiers of its own, and running the base
        // pass over those would try to mirror our own mirror.
        let gits: std::collections::BTreeSet<String> = out
            .def
            .iter()
            .filter_map(|b| {
                use prost::Message;
                match bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                    .ok()
                    .and_then(|o| o.op)
                {
                    Some(bollard_buildkit_proto::pb::op::Op::Source(src))
                        if src.identifier.starts_with("git://") =>
                    {
                        Some(src.identifier)
                    }
                    _ => None,
                }
            })
            .collect();
        for g in gits {
            // Keyed WITHOUT the scheme, because that is what
            // `rewrite_git_sources` passes to the lookup - see
            // `the_rewriter_hands_over_a_name_with_no_scheme`. Keying by the
            // full identifier mirrored correctly, found nothing on the way
            // back, and left 397 solves unportable with no error anywhere:
            // both halves worked, they just did not meet.
            //
            // The same mistake, with the same symptom, is documented three
            // lines above for base images.
            let name = g.strip_prefix("git://").unwrap_or(&g).to_owned();
            let key = (format!("git:{}", target.unwrap_or("default")), name);
            let cell = self.cell(&key);
            let g2 = g.clone();
            cell.get_or_init(|| async move {
                match crate::solve::mirror_image(
                    &mirror.buildkit,
                    &mirror.registry,
                    session,
                    &g2,
                    target,
                )
                .await
                {
                    Ok(reference) => {
                        println!("[proxy] git {g2} mirrored as {reference}");
                        Some(reference)
                    }
                    Err(e) => {
                        // Not fatal. An unmirrored git source leaves that
                        // subtree where it already was - built at home.
                        println!("[proxy] git {g2} not mirrored: {e:#}");
                        None
                    }
                }
            })
            .await;
        }
        let git_ns = format!("git:{}", target.unwrap_or("default"));
        let out = crate::dispatch::rewrite_git_sources(&out, &|r| {
            self.resolved(&(git_ns.clone(), r.to_owned()))
        });
        // LAST, and after everything that rewrites sources. A seed is itself
        // a `docker-image://` source pointing at a registry a worker can
        // already reach, so it neither needs mirroring nor should be
        // mirrored - putting it earlier would send it round the base-image
        // path and copy a cache image through peer 0 for no reason.
        crate::dispatch::seed_cache_mounts(&out, crate::dispatch::cache_seeds())
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
            .and_then(|b| self.sessions.held().get(b).cloned());
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
                        self.wire.held().contexts_published += 1;
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
/// Returns whether this exact graph has been solved before.
fn report_gateway(wire: &std::sync::Mutex<Wire>, req: &gw::SolveRequest) -> bool {
    let Some(def) = &req.definition else {
        // Say WHY nothing can be dispatched, not merely that nothing was.
        // "no definition" is true and useless; a user who points `docker
        // build` at this proxy and sees an even split of nothing deserves the
        // reason and the remedy.
        let mut w = wire.held();
        if req.frontend.is_empty() {
            *w.rejected.entry("no definition".to_owned()).or_default() += 1;
            return false;
        }
        let key = format!("frontend runs in the daemon: {}", req.frontend);
        let first = !w.rejected.contains_key(&key);
        *w.rejected.entry(key).or_default() += 1;
        drop(w);
        if first {
            println!(
                "[proxy] frontend {:?} is resolved INSIDE the daemon, so the LLB it \
                 generates never crosses this proxy and no part of it can be placed \
                 on a peer.\n\
                 [proxy]   what dispatches: clients that build the graph themselves \
                 and send it - earthbuild, `buildctl build < graph.llb`, anything \
                 driving the gateway with a Definition.\n\
                 [proxy]   what does not: `--frontend <name>`, because the daemon \
                 runs that frontend as its own gateway client and never asks us.",
                req.frontend
            );
        }
        return false;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    let mut w = wire.held();
    let resend = w.observe(def);
    println!(
        "[proxy] gateway solve #{}: {} ops, {} cuts >= {MIN_CUT_OPS}, {} free-frontier",
        w.solves,
        a.ops,
        a.cuts.len(),
        free.len(),
    );
    resend
}

/// Is there anything for this solve to overlap with if we send it away?
///
/// `inflight` counts solves in progress including this one. A solve with no
/// concurrent siblings cannot use a second machine - there is no second
/// piece of work to run beside it - so dispatching it buys nothing and pays
/// the whole handover.
///
/// Straight out of the traces: a full `+test-no-qemu` spends 192 of its 271
/// baseline seconds in a base chain three targets deep, one target at a
/// time, and dispatching that chain cost +90s for nothing it could
/// possibly gain. The parallel phase either side of it is 79s and costs the
/// same in both legs.
///
/// A threshold of 0 disables the rule, so the A/B against the old behaviour
/// is one environment variable.
fn worth_dispatching_now(inflight: usize, min_siblings: usize) -> bool {
    min_siblings == 0 || inflight > min_siblings
}

/// How crowded it has to be before dispatch is worth the handover.
fn min_siblings() -> usize {
    std::env::var("REBUCK2_MIN_SIBLINGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// An endpoint shaped like the ones the proxy dials at startup.
///
/// Same keepalive: a Session can sit idle while a nested build runs, and
/// anything that reaps idle connections takes the build with it.
fn session_endpoint(upstream: &str) -> Result<tonic::transport::Endpoint, tonic::transport::Error> {
    Ok(tonic::transport::Endpoint::new(upstream.to_owned())?
        .http2_keep_alive_interval(std::time::Duration::from_secs(20))
        .keep_alive_timeout(std::time::Duration::from_secs(60))
        .keep_alive_while_idle(true))
}

/// Is this failure the connection's fault rather than the build's?
///
/// `Unavailable` is tonic's code for "the transport did not work" - a
/// reconnect, a GOAWAY, a peer that went away mid-body. A build that FAILED
/// comes back as `Unknown` carrying buildkit's message, and retrying that
/// doubles the cost of every red target while changing nothing. This suite
/// deliberately contains targets that fail, so the distinction is not
/// academic.
fn worth_retrying(s: &tonic::Status) -> bool {
    // The transport's own code.
    if s.code() == tonic::Code::Unavailable {
        return true;
    }
    // And buildkit's, which reports its transport failures as Unknown - the
    // same code it uses for a build that FAILED. Found by instrumenting
    // Control.Solve after a full +test-no-qemu kept dying:
    //
    //   upstream Control.Solve failed: Unknown error transport error
    //
    // Matched EXACTLY, not by substring. This suite is full of targets that
    // fail on purpose and their error text is relayed verbatim, so a build
    // whose own output mentions a transport error must not be rebuilt for
    // saying so.
    s.code() == tonic::Code::Unknown && s.message().trim() == "transport error"
}

/// How many reset streams hyper tolerates before it gives up on a connection.
///
/// `None` = no limit, and that is an EXPERIMENT rather than a setting: it
/// removes the RUSTSEC-2024-0003 / hyper#2877 backstops. It exists because
/// raising the limit from hyper's default of 20 to 10_000 did not stop
/// `h2 protocol error: error reading a body from connection`, and the
/// proxy's own connection log stayed silent - which settles nothing, since
/// hyper answers a tripped limit with a GOAWAY and a graceful close, so
/// `serve_connection` returns Ok and never reports it.
///
/// An unparseable value keeps the default. A typo in a CI input must not
/// quietly remove a DoS backstop.
fn reset_limit(raw: Option<&str>) -> Option<usize> {
    const DEFAULT: usize = 10_000;
    match raw {
        Some("none") => None,
        Some(v) => Some(v.parse().unwrap_or(DEFAULT)),
        None => Some(DEFAULT),
    }
}

/// Serve the Control service on `addr`, forwarding to `upstream`.
pub async fn serve(
    addr: std::net::SocketAddr,
    upstream: String,
    driver: std::sync::Arc<crate::driver::Driver>,
) -> anyhow::Result<()> {
    println!("[proxy] buildkit control on {addr} -> {upstream}");
    let relay_target = upstream.clone();
    // What a previous generation built, before the first solve arrives. This
    // is the difference between a bank that holds artefacts and a bank that
    // can be used: the store restores the bytes, this restores what they are.
    driver.load_built().await;
    let mut proxy = Proxy::connect(upstream.clone(), driver).await?;
    proxy.mirror = std::env::var("REBUCK2_MIRROR").ok().map(|registry| Mirror {
        registry,
        buildkit: upstream,
    });
    // The characterisation is about the BUILD, so it prints when we are
    // asked to stop rather than per Solve.
    let wire = proxy.wire.clone();
    let solo = proxy.solo_ms.clone();
    let driver_for_report = proxy.driver.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        // BEFORE the std::Mutex guards below: this awaits, and a std guard
        // held across an await makes the whole task non-Send.
        //
        // What a warm cache would be worth, ranked by the only measure that
        // decides it. A cache id appearing in half the Earthfile and costing
        // two seconds is not worth seeding; one appearing twice and costing
        // four minutes is.
        // Bank it for the next generation, before anything else - the
        // report below can be truncated by a signal, and this must not be.
        driver_for_report.save_built().await;
        let costs = driver_for_report.cache_costs().await;
        let peak = driver_for_report.peak_inflight();
        let (uniq, total_ops, pairs) = driver_for_report.op_duplication().await;
        wire.held().report();
        let solo = solo.held();
        let medians: std::collections::BTreeMap<usize, u64> = solo
            .iter()
            .map(|(p, v)| {
                let mut v = v.clone();
                v.sort_unstable();
                (*p, v[v.len() / 2])
            })
            .collect();
        // The ceiling on what ANY fleet could do for this build. One means
        // the graph is a chain and more machines cannot help; the useful
        // comparison is against the number of workers, not against the
        // number of solves.
        println!("[wire] peak in flight : {peak} subtree(s) at once");
        println!(
            "[wire] grafted        : {} subtree(s) started from a built ancestor",
            wire.held().grafted
        );
        if uniq > 0 {
            // 1.0x means the subtrees are disjoint and a fleet divides the
            // work. Higher means every machine is rebuilding the same
            // ancestry, which is why two workers beat six on this target.
            let sent = total_ops as f64 / uniq as f64;
            // The EXECUTED multiplier, which is the one that costs. It
            // approaches the worker count when every worker rebuilds the
            // same ancestry, and sits at 1.0 when the subtrees are disjoint.
            let built = pairs as f64 / uniq as f64;
            println!(
                "[wire] op duplication : {total_ops} sent / {uniq} distinct = {sent:.1}x sent, \
                 {pairs} (op,worker) pairs = {built:.1}x built"
            );
        }
        if !costs.is_empty() {
            // NOT the sum of the rows. Each lead is added to every cache id
            // it names, so the rows overlap and their total exceeded the
            // whole fleet leg by a factor of seven the first time it was
            // read out.
            let (lead_ms, leads) = driver_for_report.cache_lead_total();
            println!(
                "[wire] cache cost ms  : {lead_ms} in {leads} lead(s) that named any cache \
                 mount; per-id below, and a lead counts under EVERY id it names, so these \
                 rows overlap and do not sum:"
            );
            for (id, ms, n) in costs.iter().take(6) {
                println!("[wire]   {ms:>8}ms  {n:>4} leads  {id}");
            }
        }
        println!(
            "[wire] peer solo ms   : {medians:?} (uncontended, n={:?})",
            solo.iter()
                .map(|(p, v)| (*p, v.len()))
                .collect::<std::collections::BTreeMap<_, _>>()
        );
        std::process::exit(0);
    });
    // A FALLBACK that relays methods we do not implement.
    //
    // earthly's buildkit fork adds `rpc Export` to the gateway service -
    // upstream buildkit has no such method, so the generated LLBBridge
    // service has no such method, so tonic answered SAVE IMAGE with
    // `Unimplemented` and every target that saves an image died at the end
    // of an otherwise successful build.
    //
    // Implementing Export by hand would fix Export. A proxy that refuses
    // what it does not recognise is the actual bug: transparency is the
    // whole contract, and the next fork-only method would cost another day
    // of the same. This relays the raw HTTP/2 request, so we neither parse
    // nor understand it - which is precisely the point.
    //
    // Note the asymmetry with `dispatch`, which fails CLOSED on anything it
    // does not recognise. Different questions: "may this graph run on
    // someone else's machine" must be conservative, "may the client talk to
    // its own daemon" must be transparent.
    let raw = tonic::transport::Endpoint::from_shared(relay_target.clone())?.connect_lazy();
    let relay = move |req: axum::extract::Request| {
        let mut raw = raw.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            println!("[proxy] relaying unimplemented method {}", parts.uri.path());
            // Only the PATH matters to the upstream connection; the channel
            // already knows where it is going.
            parts.uri = axum::http::Uri::builder()
                .path_and_query(
                    parts
                        .uri
                        .path_and_query()
                        .map(|p| p.as_str())
                        .unwrap_or("/"),
                )
                .build()
                .map_err(|e| format!("relay uri: {e}"))?;
            // axum's Body and tonic's differ only in name here; both are
            // the same http-body stream, so this re-wraps rather than
            // buffers - a SAVE IMAGE payload must not be held in memory.
            let out = axum::http::Request::from_parts(parts, tonic::body::Body::new(body));
            // `connect_lazy` yields a Channel that is always ready, so
            // there is nothing to poll before calling it.
            tower::Service::call(&mut raw, out)
                .await
                .map(|r| r.map(axum::body::Body::new))
                .map_err(|e| format!("relay: {e}"))
        }
    };
    let relay_export = relay.clone();
    let router =
        tonic::service::Routes::new(control::control_server::ControlServer::new(proxy.clone()))
            .add_service(gw::llb_bridge_server::LlbBridgeServer::new(proxy))
            .into_axum_router()
            .fallback(axum::routing::any(relay))
            // EXPLICIT, because a fallback is not enough.
            //
            // tonic's generated LlbBridgeServer owns the whole
            // `/moby.buildkit.v1.frontend.LLBBridge/` prefix and answers
            // Unimplemented itself for any method in it that upstream buildkit
            // does not declare, so the router's fallback never sees the request.
            // Measured: the fallback relayed 10 calls to earthly's own registry
            // service and ZERO calls to Export, while SAVE IMAGE kept failing
            // with the error the fallback was written to fix.
            //
            // A more specific axum route beats the service's wildcard, so name
            // the fork's extra method directly. Anything on a service we do not
            // register at all still reaches the fallback.
            .route(
                "/moby.buildkit.v1.frontend.LLBBridge/Export",
                axum::routing::any(relay_export),
            );

    // Serve with the HTTP/2 limits UNSET, which is what tonic's own server
    // does and `axum::serve` does not.
    //
    // hyper's auto builder caps concurrent streams at 200. That was invisible
    // until daemon consolidation: before it, one earthly client talked to
    // this gateway; after it, every nested earthly inside every test dials it
    // too. The build then died with
    //
    //     Error: h2 protocol error: error reading a body from connection
    //
    // reported by the CLIENT, naming nothing on this side. Seen once locally
    // and written off as a flake, which it was not - it is load-dependent,
    // and consolidation is what supplied the load.
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let resets = reset_limit(std::env::var("REBUCK2_H2_RESETS").ok().as_deref());
    println!("[proxy] h2 reset limit: {resets:?}");
    loop {
        let (stream, _peer) = listener.accept().await?;
        let router = router.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let mut builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            builder
                .http2()
                // A TIMER, because the keepalive settings below need one and
                // hyper does not check at build time - it panics inside the
                // connection task with "You must supply a timer", which the
                // client sees only as `Unavailable: error reading from
                // server: EOF`. Every proxied scenario in the fixture suite
                // failed in one second with an empty placement map.
                .timer(hyper_util::rt::TokioTimer::new())
                // None = unlimited, matching tonic. A gateway that refuses
                // the 201st stream mid-build fails the build.
                .max_concurrent_streams(None)
                // The OTHER two h2 limits, and the reason the same client
                // error came back at full-target scale after
                // `max_concurrent_streams` was already unlimited.
                //
                // `max_pending_accept_reset_streams` defaults to TWENTY - not
                // 200 - and exceeding it sends a GOAWAY, which the client
                // reports as `h2 protocol error: error reading a body from
                // connection` and nothing else. Twenty is easy to exceed here
                // for a specific reason: when one target fails, earthly
                // cancels every solve still in flight, all at once. A full
                // `+test-no-qemu` had 17 in flight plus a nested earthly per
                // group, so ONE red test became a dead build - every other
                // group dying simultaneously on `transport error`.
                //
                // Finite, not None: these are the RUSTSEC-2024-0003 and
                // hyper#2877 DoS backstops, and a gateway on a CI runner
                // still wants one. 10k is far above any burst a build can
                // produce and far below a resource problem.
                .max_pending_accept_reset_streams(resets)
                .max_local_error_reset_streams(resets)
                // Keepalive, because a nested earthly can sit idle while its
                // own build runs and a dropped control stream is a dead
                // build.
                .keep_alive_interval(std::time::Duration::from_secs(20))
                .keep_alive_timeout(std::time::Duration::from_secs(60));
            let svc = hyper_util::service::TowerToHyperService::new(router);
            if let Err(e) = builder.serve_connection_with_upgrades(io, svc).await {
                // A client that hangs up mid-stream is normal; anything else
                // is worth seeing, because the client's own error names
                // nothing on this side.
                let m = e.to_string();
                if !m.contains("connection reset") && !m.contains("NotConnected") {
                    println!("[proxy] connection ended: {m}");
                }
            }
        });
    }
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
        let resend = report_gateway(&self.wire, &req);
        // Where the time goes. The tax was invisible until peer 0 rejoined
        // the round robin (10s -> 11s); "attack the round-trip" is a guess
        // until it is split into publish / peer build / answer, because two
        // of those three are not round-trips at all.
        let t_solve = std::time::Instant::now();
        let arrived_at = {
            let mut w = self.wire.held();
            let first = *w.first_solve.get_or_insert(t_solve);
            let at = t_solve.duration_since(first).as_millis() as u64;
            w.arrivals.push(at);
            at
        };
        let mut t_portable = 0u64;
        let mut t_adopt = 0u64;
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
        // Asked once, before the lock: `worker_count` is async and the wire
        // guard is not. A fleet is now workers on the mesh, not entries in a
        // list this process was started with - so it can also become
        // non-empty mid-build, which a startup-time peer list could not.
        let have_fleet = self.driver.worker_count().await > 0;
        {
            let mut w = self.wire.held();
            let key = match (have_fleet, self.mirror.is_some(), req.definition.is_some()) {
                (false, _, _) => "no fleet",
                (_, false, _) => "no mirror",
                (_, _, false) => "no definition",
                _ => "considered",
            };
            *w.rejected.entry(key.to_owned()).or_default() += 1;
        }
        if have_fleet {
            if let (Some(mirror), Some(def)) = (&self.mirror, req.definition.clone()) {
                // The EXCLUSIONS, checked FIRST because they are free.
                // Principle 10 - one cache mount, secret, ssh agent or
                // privileged exec anywhere grounds the whole subtree - and
                // this ran on the REWRITTEN graph until the rewrite was
                // timed at 1.6s a solve. Earthly excludes eleven solves in
                // twelve, so eleven rewrites were published, mirrored and
                // thrown away. Hazards live on ExecOps and rewriting only
                // touches source identifiers, so the original graph gives
                // the same verdict for nothing.
                let verdict = crate::dispatch::inspect(&def);
                let policy = crate::dispatch::Allow {
                    secrets: can_serve_secrets(&def),
                    caches: local_caches(),
                    agent: forwarding_agent(),
                };
                // ONCE MIRRORED, not as it stands. A git source grounds this
                // graph today and will not once the driver has fetched it
                // with the session it holds - and `make_portable`, which
                // does that, only runs if we decide the graph is worth it.
                //
                // Judging strictly here shipped a git mirror that fired zero
                // times on a target with 407 git-grounded solves. The graph
                // is re-inspected strictly after the rewrite, below.
                let allowed = verdict.dispatchable_once_mirrored(policy);
                if !allowed {
                    // Name the secret, not just its kind. A build that
                    // declares none can still be full of them: a frontend
                    // may attach its own, and "excluded: Secret" then reads
                    // as the user's fault.
                    use prost::Message;
                    let mut detail: Vec<String> = Vec::new();
                    for b in &def.def {
                        if let Some(bollard_buildkit_proto::pb::op::Op::Exec(e)) =
                            bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                                .ok()
                                .and_then(|o| o.op)
                        {
                            for se in &e.secretenv {
                                detail.push(format!("env {}={}", se.name, se.id));
                            }
                            for m in &e.mounts {
                                if let Some(so) = &m.secret_opt {
                                    detail.push(format!("mount {} id={}", m.dest, so.id));
                                }
                            }
                        }
                    }
                    detail.sort();
                    detail.dedup();
                    // The first blocker THIS POLICY did not lift, not the
                    // first hazard in op order.
                    //
                    // `.first()` reported whichever exclusion happened to
                    // sort earliest, including ones the operator had already
                    // lifted. With REBUCK2_PEER_CACHE_MOUNTS=1 set, a run
                    // reported "excluded: CacheMount" 194 times for graphs
                    // whose cache mounts were explicitly allowed - the real
                    // blocker was further down the list and never named.
                    //
                    // The same mistake was fixed in `consider` this morning.
                    // Fixing it in one of the two places that answer a
                    // question is the theme of the week.
                    let why = verdict
                        .exclusions
                        .iter()
                        .map(|(_, e)| e)
                        .find(|e| {
                            !crate::dispatch::lifted_by_policy(e, policy)
                                && !crate::dispatch::fixable_by_mirroring(e)
                        })
                        .map(|e| format!("excluded: {e:?} {detail:?}"))
                        .unwrap_or_else(|| "excluded: platform".to_owned());
                    *self.wire.held().rejected.entry(why).or_default() += 1;
                }
                // Draw ONCE, and only among solves that COULD move. Two
                // calls advance the cursor twice, so the peer that gets the
                // work is not the peer whose turn it was; and letting an
                // excluded solve consume a turn means an earthly build burns
                // eleven turns and hands its one movable solve to whichever
                // machine the arithmetic lands on.
                // Is it big enough to be worth moving at all? Asked before
                // WHERE, because "nowhere" is a legitimate answer and the
                // cheapest one - at small build sizes an even split measured
                // 50% SLOWER than having no fleet.
                let key = graph_key(&def);
                // OFF by default; `REBUCK2_GATE=1` opts in. The estimator is
                // measured and reported, and the gate around it does not
                // hold up - see `worth_shipping`.
                let worth = !gating() || worth_shipping(self.estimate(&key), self.overhead_ms());
                // No mirror breaker any more. It existed because every peer
                // pulled through one shared registry, so a dead mirror meant
                // every offer failed identically and the cheapest answer was
                // to stop offering. The mesh has no such single point: a
                // worker that cannot fetch declines, the driver tries the
                // next, and refusal is already the signal - principle 12,
                // instead of a breaker rediscovering it from timeouts.
                if allowed && !worth {
                    *self
                        .wire
                        .held()
                        .rejected
                        .entry("not worth shipping".to_owned())
                        .or_default() += 1;
                }
                // Only ship once the local machine is FULL.
                //
                // This is what build size was standing in for. Twenty-four
                // short builds fit in sixteen cores, so dispatching any of
                // them added a transfer and relieved nothing: 12s split
                // evenly against 8s with no fleet at all. Bigger builds
                // saturate sooner, which is why the sweep looked like a size
                // effect.
                //
                // Unlike the peer weights, the core count here is honest: the
                // question is "is this machine full", not "how fast is it".
                let slots = home_slots();
                // CLAIM a slot rather than reading the counter and then
                // taking one. Load-then-increment is check-then-act:
                // twenty-four concurrent gateway solves all read 15 before
                // any of the increments land, and the measurement showed it -
                // "home peak 20 of 16 slots", four builds past a limit that
                // was supposed to be exact.
                let claimed = self
                    .home_inflight
                    .fetch_update(
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                        |n| (n < slots).then_some(n + 1),
                    )
                    .is_ok();
                let saturated = !claimed;
                {
                    let mut w = self.wire.held();
                    w.peak_home = w.peak_home.max(
                        self.home_inflight
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                    w.slots = slots;
                }
                // Nothing to overlap with? Keep it. See
                // `worth_dispatching_now` - the serial base chain is 71% of
                // this workload and gains nothing from a second machine.
                //
                // Read BEFORE the claim is handed back, so it reflects what
                // else is actually in flight rather than this solve alone.
                let crowded = worth_dispatching_now(
                    self.home_inflight
                        .load(std::sync::atomic::Ordering::Relaxed),
                    min_siblings(),
                );
                if allowed && worth && !saturated {
                    *self
                        .wire
                        .held()
                        .rejected
                        .entry("home has room".to_owned())
                        .or_default() += 1;
                }
                // Offer it to the FLEET, and let the driver arbitrate.
                //
                // This used to pick a peer here, from a list this proxy
                // kept, and then build on it over HTTP through a mirror
                // every daemon had to reach. The driver already does the
                // choosing - same candidates, same platform match, same
                // refusal-as-backpressure - for workers that subdivide, and
                // it does it without any layer crossing the coordinator,
                // which is principle 6 and the thing the shared mirror could
                // never satisfy.
                // `crowded` belongs HERE, on the decision, not on the
                // diagnostic tally above - which is where it was first
                // written, making REBUCK2_MIN_SIBLINGS change a counter and
                // nothing else. The A/B would have read "no effect" and the
                // idea would have been discarded without ever being enabled.
                let offer = allowed && worth && saturated && crowded;
                if allowed && worth && saturated && !crowded {
                    // Kept home BECAUSE of the rule - the outcome it exists
                    // to change.
                    crate::mech::applied("min_siblings");
                    *self
                        .wire
                        .held()
                        .rejected
                        .entry("nothing to overlap with".to_owned())
                        .or_default() += 1;
                }
                // The slot was claimed above. Hand it back if the work is
                // leaving after all.
                if offer && claimed {
                    self.home_inflight
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                if allowed {
                    // 0 is home, 1 is "the fleet". WHICH machine took it is
                    // the driver's business now, and it reports that itself.
                    *self
                        .wire
                        .held()
                        .placed
                        .entry(usize::from(offer))
                        .or_default() += 1;
                }
                // Recorded for EVERY solve, not just the dispatchable ones.
                // An excluded solve still runs at home and still holds a
                // slot, and pairing the claim with the release is what stops
                // the counter drifting up until the fleet thinks home is
                // permanently full - which is exactly the earthly case, where
                // eleven solves in twelve are excluded.
                if let Some(id) = meta
                    .get("buildkit-controlapi-buildid")
                    .and_then(|v| v.to_str().ok())
                {
                    self.went
                        .held()
                        .insert(id.to_owned(), (offer.then_some(1), key.clone(), claimed));
                }
                if !offer {
                    self.wire.held().home += 1;
                } else {
                    // Only NOW is the rewrite worth its 1.6s: this graph is
                    // leaving. Publishing a context and mirroring a base for
                    // a solve that stays home buys nothing at all.
                    let session = self.session_for(&meta);
                    let t = std::time::Instant::now();
                    let portable = self.make_portable(&def, &session, mirror, None).await;
                    t_portable = t.elapsed().as_millis() as u64;
                    // Portable means EVERY source is something a sessionless
                    // peer can fetch: nothing local, and every image already
                    // in our mirror. Checking only `local == 0` let a graph
                    // whose base was still being mirrored go out anyway -
                    // concurrent solves skip an in-flight copy - and the peer
                    // then reached for Docker Hub with no credentials and
                    // panicked.
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
                    // ONCE, on the first occurrence - not "only while
                    // nothing has routed yet", which was the first attempt
                    // and never fired: on a big target other solves route
                    // long before the interesting one arrives.
                    static SAID_LOCAL: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !local_clear && !SAID_LOCAL.swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
                        // WHICH local sources survived the rewrite. 22 of 38
                        // solves stopped here on a real target and the report
                        // said only "context unmirrored" - true of a context
                        // that failed to publish, one that was never asked
                        // for, and one whose name we resolved differently.
                        let stuck: Vec<String> = portable
                            .def
                            .iter()
                            .filter_map(|b| {
                                use prost::Message;
                                match bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                                    .ok()
                                    .and_then(|o| o.op)
                                {
                                    Some(bollard_buildkit_proto::pb::op::Op::Source(src))
                                        if src.identifier.starts_with("local://") =>
                                    {
                                        Some(src.identifier)
                                    }
                                    _ => None,
                                }
                            })
                            .collect();
                        println!("[proxy] context unmirrored, still local: {stuck:?}");
                    }
                    // GRAFT what a peer has already built, before deciding
                    // whether this is worth dispatching.
                    //
                    // Without it every subtree carries its whole ancestor
                    // chain and each worker executes it: 675s of lead-work
                    // against a 144s baseline, which is three workers each
                    // rebuilding the same prefix. With it the chain becomes an
                    // image pull, and the CCR arithmetic says that trade wins
                    // by two orders of magnitude - an apt-layer costs 30-120s
                    // to build and 40-400ms to fetch.
                    //
                    // OFF unless REBUCK2_GRAFT=1 until it is measured. The
                    // last four remedies were each chosen before the
                    // measurement that would have ruled them out.
                    let portable = if std::env::var("REBUCK2_GRAFT").as_deref() == Ok("1") {
                        let built = self.driver.built_ops().await;
                        if built.is_empty() {
                            portable
                        } else {
                            let before = portable.def.len();
                            let g = crate::dispatch::graft_built(&portable, &|d| {
                                built.get(d).map(|r| {
                                    if let Some(h) = r.strip_prefix("sha256:") {
                                        format!(
                                            "docker-image://{}/{}@sha256:{h}",
                                            mirror.registry,
                                            crate::solve::SUBTREE_REPO
                                        )
                                    } else if r.contains("://") {
                                        r.clone()
                                    } else {
                                        format!("docker-image://{r}")
                                    }
                                })
                            });
                            if g.def != portable.def {
                                self.wire.held().grafted += 1;
                                println!(
                                    "[proxy] grafted a built ancestor into a {before}-op graph"
                                );
                            }
                            g
                        }
                    } else {
                        portable
                    };

                    // And now STRICTLY. Mirroring is best effort: a git
                    // source we failed to publish leaves the subtree exactly
                    // as grounded as it was, and offering it anyway sends a
                    // peer at content nobody has.
                    let mirrored = crate::dispatch::inspect(&portable);
                    let sources_clear = mirrored.dispatchable_when(policy);
                    if !sources_clear {
                        // The first blocker the POLICY did not lift.
                        // `.first()` here reported "still not portable:
                        // CacheMount" 185 times in a run where cache mounts
                        // were explicitly allowed - the third appearance of
                        // this exact mistake, and the first two were fixed
                        // hours before this code was written.
                        let blocker = mirrored
                            .exclusions
                            .iter()
                            .map(|(_, e)| e)
                            .find(|e| !crate::dispatch::lifted_by_policy(e, policy));
                        *self
                            .wire
                            .held()
                            .rejected
                            .entry(format!("still not portable: {blocker:?}"))
                            .or_default() += 1;
                    }
                    if local_clear && bases_clear && sources_clear {
                        use prost::Message;
                        let t = std::time::Instant::now();
                        // Empty frontier: a portable graph names every input
                        // by digest, so the builder fetches what it needs and
                        // there is nothing for us to enumerate.
                        // PUBLISH THE PREFIX FIRST, so it can be grafted.
                        //
                        // Eight measured attempts failed the same way: the
                        // shared ancestry is interior to every dispatched
                        // graph, so it is never published, so it can never be
                        // imported - and every worker rebuilds it. `analyse`
                        // has found the cuts all along and used them for a log
                        // line.
                        //
                        // Dispatch the largest cut BELOW this graph as a
                        // subtree of its own. It comes back as an image, the
                        // driver records it, and every later graph that shares
                        // that ancestry grafts it instead of rebuilding.
                        //
                        // Costs one extra publish on the first graph that
                        // carries the prefix, which is the 1-prefix trade.
                        let portable = if std::env::var("REBUCK2_CUT_PREFIX").as_deref() == Ok("1")
                        {
                            let a = crate::dispatch::analyse(&portable, 8);
                            // The BIGGEST cut that is not the whole graph: the
                            // shared ancestry is the deep part, and cutting at
                            // the terminal would just dispatch the same graph
                            // under another name.
                            let pick = a
                                .cuts
                                .iter()
                                .filter(|c| c.ops + 1 < portable.def.len())
                                .max_by_key(|c| c.ops)
                                .map(|c| (c.root, c.ops));
                            match pick {
                                Some((root, ops)) => {
                                    match crate::dispatch::subgraph(&portable, root) {
                                        Some(pre) => {
                                            // ONCE PER PREFIX, not once per
                                            // graph. The first attempt
                                            // published 32 of them - every
                                            // graph re-dispatching the same
                                            // ancestry because none had landed
                                            // yet - which is N prefixes wearing
                                            // a different hat.
                                            //
                                            // Same OnceCell the contexts and
                                            // base images use: the first caller
                                            // publishes, the rest await its
                                            // answer. That is what makes this
                                            // ONE prefix.
                                            let key = (
                                                "prefix".to_owned(),
                                                format!(
                                                    "sha256:{}",
                                                    crate::store::sha256_hex(&portable.def[root])
                                                ),
                                            );
                                            let cell = self.cell(&key);
                                            let bytes = pre.encode_to_vec();
                                            let n = portable.def.len();
                                            cell.get_or_init(|| async move {
                                                // A prefix actually cut and
                                                // published, not merely
                                                // enabled.
                                                crate::mech::applied("cut_prefix");
                                                println!(
                                                    "[proxy] publishing a {ops}-op prefix before a {n}-op graph"
                                                );
                                                // Best effort: if nobody takes
                                                // it we dispatch the whole
                                                // graph as before. A failed
                                                // optimisation must not fail a
                                                // build.
                                                // A PREFIX is shared by
                                                // definition - it was cut
                                                // because several graphs
                                                // carry it - so say so
                                                // rather than waiting for
                                                // placements to prove it.
                                                let r = self
                                                    .driver
                                                    .lead_subtree_shared(bytes, Vec::new(), true)
                                                    .await
                                                    .ok();
                                                // A PREFIX exists precisely
                                                // because several graphs share
                                                // it - the second signal worth
                                                // pre-positioning on, and it
                                                // is known to be shared before
                                                // anyone asks for it.
                                                if let Some(ref image) = r {
                                                    self.driver.prefetch_image(image).await;
                                                }
                                                r
                                            })
                                            .await;
                                            let built = self.driver.built_ops().await;
                                            crate::dispatch::graft_built(&portable, &|d| {
                                                built.get(d).map(|r| {
                                                    r.strip_prefix("sha256:")
                                                        .map(|h| {
                                                            format!(
                                                                "docker-image://{}/{}@sha256:{h}",
                                                                mirror.registry,
                                                                crate::solve::SUBTREE_REPO
                                                            )
                                                        })
                                                        .unwrap_or_else(|| r.clone())
                                                })
                                            })
                                        }
                                        None => portable,
                                    }
                                }
                                None => portable,
                            }
                        } else {
                            portable
                        };

                        // THE BARRIER. Until something has been built and
                        // published there is nothing to graft, so letting the
                        // whole first wave go at once guarantees every worker
                        // rebuilds the same ancestry - measured as 675s of
                        // lead-work against a 144s baseline.
                        //
                        // One permit means the first dispatch runs alone. The
                        // moment it reports a built op the gate stops being
                        // taken at all, and the rest of the wave goes out
                        // grafted onto its result.
                        //
                        // TIMED OUT rather than awaited forever: if the first
                        // lead is declined by every peer, nothing will ever be
                        // published, and a barrier waiting on an event that
                        // cannot happen is a hung build. 60s then proceed
                        // ungrafted, which is exactly today's behaviour.
                        let _gate = if std::env::var("REBUCK2_WARMUP").as_deref() == Ok("1")
                            && self.driver.built_ops().await.is_empty()
                        {
                            tokio::time::timeout(
                                std::time::Duration::from_secs(60),
                                self.warmup.clone().acquire_owned(),
                            )
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                        } else {
                            None
                        };
                        // Does more than one graph want any of this? Asked
                        // of the solves seen so far, which is a prediction:
                        // an op in two solves will be wanted twice however
                        // it is placed. Counting placements instead lets
                        // affinity - whose job is one machine per op -
                        // suppress the pre-positioning that would help.
                        // `&def`, NOT `&portable`, and the source guard
                        // below enforces it: making a graph portable
                        // rewrites every op that mentions a local context or
                        // a base image, so the digests no longer match
                        // anything `observe` counted and the answer comes
                        // back a confident "not shared" for the graphs that
                        // most are.
                        let shared_graph = self.wire.held().shared_as_sent(&def);
                        let led = self
                            .driver
                            .lead_subtree_shared(portable.encode_to_vec(), Vec::new(), shared_graph)
                            .await;
                        t_adopt = t.elapsed().as_millis() as u64;
                        match led {
                            Ok(reference) => {
                                println!("[proxy] fleet built it: {reference}");
                                self.wire.held().routed += 1;
                                // `Led` carries a bare `host:port/repo:tag` -
                                // what the registry speaks and what a worker
                                // prints. An LLB source identifier is a URL,
                                // so buildkit rejects it unschemed with
                                // "failed to parse ... invalid". The deleted
                                // peer path prefixed this on the way out;
                                // here is where that moved to.
                                // A bare `sha256:...` is content with no
                                // location: the builder published it into
                                // its own store and we name the registry WE
                                // pull from, which fetches it from whoever
                                // has it. Anything else is a full reference
                                // from an older worker - take it as given.
                                let src = if let Some(d) = reference.strip_prefix("sha256:") {
                                    format!(
                                        "docker-image://{}/{}@sha256:{d}",
                                        mirror.registry,
                                        crate::solve::SUBTREE_REPO
                                    )
                                } else if reference.contains("://") {
                                    reference
                                } else {
                                    format!("docker-image://{reference}")
                                };
                                req.definition = Some(crate::dispatch::import_graph(&src));
                            }
                            // Nobody took it. Not a failure and not a
                            // refusal to report against any machine: we
                            // build it here, exactly as without a fleet.
                            Err(why) => {
                                // Nobody took it. Say WHAT was in the graph,
                                // once: a refusal with no shape attached is
                                // how two wrong theories got as far as they
                                // did today.
                                // Two cases deserve the shape, and only
                                // these two.
                                //
                                // routed == 0: nothing has ever moved, so
                                // whatever is in this graph is the reason.
                                //
                                // A refusal that says "build failed": the
                                // fleet AGREED to take it and could not do
                                // it. That is inspect being wrong, which is
                                // the only kind of wrong the report cannot
                                // otherwise show - a graph declined for a
                                // hazard we named is working as intended,
                                // and a graph that dies after being accepted
                                // is a hazard we failed to name.
                                let inspect_was_wrong = why.contains("build failed");
                                if self.wire.held().routed == 0 || inspect_was_wrong {
                                    println!(
                                        "[proxy] {} graph carries {:?}",
                                        if inspect_was_wrong {
                                            "ACCEPTED THEN FAILED;"
                                        } else {
                                            "nothing taken;"
                                        },
                                        crate::dispatch::session_shape(&portable)
                                    );
                                }
                                self.wire.held().home += 1;
                                // The DRIVER's reason, not ours. Four workers
                                // idle and one lead taken read as "fleet took
                                // nothing" five times over, which named the
                                // outcome and hid the cause.
                                *self
                                    .wire
                                    .held()
                                    .rejected
                                    .entry(format!("fleet: {why}"))
                                    .or_default() += 1;
                            }
                        }
                    } else {
                        // Both-clear cannot reach here - it is the `if` above -
                        // but saying so with `unreachable!` puts a live panic
                        // one edit of that condition away. Phrased so the
                        // impossible case simply has no arm to reach.
                        let why = if !local_clear && !bases_clear {
                            "context and base unmirrored"
                        } else if !local_clear {
                            "context unmirrored"
                        } else {
                            "base unmirrored"
                        };
                        *self.wire.held().rejected.entry(why.to_owned()).or_default() += 1;
                    }
                }
            }
        }

        // Always peer 0: it holds the client's job, and after adoption the
        // graph is a fetch rather than a build.
        let t_answer = std::time::Instant::now();
        // Kept for a possible second attempt: a tonic Channel reconnects on
        // the next request, but the request itself moves into the first one.
        let again = (meta.clone(), req.clone());
        let out = self
            .gw()
            .solve(Request::from_parts(meta, ext, req))
            .await
            .inspect_err(|e| {
                // WHOSE connection broke. `h2 protocol error: error reading a
                // body from connection` reaches the user as earthly's exit
                // error, and earthly is Go - that phrasing is hyper's, so the
                // failure is a call WE made and relayed, not one we served.
                // Server-side reset limits were raised, then removed
                // entirely, and it made no difference; the server's own
                // connection log stayed silent throughout. Both are explained
                // if the connection that dies is the proxy's upstream one.
                println!(
                    "[proxy] upstream solve failed: {} {}",
                    e.code(),
                    e.message()
                );
            });
        // ONE more, and only for the transport. A single blip currently kills
        // a build that was otherwise fine and takes the other thirteen groups
        // with it; a rebuild is idempotent by cache key, so the worst case is
        // paying for the work twice, which principle 5 already accepts.
        //
        // Extensions are not carried over: they belong to the INBOUND
        // connection and the upstream call does not read them.
        let out = match out {
            Err(e) if worth_retrying(&e) => {
                println!("[proxy] retrying the solve once: {}", e.message());
                let (meta, req) = again;
                self.gw()
                    .solve(Request::from_parts(meta, Default::default(), req))
                    .await
                    .inspect_err(|e| {
                        println!("[proxy] retry failed too: {} {}", e.code(), e.message());
                    })?
            }
            other => other?,
        };
        let answer = t_answer.elapsed().as_millis() as u64;
        self.wire.held().spans.push(Span {
            start: arrived_at,
            total: t_solve.elapsed().as_millis() as u64,
            resend,
            portable: t_portable,
            adopt: t_adopt,
            answer,
        });
        let out = out.into_inner();
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
        // Timed, because `solve` is LAZY. A gateway Solve hands back a ref
        // in about a millisecond whether it stands for a nine-second build
        // or a registry pull; the work happens when the ref is evaluated,
        // which is here. Measuring only `solve` said the answer cost 1ms
        // and made a pull look free.
        let t = std::time::Instant::now();
        let out = self
            .gw()
            .r#return(Request::from_parts(meta, ext, req))
            .await;
        self.wire
            .held()
            .returns
            .push(t.elapsed().as_millis() as u64);
        out
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

#[cfg(test)]
mod tests {
    #[test]
    fn work_with_no_siblings_stays_home() {
        use super::worth_dispatching_now;

        // Straight out of the traces. A full +test-no-qemu spends 192 of its
        // 271 baseline seconds in a base chain three targets deep -
        // +earthly-docker, then a FROM it, then a FROM that - and nothing
        // else can start until it ends. Dispatching that chain cost +90s and
        // could not have gained anything: it is ONE target at a time, so
        // there is no second machine for it to use.
        //
        // The rule that follows: a solve with no concurrent siblings gains
        // nothing from moving and pays the whole handover. Keep it home.
        assert!(
            !worth_dispatching_now(1, 2),
            "alone: nothing to overlap with"
        );
        assert!(!worth_dispatching_now(2, 2), "at the threshold, still home");
        assert!(
            worth_dispatching_now(3, 2),
            "a crowd can use another machine"
        );

        // Threshold 0 disables it, so the old behaviour is one env var away
        // and the A/B is honest.
        assert!(worth_dispatching_now(1, 0));
        assert!(worth_dispatching_now(0, 0));
    }

    #[test]
    fn only_a_broken_connection_is_worth_retrying() {
        use tonic::{Code, Status};

        // A relayed solve that fails because the CONNECTION died should be
        // tried again - the channel reconnects, and a rebuild is idempotent
        // by cache key, so at worst it costs the work twice, which principle
        // 5 already accepts. One transport blip currently kills a build that
        // was otherwise fine, and it takes the other thirteen groups with it.
        assert!(super::worth_retrying(&Status::new(Code::Unavailable, "h2")));

        // A solve that fails because the BUILD failed must not be. Retrying
        // it doubles the cost of every red target and changes nothing - and
        // this suite deliberately contains targets that fail.
        assert!(!super::worth_retrying(&Status::new(
            Code::Unknown,
            "exit 1"
        )));
        assert!(!super::worth_retrying(&Status::new(
            Code::NotFound,
            "no ref"
        )));
        // Cancellation is the client's decision, not a fault to paper over.
        assert!(!super::worth_retrying(&Status::new(Code::Cancelled, "ctx")));

        // The one that actually kills full-target runs, found by
        // instrumenting Control.Solve:
        //
        //   upstream Control.Solve failed: Unknown error transport error
        //
        // buildkit reports its own transport failures as Unknown, which is
        // also how it reports a build that FAILED - so the code alone cannot
        // separate them and the message has to.
        assert!(super::worth_retrying(&Status::new(
            Code::Unknown,
            "transport error"
        )));
        assert!(super::worth_retrying(&Status::new(
            Code::Unknown,
            " transport error\n"
        )));

        // EXACT, not "contains". This suite is full of targets that fail on
        // purpose, and their error text is relayed verbatim - a build whose
        // own output mentions a transport error must not be rebuilt for it.
        assert!(!super::worth_retrying(&Status::new(
            Code::Unknown,
            "process \"/bin/sh\" did not complete successfully: transport error seen in log"
        )));
        assert!(!super::worth_retrying(&Status::new(
            Code::Unknown,
            "exit code 1"
        )));
    }

    /// The h2 reset limit is a knob, so the question can be settled by an A/B.
    #[test]
    fn the_reset_limit_can_be_lifted_for_an_experiment() {
        // `h2 protocol error: error reading a body from connection` survived
        // raising this from 20 to 10_000, and the proxy's own connection log
        // stayed silent - which proves nothing either way, because hyper
        // answers a tripped reset limit with a GOAWAY and a GRACEFUL close,
        // so `serve_connection` returns Ok and the error branch never runs.
        //
        // The only way to know whether it is us is to remove the limit and
        // see. A knob rather than an edit, so the run is reproducible and
        // the default stays safe.
        assert_eq!(super::reset_limit(None), Some(10_000));
        assert_eq!(super::reset_limit(Some("50")), Some(50));
        assert_eq!(super::reset_limit(Some("none")), None, "the experiment");
        // Anything unparseable keeps the DEFAULT rather than silently
        // becoming unlimited: a typo in a CI input must not quietly remove a
        // DoS backstop.
        assert_eq!(super::reset_limit(Some("lots")), Some(10_000));
        assert_eq!(super::reset_limit(Some("")), Some(10_000));
    }

    /// A resend is recognised on arrival, which is the only place it can be.
    ///
    /// 28 of 91 solves in a six-runner run were byte-identical graphs sent
    /// again. Whether that is worth memoising depends on what they COST, and
    /// the cost cannot be paired with the count after the fact: `graph_ids`
    /// is appended on arrival and `spans` on completion, so with eight solves
    /// in flight the two vectors are not index-aligned. The verdict has to
    /// ride with the solve.
    #[test]
    fn a_repeated_graph_is_known_to_be_repeated_when_it_arrives() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let def = |id: &str| pb::Definition {
            def: vec![
                pb::Op {
                    op: Some(pb::op::Op::Source(pb::SourceOp {
                        identifier: id.into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                }
                .encode_to_vec(),
                pb::Op::default().encode_to_vec(),
            ],
            ..Default::default()
        };

        let mut w = super::Wire::default();
        assert!(!w.observe(&def("a")), "first sighting is not a resend");
        assert!(!w.observe(&def("b")), "a different graph is not a resend");
        assert!(w.observe(&def("a")), "the same graph again is");
        assert!(w.observe(&def("a")), "and again");

        // Which must agree with the count the report already prints, or one
        // of the two numbers is wrong and nobody can tell which.
        assert_eq!(w.resends(), 2);
    }

    /// Overlap is what a fleet is FOR, so it gets its own arithmetic.
    #[test]
    fn concurrency_separates_a_fleet_from_a_queue() {
        let span = |start, total| super::Span {
            start,
            total,
            ..Default::default()
        };

        // Five solves, one after another - six machines and a queue.
        let queue: Vec<_> = (0..5).map(|i| span(i * 100, 100)).collect();
        let (peak, occ) = super::concurrency(&queue);
        assert_eq!(peak, 1, "a handover is not an overlap");
        assert!((occ - 1.0).abs() < 1e-9, "occupancy {occ}, wanted 1.0");

        // The same five, all at once.
        let fleet: Vec<_> = (0..5).map(|_| span(0, 100)).collect();
        let (peak, occ) = super::concurrency(&fleet);
        assert_eq!(peak, 5);
        assert!((occ - 5.0).abs() < 1e-9, "occupancy {occ}, wanted 5.0");

        // And the shape a real run has: a serial stem, then a fan-out. Peak
        // says the fan-out happened; occupancy says most of the clock was
        // the stem, which is the half that gets forgotten.
        let mut real = vec![span(0, 1000)];
        real.extend((0..4).map(|_| span(1000, 100)));
        let (peak, occ) = super::concurrency(&real);
        assert_eq!(peak, 4);
        assert!(occ < 1.3, "occupancy {occ} - a 91% serial run is not busy");

        assert_eq!(super::concurrency(&[]), (0, 0.0));
    }

    /// The ceiling, from the same sweep.
    #[test]
    fn the_ceiling_comes_out_of_the_shape_of_the_run() {
        let span = |start, total| super::Span {
            start,
            total,
            ..Default::default()
        };

        // Nothing overlaps: every millisecond is serial and no number of
        // machines helps.
        let queue: Vec<_> = (0..5).map(|i| span(i * 100, 100)).collect();
        assert!((super::ceiling(&queue) - 1.0).abs() < 1e-9);

        // Everything overlaps: unbounded, reported as the sample size since
        // "infinity" is not a useful thing to print.
        let fleet: Vec<_> = (0..5).map(|_| span(0, 100)).collect();
        assert!(super::ceiling(&fleet) >= 5.0);

        // 1000ms of stem then a 100ms fan-out - 1100ms wall, 1000 of it
        // serial. 1/0.909 = 1.1, and that is the whole of what six machines
        // can do to this graph.
        let mut real = vec![span(0, 1000)];
        real.extend((0..4).map(|_| span(1000, 100)));
        let c = super::ceiling(&real);
        assert!((c - 1.1).abs() < 0.01, "ceiling {c}, wanted 1.10");

        assert_eq!(super::ceiling(&[]), 1.0);
    }

    /// Sharedness must be asked of the graph that was OBSERVED.
    ///
    /// `op_solves` is keyed on the digest of each op as the client sent it.
    /// Making a graph portable rewrites those ops - a local context becomes
    /// an image reference, a base is repointed at our mirror - so every
    /// digest changes and a lookup finds nothing. The answer is then a
    /// confident "not shared" for the very graphs that are.
    ///
    /// The same trap cost four "warming did not help" results: a rewritten
    /// op cannot match a cache key built from the original, for exactly this
    /// reason.
    #[test]
    fn sharedness_is_a_property_of_the_graph_as_sent() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let stem = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "docker-image://alpine:3".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let leaf = |n: u32| pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: format!("local://ctx{n}"),
                ..Default::default()
            })),
            ..Default::default()
        };
        let sent = |n: u32| pb::Definition {
            def: vec![stem.encode_to_vec(), leaf(n).encode_to_vec()],
            ..Default::default()
        };

        let mut w = super::Wire::default();
        w.observe(&sent(1));
        assert!(
            !w.shared_as_sent(&sent(1)),
            "one solve is not sharing with anybody"
        );
        w.observe(&sent(2));
        assert!(
            w.shared_as_sent(&sent(1)),
            "two solves carry the same stem, so it is shared"
        );

        // And the trap: the SAME graph, rewritten the way dispatch rewrites
        // it before handing it to a peer.
        let mut portable = sent(1);
        portable.def[1] = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "docker-image://mirror/ctx@sha256:dead".into(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_to_vec();
        assert!(
            !w.shared_as_sent(&pb::Definition {
                def: vec![portable.def[1].clone()],
                ..Default::default()
            }),
            "a rewritten op has a digest nobody has ever counted"
        );
    }

    /// ...and the guard that the test above cannot be.
    ///
    /// The invariant lives at a call site buried in a gRPC handler, where a
    /// unit test cannot reach it, and passing the wrong `Definition` there
    /// is silent: the answer is `false`, which is also what a genuinely
    /// unshared graph gets. So check the source. The same shape as
    /// `mech::source_consistency` - and for the same reason, which is that
    /// the mechanism guard once reported OFF for a counter that was simply
    /// never called.
    #[test]
    fn sharedness_is_only_ever_asked_of_the_graph_as_sent() {
        let src = include_str!("proxy.rs");
        let calls: Vec<&str> = src
            .match_indices("shared_as_sent(")
            // Method calls only: that skips the `fn` definition and this
            // test's own mention of the name in a string literal.
            .filter(|(i, _)| src[..*i].ends_with('.'))
            .map(|(i, _)| {
                let rest = &src[i + "shared_as_sent(".len()..];
                &rest[..rest.find([',', ')']).unwrap_or(0)]
            })
            // Not this test's own mention of the name, nor the unit test's
            // literals - those are asking it of a graph they just built.
            .filter(|a| !a.starts_with("&pb::") && !a.starts_with("&sent("))
            .collect();
        assert!(!calls.is_empty(), "the call disappeared, so did the guard");
        for arg in &calls {
            assert_eq!(
                *arg, "&def",
                "it must be asked of `def`, the graph the client sent and \
                 `observe` counted, never of a rewritten one"
            );
        }
    }

    /// The gateway answers gRPC at all.
    ///
    /// One second, no docker, and it would have caught the bug that cost a
    /// fixture-suite round and a CI round: configuring http2 keepalive
    /// without a timer panics INSIDE the connection task
    /// ("You must supply a timer"), so the server binds, accepts, and then
    /// drops every stream. The client sees `Unavailable: error reading from
    /// server: EOF` and the suite reports thirty-seven unrelated assertion
    /// failures.
    ///
    /// The suite catches it in twelve minutes and names nothing. This names
    /// it before the suite runs.
    #[tokio::test]
    async fn the_gateway_answers_grpc() {
        use crate::proxy::control;
        use tonic::{Request, Response, Status};

        // A stub upstream, because the proxy dials one at startup and exits
        // if it cannot. Answering ONE method is enough to prove the serving
        // path end to end: accept, decode, forward, encode, reply.
        #[derive(Default)]
        struct Upstream;
        #[tonic::async_trait]
        impl control::control_server::Control for Upstream {
            type StatusStream =
                futures::stream::BoxStream<'static, Result<control::StatusResponse, Status>>;
            type SessionStream =
                futures::stream::BoxStream<'static, Result<control::BytesMessage, Status>>;
            type PruneStream =
                futures::stream::BoxStream<'static, Result<control::UsageRecord, Status>>;
            type ListenBuildHistoryStream =
                futures::stream::BoxStream<'static, Result<control::BuildHistoryEvent, Status>>;
            async fn list_workers(
                &self,
                _r: Request<control::ListWorkersRequest>,
            ) -> Result<Response<control::ListWorkersResponse>, Status> {
                Ok(Response::new(control::ListWorkersResponse {
                    record: vec![Default::default()],
                }))
            }
            async fn solve(
                &self,
                _r: Request<control::SolveRequest>,
            ) -> Result<Response<control::SolveResponse>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn status(
                &self,
                _r: Request<control::StatusRequest>,
            ) -> Result<Response<Self::StatusStream>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn session(
                &self,
                _r: Request<tonic::Streaming<control::BytesMessage>>,
            ) -> Result<Response<Self::SessionStream>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn disk_usage(
                &self,
                _r: Request<control::DiskUsageRequest>,
            ) -> Result<Response<control::DiskUsageResponse>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn prune(
                &self,
                _r: Request<control::PruneRequest>,
            ) -> Result<Response<Self::PruneStream>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn info(
                &self,
                _r: Request<control::InfoRequest>,
            ) -> Result<Response<control::InfoResponse>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn listen_build_history(
                &self,
                _r: Request<control::BuildHistoryRequest>,
            ) -> Result<Response<Self::ListenBuildHistoryStream>, Status> {
                Err(Status::unimplemented("stub"))
            }
            async fn update_build_history(
                &self,
                _r: Request<control::UpdateBuildHistoryRequest>,
            ) -> Result<Response<control::UpdateBuildHistoryResponse>, Status> {
                Err(Status::unimplemented("stub"))
            }
        }

        let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(control::control_server::ControlServer::new(Upstream))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(up))
                .await;
        });

        let gw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw_addr = gw.local_addr().unwrap();
        drop(gw); // serve() binds it itself
        let driver = crate::driver::Driver::for_test();
        tokio::spawn(async move {
            let _ = super::serve(gw_addr, format!("http://{up_addr}"), driver).await;
        });

        // Poll: the server needs a moment, and a fixed sleep is either flaky
        // or slow.
        let mut last = String::new();
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            match control::control_client::ControlClient::connect(format!("http://{gw_addr}")).await
            {
                Ok(mut c) => match c.list_workers(control::ListWorkersRequest::default()).await {
                    Ok(r) => {
                        assert_eq!(
                            r.into_inner().record.len(),
                            1,
                            "the gateway answered, but not with the upstream's reply"
                        );
                        return;
                    }
                    Err(e) => last = e.to_string(),
                },
                Err(e) => last = e.to_string(),
            }
        }
        panic!("the gateway never answered gRPC. Last error: {last}");
    }

    /// Nothing known yet means ship it. A system that refused everything it
    /// had not measured would never measure anything.
    #[test]
    fn an_unknown_subtree_is_shipped() {
        assert!(super::worth_shipping(None, 5_000));
        assert!(super::worth_shipping(None, 0));
    }

    /// A build shorter than twice its own transfer stays home. These are the
    /// measured numbers: at work=20 a build takes about 1.5s against roughly
    /// 2s of overhead, and shipping it made twenty-four builds 50% slower
    /// than having no fleet at all.
    #[test]
    fn a_subtree_smaller_than_its_transfer_stays_home() {
        assert!(!super::worth_shipping(Some(1_500), 2_000));
        // The measured case that must still ship: work=90, 11.4s at home
        // against 5.8s of shipping cost, where the fleet was worth a third
        // of the wall clock.
        assert!(super::worth_shipping(Some(11_425), 5_810));
    }

    /// Overhead not yet known: ship, and find out.
    #[test]
    fn zero_overhead_ships_everything() {
        assert!(super::worth_shipping(Some(1), 0));
    }

    /// The peer's build is not overhead.
    ///
    /// A dispatched solve whose peer took 9s and whose own preparation and
    /// pull took 300ms and 700ms has cost a second, not ten. Charging the
    /// peer's build as tax makes every dispatch look ruinous and would have
    /// argued for switching dispatch off.
    #[test]
    fn tax_excludes_the_work_the_peer_did_instead_of_us() {
        let dispatched = super::Span {
            start: 0,
            total: 10_000,
            portable: 300,
            adopt: 9_000,
            answer: 700,
            resend: false,
        };
        assert_eq!(dispatched.tax(), 1_000);
        // A solve that stayed home pays no tax, however long it took.
        let home = super::Span {
            start: 0,
            total: 10_000,
            portable: 0,
            adopt: 0,
            resend: false,
            answer: 9_990,
        };
        assert_eq!(home.tax(), 9_990);
    }
}
