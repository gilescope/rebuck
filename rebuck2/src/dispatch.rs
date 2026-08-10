//! Whether a subtree may travel — the buildkit-facing half of dispatch.
//!
//! Principle 10 says the unit of work one machine asks another for is a
//! SUBTREE, never a vertex, and it comes with three consequences that are
//! all conservative:
//!
//! - **The whole definition is the unit.** One cache mount, one secret, one
//!   privileged exec anywhere in it excludes ALL of it. A
//!   partially-dispatchable tree is not dispatchable.
//!
//!   Not by propagating a verdict up the edges - `inspect` is a linear scan
//!   and never reads `op.inputs`, because the graph travels or stays as one
//!   piece and its shape cannot change that answer. Said plainly because
//!   "propagates upward" reads like a traversal exists to be reused, and
//!   carving at a seam would have to build one from nothing.
//! - **Platform is the union of the subtree's constraints.** One linux-only
//!   vertex pins the tree.
//! - **Failure granularity is the subtree.** It fails and re-runs as a unit,
//!   which is the price of not paying for its interior.
//!
//! This module answers only the first two, and answers them from the LLB
//! itself rather than from anything earthly told us — a `Definition` is what
//! actually gets built, so it is the honest place to ask.
//!
//! Every uncertainty resolves to "do not dispatch". Duplicate work is always
//! correct (principle 5); a subtree shipped to a peer that cannot honour its
//! mounts is not.

use std::collections::{BTreeMap, BTreeSet};

use bollard_buildkit_proto::pb;
use prost::Message;

/// Why a subtree may not travel.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exclusion {
    /// Already excluded from the lease: adoption is unsound. bazel keeps its
    /// real output tree in the mount and leaves a symlink in the layer, so a
    /// follower adopting it gets a dangling result (measured).
    CacheMount,
    /// Shipping the spec ships the secret reference.
    Secret,
    /// An agent socket is this machine's, definitionally.
    SshAgent,
    /// Granting a peer privileged exec is a trust decision, not a scheduling
    /// one.
    Insecure,
    /// Host networking means THIS host's network.
    HostNetwork,
    /// An op we could not read. Conservative on purpose: we cannot show it is
    /// safe, so it is not.
    Undecodable,
    /// A mount type outside the five buildkit declares - so a fork's, and its
    /// meaning is whatever that fork decided.
    ///
    /// Failing OPEN here cost a day. earthly's fork adds `SOCKET = 101`
    /// (`solver/pb/ops.proto:123`, "Earthly specific") and attaches two to
    /// every RUN. Those are session-backed: a worker takes the lead, then dies
    /// with `no active sessions`. inspect called the graph clean throughout,
    /// because it matched the types it knew and said nothing about the rest.
    ///
    /// Carries the number - having no name for it is the entire point.
    UnknownMount(i32),
    /// A source scheme a sessionless solve cannot be trusted to fetch.
    ///
    /// `git` is the measured one: buildkit resolves git credentials through
    /// the client session's auth provider, and a worker has no session, so
    /// the solve dies with `no active sessions` AFTER the fleet accepted the
    /// lead. On earthly's fork the error path then nil-derefs and takes the
    /// daemon down with it.
    ///
    /// An ALLOW-LIST, for the same reason `UnknownMount` exists: the schemes
    /// we can prove portable are few and known, and everything else is a
    /// guess. Guessing cost a day on mount type 101.
    SessionSource(String),
}

/// Where the subtree can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    /// Nothing declared one; any worker will do.
    Any,
    /// Every op that declared one agrees.
    Pinned(String),
    /// Two ops demand different platforms, so no single peer can build this
    /// subtree as a unit. Undispatchable rather than a choice to make.
    Conflict(BTreeSet<String>),
}

/// Which hazards the caller can neutralise, and therefore tolerate.
///
/// A struct rather than positional bools. Three of them read
/// `dispatchable_when(true, false, true)` at the call site, and this project
/// has already shipped one bug from a boolean that meant something other
/// than the reader assumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Allow {
    /// We can answer `GetSecret` for every secret this graph names.
    pub secrets: bool,
    /// The peer may use its own cache mount instead of ours.
    pub caches: bool,
    /// We can forward an ssh agent to the peer.
    pub agent: bool,
}

/// What inspecting a `Definition` concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// `(op index, why)`, in op order. Every one, not the first — a report
    /// naming one blocker leads to fixing it and finding the next.
    pub exclusions: Vec<(usize, Exclusion)>,
    pub platform: Platform,
    /// The union of every op's worker constraints. These AND together rather
    /// than conflicting, so a union is exactly right.
    pub constraints: BTreeSet<String>,
    pub ops: usize,
}

impl Verdict {
    #[allow(dead_code)] // the unconditional form; the proxy uses dispatchable_with
    pub fn dispatchable(&self) -> bool {
        self.dispatchable_with(false)
    }

    /// `serving_secrets` lifts the Secret exclusion; `local_caches` lifts
    /// CacheMount. Nothing else, ever, from either.
    ///
    /// A secret was fatal because a peer solves with no session and has
    /// nobody to ask. With `buildkit_session` we can attach one and answer
    /// the callback ourselves, so the hazard is gone - for secrets. Cache
    /// mounts, ssh sockets and host binds are untouched: each needs a
    /// different service, and lifting them together would be assuming three
    /// things from evidence about one.
    pub fn dispatchable_with(&self, serving_secrets: bool) -> bool {
        self.dispatchable_when(Allow {
            secrets: serving_secrets,
            ..Default::default()
        })
    }

    /// A cache mount is scratch space, and a peer has its own.
    ///
    /// It was excluded because it is daemon-local state, which is true and
    /// turns out not to be the point. A cache mount is not shared between
    /// daemons even without a fleet - it is not in the cache key, it is
    /// dropped by gc, and it does not survive a daemon restart. A build whose
    /// OUTPUT depends on what is in one is already non-reproducible on a
    /// single machine; dispatch does not make that worse, it just finds it
    /// sooner.
    ///
    /// So a peer builds with its own, colder, cache mount and produces the
    /// same bytes. Off by default anyway, because "already broken" is a
    /// reason to allow it, not a reason to assume nobody depends on it.
    pub fn dispatchable_when(&self, allow: Allow) -> bool {
        let blocked = self.exclusions.iter().any(|(_, e)| !lifted_by(e, allow));
        !blocked && !matches!(self.platform, Platform::Conflict(_))
    }
}

/// The fleet's standing policy on which hazards may travel.
///
/// ONE reader of the environment, because the gateway and the driver used to
/// have their own and disagreed: the gateway offered cache-mount subtrees
/// that the driver then refused as undispatchable, and the report blamed the
/// workers.
///
/// Read once - a policy that changes halfway through a build would place two
/// identical subtrees differently.
pub fn policy() -> Allow {
    static P: std::sync::OnceLock<Allow> = std::sync::OnceLock::new();
    *P.get_or_init(|| Allow {
        caches: std::env::var("REBUCK2_PEER_CACHE_MOUNTS").as_deref() == Ok("1"),
        ..Default::default()
    })
}

/// Does `allow` lift this hazard?
///
/// One table, because `dispatchable_when` and `consider` used to answer
/// it separately and a subtree the first would offer was refused by the
/// second.
fn lifted_by(e: &Exclusion, allow: Allow) -> bool {
    match e {
        Exclusion::Secret => allow.secrets,
        Exclusion::CacheMount => allow.caches,
        Exclusion::SshAgent => allow.agent,
        // Insecure exec and host networking are never lifted. Granting a
        // privilege is a trust decision, not a scheduling one, and there is
        // no session service that makes a peer's `--privileged` mean what
        // the client's would have meant. An unknown mount type is not lifted
        // either - opting in means having weighed it.
        _ => false,
    }
}

/// Every secret id this graph mounts, in op order.
///
/// Needed before dispatch, not after: lifting the Secret exclusion because
/// we *can* serve secrets, without checking we can serve THESE secrets,
/// offers work that is certain to fail. On an earthly build that is eleven
/// doomed round trips - the debugger's secret is generated per build and
/// kept in earthly's own internal store, so nothing outside that process can
/// resolve it.
pub fn secret_ids(def: &pb::Definition) -> Vec<String> {
    use prost::Message;
    let mut out = Vec::new();
    for bytes in &def.def {
        let Some(pb::op::Op::Exec(e)) = pb::Op::decode(bytes.as_slice()).ok().and_then(|o| o.op)
        else {
            continue;
        };
        for m in &e.mounts {
            if let Some(so) = &m.secret_opt {
                out.push(so.id.clone());
            }
        }
        for se in &e.secretenv {
            out.push(se.id.clone());
        }
    }
    out
}

/// `os/arch[/variant]`, as buildkit itself writes a platform.
fn plat_str(p: &pb::Platform) -> String {
    let base = format!("{}/{}", p.os, p.architecture);
    if p.variant.is_empty() {
        base
    } else {
        format!("{base}/{}", p.variant)
    }
}

