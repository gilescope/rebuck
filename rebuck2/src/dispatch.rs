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
