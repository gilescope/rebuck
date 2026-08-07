//! Whether a subtree may travel — the buildkit-facing half of dispatch.
//!
//! Principle 10 says the unit of work one machine asks another for is a
//! SUBTREE, never a vertex, and it comes with three consequences that are
//! all conservative:
//!
//! - **Exclusions propagate upward.** One cache mount, one secret, one
//!   privileged exec anywhere in the subtree excludes the WHOLE subtree. A
//!   partially-dispatchable tree is not dispatchable.
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

use std::collections::BTreeSet;

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
    pub fn dispatchable(&self) -> bool {
        self.exclusions.is_empty() && !matches!(self.platform, Platform::Conflict(_))
    }
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
fn hazard(op: &pb::Op) -> Option<Exclusion> {
    let pb::op::Op::Exec(e) = op.op.as_ref()? else {
        return None;
    };
    if e.security == pb::SecurityMode::Insecure as i32 {
        return Some(Exclusion::Insecure);
    }
    if e.network == pb::NetMode::Host as i32 {
        return Some(Exclusion::HostNetwork);
    }
    if !e.secretenv.is_empty() {
        return Some(Exclusion::Secret);
    }
    e.mounts.iter().find_map(|m| {
        // Read BOTH the type and the option: a cache mount is identified by
        // either, and trusting one alone leaves the other as a way through.
        if m.mount_type == pb::MountType::Cache as i32 || m.cache_opt.is_some() {
            Some(Exclusion::CacheMount)
        } else if m.mount_type == pb::MountType::Secret as i32 || m.secret_opt.is_some() {
            Some(Exclusion::Secret)
        } else if m.mount_type == pb::MountType::Ssh as i32 || m.ssh_opt.is_some() {
            Some(Exclusion::SshAgent)
        } else {
            None
        }
    })
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
        if let Some(why) = hazard(&op) {
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
pub enum Next {
    /// A peer is blocked on this. Always first.
    Peer(u64),
    Driver(u64),
    Idle,
}

/// Should this worker accept an offered subtree?
pub fn consider(load: Load, v: &Verdict, my_platform: &str) -> Result<(), Refusal> {
    // Checked in this order on purpose. "This should never have been
    // offered" and "I can never run this" must outrank "not right now":
    // Saturated invites the offer back, and an offer that can never be
    // accepted would then circulate forever.
    if let Some((_, why)) = v.exclusions.first() {
        return Err(Refusal::Undispatchable(why.clone()));
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
pub fn offer_order(v: &Verdict, cands: &[Candidate]) -> Vec<u64> {
    let mut able: Vec<&Candidate> = cands
        .iter()
        .filter(|c| {
            // Ask the same question the peer will. A candidate that would
            // decline is a wasted round trip, and a saturated one says so
            // in its own load without being asked.
            !matches!(
                consider(c.load, v, &c.platform),
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
        assert_eq!(consider(load(4, 1, 1), &v, "linux/arm64"), Ok(()));

        // Saturated is the signal principle 12 is built on: a driver that
        // cannot place work has learned the fleet is full without a metric.
        assert_eq!(
            consider(load(2, 1, 1), &v, "linux/arm64"),
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
            consider(load(4, 0, 0), &pinned, "linux/amd64"),
            Err(Refusal::WrongPlatform {
                wants: "linux/arm64".into(),
                have: "linux/amd64".into()
            })
        );
        assert_eq!(consider(load(4, 0, 0), &pinned, "linux/arm64"), Ok(()));
        // An unpinned subtree runs anywhere.
        assert_eq!(consider(load(4, 0, 0), &v, "windows/amd64"), Ok(()));

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
            consider(load(4, 0, 0), &bad, "linux/arm64"),
            Err(Refusal::Undispatchable(Exclusion::Secret))
        );

        // Refusing an undispatchable subtree outranks being saturated: the
        // offer was wrong, and saying "try me later" invites it back.
        assert_eq!(
            consider(load(1, 1, 0), &bad, "linux/arm64"),
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
        );
        assert_eq!(got, vec![3], "the only peer that can run it, busy or not");

        // Nobody able => build it yourself. Duplicate work is always
        // correct; a stall is worse than the work we set out to avoid.
        assert_eq!(
            offer_order(&pinned, &[cand(1, "linux/amd64", load(8, 0, 0))]),
            Vec::<u64>::new()
        );
        assert_eq!(offer_order(&v, &[]), Vec::<u64>::new());

        // An undispatchable subtree is offered to NOBODY, however idle the
        // fleet is - the exclusion is about the work, not the capacity.
        let bad = inspect(&def(vec![with_exec(plain(), |e| {
            e.mounts = vec![mount(pb::MountType::Cache)]
        })]));
        assert_eq!(
            offer_order(&bad, &[cand(1, "linux/arm64", load(8, 0, 0))]),
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
        );
        assert_eq!(got, vec![2, 9]);
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

    #[test]
    fn one_hazard_anywhere_excludes_the_whole_subtree() {
        // Principle 10: exclusions propagate UPWARD. The hazard is on the
        // middle op of three, and the verdict is about the tree, not the op.
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