/// What grounds this op, if anything.
///
/// Only an exec can carry these: a source or a file op has no mounts, no
/// secrets and no security mode. Grounding those would ground exactly the
/// `FROM <registry image>` chains principle 11 calls the BEST handover -
/// the ones whose whole frontier is a digest any machine can fetch.
/// Source schemes a peer can fetch with no session at all.
///
/// `docker-image` is here because `make_portable` mirrors every image into a
/// registry the fleet can reach; `local` because the same pass republishes
/// contexts as content, and what survives that is counted separately as an
/// unmirrored context rather than as a hazard.
const PORTABLE_SCHEMES: [&str; 4] = ["docker-image", "local", "http", "https"];

/// The mount types buildkit itself declares. Anything else is a fork's.
const KNOWN_MOUNTS: [i32; 5] = [
    pb::MountType::Bind as i32,
    pb::MountType::Secret as i32,
    pb::MountType::Ssh as i32,
    pb::MountType::Cache as i32,
    pb::MountType::Tmpfs as i32,
];

fn hazards(op: &pb::Op) -> Vec<Exclusion> {
    let mut out = Vec::new();
    // Sources first: an op that is not an Exec can still ground a subtree.
    // This used to return early on anything that was not an Exec, which is
    // how a lone `source: git` reached a worker with no session.
    if let Some(pb::op::Op::Source(src)) = op.op.as_ref() {
        let scheme = src
            .identifier
            .split_once("://")
            .map(|(s, _)| s)
            .unwrap_or("");
        if !PORTABLE_SCHEMES.contains(&scheme) {
            out.push(Exclusion::SessionSource(scheme.to_owned()));
        }
    }
    let Some(pb::op::Op::Exec(e)) = op.op.as_ref() else {
        return out;
    };
    // EVERY hazard on this op, not the first.
    //
    // This returned one, and one is wrong as soon as a lift exists: an
    // earthly RUN carries a go-mod cache mount AND the debugger's socket on
    // the same exec, so reporting only the cache mount and then lifting it
    // with REBUCK2_PEER_CACHE_MOUNTS=1 made the op look clean. The graph was
    // offered to a worker that solves without a session and declined with
    // "no active sessions" - fail-open saved the build and the fleet did
    // nothing, which is the failure this codebase is mostly about.
    if e.security == pb::SecurityMode::Insecure as i32 {
        out.push(Exclusion::Insecure);
    }
    if e.network == pb::NetMode::Host as i32 {
        out.push(Exclusion::HostNetwork);
    }
    if !e.secretenv.is_empty() {
        out.push(Exclusion::Secret);
    }
    // NOT detected: a mount that binds a path from the WORKER's filesystem.
    //
    // Deliberate, and settled by experiment rather than left as a hole. The
    // proto has no host-bind flag, and the obvious encoding - `Bind` with
    // `input = -1` and a selector - is not one: a stock daemon reads the
    // selector against an empty input and fails with `open <path>: no such
    // file or directory`. `llb.HostBind()` comes from earthbuild's FORK of
    // buildkit (go.mod replaces moby/buildkit with earthbuild/buildkit), so
    // it is not expressible in a graph a stock daemon would accept.
    //
    // Which means a host bind can only reach us from an earthly client, and
    // those graphs are already excluded by the secret and ssh mounts that
    // come with it. Guessing at a detection rule for an encoding this tree
    // cannot produce would be a check that never fires, tested by nothing.
    for m in &e.mounts {
        // Read BOTH the type and the option: a cache mount is identified by
        // either, and trusting one alone leaves the other as a way through.
        if m.mount_type == pb::MountType::Cache as i32 || m.cache_opt.is_some() {
            out.push(Exclusion::CacheMount);
        }
        if m.mount_type == pb::MountType::Secret as i32 || m.secret_opt.is_some() {
            out.push(Exclusion::Secret);
        }
        if m.mount_type == pb::MountType::Ssh as i32 || m.ssh_opt.is_some() {
            out.push(Exclusion::SshAgent);
        }
        // Fail CLOSED on anything else. A number we do not recognise is a
        // fork extension whose requirements we cannot see.
        if !KNOWN_MOUNTS.contains(&m.mount_type) {
            out.push(Exclusion::UnknownMount(m.mount_type));
        }
    }
    out.dedup();
    out
}

/// Everything a graph carries that a SESSIONLESS solve cannot satisfy,
/// spelled out for a human.
///
/// `inspect` answers yes-or-no from the hazards it models. This answers "what
/// is actually in here", and exists because a real earthly graph was declined
/// with `no active sessions` while inspect called it clean - so the model has
/// a gap and guessing at it twice was already one time too many.
///
/// Mount types by NUMBER as well as name: an unmodelled type is exactly the
/// case worth seeing, and it has no name here to print.
pub fn session_shape(def: &pb::Definition) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for bytes in &def.def {
        let Ok(op) = pb::Op::decode(bytes.as_slice()) else {
            *out.entry("op: undecodable".into()).or_default() += 1;
            continue;
        };
        match op.op {
            Some(pb::op::Op::Source(ref src)) => {
                let scheme = src
                    .identifier
                    .split_once("://")
                    .map(|(s, _)| s.to_owned())
                    .unwrap_or_else(|| "source: no scheme".into());
                *out.entry(format!("source: {scheme}")).or_default() += 1;
            }
            Some(pb::op::Op::Exec(ref e)) => {
                if !e.secretenv.is_empty() {
                    *out.entry("exec: secretenv".into()).or_default() += 1;
                }
                for m in &e.mounts {
                    let name = match m.mount_type {
                        x if x == pb::MountType::Bind as i32 => "bind".to_owned(),
                        x if x == pb::MountType::Secret as i32 => "secret".to_owned(),
                        x if x == pb::MountType::Ssh as i32 => "ssh".to_owned(),
                        x if x == pb::MountType::Cache as i32 => "cache".to_owned(),
                        x if x == pb::MountType::Tmpfs as i32 => "tmpfs".to_owned(),
                        other => format!("UNMODELLED({other})"),
                    };
                    *out.entry(format!("mount: {name}")).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    out
}

/// Read a `Definition` and decide whether its subtree may be handed to a peer.
pub fn inspect(def: &pb::Definition) -> Verdict {
    let mut exclusions = Vec::new();
    let mut platforms: BTreeSet<String> = BTreeSet::new();
    let mut constraints: BTreeSet<String> = BTreeSet::new();

    for (i, bytes) in def.def.iter().enumerate() {
        let Ok(op) = pb::Op::decode(bytes.as_slice()) else {
            exclusions.push((i, Exclusion::Undecodable));
            continue;
        };
        for why in hazards(&op) {
            exclusions.push((i, why));
        }
        if let Some(p) = &op.platform {
            platforms.insert(plat_str(p));
        }
        if let Some(c) = &op.constraints {
            constraints.extend(c.filter.iter().cloned());
        }
    }

    // One declaring vertex pins the tree; two that disagree ground it,
    // because a subtree is built by ONE peer or not at all.
    let platform = match platforms.len() {
        0 => Platform::Any,
        1 => Platform::Pinned(platforms.into_iter().next().expect("len 1")),
        _ => Platform::Conflict(platforms),
    };

    Verdict {
        exclusions,
        platform,
        constraints,
        ops: def.def.len(),
    }
}

/// Why a worker said no to an offer.
///
/// Every one is a REASON, not an error: a decline is the protocol working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No free slot. The backpressure signal proper.
    Saturated,
    /// This peer cannot run it - the subtree is pinned elsewhere.
    WrongPlatform { wants: String, have: String },
    /// The subtree may not travel at all; whoever offered it should not
    /// have. Refusing rather than trusting the offerer's check is cheap.
    Undispatchable(Exclusion),
}

/// A worker's current occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Load {
    pub slots: usize,
    /// Subtrees being built for a PEER - each has a machine blocked on it.
    pub peer: usize,
    /// Ordinary jobs from the driver. Nobody is waiting on these.
    pub driver: usize,
}

impl Load {
    pub fn free(&self) -> usize {
        self.slots.saturating_sub(self.peer + self.driver)
    }
}

/// What a worker should pick up next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // driver line, not the proxy - see lease.rs
pub enum Next {
    /// A peer is blocked on this. Always first.
    Peer(u64),
    Driver(u64),
    Idle,
}

/// Should this worker accept an offered subtree?
/// Against a stated policy.
///
/// The policy is a PARAMETER because two components were deciding this same
/// question by different rules: the gateway lifted cache mounts from the
/// environment and offered the subtree, the driver's `offer_order` asked
/// `consider` - which had no policy - and refused it. Four idle workers of
/// the right platform, and the offer died on the arbiter.
///
/// Whoever decides to offer and whoever decides where must be asking the
/// same question. This is that question.
pub fn consider(load: Load, v: &Verdict, my_platform: &str, allow: Allow) -> Result<(), Refusal> {
    // Checked in this order on purpose. "This should never have been
    // offered" and "I can never run this" must outrank "not right now":
    // Saturated invites the offer back, and an offer that can never be
    // accepted would then circulate forever.
    if !v.dispatchable_when(allow) {
        // Report the first blocker the POLICY did not lift, so the message
        // names something the operator can act on rather than whichever
        // hazard happens to sort first.
        let why = v
            .exclusions
            .iter()
            .map(|(_, e)| e)
            .find(|e| !lifted_by(e, allow))
            .cloned()
            .unwrap_or(Exclusion::Undecodable);
        return Err(Refusal::Undispatchable(why));
    }
    let wants = match &v.platform {
        Platform::Any => None,
        Platform::Pinned(p) => Some(p.clone()),
        // Nowhere can run a split subtree, so no platform string matches.
        Platform::Conflict(set) => Some(set.iter().cloned().collect::<Vec<_>>().join(" and ")),
    };
    if let Some(wants) = wants {
        if wants != my_platform {
            return Err(Refusal::WrongPlatform {
                wants,
                have: my_platform.to_owned(),
            });
        }
    }
    if load.free() == 0 {
        return Err(Refusal::Saturated);
    }
    Ok(())
}

/// Which pending item to start. Principle 12: finishing beats starting.
#[allow(dead_code)] // driver line, not the proxy - see lease.rs
pub fn next_work(load: Load, peer: &[u64], driver: &[u64]) -> Next {
    // Start nothing when full. Completions set makespan, starts do not - a
    // fleet that always accepts converges on every machine being 90%
    // through something and nothing finishing.
    if load.free() == 0 {
        return Next::Idle;
    }
    // Peer work first, unconditionally. A subdivided branch has a machine
    // BLOCKED on it; new driver work does not. Without this, subdivision is
    // a regression: workers sit on warm state waiting for peers who took
    // fresh driver work instead.
    if let Some(&j) = peer.first() {
        return Next::Peer(j);
    }
    driver.first().map_or(Next::Idle, |&j| Next::Driver(j))
}

/// Below this, shipping a subtree costs more than building it.
///
/// NOT a cost model, which the plan rules out: it is the stall trigger.
/// Anything still running after this is BY DEFINITION not a 5ms `echo`, and
/// that is self-calibrating in a way a threshold table is not. The value is
/// deliberately coarse - two orders of magnitude above the millisecond
/// vertices (58% of one shard's execs are `echo`/`test`/`diff`/`mkdir`) and
/// an order below the stem at ~94s - so being wrong by a factor of two
/// changes no decision.
#[allow(dead_code)] // driver line, not the proxy - see lease.rs
pub const STALL: std::time::Duration = std::time::Duration::from_secs(5);

/// A peer we could offer this subtree to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: u64,
    pub platform: String,
    pub load: Load,
}

