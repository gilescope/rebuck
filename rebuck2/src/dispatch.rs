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

impl Platform {
    /// The one architecture this graph demands, if it demands one.
    ///
    /// Known from the GRAPH, before anyone decides where it goes, which is
    /// what makes it usable for mirroring: `mirror_image` copies a base for
    /// a named platform and the dispatch site was passing `None`, so every
    /// base was mirrored for the coordinator's own architecture.
    ///
    /// That has never mattered because every worker was amd64 and a pinned
    /// arm64 solve had nowhere to go - `not routed` on `+all-buildkitd` says
    /// so in as many words. With one arm64 worker in the fleet it starts
    /// mattering immediately, and it would surface as a worker pulling a
    /// base for the wrong architecture.
    ///
    /// `Conflict` names none: guessing one would mirror a base for half of a
    /// subtree that nowhere can build anyway.
    pub fn pinned(&self) -> Option<&str> {
        match self {
            Platform::Pinned(p) => Some(p),
            Platform::Any | Platform::Conflict(_) => None,
        }
    }
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

    /// As [`Verdict::dispatchable_when`], but ignoring hazards the driver is
    /// about to mirror away.
    ///
    /// For the decision of whether a graph is WORTH making portable. The
    /// graph must be inspected again afterwards and pass the strict test:
    /// mirroring is best-effort, and a source we failed to publish leaves
    /// the subtree exactly as grounded as it was.
    pub fn dispatchable_once_mirrored(&self, allow: Allow) -> bool {
        let blocked = self
            .exclusions
            .iter()
            .any(|(_, e)| !lifted_by(e, allow) && !fixable_by_mirroring(e));
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

/// Can the DRIVER fix this hazard by republishing content, rather than the
/// operator lifting it by policy?
///
/// A git source is not a property of the build the way a cache mount is: the
/// driver holds a session, so it can fetch the tree and publish it as an
/// image, and the hazard simply stops existing. Judging it on the raw graph
/// refuses the subtree before the fix has a chance to run.
///
/// This distinction did not exist while every hazard lived on an ExecOp -
/// rewriting only ever touched source identifiers, so the pre-rewrite
/// verdict was the post-rewrite verdict. Adding source hazards broke that,
/// silently: git mirroring was written, committed, and fired zero times.
pub fn fixable_by_mirroring(e: &Exclusion) -> bool {
    matches!(e, Exclusion::SessionSource(_))
}

/// Does `allow` lift this hazard?
///
/// One table, because `dispatchable_when` and `consider` used to answer
/// it separately and a subtree the first would offer was refused by the
/// second.
pub fn lifted_by_policy(e: &Exclusion, allow: Allow) -> bool {
    lifted_by(e, allow)
}

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

/// Which cache mounts a graph names, by the id buildkit keys them on.
///
/// A cache mount is scratch, so a peer builds correctly with a COLD one -
/// correctly and slowly. On earthbuild's Earthfile four ids do nearly all
/// the work (`go-mod`, `go-build`, `npm`, `littleredcorvette-id`), and a
/// worker that has never seen them re-downloads the Go module graph before
/// it can start.
///
/// Naming them is the first step to deciding which are worth seeding: the
/// answer is not "all of them", and it cannot be guessed from the Earthfile
/// because frequency in the source says nothing about time spent.
/// Observed cache-mount inputs, as a file the next run can read.
///
/// `id \t selector \t base64(op bytes)` per line. Base64 because op bytes
/// are arbitrary protobuf - NUL, tab and newline all occur in them - and a
/// raw write would corrupt on the first op containing a tab, leaving the
/// reader silently one field short.
///
/// Written by the proxy at the end of a run and read by `harvest-cache`,
/// usually a run later, carried between them by the bank. Two processes and
/// two runs apart is exactly the kind of seam that has cost this project
/// thirteen faults, so both directions live here and are tested together.
pub fn encode_cache_inputs(m: &BTreeMap<String, (Vec<u8>, String)>) -> String {
    use base64::Engine;
    m.iter()
        .map(|(id, (op, sel))| {
            format!(
                "{id}\t{sel}\t{}",
                base64::engine::general_purpose::STANDARD.encode(op)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The reverse of [`encode_cache_inputs`]. A malformed line is dropped.
///
/// Dropped rather than repaired: the file rides between runs in a cache and
/// can arrive truncated, and a half-read op would produce a seed pointing at
/// the wrong directory - which reads as an empty cache and not as an error.
pub fn decode_cache_inputs(text: &str) -> BTreeMap<String, (Vec<u8>, String)> {
    use base64::Engine;
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let id = f.next()?;
            let sel = f.next()?;
            let op = base64::engine::general_purpose::STANDARD
                .decode(f.next()?)
                .ok()?;
            (!id.is_empty()).then(|| (id.to_owned(), (op, sel.to_owned())))
        })
        .collect()
}

/// `~/x` as a real path.
///
/// A workflow's `env:` value has `${{ }}` expressions evaluated and nothing
/// else, so `~/.cache/...` arrives at the process as a literal tilde and
/// every write lands in a directory called `~` or fails. The shell would
/// have expanded it; a process handed that environment will not.
///
/// `~user` is left alone - that is a shell convention for somebody else's
/// home and not ours to guess at.
pub fn expand_home(path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => std::env::var("HOME").unwrap_or_else(|_| path.to_owned()),
        Some(rest) if rest.starts_with('/') => match std::env::var("HOME") {
            Ok(h) => format!("{h}{rest}"),
            Err(_) => path.to_owned(),
        },
        _ => path.to_owned(),
    }
}

/// The exact input op behind each cache mount, keyed by cache id.
///
/// Returns `(op bytes, selector)`. The bytes are lifted from the graph
/// verbatim, and that is the whole point: `getRefCacheDir` keys a cache
/// directory on `id` plus the input's `ref.ID()`, so reading the directory
/// earthly writes means presenting an input that hashes identically.
///
/// Reconstruction was tried and failed silently. Building
/// `Scratch().File(Mkdir("/cache", 0644))` by hand from a reading of
/// `runmount.go` produced an op that did not match - a platform on the op,
/// a constraint, or a differing convention for a FileAction's unused fields
/// is enough - and the harvest read an empty directory it created itself.
/// No error, no warning, just 0.0 MiB and a run's delay.
///
/// A mount with no input contributes nothing: there is no ref in its key, so
/// there is nothing to reproduce.
pub fn cache_mount_inputs(def: &pb::Definition) -> BTreeMap<String, (Vec<u8>, String)> {
    let by_digest: BTreeMap<String, &Vec<u8>> = def
        .def
        .iter()
        .map(|b| (format!("sha256:{}", crate::store::sha256_hex(b)), b))
        .collect();
    let mut out = BTreeMap::new();
    for bytes in &def.def {
        let Ok(op) = pb::Op::decode(bytes.as_slice()) else {
            continue;
        };
        let Some(pb::op::Op::Exec(e)) = &op.op else {
            continue;
        };
        for m in &e.mounts {
            let (Some(c), true) = (&m.cache_opt, m.input >= 0) else {
                continue;
            };
            let Some(input) = op.inputs.get(m.input as usize) else {
                continue;
            };
            if let Some(src) = by_digest.get(&input.digest) {
                out.entry(c.id.clone())
                    .or_insert_with(|| ((*src).clone(), m.selector.clone()));
            }
        }
    }
    out
}

/// Cache mounts as the CLIENT wrote them: id, and whether it has an input.
///
/// The input decides which directory the mount is, not just what it starts
/// from. `getRefCacheDir` builds its key as
///
/// ```text
///     key := id
///     if ref != nil { key += ":" + ref.ID() }
/// ```
///
/// so a mount with an input is a DIFFERENT cache dir from the same id
/// without one. Harvesting id `go-mod` with no input therefore reads a
/// directory earthly may never have written to - which is the leading
/// explanation for four harvests coming back at 0.0 MiB from a daemon that
/// had just run a build using those exact ids.
///
/// Read off the graphs the client actually sent, because guessing what
/// earthly emits is how the last three of these went.
pub fn cache_mount_shapes(def: &pb::Definition) -> BTreeSet<(String, bool)> {
    let mut out = BTreeSet::new();
    for bytes in &def.def {
        let Ok(op) = pb::Op::decode(bytes.as_slice()) else {
            continue;
        };
        let Some(pb::op::Op::Exec(e)) = op.op else {
            continue;
        };
        for m in &e.mounts {
            if let Some(c) = &m.cache_opt {
                out.insert((c.id.clone(), m.input >= 0));
            }
        }
    }
    out
}

pub fn cache_ids(def: &pb::Definition) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for bytes in &def.def {
        let Ok(op) = pb::Op::decode(bytes.as_slice()) else {
            continue;
        };
        let Some(pb::op::Op::Exec(e)) = op.op else {
            continue;
        };
        for m in &e.mounts {
            if let Some(c) = &m.cache_opt {
                out.insert(c.id.clone());
            }
        }
    }
    out
}

/// May a peer's failure be reported to the client as the client's own?
///
/// The second half of [`is_build_verdict`], and the expensive half. Stopping
/// the retry took `+lint-all` from 628s to 252s; the ~160s still left is
/// four home rebuilds of a target that already failed on a peer.
///
/// Three conditions, and each removes a way of being wrong.
///
/// * `enabled` - off by default. A needless rebuild costs seconds; a false
///   red costs trust in the whole rig, and those are not the same size.
/// * `verbatim` - the dispatched definition was byte-identical to the
///   client's. Making a graph portable rewrites local contexts into images
///   and repoints base images at our mirror, and this project has produced
///   failures from exactly that (`no active sessions`,
///   `security.insecure is not allowed`). A verdict on a graph we altered
///   is a verdict about OUR graph.
/// * `lifted_cache` - whether a cache mount travelled. If one did, the peer
///   ran the client's exact graph against a COLD cache, and a cold `go-mod`
///   fetches over the network: a blip there is `exit code: 1` from `go mod
///   download` on a build that passes everywhere else. Principle 20 from the
///   other side - seeding a self-keying cache is safe because a wrong entry
///   is never found, and trusting a verdict produced against a cold one is
///   a different claim entirely.
///
/// Platform needs no condition: `consider` already refuses a candidate whose
/// platform does not match the graph, so a peer that took the work matched
/// it.
/// How many configured seeds actually resolved.
///
/// A process-wide number because the report is assembled in a signal
/// handler that holds no proxy. Three seeding runs have now produced no
/// seeding for three different mechanical reasons - a binary path, a bare
/// digest, and a cache id nobody had read off a real run - and each time
/// "did it even run" cost a log dig. `seeds=0/3` in the verdict line answers
/// it without one.
static SEEDS_RESOLVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn note_seeds_resolved(n: usize) {
    SEEDS_RESOLVED.store(n, std::sync::atomic::Ordering::Relaxed);
}

pub fn seeds_resolved() -> usize {
    SEEDS_RESOLVED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn trust_peer_verdicts() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var("REBUCK2_TRUST_VERDICT").as_deref() == Ok("1"))
}

/// See [`trust_peer_verdicts`] for the switch; this is the rule.
pub fn trust_verdict(enabled: bool, verbatim: bool, lifted_cache: bool) -> bool {
    enabled && verbatim && !lifted_cache
}

/// Did the BUILD fail, as opposed to the peer failing to build it?
///
/// The distinction the fleet was missing. `subtree_declined` re-offers to
/// the next peer whatever the reason, which is right for "no slots", "wrong
/// platform" or a daemon that died, and catastrophic for a build that simply
/// fails: every machine runs it, every machine fails, and the client waits
/// for all of them.
///
/// Measured on `+lint-all`: 92s on one machine, 628s on six, with four
/// solves of ~470s apiece and a `not routed` table naming the same
/// `golangci-lint ... exit code: 1` four times. The single-machine build
/// pays a deterministic failure once; the fleet paid it per peer.
///
/// Matched on buildkit's own framing - the container RAN and the process
/// exited non-zero - and nothing else. Every machine-shaped failure stays
/// retryable, because the two mistakes are not the same size: retrying a
/// verdict costs time, and refusing to retry a machine fault costs the
/// build.
pub fn is_build_verdict(why: &str) -> bool {
    let Some(tail) = why
        .rsplit("did not complete successfully: exit code:")
        .next()
    else {
        return false;
    };
    if tail.len() == why.len() {
        return false;
    }
    let Some(code) = tail
        .split_whitespace()
        .next()
        .and_then(|c| c.trim_end_matches(['"', ',', '.']).parse::<i32>().ok())
    else {
        return false;
    };
    // A SIGNAL is not a verdict. 128+N means something killed the process,
    // and on a build runner that something is nearly always the OOM killer:
    // 137 is SIGKILL, 143 SIGTERM. A machine with more memory may well
    // succeed, so those stay retryable - which is the whole point of the
    // predicate, and it shipped inverted for an hour with 137 asserted as a
    // verdict. That would have turned "this worker ran out of memory" into
    // "your build fails".
    //
    // 128 exactly is a real exit status, not a signal death.
    !(129..=192).contains(&code)
}

/// `id:path,id:path` as the workflow writes it.
///
/// In Rust because the shell version could not be tested and was wrong. It
/// split on the FIRST colon and rejected `id == path` as malformed, which
/// throws away every mount written without an `id=` - buildkit keys those on
/// the destination, so the id is the path and the pair reads
/// `/go/pkg/mod:/go/pkg/mod`. Two entries of exactly that shape had been
/// added, deliberately, two commits before the guard ate them.
///
/// Split on the LAST colon, and check what actually separates a good pair
/// from a bad one: there is a colon, the path is absolute, the id is not
/// empty. Anything else is dropped with a line rather than guessed at.
pub fn parse_seed_pairs(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((id, path)) = pair.rsplit_once(':') else {
            println!("[harvest] no colon in {pair:?}, wanted id:path");
            continue;
        };
        if id.is_empty() {
            println!("[harvest] empty id in {pair:?}");
            continue;
        }
        if !path.starts_with('/') {
            println!("[harvest] {path:?} is not an absolute mount path");
            continue;
        }
        out.push((id.to_owned(), path.to_owned()));
    }
    out
}

/// An image name as an LLB source identifier buildkit will accept.
///
/// `llb.Image("busybox:1")` normalises before building the op. Hand-built
/// LLB does not, and buildkit's `NewImageIdentifier` runs containerd's
/// `reference.Parse` and then insists the result has an `Object` - a tag or
/// a digest. Containerd's parser wants a registry host to find one, so
/// `busybox:1` yields nothing and the solve dies as
///
///     failed to load cache key: object required
///
/// which names neither the image nor the field it is complaining about.
///
/// Every other identifier this crate writes is fully qualified already -
/// mirrored bases are `host/repo@sha256:...` - so the harvest was the first
/// caller to find this, and it found it in the five-minute smoke test
/// rather than in a twenty-five minute fleet run.
pub fn image_identifier(name: &str) -> String {
    if name.contains("://") {
        return name.to_owned();
    }
    // A host has a dot, a colon, or is `localhost`. Anything else is a
    // Docker Hub short name and needs the library path spelled out, exactly
    // as `reference.ParseNormalizedNamed` would.
    let first = name.split('/').next().unwrap_or(name);
    let has_host =
        name.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    let full = if has_host {
        name.to_owned()
    } else if name.contains('/') {
        format!("docker.io/{name}")
    } else {
        format!("docker.io/library/{name}")
    };
    format!("docker-image://{full}")
}

/// The id a daemon actually holds, given the id an operator asked for.
///
/// A mount written without `id=` in the Earthfile is NOT keyed on its
/// destination. Earthly computes `/run/cache/<per-target hash>/<target>`,
/// so a seed list naming `/root/.cache/golangci_lint` names an id that
/// exists nowhere - and the harvest then reads an empty directory it
/// created itself, which looks exactly like a cold cache.
///
/// Exact match first, then a unique tail match. Ambiguity is refused rather
/// than resolved: two targets can share a mount path, and harvesting one
/// target's cache to seed every other is the one thing principle 20 forbids
/// outright.
///
/// Named by the operator either way - this widens how a name is matched, not
/// what may be seeded.
pub fn resolve_cache_id(want: &str, held: &[String]) -> Option<String> {
    if held.iter().any(|h| h == want) {
        return Some(want.to_owned());
    }
    let tail = if want.starts_with('/') {
        want.to_owned()
    } else {
        format!("/{want}")
    };
    let mut hits = held.iter().filter(|h| h.ends_with(&tail));
    let first = hits.next()?;
    if hits.next().is_some() {
        println!(
            "[dispatch] cache id {want:?} matches more than one id on this daemon - \
             refusing to guess which"
        );
        return None;
    }
    println!("[dispatch] cache id {want:?} is {first:?} on this daemon");
    Some(first.clone())
}

