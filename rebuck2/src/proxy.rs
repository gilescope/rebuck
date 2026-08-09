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

/// One upstream daemon.
#[derive(Clone)]
pub struct Peer {
    /// Relative share of work, declared by the operator as `url*N`. Buildkit
    /// does not report capacity - `ListWorkers` gives platforms, snapshotter,
    /// executor and gc policy, and nothing about cores - so this is the only
    /// place it can come from.
    weight: usize,
    /// What this daemon says it can run, in ITS order - first is native.
    /// Empty if it would not say (a daemon that cannot answer ListWorkers is
    /// still usable; it is simply never preferred as native).
    platforms: Vec<String>,
    pub addr: String,
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
    published: Published,
    /// Extra daemons this proxy may route work to. Peer 0 is always the
    /// upstream above - the one holding the client's session.
    peers: std::sync::Arc<Vec<Peer>>,
    /// build id -> where its work went. Written by the gateway solve, read by
    /// `Control.Solve` when it finishes, because only the gateway knows the
    /// placement and only Control knows what the client actually waited.
    went: Went,
    /// When to start offering work again after the shared mirror was found
    /// dead. `None` means it is believed healthy.
    ///
    /// Per-peer memory cannot express this: no peer is at fault, and every
    /// peer is unusable. Measured with the registry killed - the healthy peer
    /// was offered eight more doomed adoptions in the next round, each
    /// waiting out a push it could never complete.
    mirror_down_until: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Builds currently running at home, so dispatch can wait until the
    /// local machine is actually full. Incremented when a solve is placed
    /// home, decremented when the client's `Control.Solve` for it returns -
    /// the gateway solve is lazy and returns in a millisecond, so it cannot
    /// mark the end of anything.
    home_inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// graph key -> observed client-visible durations. The estimate that
    /// decides whether a subtree is worth shipping.
    seen_ms: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u64>>>>,
    /// What completed adoptions have cost, in ms. The basis for calling one
    /// slow - see `hedge_after`.
    adopted_ms: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
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
    /// How many times each peer has been taken back from, indexed by peer.
    ///
    /// Never reset. Within one proxy's life a machine that was four times
    /// slower than the fleet stays four times slower; forgetting means
    /// rediscovering it once per solve, at the cost of the bound each time.
    strikes: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>>,
    /// Adoptions currently in flight on each AWAY peer, indexed by peer.
    ///
    /// Index 0 is unused and always zero: peer 0 never adopts. Kept aligned
    /// with `peers` so a peer index means the same thing everywhere - an
    /// off-by-one here would silently overload one machine and starve
    /// another, which looks like a slow fleet, not a bug.
    outstanding: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>>,
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
            adopted_ms: Default::default(),
            solo_ms: Default::default(),
            outstanding: Default::default(),
            strikes: Default::default(),
            went: Default::default(),
            seen_ms: Default::default(),
            home_inflight: Default::default(),
            mirror_down_until: Default::default(),
            next_peer: Default::default(),
        })
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    /// Has the mirror been found dead recently enough to stop trying?
    ///
    /// Optimistic on expiry: the state simply clears and the next solve is
    /// offered normally. If the mirror is still dead that solve refuses,
    /// re-probes and re-arms - one wasted adoption per cooldown instead of
    /// one per solve, and no background polling of a thing nobody is using.
    fn mirror_believed_down(&self) -> bool {
        /// Long enough that a restart is not hammered, short enough that a
        /// recovered mirror is back in service within one build.
        const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(15);
        let mut g = self.mirror_down_until.held();
        match *g {
            Some(t) if t.elapsed() < COOLDOWN => true,
            Some(_) => {
                *g = None;
                false
            }
            None => false,
        }
    }

    /// Is the shared mirror answering?
    ///
    /// The discriminator between "this peer is bad" and "the thing between us
    /// is bad". Killing the registry mid-build produced eight refusals and
    /// struck a machine that had done nothing wrong: the two ends of a
    /// two-party protocol cannot tell a third party's failure apart from each
    /// other's without asking someone.
    ///
    /// `/v2/` is the registry API root, and any answer at all - including a
    /// 401 - means something is listening. Only a transport failure counts as
    /// dead, so a mirror that is up but unhappy still leaves the peer
    /// accountable.
    async fn mirror_alive(&self) -> bool {
        let Some(m) = &self.mirror else {
            return false;
        };
        // The mirror is named for the DAEMONS, and `host.docker.internal` is
        // a name only a container has - the proxy runs on the host, where it
        // does not resolve. Probing it unresolved answered "dead" every time,
        // so no peer was ever blamed for anything, including its own death.
        // The substitution is exact rather than clever: that hostname means
        // "the machine this proxy is on", so loopback is the same place.
        let addr = m.registry.replace("host.docker.internal", "127.0.0.1");
        let url = format!("http://{addr}/v2/");
        match reqwest::Client::new()
            .get(&url)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
        {
            Ok(_) => true,
            Err(e) => {
                println!("[proxy] mirror {addr} is not answering: {e}");
                *self.mirror_down_until.held() = Some(std::time::Instant::now());
                false
            }
        }
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

    /// Whose turn it is next, over the WHOLE fleet.
    /// `None` means build it here. See `place`.
    fn next_place(&self, want: &crate::dispatch::Platform, home_allowed: bool) -> Option<usize> {
        let cursor = self
            .next_peer
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let read = |v: &[std::sync::atomic::AtomicUsize]| -> Vec<usize> {
            v.iter()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                .collect()
        };
        // An unpinned graph is NOT native everywhere, however much it looks
        // it. Measured across two real machines: an arm64 host mirrors
        // `alpine:3.20` and gets the arm64 image, because that is what
        // resolving it here means. The portable graph then names a
        // single-architecture base, and an x86 peer that pulls it dies with
        // `exit code: 255` - a binary it cannot run - after six seconds of
        // pulling. Fail-open recovered every build, so the only cost was time
        // and a frightening log line.
        //
        // That was true while the mirror held one architecture, and it is not
        // true now: the base is mirrored FOR THE PEER (`make_portable`'s
        // `target`), so an unpinned graph is native on every peer - each
        // builds for itself against a base fetched for itself. The fallback
        // to peer 0's architecture stays deleted rather than commented out;
        // the reason it existed is gone.
        let pinned = match want {
            crate::dispatch::Platform::Pinned(p) => Some(p.clone()),
            crate::dispatch::Platform::Any => None,
            crate::dispatch::Platform::Conflict(_) => None,
        };
        let native: Vec<bool> = match &pinned {
            Some(p) => self.peers[1..]
                .iter()
                .map(|peer| native_for(&peer.platforms, p))
                .collect(),
            None => vec![true; self.peers.len().saturating_sub(1)],
        };
        // Declared weights are the starting point; observation overrides them
        // once there is any. Deliberately not the other way round - a
        // declaration is a guess about hardware, and the guess measured
        // WORSE than a flat split.
        let mut weights: Vec<usize> = self.peers.iter().map(|p| p.weight).collect();
        // OFF by default. `REBUCK2_ADAPT=1` opts in.
        //
        // The law is right and this implementation of it is not. Service time
        // is ENDOGENOUS: the thing being measured is caused by the thing being
        // set. Load the home side and its mean rises, the ratio falls below
        // one, the controller reads that as "home is the straggler" and sends
        // MORE work away, which raises the away mean, and so on. Two runs of
        // the same three rounds:
        //
        // ```text
        // adapt   24 builds x3   18 14 14s   placed 40:32
        // adapt   same again     24 17 24s   placed 34:38   <- drifted away
        // pinned  control        18 15 16s   placed 36:36
        // pinned  control        18 15 16s   placed 36:36
        // ```
        //
        // The control also shows where most of the round-two gain comes from,
        // and it is not placement: the peers warm their own caches, and a
        // self-tuner left switched on would have taken the credit.
        let adapt = std::env::var("REBUCK2_ADAPT").as_deref() == Ok("1");
        if adapt && weights.len() == 2 {
            // Two-machine fleets only, for now: with more peers the balance
            // condition is a system of equations, not a ratio, and shipping
            // the two-peer arithmetic as though it generalised would be the
            // same error as guessing from cores.
            let w = self.wire.held();
            let mean = |v: &[u64]| -> u64 {
                if v.len() < MIN_SAMPLES {
                    0
                } else {
                    v.iter().sum::<u64>() / v.len() as u64
                }
            };
            let (h, a) = (mean(&w.home_ms), mean(&w.away_ms));
            drop(w);
            if h > 0 && a > 0 {
                let (wh, wa) = derive_weights(h, a, WEIGHT_CAP);
                weights = vec![wh, wa];
            }
        }
        place(
            cursor,
            home_allowed,
            &weights,
            &read(&self.outstanding[1..]),
            &read(&self.strikes[1..]),
            &native,
        )
    }

    /// Hold a peer responsible: deprioritise it for the rest of the run, and
    /// say so in the report.
    ///
    /// Both together, always. The counter that steers placement and the
    /// counter an operator reads used to be set in different places, so a
    /// peer could be quietly demoted with nothing in the report to explain
    /// why the fleet had gone lopsided.
    fn strike(&self, peer: usize) {
        self.strikes[peer].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.wire.held().struck.entry(peer).or_default() += 1;
    }

    /// Wait for a peer to build it - but not forever, once the fleet has said
    /// what "forever" means.
    ///
    /// Measured: three daemons, six equal builds, one peer held to a quarter
    /// of a CPU. It got its fair share and the whole build waited 164s
    /// instead of 12s. No placement rule fixes that, because the placement
    /// was correct on the information available - the peer was idle. The
    /// information only arrives afterwards, when a normal adoption finishes
    /// and the straggler does not.
    ///
    /// So the deadline is re-read while waiting rather than fixed at the
    /// start: the first completed adoption anywhere in the fleet is what
    /// makes the straggler measurably abnormal. Abandoning it returns
    /// `Err`, and the caller's existing fail-open path then builds at home -
    /// principle 5. The peer is not cancelled and its push is not wasted if
    /// it lands: the tag is the digest of the graph, so whoever gets there
    /// first wins (principle 3) and a later identical push is a no-op.
    async fn adopt_or_take_back(
        &self,
        peer: usize,
        addr: &str,
        registry: &str,
        portable: &bollard_buildkit_proto::pb::Definition,
        started: std::time::Instant,
    ) -> Adoption {
        /// How often to re-ask whether this has become abnormal. Coarse on
        /// purpose: the answer changes on the scale of whole builds.
        const POLL: std::time::Duration = std::time::Duration::from_millis(250);
        let mut fut = Box::pin(crate::solve::build_and_publish(
            addr,
            registry,
            portable.clone(),
            serving_secrets(),
            forwarding_agent(),
        ));
        loop {
            tokio::select! {
                r = &mut fut => return match r {
                    Ok(reference) => Adoption::Done(reference),
                    Err(e) => Adoption::Refused(e),
                },
                _ = tokio::time::sleep(POLL) => {
                    let observed = self.adopted_ms.held().clone();
                    let Some(limit) = hedge_after(&observed) else { continue };
                    if started.elapsed() <= limit {
                        continue;
                    }
                    println!(
                        "[proxy] taking it back from peer {peer}: {:?} elapsed, normal is {:?}",
                        started.elapsed(),
                        limit
                    );
                    *self
                        .wire
                        .held()
                        .rejected
                        .entry(format!("peer {peer} too slow"))
                        .or_default() += 1;
                    self.strike(peer);
                    return Adoption::TookBack;
                }
            }
        }
    }

    /// The gateway rides the same channel, because the client's does.
    fn gw(&self) -> GwClient {
        gw::llb_bridge_client::LlbBridgeClient::new(self.channel.clone())
    }

    /// Add daemons to route to. Peer 0 is always this proxy's upstream.
    pub async fn with_peers(mut self, addrs: &[String]) -> anyhow::Result<Self> {
        let mut peers = vec![Peer {
            // Peer 0's own share. `REBUCK2_HOME_WEIGHT` because it has no
            // `--peer` flag to carry a `*N`.
            weight: std::env::var("REBUCK2_HOME_WEIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            platforms: platforms_of(self.channel.clone()).await,
            addr: "upstream".into(),
        }];
        for a in addrs {
            // `http://host:port*2` - twice the share. Split from the RIGHT
            // because a URL will not contain `*`.
            let (url, weight) = match a.rsplit_once('*') {
                Some((u, w)) => (u.to_owned(), w.parse().unwrap_or(1usize)),
                None => (a.clone(), 1usize),
            };
            let channel = tonic::transport::Endpoint::new(url.clone())?
                .connect()
                .await?;
            peers.push(Peer {
                weight: weight.max(1),
                platforms: platforms_of(channel).await,
                addr: url,
            });
        }
        for (i, p) in peers.iter().enumerate() {
            println!(
                "[proxy] peer {i} {} native {} weight {}",
                p.addr,
                p.platforms.first().map(String::as_str).unwrap_or("unknown"),
                p.weight
            );
        }
        println!("[proxy] {} daemon(s) in the fleet", peers.len());
        self.outstanding = std::sync::Arc::new(
            (0..peers.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        );
        self.strikes = std::sync::Arc::new(
            (0..peers.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
        );
        self.peers = std::sync::Arc::new(peers);
        Ok(self)
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
        let t = std::time::Instant::now();
        let out = self
            .client()
            .solve(Request::from_parts(meta, ext, req))
            .await;
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
        let up = self.wire.clone();
        let inbound = stream.filter_map(move |m| {
            if let Ok(msg) = &m {
                up.held()
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
                down.held()
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
    std::env::var("REBUCK2_PEER_CACHE_MOUNTS").as_deref() == Ok("1")
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

/// How many completed builds before a side's mean means anything.
const MIN_SAMPLES: usize = 2;
/// Largest weight `derive_weights` will hand out. Bounds how blocky `turn`'s
/// rotation can get.
const WEIGHT_CAP: usize = 8;

/// Turn two mean service times into a pair of small weights.
///
/// The control law, and it is measured rather than reasoned: at the split
/// that minimises wall clock, the two sides FINISH TOGETHER. Twenty-four
/// builds across two machines -
///
/// ```text
/// 12:12  wall 18s   home 11425ms  away 17235ms  ratio 1.51
/// 16: 8  wall 15s   home 15228ms  away 15506ms  ratio 1.02
/// ```
///
/// A ratio above one says the away side is the straggler and the share
/// should move home; the balance point is ratio 1. Shares therefore go as
/// the INVERSE of service time, which needs no core counts and no declared
/// capacity: an operator guessing from cores got 21s, and this arithmetic on
/// the 12:12 numbers gives 3:2, which is next to the 16:8 that measured best.
///
/// Small integers, because `turn` hands out contiguous blocks: weights of
/// 15228 and 15506 would send fifteen thousand consecutive solves to one peer
/// before the other saw a single one. `cap` bounds the denominator, and the
/// approximation is the best rational within it rather than a truncation -
/// 1.51 must become 3:2, not 1:1.
fn derive_weights(home_ms: u64, away_ms: u64, cap: usize) -> (usize, usize) {
    if home_ms == 0 || away_ms == 0 {
        return (1, 1);
    }
    // home:away = away_ms:home_ms - the side that takes longer gets fewer.
    let ratio = away_ms as f64 / home_ms as f64;
    let (mut best, mut err) = ((1usize, 1usize), f64::INFINITY);
    for d in 1..=cap {
        let n = ((ratio * d as f64).round() as usize).clamp(1, cap);
        let e = (ratio - n as f64 / d as f64).abs();
        if e < err {
            err = e;
            best = (n, d);
        }
    }
    best
}

fn turn(cursor: usize, weights: &[usize]) -> usize {
    debug_assert!(!weights.is_empty(), "a fleet with no peers has no turns");
    // Weighted, because machines are not interchangeable and buildkit will
    // not say so: `ListWorkers` reports platforms, snapshotter, executor and
    // gc policy, and nothing at all about how many cores are behind them.
    // Measured on two real machines - a flat split sent half the work to a
    // 32-core box and half to a 16-core one, which is the visible waste in
    // the 24s -> 18s result.
    //
    // A weight of 2 means "twice as many turns", nothing more precise. That
    // is principle 13: a coarse estimate an operator can state is worth more
    // than a exact one nobody can obtain.
    let total: usize = weights.iter().sum();
    let Some(mut at) = cursor.checked_rem(total) else {
        return 0;
    };
    for (i, &w) in weights.iter().enumerate() {
        if at < w {
            return i;
        }
        at -= w;
    }
    0
}

/// How an attempt to place a solve on a peer ended.
///
/// This was `Option<Result<String>>`, and the take-back arrived as an `Err` -
/// so a solve we withdrew ourselves was counted as "peer refused", blaming a
/// machine for something it did not do. Three outcomes need three names.
enum Adoption {
    /// The peer built it and published it under this reference.
    Done(String),
    /// The peer was asked and could not.
    Refused(anyhow::Error),
    /// We stopped waiting. Already counted; the peer may still finish.
    TookBack,
    /// Never offered - the graph was not portable.
    NotOffered,
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

fn hedge_after(observed: &[u64]) -> Option<std::time::Duration> {
    /// Not two: a peer that is merely on the slow side of normal should
    /// finish, not be abandoned with the work half done (principle 12 -
    /// finishing beats starting).
    const FACTOR: u32 = 3;
    /// Below this, a "straggler" is scheduling noise.
    const FLOOR: std::time::Duration = std::time::Duration::from_secs(5);
    if observed.is_empty() {
        return None;
    }
    let mut v = observed.to_vec();
    v.sort_unstable();
    let median = v[v.len() / 2];
    Some(std::cmp::max(
        FLOOR,
        std::time::Duration::from_millis(median) * FACTOR,
    ))
}

/// Ask a daemon what it can run, in its own order.
///
/// A daemon that will not answer is not excluded - it is simply never
/// preferred as native. Failing closed here would turn one unhealthy
/// ListWorkers into a fleet of one (principle 5).
async fn platforms_of(channel: Chan) -> Vec<String> {
    match control::control_client::ControlClient::new(channel)
        .list_workers(control::ListWorkersRequest::default())
        .await
    {
        Ok(r) => r
            .into_inner()
            .record
            .first()
            .map(|w| {
                w.platforms
                    .iter()
                    .map(|p| format!("{}/{}", p.os, p.architecture))
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            println!("[proxy] a daemon would not list workers ({e}); never preferred as native");
            Vec::new()
        }
    }
}

/// Whether a peer can build this platform NATIVELY.
///
/// Measured, and it is the whole reason platform filtering is not a one-line
/// set membership test. A stock buildkitd on an arm64 host reports:
///
/// ```text
/// linux/arm64,linux/amd64,linux/amd64/v2,linux/riscv64,linux/ppc64le,...
/// ```
///
/// and the same image forced to amd64 reports:
///
/// ```text
/// linux/amd64,linux/amd64/v2,linux/amd64/v3,linux/arm64,linux/riscv64,...
/// ```
///
/// Every daemon claims nearly every platform, because binfmt/QEMU will run
/// anything. So "does this peer support linux/amd64" is answered YES by every
/// peer in any fleet and filtering on it is a no-op. The worker's OWN
/// architecture is the one it lists FIRST, and that is the distinction native
/// multi-arch is about: an emulated build is legal and five to ten times
/// slower.
///
/// Emulation is not refused here - a fleet with no native peer should still
/// build. It is deprioritised, and if it turns out ruinous the existing
/// take-back catches it.
fn native_for(worker_platforms: &[String], want: &str) -> bool {
    worker_platforms.first().is_some_and(|p| p == want)
}

/// Where this solve goes: `None` means home.
///
/// Three inputs, because each answers something the others cannot. `cursor`
/// keeps peer 0 in the rotation (its own occupancy is unobservable from here).
/// `load` is exact in-flight adoptions. `strikes` is memory: a peer taken back
/// for being abnormally slow is not merely busy, and without memory the fleet
/// rediscovers that fact once per solve.
///
/// `load` and `strikes` are indexed over AWAY peers only - peer 0 is neither
/// counted nor struck - so the returned index is offset back into fleet space
/// by the caller.
fn place(
    cursor: usize,
    // `false` once the caller has established the work must leave - then this
    // only chooses WHICH peer.
    home_allowed: bool,
    // One per peer INCLUDING peer 0, so home takes a share proportional to
    // its own capacity rather than a flat 1/n.
    weights: &[usize],
    load: &[usize],
    strikes: &[usize],
    // `true` where the away peer can build the wanted platform natively.
    // All-true when the graph pins no platform, which is the common case.
    native: &[bool],
) -> Option<usize> {
    /// A struck peer is treated as though it already holds this many jobs.
    ///
    /// Deliberately the same number as `hedge_after`'s factor: the peer
    /// exceeded three times normal, so it is discounted by three times a job.
    /// This is a bias, not a ban - a peer with one strike still wins against
    /// peers holding four jobs each, and a fleet that banned machines
    /// outright would shrink itself on one bad minute.
    const STRIKE_WEIGHT: usize = 3;
    if weights.len() <= 1 {
        return None;
    }
    // The home/away decision may already have been made, and if it has, do
    // not make it again. Saturation says home is FULL; running the weighted
    // rotation on top of that sent half the saturated solves home anyway -
    // two mechanisms deciding the same thing in series, each unaware of the
    // other. It showed as `placed {0: 20, 1: 4}` with 16 slots: sixteen while
    // home had room, then eight saturated of which the rotation kept four.
    // UNREACHABLE as things stand: the sole caller passes `home_allowed:
    // false`, because saturation has already settled home-versus-away by the
    // time this is asked. Kept, with the reason, because the branch reads as
    // live and a reader who assumes it fires will conclude that a weight
    // shifts work between home and away. It does not - only between away
    // peers, via the score below. Slots are the home/away control.
    if home_allowed && turn(cursor, weights) == 0 {
        return None;
    }
    debug_assert_eq!(load.len(), strikes.len(), "away peers counted twice over");
    debug_assert_eq!(load.len(), native.len(), "away peers counted twice over");
    // Every away peer has misbehaved: home is the fail-open answer
    // (principle 5). Without this a two-daemon fleet whose only peer is bad
    // would offer to it, wait out the bound, and take it back - every solve.
    if !strikes.is_empty() && strikes.iter().all(|&s| s > 0) {
        return None;
    }
    // No away peer can run this natively: home, not "slowly somewhere".
    //
    // This started as a BIAS, on the reasoning that emulation is legal and
    // merely five to ten times slower. Two real machines refuted it. The base
    // image in the mirror is single-architecture - an arm64 host resolving
    // `alpine:3.20` mirrors the arm64 image - so a peer of another
    // architecture does not run it slowly, it fails with `exit code: 255`
    // after pulling. Emulation would need the mirror to carry a manifest
    // LIST, which it does not yet.
    if !native.is_empty() && !native.iter().any(|&n| n) {
        return None;
    }
    /// What an emulated peer is discounted by. Larger than STRIKE_WEIGHT
    /// because emulation is a known 5-10x, where a strike is one bad
    /// observation - but finite, so a fleet with no native peer still builds
    /// rather than refusing work it can do slowly.
    const EMULATED_WEIGHT: usize = 8;
    /// Fixed-point, so load can be divided by capacity in integers: four
    /// jobs on a weight-2 machine scores the same as two on a weight-1.
    const SCALE: usize = 1024;
    let effective: Vec<usize> = load
        .iter()
        .zip(strikes)
        .zip(native)
        .zip(&weights[1..])
        .map(|(((&l, &s), &n), &w)| {
            // Load is normalised by capacity; the penalties are NOT. A strike
            // says the machine misbehaved and a foreign architecture says it
            // must emulate - neither is something a bigger machine absorbs.
            l * SCALE / w.max(1) + (s * STRIKE_WEIGHT + usize::from(!n) * EMULATED_WEIGHT) * SCALE
        })
        .collect();
    Some(1 + least_loaded(&effective, cursor))
}

/// A peer is spoken for, from the moment it is CHOSEN until the attempt ends.
///
/// The span matters more than the counting. Reserving at adoption time looks
/// equivalent and is not: ~1.6s of `make_portable` sits between the choice
/// and the adoption, so a burst of solves all decide against a counter that
/// is still zero and pile onto the same peer. Measured - twelve solves
/// arriving in 130ms split 6/6 with one peer weighted four times the other.
///
/// A guard rather than a decrement on each exit path, because the exits
/// include a take-back inside a `select!` loop. A missed decrement never
/// heals, and a peer holding a phantom job is one the fleet stops choosing.
struct Holding<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for Holding<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Which away peer, given how much each is already holding.
///
/// `turn` decides home-or-away and this decides WHICH away, because the two
/// questions have different amounts of information behind them. Outstanding
/// adoptions are known exactly - the future is held across
/// `build_and_publish`, so the count is incremented before and decremented
/// after. Peer 0's occupancy is not knowable from here at all: its work is
/// done by the daemon being proxied and its gateway solve returns lazily in
/// about a millisecond, so counting it would show peer 0 permanently idle and
/// hand it everything.
///
/// The scan starts at `cursor` so that EQUAL loads round-robin instead of
/// piling on the lowest index - which is also why this degrades to exactly
/// `turn`'s behaviour when every peer is idle, and only diverges when one
/// genuinely is busier. Never worse than round robin, better whenever solves
/// arrive spread out over a build rather than all at once.
fn least_loaded(load: &[usize], cursor: usize) -> usize {
    debug_assert!(!load.is_empty(), "no away peers to choose between");
    let n = load.len().max(1);
    // `min_by_key` keeps the FIRST minimum, and the scan starts at the
    // cursor, so a tie goes to the peer after the last one picked.
    (0..n)
        .map(|i| (cursor + i) % n)
        .min_by_key(|&i| load.get(i).copied().unwrap_or(usize::MAX))
        .unwrap_or(0)
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
    pub total: u64,
    /// Rewriting the graph: publishing the context, mirroring base images.
    pub portable: u64,
    /// The peer building it and pushing the result.
    pub adopt: u64,
    /// Peer 0 answering - after adoption this is a PULL, not a build.
    pub answer: u64,
}

impl Span {
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
    /// Where each solve was placed: key 0 is home, 1.. are peers.
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
            let short = u64::from_str_radix(&digest[..16], 16).unwrap_or_default();
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
            (self.repeated_ops * 100).checked_div(self.ops).unwrap_or(0)
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
        println!("[wire] built at home  : {} (peer 0's own share)", self.home);
        println!("[wire] placed         : {:?} (0 = home)", self.placed);
        self.diagnose();
        if let (Some(&first), Some(&last)) = (self.arrivals.first(), self.arrivals.last()) {
            println!(
                "[wire] arrivals       : {} solves spread over {}ms (first {first}, last {last})",
                self.arrivals.len(),
                last.saturating_sub(first)
            );
        }
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
                "[wire] solve {i} ms     : total {} = portable {} + peer {} + answer {} \
                 (tax {})",
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
        crate::dispatch::rewrite_registry_sources(&out, &|r| {
            self.resolved(&(key_ns.clone(), r.to_owned()))
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
fn report_gateway(wire: &std::sync::Mutex<Wire>, req: &gw::SolveRequest) {
    let Some(def) = &req.definition else {
        // Say WHY nothing can be dispatched, not merely that nothing was.
        // "no definition" is true and useless; a user who points `docker
        // build` at this proxy and sees an even split of nothing deserves the
        // reason and the remedy.
        let mut w = wire.held();
        if req.frontend.is_empty() {
            *w.rejected.entry("no definition".to_owned()).or_default() += 1;
            return;
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
        return;
    };
    let a = crate::dispatch::analyse(def, MIN_CUT_OPS);
    let free: Vec<&crate::dispatch::Cut> = a.free_cuts().collect();
    let mut w = wire.held();
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
    let solo = proxy.solo_ms.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
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
        println!(
            "[wire] peer solo ms   : {medians:?} (uncontended, n={:?})",
            solo.iter()
                .map(|(p, v)| (*p, v.len()))
                .collect::<std::collections::BTreeMap<_, _>>()
        );
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
        // Where the time goes. The tax was invisible until peer 0 rejoined
        // the round robin (10s -> 11s); "attack the round-trip" is a guess
        // until it is split into publish / peer build / answer, because two
        // of those three are not round-trips at all.
        let t_solve = std::time::Instant::now();
        {
            let mut w = self.wire.held();
            let first = *w.first_solve.get_or_insert(t_solve);
            let at = t_solve.duration_since(first).as_millis() as u64;
            w.arrivals.push(at);
        }
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
        {
            let mut w = self.wire.held();
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
                let allowed = verdict.dispatchable_when(crate::dispatch::Allow {
                    secrets: can_serve_secrets(&def),
                    caches: local_caches(),
                    agent: forwarding_agent(),
                });
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
                    let why = verdict
                        .exclusions
                        .first()
                        .map(|(_, e)| format!("excluded: {e:?} {detail:?}"))
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
                // Nothing can be adopted while the mirror is down, whoever
                // owns the fault. Building at home IS the fail-open answer,
                // so refusing to offer costs nothing beyond the fleet.
                let mirror_down = self.mirror_believed_down();
                if allowed && worth && mirror_down {
                    *self
                        .wire
                        .held()
                        .rejected
                        .entry("mirror down, not offering".to_owned())
                        .or_default() += 1;
                }
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
                if allowed && worth && !saturated {
                    *self
                        .wire
                        .held()
                        .rejected
                        .entry("home has room".to_owned())
                        .or_default() += 1;
                }
                let peer = if allowed && worth && saturated && !mirror_down {
                    // Home is full: this only chooses which peer.
                    self.next_place(&verdict.platform, false)
                } else {
                    None
                };
                // Reserved here, not at adoption: everything between the two
                // is preparation, and a sibling deciding during it must see
                // this peer as busy.
                let _holding = peer.map(|p| {
                    self.outstanding[p].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Holding(&self.outstanding[p])
                });
                // The slot was claimed above. Hand it back if the work is
                // leaving after all.
                if peer.is_some() && claimed {
                    self.home_inflight
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                if allowed {
                    *self
                        .wire
                        .held()
                        .placed
                        .entry(peer.unwrap_or(0))
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
                        .insert(id.to_owned(), (peer, key.clone(), claimed));
                }
                if allowed && peer.is_none() {
                    // Peer 0's turn. It already holds the job, the session
                    // and the graph, so its share is served by falling
                    // through to the ordinary solve below - no publish, no
                    // import, no adoption. This is not merely the cheaper
                    // route: peer 0's `addr` is the sentinel "upstream", so
                    // it is the only route.
                    self.wire.held().home += 1;
                } else if let Some(peer) = peer {
                    let addr = self.peers[peer].addr.clone();
                    debug_assert_ne!(
                        addr, "upstream",
                        "peer 0 has no dialable address; its turn is the home path"
                    );
                    // Only NOW is the rewrite worth its 1.6s: this graph is
                    // leaving. Publishing a context and mirroring a base for
                    // a solve that stays home buys nothing at all.
                    let session = self.session_for(&meta);
                    let t = std::time::Instant::now();
                    let target = self.peers[peer].platforms.first().cloned();
                    let portable = self
                        .make_portable(&def, &session, mirror, target.as_deref())
                        .await;
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
                    // Counting the reason is not the same as ACTING on it:
                    // an unportable graph must not be sent, or the peer
                    // reaches for Docker Hub with no credentials. Moving the
                    // rewrite in here turned a `movable` conjunction into a
                    // count-and-continue, which is how a check becomes
                    // decoration.
                    let adopted = if local_clear && bases_clear {
                        // The reservation is taken at the DECISION, above,
                        // and this solve is inside the count - so one means
                        // this adoption has the peer to itself and its
                        // duration is a clean speed sample.
                        //
                        // It used to add here and test for zero, which is
                        // the same question asked too late: everything
                        // between the choice and this line is preparation,
                        // and a peer picked up three siblings during it that
                        // this counter could not see.
                        let alone =
                            self.outstanding[peer].load(std::sync::atomic::Ordering::Relaxed) == 1;
                        let t = std::time::Instant::now();
                        let r = self
                            .adopt_or_take_back(peer, &addr, &mirror.registry, &portable, t)
                            .await;
                        t_adopt = t.elapsed().as_millis() as u64;
                        // Still one, still just us. The guard gives the
                        // slot back when the solve ends, not here, so this
                        // reads the live count rather than a pre-decrement
                        // value.
                        let still_alone =
                            self.outstanding[peer].load(std::sync::atomic::Ordering::Relaxed) == 1;
                        // Only a COMPLETED adoption tells us what normal
                        // costs. Recording a take-back would fold our own
                        // impatience into the threshold that produced it.
                        if matches!(r, Adoption::Done(_)) {
                            remember(&mut self.adopted_ms.held(), t_adopt);
                            // Still alone at the END as well as the start:
                            // a second adoption arriving mid-build makes the
                            // sample contended, and a contended sample is the
                            // endogenous number this exists to avoid.
                            if alone && still_alone {
                                remember(self.solo_ms.held().entry(peer).or_default(), t_adopt);
                            }
                        }
                        r
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
                        Adoption::NotOffered
                    };
                    match adopted {
                        // Nothing was offered, so nothing refused. Reporting
                        // "peer refused" here would blame a machine that was
                        // never asked.
                        Adoption::NotOffered | Adoption::TookBack => {}
                        Adoption::Done(reference) => {
                            println!("[proxy] adopted from peer {peer}: {reference}");
                            self.wire.held().routed += 1;
                            req.definition = Some(crate::dispatch::import_graph(&reference));
                        }
                        // Fail open: build it here, exactly as we would
                        // have without a fleet.
                        Adoption::Refused(e) => {
                            use prost::Message;
                            let srcs: Vec<String> = portable
                                .def
                                .iter()
                                .filter_map(|b| {
                                    match bollard_buildkit_proto::pb::Op::decode(b.as_slice())
                                        .ok()?
                                        .op?
                                    {
                                        bollard_buildkit_proto::pb::op::Op::Source(s) => {
                                            Some(s.identifier)
                                        }
                                        _ => None,
                                    }
                                })
                                .collect();
                            println!(
                                "[proxy] peer {peer} could not take it: {e:#} | sources={srcs:?}"
                            );
                            self.wire.held().last_refusal = Some(format!("{e:#}"));
                            // Count the FAILURES too. `routed` counts only
                            // successes, so a fleet attempting twelve
                            // adoptions and completing one looked identical
                            // to a fleet attempting one - which sent me
                            // hunting a placement bug that did not exist.
                            *self
                                .wire
                                .held()
                                .rejected
                                .entry(format!("peer {peer} refused"))
                                .or_default() += 1;
                            // A refusal strikes, exactly as a take-back does.
                            // Measured by killing a peer mid-build: it was
                            // offered work eight more times, each offer
                            // waiting out a transport error, because only
                            // slowness was remembered and failure was not.
                            // The build still finished with the right bytes -
                            // fail-open works - but it paid for the same
                            // discovery eight times.
                            //
                            // Only if the peer is the one that failed, though.
                            // Killing the REGISTRY produced the same eight
                            // refusals and struck a machine that had done
                            // nothing wrong.
                            if self.mirror_alive().await {
                                self.strike(peer);
                            } else {
                                *self
                                    .wire
                                    .held()
                                    .rejected
                                    .entry("mirror down, peer not blamed".to_owned())
                                    .or_default() += 1;
                            }
                        }
                    }
                }
            }
        }

        // Always peer 0: it holds the client's job, and after adoption the
        // graph is a fetch rather than a build.
        let t_answer = std::time::Instant::now();
        let out = self.gw().solve(Request::from_parts(meta, ext, req)).await?;
        let answer = t_answer.elapsed().as_millis() as u64;
        self.wire.held().spans.push(Span {
            total: t_solve.elapsed().as_millis() as u64,
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
    use super::turn;

    /// The regression: peer 0 must get its share.
    ///
    /// Under the old `1 + cursor % (peers - 1)` this fails on the first
    /// assert - peer 0 never came up, however many solves arrived, so a
    /// two-machine fleet ran on one machine and called it dispatch.
    #[test]
    fn every_peer_in_the_fleet_gets_a_turn() {
        for peers in 1..6usize {
            let mut seen = vec![0usize; peers];
            for cursor in 0..peers * 3 {
                seen[turn(cursor, &vec![1usize; peers])] += 1;
            }
            assert_eq!(seen[0], 3, "peer 0 skipped in a fleet of {peers}");
            assert!(
                seen.iter().all(|&n| n == 3),
                "uneven over {peers} peers: {seen:?}"
            );
        }
    }

    /// Native beats emulated, but emulated still beats nowhere.
    ///
    /// Every buildkitd claims nearly every platform because binfmt will run
    /// anything, so a set-membership filter is answered YES by every peer and
    /// changes nothing. The worker's own architecture is the one it lists
    /// FIRST.
    #[test]
    fn a_native_peer_is_preferred_and_an_emulated_one_is_not_refused() {
        let arm = vec!["linux/arm64".to_owned(), "linux/amd64".to_owned()];
        let x86 = vec!["linux/amd64".to_owned(), "linux/arm64".to_owned()];
        assert!(super::native_for(&arm, "linux/arm64"));
        assert!(
            !super::native_for(&arm, "linux/amd64"),
            "emulation is not native"
        );
        assert!(super::native_for(&x86, "linux/amd64"));
        // Peer 1 emulates, peer 2 is native: every away turn goes to peer 2.
        let away: Vec<usize> = (0..8)
            .filter_map(|c| super::place(c, true, &[1, 1, 1], &[0, 0], &[0, 0], &[false, true]))
            .collect();
        assert!(!away.is_empty());
        assert!(
            away.iter().all(|&p| p == 2),
            "emulated peer chosen: {away:?}"
        );
        // Nobody native: HOME. This asserted the opposite until two real
        // machines were involved - emulation is not merely slow here, because
        // the mirrored base is single-architecture and a foreign peer fails
        // with `exit code: 255` after pulling it.
        let away: Vec<usize> = (0..8)
            .filter_map(|c| super::place(c, true, &[1, 1, 1], &[0, 0], &[0, 0], &[false, false]))
            .collect();
        assert!(
            away.is_empty(),
            "offered an arm64 base to a peer that cannot execute it: {away:?}"
        );
    }

    /// A daemon that will not answer ListWorkers is usable, just never
    /// preferred - failing closed would turn one bad health check into a
    /// fleet of one.
    #[test]
    fn a_silent_daemon_is_not_native_and_not_excluded() {
        assert!(!super::native_for(&[], "linux/arm64"));
    }

    /// A struck peer stops getting an equal share.
    #[test]
    fn a_peer_taken_back_from_is_avoided() {
        // Two away peers, peer 2 struck once. Every away turn goes to peer 1.
        let load = [0usize, 0];
        let strikes = [0usize, 1];
        let away: Vec<usize> = (0..12)
            .filter_map(|c| super::place(c, true, &[1, 1, 1], &load, &strikes, &[true, true]))
            .collect();
        assert!(!away.is_empty(), "no away turns at all");
        assert!(
            away.iter().all(|&p| p == 1),
            "struck peer 2 still got work: {away:?}"
        );
    }

    /// A bias, not a ban. A struck peer beats peers that are genuinely
    /// swamped - otherwise one bad minute permanently shrinks the fleet.
    #[test]
    fn a_struck_peer_still_beats_a_swamped_one() {
        let away: Vec<usize> = (0..6)
            .filter_map(|c| super::place(c, true, &[1, 1, 1], &[9, 0], &[0, 1], &[true, true]))
            .collect();
        assert!(
            away.iter().all(|&p| p == 2),
            "peer 1 holding nine jobs was preferred to a once-struck idle peer: {away:?}"
        );
    }

    /// Nowhere good to send it is not a reason to send it somewhere bad.
    ///
    /// Without this a two-daemon fleet whose only peer is slow would offer,
    /// wait out the bound and take it back on EVERY solve - paying the
    /// straggler tax forever to learn something it already knew.
    #[test]
    fn a_wholly_struck_fleet_builds_at_home() {
        for cursor in 0..8 {
            assert_eq!(
                super::place(cursor, true, &[1, 1], &[0], &[1], &[true]),
                None
            );
            assert_eq!(
                super::place(cursor, true, &[1, 1, 1], &[0, 0], &[2, 1], &[true, true]),
                None
            );
        }
    }

    /// A lone daemon has nowhere to send anything.
    #[test]
    fn a_fleet_of_one_never_dispatches() {
        assert_eq!(super::place(0, true, &[1], &[], &[], &[]), None);
        assert_eq!(super::place(7, true, &[1], &[], &[], &[]), None);
    }

    /// With nothing observed, nothing is slow.
    ///
    /// The alternative - a cold default - is a number chosen to make one
    /// fixture look good. Any value between 9.5s and 163s "fixes" the slow
    /// peer measurement, which is exactly why none of them should be picked
    /// before the fleet has said what normal is.
    #[test]
    fn a_cold_fleet_calls_nothing_a_straggler() {
        assert_eq!(super::hedge_after(&[]), None);
    }

    /// Three times the median, and the median is not the mean: one 163s
    /// straggler among normal work must not drag the threshold up to meet
    /// itself.
    #[test]
    fn the_threshold_is_not_moved_by_the_straggler_it_judges() {
        let normal = [9_400u64, 9_500, 9_600];
        let with_straggler = [9_400u64, 9_500, 9_600, 163_000];
        assert_eq!(
            super::hedge_after(&normal),
            Some(std::time::Duration::from_millis(28_500))
        );
        // Median of the 4-element list is 9600, so the threshold moves by
        // 300ms, not by two and a half minutes. A mean would have put it at
        // over four minutes and never fired.
        assert_eq!(
            super::hedge_after(&with_straggler),
            Some(std::time::Duration::from_millis(28_800))
        );
    }

    #[test]
    fn the_hedge_forgets_a_fleet_that_is_no_longer_slow() {
        // Timing history grew forever. Three faults, one cause.
        //
        // Memory and CPU are the obvious two: `hedge_after` clones and sorts
        // the WHOLE history on every adoption, so a long-lived proxy pays
        // O(n log n) against an n that never stops rising.
        //
        // The third is the one that matters. An all-time median cannot track
        // conditions. A fleet that was slow this morning keeps a high
        // threshold all afternoon, so the hedge stops withdrawing from
        // stragglers exactly when the rest of the fleet has got fast enough
        // for one to matter.
        let mut v = Vec::new();
        for _ in 0..200 {
            super::remember(&mut v, 10_000);
        }
        let slow = super::hedge_after(&v).expect("a threshold");

        // Conditions improve. Enough observations to fill the window, and no
        // more - if the window were unbounded the old 10s samples would still
        // be half the sample and the median would barely move.
        for _ in 0..super::OBSERVATIONS {
            super::remember(&mut v, 100);
        }
        let fast = super::hedge_after(&v).expect("a threshold");

        assert_eq!(v.len(), super::OBSERVATIONS, "the window is not bounded");
        assert!(
            fast < slow,
            "hedge did not follow the fleet down: {fast:?} vs {slow:?}"
        );
        // 100ms work: 3x the median is under the floor, so the floor governs.
        assert_eq!(fast, std::time::Duration::from_secs(5));
    }

    /// Fast work is not hedged on jitter.
    #[test]
    fn a_fleet_of_fast_subtrees_has_a_floor() {
        assert_eq!(
            super::hedge_after(&[80, 90, 100]),
            Some(std::time::Duration::from_secs(5)),
            "270ms would abandon peers over scheduling noise"
        );
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

    /// The control law reproduces the measured optimum.
    ///
    /// These are the real numbers from two machines. The flat split's
    /// observation must point AT the split that measured best, or the law is
    /// just an equation that happens to run.
    #[test]
    fn observed_service_times_point_at_the_measured_optimum() {
        // 12:12, wall 18s - away is the straggler.
        assert_eq!(super::derive_weights(11_425, 17_235, 8), (3, 2));
        // 16:8, wall 15s - balanced, so stay put.
        assert_eq!(super::derive_weights(15_228, 15_506, 8), (1, 1));
    }

    /// 1.51 must become 3:2, not 1:1. Truncating the ratio would report
    /// "balanced" for a fleet that is half again slower on one side.
    #[test]
    fn a_ratio_between_whole_numbers_is_not_truncated() {
        assert_eq!(super::derive_weights(1_000, 1_510, 8), (3, 2));
        assert_eq!(super::derive_weights(1_000, 1_250, 8), (5, 4));
        assert_eq!(super::derive_weights(1_000, 2_000, 8), (2, 1));
    }

    /// No samples, no opinion.
    #[test]
    fn a_side_with_no_time_gets_an_even_split() {
        assert_eq!(super::derive_weights(0, 5_000, 8), (1, 1));
        assert_eq!(super::derive_weights(5_000, 0, 8), (1, 1));
    }

    /// Once home is full, the rotation must not send work home anyway.
    ///
    /// The regression: with `home_allowed` ignored, half of every saturated
    /// solve went home on its turn, and a fleet with sixteen slots and
    /// twenty-four builds placed twenty at home instead of sixteen.
    #[test]
    fn a_full_home_is_not_offered_the_work_again() {
        let away: Vec<Option<usize>> = (0..8)
            .map(|c| super::place(c, false, &[1, 1], &[0], &[0], &[true]))
            .collect();
        assert!(
            away.iter().all(|p| *p == Some(1)),
            "work went home when home was full: {away:?}"
        );
        // And with home allowed, it still takes its share.
        let mixed: Vec<Option<usize>> = (0..8)
            .map(|c| super::place(c, true, &[1, 1], &[0], &[0], &[true]))
            .collect();
        assert!(
            mixed.iter().any(|p| p.is_none()),
            "home got no turns at all"
        );
    }

    /// A declared weight buys proportionally more turns.
    ///
    /// Buildkit will not say how big a machine is, so an operator saying
    /// "twice the share" is the only capacity signal there is. Measured on
    /// two real machines: a flat split sent half the work to a 32-core box
    /// and half to a 16-core one.
    #[test]
    fn a_heavier_peer_takes_proportionally_more() {
        // home 1, peer1 2, peer2 1 - six turns is one whole cycle.
        let w = [1usize, 2, 1];
        let picks: Vec<usize> = (0..8).map(|c| super::turn(c, &w)).collect();
        assert_eq!(picks, vec![0, 1, 1, 2, 0, 1, 1, 2]);
    }

    /// Load is compared PER UNIT of capacity, so a big machine holding four
    /// jobs is no busier than a small one holding two.
    #[test]
    fn load_is_measured_against_capacity() {
        // peer 1 has weight 2 and holds 3; peer 2 has weight 1 and holds 2.
        // 3/2 > 2/1 is false, so peer 1 is the less loaded of the two.
        let away: Vec<usize> = (0..4)
            .filter_map(|c| super::place(c, true, &[0, 2, 1], &[3, 2], &[0, 0], &[true, true]))
            .collect();
        assert!(
            !away.is_empty(),
            "home weight 0 should send everything away"
        );
        assert!(
            away.iter().all(|&p| p == 1),
            "the roomier machine was passed over: {away:?}"
        );
    }

    /// Equal load must round-robin, not pile on peer 1.
    ///
    /// `min_by_key` keeps the first minimum, so scanning from 0 would send
    /// every away solve to the same machine and the extra peers would sit
    /// idle - the offloading bug again, one level down. Starting the scan at
    /// the cursor is what prevents it.
    #[test]
    fn an_idle_fleet_still_takes_turns() {
        let idle = [0usize, 0, 0];
        let picks: Vec<usize> = (0..6).map(|c| super::least_loaded(&idle, c)).collect();
        assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
    }

    /// A busy peer is skipped even when it is its turn.
    #[test]
    fn a_loaded_peer_is_passed_over() {
        // Peer 1 holds three adoptions; peers 0 and 2 hold none.
        let load = [0usize, 3, 0];
        for cursor in 0..6 {
            assert_ne!(
                super::least_loaded(&load, cursor),
                1,
                "cursor {cursor} chose the loaded peer"
            );
        }
        // And it comes back into rotation once it drains.
        assert_eq!(super::least_loaded(&[0, 0, 0], 1), 1);
    }

    /// The whole fleet busy is not a reason to pick nobody, and not a reason
    /// to always pick the same one.
    #[test]
    fn a_uniformly_busy_fleet_still_rotates() {
        let load = [2usize, 2, 2];
        let picks: Vec<usize> = (0..3).map(|c| super::least_loaded(&load, c)).collect();
        assert_eq!(picks, vec![0, 1, 2]);
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
            total: 10_000,
            portable: 300,
            adopt: 9_000,
            answer: 700,
        };
        assert_eq!(dispatched.tax(), 1_000);
        // A solve that stayed home pays no tax, however long it took.
        let home = super::Span {
            total: 10_000,
            portable: 0,
            adopt: 0,
            answer: 9_990,
        };
        assert_eq!(home.tax(), 9_990);
    }

    /// A cursor that has wrapped `usize` still lands in the fleet. Round
    /// robin over a long-lived proxy is the only user of this and it counts
    /// up forever.
    #[test]
    fn a_wrapped_cursor_still_names_a_peer() {
        for peers in 1..6usize {
            assert!(turn(usize::MAX, &vec![1usize; peers]) < peers);
        }
    }
}