/// Is this subtree big enough to be worth sending anywhere?
///
/// `est_p90` is the timing store's answer for this target, when it has one.
/// A first build has none, and the fallback is the stall itself - which is
/// why no cold-start path has to be maintained separately.
#[allow(dead_code)] // driver line, not the proxy - see lease.rs
pub fn worth_offering(
    est_p90: Option<std::time::Duration>,
    running_for: std::time::Duration,
) -> bool {
    // An estimate answers before the work has run, which is the whole value
    // of keeping one: run two does not re-learn what run one already knew.
    // Without one, the stall IS the answer, so the first build of anything
    // needs no special case.
    est_p90.unwrap_or(running_for) > STALL
}

/// Who to offer this subtree to, best first. Empty means build it yourself.
pub fn offer_order(v: &Verdict, cands: &[Candidate], allow: Allow) -> Vec<u64> {
    let mut able: Vec<&Candidate> = cands
        .iter()
        .filter(|c| {
            // Ask the same question the peer will. A candidate that would
            // decline is a wasted round trip, and a saturated one says so
            // in its own load without being asked.
            !matches!(
                consider(c.load, v, &c.platform, allow),
                Err(Refusal::Undispatchable(_)) | Err(Refusal::WrongPlatform { .. })
            ) && c.load.free() > 0
        })
        .collect();
    // Emptiest first, so the work starts soonest. Ties on id, so two
    // drivers deciding from the same state offer in the same order rather
    // than crossing over.
    able.sort_by_key(|c| (std::cmp::Reverse(c.load.free()), c.id));
    able.into_iter().map(|c| c.id).collect()
}

/// One offered subtree, tracked through the fleet until someone takes it.
///
/// The driver ARBITRATES: it offers to one peer at a time, in
/// [`offer_order`], and moves on when refused. It never assigns, and it
/// never broadcasts - two peers building the same subtree is the duplicate
/// work the fleet exists to avoid, and principle 3 would then have to throw
/// one result away.
///
/// Exhausting the candidates is not a failure. It means the requester
/// builds it itself, which is what it would have done without dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    order: Vec<u64>,
    next: usize,
    /// Who is currently holding the offer, if anyone.
    outstanding: Option<u64>,
}

impl Placement {
    pub fn new(v: &Verdict, cands: &[Candidate], allow: Allow) -> Self {
        Placement {
            order: offer_order(v, cands, allow),
            next: 0,
            outstanding: None,
        }
    }

    /// Who is holding this offer right now, if anyone.
    ///
    /// Exposed so the driver can price a worker's outstanding LEADS when it
    /// picks the next one. Derived from the placements themselves rather
    /// than tallied alongside them: a second counter for the same fact is
    /// how the proxy ended up dispatching against a number nothing updated.
    pub fn holder(&self) -> Option<u64> {
        self.outstanding
    }

    /// Offer to the next peer. `None` = nobody left; build it yourself.
    pub fn offer(&mut self) -> Option<u64> {
        let who = self.order.get(self.next).copied();
        self.next += 1;
        self.outstanding = who;
        who
    }

    /// That peer said no. Returns the next to try, if any.
    ///
    /// Ignores a reply from anyone who is not the current holder. Replies
    /// race: a stale decline from a peer we already gave up on would
    /// otherwise skip the one currently holding the offer, leaving the
    /// subtree placed nowhere while we believe it placed.
    pub fn declined(&mut self, who: u64) -> Option<u64> {
        if self.outstanding != Some(who) {
            return self.outstanding;
        }
        self.offer()
    }

    /// Is an offer currently outstanding with someone?
    #[allow(dead_code)] // driver line, not the proxy - see lease.rs
    pub fn outstanding(&self) -> Option<u64> {
        self.outstanding
    }
}

/// What a subtree needs from outside itself.
///
/// An LLB subtree is reachability-closed down to its SOURCE ops, so its
/// frontier is exactly those sources - and their schemes say what the
/// handover costs. Principle 11's table, read off the graph:
///
/// - `docker-image://` — a digest any machine can pull. FREE: the peer
///   needs nothing from us at all, which is the best possible handover.
/// - `local://` — the build context, which lives on the invoking machine
///   and arrives by filesync. This is `LOCALLY` in all but name.
/// - anything else (`git://`, `http://`) — fetchable, but by whom and at
///   what cost is not ours to assume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontier {
    pub registry: usize,
    pub local: usize,
    pub other: usize,
}

impl Frontier {
    /// Nothing has to travel from us for a peer to build this.
    pub fn is_free(&self) -> bool {
        self.local == 0 && self.other == 0 && self.registry > 0
    }
}

/// One possible cut, and what it would cost to hand over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    /// Index into `Definition.def`.
    pub root: usize,
    /// Ops in the subtree rooted here, including the root.
    pub ops: usize,
    pub frontier: Frontier,
}

/// What a whole `Definition` offers a dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub ops: usize,
    /// Every cut worth naming, largest subtree first.
    pub cuts: Vec<Cut>,
}

impl Analysis {
    /// Cuts a peer could take with no transfer from us.
    pub fn free_cuts(&self) -> impl Iterator<Item = &Cut> {
        self.cuts.iter().filter(|c| c.frontier.is_free())
    }
}

/// Read a graph and report where it could be cut.
///
/// Reports rather than decides: this is the instrument that answers "is
/// there anything here worth dispatching" before any mechanism is built to
/// dispatch it.
pub fn analyse(def: &pb::Definition, min_ops: usize) -> Analysis {
    // Ops are addressed by the digest of their encoded bytes, which is how
    // buildkit itself links them - so the index is ours and the digest is
    // the graph's.
    let decoded: Vec<Option<pb::Op>> = def
        .def
        .iter()
        .map(|b| pb::Op::decode(b.as_slice()).ok())
        .collect();
    let by_digest: BTreeMap<String, usize> = def
        .def
        .iter()
        .enumerate()
        .map(|(i, b)| (format!("sha256:{}", crate::store::sha256_hex(b)), i))
        .collect();

    let mut cuts = Vec::new();
    for root in 0..def.def.len() {
        let mut seen = std::collections::BTreeSet::new();
        let mut frontier = Frontier::default();
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            // An op we cannot read still COUNTS: shrinking the measured
            // subtree because a byte string surprised us would make a wide
            // cut look narrow, which is the wrong direction to be wrong in.
            let Some(op) = decoded.get(i).and_then(Option::as_ref) else {
                continue;
            };
            if let Some(pb::op::Op::Source(src)) = &op.op {
                match src.identifier.split_once("://").map(|(s, _)| s) {
                    Some("docker-image") => frontier.registry += 1,
                    Some("local") => frontier.local += 1,
                    _ => frontier.other += 1,
                }
            }
            for input in &op.inputs {
                if let Some(&j) = by_digest.get(&input.digest) {
                    stack.push(j);
                }
            }
        }
        if seen.len() >= min_ops {
            cuts.push(Cut {
                root,
                ops: seen.len(),
                frontier,
            });
        }
    }
    // Largest first: the biggest subtree with a free frontier is the one
    // worth asking about, and a caller reading only the head should get it.
    cuts.sort_by_key(|c| (std::cmp::Reverse(c.ops), c.root));
    Analysis {
        ops: def.def.len(),
        cuts,
    }
}