/// Earthly's cache-mount input: scratch with `/cache` created.
///
/// One function because two copies of it would be two chances to differ,
/// and a difference here is invisible - it does not fail, it silently reads
/// a different cache directory. From `earthfile2llb/runmount.go`:
///
/// ```go
///     state = c.cacheContext                    // pllb.Scratch()
///     state = state.File(pllb.Mkdir("/cache", mountMode))
///     mountOpts = append(mountOpts, llb.SourcePath("/cache"))
/// ```
///
/// The bytes matter, not the intent, and the reason is sharper than it
/// first looks. A cache ref's `ID()` is `identity.NewID()` - RANDOM, chosen
/// when the record is created, not derived from content. So `getRefCacheDir`
/// keying on `ref.ID()` means an op that differs by one field does not get a
/// nearly-right key, it gets a brand new record with a brand new random id
/// and therefore a fresh empty directory.
///
/// Identical LLB reaches the same ref only because buildkit DEDUPS within
/// one daemon: the same digest finds the existing record. Across daemons the
/// ids are unrelated, which is fine here - each worker builds its own
/// directory from the seed image - but it does mean "the same bytes" is a
/// requirement and not an optimisation.
pub fn earthly_cache_context() -> Vec<u8> {
    pb::Op {
        op: Some(pb::op::Op::File(pb::FileOp {
            actions: vec![pb::FileAction {
                // -1: builds on nothing, which is Scratch.
                input: -1,
                secondary_input: -1,
                output: 0,
                action: Some(pb::file_action::Action::Mkdir(pb::FileActionMkDir {
                    path: "/cache".into(),
                    mode: 0o644,
                    make_parents: false,
                    ..Default::default()
                })),
            }],
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

/// A one-command graph with a writable cache mount, for checking seeding.
///
/// Both halves of a round trip are this shape. Write a marker into cache A,
/// harvest A, then read the marker back out of cache **B** seeded from that
/// harvest - and B has to be a different id, or the read passes by meeting
/// A's own warm mount and proves nothing at all.
///
/// The command decides success: a non-zero exit fails the solve, so
/// `test -f /c/marker` IS the assertion and no output has to be inspected.
///
/// No scratch output, unlike [`harvest_graph`]: that one exports a layer,
/// this one only has to succeed or fail, and an output mount nothing writes
/// to would export an empty layer on every check.
pub fn cache_probe_graph(base: &str, cache_id: &str, dest: &str, cmd: &str) -> pb::Definition {
    // The probe writes the cache the way EARTHLY writes it, which is the
    // difference between a rig that tests the mechanism and one that
    // confirms its own assumptions.
    //
    // It did the latter for eight CI attempts: the probe mounted the cache
    // with no input, the harvest read it with no input, the round trip
    // passed, and neither half resembled the graphs earthly actually sends.
    // The first thing the corrected harvest did was fail against this probe,
    // which is the rig finally disagreeing with itself.
    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: image_identifier(base),
            ..Default::default()
        })),
        ..Default::default()
    };
    let src_bytes = src.encode_to_vec();
    let src_digest = format!("sha256:{}", crate::store::sha256_hex(&src_bytes));

    // Earthly's cache-mount input, reconstructed. See `harvest_graph` for
    // why the input decides WHICH directory the mount is.
    let mkdir_bytes = earthly_cache_context();
    let mkdir_digest = format!("sha256:{}", crate::store::sha256_hex(&mkdir_bytes));

    let exec = pb::Op {
        inputs: vec![
            pb::Input {
                digest: src_digest,
                index: 0,
            },
            pb::Input {
                digest: mkdir_digest,
                index: 0,
            },
        ],
        op: Some(pb::op::Op::Exec(pb::ExecOp {
            meta: Some(pb::Meta {
                args: vec!["/bin/sh".into(), "-c".into(), cmd.to_owned()],
                // PATH, because hand-built LLB has no image config behind
                // it. A frontend merges the image's own Env into Meta;
                // constructing the op directly does not, so `cp` and `test`
                // are not on any path and `/bin/sh -c` reports
                // `did not complete successfully: exit code: 1` naming the
                // whole command and none of the reason.
                //
                // The same shape as the image-identifier fault: `llb.Image`
                // and `llb.Exec` do a normalisation step for you, and a
                // hand-written op inherits none of it.
                env: vec![
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
                ],
                cwd: "/".into(),
                ..Default::default()
            }),
            mounts: vec![
                // OUTPUT 0, and it must exist. The terminal names `index: 0`
                // of this exec, so an exec declaring no output 0 is a graph
                // that cannot export - the first version had none, with a
                // comment saying a probe exports nothing. It does not need
                // the layer; it needs the index to be there.
                pb::Mount {
                    input: 0,
                    dest: "/".into(),
                    output: 0,
                    ..Default::default()
                },
                pb::Mount {
                    input: 1,
                    selector: "/cache".into(),
                    dest: dest.to_owned(),
                    // Never the cache: buildkit refuses to export one, which
                    // is why `harvest_graph` copies out of it instead.
                    output: -1,
                    mount_type: pb::MountType::Cache as i32,
                    cache_opt: Some(pb::CacheOpt {
                        id: cache_id.to_owned(),
                        sharing: pb::CacheSharingOpt::Shared as i32,
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        })),
        ..Default::default()
    };
    let exec_bytes = exec.encode_to_vec();
    let exec_digest = format!("sha256:{}", crate::store::sha256_hex(&exec_bytes));

    pb::Definition {
        def: vec![
            src_bytes,
            mkdir_bytes,
            exec_bytes,
            pb::Op {
                inputs: vec![pb::Input {
                    digest: exec_digest,
                    index: 0,
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ],
        ..Default::default()
    }
}

/// Take a warm cache mount OUT of a daemon, as a layer.
///
/// The other half of [`seed_cache_mounts`]. Seeding needs an image whose
/// root IS the cache contents, and nothing in the Earthfile produces one -
/// a cache mount is deliberately not exportable, which is why buildkit
/// refuses `output` on one. So: mount the cache read-only beside an empty
/// scratch mount, copy across, and let the scratch be the result.
///
/// Both halves are buildkit's own behaviour, in
/// `frontend/gateway/container/container.go`:
///
/// * `MountType_BIND` with `Input == Empty` and an output becomes
///   `makeMutable(m, nil)` - a fresh writable dir exported as the result.
/// * `MountType_CACHE` becomes `MountableCache(ctx, m, ref, g)`, whose
///   `ref` is the mount's input - the seeding path this feeds.
///
/// `base` only needs a shell and `cp`. It is a parameter rather than a
/// constant because the harvest must run on a machine that can already pull
/// it, and the fleet's mirror is the only registry every peer is known to
/// reach.
pub fn harvest_graph(base: &str, cache_id: &str, dest: &str) -> pb::Definition {
    harvest_graph_with(base, cache_id, dest, None)
}

/// [`harvest_graph`], given the cache mount's input taken from a real graph.
///
/// `input` is `(op bytes, selector)` from [`cache_mount_inputs`]. Passing
/// `None` reconstructs earthly's shape, which is a guess that has been
/// measured wrong: it produces a different digest, a different ref, and a
/// different - empty - cache directory, with no error anywhere.
pub fn harvest_graph_with(
    base: &str,
    cache_id: &str,
    dest: &str,
    input: Option<(Vec<u8>, String)>,
) -> pb::Definition {
    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: image_identifier(base),
            ..Default::default()
        })),
        ..Default::default()
    };
    let src_bytes = src.encode_to_vec();
    let src_digest = format!("sha256:{}", crate::store::sha256_hex(&src_bytes));

    // THE MOUNT'S INPUT, and it is the whole reason four harvests read
    // nothing. From earthfile2llb/runmount.go:
    //
    //     state = c.cacheContext                    // pllb.Scratch()
    //     state = state.File(pllb.Mkdir("/cache", mountMode))
    //     mountOpts = append(mountOpts, llb.SourcePath("/cache"))
    //     return []llb.RunOption{pllb.AddMount(mountTarget, state, ...)}
    //
    // and from buildkit's getRefCacheDir:
    //
    //     key := id
    //     if ref != nil { key += ":" + ref.ID() }
    //
    // So earthly's `go-mod` sits at `go-mod:<ref of that scratch+mkdir>`,
    // and a harvest mounting `go-mod` with no input reads plain `go-mod` -
    // a directory it creates itself and nothing has ever written to. The
    // daemon held 1.71 GB under these exact ids while the harvest reported
    // 0.0 MiB.
    //
    // A FALLBACK ONLY, and a measured-wrong one: see `cache_mount_inputs`.
    // The ref id is random per record, so an op that differs by one field
    // gets a fresh record and a fresh empty directory rather than anything
    // approximately right. 0o644 is earthly's default mode for a cache
    // mount, from the same function - and matching the mode was not
    // sufficient.
    // Taken from a real graph when we have one, reconstructed only as a
    // fallback - see `cache_mount_inputs` for why the fallback is a guess.
    let (mkdir_bytes, selector) =
        input.unwrap_or_else(|| (earthly_cache_context(), "/cache".to_owned()));
    let mkdir_digest = format!("sha256:{}", crate::store::sha256_hex(&mkdir_bytes));

    // `/.seed` and not `/out`: the harvest runs against whatever base the
    // operator named, and a path that already exists would be shadowed by
    // the mount and copied into itself.
    const OUT: &str = "/.seed";
    let exec = pb::Op {
        inputs: vec![
            pb::Input {
                digest: src_digest.clone(),
                index: 0,
            },
            // Input 1: the mkdir, which the cache mount points at.
            pb::Input {
                digest: mkdir_digest,
                index: 0,
            },
        ],
        op: Some(pb::op::Op::Exec(pb::ExecOp {
            meta: Some(pb::Meta {
                // `.` after the slash so dotfiles come too, and `|| true` so
                // an EMPTY cache harvests an empty layer instead of failing
                // the build. A missing seed is a cold worker, which is
                // today's behaviour; a failed solve is a broken one.
                //
                // stderr is NOT swallowed. It was, and the first real
                // failure then read `exit code: 1` with the whole command
                // quoted and not one word of why - which is a minute of
                // guessing per attempt, and there have been eleven.
                args: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("cp -a {dest}/. {OUT}/ || true"),
                ],
                // PATH, because hand-built LLB has no image config behind
                // it. A frontend merges the image's own Env into Meta;
                // constructing the op directly does not, so `cp` and `test`
                // are not on any path and `/bin/sh -c` reports
                // `did not complete successfully: exit code: 1` naming the
                // whole command and none of the reason.
                //
                // The same shape as the image-identifier fault: `llb.Image`
                // and `llb.Exec` do a normalisation step for you, and a
                // hand-written op inherits none of it.
                env: vec![
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
                ],
                cwd: "/".into(),
                ..Default::default()
            }),
            mounts: vec![
                // OUTPUT 1, and it has to be something. From buildkit's
                // `PrepareMounts`: a BIND mount with an output becomes
                // `makeMutable`, while the root-mount branch below it only
                // makes one when `m.Readonly` is set - so `readonly: false`
                // with no output leaves the root as the IMMUTABLE ref,
                // which is the opposite of what the comment above that
                // branch says a root needs. The exec then fails as `exit
                // code: 1` without running a line, and three attempts went
                // on a `cp` that turned out to be innocent - proven by
                // running the same `cp` in the same image by hand, where it
                // exits 0 even with no PATH at all.
                //
                // Index 1 so index 0 stays the seed: the terminal selects 0.
                pb::Mount {
                    input: 0,
                    dest: "/".into(),
                    output: -1,
                    // READONLY, which is what gets it a mutable ref.
                    // `PrepareMounts`: a root with no output is made mutable
                    // only when `Readonly` is set, and left immutable
                    // otherwise - so this flag is the difference between a
                    // container that starts and `exit code: 1` before a
                    // line runs. Backwards-looking, and it is buildkit's
                    // spelling, not ours.
                    readonly: true,
                    ..Default::default()
                },
                pb::Mount {
                    // Input 1: the mkdir built above, which is what makes
                    // this key match earthly's. See that comment for why.
                    input: 1,
                    selector: selector.clone(),
                    dest: dest.to_owned(),
                    output: -1,
                    readonly: true,
                    mount_type: pb::MountType::Cache as i32,
                    cache_opt: Some(pb::CacheOpt {
                        id: cache_id.to_owned(),
                        sharing: pb::CacheSharingOpt::Shared as i32,
                    }),
                    ..Default::default()
                },
                pb::Mount {
                    input: -1,
                    dest: OUT.into(),
                    output: 0,
                    mount_type: pb::MountType::Bind as i32,
                    ..Default::default()
                },
            ],
            ..Default::default()
        })),
        ..Default::default()
    };
    let exec_bytes = exec.encode_to_vec();
    let exec_digest = format!("sha256:{}", crate::store::sha256_hex(&exec_bytes));

    pb::Definition {
        def: vec![
            src_bytes,
            mkdir_bytes,
            exec_bytes,
            // The terminal: no union, one input, and LAST. loadLLB deletes
            // exactly the last entry and hands anything else union-less to
            // `ResolveOp`, which reports `no support for <nil>`.
            pb::Op {
                inputs: vec![pb::Input {
                    digest: exec_digest,
                    index: 0,
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ],
        ..Default::default()
    }
}

/// Give named cache mounts a starting point, so a cold worker is not cold.
///
/// This is the answer to the largest measured cost in the project. Lifting a
/// cache mount is what makes a subtree dispatchable - otherwise it is
/// grounded to the machine holding the mount - but lifting the hazard does
/// not lift the cost: the worker builds against its OWN mount, which is
/// empty, so `go mod download` runs again. Measured at ~24s a lead across 64
/// leads on one target, and it is why a six-machine `+all-binaries` loses to
/// a one-machine baseline that fills one `/go/pkg/mod` and reuses it five
/// times.
///
/// buildkit already supports the fix and nothing has been using it.
/// `getRefCacheDirNoCache` creates a cache dir as a copy-on-write ref over
/// the mount's INPUT when no dir exists yet - `cm.New(ctx, ref, ...)` in
/// `solver/llbsolver/mounts/mount.go`. So a cache mount with an input starts
/// filled. `vertex.go` names the same property in a comment: "value shows in
/// mount is on top of a ref".
///
/// The Earthfile never learns about this (principle 15). We are already
/// rewriting the graph to make it portable; this is one more rewrite on the
/// same spine.
///
/// Only the ids in `seeds` are touched. Seeding every mount would make a
/// worker pull an image for a cache nobody measured, and the ranking of
/// which are worth it is what [`crate::driver::Driver::cache_costs`] exists
/// to produce.
///
/// A mount that ALREADY has an input is seeded by replacing it. That is not
/// a special case: every earthly cache mount has one, and the earlier
/// `input < 0` filter meant this function could never apply to a real graph
/// at all - a run reported four seeds resolved and rewrote nothing. The
/// input is what selects which cache directory the mount is
/// (`getRefCacheDir` keys on the id plus the input's ref), so pointing it at
/// the seed both chooses a directory and supplies its contents.
///
/// The old input's `selector` is cleared with it. Earthly's is
/// `SourcePath("/cache")`, a path inside the state IT mounted; the seed
/// image's root is the cache contents, so keeping the selector would pick a
/// directory the seed does not have and the mount would come up empty -
/// which looks exactly like not seeding.
pub fn seed_cache_mounts(def: &pb::Definition, seeds: &BTreeMap<String, String>) -> pb::Definition {
    if seeds.is_empty() {
        return def.clone();
    }
    let digest = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
    let source_for = |image: &str| -> (Vec<u8>, String) {
        let bytes = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: if image.contains("://") {
                    image.to_owned()
                } else {
                    format!("docker-image://{image}")
                },
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_to_vec();
        let d = digest(&bytes);
        (bytes, d)
    };

    // The source ops go FIRST and in one pass. LLB is topological and a
    // source has no inputs, so the front is always legal; appending them
    // instead would put an op after the terminal, which `loadLLB` reads as a
    // second terminal and rejects with `no support for <nil>`. That failure
    // mode has already cost this project a day.
    let mut added: Vec<Vec<u8>> = Vec::new();
    let mut seed_digest: BTreeMap<String, String> = BTreeMap::new();
    let mut present: BTreeSet<String> = def.def.iter().map(|b| digest(b)).collect();
    for image in seeds.values().collect::<BTreeSet<_>>() {
        // REFUSED, not wrapped. `sha256:abc` is content with no location -
        // what `build_subtree` answers with - and `docker-image://sha256:abc`
        // parses nowhere. The seeding path emitted exactly that for one
        // commit and it would have failed quietly, because the resolve check
        // drops a seed it cannot read and the run then reports that seeding
        // did not pay for a mechanism that never addressed anything.
        // `solve::pullable` is what turns one into the other.
        if image.starts_with("sha256:") {
            println!(
                "[dispatch] seed {image} is a bare digest, not a reference - \
                 needs a registry in front of it (see solve::pullable). Skipping."
            );
            continue;
        }
        let (bytes, d) = source_for(image);
        if present.insert(d.clone()) {
            added.push(bytes);
        }
        seed_digest.insert(image.clone(), d);
    }

    let used = std::cell::Cell::new(false);
    let edited = rewrite_ops(def, &|op| {
        let Some(pb::op::Op::Exec(e)) = op.op.as_mut() else {
            return false;
        };
        // Collected first: `op.inputs` and `op.op` cannot both be borrowed
        // mutably, and the index of a new input depends on how many were
        // appended before it.
        // NO `input < 0` FILTER. It was there, and it meant this transform
        // could never touch a real graph: every earthly cache mount already
        // carries an input - `AddMount(target, cacheContext.File(Mkdir(
        // "/cache")), ...)` - so the filter excluded all of them. A run
        // reported `seeds=4/4` resolved and rewrote nothing at all.
        //
        // Replacing the input is what seeding MEANS here. The input selects
        // which cache directory the mount is (`getRefCacheDir` keys on id
        // plus the input's ref), so pointing it at the seed both chooses a
        // directory and supplies its initial contents.
        let wants: Vec<(usize, String)> = e
            .mounts
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let id = &m.cache_opt.as_ref()?.id;
                seeds
                    .get(id)
                    .and_then(|img| seed_digest.get(img))
                    .map(|d| (i, d.clone()))
            })
            .collect();
        if wants.is_empty() {
            return false;
        }
        for (mount, d) in wants {
            // Reuse an input edge if this op already has one pointing at the
            // seed, rather than adding a duplicate per mount.
            let at = match op.inputs.iter().position(|i| i.digest == d) {
                Some(at) => at,
                None => {
                    op.inputs.push(pb::Input {
                        digest: d,
                        index: 0,
                    });
                    op.inputs.len() - 1
                }
            };
            // APPENDED, never inserted: every existing `Mount.input` is an
            // index into this vector and renumbering them would silently
            // remount the rootfs somewhere else.
            let Some(pb::op::Op::Exec(e)) = op.op.as_mut() else {
                continue;
            };
            e.mounts[mount].input = at as i64;
            // AND THE SELECTOR GOES WITH IT. Earthly's mount carries
            // `SourcePath("/cache")`, a path inside the OLD input. The
            // seed image's root IS the cache contents, so keeping `/cache`
            // would select a directory the seed does not have and the mount
            // would come up empty - which is the same symptom as not
            // seeding at all, and this project has spent enough runs on
            // that distinction.
            e.mounts[mount].selector = String::new();
            used.set(true);
        }
        true
    });
    if !used.get() {
        return def.clone();
    }
    crate::mech::applied("seed_mounts");
    let mut out = edited;
    // Prepended, and the metadata comes along or buildkit treats the vertex
    // as having no options at all.
    let mut def_out = added;
    def_out.append(&mut out.def);
    out.def = def_out;
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

/// Should placement prefer a peer that already holds the subtree's ancestry?
///
/// OFF by default, like every other mechanism here, and for the same reason:
/// three of them have now been measured and all three cost more than they
/// saved. This one at least SUBTRACTS handovers rather than adding them, but
/// that is an argument, and arguments have lost to measurements every time.
/// Is this graph too small to be worth sending anywhere?
///
/// Placing a lead has a near-constant toll, so the smallest job pays the
/// most. `+test-ast` dispatched 412 solves to run a 204-second build and took
/// 1773s: the median lead ran 2.6 seconds to do a `jq` and a `diff`, and lead
/// duration barely varies with what is in the lead.
///
/// The first version of this said "a lead costs what it fetches", on a byte
/// count that turned out to be loopback - across 414 leads, bytes served
/// correlate with duration at r = 0.06. Op count is a proxy for the SIZE OF
/// THE JOB, which is what has to beat the toll; it is not a proxy for
/// transport, and against bytes it correlates at only 0.32.
///
/// Op count is a crude proxy for how much work a graph is, and deliberately
/// so - principle 13. It is known before dispatch, it needs no history, and
/// the failure it prevents is enormous while the cost of being wrong is one
/// solve built at home. That asymmetry is what makes a crude rule the right
/// one here.
///
/// The floor is INCLUSIVE at the other end: a graph with exactly `floor`
/// ops travels. Zero disables it, which is today's behaviour and what every
/// number in `docs/fleet-findings.md` was measured against.
///
/// Not the same question as `min_siblings`, which asks whether anything
/// else is in flight. That one is about whether dispatch can overlap; this
/// is about whether it is worth doing at all, and it would not have stopped
/// any of the 412.
pub fn too_small_to_send(ops: usize, floor: usize) -> bool {
    floor > 0 && ops < floor
}

/// The op floor from the environment, read once.
pub fn min_ops() -> usize {
    static M: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("REBUCK2_MIN_OPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

pub fn affinity() -> bool {
    std::env::var("REBUCK2_AFFINITY").as_deref() == Ok("1")
}

/// Images to start named cache mounts from, as `id=ref,id=ref`.
///
/// NAMED, never discovered, and that is principle 20 rather than a stage
/// this will grow out of. `cache_ids` already enumerates every id a graph
/// mentions and the cost table already ranks them by seconds, so harvesting
/// the lot is a few lines away - and it would be wrong. The ranking says
/// which are EXPENSIVE; it cannot say which are SAFE. A self-keying cache
/// (`go-mod`, `go-build`, `npm`) addresses every entry by content or by
/// name-and-version, so a seeded entry that does not belong simply is never
/// looked up. A positional one - a scratch dir, an output staging area -
/// hands somebody else's bytes to a build that asked for a path, and that
/// is principle 5's line.
///
/// An operator writing `go-mod` here is asserting something about Go's
/// module cache that no measurement from outside can establish.
///
/// The refs are produced by `rebuck2 harvest-cache`, which is a separate
/// step for a separate reason: harvesting fails in its own ways, and the
/// measurement this exists for needs the seed to be a fixed input to a run
/// rather than something that may or may not have happened inside it.
///
/// Read once, like every other policy here: a seed map that changed halfway
/// through a build would give two identical subtrees different graphs.
pub fn cache_seeds() -> &'static BTreeMap<String, String> {
    static S: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        // A FILE as well as a variable, and that is not a convenience. The
        // seeds are harvested from the daemon that just did the work, which
        // is after this process starts - a variable set then would never
        // reach it. This is read lazily, on the first dispatch, so a file
        // written between startup and the fleet leg is seen.
        let inline = std::env::var("REBUCK2_CACHE_SEEDS").ok();
        let from_file = std::env::var("REBUCK2_CACHE_SEEDS_FILE")
            .ok()
            .filter(|p| !p.is_empty())
            .and_then(|p| match std::fs::read_to_string(&p) {
                Ok(t) => Some(t),
                Err(e) => {
                    // NAMED and not silent: a seeds file the harvest failed
                    // to write looks exactly like seeding not paying off.
                    println!("[dispatch] REBUCK2_CACHE_SEEDS_FILE {p}: {e}");
                    None
                }
            });
        let mut m = parse_cache_seeds(inline.as_deref());
        // Newlines count as separators too, because a file written one pair
        // per line is what a shell loop produces.
        m.extend(parse_cache_seeds(
            from_file.map(|t| t.replace('\n', ",")).as_deref(),
        ));
        if !m.is_empty() {
            println!("[dispatch] cache seeds: {m:?}");
        }
        m
    })
}

/// `id=ref` pairs, comma separated. Anything unparseable is dropped LOUDLY.
///
/// Silence here would be the third instance of a mechanism measured while
/// switched off: a typo in one pair would leave the others working and the
/// run would look like seeding simply did not pay.
pub fn parse_cache_seeds(raw: Option<&str>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for part in raw.unwrap_or_default().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((id, r)) if !id.trim().is_empty() && !r.trim().is_empty() => {
                out.insert(id.trim().to_owned(), r.trim().to_owned());
            }
            _ => println!("[dispatch] REBUCK2_CACHE_SEEDS: ignoring {part:?}, wanted id=ref"),
        }
    }
    out
}

/// Who to offer this subtree to, best first. Empty means build it yourself.
/// One peer's warmth for one subtree: ops it has built, mounts it has filled.
///
/// Two different things are being counted and they are not the same size.
/// Reusing an op saves what that op cost. Meeting a warm cache MOUNT saves
/// what a cold one costs, and one run measured 64 leads at ~24s each behind
/// a cold `go-mod` against a p50 lead of 8.6s - so a warm mount is worth
/// more than a whole median lead, and has to outrank any op overlap a single
/// subtree can plausibly contain.
///
/// 64 is coarse on purpose (principle 13). It is not a calibration; it is
/// "more ops than a subtree here has", so a shared mount always wins and op
/// overlap breaks the ties between peers holding the same mounts. A finer
/// number would need per-id costs the driver only learns at the END of a
/// run, which is a generation too late to place anything.
pub fn warmth(ops: u32, caches: u32) -> u32 {
    const MOUNT: u32 = 64;
    ops.saturating_add(caches.saturating_mul(MOUNT))
}

// Affinity-free form. The driver always passes a warmth function now, so
// this survives as the definition of the default order and is what the
// ordering tests pin.
#[allow(dead_code)]
pub fn offer_order(v: &Verdict, cands: &[Candidate], allow: Allow) -> Vec<u64> {
    offer_order_warm(v, cands, allow, &|_| 0)
}

/// [`offer_order`], preferring a peer that already holds this subtree's
/// ancestry.
///
/// `warm(id)` is how much of this work that peer has already done - ops it
/// has built before. Least-loaded-first alone is load BALANCING, and
/// balancing is the wrong default here: it spreads work that shares
/// ancestry across machines, and every machine that touches a layer pays to
/// pull, decompress and unpack it into its own snapshotter. Measured on a
/// full `+test-no-qemu`: `op duplication 2.3x built`, every op materialised
/// on 2.3 machines against an ideal of 1.
///
/// Affinity is applied SUBJECT to capacity, never instead of it - the
/// `free() > 0` filter still runs first, so a warm peer that would decline
/// is still not asked. Within what is left, warmth outranks emptiness and
/// emptiness breaks ties, so this refines the old order rather than
/// replacing it.
pub fn offer_order_warm(
    v: &Verdict,
    cands: &[Candidate],
    allow: Allow,
    warm: &dyn Fn(u64) -> u32,
) -> Vec<u64> {
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
    // Warmest first, then emptiest so the work starts soonest. Ties on id,
    // so two drivers deciding from the same state offer in the same order
    // rather than crossing over.
    let before: Vec<u64> = able.iter().map(|c| c.id).collect();
    able.sort_by_key(|c| {
        (
            std::cmp::Reverse(warm(c.id)),
            std::cmp::Reverse(c.load.free()),
            c.id,
        )
    });
    // APPLIED means the order changed, not that the flag was on. An
    // affinity that never reorders anything is indistinguishable from one
    // that is switched off, and that distinction has cost three mechanisms.
    if before != able.iter().map(|c| c.id).collect::<Vec<u64>>() {
        crate::mech::applied("affinity");
    }
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
    #[allow(dead_code)] // as offer_order: the default order, pinned by tests
    pub fn new(v: &Verdict, cands: &[Candidate], allow: Allow) -> Self {
        Placement::new_warm(v, cands, allow, &|_| 0)
    }

    /// [`Placement::new`], preferring peers that already hold the ancestry.
    /// See [`offer_order_warm`].
    pub fn new_warm(
        v: &Verdict,
        cands: &[Candidate],
        allow: Allow,
        warm: &dyn Fn(u64) -> u32,
    ) -> Self {
        Placement {
            order: offer_order_warm(v, cands, allow, warm),
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

/// Swap `git://` sources for images the driver already fetched.
///
/// A git source is the largest single reason earthbuild's own Earthfile does
/// not dispatch - 407 of 1018 solves on `+test-no-qemu`. buildkit resolves
/// git credentials through the client session, and a worker has none, so the
/// subtree is grounded by [`Exclusion::SessionSource`].
///
/// The driver DOES have the session. It can fetch the tree once and publish
/// it as content, exactly as it already does for base images and local
/// contexts, and then the graph names something any peer can pull. Same
/// trick, third kind of source.
pub fn rewrite_git_sources(
    def: &pb::Definition,
    replacement: &dyn Fn(&str) -> Option<String>,
) -> pb::Definition {
    rewrite_sources(def, "git://", replacement)
}

/// The subgraph rooted at `root`, as a Definition a peer can solve.
///
/// The missing dispatch unit. `analyse` has found cuts since the beginning and
/// used them for a log line; nothing has ever BUILT one. That is why eight
/// measured attempts to stop workers rebuilding the shared ancestry all
/// failed the same way: the ancestry is interior to every dispatched graph,
/// so it is never published, so it can never be grafted. A prefix has to be
/// solved as a subtree in its own right before it can be imported as one.
///
/// Keeps only ops reachable from `root`, in their original order (buildkit
/// marshals topologically, and preserving order keeps digests stable), and
/// appends a terminal pointing at the root - which is buildkit's own
/// convention for "this is the result".
pub fn subgraph(def: &pb::Definition, root: usize) -> Option<pb::Definition> {
    let digest = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
    let by_digest: BTreeMap<String, usize> = def
        .def
        .iter()
        .enumerate()
        .map(|(i, b)| (digest(b), i))
        .collect();

    let mut keep = std::collections::BTreeSet::new();
    let mut stack = vec![root];
    while let Some(i) = stack.pop() {
        if !keep.insert(i) {
            continue;
        }
        let Some(op) = def
            .def
            .get(i)
            .and_then(|b| pb::Op::decode(b.as_slice()).ok())
        else {
            continue;
        };
        for input in &op.inputs {
            if let Some(&j) = by_digest.get(&input.digest) {
                stack.push(j);
            }
        }
    }
    // A cut of one op is the op itself: dispatching it buys nothing and costs
    // a publish.
    if keep.len() < 2 {
        return None;
    }

    // Every op we keep must have an op union. The one op that legitimately
    // has none is the terminal, and buildkit recognises exactly one of those:
    // whichever entry is LAST. Hand it a graph containing a second - by
    // cutting AT the terminal, so the synthetic one we append points at the
    // original - and it resolves that original as a vertex, falls off the end
    // of ResolveOp's switch, and reports
    //
    //     failed to load cache key: no support for <nil>
    //
    // naming neither the op nor the field, because `%T` of a nil interface is
    // all it has to print. Cost three wrong theories: a lossy re-encode, then
    // a dropped platform, then reading `worker/base/worker.go:385`.
    if keep.iter().any(|&i| {
        pb::Op::decode(def.def[i].as_slice())
            .map(|o| o.op.is_none())
            .unwrap_or(true)
    }) {
        return None;
    }

    let root_digest = digest(def.def.get(root)?);
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(keep.len() + 1);
    let mut metadata = BTreeMap::new();
    for i in keep.iter().copied() {
        let bytes = &def.def[i];
        if let Some(m) = def.metadata.get(&digest(bytes)) {
            metadata.insert(digest(bytes), m.clone());
        }
        out.push(bytes.clone());
    }
    // The terminal. No `op`, one input - exactly how buildkit marshals the
    // end of a definition, and what `build_subtree` will export.
    let term = pb::Op {
        inputs: vec![pb::Input {
            digest: root_digest,
            index: 0,
        }],
        ..Default::default()
    };
    out.push(term.encode_to_vec());

    Some(pb::Definition {
        def: out,
        metadata: metadata.into_iter().collect(),
        ..def.clone()
    })
}

/// Replace ops we have ALREADY BUILT with an import of their published result.
///
/// This is the step from N prefixes to 1, and it is the only thing measurement
/// has left standing. A dispatched subtree carries its whole ancestor chain,
/// buildkit dedupes only within one daemon, so three workers build the shared
/// prefix three times where one machine builds it once - measured at 675s of
/// lead-work against a 144s baseline, which is the duplication ceiling and not
/// a scheduling problem.
///
/// Keyed on the OP DIGEST, which is sound because a buildkit op's bytes embed
/// the digests of its inputs: two ops with the same digest have the same
/// ancestry, transitively, so an image built from one is a correct substitute
/// for the other.
///
/// The replaced op keeps no inputs. That is the point - the ancestry stops
/// being work and becomes a pull.
/// Does a round trip through OUR proto types preserve these bytes?
///
/// Op bytes come from earthly's FORK of the buildkit proto. prost keeps only
/// the fields it models and drops the rest without complaint, so any
/// re-encode is potentially lossy - and the loss surfaces far away: an
/// ExecOp that lost its platform is reported by buildkit as
///
///     no support for running processes with <nil> platform
///
/// which earthly truncates to `no support for <nil>`, naming neither the op
/// nor the field. Three field-loss bugs have cost a day between them - the
/// dropped exporter, the unmodelled SOCKET=101 mount, and this.
#[cfg(debug_assertions)]
fn reencode_is_lossless(bytes: &[u8]) -> bool {
    match pb::Op::decode(bytes) {
        // Undecodable is not loss: both rewrites carry those verbatim and
        // never re-encode them.
        Err(_) => true,
        Ok(op) => op.encode_to_vec() == bytes,
    }
}

/// Assert an op carried VERBATIM would have survived a round trip.
///
/// Checked exactly where both forms are in hand. It does not protect the
/// verbatim path - that path is already safe - it detects that some OTHER
/// path which re-encodes would be lossy, at the point where the op is
/// available to name. Debug-only: the answer cannot change at runtime.
macro_rules! debug_assert_lossless {
    ($bytes:expr, $where:literal) => {
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                reencode_is_lossless($bytes),
                "{}: a round trip through our proto types CHANGED these op \
                 bytes. Carrying verbatim here is correct; the warning is \
                 that any path re-encoding this op silently drops whatever \
                 earthly's fork added to it.",
                $where
            );
        }
    };
}

pub fn graft_built(def: &pb::Definition, built: &dyn Fn(&str) -> Option<String>) -> pb::Definition {
    let digest = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));

    // Untouched means UNTOUCHED. Reserialising an unchanged graph changes
    // every digest and buildkit reads that as a whole-build cache miss.
    if !def.def.iter().any(|b| built(&digest(b)).is_some()) {
        return def.clone();
    }

    // Only ops every consumer reads at INDEX 0 may be grafted.
    //
    // An image source has one output. An ExecOp has one per mount, so a
    // consumer can legitimately reference index 1, 2, ... - and replacing
    // that op with a source leaves those inputs pointing at an output that
    // does not exist. buildkit reports it three steps later as
    //
    //     failed to load cache key: no support for <nil>
    //
    // which names neither the op nor the index. Measured on six machines:
    // every worker declined, the fleet leg failed, and the message pointed
    // nowhere near the graft that caused it.
    let mut multi_output: std::collections::BTreeSet<String> = Default::default();
    for bytes in &def.def {
        if let Ok(op) = pb::Op::decode(bytes.as_slice()) {
            for input in &op.inputs {
                if input.index != 0 {
                    multi_output.insert(input.digest.clone());
                }
            }
        }
    }

    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(def.def.len());
    let mut metadata = def.metadata.clone();

    for bytes in &def.def {
        let before = digest(bytes);
        if let Some(reference) = built(&before).filter(|_| !multi_output.contains(&before)) {
            // Grafted: an image source in place of the op and everything it
            // depended on. Its inputs are dropped, so the ops above it become
            // unreachable - harmless, buildkit walks from the terminal.
            // KEEP THE PLATFORM. A source op carries one, and buildkit
            // resolves the image for it - `mirror_image` sets it for exactly
            // this reason, or an arm64 daemon mirrors the arm64 variant and
            // an x86 peer dies on `exit code: 255`.
            //
            // Dropping it here produced, three steps away:
            //
            //     no support for running processes with <nil> platform
            //
            // truncated by earthly to `no support for <nil>`. The first
            // theory was a lossy proto round trip - plausible, since this
            // codebase has lost an exporter and a mount type that way - and
            // it was wrong: the platform was not lost in translation, it was
            // never copied.
            //
            // Constraints travel too: a worker-selection constraint on the
            // op it replaces still applies to fetching the result.
            let replaced = pb::Op::decode(bytes.as_slice()).ok();
            let src = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: reference,
                    ..Default::default()
                })),
                platform: replaced.as_ref().and_then(|o| o.platform.clone()),
                constraints: replaced.as_ref().and_then(|o| o.constraints.clone()),
                ..Default::default()
            };
            let nb = src.encode_to_vec();
            let after = digest(&nb);
            // An op REPLACED by its built ancestor: the outcome grafting
            // exists to produce. Counted because grafting spent a week
            // switched on while its flag said otherwise.
            crate::mech::applied("graft");
            if after != before {
                remap.insert(before.clone(), after.clone());
                if let Some(m) = metadata.remove(&before) {
                    metadata.insert(after, m);
                }
            }
            out.push(nb);
            continue;
        }
        let Ok(mut op) = pb::Op::decode(bytes.as_slice()) else {
            out.push(bytes.clone());
            continue;
        };
        // RE-ENCODE ONLY WHAT CHANGED. prost drops fields it does not model,
        // and these bytes come from earthly's FORK of the buildkit proto -
        // so a round trip through our types is lossy for anything the fork
        // added. An op whose inputs were not remapped must travel verbatim.
        //
        // Measured: re-encoding every op stripped the ExecOp platform, and
        // buildkit refused the graph with
        //
        //     no support for running processes with <nil> platform
        //
        // which earthly truncates to `no support for <nil>`, naming neither
        // the op nor the field. Same class as the exporter that was dropped
        // for being an unknown field this morning: protobuf loses what it
        // does not know, quietly.
        let touched = op.inputs.iter().any(|i| remap.contains_key(&i.digest));
        if !touched {
            debug_assert_lossless!(bytes, "graft_built");
            out.push(bytes.clone());
            continue;
        }
        for input in &mut op.inputs {
            if let Some(new) = remap.get(&input.digest) {
                input.digest = new.clone();
            }
        }
        let nb = op.encode_to_vec();
        let after = digest(&nb);
        if after != before {
            remap.insert(before.clone(), after.clone());
            if let Some(m) = metadata.remove(&before) {
                metadata.insert(after, m);
            }
        }
        out.push(nb);
    }

    prune(pb::Definition {
        def: out,
        metadata,
        ..def.clone()
    })
}

/// What the graph calls the op at `index`, if it calls it anything.
///
/// `llb.customname` is buildkit's own human label - the thing earthly prints
/// as `+base | --> FROM alpine` - and it is the only name an LLB graph
/// carries. Without it a lead is logged as the digest it produced, so the
/// job that owned 40% of a wall clock could not be identified at all.
///
/// `None` rather than a placeholder: the caller falls back to the digest,
/// which is unhelpful but true.
pub fn describe(def: &pb::Definition, index: usize) -> Option<String> {
    let bytes = def.def.get(index)?;
    let digest = format!("sha256:{}", crate::store::sha256_hex(bytes));
    let raw = def
        .metadata
        .get(&digest)?
        .description
        .get("llb.customname")
        .filter(|n| !n.is_empty())?;
    Some(readable(raw))
}

/// Turn earthly's vertex name into something a human can read.
///
/// earthly writes `[<base64 VertexMeta JSON>] <human tail>`, so the target
/// name - the one thing worth knowing about a 250-second lead - is inside
/// the base64, and the readable half comes AFTER the bracket. Reading
/// between the brackets yields a wall of base64 and discards the useful
/// part, which is exactly what the first version of the CI panel printed.
///
/// Anything that does not parse is returned unchanged: a name we cannot
/// decode still beats no name.
fn readable(raw: &str) -> String {
    use base64::Engine;

    let Some((b64, tail)) = raw
        .strip_prefix('[')
        .and_then(|r| r.split_once("] "))
        .filter(|(b64, _)| !b64.is_empty())
    else {
        return raw.to_owned();
    };
    let Ok(json) = base64::engine::general_purpose::STANDARD.decode(b64) else {
        return raw.to_owned();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&json) else {
        return raw.to_owned();
    };
    // `tnm` is VertexMeta.TargetName; `sl` its source location. Either may
    // be absent - a vertex earthly generates itself has no target.
    let target = v.get("tnm").and_then(|t| t.as_str()).unwrap_or_default();
    let at = match (
        v.pointer("/sl/file").and_then(|f| f.as_str()),
        v.pointer("/sl/startLine").and_then(|l| l.as_u64()),
    ) {
        (Some(f), Some(l)) => format!("{f}:{l}"),
        (Some(f), None) => f.to_owned(),
        _ => String::new(),
    };
    [target, &at, tail]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a whole subtree is called: the name of the op its terminal points at.
///
/// The terminal itself is never named - it is a pointer, not work.
pub fn describe_root(def: &pb::Definition) -> Option<String> {
    let last = def.def.last()?;
    let target = pb::Op::decode(last.as_slice())
        .ok()?
        .inputs
        .first()?
        .digest
        .clone();
    let at = def
        .def
        .iter()
        .position(|b| format!("sha256:{}", crate::store::sha256_hex(b)) == target)?;
    describe(def, at)
}

/// Apply an in-place edit to every op, cascading the digests it changes.
///
/// The shared spine of every graph rewrite here, and it exists because each
/// one that grew its own copy grew its own bug: an op re-encoded when it did
/// not change loses the fields earthly's fork added to the proto; a changed
/// op whose consumers are not updated dangles; a rewrite that reorders stops
/// the terminal being last and buildkit resolves it as a vertex.
///
/// `edit` returns whether it changed anything. Returning false must mean the
/// bytes are untouched, because that is what lets them travel verbatim.
fn rewrite_ops(def: &pb::Definition, edit: &dyn Fn(&mut pb::Op) -> bool) -> pb::Definition {
    let digest = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut metadata = def.metadata.clone();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(def.def.len());

    // In order, so the terminal stays last. LLB is topological, so an op's
    // inputs are always already remapped by the time it is reached.
    for bytes in &def.def {
        let before = digest(bytes);
        let Ok(mut op) = pb::Op::decode(bytes.as_slice()) else {
            out.push(bytes.clone());
            continue;
        };
        let touched_inputs = op.inputs.iter().any(|i| remap.contains_key(&i.digest));
        let edited = edit(&mut op);
        if !touched_inputs && !edited {
            debug_assert_lossless!(bytes, "rewrite_ops");
            out.push(bytes.clone());
            continue;
        }
        for input in &mut op.inputs {
            if let Some(new) = remap.get(&input.digest) {
                input.digest = new.clone();
            }
        }
        let nb = op.encode_to_vec();
        let after = digest(&nb);
        if after != before {
            remap.insert(before.clone(), after.clone());
            if let Some(m) = metadata.remove(&before) {
                metadata.insert(after, m);
            }
        }
        out.push(nb);
    }

    prune(pb::Definition {
        def: out,
        metadata,
        ..def.clone()
    })
}

/// Point every nested build at the daemon that is about to run it.
///
/// earthly forwards its own `BUILDKIT_HOST` into every `RUN`, so a nested
/// earthly shares the outer daemon instead of standing one up. On a single
/// machine that daemon is local and the forwarding halves the build. In a
/// fleet it is the COORDINATOR, so every nested build on every worker dials
/// one machine: the five leads that own a full `+test-no-qemu` critical path
/// are all nested builds at 231-258s, each longer than the entire
/// single-machine build.
///
/// The address cannot be chosen where the graph is built - the converter
/// runs before placement and does not know which worker will execute the op.
/// earthly's own attempt at a machine-independent constant,
/// `tcp://buildkitsandbox:8372`, resolves locally for most execs and NOT for
/// `--privileged --entrypoint` ones, where the nested earthly reports
/// `could not connect to buildkit: timeout 1m0s`.
///
/// So the worker substitutes its own address when the subtree arrives, which
/// is the one place the answer is known. An EMPTY value is left alone: that
/// is `force_internal_buildkit` deliberately unsetting it so the nested build
/// stands up its own daemon, and filling it in would silently undo the
/// exemption.
///
/// Ops that do not carry the variable travel byte for byte - re-encoding
/// through our types drops whatever earthly's fork added to the proto.
pub fn retarget_buildkit_host(def: &pb::Definition, addr: &str) -> pb::Definition {
    let rewrite = |op: &mut pb::Op| -> bool {
        let Some(pb::op::Op::Exec(e)) = op.op.as_mut() else {
            return false;
        };
        let Some(meta) = e.meta.as_mut() else {
            return false;
        };
        let mut hit = false;
        for entry in &mut meta.env {
            let Some((k, v)) = entry.split_once('=') else {
                continue;
            };
            if k.ends_with("BUILDKIT_HOST") && !v.is_empty() && v != addr {
                *entry = format!("{k}={addr}");
                hit = true;
            }
        }
        hit
    };
    rewrite_ops(def, &rewrite)
}

/// Drop everything the terminal cannot reach.
///
/// Grafting orphans by construction: replacing a subtree root with the image
/// it built makes every op beneath it unreachable, and they stay in `def`
/// unless something removes them. Buildkit tolerates that - `loadLLB` walks
/// from the terminal and never looks at the rest - so it costs only bytes on
/// the wire, which is why it went unnoticed.
///
/// What it does NOT tolerate is our own arithmetic. The cut-prefix picker
/// excludes "the whole graph" by comparing a cut's op count against
/// `def.len()`; orphans inflate the divisor, a terminal-rooted cut slips
/// through, and the graph that gets dispatched has two terminals in it. The
/// symptom was `no support for <nil>`, three hops away from this line.
///
/// Order is preserved, so the terminal stays last - which is the only way
/// buildkit knows it IS the terminal.
fn prune(def: pb::Definition) -> pb::Definition {
    let digest = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
    let by_digest: BTreeMap<String, usize> = def
        .def
        .iter()
        .enumerate()
        .map(|(i, b)| (digest(b), i))
        .collect();

    let Some(last) = def.def.len().checked_sub(1) else {
        return def;
    };
    let mut keep = std::collections::BTreeSet::new();
    let mut stack = vec![last];
    while let Some(i) = stack.pop() {
        if !keep.insert(i) {
            continue;
        }
        let Some(op) = def
            .def
            .get(i)
            .and_then(|b| pb::Op::decode(b.as_slice()).ok())
        else {
            continue;
        };
        for input in &op.inputs {
            if let Some(&j) = by_digest.get(&input.digest) {
                stack.push(j);
            }
        }
    }
    if keep.len() == def.def.len() {
        return def;
    }

    let out: Vec<Vec<u8>> = keep.iter().map(|&i| def.def[i].clone()).collect();
    let live: std::collections::BTreeSet<String> = out.iter().map(|b| digest(b)).collect();
    let metadata = def
        .metadata
        .into_iter()
        .filter(|(k, _)| live.contains(k))
        .collect();
    pb::Definition {
        def: out,
        metadata,
        ..def
    }
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

        let mut changed = false;
        for input in &mut op.inputs {
            if let Some(new) = remap.get(&input.digest) {
                input.digest = new.clone();
                changed = true;
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
                changed = true;
                // Local sources carry filesync attrs - include patterns,
                // session ids - that mean nothing to a registry source and
                // would be a stale reference to a session the peer has no
                // part in.
                src.attrs.clear();
            }
        }

        // Verbatim when nothing changed, for the same reason as
        // `graft_built`: these bytes come from earthly's FORK of the
        // buildkit proto and a round trip through our types drops whatever
        // the fork added. Long-standing here and never observed to bite,
        // which is not the same as safe.
        if !changed {
            debug_assert_lossless!(bytes, "rewrite_sources");
            out.push(bytes.clone());
            continue;
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
    /// A graph too small to be worth the bytes it drags across.
    ///
    /// `+test-ast` dispatched 412 solves to run a 204-second build and took
    /// 1773s. Median lead: 2.6s, for a `jq` and a `diff`. Duration barely
    /// varies with what is in the lead, so a lead carrying milliseconds of
    /// work is pure loss - it pays the same toll as a real one.
    #[test]
    fn a_graph_can_be_too_small_to_send() {
        let keep = super::too_small_to_send;
        // Off by default: zero means no floor, which is today's behaviour
        // and what every number in the findings was measured against.
        assert!(!keep(2, 0));
        assert!(!keep(763, 0));
        // With a floor, the tiny ones stay home and the big ones go.
        assert!(keep(2, 20));
        assert!(keep(16, 20));
        assert!(!keep(20, 20), "the floor is inclusive - 20 ops clears it");
        assert!(!keep(37, 20), "the median +test-ast lead still travels");
        assert!(!keep(763, 20));
    }

    /// The proxy writes these and the harvest reads them, in two processes
    /// and often two runs apart. One function each way, tested together.
    #[test]
    fn observed_cache_inputs_survive_the_round_trip() {
        let mut m = std::collections::BTreeMap::new();
        // Op bytes are arbitrary protobuf: NUL, newline and tab all occur,
        // which is why they are base64 and not written raw. A raw write
        // would corrupt on the first op containing a tab and the harvest
        // would silently read one field short.
        m.insert(
            "go-mod".to_owned(),
            (vec![0u8, 10, 9, 255, 128, 1], "/cache".to_owned()),
        );
        m.insert(
            "/run/cache/abc123/root/.cache/golangci_lint".to_owned(),
            (vec![1u8, 2, 3], String::new()),
        );

        let text = super::encode_cache_inputs(&m);
        assert_eq!(text.lines().count(), 2);
        let back = super::decode_cache_inputs(&text);
        assert_eq!(back, m, "byte for byte, including an empty selector");

        // A truncated or foreign line is dropped, not guessed at. This file
        // rides between runs in a cache, so it can arrive half-written.
        let back = super::decode_cache_inputs("go-mod\t/cache\nnot-a-line\n\n");
        assert!(back.is_empty(), "a line missing its payload is not a seed");
        assert!(super::decode_cache_inputs("").is_empty());
    }

    #[test]
    fn a_leading_tilde_is_expanded_because_a_workflow_env_does_not() {
        // GitHub evaluates `${{ }}` in an `env:` value and nothing else, so
        // `~/.cache/x` reaches the process as a literal tilde and every
        // write to it fails on a directory called `~`. The shell would have
        // expanded it; a process started with that env does not.
        std::env::set_var("HOME", "/home/runner");
        assert_eq!(super::expand_home("~/.cache/x"), "/home/runner/.cache/x");
        assert_eq!(super::expand_home("~"), "/home/runner");
        // Not a prefix match on the character: `~foo` is a username in
        // shell and is not ours to interpret.
        assert_eq!(super::expand_home("~foo/x"), "~foo/x");
        assert_eq!(super::expand_home("/tmp/x"), "/tmp/x");
        assert_eq!(super::expand_home("relative/x"), "relative/x");
    }

    /// The input op is TAKEN from a real graph, not rebuilt from a reading
    /// of one.
    ///
    /// Reconstructing earthly's `Scratch().File(Mkdir("/cache"))` produced
    /// an op that did not hash to earthly's ref, so the harvest read an
    /// empty directory - `getRefCacheDir` keys on `ref.ID()` and near enough
    /// does not exist. Any of a platform, a constraint or a differing
    /// convention for the FileAction's unused fields is a different digest,
    /// and the failure is silent.
    ///
    /// The proxy sees the bytes. Taking them verbatim cannot drift.
    #[test]
    fn a_cache_mounts_input_is_lifted_out_of_a_real_graph() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        // Something a reconstruction would get wrong: a platform on the op.
        let ctx = pb::Op {
            platform: Some(pb::Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                ..Default::default()
            }),
            op: Some(pb::op::Op::File(pb::FileOp {
                actions: vec![pb::FileAction {
                    input: -1,
                    secondary_input: -1,
                    output: 0,
                    action: Some(pb::file_action::Action::Mkdir(pb::FileActionMkDir {
                        path: "/cache".into(),
                        mode: 0o644,
                        ..Default::default()
                    })),
                }],
            })),
            ..Default::default()
        };
        let ctx_bytes = ctx.encode_to_vec();
        let ctx_digest = format!("sha256:{}", crate::store::sha256_hex(&ctx_bytes));
        let base = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "docker-image://golang:1".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let exec = pb::Op {
            inputs: vec![
                pb::Input {
                    digest: format!("sha256:{}", crate::store::sha256_hex(&base.encode_to_vec())),
                    index: 0,
                },
                pb::Input {
                    digest: ctx_digest.clone(),
                    index: 0,
                },
            ],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                mounts: vec![
                    pb::Mount {
                        input: 0,
                        dest: "/".into(),
                        output: 0,
                        ..Default::default()
                    },
                    pb::Mount {
                        input: 1,
                        selector: "/cache".into(),
                        dest: "/go/pkg/mod".into(),
                        output: -1,
                        mount_type: pb::MountType::Cache as i32,
                        cache_opt: Some(pb::CacheOpt {
                            id: "go-mod".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let def = pb::Definition {
            def: vec![
                base.encode_to_vec(),
                ctx_bytes.clone(),
                exec.encode_to_vec(),
            ],
            ..Default::default()
        };

        let found = super::cache_mount_inputs(&def);
        assert_eq!(
            found.get("go-mod").map(|(b, sel)| (b.clone(), sel.clone())),
            Some((ctx_bytes, "/cache".to_owned())),
            "the exact bytes, and the selector that goes with them"
        );

        // A mount with NO input contributes nothing rather than an empty
        // guess: seeding one of those needs no input to copy.
        let plain = pb::Definition {
            def: vec![pb::Op {
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    mounts: vec![pb::Mount {
                        input: -1,
                        dest: "/c".into(),
                        mount_type: pb::MountType::Cache as i32,
                        cache_opt: Some(pb::CacheOpt {
                            id: "bare".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                })),
                ..Default::default()
            }
            .encode_to_vec()],
            ..Default::default()
        };
        assert!(super::cache_mount_inputs(&plain).is_empty());
    }

    /// An id the Earthfile never named still has to be found.
    ///
    /// A mount written without `id=` is not keyed on its destination:
    /// earthly computes `/run/cache/<per-target hash>/<target>`. So a seed
    /// list saying `/root/.cache/golangci_lint` names an id that exists
    /// nowhere, and the harvest reads an empty directory it created itself -
    /// which is indistinguishable from the cache being cold.
    #[test]
    fn a_namespaced_cache_id_is_matched_by_its_tail() {
        let held = [
            "go-mod".to_owned(),
            "go-build".to_owned(),
            "/run/cache/b369714dd3084d9bf3adc7911b40056e0f36f2d79516e5474021e7d61bddc541\
             /root/.cache/golangci_lint"
                .to_owned(),
        ];
        let r = |want: &str| super::resolve_cache_id(want, &held);

        // An exact id wins outright, with no searching.
        assert_eq!(r("go-mod"), Some("go-mod".to_owned()));
        // The namespaced one is found by its tail.
        assert_eq!(r("/root/.cache/golangci_lint"), Some(held[2].clone()));
        // Nothing that matches is nothing, NOT a guess. Harvesting the
        // wrong id reads an empty dir and reports it as an empty cache.
        assert_eq!(r("npm"), None);
    }

    /// Two candidates is not an answer.
    #[test]
    fn an_ambiguous_cache_id_is_refused() {
        let held = [
            "/run/cache/aaa/root/.cache/x".to_owned(),
            "/run/cache/bbb/root/.cache/x".to_owned(),
        ];
        // Both end with the same tail - two targets with the same mount
        // path. Picking either would harvest one target's cache and seed it
        // into every other, which is the one thing principle 20 forbids
        // outright: a cache that is not the one asked for.
        assert_eq!(super::resolve_cache_id("/root/.cache/x", &held), None);
    }

    /// The harvest has to mount the cache the way EARTHLY mounts it.
    #[test]
    fn the_harvest_mounts_the_cache_the_way_earthly_does() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let def = super::harvest_graph("busybox:1", "go-mod", "/go/pkg/mod");
        let ops: Vec<pb::Op> = def
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).unwrap())
            .collect();

        // A FileOp appeared: scratch with /cache created, which is what
        // earthfile2llb hands to `AddMount` -
        //   state = c.cacheContext            // pllb.Scratch()
        //   state = state.File(pllb.Mkdir("/cache", mode))
        //   mountOpts = append(mountOpts, llb.SourcePath("/cache"))
        let mkdir = ops.iter().find_map(|o| match &o.op {
            Some(pb::op::Op::File(f)) => Some(f),
            _ => None,
        });
        let mkdir = mkdir.expect("the cache mount's input is a mkdir over scratch");
        assert_eq!(mkdir.actions.len(), 1);
        let Some(pb::file_action::Action::Mkdir(m)) = &mkdir.actions[0].action else {
            panic!("the one action is a mkdir")
        };
        assert_eq!(m.path, "/cache");
        // SCRATCH, not a base: input -1 on the action, so the FileOp builds
        // on nothing. Earthly's `cacheContext` is `pllb.Scratch()`, and the
        // ref this produces is what the cache key is built from - a
        // different base gives a different ref gives a different directory.
        assert_eq!(mkdir.actions[0].input, -1);

        let exec = ops
            .iter()
            .find_map(|o| match &o.op {
                Some(pb::op::Op::Exec(e)) => Some((o, e)),
                _ => None,
            })
            .expect("the copy");
        let (exec_op, e) = exec;
        let cache = e
            .mounts
            .iter()
            .find(|m| m.mount_type == pb::MountType::Cache as i32)
            .expect("the cache is mounted");

        // AND IT HAS THAT INPUT. Without one the key is plain `go-mod`,
        // which is a directory nothing has ever written to - four harvests
        // read 0.0 MiB out of exactly that.
        assert!(cache.input >= 0, "the cache mount must carry an input");
        let src = &exec_op.inputs[cache.input as usize];
        let mkdir_digest = format!(
            "sha256:{}",
            crate::store::sha256_hex(
                &def.def
                    .iter()
                    .find(|b| matches!(
                        pb::Op::decode(b.as_slice()).map(|o| o.op),
                        Ok(Some(pb::op::Op::File(_)))
                    ))
                    .unwrap()
                    .clone()
            )
        );
        assert_eq!(src.digest, mkdir_digest, "and the input is the mkdir");
        assert_eq!(cache.selector, "/cache", "with earthly's SourcePath");
    }

    /// A graph that pins a platform says which one, before anyone places it.
    #[test]
    fn a_pinned_graph_names_the_architecture_to_mirror_for() {
        use super::Platform;
        assert_eq!(Platform::Any.pinned(), None);
        assert_eq!(
            Platform::Pinned("linux/arm64".into()).pinned(),
            Some("linux/arm64")
        );
        // A CONFLICT names no single architecture, and guessing one would
        // mirror a base for the wrong half of a subtree that nowhere can run
        // anyway. `inspect` already refuses it; this must not quietly pick.
        assert_eq!(
            Platform::Conflict(["linux/amd64".into(), "linux/arm64".into()].into()).pinned(),
            None
        );
    }

    /// One graph shape for both halves of a seeding round trip.
    #[test]
    fn a_cache_probe_writes_or_reads_and_says_which() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let g = super::cache_probe_graph("alpine:3", "probe", "/c", "touch /c/marker");
        let ops: Vec<pb::Op> = g
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).unwrap())
            .collect();
        // Source, mkdir, exec, terminal. The mkdir is earthly's cache-mount
        // input, which the probe now reproduces so that it writes the same
        // directory earthly writes.
        assert_eq!(ops.len(), 4, "source, mkdir, exec, terminal");
        assert!(ops.last().unwrap().op.is_none(), "and the terminal is last");

        let e = ops
            .iter()
            .find_map(|o| match &o.op {
                Some(pb::op::Op::Exec(e)) => Some(e),
                _ => None,
            })
            .expect("the op that runs the command");
        let cache = e
            .mounts
            .iter()
            .find(|m| m.mount_type == pb::MountType::Cache as i32)
            .expect("a cache is mounted");
        assert_eq!(cache.cache_opt.as_ref().unwrap().id, "probe");
        assert_eq!(cache.dest, "/c");
        // WRITABLE, unlike the harvest's. A probe that writes needs to.
        assert!(!cache.readonly);
        // AND IT HAS EARTHLY'S INPUT. This assertion said the opposite -
        // "no input, so `seed_cache_mounts` has somewhere to attach one" -
        // which was true of the transform as written and false of every
        // graph earthly sends. Seeding replaces an input; it does not
        // require an absent one.
        assert_eq!(cache.input, 1, "the mkdir, as earthly mounts it");
        assert_eq!(cache.selector, "/cache");
        assert!(e
            .meta
            .as_ref()
            .unwrap()
            .args
            .join(" ")
            .contains("touch /c/marker"));
        let env = &e.meta.as_ref().unwrap().env;
        assert!(
            env.iter().any(|v| v.starts_with("PATH=")),
            "a probe runs `test` and `touch`, which are on no path without one: {env:?}"
        );

        // THE ROOTFS IS THE OUTPUT, and it has to be something.
        //
        // The terminal names `index: 0` of the exec, so an exec declaring no
        // output 0 is a graph that cannot export - and the first version of
        // this had no output at all, with a confident comment saying a probe
        // exports nothing. It does not need the LAYER; it needs the index to
        // exist. Caught by reading the graph back rather than by running it,
        // which would have been fault nine.
        assert_eq!(
            e.mounts
                .iter()
                .filter(|m| m.output == 0)
                .map(|m| m.dest.as_str())
                .collect::<Vec<_>>(),
            vec!["/"],
            "exactly one output, and it is the rootfs"
        );
        // Not the cache: buildkit refuses to export a cache mount, which is
        // why `harvest_graph` copies out of one instead.
        assert_eq!(cache.output, -1);
    }

    /// `id:path` pairs, and the shape that broke the shell version.
    #[test]
    fn seed_pairs_accept_an_id_that_is_a_path() {
        let f = super::parse_seed_pairs;
        assert_eq!(
            f("go-mod:/go/pkg/mod"),
            vec![("go-mod".to_owned(), "/go/pkg/mod".to_owned())]
        );
        // THE ONE THAT BROKE IT. A mount written without `id=` is keyed by
        // buildkit on its destination, so the id IS the path. The shell
        // version split on the first colon and then rejected id == path as
        // malformed - a guard written before this case existed, which then
        // threw away the two entries added to cover it.
        assert_eq!(
            f("/go/pkg/mod:/go/pkg/mod"),
            vec![("/go/pkg/mod".to_owned(), "/go/pkg/mod".to_owned())]
        );
        assert_eq!(
            f("/root/.cache/golangci_lint:/root/.cache/golangci_lint"),
            vec![(
                "/root/.cache/golangci_lint".to_owned(),
                "/root/.cache/golangci_lint".to_owned()
            )]
        );
        // Several, with the whitespace a human leaves in.
        assert_eq!(f("a:/x, b:/y").len(), 2);
        // Malformed shapes are dropped, not guessed at.
        assert!(f("nocolon").is_empty());
        assert!(f("id:relative/path").is_empty(), "a mount path is absolute");
        assert!(f(":/x").is_empty(), "an empty id names no cache");
        assert!(f("").is_empty());
    }

    /// The shapes `image_identifier` has to get right.
    #[test]
    fn a_short_image_name_is_qualified_the_way_buildkit_expects() {
        let f = super::image_identifier;
        assert_eq!(f("busybox:1"), "docker-image://docker.io/library/busybox:1");
        assert_eq!(f("alpine"), "docker-image://docker.io/library/alpine");
        // A user's repo on Docker Hub: `docker.io`, but no `library`.
        assert_eq!(
            f("earthbuild/buildkitd:v1"),
            "docker-image://docker.io/earthbuild/buildkitd:v1"
        );
        // A real host is left alone. A dot, a colon or `localhost` is what
        // distinguishes one from a Docker Hub namespace - the same rule
        // containerd's ParseNormalizedNamed uses, and the reason
        // `earthbuild/buildkitd` is NOT a host called `earthbuild`.
        assert_eq!(f("ghcr.io/x/y:v2"), "docker-image://ghcr.io/x/y:v2");
        assert_eq!(
            f("172.17.0.1:15000/r/s@sha256:aa"),
            "docker-image://172.17.0.1:15000/r/s@sha256:aa"
        );
        assert_eq!(f("localhost:5000/x:1"), "docker-image://localhost:5000/x:1");
        // Already an identifier: untouched, whatever the scheme.
        assert_eq!(f("docker-image://x/y:1"), "docker-image://x/y:1");
        assert_eq!(f("git://h/r.git#main"), "git://h/r.git#main");
    }

    /// When may a peer's failure be reported as the client's?
    ///
    /// Only when the peer ran what the client asked for. Making a graph
    /// portable rewrites local contexts into images and repoints base images
    /// at our mirror, and this project has produced failures from exactly
    /// that - `no active sessions`, `security.insecure is not allowed`. A
    /// verdict on a rewritten graph could be red where the truth is green,
    /// which is the one direction principle 5 forbids outright.
    #[test]
    fn a_peers_verdict_is_the_clients_only_if_it_ran_the_clients_graph() {
        let t = super::trust_verdict;
        // Off unless asked for. The default has to be the safe one: a
        // needless rebuild costs seconds, a false red costs trust in the
        // whole rig.
        assert!(!t(false, true, false));
        assert!(!t(false, true, true));
        // On, and the graph went out untouched.
        assert!(t(true, true, false));
        // On, but we rewrote it - so its failure is about OUR graph, not the
        // client's.
        assert!(!t(true, false, false));
        // On, untouched, but a cache mount was LIFTED, so the peer ran
        // against a cold one. `go mod download` on a cold cache needs the
        // network, and a blip there is exit code 1 on a build that passes
        // anywhere else. Principle 20 from the other side: seeding a
        // self-keying cache is safe because a wrong entry is never found,
        // and trusting a verdict from a cold one is a different claim.
        assert!(!t(true, true, true));
    }

    /// A build that FAILED is not a peer that could not.
    ///
    /// `+lint-all`: baseline 92s, fleet 628s, and four solves of ~470s each
    /// in a run whose whole single-machine build is a minute and a half. The
    /// `not routed` table named it - the same `golangci-lint ... exit code:
    /// 1` four times over. A deterministic failure was offered to peer after
    /// peer, each ran the whole lint, each failed identically.
    #[test]
    fn a_failed_build_is_a_verdict_and_a_dead_peer_is_not() {
        let v = super::is_build_verdict;

        // buildkit's own wording when the container ran and the command
        // exited non-zero. No other machine can do better with this graph.
        assert!(v(
            "build failed: solve: Unknown error process \"/bin/sh -c golangci-lint run\" \
             did not complete successfully: exit code: 1"
        ));
        assert!(v("did not complete successfully: exit code: 2"));
        assert!(v("did not complete successfully: exit code: 127"));

        // But NOT a signal. 128+N is "something killed the process", and on
        // a build runner the something is nearly always the OOM killer -
        // 137 is SIGKILL, 143 is SIGTERM. A machine with more memory may
        // well succeed, so those stay retryable. This was shipped the other
        // way round an hour ago with 137 asserted as a verdict, which would
        // have turned "this worker ran out of memory" into "your build
        // fails" - the exact confusion the whole predicate exists to
        // prevent, inverted.
        assert!(!v("did not complete successfully: exit code: 137"));
        assert!(!v("did not complete successfully: exit code: 143"));
        assert!(!v("did not complete successfully: exit code: 129"));
        // 128 itself is not a signal death, it is a real exit status.
        assert!(v("did not complete successfully: exit code: 128"));

        // Everything a DIFFERENT machine might survive stays retryable, and
        // the bias is deliberate: retrying a verdict costs time, declining
        // to retry a machine fault costs the build.
        assert!(!v("build failed and daemon died: solve: transport error"));
        assert!(!v("build failed: solve: Unknown error no active sessions"));
        assert!(!v("build failed: solve: Unknown error no such job t7p8h37"));
        assert!(!v("no slots"));
        assert!(!v("wrong platform: wanted linux/arm64"));
        assert!(!v(""));

        // A build whose own OUTPUT quotes the phrase is still a verdict -
        // which is correct here and is the opposite of the `worth_retrying`
        // case, where a test printing "transport error" must not trigger a
        // rebuild. The difference: there the phrase could be anywhere in a
        // relayed message, here buildkit itself frames it with the exit
        // code, and a graph that made a process exit non-zero will do it
        // again wherever it runs.
        assert!(v(
            "build failed: solve: process \"sh -c echo hi\" did not complete \
             successfully: exit code: 2"
        ));

        // The report trims a verdict to its exit code, and the reason is
        // length: buildkit inlines the whole environment into the message,
        // and one of these ran to 700 characters and made `not routed`
        // unreadable. This is the split that does it.
        let long = "build failed: solve: Unknown error process \"/bin/sh -c \
                    GOLANG_VERSION=1.26.5 ... golangci-lint run\" did not complete \
                    successfully: exit code: 1";
        assert_eq!(long.rsplit("exit code:").next().map(str::trim), Some("1"));
    }

    /// The graph that takes a warm mount OUT of a daemon.
    ///
    /// Both halves are checked against buildkit's own
    /// `frontend/gateway/container/container.go`:
    ///
    /// * `MountType_BIND` with `Input == Empty` and an output is
    ///   `makeMutable(m, nil)` - a fresh writable dir that becomes the
    ///   result. That is where the copy lands.
    /// * `MountType_CACHE` is `MountableCache(ctx, m, ref, g)` with `ref`
    ///   being the mount's input, which is the seeding path this harvest
    ///   feeds.
    #[test]
    fn the_harvest_graph_reads_a_cache_and_writes_a_layer() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        // FULLY QUALIFIED, and the short form must be made so.
        //
        // `llb.Image("busybox:1")` normalises before building the op;
        // hand-built LLB does not, and buildkit's
        // `NewImageIdentifier` runs containerd's `reference.Parse` and then
        // insists on `ref.Object`. Given `busybox:1` that parse yields no
        // object and the solve dies as
        //
        //     failed to load cache key: object required
        //
        // which names neither the image nor the field. Every other
        // identifier this crate writes happens to be fully qualified
        // already - mirrored bases are `host/repo@sha256:...` - so the
        // harvest was the first to find it.
        let def = super::harvest_graph("busybox:1", "go-mod", "/go/pkg/mod");
        let ops: Vec<pb::Op> = def
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).unwrap())
            .collect();

        // Source, exec, terminal - and the terminal is LAST, which is the
        // only place loadLLB will accept it.
        // Source, MKDIR, exec, terminal - four now. The mkdir is earthly's
        // cache-mount input reconstructed; see `harvest_graph`.
        assert_eq!(ops.len(), 4);
        let Some(pb::op::Op::Source(src)) = &ops[0].op else {
            panic!("the first op is the base image")
        };
        assert_eq!(
            src.identifier, "docker-image://docker.io/library/busybox:1",
            "a short image name has to be qualified before buildkit sees it"
        );
        // A name that is already qualified is left exactly as written.
        let q = super::harvest_graph("ghcr.io/x/y:v2", "id", "/d");
        let Some(pb::op::Op::Source(s2)) = &pb::Op::decode(q.def[0].as_slice()).unwrap().op else {
            panic!()
        };
        assert_eq!(s2.identifier, "docker-image://ghcr.io/x/y:v2");
        // By POSITION no longer: the mkdir sits between the source and the
        // exec, so index 1 is not the copy any more. Found by kind.
        assert!(
            ops.last().unwrap().op.is_none(),
            "the terminal carries no union and is last"
        );
        let e = ops
            .iter()
            .find_map(|o| match &o.op {
                Some(pb::op::Op::Exec(e)) => Some(e),
                _ => None,
            })
            .expect("the copy");
        let cache = e
            .mounts
            .iter()
            .find(|m| m.mount_type == pb::MountType::Cache as i32)
            .expect("the cache is mounted");
        assert_eq!(cache.cache_opt.as_ref().unwrap().id, "go-mod");
        // IT HAS AN INPUT, and this assertion used to say the opposite -
        // "we are reading it, not seeding it" - which sounds right and cost
        // eight CI attempts. The input does not seed the mount here, it
        // selects WHICH mount: `getRefCacheDir` keys on the id plus the
        // input's ref, so reading earthly's cache means reproducing
        // earthly's input.
        assert_eq!(cache.input, 1, "the mkdir, so the key matches earthly's");
        assert_eq!(cache.output, -1, "a cache mount is not exportable");

        let out = e
            .mounts
            .iter()
            .find(|m| m.output == 0)
            .expect("something has to be the result");
        assert_eq!(out.input, -1, "scratch: buildkit makes it mutable for us");
        assert_eq!(out.mount_type, pb::MountType::Bind as i32);
        assert_eq!(
            out.dest, "/.seed",
            "and index 0 is the seed, not the rootfs"
        );

        // THE ROOTFS IS READONLY AND HAS NO OUTPUT, and both halves are
        // load-bearing. Proven against a real daemon, not reasoned about -
        // three earlier spellings each failed differently.
        //
        // `readonly: true` is what gets the root a MUTABLE ref, backwards as
        // that reads: `PrepareMounts` makes one for a root with no output
        // only when Readonly is set, and leaves it immutable otherwise. With
        // `readonly: false, output: -1` the exec dies as `exit code: 1`
        // before running a line.
        //
        // And no output, because buildkit numbers results by ORDER OF
        // APPEARANCE among mounts that have one, not by the `output` value.
        // With the root at `output: 1` and the seed at `output: 0` the
        // exported layer was the busybox rootfs - 1.9 MB of /bin, plus a
        // 97-byte diff holding `proc/` and `sys/`. Read out of the registry
        // by hand, which is what turned "the seeded mount came up empty"
        // into a five-minute fix.
        let root = e
            .mounts
            .iter()
            .find(|m| m.dest == "/")
            .expect("a rootfs is mounted");
        assert_eq!(root.output, -1, "the root is not the result");
        assert!(root.readonly, "and readonly is what makes it startable");

        // The copy has to name both ends, or this harvests an empty layer
        // and every seeded worker starts exactly as cold as before while the
        // mechanism counts itself as applied.
        let cmd = e.meta.as_ref().unwrap().args.join(" ");
        assert!(cmd.contains(&cache.dest), "reads {}", cache.dest);
        assert!(cmd.contains(&out.dest), "writes {}", out.dest);

        // The dest is where the mount will be SEEDED, not where it is read.
        // Getting these the same way round matters: the layer's root becomes
        // the cache dir's initial contents.
        assert_eq!(cache.dest, "/go/pkg/mod");
    }

    #[test]
    fn cache_seed_pairs_are_read_or_reported() {
        let m = super::parse_cache_seeds(Some("go-mod=reg/a@sha256:1, npm=reg/b@sha256:2"));
        assert_eq!(m.get("go-mod").map(String::as_str), Some("reg/a@sha256:1"));
        assert_eq!(m.get("npm").map(String::as_str), Some("reg/b@sha256:2"));
        assert_eq!(m.len(), 2, "whitespace is not a third pair");
        // A half-written pair takes only itself out. It also prints, which is
        // the part that matters: silently dropping one would leave the run
        // looking like seeding did not pay.
        let m = super::parse_cache_seeds(Some("go-mod=reg/a,broken,=x,y="));
        assert_eq!(m.len(), 1);
        assert!(super::parse_cache_seeds(None).is_empty());
        // One pair per line is what a shell loop writes, and it must parse
        // the same as the comma form - the file is the path that actually
        // gets used, since the harvest happens after this process starts.
        let text = "go-mod=reg/a@sha256:1\nnpm=reg/b@sha256:2\n";
        let m = super::parse_cache_seeds(Some(&text.replace('\n', ",")));
        assert_eq!(m.len(), 2, "newlines separate pairs: {m:?}");
    }

    /// A cold cache mount can be handed a starting point.
    ///
    /// buildkit builds a cache dir as a copy-on-write ref over the mount's
    /// INPUT when the dir does not exist yet - `getRefCacheDirNoCache` calls
    /// `cm.New(ctx, ref, ...)` with exactly that ref. So an input turns "this
    /// worker has never seen go-mod" into "this worker starts from a filled
    /// one", without the Earthfile knowing (principle 15).
    #[test]
    fn a_cold_cache_mount_can_be_seeded_from_an_image() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let base = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "docker-image://golang:1".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let base_d = format!("sha256:{}", crate::store::sha256_hex(&base.encode_to_vec()));
        let mount = |id: &str, dest: &str| pb::Mount {
            input: -1,
            dest: dest.into(),
            output: -1,
            mount_type: pb::MountType::Cache as i32,
            cache_opt: Some(pb::CacheOpt {
                id: id.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let exec = pb::Op {
            inputs: vec![pb::Input {
                digest: base_d.clone(),
                index: 0,
            }],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                mounts: vec![
                    pb::Mount {
                        input: 0,
                        dest: "/".into(),
                        output: 0,
                        ..Default::default()
                    },
                    mount("go-mod", "/go/pkg/mod"),
                    mount("npm", "/root/.npm"),
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let def = pb::Definition {
            def: vec![base.encode_to_vec(), exec.encode_to_vec()],
            ..Default::default()
        };

        let mut seeds = std::collections::BTreeMap::new();
        seeds.insert("go-mod".to_owned(), "reg/seed@sha256:aa".to_owned());
        let out = super::seed_cache_mounts(&def, &seeds);

        // One op added, and it is the seed source.
        assert_eq!(out.def.len(), 3, "the seed image is a new source op");
        let seed_src = out
            .def
            .iter()
            .filter_map(|b| pb::Op::decode(b.as_slice()).ok())
            .find_map(|o| match o.op {
                Some(pb::op::Op::Source(s)) if s.identifier.contains("seed") => Some(s.identifier),
                _ => None,
            });
        assert_eq!(
            seed_src.as_deref(),
            Some("docker-image://reg/seed@sha256:aa")
        );

        let e = out
            .def
            .iter()
            .filter_map(|b| pb::Op::decode(b.as_slice()).ok())
            .find_map(|o| match o.op {
                Some(pb::op::Op::Exec(e)) => Some((o.inputs, e)),
                _ => None,
            })
            .expect("the exec survived");
        let (inputs, e) = e;
        let go = &e.mounts[1];
        assert!(go.input >= 0, "go-mod now starts from something");
        assert_eq!(
            inputs[go.input as usize].digest,
            format!(
                "sha256:{}",
                crate::store::sha256_hex(
                    &pb::Op {
                        op: Some(pb::op::Op::Source(pb::SourceOp {
                            identifier: "docker-image://reg/seed@sha256:aa".into(),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }
                    .encode_to_vec()
                )
            ),
            "and that something is the seed"
        );
        // The UNSEEDED mount is untouched: seeding every mount would make a
        // worker pull images for caches nobody measured.
        assert_eq!(e.mounts[2].input, -1, "npm was not asked for");
        // And the rootfs mount still points at the base, not at the seed -
        // appending an input must not renumber the existing ones.
        assert_eq!(e.mounts[0].input, 0);
        assert_eq!(inputs[0].digest, base_d);

        // A MOUNT THAT ALREADY HAS AN INPUT still gets seeded, by replacing
        // it. This is not an edge case: EVERY earthly cache mount has one -
        // `AddMount(target, cacheContext.File(Mkdir("/cache")), ...)` - so
        // the original `input < 0` filter meant the transform could never
        // apply to a single real graph. It reported `seeds=4/4` resolved and
        // rewrote nothing.
        let existing = pb::Op {
            inputs: vec![
                pb::Input {
                    digest: "sha256:base".into(),
                    index: 0,
                },
                pb::Input {
                    digest: "sha256:someref".into(),
                    index: 0,
                },
            ],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                mounts: vec![
                    pb::Mount {
                        input: 0,
                        dest: "/".into(),
                        output: 0,
                        ..Default::default()
                    },
                    pb::Mount {
                        input: 1,
                        selector: "/cache".into(),
                        dest: "/go/pkg/mod".into(),
                        output: -1,
                        mount_type: pb::MountType::Cache as i32,
                        cache_opt: Some(pb::CacheOpt {
                            id: "go-mod".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let with_input = pb::Definition {
            def: vec![existing.encode_to_vec()],
            ..Default::default()
        };
        let mut gm = std::collections::BTreeMap::new();
        gm.insert("go-mod".to_owned(), "reg/seed@sha256:bb".to_owned());
        let out = super::seed_cache_mounts(&with_input, &gm);
        assert_ne!(
            out.def, with_input.def,
            "a mount with an input is still seeded"
        );
        let e = out
            .def
            .iter()
            .filter_map(|b| pb::Op::decode(b.as_slice()).ok())
            .find_map(|o| match o.op {
                Some(pb::op::Op::Exec(e)) => Some((o.inputs, e)),
                _ => None,
            })
            .expect("the exec");
        let (inputs, e) = e;
        let m = &e.mounts[1];
        assert!(
            inputs[m.input as usize].digest.contains("seed") || m.input == 2,
            "the cache mount points at the seed now, not at the old ref"
        );
        // AND THE SELECTOR GOES. It named a path inside the OLD input; the
        // seed image's root is the cache contents, so keeping `/cache` would
        // select a directory the seed does not have.
        assert_eq!(m.selector, "", "the old input's selector does not apply");

        // A BARE DIGEST is refused rather than wrapped. `sha256:abc` is
        // what `build_subtree` answers with - content, no location - and
        // `docker-image://sha256:abc` parses nowhere. The seeding path built
        // exactly that for one commit, and it would have failed quietly:
        // the resolve check drops a seed it cannot read, so the run reports
        // that seeding did not pay for a mechanism that never addressed
        // anything. `solve::pullable` is what turns one into the other.
        let mut bare = std::collections::BTreeMap::new();
        bare.insert("go-mod".to_owned(), "sha256:deadbeef".to_owned());
        let out = super::seed_cache_mounts(&def, &bare);
        assert_eq!(out.def, def.def, "a bare digest seeds nothing");

        // Nothing to seed is the identity, byte for byte. A transform that
        // rewrites a graph it had no reason to touch changes every digest
        // below it for nothing.
        let same = super::seed_cache_mounts(&def, &Default::default());
        assert_eq!(same.def, def.def);

        // THE TERMINAL IS STILL LAST, on a graph that has one. `loadLLB`
        // deletes exactly the final entry and hands anything else
        // union-less to `ResolveOp`, whose default arm reports
        // `no support for <nil>` - a day went on that message once, and this
        // transform PREPENDS ops, which is the operation most likely to do
        // it again. Proven by appending instead: it fails, here.
        let with_terminal = pb::Definition {
            def: {
                let mut d = def.def.clone();
                let exec_d = format!("sha256:{}", crate::store::sha256_hex(&d[1]));
                d.push(
                    pb::Op {
                        inputs: vec![pb::Input {
                            digest: exec_d,
                            index: 0,
                        }],
                        ..Default::default()
                    }
                    .encode_to_vec(),
                );
                d
            },
            ..Default::default()
        };
        let out = super::seed_cache_mounts(&with_terminal, &seeds);
        let last = pb::Op::decode(out.def.last().unwrap().as_slice()).unwrap();
        assert!(last.op.is_none(), "the terminal must still be last");
        assert_eq!(
            out.def
                .iter()
                .filter(|b| pb::Op::decode(b.as_slice()).is_ok_and(|o| o.op.is_none()))
                .count(),
            1,
            "and there must be exactly one of it"
        );
    }

    /// A warm cache MOUNT outranks a warm op, and by how much.
    ///
    /// The two are not the same size. Reusing an op saves whatever that op
    /// cost; meeting a warm `go-mod` saves the ~24s that 64 leads in one
    /// measured run each spent behind a cold one, against a p50 lead of
    /// 8.6s. So one warm mount has to beat any plausible op overlap inside a
    /// single subtree, and subtrees here run to tens of ops.
    #[test]
    fn cache_warmth_outranks_op_warmth() {
        // Ten shared ops loses to one shared cache mount.
        assert!(super::warmth(10, 0) < super::warmth(0, 1));
        // And to be sure it is not merely ordered: it still loses at fifty.
        assert!(super::warmth(50, 0) < super::warmth(0, 1));
        // Op warmth still breaks ties between peers with the same mounts.
        assert!(super::warmth(3, 1) > super::warmth(2, 1));
        // Two mounts beat one, whatever the ops say.
        assert!(super::warmth(0, 2) > super::warmth(60, 1));
        assert_eq!(super::warmth(0, 0), 0);
    }

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
    fn work_goes_where_its_ancestry_already_is() {
        // Least-loaded-first is load balancing, and load balancing is the
        // WRONG default here: it spreads work that shares ancestry across
        // machines, and every machine that touches a layer has to pull,
        // decompress and unpack it into its own snapshotter. Measured on a
        // full +test-no-qemu: `op duplication 2.3x built` - every op
        // materialised on 2.3 machines on average, against an ideal of 1.
        //
        // So prefer a peer that already has the ancestry, and only among
        // peers that could take the work anyway. Affinity subject to
        // capacity, never instead of it.
        let load = |slots, driver, peer| Load {
            slots,
            driver,
            peer,
        };
        let cand = |id, l| Candidate {
            id,
            platform: "linux/arm64".to_owned(),
            load: l,
        };
        let v = ok_verdict();
        let cands = [
            cand(1, load(4, 0, 0)),
            cand(2, load(4, 0, 0)),
            cand(3, load(4, 0, 0)),
        ];

        // All three idle: without affinity, id order. With it, the peer that
        // already built some of this goes first.
        assert_eq!(
            offer_order(&v, &cands, Allow::default()),
            vec![1, 2, 3],
            "unchanged when nothing is warm"
        );
        let warm = |id: u64| if id == 3 { 7 } else { 0 };
        assert_eq!(
            offer_order_warm(&v, &cands, Allow::default(), &warm),
            vec![3, 1, 2],
            "the peer holding the ancestry is asked first"
        );

        // Capacity still wins over warmth when the warm peer is FULL: a peer
        // that would decline is a wasted round trip however warm it is.
        let full = [
            cand(1, load(4, 0, 0)),
            cand(2, load(4, 4, 0)), // saturated
        ];
        assert_eq!(
            offer_order_warm(&v, &full, Allow::default(), &|id| if id == 2 {
                99
            } else {
                0
            }),
            vec![1],
            "a saturated peer is not offered to, warm or not"
        );

        // And among equally warm peers the emptiest still goes first, so
        // affinity refines the old order rather than replacing it.
        let mixed = [cand(1, load(4, 3, 0)), cand(2, load(4, 1, 0))];
        assert_eq!(
            offer_order_warm(&v, &mixed, Allow::default(), &|_| 5),
            vec![2, 1],
            "equal warmth falls back to emptiest first"
        );
    }

    /// What must be true of ANY graph this code hands to buildkit.
    ///
    /// Written after the fourth structural bug, because all four broke the
    /// same handful of rules and each was caught by a different accident:
    ///
    ///   - the terminal stopped being last, so buildkit resolved an op with
    ///     no union and said `no support for <nil>`
    ///   - a graft left orphans, which inflated `def.len()` and let a
    ///     terminal-rooted cut through the cut-prefix picker
    ///   - a re-encode of an UNCHANGED op dropped fields earthly's fork adds
    ///     to the proto
    ///   - metadata kept keys for ops that no longer existed
    ///
    /// Each rewrite is checked against all of them rather than against the
    /// one that broke it.
    fn assert_well_formed(before: &pb::Definition, after: &pb::Definition, what: &str) {
        let dig = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let ops: Vec<pb::Op> = after
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).unwrap_or_else(|e| panic!("{what}: {e}")))
            .collect();
        assert!(!ops.is_empty(), "{what}: empty definition");

        // ONE terminal, and it is last.
        let (last, rest) = ops.split_last().expect("non-empty");
        assert!(last.op.is_none(), "{what}: last op is not a terminal");
        assert!(
            rest.iter().all(|o| o.op.is_some()),
            "{what}: a second op has no union - buildkit resolves it as a vertex"
        );

        // Every input resolves, and nothing is unreachable.
        let by_digest: std::collections::BTreeSet<String> =
            after.def.iter().map(|b| dig(b)).collect();
        for (i, op) in ops.iter().enumerate() {
            for input in &op.inputs {
                assert!(
                    by_digest.contains(&input.digest),
                    "{what}: op {i} points at a digest that is not in the graph"
                );
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = vec![after.def.len() - 1];
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            for input in &ops[i].inputs {
                if let Some(j) = after.def.iter().position(|b| dig(b) == input.digest) {
                    stack.push(j);
                }
            }
        }
        assert_eq!(
            seen.len(),
            after.def.len(),
            "{what}: {} orphan(s) the terminal cannot reach",
            after.def.len() - seen.len()
        );

        // Metadata describes ops that exist.
        for k in after.metadata.keys() {
            assert!(
                by_digest.contains(k),
                "{what}: metadata for a pruned op {k}"
            );
        }

        // Anything carried over unchanged is carried over BYTE for byte.
        let kept: Vec<&Vec<u8>> = after
            .def
            .iter()
            .filter(|b| before.def.contains(b))
            .collect();
        for b in kept {
            assert!(
                before.def.contains(b),
                "{what}: an op claims to be unchanged but is not"
            );
        }
    }

    #[test]
    fn every_rewrite_leaves_a_graph_buildkit_can_load() {
        let exec = |env: Vec<&str>| pb::Op {
            op: Some(OpKind::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    args: vec!["/bin/sh".into()],
                    env: env.into_iter().map(String::from).collect(),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        };
        let d = chain(vec![
            (src("docker-image://docker.io/library/alpine:3.20"), vec![]),
            (src("git://github.com/example/repo.git#main"), vec![]),
            (
                exec(vec!["EARTHLY_BUILDKIT_HOST=tcp://10.0.0.1:1234"]),
                vec![0, 1],
            ),
            (plain(), vec![2]),
            (pb::Op::default(), vec![3]),
        ]);
        let mid = format!("sha256:{}", crate::store::sha256_hex(&d.def[2]));

        // Each rewrite in turn, and then the compositions that actually run
        // in the proxy - the terminal-as-vertex bug only appeared when
        // grafting and cutting were both on.
        assert_well_formed(
            &d,
            &retarget_buildkit_host(&d, "tcp://10.0.0.9:8372"),
            "retarget",
        );
        assert_well_formed(&d, &rewrite_git_sources(&d, &|_| None), "git (no-op)");
        assert_well_formed(
            &d,
            &rewrite_git_sources(&d, &|_| Some("docker-image://reg/m@sha256:aa".into())),
            "git (mirrored)",
        );
        let grafted = graft_built(&d, &|dg| {
            (dg == mid).then(|| "docker-image://reg/x@sha256:beef".to_owned())
        });
        assert_well_formed(&d, &grafted, "graft");
        assert_well_formed(
            &grafted,
            &retarget_buildkit_host(&grafted, "tcp://10.0.0.9:8372"),
            "graft then retarget",
        );
        if let Some(cut) = subgraph(&d, 3) {
            assert_well_formed(&d, &cut, "subgraph");
        }

        // A rewrite with nothing to do must be byte-identical, or every
        // solve it touches becomes a whole-build cache miss.
        assert_eq!(
            retarget_buildkit_host(&d, "tcp://10.0.0.1:1234").def,
            d.def,
            "already pointing there: no change"
        );
        assert_eq!(
            graft_built(&d, &|_| None).def,
            d.def,
            "nothing built: no change"
        );
    }

    #[test]
    fn a_nested_build_is_pointed_at_the_daemon_running_it() {
        // The funnel: earthly forwards its own BUILDKIT_HOST into every RUN,
        // so a nested earthly on ANY worker dials the coordinator. The five
        // leads owning a full +test-no-qemu critical path are nested builds
        // at 231-258s, each longer than the whole single-machine build.
        //
        // A constant cannot fix it. `tcp://buildkitsandbox:8372` resolves
        // locally for most execs and NOT for `--privileged --entrypoint`
        // ones, where the nested earthly reports
        // `could not connect to buildkit: timeout 1m0s` and the test dies.
        //
        // But the address does not have to be chosen where the graph is
        // built. The WORKER knows how its own daemon is reached, and it can
        // substitute that when the subtree arrives.
        let exec = |env: Vec<&str>| pb::Op {
            op: Some(OpKind::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    args: vec!["/bin/sh".into()],
                    env: env.into_iter().map(String::from).collect(),
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        };
        let d = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (
                exec(vec![
                    "PATH=/bin",
                    "EARTHLY_BUILDKIT_HOST=tcp://10.0.0.1:1234",
                ]),
                vec![0],
            ),
            (exec(vec!["PATH=/bin"]), vec![1]),
            (pb::Op::default(), vec![2]),
        ]);

        let out = retarget_buildkit_host(&d, "tcp://10.0.0.9:8372");
        let ops: Vec<pb::Op> = out
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).expect("decodes"))
            .collect();
        let env_of = |i: usize| match &ops[i].op {
            Some(OpKind::Exec(e)) => e.meta.as_ref().expect("meta").env.clone(),
            _ => vec![],
        };
        assert_eq!(
            env_of(1),
            vec!["PATH=/bin", "EARTHLY_BUILDKIT_HOST=tcp://10.0.0.9:8372"],
            "the forwarded address becomes this machine's daemon"
        );
        assert_eq!(env_of(2), vec!["PATH=/bin"], "and nothing else is touched");

        // The op that changed has a new digest, so its consumer must follow
        // it - a graph that dangles here is a build that cannot load.
        let new1 = format!("sha256:{}", crate::store::sha256_hex(&out.def[1]));
        assert_eq!(ops[2].inputs[0].digest, new1, "the consumer follows");
        assert!(ops.last().expect("non-empty").op.is_none(), "terminal last");

        // An UNCHANGED op must travel byte for byte. Re-encoding through our
        // types drops whatever earthly's fork added to the proto, and that
        // has already cost two days once.
        assert_eq!(out.def[0], d.def[0], "untouched ops are verbatim");

        // Nothing to do is nothing done - a graph with no forwarded host
        // must come back byte-identical, or every solve becomes a cache miss.
        let plain_d = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (exec(vec!["PATH=/bin"]), vec![0]),
            (pb::Op::default(), vec![1]),
        ]);
        assert_eq!(
            retarget_buildkit_host(&plain_d, "tcp://10.0.0.9:8372").def,
            plain_d.def,
            "no forwarded host, no rewrite"
        );

        // Both spellings: earthly reads EARTH_ first and falls back to the
        // deprecated EARTHLY_, and the entrypoint reads bare BUILDKIT_HOST.
        for name in ["BUILDKIT_HOST", "EARTH_BUILDKIT_HOST"] {
            let d = chain(vec![
                (src("docker-image://alpine:3.20"), vec![]),
                (exec(vec![&format!("{name}=tcp://10.0.0.1:1234")]), vec![0]),
                (pb::Op::default(), vec![1]),
            ]);
            let out = retarget_buildkit_host(&d, "tcp://10.0.0.9:8372");
            let got = pb::Op::decode(out.def[1].as_slice()).expect("decodes");
            let env = match got.op {
                Some(OpKind::Exec(e)) => e.meta.expect("meta").env,
                _ => vec![],
            };
            assert_eq!(env, vec![format!("{name}=tcp://10.0.0.9:8372")], "{name}");
        }

        // An EMPTY value is a deliberate opt-out - `force_internal_buildkit`
        // unsets these so the nested build stands up its own daemon, and
        // filling it back in would silently undo that.
        let optout = chain(vec![
            (src("docker-image://alpine:3.20"), vec![]),
            (exec(vec!["BUILDKIT_HOST="]), vec![0]),
            (pb::Op::default(), vec![1]),
        ]);
        assert_eq!(
            retarget_buildkit_host(&optout, "tcp://10.0.0.9:8372").def,
            optout.def,
            "an emptied host stays empty"
        );
    }

    #[test]
    fn a_subtree_can_say_what_it_is() {
        // Leads are logged by the digest of what they produced, which names
        // nothing: `built at sha256:8793cb6b... in 209141ms` is 40% of a
        // wall clock and no clue which target owns it. buildkit carries
        // `llb.customname` in op metadata - it is what earthly prints as
        // `+base | --> FROM ...` - so the graph can say.
        let mut d = chain(vec![
            (src("docker-image://docker.io/library/alpine:3.20"), vec![]),
            (plain(), vec![0]),
        ]);
        let root = format!("sha256:{}", crate::store::sha256_hex(&d.def[1]));
        d.metadata.insert(
            root,
            pb::OpMetadata {
                description: [("llb.customname".to_owned(), "+base RUN apk add".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        assert_eq!(describe(&d, 1).as_deref(), Some("+base RUN apk add"));

        // earthly does not write a plain string. It writes
        // `[<base64 VertexMeta JSON>] <human tail>` - the target name lives
        // only inside the base64, and the readable half is AFTER the
        // bracket, so taking what is between the brackets yields a wall of
        // base64 and drops the useful part. Which is what the first version
        // of the CI panel printed.
        let meta = r#"{"tnm":"./tests+ga-no-qemu-group7","sl":{"file":"tests/Earthfile","startLine":1323}}"#;
        let encoded = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(meta)
        };
        let earthly_name = format!("[{encoded}] RUN --privileged /bin/sh");
        d.metadata.insert(
            format!("sha256:{}", crate::store::sha256_hex(&d.def[0])),
            pb::OpMetadata {
                description: [("llb.customname".to_owned(), earthly_name)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        assert_eq!(
            describe(&d, 0).as_deref(),
            Some("./tests+ga-no-qemu-group7 tests/Earthfile:1323 RUN --privileged /bin/sh"),
        );

        // Undecodable brackets are left alone rather than dropped: a name we
        // cannot parse is still better than no name.
        d.metadata.insert(
            format!("sha256:{}", crate::store::sha256_hex(&d.def[0])),
            pb::OpMetadata {
                description: [(
                    "llb.customname".to_owned(),
                    "[not-base64!] FROM x".to_owned(),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );
        assert_eq!(describe(&d, 0).as_deref(), Some("[not-base64!] FROM x"));

        // An index that is not there is not a panic.
        assert_eq!(describe(&d, 99), None);

        // A whole subtree is named by the op its terminal points at, never
        // by the terminal, which is a pointer rather than work.
        let cut = subgraph(&d, 1).expect("a cut at the RUN");
        assert_eq!(describe_root(&cut).as_deref(), Some("+base RUN apk add"));
        assert_eq!(describe_root(&pb::Definition::default()), None);
    }

    #[test]
    fn grafting_leaves_no_orphans_behind() {
        // Replacing a subtree with the image it built makes every op BELOW
        // the replaced root unreachable - and they stayed in `def`, which is
        // how `no support for <nil>` got in. The cut-prefix picker excludes
        // "the whole graph" by comparing a cut's op count against
        // `def.len()`, and orphans make that comparison lie: a cut rooted at
        // the TERMINAL counts fewer ops than the definition holds, passes the
        // filter, and gets dispatched. Which is also why the error only ever
        // appeared with grafting on.
        let d = chain(vec![
            (src("docker-image://docker.io/library/alpine:3.20"), vec![]),
            (plain(), vec![0]), // gets replaced
            (plain(), vec![1]),
            (pb::Op::default(), vec![2]),
        ]);
        let target = format!("sha256:{}", crate::store::sha256_hex(&d.def[1]));
        let g = graft_built(&d, &|dgst| {
            (dgst == target).then(|| "docker-image://reg/built@sha256:abc".to_owned())
        });

        // op 0 is now an orphan: nothing reaches it once op 1 is a source.
        assert_eq!(g.def.len(), 3, "the orphan is gone");
        let ops: Vec<pb::Op> = g
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).expect("decodes"))
            .collect();
        assert!(ops.last().expect("non-empty").op.is_none(), "terminal last");
        // Metadata must follow: an entry keyed by a digest no longer in the
        // definition is dead weight the graph carries to every worker.
        for k in g.metadata.keys() {
            assert!(
                g.def
                    .iter()
                    .any(|b| &format!("sha256:{}", crate::store::sha256_hex(b)) == k),
                "metadata for a pruned op: {k}"
            );
        }
    }

    #[test]
    fn a_cut_never_makes_a_terminal_into_a_vertex() {
        // The bug this pins, and it took three wrong theories to find:
        //
        //     failed to load cache key: no support for <nil>
        //
        // is `worker/base/worker.go`'s ResolveOp falling through its switch
        // on the op union - `%T` of a nil interface prints `<nil>`. So an op
        // with NO union reached the solver as a vertex, and the only op
        // without one is the terminal.
        //
        // `loadLLB` deletes exactly one terminal: the LAST entry in `def`.
        // Anything else reachable from it must be a real op. Cutting at the
        // terminal produces a graph with two - the synthetic one at the end,
        // and the original one it now points at - and buildkit tries to run
        // the original.
        let d = chain(vec![
            (src("docker-image://docker.io/library/alpine:3.20"), vec![]),
            (plain(), vec![0]),
            // The terminal, as buildkit marshals it: inputs, no op.
            (pb::Op::default(), vec![1]),
        ]);
        assert!(
            subgraph(&d, 2).is_none(),
            "the terminal is not a buildable root"
        );

        // A cut at a real op still works, and its own terminal is last.
        let cut = subgraph(&d, 1).expect("a cut at the RUN");
        let ops: Vec<pb::Op> = cut
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).expect("decodes"))
            .collect();
        let (last, rest) = ops.split_last().expect("non-empty");
        assert!(last.op.is_none(), "the last op is the terminal");
        assert!(
            rest.iter().all(|o| o.op.is_some()),
            "every other op has a union, or the solver hits its default arm"
        );
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
    fn a_git_source_must_not_be_refused_before_the_mirror_runs() {
        // The bug this encodes shipped: git mirroring was written, tested,
        // committed, and fired ZERO times on a target with 407 git-grounded
        // solves. `inspect` refused the graph before `make_portable` was
        // ever called, so the code that would have fixed it never ran.
        //
        // The stale assumption was written down one line above the check:
        // "hazards live on ExecOps and rewriting only touches source
        // identifiers, so the original graph gives the same verdict". True
        // until source hazards existed.
        let mut g = plain();
        g.op = Some(OpKind::Source(pb::SourceOp {
            identifier: "git://github.com/example/repo.git#main".to_owned(),
            ..Default::default()
        }));
        let v = inspect(&def(vec![g]));

        assert!(
            !v.dispatchable_when(Allow::default()),
            "strictly, a raw git source is not dispatchable"
        );
        assert!(
            v.dispatchable_once_mirrored(Allow::default()),
            "but it is worth making portable, which is a different question"
        );

        // A hazard the driver CANNOT fix is still refused by both. Mirroring
        // republishes content; it does not grant privileges.
        let mut i = plain();
        i.op = Some(OpKind::Exec(pb::ExecOp {
            security: pb::SecurityMode::Insecure as i32,
            ..exec_of(&plain())
        }));
        let iv = inspect(&def(vec![i]));
        assert!(
            !iv.dispatchable_once_mirrored(Allow::default()),
            "no amount of republishing makes a peer's --privileged safe"
        );
    }

    #[test]
    fn the_rewriter_hands_over_a_name_with_no_scheme() {
        // The CONTRACT, pinned, because breaking it is silent and has now
        // cost two bugs: a mirror keyed by the full `git://...` identifier
        // was looked up by the scheme-less name, never matched, and 397
        // solves stayed unportable while the mirror reported success.
        //
        // `make_portable` COLLECTS full identifiers and RESOLVES with
        // whatever this passes. If the two ever disagree the failure is a
        // rewrite that quietly does nothing - there is no error anywhere,
        // because both halves worked.
        let mut g = plain();
        g.op = Some(OpKind::Source(pb::SourceOp {
            identifier: "git://example.com/r.git#main".to_owned(),
            ..Default::default()
        }));
        let seen = std::cell::RefCell::new(Vec::new());
        rewrite_git_sources(&def(vec![g]), &|name| {
            seen.borrow_mut().push(name.to_owned());
            None
        });
        assert_eq!(
            seen.into_inner(),
            vec!["example.com/r.git#main".to_owned()],
            "the replacement is called WITHOUT the scheme"
        );
    }

    #[test]
    fn an_op_read_at_a_nonzero_index_must_not_be_grafted() {
        // An image source has ONE output. An ExecOp has one per mount, so a
        // consumer may read index 1, 2, ... - and grafting that op leaves
        // those inputs pointing at an output that does not exist.
        //
        // buildkit reports it as `failed to load cache key: no support for
        // <nil>`, three steps from the cause and naming neither the op nor
        // the index. Measured on six machines: every worker declined and the
        // fleet leg failed.
        let base = plain();
        let base_b = base.encode_to_vec();
        let base_d = format!("sha256:{}", crate::store::sha256_hex(&base_b));
        // Reads the base's SECOND output - a second mount, say.
        let mut consumer = plain();
        consumer.inputs = vec![pb::Input {
            digest: base_d.clone(),
            index: 1,
        }];
        let d = pb::Definition {
            def: vec![base_b, consumer.encode_to_vec()],
            ..Default::default()
        };

        let out = graft_built(&d, &|dg| {
            (dg == base_d).then(|| "docker-image://reg/x@sha256:beef".to_owned())
        });
        assert_eq!(
            out.def, d.def,
            "an op read at index 1 was grafted into a single-output source"
        );
    }

    #[test]
    fn a_cut_becomes_a_definition_a_peer_can_solve() {
        // The dispatch unit that was missing. `analyse` has found cuts since
        // the beginning and used them for a log line; nothing built one. So
        // the shared prefix stayed interior to every dispatched graph, never
        // got published, and could never be grafted - which is why eight
        // measured attempts to stop workers rebuilding it all failed the
        // same way.
        let base = plain();
        let base_d = format!("sha256:{}", crate::store::sha256_hex(&base.encode_to_vec()));
        let mut mid = plain();
        mid.inputs = vec![pb::Input {
            digest: base_d,
            index: 0,
        }];
        let mid_b = mid.encode_to_vec();
        let mid_d = format!("sha256:{}", crate::store::sha256_hex(&mid_b));
        // A sibling that must NOT come along: it is not an ancestor of mid.
        let mut other = plain();
        other.inputs = vec![pb::Input {
            digest: mid_d.clone(),
            index: 0,
        }];
        let d = pb::Definition {
            def: vec![base.encode_to_vec(), mid_b, other.encode_to_vec()],
            ..Default::default()
        };

        let cut = subgraph(&d, 1).expect("a two-op cut is worth dispatching");
        // base + mid + terminal, and NOT the consumer above it.
        assert_eq!(cut.def.len(), 3, "took the wrong ops: {}", cut.def.len());
        let term = pb::Op::decode(cut.def.last().unwrap().as_slice()).unwrap();
        assert!(term.op.is_none(), "the terminal carries no op");
        assert_eq!(
            term.inputs[0].digest, mid_d,
            "the terminal must point at the cut's root, or the peer builds \
             something else"
        );

        // A single op is not worth a publish.
        assert!(subgraph(&d, 0).is_none(), "a one-op cut must be refused");
    }

    #[test]
    fn a_built_ancestor_becomes_a_pull_not_a_rebuild() {
        // The step from N prefixes to 1.
        //
        // Measured: three workers spend 675s of lead-work doing what one
        // machine does in 144s, because each dispatched subtree carries its
        // whole ancestor chain and buildkit dedupes only within one daemon.
        // Grafting turns that chain into an image the worker pulls.
        let base = plain();
        let base_d = format!("sha256:{}", crate::store::sha256_hex(&base.encode_to_vec()));
        let mut mid = plain();
        mid.inputs = vec![pb::Input {
            digest: base_d.clone(),
            index: 0,
        }];
        let mid_b = mid.encode_to_vec();
        let mid_d = format!("sha256:{}", crate::store::sha256_hex(&mid_b));
        let mut top = plain();
        top.inputs = vec![pb::Input {
            digest: mid_d.clone(),
            index: 0,
        }];
        let d = pb::Definition {
            def: vec![base.encode_to_vec(), mid_b, top.encode_to_vec()],
            ..Default::default()
        };

        // Nothing built yet: byte-identical, because reserialising an
        // unchanged graph changes every digest and buildkit reads that as a
        // whole-build cache miss.
        assert_eq!(
            graft_built(&d, &|_| None).def,
            d.def,
            "no-op must not touch"
        );

        // `mid` has been built and published. It should become a source, and
        // `top` should point at the NEW digest.
        let out = graft_built(&d, &|dg| {
            (dg == mid_d).then(|| "docker-image://reg/x@sha256:beef".to_owned())
        });
        let ops: Vec<pb::Op> = out
            .def
            .iter()
            .map(|b| pb::Op::decode(b.as_slice()).unwrap())
            .collect();
        // BY KIND, not by index: grafting prunes what the replaced op used to
        // depend on, so positions shift by however much ancestry died.
        let (at, grafted) = ops
            .iter()
            .enumerate()
            .find(|(_, o)| matches!(&o.op, Some(OpKind::Source(s)) if s.identifier.ends_with("@sha256:beef")))
            .expect("the built op must become an import");
        assert!(
            grafted.inputs.is_empty(),
            "a grafted op keeps no inputs - the ancestry stops being work"
        );
        let new_mid = format!("sha256:{}", crate::store::sha256_hex(&out.def[at]));
        assert_eq!(
            ops[at + 1].inputs[0].digest,
            new_mid,
            "the consumer must follow the graft, or the graph dangles"
        );
        // And the ancestry really is gone, not merely bypassed.
        assert_eq!(out.def.len(), d.def.len() - 1, "the base op was pruned");
    }

    #[test]
    fn a_mirrored_git_source_stops_being_a_hazard() {
        // The point of mirroring: the driver fetches with its session, and
        // what the peer sees is an ordinary image. If this rewrite does not
        // clear the exclusion it has bought nothing.
        let mut op = plain();
        op.op = Some(OpKind::Source(pb::SourceOp {
            identifier: "git://github.com/example/repo.git#main".to_owned(),
            ..Default::default()
        }));
        let before = def(vec![op]);
        assert!(!inspect(&before).dispatchable(), "precondition");

        let after = rewrite_git_sources(&before, &|_| {
            Some("docker-image://reg:5000/rebuck2/base:deadbeef".to_owned())
        });
        assert!(
            inspect(&after).dispatchable(),
            "mirroring a git source must clear it: {:?}",
            inspect(&after).exclusions
        );

        // An unmirrored one stays put rather than being dropped. Pointing a
        // peer at content nobody published is worse than not dispatching.
        let untouched = rewrite_git_sources(&before, &|_| None);
        assert!(
            !inspect(&untouched).dispatchable(),
            "a git source we could not mirror must still ground the subtree"
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