/// Rewrite every `local://` source to fetch from somewhere a peer can reach.
///
/// The build context has exactly one holder - the client - and it arrives by
/// filesync over the session. Measured: a 32 MiB context is 32 MiB through
/// whoever proxies that session. So a peer cannot obtain it, and every
/// `COPY`-bearing subtree is undispatchable without putting the coordinator
/// on the data path for the whole repository, once per peer.
///
/// Principle 9 already answers this shape. The client is an ORIGIN, and the
/// rule for origins is fetch once into the fleet and serve peer to peer. We
/// receive the context anyway (we are proxying the session); publishing it
/// as content and rewriting the graph to point at that content turns N
/// transfers through the coordinator into one, after which the mesh serves
/// it like any other blob.
///
/// # The digest cascade, which is the whole difficulty
///
/// LLB ops reference each other BY THE DIGEST OF THEIR BYTES. Change one
/// op and its digest changes, so every op that inputs from it now points at
/// something that does not exist - and those ops' digests change in turn,
/// all the way to the root. A rewrite is therefore not a substitution; it is
/// a rebuild of the graph in topological order.
///
/// `replacement` is asked per local source NAME (`context`, `dockerfile`),
/// because those are different directories. Returning `None` leaves that
/// source alone, which keeps the subtree undispatchable rather than wrong.
/// Rewrite REGISTRY sources too, so a peer needs no upstream at all.
///
/// Same cascade as [`rewrite_local_sources`] and the same per-source rule:
/// each reference is asked for separately, because two different images
/// rewritten to one identifier would encode identically and collapse.
pub fn rewrite_registry_sources(
    def: &pb::Definition,
    replacement: &dyn Fn(&str) -> Option<String>,
) -> pb::Definition {
    rewrite_sources(def, "docker-image://", replacement)
}

pub fn rewrite_local_sources(
    def: &pb::Definition,
    replacement: &dyn Fn(&str) -> Option<String>,
) -> pb::Definition {
    rewrite_sources(def, "local://", replacement)
}

/// The cascade, shared by both rewrites.
fn rewrite_sources(
    def: &pb::Definition,
    scheme: &str,
    replacement: &dyn Fn(&str) -> Option<String>,
) -> pb::Definition {
    let digest = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));

    // Nothing local: return the bytes untouched rather than equivalent.
    // Reserialising an unchanged graph changes every digest for nothing,
    // and buildkit would read the result as a cache miss for the whole
    // build - the most expensive possible no-op.
    let has_local = def.def.iter().any(|b| {
        matches!(
            pb::Op::decode(b.as_slice()).ok().and_then(|o| o.op),
            Some(pb::op::Op::Source(s)) if s.identifier.starts_with(scheme)
        )
    });
    if !has_local {
        return def.clone();
    }

    // Ops arrive topologically sorted (buildkit marshals them that way), so
    // one forward pass suffices: by the time an op is reached, everything it
    // inputs from has been rewritten and its new digest is known.
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(def.def.len());

    for bytes in &def.def {
        let Ok(mut op) = pb::Op::decode(bytes.as_slice()) else {
            // Unreadable: carry it verbatim. Its digest is unchanged, so
            // anything referencing it still resolves.
            out.push(bytes.clone());
            continue;
        };
        let before = digest(bytes);

        for input in &mut op.inputs {
            if let Some(new) = remap.get(&input.digest) {
                input.digest = new.clone();
            }
        }
        if let Some(pb::op::Op::Source(src)) = &mut op.op {
            if let Some(name) = src.identifier.strip_prefix(scheme) {
                // `replacement` returns a FULL identifier including its
                // scheme - the identifier is replaced wholesale, and a bare
                // `host:port/name:tag` is rejected by buildkit as invalid.
                // Per SOURCE, not one replacement for all of them. Earthly
                // passes `context` and `dockerfile` separately and they are
                // different directories; rewriting both to one identifier
                // makes them encode identically, so they collapse to one op
                // and the second content silently becomes the first.
                let Some(new) = replacement(name) else {
                    out.push(bytes.clone());
                    continue;
                };
                src.identifier = new;
                // Local sources carry filesync attrs - include patterns,
                // session ids - that mean nothing to a registry source and
                // would be a stale reference to a session the peer has no
                // part in.
                src.attrs.clear();
            }
        }

        let rebuilt = op.encode_to_vec();
        let after = digest(&rebuilt);
        if after != before {
            remap.insert(before, after);
        }
        out.push(rebuilt);
    }

    // Metadata is keyed by op digest, so it has to follow the remap or the
    // graph loses its descriptions and cache hints.
    let metadata = def
        .metadata
        .iter()
        .map(|(k, v)| {
            (
                remap.get(k).cloned().unwrap_or_else(|| k.clone()),
                v.clone(),
            )
        })
        .collect();

    pb::Definition {
        def: out,
        metadata,
        source: def.source.clone(),
    }
}

/// A graph that is nothing but "fetch this image".
///
/// The other half of adoption. A peer builds the real work and publishes
/// it; the daemon holding the client's job is then handed THIS, so it
/// fetches content instead of building and returns a ref of its own -
/// which is what makes the client's later `read_dir` and `return` work,
/// since refs and jobs are both daemon-local.
///
/// Liveness never requires keeping your OWN bytes, only some bytes
/// (principle 3). Here it does not even require building them.
pub fn import_graph(reference: &str) -> pb::Definition {
    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: reference.to_owned(),
            ..Default::default()
        })),
        ..Default::default()
    };
    let src_b = src.encode_to_vec();
    let digest = format!("sha256:{}", crate::store::sha256_hex(&src_b));
    // LLB needs its terminal op: no `op` of its own, one input naming the
    // result. Without it buildkit solves nothing and calls that success.
    let term = pb::Op {
        inputs: vec![pb::Input {
            digest: digest.clone(),
            index: 0,
        }],
        ..Default::default()
    };
    pb::Definition {
        metadata: [(digest, pb::OpMetadata::default())].into_iter().collect(),
        def: vec![src_b, term.encode_to_vec()],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard_buildkit_proto::pb::{op::Op as OpKind, ExecOp, Meta, Mount, SecretEnv};

    /// An op that does ordinary work: no mounts, no secrets, sandboxed.
    fn plain() -> pb::Op {
        pb::Op {
            op: Some(OpKind::Exec(ExecOp {
                meta: Some(Meta {
                    args: vec!["/bin/sh".into(), "-c".into(), "true".into()],
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn exec_of(o: &pb::Op) -> ExecOp {
        match o.op.clone() {
            Some(OpKind::Exec(e)) => e,
            _ => panic!("not an exec"),
        }
    }

    fn with_exec(mut o: pb::Op, f: impl FnOnce(&mut ExecOp)) -> pb::Op {
        let mut e = exec_of(&o);
        f(&mut e);
        o.op = Some(OpKind::Exec(e));
        o
    }

    fn def(ops: Vec<pb::Op>) -> pb::Definition {
        pb::Definition {
            def: ops.iter().map(|o| o.encode_to_vec()).collect(),
            ..Default::default()
        }
    }

    fn mount(kind: pb::MountType) -> Mount {
        let mut m = Mount {
            dest: "/m".into(),
            mount_type: kind as i32,
            ..Default::default()
        };
        if kind == pb::MountType::Cache {
            m.cache_opt = Some(pb::CacheOpt::default());
        }
        m
    }

    /// A mount by RAW type number - the only way to build a fork's.
    fn mount_of_type(kind: i32) -> Mount {
        Mount {
            dest: "/m".into(),
            mount_type: kind,
            ..Default::default()
        }
    }

    // --- M4: the offer, and the right to refuse ------------------------

    fn load(slots: usize, peer: usize, driver: usize) -> Load {
        Load {
            slots,
            peer,
            driver,
        }
    }

    fn ok_verdict() -> Verdict {
        inspect(&def(vec![plain()]))
    }

    #[test]
    fn a_worker_may_refuse_and_that_is_the_backpressure() {
        let v = ok_verdict();
        assert_eq!(
            consider(load(4, 1, 1), &v, "linux/arm64", Allow::default()),
            Ok(())
        );

        // Saturated is the signal principle 12 is built on: a driver that
        // cannot place work has learned the fleet is full without a metric.
        assert_eq!(
            consider(load(2, 1, 1), &v, "linux/arm64", Allow::default()),
            Err(Refusal::Saturated)
        );

        // A pinned subtree offered to the wrong machine. Emulation is a
        // trap, not a fallback - accepting here is how an amd64 box ends up
        // running arm64 work slowly and the queue calls it scheduled.
        let pinned = {
            let mut o = plain();
            o.platform = Some(pb::Platform {
                os: "linux".into(),
                architecture: "arm64".into(),
                ..Default::default()
            });
            inspect(&def(vec![o]))
        };
        assert_eq!(
            consider(load(4, 0, 0), &pinned, "linux/amd64", Allow::default()),
            Err(Refusal::WrongPlatform {
                wants: "linux/arm64".into(),
                have: "linux/amd64".into()
            })
        );
        assert_eq!(
            consider(load(4, 0, 0), &pinned, "linux/arm64", Allow::default()),
            Ok(())
        );
        // An unpinned subtree runs anywhere.
        assert_eq!(
            consider(load(4, 0, 0), &v, "windows/amd64", Allow::default()),
            Ok(())
        );

        // The offerer already checked dispatchability. Checking again costs
        // one comparison and means a bug there cannot ship us a secret.
        let bad = inspect(&def(vec![with_exec(plain(), |e| {
            e.secretenv = vec![SecretEnv {
                id: "tok".into(),
                name: "TOK".into(),
                ..Default::default()
            }]
        })]));
        assert_eq!(
            consider(load(4, 0, 0), &bad, "linux/arm64", Allow::default()),
            Err(Refusal::Undispatchable(Exclusion::Secret))
        );

        // Refusing an undispatchable subtree outranks being saturated: the
        // offer was wrong, and saying "try me later" invites it back.
        assert_eq!(
            consider(load(1, 1, 0), &bad, "linux/arm64", Allow::default()),
            Err(Refusal::Undispatchable(Exclusion::Secret))
        );
    }

    #[test]
    fn peer_work_goes_first_or_subdivision_is_a_regression() {
        // Principle 12. When A subdivides and hands a branch to B, A is
        // BLOCKED on B. If B prefers fresh driver work, A stalls while
        // holding everything it has built - so the very mechanism meant to
        // improve balance produces a fleet of blocked machines sitting on
        // warm state.
        assert_eq!(next_work(load(4, 0, 0), &[7], &[1, 2]), Next::Peer(7));
        assert_eq!(next_work(load(4, 0, 0), &[], &[1, 2]), Next::Driver(1));
        assert_eq!(next_work(load(4, 0, 0), &[], &[]), Next::Idle);

        // Order within a queue is arrival order; the PRIORITY is between
        // the queues, not inside them.
        assert_eq!(next_work(load(4, 0, 0), &[9, 3], &[]), Next::Peer(9));

        // No free slot: start nothing. Completions set makespan, starts do
        // not - a fleet that always accepts converges on every machine
        // being 90% through something and nothing finishing.
        assert_eq!(next_work(load(2, 1, 1), &[7], &[1]), Next::Idle);
        assert_eq!(next_work(load(0, 0, 0), &[7], &[1]), Next::Idle);
    }

    #[test]
    fn the_wire_stays_readable_to_a_peer_that_has_not_been_updated() {
        // postcard encodes an enum variant by INDEX, so inserting one in
        // the middle silently reinterprets every later variant on a mixed-
        // version fleet - a Ping read as a Finalize. New variants go on the
        // END, and this pins the ones that already shipped.
        use crate::mesh::{Dig, D2W, W2D};
        let at = |v: &D2W| postcard::to_allocvec(v).unwrap()[0];
        assert_eq!(
            at(&D2W::Welcome {
                decentralized: true
            }),
            0
        );
        assert_eq!(
            at(&D2W::Run {
                job: 1,
                action: Dig {
                    hash: "a".into(),
                    size: 1
                }
            }),
            1
        );
        assert_eq!(at(&D2W::Blooms { peers: vec![] }), 2);
        assert_eq!(at(&D2W::Finalize { shard: 0, of: 1 }), 3);
        assert_eq!(at(&D2W::Ping { vitals: None }), 4);
        assert_eq!(at(&D2W::Exit), 5);
        assert_eq!(
            at(&D2W::Lead {
                job: 1,
                subtree: vec![],
                frontier: vec![]
            }),
            6,
            "Lead must be LAST - moving it renumbers everything before it"
        );

        // And it must survive the round trip with its payload intact: the
        // subtree is a serialised Definition and the frontier is what the
        // peer needs to fetch, so a lossy encode is a build that cannot start.
        let lead = D2W::Lead {
            job: 42,
            subtree: b"\x0a\x02hi".to_vec(),
            frontier: vec![Dig {
                hash: "beef".into(),
                size: 7,
            }],
        };
        let bytes = postcard::to_allocvec(&lead).unwrap();
        match postcard::from_bytes::<D2W>(&bytes).unwrap() {
            D2W::Lead {
                job,
                subtree,
                frontier,
            } => {
                assert_eq!(job, 42);
                assert_eq!(subtree, b"\x0a\x02hi".to_vec());
                assert_eq!(frontier.len(), 1);
                assert_eq!(frontier[0].hash, "beef");
            }
            other => panic!("round trip lost the variant: {other:?}"),
        }

        let decline = W2D::Decline {
            job: 42,
            why: "saturated".into(),
        };
        let bytes = postcard::to_allocvec(&decline).unwrap();
        assert!(matches!(
            postcard::from_bytes::<W2D>(&bytes).unwrap(),
            W2D::Decline { job: 42, .. }
        ));
    }

    #[test]
    fn only_work_worth_shipping_is_shipped() {
        use std::time::Duration;
        let s = Duration::from_secs;
        let ms = Duration::from_millis;

        // A cost model is only needed if a wrong decision is HARMFUL. The
        // plan rules one out in favour of the stall trigger: anything still
        // running after STALL is by definition not a 5ms echo.
        assert!(!worth_offering(Some(ms(5)), ms(1)), "an echo stays home");
        assert!(worth_offering(Some(s(94)), ms(1)), "the stem travels");

        // No estimate is the FIRST build of anything, and it must be
        // survivable rather than special-cased: the stall answers it.
        assert!(!worth_offering(None, ms(200)));
        assert!(worth_offering(None, s(30)), "still running - not an echo");

        // The estimate decides even before the work has run long, which is
        // the whole value of having one: run two does not wait to find out
        // what run one already learned.
        assert!(worth_offering(Some(s(60)), Duration::ZERO));
    }

    #[test]
    fn an_offer_goes_to_someone_who_could_actually_take_it() {
        let cand = |id, plat: &str, l: Load| Candidate {
            id,
            platform: plat.into(),
            load: l,
        };
        let v = ok_verdict();

        // Emptiest first: the work starts soonest, and the offer is least
        // likely to come back as a decline.
        let got = offer_order(
            &v,
            &[
                cand(1, "linux/arm64", load(4, 2, 1)),
                cand(2, "linux/arm64", load(4, 0, 0)),
                cand(3, "linux/arm64", load(4, 1, 0)),
            ],
            Allow::default(),
        );
        assert_eq!(got, vec![2, 3, 1]);

        // A saturated peer is not offered to at all - asking costs a round
        // trip to be told what its load already said.
        let got = offer_order(
            &v,
            &[
                cand(1, "linux/arm64", load(2, 1, 1)),
                cand(2, "linux/arm64", load(4, 0, 0)),
            ],
            Allow::default(),
        );
        assert_eq!(got, vec![2]);

        // Platform is honoured before anything else. Idle mac and windows
        // runners are NOT spare capacity for linux work, and emulation is a
        // trap rather than a fallback - a queue that hands arm64 work to an
        // amd64 box and calls it scheduled is the failure here.
        let pinned = {
            let mut o = plain();
            o.platform = Some(pb::Platform {
                os: "linux".into(),
                architecture: "arm64".into(),
                ..Default::default()
            });
            inspect(&def(vec![o]))
        };
        let got = offer_order(
            &pinned,
            &[
                cand(1, "linux/amd64", load(8, 0, 0)),
                cand(2, "darwin/arm64", load(8, 0, 0)),
                cand(3, "linux/arm64", load(4, 3, 0)),
            ],
            Allow::default(),
        );
        assert_eq!(got, vec![3], "the only peer that can run it, busy or not");

        // Nobody able => build it yourself. Duplicate work is always
        // correct; a stall is worse than the work we set out to avoid.
        assert_eq!(
            offer_order(
                &pinned,
                &[cand(1, "linux/amd64", load(8, 0, 0))],
                Allow::default()
            ),
            Vec::<u64>::new()
        );
        assert_eq!(offer_order(&v, &[], Allow::default()), Vec::<u64>::new());

        // An undispatchable subtree is offered to NOBODY, however idle the
        // fleet is - the exclusion is about the work, not the capacity.
        let bad = inspect(&def(vec![with_exec(plain(), |e| {
            e.mounts = vec![mount(pb::MountType::Cache)]
        })]));
        assert_eq!(
            offer_order(
                &bad,
                &[cand(1, "linux/arm64", load(8, 0, 0))],
                Allow::default()
            ),
            Vec::<u64>::new()
        );

        // Ties break on id, so two drivers deciding from the same state
        // offer in the same order rather than crossing over.
        let got = offer_order(
            &v,
            &[
                cand(9, "linux/arm64", load(4, 0, 0)),
                cand(2, "linux/arm64", load(4, 0, 0)),
            ],
            Allow::default(),
        );
        assert_eq!(got, vec![2, 9]);
    }

    #[test]
    fn a_subtree_is_offered_to_one_peer_at_a_time() {
        let c = |id, l: Load| Candidate {
            id,
            platform: "linux/arm64".into(),
            load: l,
        };
        let v = ok_verdict();
        let mut p = Placement::new(
            &v,
            &[
                c(1, load(4, 2, 0)),
                c(2, load(4, 0, 0)),
                c(3, load(4, 1, 0)),
            ],
            Allow::default(),
        );

        // Emptiest first, and ONE at a time. Broadcasting would have two
        // peers build the same subtree - the duplicate work the fleet
        // exists to avoid, and principle 3 would throw one result away.
        assert_eq!(p.offer(), Some(2));
        assert_eq!(p.outstanding(), Some(2));

        // A refusal moves to the next, and the refuser is not asked again.
        assert_eq!(p.declined(2), Some(3));
        assert_eq!(p.outstanding(), Some(3));
        assert_eq!(p.declined(3), Some(1));
        assert_eq!(p.declined(1), None, "nobody left");
        assert_eq!(p.outstanding(), None);

        // Exhausted is not a failure: the requester builds it, which is
        // what it would have done without dispatch at all.
        assert_eq!(p.offer(), None, "and it stays exhausted");
    }

    #[test]
    fn a_decline_from_someone_else_does_not_move_us_on() {
        // Replies race. A stale decline from a peer we already gave up on
        // must not skip the peer currently holding the offer - that would
        // leave the subtree placed nowhere while we believe it is placed.
        let c = |id| Candidate {
            id,
            platform: "linux/arm64".into(),
            load: load(4, 0, 0),
        };
        let mut p = Placement::new(&ok_verdict(), &[c(1), c(2), c(3)], Allow::default());
        assert_eq!(p.offer(), Some(1));
        assert_eq!(p.declined(1), Some(2));

        // 1 declining again, or a reply arriving late, changes nothing.
        assert_eq!(p.declined(1), Some(2));
        assert_eq!(p.outstanding(), Some(2), "2 still holds it");
        assert_eq!(p.declined(99), Some(2), "a stranger cannot move us on");
    }

    #[test]
    fn an_unplaceable_subtree_is_offered_to_nobody() {
        let c = |id| Candidate {
            id,
            platform: "linux/arm64".into(),
            load: load(8, 0, 0),
        };
        // Undispatchable: no fleet, however idle, may be offered it.
        let bad = inspect(&def(vec![with_exec(plain(), |e| {
            e.mounts = vec![mount(pb::MountType::Cache)]
        })]));
        let mut p = Placement::new(&bad, &[c(1), c(2)], Allow::default());
        assert_eq!(p.offer(), None);

        // An empty fleet is the same answer by a different route.
        let mut p = Placement::new(&ok_verdict(), &[], Allow::default());
        assert_eq!(p.offer(), None);
    }

    fn src(id: &str) -> pb::Op {
        pb::Op {
            op: Some(OpKind::Source(pb::SourceOp {
                identifier: id.into(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Wire `op` to depend on the ops at `inputs` (indices into the list
    /// being built), using the digest convention buildkit itself uses.
    fn chain(ops: Vec<(pb::Op, Vec<usize>)>) -> pb::Definition {
        let mut encoded: Vec<Vec<u8>> = Vec::new();
        for (mut op, inputs) in ops {
            op.inputs = inputs
                .iter()
                .map(|i| pb::Input {
                    digest: format!("sha256:{}", crate::store::sha256_hex(&encoded[*i])),
                    index: 0,
                })
                .collect();
            encoded.push(op.encode_to_vec());
        }
        pb::Definition {
            def: encoded,
            ..Default::default()
        }
    }

    #[test]
    fn a_registry_rooted_chain_is_the_best_possible_handover() {
        // FROM alpine -> RUN -> RUN. Everything it needs is a public
        // digest, so a peer needs NOTHING from us: principle 11's free
        // seam, read straight off the graph.
        let d = chain(vec![
            (src("docker-image://docker.io/library/alpine:3.20"), vec![]),
            (plain(), vec![0]),
            (plain(), vec![1]),
        ]);
        let a = analyse(&d, 2);
        assert_eq!(a.ops, 3);

        let top = a.cuts.first().expect("a cut at the top of the chain");
        assert_eq!(top.root, 2);
        assert_eq!(top.ops, 3, "the whole chain is reachable from the top");
        assert_eq!(
            top.frontier,
            Frontier {
                registry: 1,
                local: 0,
                other: 0
            }
        );
        assert!(top.frontier.is_free());
        assert_eq!(a.free_cuts().count(), 2, "the two multi-op subtrees");
    }

    #[test]
    fn a_context_rooted_chain_is_not_free_and_that_is_the_point() {
        // `local://` is the build context - it lives on the invoking
        // machine and arrives by filesync. This is LOCALLY in all but
        // name, and a peer cannot serve itself from it.
        let d = chain(vec![(src("local://context"), vec![]), (plain(), vec![0])]);
        let a = analyse(&d, 2);
        let top = a.cuts.first().unwrap();
        assert_eq!(
            top.frontier,
            Frontier {
                registry: 0,
                local: 1,
                other: 0
            }
        );
        assert!(!top.frontier.is_free());
        assert_eq!(a.free_cuts().count(), 0);

        // A chain that touches BOTH is not free either: one filesync
        // input is enough to ground the handover.
        let d = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (src("local://context"), vec![]),
            (plain(), vec![0, 1]),
        ]);
        let top = analyse(&d, 2).cuts.into_iter().next().unwrap();
        assert_eq!(
            top.frontier,
            Frontier {
                registry: 1,
                local: 1,
                other: 0
            }
        );
        assert!(!top.frontier.is_free());
    }

    #[test]
    fn tiny_subtrees_are_not_reported_as_opportunities() {
        // Over half of every shard is milliseconds of work. A cut list
        // that includes every single-op subtree is a list of things not
        // worth shipping, and it would bury the ones that are.
        let d = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (plain(), vec![0]),
        ]);
        assert_eq!(analyse(&d, 2).cuts.len(), 1, "only the 2-op subtree");
        assert_eq!(analyse(&d, 3).cuts.len(), 0, "nothing that big here");
        assert_eq!(analyse(&pb::Definition::default(), 1).cuts.len(), 0);

        // An unreadable op must not silently shrink a subtree's measured
        // size - that would make a wide cut look narrow.
        let mut d = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (plain(), vec![0]),
        ]);
        d.def.push(b"not a protobuf".to_vec());
        assert_eq!(analyse(&d, 2).ops, 3, "the op still counts as present");
    }

    #[test]
    fn rewriting_a_source_relinks_every_op_that_depended_on_it() {
        // The cascade. ops reference each other by the digest of their
        // BYTES, so changing a source changes its digest, which orphans its
        // consumer, which changes that op's digest too, to the root.
        let d = chain(vec![
            (src("local://context"), vec![]),
            (plain(), vec![0]),
            (plain(), vec![1]),
        ]);
        let out = rewrite_local_sources(&d, &|n| {
            Some(format!("docker-image://mesh.local/{n}@sha256:abc"))
        });

        assert_eq!(out.def.len(), d.def.len(), "no op may be added or lost");
        let a = analyse(&out, 1);
        assert_eq!(
            a.cuts.iter().map(|c| c.ops).max(),
            Some(3),
            "the chain must still be a chain - if the relink failed, the \
             top op reaches nothing and its closure is 1"
        );

        // The point of the exercise: no local source survives, so the
        // frontier is free and the subtree can travel.
        let top = a.cuts.first().unwrap();
        assert_eq!(top.frontier.local, 0);
        assert_eq!(top.frontier.registry, 1);
        assert!(top.frontier.is_free(), "{:?}", top.frontier);
    }

    #[test]
    fn a_graph_with_nothing_local_is_returned_untouched() {
        // Byte-identical, not merely equivalent: a rewrite that reserialises
        // an untouched graph changes every digest for nothing, and buildkit
        // would treat the result as a cache miss for the entire build.
        let d = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (plain(), vec![0]),
        ]);
        let out = rewrite_local_sources(&d, &|n| Some(format!("docker-image://x/{n}")));
        assert_eq!(out.def, d.def, "an untouched graph must not be rebuilt");
    }

    #[test]
    fn every_local_source_is_rewritten_not_just_the_first() {
        // Two contexts is normal - earthly passes `context` and
        // `dockerfile` separately, and a build with several COPY roots has
        // more. Rewriting one and leaving the rest still grounds the cut.
        let d = chain(vec![
            (src("local://context"), vec![]),
            (src("local://dockerfile"), vec![]),
            (plain(), vec![0, 1]),
        ]);
        // Each gets its OWN replacement. One identifier for both would make
        // them encode identically, collapse to a single op, and hand the
        // dockerfile's content to whatever wanted the context.
        let out = rewrite_local_sources(&d, &|n| Some(format!("docker-image://mesh/{n}")));
        let top = analyse(&out, 1).cuts.first().cloned().unwrap();
        assert_eq!(top.frontier.local, 0, "both must go");
        assert_eq!(top.frontier.registry, 2, "and they must stay DISTINCT");
        assert_eq!(top.ops, 3);

        // A source we have no replacement for is left alone - the subtree
        // stays undispatchable, which is the safe answer, rather than being
        // rewritten to point at content nobody published.
        let partial = rewrite_local_sources(&d, &|n| {
            (n == "context").then(|| "docker-image://mesh/context".to_owned())
        });
        let top = analyse(&partial, 1).cuts.first().cloned().unwrap();
        assert_eq!(top.frontier.local, 1, "the unmapped one survives");
        assert!(!top.frontier.is_free());
    }

    #[test]
    fn an_import_graph_fetches_instead_of_building() {
        let d = import_graph("docker-image://mesh.local/rebuck2/adopted:abc");
        // Two ops: the source and LLB's terminal. Omitting the terminal
        // makes buildkit solve nothing and report success, which is the
        // most misleading answer available.
        assert_eq!(d.def.len(), 2);
        assert_eq!(d.metadata.len(), 1, "the source is described");

        let a = analyse(&d, 1);
        let top = a.cuts.first().expect("a cut");
        assert_eq!(top.ops, 2, "the terminal reaches the source");
        assert_eq!(
            top.frontier,
            Frontier {
                registry: 1,
                local: 0,
                other: 0
            }
        );
        assert!(
            top.frontier.is_free(),
            "an adopted result must itself be free to travel, or adoption \
             just moves the problem one machine along"
        );

        // Nothing to execute: an import that carried an exec would be
        // building, not adopting.
        assert!(inspect(&d).dispatchable());
        assert_eq!(inspect(&d).exclusions, vec![]);
    }

    #[test]
    fn ordinary_work_travels() {
        let v = inspect(&def(vec![plain(), plain(), plain()]));
        assert_eq!(v.ops, 3);
        assert_eq!(v.exclusions, vec![]);
        assert_eq!(v.platform, Platform::Any);
        assert!(v.dispatchable());

        // An empty definition is not an error - it is nothing to send.
        let empty = inspect(&def(vec![]));
        assert_eq!(empty.ops, 0);
        assert!(empty.dispatchable());
    }

    /// The three lifts compose, and none of them lifts a fourth thing.
    ///
    /// Each arm is independent by construction, which is exactly the kind of
    /// claim that stops being true when someone adds the next one. A graph
    /// carrying all three tolerable hazards must dispatch when all three are
    /// allowed, and a single insecure exec must still ground it however many
    /// permissions are granted.
    #[test]
    fn the_lifts_compose_and_do_not_widen() {
        let three = Verdict {
            exclusions: vec![
                (0, Exclusion::CacheMount),
                (1, Exclusion::Secret),
                (2, Exclusion::SshAgent),
            ],
            platform: Platform::Any,
            constraints: Default::default(),
            ops: 3,
        };
        assert!(!three.dispatchable());
        assert!(!three.dispatchable_when(Allow {
            secrets: true,
            caches: true,
            agent: false,
        }));
        assert!(three.dispatchable_when(Allow {
            secrets: true,
            caches: true,
            agent: true,
        }));

        // A privilege is never lifted, whatever else is.
        let privileged = Verdict {
            exclusions: vec![(0, Exclusion::Insecure)],
            ..three.clone()
        };
        assert!(!privileged.dispatchable_when(Allow {
            secrets: true,
            caches: true,
            agent: true,
        }));
    }

    #[test]
    fn one_hazard_anywhere_excludes_the_whole_subtree() {
        // Principle 10: the verdict is about the tree, not the op. `def`
        // builds UNLINKED ops, so this is the disconnected case - three
        // roots, one of them hazardous. Upward propagation through a real
        // edge is `a_clean_branch_does_not_escape_its_sibling` below.
        for (hazard, why) in [
            (
                with_exec(plain(), |e| e.mounts = vec![mount(pb::MountType::Cache)]),
                Exclusion::CacheMount,
            ),
            (
                with_exec(plain(), |e| e.mounts = vec![mount(pb::MountType::Secret)]),
                Exclusion::Secret,
            ),
            (
                with_exec(plain(), |e| e.mounts = vec![mount(pb::MountType::Ssh)]),
                Exclusion::SshAgent,
            ),
            (
                with_exec(plain(), |e| {
                    e.secretenv = vec![SecretEnv {
                        id: "npm-token".into(),
                        name: "NPM_TOKEN".into(),
                        ..Default::default()
                    }]
                }),
                Exclusion::Secret,
            ),
            (
                with_exec(plain(), |e| e.security = pb::SecurityMode::Insecure as i32),
                Exclusion::Insecure,
            ),
            (
                with_exec(plain(), |e| e.network = pb::NetMode::Host as i32),
                Exclusion::HostNetwork,
            ),
        ] {
            let v = inspect(&def(vec![plain(), hazard, plain()]));
            assert_eq!(v.exclusions, vec![(1, why.clone())], "{why:?}");
            assert!(!v.dispatchable(), "{why:?} must ground the whole subtree");
        }

        // A plain bind mount and a tmpfs are NOT hazards - excluding them
        // would ground almost every real subtree and the mechanism would
        // have nothing left to dispatch.
        for ok in [pb::MountType::Bind, pb::MountType::Tmpfs] {
            let v = inspect(&def(vec![with_exec(plain(), |e| {
                e.mounts = vec![mount(ok)]
            })]));
            assert!(v.dispatchable(), "{ok:?} is ordinary");
        }
    }

    #[test]
    fn one_op_can_carry_two_hazards_and_both_must_count() {
        // `every_blocker_is_reported_not_just_the_first` uses one hazard per
        // OP, so it says nothing about an op carrying two - and a real
        // earthly RUN carries exactly that: a go-mod cache mount and the
        // debugger's socket on the same exec.
        //
        // With only the first reported, lifting it makes the op look clean:
        // REBUCK2_PEER_CACHE_MOUNTS=1 lifts CacheMount, nothing mentions the
        // socket, and the graph is offered to a worker that builds without a
        // session.
        //
        // The bug is real, but do NOT credit it with the earthly decline this
        // was written next to: that one was mount type 101, and is
        // `a_fork_only_mount_type_is_a_blocker_not_a_blank`. Fixing this
        // changed that measurement by nothing at all.
        let mut e = exec_of(&plain());
        e.mounts = vec![mount(pb::MountType::Cache), mount(pb::MountType::Ssh)];
        let mut op = plain();
        op.op = Some(OpKind::Exec(e));

        let v = inspect(&def(vec![op]));
        assert!(
            v.exclusions
                .iter()
                .any(|(_, x)| *x == Exclusion::CacheMount),
            "cache mount not reported: {:?}",
            v.exclusions
        );
        assert!(
            v.exclusions.iter().any(|(_, x)| *x == Exclusion::SshAgent),
            "ssh mount hidden behind the cache mount: {:?}",
            v.exclusions
        );

        // And the point of reporting both: lifting ONE must not clear the op.
        assert!(
            !v.dispatchable_when(Allow {
                caches: true,
                ..Default::default()
            }),
            "lifting the cache mount let an ssh mount travel"
        );
    }

    #[test]
    fn the_driver_offers_what_the_gateway_was_allowed_to_offer() {
        // Two components deciding the same question by different rules.
        //
        // The gateway asks `dispatchable_when(Allow { caches: true })` when
        // REBUCK2_PEER_CACHE_MOUNTS=1 and offers the subtree. The driver then
        // asks `consider`, which took no Allow at all, saw a CacheMount
        // exclusion and refused - so the offer died on the arbiter with four
        // idle workers of exactly the right platform in front of it:
        //
        //   no peer can take it (wanted Pinned("linux/arm64"); had
        //     1:linux/arm64 0/16, 2:linux/arm64 0/16,
        //     3:linux/arm64 0/16, 4:linux/arm64 0/16)
        //
        // Measured on `earthly +code`: 1 of 6 solves routed, and the 5 that
        // did not were exactly the 5 carrying cache mounts.
        let mut e = exec_of(&plain());
        e.mounts = vec![mount(pb::MountType::Cache)];
        let mut op = plain();
        op.op = Some(OpKind::Exec(e));
        let v = inspect(&def(vec![op]));

        let idle = Candidate {
            id: 7,
            platform: "linux/arm64".to_owned(),
            load: load(16, 0, 0),
        };
        // Same graph, same worker. The ONLY difference is the policy, and it
        // must be the one thing that decides.
        assert!(
            offer_order(&v, std::slice::from_ref(&idle), Allow::default()).is_empty(),
            "a cache mount is excluded by default and must not be offered"
        );
        assert_eq!(
            offer_order(
                &v,
                std::slice::from_ref(&idle),
                Allow {
                    caches: true,
                    ..Default::default()
                }
            ),
            vec![7],
            "the gateway was allowed to offer this; the driver must agree"
        );
    }

    #[test]
    fn a_git_source_needs_the_session_a_worker_does_not_have() {
        // Found the same way as mount 101, one layer up: `inspect` had
        // opinions about MOUNTS and none at all about source schemes.
        //
        // A worker solves with no session, because a dispatched subtree is
        // supposed to need none. buildkit's git source resolves credentials
        // through the session's auth provider, so the solve dies with
        //
        //     build failed: solve: Unknown error no active sessions
        //
        // after the fleet has already accepted the lead - and on earthly's
        // fork the error path then nil-derefs and takes the daemon with it.
        // Measured on `earthly +test-no-qemu`: the graph carried exactly
        // one `source: git` and nothing else remarkable.
        //
        // Public or private makes no difference here. We cannot tell from
        // the identifier whether the fetch will reach for a credential, and
        // a subtree that MIGHT need a session is not dispatchable.
        let mut op = plain();
        op.op = Some(OpKind::Source(pb::SourceOp {
            identifier: "git://github.com/example/repo.git#main".to_owned(),
            ..Default::default()
        }));
        let v = inspect(&def(vec![op]));
        assert!(
            v.exclusions
                .iter()
                .any(|(_, x)| matches!(x, Exclusion::SessionSource(s) if s == "git")),
            "a git source sailed through: {:?}",
            v.exclusions
        );
        assert!(
            !v.dispatchable_when(Allow {
                caches: true,
                secrets: true,
                agent: true,
            }),
            "no flag may lift a source we cannot fetch without a session"
        );

        // A mirrored image is the case this must NOT catch: it is precisely
        // what `make_portable` produces, and excluding it would ground
        // every graph in the fleet.
        let mut ok = plain();
        ok.op = Some(OpKind::Source(pb::SourceOp {
            identifier: "docker-image://reg:5000/rebuck2/base:abc".to_owned(),
            ..Default::default()
        }));
        assert!(
            inspect(&def(vec![ok])).dispatchable(),
            "a mirrored image must stay dispatchable"
        );
    }

    #[test]
    fn a_fork_only_mount_type_is_a_blocker_not_a_blank() {
        // 101 is not a number anyone would guess. earthly's buildkit fork
        // declares `SOCKET = 101; // Earthly specific.` (solver/pb/ops.proto)
        // and its converter attaches TWO to every non-LOCALLY RUN.
        //
        // The old matcher tested for the five types buildkit declares and
        // said nothing about the rest, so a graph carrying only these looked
        // CLEAN: offered, led, and then dead on the worker with
        //
        //     build failed: solve: Unknown error no active sessions
        //
        // Nothing in that message names a mount, and inspect - the one
        // component whose entire job is to predict it - was the component
        // asserting there was nothing to name.
        //
        // The rule this encodes is not "exclude 101". It is that an unknown
        // mount type fails CLOSED, so the NEXT fork extension costs a
        // declined offer instead of a day.
        const EARTHLY_SOCKET: i32 = 101;
        let mut e = exec_of(&plain());
        e.mounts = vec![mount_of_type(EARTHLY_SOCKET), mount_of_type(EARTHLY_SOCKET)];
        let mut op = plain();
        op.op = Some(OpKind::Exec(e));
        let v = inspect(&def(vec![op]));

        assert!(
            v.exclusions
                .iter()
                .any(|(_, x)| *x == Exclusion::UnknownMount(EARTHLY_SOCKET)),
            "fork mount type sailed through: {:?}",
            v.exclusions
        );
        // Nothing lifts it. Every Allow flag names a hazard we understand
        // well enough to weigh; this one we do not understand at all.
        assert!(
            !v.dispatchable_when(Allow {
                caches: true,
                secrets: true,
                ..Default::default()
            }),
            "an opt-in flag cleared a mount type we cannot even name"
        );
    }

    #[test]
    fn every_blocker_is_reported_not_just_the_first() {
        // A report naming one blocker gets it fixed and then finds the next.
        let v = inspect(&def(vec![
            with_exec(plain(), |e| e.mounts = vec![mount(pb::MountType::Cache)]),
            plain(),
            with_exec(plain(), |e| e.security = pb::SecurityMode::Insecure as i32),
        ]));
        assert_eq!(
            v.exclusions,
            vec![(0, Exclusion::CacheMount), (2, Exclusion::Insecure)]
        );
    }

    #[test]
    fn a_clean_branch_does_not_escape_its_sibling() {
        // The claim "a partially-dispatchable tree is not dispatchable" was
        // only ever checked on a forest of unlinked ops. This builds the
        // shape the sentence is about: one base, two branches, a join.
        //
        //        3  join
        //       / \
        //  clean 1  2  hazard
        //       \ /
        //        0  alpine
        //
        // What it establishes is NOT that the hazard propagates along the
        // edge - `inspect` never reads `op.inputs`, so no traversal happens
        // and the linked case cannot differ from the unlinked one. It
        // establishes the thing worth pinning: branch 1 is dispatchable in
        // isolation and still travels nowhere, because the unit that gets
        // placed is the whole `Definition` and shape is not consulted.
        //
        // Written to go red on purpose. A pass that carves at the seam has
        // to make edges matter, and this is the test that will notice.
        let tree = |hazardous: bool| {
            let branch = if hazardous {
                with_exec(plain(), |e| e.mounts = vec![mount(pb::MountType::Secret)])
            } else {
                plain()
            };
            chain(vec![
                (src("docker-image://docker.io/library/alpine:3.20"), vec![]),
                (plain(), vec![0]),
                (branch, vec![0]),
                (plain(), vec![1, 2]),
            ])
        };

        let v = inspect(&tree(true));
        assert_eq!(v.exclusions, vec![(2, Exclusion::Secret)]);
        assert!(!v.dispatchable(), "one hazardous branch grounds the join");

        // The control. Same four ops, same edges, hazard removed - so the
        // discriminating variable is the secret and not the shape.
        assert!(
            inspect(&tree(false)).dispatchable(),
            "the shape alone must not ground it, or the test above proves nothing"
        );
    }

    #[test]
    fn platform_is_the_union_and_a_split_one_cannot_travel() {
        let plat = |os: &str, arch: &str| pb::Platform {
            os: os.into(),
            architecture: arch.into(),
            ..Default::default()
        };
        let on = |p: pb::Platform| {
            let mut o = plain();
            o.platform = Some(p);
            o
        };

        // One declaring vertex pins the whole tree.
        let v = inspect(&def(vec![plain(), on(plat("linux", "arm64")), plain()]));
        assert_eq!(v.platform, Platform::Pinned("linux/arm64".into()));
        assert!(v.dispatchable());

        // Agreement is not a conflict.
        let v = inspect(&def(vec![
            on(plat("linux", "arm64")),
            on(plat("linux", "arm64")),
        ]));
        assert_eq!(v.platform, Platform::Pinned("linux/arm64".into()));
        assert!(v.dispatchable());

        // Two platforms in one subtree: no single peer can build it as a
        // unit, so it does not travel. Conservative, per principle 10.
        let v = inspect(&def(vec![
            on(plat("linux", "arm64")),
            on(plat("linux", "amd64")),
        ]));
        assert_eq!(
            v.platform,
            Platform::Conflict(
                ["linux/amd64".to_owned(), "linux/arm64".to_owned()]
                    .into_iter()
                    .collect()
            )
        );
        assert!(!v.dispatchable(), "a split-platform subtree cannot travel");

        // Worker constraints AND together, so their union is exactly right
        // and can never conflict.
        let mut a = plain();
        a.constraints = Some(pb::WorkerConstraints {
            filter: vec!["label.gpu==true".into()],
        });
        let mut b = plain();
        b.constraints = Some(pb::WorkerConstraints {
            filter: vec!["label.fast==true".into(), "label.gpu==true".into()],
        });
        let v = inspect(&def(vec![a, b]));
        assert_eq!(
            v.constraints,
            ["label.fast==true".to_owned(), "label.gpu==true".to_owned()]
                .into_iter()
                .collect()
        );
        assert!(
            v.dispatchable(),
            "constraints narrow the fleet, not the tree"
        );
    }

    #[test]
    fn an_op_we_cannot_read_is_not_assumed_safe() {
        // The definition is bytes off a wire. We cannot show an unreadable
        // op is free of secrets or cache mounts, so it does not travel -
        // duplicate work is always correct, a mis-shipped subtree is not.
        let mut d = def(vec![plain()]);
        d.def.push(b"not a protobuf at all".to_vec());
        let v = inspect(&d);
        assert_eq!(v.exclusions, vec![(1, Exclusion::Undecodable)]);
        assert!(!v.dispatchable());

        // A non-exec op (a source, a file op) is ordinary: it carries no
        // mounts and no secrets, and grounding those would ground the
        // FROM-rooted chains that principle 11 calls the best handover.
        let src = pb::Op {
            op: Some(OpKind::Source(pb::SourceOp {
                identifier: "docker-image://alpine:3.20".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(inspect(&def(vec![src])).dispatchable());
    }
}
