//! Asking a buildkitd to build a subtree someone else offered us.
//!
//! Two halves, deliberately separated. Building the [`SolveRequest`] is pure
//! and is where the mistakes live — a wrong exporter sends the result to the
//! wrong place, a stray entitlement grants a privilege we refused to
//! dispatch. Talking to the daemon is I/O and cannot be honestly unit-tested;
//! it wants the e2e rig and a live buildkitd.
//!
//! **No session is needed**, which is a measured finding rather than an
//! assumption: buildkit accepts a solve with none when the build has no
//! local sources and needs no registry auth. A session exists to carry
//! filesync and credentials back to the daemon, and a dispatched subtree
//! has neither - its inputs are digests. That removes the largest piece of
//! machinery this was expected to need.
//!
//! The result is exported by PUSHING to this worker's own loopback registry
//! (`crate::registry`). That is principle 6 as a mechanism: the layers land
//! where a peer can fetch them directly, and the driver — which arbitrates
//! the offer — carries none of them. The test for it is deliberately blunt:
//! after the build, look at the driver's disk.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bollard_buildkit_proto::moby::buildkit::v1 as control;
use bollard_buildkit_proto::pb;

/// Monotonic within a process; paired with the pid it makes a solve ref
/// that cannot repeat. See [`solve_request`] for why that matters.
static SOLVE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Where a built subtree is published, and how a peer names it.
///
/// One repo per job: two subtrees building concurrently on one worker must
/// not collide on a tag, and a peer fetching the result asks for exactly
/// this string.
/// Where a built subtree lands. One repo, so a digest reference needs only
/// the registry that is serving it, not the job that made it.
pub const SUBTREE_REPO: &str = "rebuck2/subtree";

pub fn result_ref(registry: &str, job: u64) -> String {
    format!("{registry}/{SUBTREE_REPO}:job-{job}")
}

/// A published digest, as a reference a daemon can actually pull.
///
/// `build_subtree` answers with a BARE `sha256:...` on purpose - a digest
/// names content and not a location, so whoever ends up holding it can serve
/// it, and that is what let results travel between machines at all. Every
/// consumer then has to prefix the registry IT will pull from, and each one
/// had grown its own copy of that line.
///
/// The copy that did not exist was the one the cache-mount seed needed.
/// `harvest-cache` printed the bare digest, `seed_cache_mounts` wrapped it as
/// `docker-image://sha256:...`, and no daemon can parse that. It would have
/// failed safe - the resolve check drops a seed it cannot read - so the run
/// would have reported "seeding did not pay" for a mechanism that never
/// addressed anything.
/// [`pullable`], as an LLB source identifier.
///
/// An LLB source is a URL and buildkit rejects one without a scheme -
/// `failed to parse ... invalid`. Three copies of this rule lived in
/// proxy.rs and only two of them said so: the third handed a full unschemed
/// reference straight through, which is the exact failure the comment beside
/// it warned about.
pub fn llb_source(registry: &str, reference: &str) -> String {
    let r = pullable(registry, reference);
    if r.contains("://") {
        r
    } else {
        format!("docker-image://{r}")
    }
}

pub fn pullable(registry: &str, reference: &str) -> String {
    match reference.strip_prefix("sha256:") {
        Some(d) => format!("{registry}/{SUBTREE_REPO}@sha256:{d}"),
        // Already a full reference, from an older worker or an operator
        // naming an image they published themselves.
        None => reference.to_owned(),
    }
}

/// Exporter attrs for everything this crate pushes to the mirror - adopted
/// results, mirrored bases, published contexts, driver subtrees.
///
/// The two timestamp attrs are what make republishing an unchanged input
/// land on the bytes already there. Without them the tag moves on every
/// publish and everything it used to name is garbage from that instant.
///
/// Measured, 24 adoptions of 4 distinct graphs into one store:
///
/// | attrs | blobs | orphaned |
/// | ------------------------------ | ----- | -------- |
/// | neither | 75 | 60 |
/// | `source-date-epoch` only | 75 | 60 |
/// | both | 15 | 0 |
///
/// `source-date-epoch` alone buys NOTHING, which is the counter-intuitive
/// part. It does fix the config's `created` field - verified, it reads
/// 1970-01-01 - but the config also carries the layer diffIDs, and the layer
/// tar still holds real mtimes. So the chain runs layer -> diffID -> config
/// -> manifest -> tag, and the timestamp in the config was a passenger.
/// `rewrite-timestamp` is what settles the layer, and it needs
/// `source-date-epoch` to know what to write.
///
/// This rewrites mtimes inside published layers, so it is worth being
/// explicit that it does NOT change what a client gets: output digests were
/// identical across all three variants above. The mirror is a transport for
/// a result the client computes for itself.
/// How to compress the layers this fleet exports.
///
/// Unpack is the bottleneck, measured: 87% of lead time in one run was in
/// the seventeen leads that fetched over a MiB, at 7.2 MB/s on a gigabit
/// runner. That rate is gzip decompression plus overlayfs writes on two
/// cores, and every layer the fleet moves is one we exported, so this is
/// ours to set.
///
/// `zstd` decompresses several times faster than gzip at a similar size.
/// `uncompressed` skips decompression entirely, paying bytes for CPU, which
/// on a local mesh with slow cores can win outright.
///
/// OFF by default. gzip is what buildkit does and what every number in
/// `docs/fleet-findings.md` was measured against; changing it silently
/// would reprice the whole document.
///
/// `force-compression` is not optional when this is set. Without it buildkit
/// reuses whatever compression a layer already carries, so a re-exported
/// base image stays gzip and the setting reads as having done nothing -
/// which is the failure mode this project has spent the most time on.
fn compression_attrs(want: Option<&str>) -> HashMap<String, String> {
    let Some(v) = want.map(str::trim).filter(|v| !v.is_empty()) else {
        return HashMap::new();
    };
    if !matches!(v, "gzip" | "zstd" | "uncompressed" | "estargz") {
        println!("[solve] REBUCK2_COMPRESSION={v:?} is not one of gzip|zstd|uncompressed|estargz");
        return HashMap::new();
    }
    HashMap::from([
        ("compression".to_owned(), v.to_owned()),
        ("force-compression".to_owned(), "true".to_owned()),
    ])
}

fn publish_attrs(name: String) -> HashMap<String, String> {
    HashMap::from([
        ("name".to_owned(), name),
        // Push, or the result stays in this daemon's own cache and no peer
        // can have it. The export IS the handover.
        ("push".to_owned(), "true".to_owned()),
        // Plain HTTP: the mirror has no TLS and no auth, which is precisely
        // why it is bound to loopback. Without this buildkit attempts https
        // and the push dies on a certificate nobody issued.
        ("registry.insecure".to_owned(), "true".to_owned()),
        ("source-date-epoch".to_owned(), "0".to_owned()),
        ("rewrite-timestamp".to_owned(), "true".to_owned()),
    ])
    .into_iter()
    .chain(compression_attrs(
        std::env::var("REBUCK2_COMPRESSION").ok().as_deref(),
    ))
    .collect()
}

/// `host:port/repo:tag` + digest -> `host:port/repo@sha256:...`
///
/// The tag has to come off, and the host's port must not. Both are colons,
/// and only the LAST one separates the tag - `rsplit_once` rather than
/// `split_once`, which would truncate `127.0.0.1:5000/x:t` to `127.0.0.1`
/// and produce a reference naming a registry that does not exist.
fn by_digest_ref(name: &str, digest: &str) -> String {
    // The tag colon is the one AFTER the last slash. `rsplit_once(':')`
    // alone gets the common case right and quietly eats the PORT of a
    // registry named without a tag - `registry:5000/repo` becomes
    // `registry@sha256:...`. Constructed names here always carry a tag, so
    // that would have sat unexercised until something else called this.
    let cut = name.rfind('/').map_or(0, |i| i + 1);
    let repo = match name[cut..].rfind(':') {
        Some(i) => &name[..cut + i],
        None => name,
    };
    format!("{repo}@{digest}")
}

/// A legal OCI tag naming one context within one build.
///
/// An OCI tag is `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`, and earthly names its
/// contexts by relative path - `./buildkitd`, `./tests/config`. Interpolating
/// the name made the reference illegal and the push failed with `invalid
/// reference format`, which the gateway then reported as the context simply
/// being unmirrored. 686 of 1326 solves on `+test-no-qemu`, over half the
/// target, blocked on that.
///
/// HASHED, not sanitised. Any scheme that substitutes illegal characters
/// collapses `./a/b` and `./a-b` onto one tag, and two contexts sharing a tag
/// is a build that silently uses the wrong files.
pub fn context_tag(session: &str, local_name: &str) -> String {
    // The session stays readable: it is already tag-legal, and being able to
    // see which build a tag belongs to is worth the length.
    format!(
        "{session}-{}",
        &crate::store::sha256_hex(local_name.as_bytes())[..32]
    )
}

/// Where the fleet keeps its shared buildkit cache, if it keeps one.
///
/// `None` unless REBUCK2_FLEET_CACHE=1, and OFF by default on purpose: this
/// is the first change that could make the fleet FASTER rather than merely
/// correct, and an unmeasured speedup that defaults on is indistinguishable
/// from one that does not work.
///
/// The measurement it exists to answer: six machines took 590s against 307s
/// on one, and `go-mod` (~24.2s per lead) plus `go-build` (~24.7s) account
/// for the gap. A cache mount does not travel, so one machine pays that once
/// and six machines pay it six times.
pub fn fleet_cache_ref(registry: &str) -> Option<String> {
    (fleet_cache_mode() != "off").then(|| format!("{registry}/rebuck2/cache:fleet"))
}

/// `off` (default), `read`, or `readwrite`.
///
/// Three settings because the first measurement killed one of them and left
/// the other two open. Six machines with `readwrite` took 755s against 590s
/// without any cache at all - `mode=max` exports the whole layer set after
/// EVERY solve, and there were 84 of them, so the write amplification grows
/// with exactly the number this fleet exists to increase.
///
/// `read` keeps the half that could still pay: a worker that imports a warm
/// `go-mod` and never exports one moves the cache once instead of 84 times.
pub fn fleet_cache_mode() -> String {
    std::env::var("REBUCK2_FLEET_CACHE")
        .map(|v| match v.as_str() {
            "1" | "readwrite" | "rw" => "readwrite".to_owned(),
            "read" | "ro" => "read".to_owned(),
            _ => "off".to_owned(),
        })
        .unwrap_or_else(|_| "off".to_owned())
}

/// Import from the fleet cache, and export back into it.
///
/// ONE ref for the whole fleet, and last-writer-wins on the tag. That is
/// crude and it is the right first version: the win being chased is a worker
/// starting warm rather than re-downloading the Go module graph, and for
/// that it does not matter whose export it reads, only that it reads one.
///
/// `ignore-error=true` on the export because a cache that fails to publish
/// must never fail the build. The whole feature is an optimisation, and an
/// optimisation that can turn a green build red is a liability.
fn cache_opts(registry: &str) -> Option<control::CacheOptions> {
    let r = fleet_cache_ref(registry)?;
    let entry = |extra: &[(&str, &str)]| control::CacheOptionsEntry {
        r#type: "registry".to_owned(),
        attrs: [("ref".to_owned(), r.clone())]
            .into_iter()
            .chain(
                extra
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
            )
            .collect(),
    };
    Some(control::CacheOptions {
        imports: vec![entry(&[("registry.insecure", "true")])],
        // EXPORTS ONLY IN readwrite, and that is a measured decision:
        // exporting per solve made six machines 165s SLOWER than no cache at
        // all. Reading costs one transfer; writing cost 84.
        exports: if fleet_cache_mode() == "readwrite" {
            vec![entry(&[
                ("registry.insecure", "true"),
                // max: intermediate layers too - a cache of final images
                // would not warm a `go mod download`.
                ("mode", "max"),
                ("ignore-error", "true"),
            ])]
        } else {
            Vec::new()
        },
        ..Default::default()
    })
}

/// Cache options for the ONE build that populates the fleet's cache.
///
/// Export only. The client's build runs once on peer 0, so exporting from it
/// costs one write; the 84 dispatched subtrees then import what it wrote.
/// That asymmetry is the whole point - exporting from every subtree instead
/// made six machines 165s slower than no cache at all.
pub fn reference_export(registry: &str) -> Option<control::CacheOptions> {
    let r = fleet_cache_ref(registry)?;
    Some(control::CacheOptions {
        exports: vec![control::CacheOptionsEntry {
            r#type: "registry".to_owned(),
            attrs: [
                ("ref", r.as_str()),
                ("registry.insecure", "true"),
                // max, or the cache holds final layers only and warms
                // nothing a worker actually needs.
                ("mode", "max"),
                ("ignore-error", "true"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        }],
        ..Default::default()
    })
}

/// The request that builds `def` and publishes it where a peer can get it.
pub fn solve_request(
    job: u64,
    def: pb::Definition,
    registry: &str,
    session: &str,
) -> control::SolveRequest {
    let attrs = publish_attrs(result_ref(registry, job));

    control::SolveRequest {
        // Buildkit keys solves by this ref and rejects a repeat with
        // `job ID "..." exists`. Two consequences, both measured rather
        // than guessed, and the second only after the first was fixed
        // badly:
        //
        // Empty works EXACTLY ONCE per daemon, then fails forever.
        //
        // And the JOB NUMBER is not enough either. A worker's buildkitd
        // outlives any one lap, so job 1 comes round again next run and
        // collides with its own history. The pid and a counter are what
        // make it unrepeatable; the job number is in there to be legible
        // in buildkit's own progress output, not to provide uniqueness.
        r#ref: format!(
            "rebuck2-job{job}.{}.{}",
            std::process::id(),
            SOLVE_SEQ.fetch_add(1, Ordering::Relaxed)
        ),
        definition: Some(def),
        // The fleet's shared cache, when one is configured. Both the fork
        // and upstream have had Exports/Imports since v0.4.0, so unlike the
        // exporter there is no deprecated pair to set as well.
        cache: cache_opts(registry),
        // BOTH forms, deliberately. earthly's fork predates `exporters` and
        // reads only the deprecated pair; protobuf drops a field it does not
        // know without a word, so a request carrying only the new form asks
        // that daemon for no export at all - it solves, publishes nothing,
        // and returns success. See
        // `the_exporter_is_named_the_old_way_too_or_a_fork_ignores_it`.
        exporter_deprecated: "image".to_owned(),
        exporter_attrs_deprecated: attrs.clone(),
        exporters: vec![control::Exporter {
            r#type: "image".to_owned(),
            attrs,
        }],
        session: session.to_owned(),
        // No frontend. The requester already solved this into ops; running
        // one here would re-parse bytes that are not a Dockerfile.
        //
        // No entitlements, EVER. `dispatch::inspect` grounds any subtree
        // wanting insecure exec or host networking, so asking for the
        // privilege here would be incoherent - and would hand a peer's
        // daemon a capability on the strength of an offer. Granting
        // privileged exec is a trust decision, not a scheduling one.
        ..Default::default()
    }
}

/// Dial a buildkitd's Control service.
///
/// `addr` is a gRPC endpoint (`http://127.0.0.1:1234`). Earthly runs a
/// buildkitd per container, so on a worker this is loopback.
/// A daemon address as tonic needs it.
///
/// Every caller in this crate has to know that a buildkit address is a URL
/// to tonic and a `host:port` to everyone else - the workflow, buildctl,
/// earthly's own `--buildkit-host`, and every log line that ever printed
/// one. `harvest-cache` did not know, and died on
///
///     transport error
///     caused by: invalid URL, scheme is missing
///
/// which is tonic's wording and names no argument. That is the fourth
/// distinct fault to stop a seeding run, and the sign that the convention
/// should never have been the caller's to keep.
///
/// `tcp://` is translated rather than passed through: it is what earthly
/// writes and what tonic cannot dial, so leaving it would move the same
/// error later.
pub fn daemon_url(addr: &str) -> String {
    match addr.split_once("://") {
        Some(("tcp", rest)) => format!("http://{rest}"),
        Some(_) => addr.to_owned(),
        None => format!("http://{addr}"),
    }
}

pub async fn connect(
    addr: &str,
) -> anyhow::Result<control::control_client::ControlClient<tonic::transport::Channel>> {
    Ok(control::control_client::ControlClient::connect(daemon_url(addr)).await?)
}

/// The one-source-and-a-terminal graph that pulls a context out of a client.
///
/// Separate from [`publish_context`] because the interesting part is the
/// attrs, and they are worth asserting on without a daemon.
///
/// `local.sharedkeyhint` is the differ's baseline: buildkit keys the
/// previously-transferred filesystem by it, and fsutil sends only what
/// changed since. Omitting it is not a slow path, it is a full re-send of
/// the whole context on every build - which is how earthbuild's
/// `copy-test-verbose-output` caught it, that test asserting a file it had
/// already sent was not sent again.
///
/// The hint is NAMESPACED rather than borrowed from the op we are
/// materialising for, and that is a correctness point, not tidiness: a hint
/// identifies a transferred filesystem, so sharing one with earthly's own
/// `local.includepattern`-filtered transfers risks serving a filtered
/// snapshot as if it were the whole context. We only ever publish the whole
/// thing, so a prefix nobody else writes keeps the baseline honest.
///
/// Considered mixing earthly's own hint in to keep two projects that both
/// call their context `context` off one baseline (rejected: their hint is
/// only assumed stable across builds, and if it is not, ours resets every
/// build and the re-send comes back). A shared baseline between projects
/// costs a worse diff, never a wrong one - fsutil syncs, it does not trust.
fn context_def(session: &str, local_name: &str) -> pb::Definition {
    use prost::Message;

    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: format!("local://{local_name}"),
            attrs: [
                ("local.session".to_owned(), session.to_owned()),
                // NOT keyed by session: a second `earthly` invocation is a
                // new session, and diffing against the previous BUILD is the
                // point.
                (
                    "local.sharedkeyhint".to_owned(),
                    format!("rebuck2-full:{local_name}"),
                ),
            ]
            .into_iter()
            .collect(),
        })),
        ..Default::default()
    };
    let src_b = src.encode_to_vec();
    let term = pb::Op {
        inputs: vec![pb::Input {
            digest: format!("sha256:{}", crate::store::sha256_hex(&src_b)),
            index: 0,
        }],
        ..Default::default()
    };
    pb::Definition {
        metadata: [(
            format!("sha256:{}", crate::store::sha256_hex(&src_b)),
            pb::OpMetadata::default(),
        )]
        .into_iter()
        .collect(),
        def: vec![src_b, term.encode_to_vec()],
        ..Default::default()
    }
}

/// Materialise a client's build context as an image a peer can pull.
///
/// The context reaches a daemon by filesync over the client's session, and
/// only that daemon can ask for it. So we do not intercept the bytes - which
/// would mean demultiplexing a gRPC connection tunnelled inside the session
/// stream - we ask the daemon that already has the session to hand the
/// context back to us as content.
///
/// The trick is `session`: passing the CLIENT's session id makes the daemon
/// resolve `local://` through the filesync the client is already serving.
/// The graph is one source op and a terminal, exported straight to the
/// mirror.
///
/// Principle 9, with the client as the origin: fetch once into the fleet,
/// then serve peer to peer.
pub async fn publish_context(
    bk_addr: &str,
    registry: &str,
    session: &str,
    local_name: &str,
) -> anyhow::Result<String> {
    let def = context_def(session, local_name);

    // Named by the session, so two concurrent builds do not publish over
    // each other, and a rebuild of the same context is the same ref.
    let name = format!(
        "{registry}/rebuck2/context:{}",
        context_tag(session, local_name)
    );
    let attrs = publish_attrs(name.clone());

    let mut c = connect(bk_addr).await?;
    let resp = c
        .solve(control::SolveRequest {
            r#ref: format!(
                "rebuck2-ctx.{}.{}",
                std::process::id(),
                SOLVE_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            definition: Some(def),
            // The client's session, not ours: it is the only one with the files.
            session: session.to_owned(),
            // BOTH forms. `exporters` is the current field; older daemons -
            // earthbuild ships a v0.8.17-era buildkitd - read only
            // `exporter_deprecated`, IGNORE the plural silently, and return a
            // successful solve having exported nothing. A push that no-ops
            // while reporting success is the worst possible failure mode, and
            // it cost an iteration to find.
            exporter_deprecated: "image".to_owned(),
            exporter_attrs_deprecated: attrs.clone(),
            exporters: vec![control::Exporter {
                r#type: "image".to_owned(),
                attrs,
            }],
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("publish context: {} {}", e.code(), e.message()))?;
    // BY DIGEST, the third and last kind of source that has to cross a
    // machine. Subtree results, base images, and now contexts: each was
    // fixed separately and each failed the same way, because on ONE machine
    // a tag in the local registry resolves perfectly and nothing is wrong.
    //
    // Measured across three runners, in this order, each fix revealing the
    // next: base first, then
    //
    //     failed to load cache key: 172.17.0.1:15000/rebuck2/context:<tag>
    //
    // `172.17.0.1:15000` exists on every runner and is a different registry
    // on each. A digest is content and needs no gossip; a tag is a name in
    // one machine's namespace.
    let by_digest = resp
        .into_inner()
        .exporter_response
        .get("containerimage.digest")
        .filter(|d| d.starts_with("sha256:"))
        .cloned();
    Ok(match by_digest {
        Some(d) => format!("docker-image://{}", by_digest_ref(&name, &d)),
        None => format!("docker-image://{name}"),
    })
}

/// The blobs an image manifest names: its config and every layer.
///
/// Pure, because the interesting failure is a manifest shape we do not
/// expect - a manifest LIST rather than a manifest, an empty layers array,
/// a digest without its algorithm - and none of those need a registry to
/// reproduce.
pub fn manifest_blobs(json: &str) -> Vec<crate::mesh::Dig> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    // A manifest list names manifests, not layers. Following it would need
    // another fetch and a platform choice; returning nothing leaves the lazy
    // path exactly as it was, which is the right failure for an advisory
    // mechanism.
    let mut out = Vec::new();
    for key in ["config"] {
        if let (Some(d), Some(sz)) = (v[key]["digest"].as_str(), v[key]["size"].as_i64()) {
            out.push(crate::mesh::Dig {
                hash: d.trim_start_matches("sha256:").to_owned(),
                size: sz,
            });
        }
    }
    for l in v["layers"].as_array().into_iter().flatten() {
        if let (Some(d), Some(sz)) = (l["digest"].as_str(), l["size"].as_i64()) {
            out.push(crate::mesh::Dig {
                hash: d.trim_start_matches("sha256:").to_owned(),
                size: sz,
            });
        }
    }
    out
}

/// Ask the mirror what an image is made of.
///
/// Best effort by design: this feeds a PREFETCH, and a prefetch that cannot
/// find out what to fetch leaves the lazy path untouched.
/// The registry URL that would return this image's manifest.
///
/// `Err` names what is missing. The caller used to fold every failure into
/// one `None` and print "could not read the manifest", which appeared 412
/// times in a single run without ever saying whether the reference was
/// unparseable, the host unreachable, or the registry unhappy - three faults
/// with three different fixes and one message.
pub fn manifest_url(image_ref: &str) -> std::result::Result<String, String> {
    let r = image_ref
        .strip_prefix("docker-image://")
        .unwrap_or(image_ref);
    let Some((host, rest)) = r.split_once('/') else {
        return Err(format!(
            "{r}: no host - a manifest URL needs `host/repo@digest` or \
             `host/repo:tag`, and a bare digest names content without saying \
             where to ask for it"
        ));
    };
    let (repo, reference) = match rest.rsplit_once('@') {
        Some((repo, dig)) => (repo, dig.to_owned()),
        None => match rest.rsplit_once(':') {
            Some((repo, tag)) => (repo, tag.to_owned()),
            None => {
                return Err(format!(
                    "{r}: no tag and no digest - nothing after `{rest}` to \
                     identify which manifest"
                ))
            }
        },
    };
    Ok(format!("http://{host}/v2/{repo}/manifests/{reference}"))
}

/// The blobs an image is made of, or the reason we could not find out.
///
/// Reports rather than returns the reason, because every caller so far wants
/// it printed and none can act on it. Silence here cost two runs: prefetch
/// announced nothing at all in either, and the log said only that a manifest
/// could not be read.
pub async fn image_blobs(image_ref: &str) -> Option<Vec<crate::mesh::Dig>> {
    let url = match manifest_url(image_ref) {
        Ok(u) => u,
        Err(why) => {
            println!("[solve] no manifest URL for {why}");
            return None;
        }
    };
    let res = match reqwest::Client::new()
        .get(&url)
        .header(
            "Accept",
            "application/vnd.oci.image.manifest.v1+json,\
             application/vnd.docker.distribution.manifest.v2+json",
        )
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            println!("[solve] {url}: {e}");
            return None;
        }
    };
    // A NON-2xx is not the same as an unreachable registry, and folding them
    // together is what made 412 identical lines out of what may well be two
    // different problems. The body is read either way - a registry's error
    // body is often the only statement of what it objected to.
    let status = res.status();
    let body = match res.text().await {
        Ok(b) => b,
        Err(e) => {
            println!("[solve] {url}: {status}, body unreadable: {e}");
            return None;
        }
    };
    if !status.is_success() {
        println!(
            "[solve] {url}: {status} {}",
            body.chars().take(160).collect::<String>()
        );
        return None;
    }
    Some(manifest_blobs(&body))
}

/// Copy a registry image into the mirror, so a peer can fetch it without
/// credentials.
///
/// A sessionless peer cannot pull from Docker Hub - registry auth travels
/// over the session, and rewriting `local://` away is precisely what leaves
/// a peer without one. Configuring the mirror as a `mirrors` entry does not
/// help either: the pull-through serves blobs but 404s a manifest, so
/// buildkit falls through to the origin and dies there.
///
/// So the base image is copied the same way the context is: by the daemon
/// that DOES have the session, once, into the mirror. Principle 9 as
/// written - the origin registry is a fallback, not a data path.
pub async fn mirror_image(
    bk_addr: &str,
    registry: &str,
    session: &str,
    reference: &str,
    // Which variant to fetch, as `os/arch`. The mirroring daemon resolves
    // the reference for THIS platform, not for its own.
    platform: Option<&str>,
) -> anyhow::Result<String> {
    use prost::Message;
    // A one-op graph: fetch it, export it. No exec, so nothing is built -
    // this is a copy with extra steps, and the extra steps are what let the
    // daemon with the credentials do the fetching.
    //
    // The platform is not cosmetic. Without it an arm64 daemon mirroring
    // `alpine:3.20` puts the ARM64 image in the mirror, and an x86 peer
    // handed that graph pulls it and dies with `exit code: 255` on a binary
    // it cannot execute. Measured across two real machines; invisible on one.
    let plat = platform.and_then(|p| {
        let (os, architecture) = p.split_once('/')?;
        Some(pb::Platform {
            os: os.to_owned(),
            architecture: architecture.to_owned(),
            ..Default::default()
        })
    });
    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: reference.to_owned(),
            ..Default::default()
        })),
        platform: plat,
        ..Default::default()
    };
    let src_b = src.encode_to_vec();
    let digest = format!("sha256:{}", crate::store::sha256_hex(&src_b));
    let term = pb::Op {
        inputs: vec![pb::Input {
            digest: digest.clone(),
            index: 0,
        }],
        ..Default::default()
    };
    let def = pb::Definition {
        metadata: [(digest, pb::OpMetadata::default())].into_iter().collect(),
        def: vec![src_b, term.encode_to_vec()],
        ..Default::default()
    };

    // The platform is IN THE TAG. Two architectures of one image are two
    // different blobs, and a tag that named only the reference would have the
    // second overwrite the first - or worse, be reused by a peer of the wrong
    // architecture and fail as described above.
    let tag = format!(
        "{}{}",
        &crate::store::sha256_hex(reference.as_bytes())[..32],
        platform
            .map(|p| format!("-{}", p.replace('/', "-")))
            .unwrap_or_default()
    );
    let name = format!("{registry}/rebuck2/base:{tag}");
    let attrs = publish_attrs(name.clone());

    let mut c = connect(bk_addr).await?;
    let resp = c
        .solve(control::SolveRequest {
            r#ref: format!(
                "rebuck2-base.{}.{}",
                std::process::id(),
                SOLVE_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            definition: Some(def),
            // The CLIENT's session, for the same reason the context needs it.
            // Registry auth travels over the session, and buildkit cannot do
            // even an ANONYMOUS Docker Hub pull without it - the token comes
            // from the session's auth service. A warm cache hid this once:
            // the copy succeeded because the image was already local, and
            // failed the moment it actually had to fetch.
            session: session.to_owned(),
            exporter_deprecated: "image".to_owned(),
            exporter_attrs_deprecated: attrs.clone(),
            exporters: vec![control::Exporter {
                r#type: "image".to_owned(),
                attrs,
            }],
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("mirror {reference}: {} {}", e.code(), e.message()))?;

    // BY DIGEST where the daemon gives us one, and this is what makes a
    // second machine work at all.
    //
    // A tag lives in ONE registry's mutable namespace. `172.17.0.1:15000`
    // resolves on every GitHub runner and points at a DIFFERENT registry on
    // each, so a worker handed `172.17.0.1:15000/rebuck2/base:<tag>` asks its
    // own registry, which has never heard of it:
    //
    //     failed to load cache key: 172.17.0.1:15000/rebuck2/base:<tag>
    //
    // Measured across three runners: subtrees were offered, taken, and every
    // one of them died there. Tags are not gossiped - `registry.rs` has
    // called that the next step for a while, and this is the step.
    //
    // A digest needs no gossip. The worker's registry misses, asks the mesh
    // by hash, and whoever mirrored it serves it.
    let by_digest = resp
        .into_inner()
        .exporter_response
        .get("containerimage.digest")
        .filter(|d| d.starts_with("sha256:"))
        .cloned();
    Ok(match by_digest {
        // `repo@sha256:...`, which is what an OCI client resolves without
        // consulting the tag namespace at all.
        Some(d) => format!("docker-image://{}", by_digest_ref(&name, &d)),
        // A daemon that reports no digest has published under the tag and
        // nothing else. On one machine that still works, so this degrades
        // rather than failing - unlike `build_subtree`, where the same
        // fallback named a reference that did not exist.
        None => format!("docker-image://{name}"),
    })
}

/// Build an offered subtree and publish it where a peer can fetch it.
///
/// Returns the ref the requester pulls. Everything here is I/O: the dial,
/// the solve, and the push the exporter performs. The DECISIONS - may it
/// travel, is it worth sending, will this worker take it - were all made
/// before we got here, by `crate::dispatch`.
/// What the DAEMON can build, in buildkit's own spelling.
///
/// A worker lends its buildkitd, not its host, and on macOS those disagree:
/// the host is `darwin/arm64` and the daemon in its container is
/// `linux/arm64`. A worker advertising the host is offered nothing, because
/// no linux graph matches it - measured, and the fleet reported "took
/// nothing" six times for six perfectly dispatchable solves.
///
/// A Linux runner hides this completely: host and daemon agree there, so the
/// wrong value happens to be right and the bug waits for the first developer
/// on a Mac.
///
/// First platform is the native one; the rest are emulated. Empty on any
/// failure, and the caller falls back to the host rather than refusing to
/// join - a worker that cannot say what it is should still be able to lend
/// REAPI capacity.
pub async fn daemon_platforms(bk_addr: &str) -> Vec<String> {
    let Ok(mut c) = connect(bk_addr).await else {
        return Vec::new();
    };
    let Ok(resp) = c.list_workers(control::ListWorkersRequest::default()).await else {
        return Vec::new();
    };
    resp.into_inner()
        .record
        .first()
        .map(|w| {
            w.platforms
                .iter()
                .map(|p| format!("{}/{}", p.os, p.architecture))
                .collect()
        })
        .unwrap_or_default()
}

/// The cache mounts a daemon holds, by the id it keys them under.
///
/// buildkit names a cache mount record `cached mount <dest> from <manager>`,
/// plus ` with id "<id>"` when the id differs from the dest - see
/// `MountManager.getRefCacheDir`. So the daemon's own accounting can be
/// asked which ids exist and how big they are, which is the question a
/// harvest that returned 0.0 MiB leaves open.
///
/// Returns `(description, bytes)` for cache-mount records only. Best effort:
/// a daemon that will not answer leaves the caller exactly as informed as it
/// was.
pub async fn cache_mounts(addr: &str) -> Vec<(String, i64)> {
    let Ok(mut c) = connect(addr).await else {
        return Vec::new();
    };
    let Ok(resp) = c.disk_usage(control::DiskUsageRequest::default()).await else {
        return Vec::new();
    };
    let mut out: Vec<(String, i64)> = resp
        .into_inner()
        .record
        .into_iter()
        .filter(|r| r.record_type == "cachemount" || r.description.starts_with("cached mount "))
        .map(|r| (r.description, r.size))
        .collect();
    out.sort_by_key(|(d, sz)| (std::cmp::Reverse(*sz), d.clone()));
    out
}

/// The cache ids a daemon holds, pulled out of the descriptions.
///
/// buildkit appends ` with id "<id>"` to a cache mount's description when
/// the id differs from the destination, and omits it when they are the
/// same. See `MountManager.getRefCacheDir`. So the id is either in quotes at
/// the end or it IS the dest, and both cases have to be read.
pub fn cache_ids_held(mounts: &[(String, i64)]) -> Vec<String> {
    mounts
        .iter()
        .filter_map(|(desc, _)| {
            match desc.rsplit_once(" with id \"") {
                Some((_, tail)) => tail.strip_suffix('"').map(str::to_owned),
                // `cached mount <dest> from ...` - the id is the dest.
                None => desc
                    .strip_prefix("cached mount ")
                    .and_then(|r| r.split_once(" from "))
                    .map(|(dest, _)| dest.to_owned()),
            }
        })
        .collect()
}

/// Per-vertex timings for one solve, keyed the way the coordinator keys
/// them.
///
/// The discriminator for the 12.5x: the fleet spends 4,521s building against
/// a 212s whole-build baseline, duplication explains 1.7x of it, and nothing
/// says whether the rest is the same ops costing more on a worker or ops the
/// baseline never ran at all.
///
/// A vertex digest is `sha256:<hex of the marshalled op bytes>`
/// (`vertex.go:337`), so identical bytes give an identical key on either
/// machine and the two halves join on it. Same `note_vertices` the proxy
/// uses, against the worker's own daemon instead of the client's stream.
///
/// Best effort throughout: a worker that cannot read its own status stream
/// still builds, and this is a measurement rather than a dependency.
pub async fn vertex_times(
    addr: &str,
    solve_ref: &str,
) -> std::collections::BTreeMap<String, (u64, bool)> {
    let mut seen = Default::default();
    let Ok(mut c) = connect(addr).await else {
        return seen;
    };
    let Ok(stream) = c
        .status(control::StatusRequest {
            r#ref: solve_ref.to_owned(),
        })
        .await
    else {
        return seen;
    };
    let mut stream = stream.into_inner();
    // The stream stays open for the life of the solve; this runs after it,
    // so what arrives is the replay and then the end.
    while let Ok(Some(resp)) = stream.message().await {
        crate::proxy::note_vertices(&mut seen, &resp.vertexes);
    }
    seen
}

/// What a failed solve actually printed, from `Control.Status`.
///
/// A `Solve` answers with a code and a sentence. The container's own output
/// goes to the STATUS stream instead, keyed by the solve's ref: the
/// `cp: can't stat` or the `go: module lookup failed` that says WHY. Nothing
/// in this crate was reading it. So every failed subtree in every run so far has
/// reported its exit code and thrown away its reason, and the seeding
/// harvest spent three attempts on `exit code: 1` with nothing to read.
///
/// Best effort by construction: it is opened alongside the solve and
/// whatever has arrived by the time the solve fails is what gets printed. A
/// diagnostic that can fail a build is worse than no diagnostic.
async fn tail_logs(addr: &str, solve_ref: &str, lines: usize) -> Vec<String> {
    let Ok(mut c) = connect(addr).await else {
        return Vec::new();
    };
    let Ok(stream) = c
        .status(control::StatusRequest {
            r#ref: solve_ref.to_owned(),
        })
        .await
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let mut stream = stream.into_inner();
    // BOUNDED. A busy daemon can stream for as long as the build runs, and
    // this is called after the build is already over - but "already over"
    // is a race, so the wait is capped rather than trusted.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Ok(Ok(Some(msg))) = tokio::time::timeout_at(deadline, stream.message()).await {
        for l in &msg.logs {
            let text = String::from_utf8_lossy(&l.msg);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    out.push(line.to_owned());
                }
            }
        }
    }
    if out.len() > lines {
        out.split_off(out.len() - lines)
    } else {
        out
    }
}

pub async fn build_subtree(
    bk_addr: &str,
    registry: &str,
    job: u64,
    def: pb::Definition,
) -> anyhow::Result<String> {
    let mut c = connect(bk_addr).await?;
    // No session: measured, buildkit accepts a solve without one when the
    // build has no local sources and needs no registry auth, and a
    // dispatched subtree has neither.
    let req = solve_request(job, def, registry, "");
    // Kept, so the status stream can be asked about this exact solve.
    let solve_ref = req.r#ref.clone();
    let resp = match c.solve(req).await {
        Ok(r) => r.into_inner(),
        Err(e) => {
            // THE CONTAINER'S OWN WORDS. `solve` answers with a code and a
            // sentence; the `cp: can't stat` that says why is on the status
            // stream, and discarding it has cost three attempts on one
            // `exit code: 1`.
            let tail = tail_logs(bk_addr, &solve_ref, 20).await;
            if !tail.is_empty() {
                println!(
                    "[solve] what the build printed, last {} line(s):",
                    tail.len()
                );
                for l in &tail {
                    println!("[solve]   {l}");
                }
            }
            return Err(anyhow::anyhow!("solve: {} {}", e.code(), e.message()));
        }
    };
    // BY DIGEST, not by the tag we pushed to.
    //
    // A tag lives in ONE registry's mutable namespace. The requester is on
    // another machine with its own registry, and propagating tags between
    // them is a whole gossip problem - `registry.rs` calls it the next step.
    // A digest needs none of it: whoever ends up holding the manifest can
    // serve it, which is what the mesh already does for every other blob.
    //
    // Falls back to the tag when the exporter does not report one. On one
    // machine both work and the tag is what every earlier measurement used,
    // so this degrades to the old behaviour rather than failing.
    //
    // And BARE - the digest alone, no host. The builder pushed into the
    // registry it can reach, which on another machine is not the one the
    // requester can reach. A digest names content and not a location, so the
    // requester prefixes whichever registry it will actually pull from and
    // that registry answers by asking the fleet.
    //
    // Naming the builder's own registry here is what confined the fleet to
    // one host: it produced a reference nobody else could resolve.
    // PER-VERTEX timings, collected here because `solve_ref` is generated
    // inside `solve_request` and never escapes. The fleet spends 4,521s
    // building against a 212s whole-build baseline; duplication explains
    // 1.7x and nothing says whether the rest is the same ops costing more
    // on a worker or ops the baseline never ran. The digest joins the two
    // halves and this is the half that was missing.
    // OFF by default, and measured harmful. This runs inside the interval
    // `build_ms` covers, so its cost lands inside the number it exists to
    // explain: the leg went 1050s to 1207s and lead time 8,239s to 9,383s
    // with no mechanism change. Principle 16, in code written two hours
    // after the entry about it - I checked the PROXY tap for exactly this
    // and never asked the same question here.
    //
    // Its output is confounded as well. One worker reported 4,964 seconds of
    // vertex time inside a 1,207-second leg, because buildkit runs vertices
    // concurrently and their wall clocks sum to several times the elapsed
    // time. Vertex time and lead time are not comparable quantities, so the
    // ratio the split was designed around means nothing.
    //
    // Kept because the collection is correct and a future comparison
    // against the baseline's SAME digests would want it - that join is
    // blocked only because the baseline bypasses the proxy.
    if std::env::var("REBUCK2_WORKER_VERTICES").as_deref() == Ok("1") {
        let mut w = WORKER_VERTICES.lock().await;
        for (d, v) in vertex_times(bk_addr, &solve_ref).await {
            w.entry(d).or_insert(v);
        }
    }
    published_reference(&resp.exporter_response, registry, job)
}

/// Every vertex this worker's daemon ran, by digest. See `vertex_times`.
pub static WORKER_VERTICES: tokio::sync::Mutex<std::collections::BTreeMap<String, (u64, bool)>> =
    tokio::sync::Mutex::const_new(std::collections::BTreeMap::new());

/// What this worker spent building, per vertex, for the digest join.
pub async fn worker_vertex_summary() {
    let w = WORKER_VERTICES.lock().await;
    if w.is_empty() {
        return;
    }
    let ran = w.values().filter(|(_, c)| !c).count();
    let ms: u64 = w.values().filter(|(_, c)| !c).map(|(m, _)| m).sum();
    println!(
        "[worker] vertices     : {ran} ran in {ms}ms, {} cache hit(s) - join these \
         digests against the coordinator's baseline to price the fleet's build cost",
        w.values().filter(|(_, c)| *c).count()
    );
    // The ten most expensive, so a join can start without the whole map.
    let mut v: Vec<(&String, &(u64, bool))> = w.iter().filter(|(_, (_, c))| !c).collect();
    v.sort_by_key(|(_, (m, _))| std::cmp::Reverse(*m));
    for (d, (m, _)) in v.into_iter().take(10) {
        println!("[worker]   {m:>8}ms  {d}");
    }
}

/// What the builder may CLAIM to have published, given what the exporter said.
pub fn published_reference(
    exporter_response: &HashMap<String, String>,
    registry: &str,
    job: u64,
) -> anyhow::Result<String> {
    if let Some(d) = exporter_response
        .get("containerimage.digest")
        .filter(|d| d.starts_with("sha256:"))
    {
        return Ok(d.clone());
    }
    // NOT the tag. See `a_silent_export_is_a_failed_handover_not_a_tag`:
    // falling back here reports a name the registry does not hold, and the
    // requester finds out several minutes later with `not found`.
    //
    // The keys are in the message because the fix for THIS being wrong is
    // knowing what the exporter did say instead.
    let mut keys: Vec<&str> = exporter_response.keys().map(String::as_str).collect();
    keys.sort_unstable();
    anyhow::bail!(
        "subtree {job}: exporter reported no digest, so nothing was published to \
         {} - refusing to claim {}. exporter said: {keys:?}",
        registry,
        result_ref(registry, job)
    )
}

#[cfg(test)]
mod tests {
    /// "Could not read the manifest" is four different faults.
    ///
    /// It printed 412 times in one run and 82 in another - prefetch, the
    /// mechanism meant to fix cross-machine repetition, failing on every
    /// single image - and the message names none of: an unparseable
    /// reference, an unreachable host, a registry that answered with an
    /// error, or a manifest with no layers. Each wants a different fix and
    /// the log cannot tell them apart, so two runs of evidence say only
    /// "something is wrong somewhere".
    #[test]
    fn a_manifest_url_says_why_it_could_not_be_built() {
        let u = super::manifest_url;
        assert_eq!(
            u("docker-image://reg:15000/rebuck2/base@sha256:abc").unwrap(),
            "http://reg:15000/v2/rebuck2/base/manifests/sha256:abc"
        );
        assert_eq!(
            u("reg:15000/lib/busybox:1").unwrap(),
            "http://reg:15000/v2/lib/busybox/manifests/1"
        );
        // A BARE digest is the shape that would fail silently: no host, no
        // repo, nothing to build a URL from. It must name itself.
        let e = u("sha256:abcdef").unwrap_err();
        assert!(e.contains("sha256:abcdef"), "{e}");
        assert!(e.contains("host"), "says what is missing: {e}");
        // Host but no tag and no digest - equally unusable, equally silent
        // before.
        let e = u("reg:15000/rebuck2/base").unwrap_err();
        assert!(e.contains("tag") || e.contains("digest"), "{e}");
    }

    /// How our own layers are compressed, because unpack is the bottleneck.
    ///
    /// 87% of lead time in a measured run was in the seventeen leads that
    /// fetched more than a MiB, at 7.2 MB/s on a gigabit runner. That rate
    /// is gzip decompression plus overlayfs writes on two cores, not the
    /// wire - and every layer this fleet moves is one WE exported, so the
    /// compression is ours to choose.
    #[test]
    fn the_export_compression_is_ours_to_choose() {
        let a = |v: Option<&str>| {
            let m = super::compression_attrs(v);
            (
                m.get("compression").cloned(),
                m.get("force-compression").cloned(),
            )
        };
        // Default OFF: gzip is what buildkit does and what every number in
        // the findings was measured against. Changing it silently would
        // reprice the whole document.
        assert_eq!(a(None), (None, None));
        assert_eq!(a(Some("")), (None, None));
        // zstd decompresses several times faster than gzip at similar size.
        assert_eq!(
            a(Some("zstd")),
            (Some("zstd".to_owned()), Some("true".to_owned()))
        );
        // And the extreme: no decompression at all, paying bytes for CPU.
        // On a fast local mesh with slow cores that can win outright.
        assert_eq!(
            a(Some("uncompressed")),
            (Some("uncompressed".to_owned()), Some("true".to_owned()))
        );
        // `force-compression` matters: without it buildkit reuses whatever
        // compression a layer already had, so a re-exported base image stays
        // gzip and the setting reads as having done nothing.
        assert!(a(Some("gzip")).1.is_some());
        // Anything unrecognised is refused rather than passed through to a
        // daemon that answers with a less obvious error.
        assert_eq!(a(Some("brotli")), (None, None));
    }

    #[test]
    fn cache_ids_come_out_of_buildkits_own_descriptions() {
        let held = vec![
            (
                "cached mount /go/pkg/mod from exec /bin/sh -c 'go mod download' \
                 with id \"go-mod\""
                    .to_owned(),
                1_000i64,
            ),
            // No ` with id`, because buildkit omits it when the id IS the
            // dest. Reading only the quoted form would silently drop these.
            ("cached mount /root/.npm from exec npm ci".to_owned(), 5i64),
        ];
        let ids = super::cache_ids_held(&held);
        assert_eq!(ids, vec!["go-mod".to_owned(), "/root/.npm".to_owned()]);
    }

    /// An LLB source identifier is a URL, and three copies of this rule
    /// disagreed about that.
    #[test]
    fn every_reference_shape_becomes_a_valid_llb_source() {
        let f = |r: &str| super::llb_source("172.17.0.1:15000", r);
        assert_eq!(
            f("sha256:abc"),
            "docker-image://172.17.0.1:15000/rebuck2/subtree@sha256:abc"
        );
        // THE ONE THAT WAS WRONG. A full reference with no scheme, which is
        // what an older worker sends, was passed through unchanged by one of
        // the three copies - and buildkit rejects an unschemed identifier
        // with "failed to parse ... invalid", which is precisely what the
        // comment three lines above that copy warned about.
        assert_eq!(f("ghcr.io/me/x:v1"), "docker-image://ghcr.io/me/x:v1");
        // Already a URL: left alone, scheme and all.
        assert_eq!(
            f("docker-image://ghcr.io/me/x:v1"),
            "docker-image://ghcr.io/me/x:v1"
        );
        assert_eq!(
            f("git://example.com/r.git#main"),
            "git://example.com/r.git#main"
        );
    }

    /// A daemon address is a URL to tonic and a host:port to everyone else.
    ///
    /// Fourth distinct fault to stop a seeding run. `harvest-cache --bk
    /// 10.1.0.5:28372` died on `transport error / invalid URL, scheme is
    /// missing` - tonic's wording, and one that says nothing about which
    /// argument it means. The proxy is started with `--upstream
    /// http://$BK_ADDR` and never hit it; the new caller had no reason to
    /// know the convention, which is a sign the convention should not have
    /// been the caller's to keep.
    #[test]
    fn a_daemon_address_is_a_url_whether_or_not_it_was_written_as_one() {
        let d = super::daemon_url;
        assert_eq!(d("10.1.0.5:28372"), "http://10.1.0.5:28372");
        assert_eq!(d("127.0.0.1:8372"), "http://127.0.0.1:8372");
        // Already a URL: untouched, including the schemes buildkit itself
        // uses for a TLS daemon or a unix socket.
        assert_eq!(d("http://127.0.0.1:8372"), "http://127.0.0.1:8372");
        assert_eq!(d("https://bk.example:443"), "https://bk.example:443");
        assert_eq!(
            d("unix:///run/buildkit/buildkitd.sock"),
            "unix:///run/buildkit/buildkitd.sock"
        );
        // tcp:// is what earthly writes and what tonic cannot dial, so it
        // becomes http:// rather than being passed through to fail later.
        assert_eq!(d("tcp://host:8372"), "http://host:8372");
    }

    #[test]
    fn a_bare_digest_becomes_something_a_daemon_can_pull() {
        let p = |r: &str| super::pullable("172.17.0.1:15000", r);
        assert_eq!(
            p("sha256:abc"),
            "172.17.0.1:15000/rebuck2/subtree@sha256:abc"
        );
        // Not `docker-image://sha256:abc`, which is what the seeding path
        // built and no daemon can parse. It would have failed SAFE - the
        // resolve check drops a seed it cannot read - and the run would have
        // reported that seeding did not pay for a mechanism that never
        // addressed anything.
        assert!(!p("sha256:abc").starts_with("sha256:"));
        // An operator's own image is left alone, scheme or no scheme.
        assert_eq!(p("ghcr.io/me/seed:v1"), "ghcr.io/me/seed:v1");
        assert_eq!(
            p("docker-image://ghcr.io/me/seed:v1"),
            "docker-image://ghcr.io/me/seed:v1"
        );
    }

    #[test]
    fn a_manifest_names_its_config_and_layers_or_nothing() {
        use super::manifest_blobs;

        let m = r#"{
          "schemaVersion": 2,
          "config": {"digest":"sha256:cfg","size":1234},
          "layers": [
            {"digest":"sha256:aaa","size":100},
            {"digest":"sha256:bbb","size":200}
          ]
        }"#;
        let b = manifest_blobs(m);
        assert_eq!(b.len(), 3, "config plus two layers");
        // The algorithm prefix is stripped: the mesh keys blobs by bare hash
        // and a prefetch for `sha256:aaa` would warm nothing that a fetch for
        // `aaa` later consults.
        assert_eq!(b[0].hash, "cfg");
        assert_eq!(b[1].hash, "aaa");
        assert_eq!(b[1].size, 100);

        // A manifest LIST names manifests, not layers. Following it needs
        // another fetch and a platform choice; returning nothing leaves the
        // lazy path exactly as it was, which is the right failure for
        // something advisory.
        let list = r#"{"schemaVersion":2,
          "mediaType":"application/vnd.oci.image.index.v1+json",
          "manifests":[{"digest":"sha256:zzz","size":9}]}"#;
        assert!(manifest_blobs(list).is_empty(), "no layers, no guesses");

        // And nothing that is not a manifest is a panic.
        assert!(manifest_blobs("not json").is_empty());
        assert!(manifest_blobs("{}").is_empty());
        assert!(
            manifest_blobs(r#"{"layers":[{"digest":"sha256:x"}]}"#).is_empty(),
            "a layer with no size is not a Dig"
        );
    }
    use super::*;

    #[test]
    fn a_published_context_diffs_against_the_last_one_it_published() {
        use prost::Message;

        // Why this exists: `copy-test-verbose-output` in earthbuild's own
        // suite asserts that a second build does NOT re-send a file it
        // already sent, and it failed ONLY through the fleet. The cause is
        // here rather than anywhere clever - the synthetic `local://` op
        // that materialises a context carried no `local.sharedkeyhint`, so
        // fsutil had no previous transfer to diff against and the client
        // re-sent every byte, every build.
        let of = |d: &pb::Definition| -> pb::SourceOp {
            d.def
                .iter()
                .filter_map(|b| pb::Op::decode(b.as_slice()).ok())
                .find_map(|o| match o.op {
                    Some(pb::op::Op::Source(s)) => Some(s),
                    _ => None,
                })
                .expect("a context definition is a source and a terminal")
        };

        let a = of(&super::context_def("session-a", "context"));
        let hint = a.attrs.get("local.sharedkeyhint").expect("a hint");

        // NAMESPACED, and that is the load-bearing part. A hint is a cache
        // key for a transferred filesystem: sharing one with earthly's own
        // filtered transfers could serve a `local.includepattern`-filtered
        // snapshot as though it were the whole context. We only ever publish
        // the WHOLE context, so a prefix no other producer uses keeps our
        // baseline honest.
        assert!(hint.starts_with("rebuck2-full:"), "{hint}");
        assert!(hint.contains("context"), "names the context: {hint}");

        // Stable ACROSS sessions - a second `earthly` invocation is a new
        // session, and diffing against the previous build is the whole
        // point. (The tag stays per-session; only the differ's baseline is
        // shared.)
        let b = of(&super::context_def("session-b", "context"));
        assert_eq!(b.attrs.get("local.sharedkeyhint"), Some(hint));
        // ...but two different contexts must not share a baseline.
        let c = of(&super::context_def("session-a", "other"));
        assert_ne!(c.attrs.get("local.sharedkeyhint"), Some(hint));

        // The session still has to be the CLIENT's: it is the only one
        // serving the files.
        assert_eq!(
            a.attrs.get("local.session").map(String::as_str),
            Some("session-a")
        );
        assert_eq!(a.identifier, "local://context");
    }

    #[test]
    fn the_fleet_cache_is_off_unless_asked_for_and_never_fails_a_build() {
        // OFF by default, and that is a decision rather than an oversight:
        // this is the first change that could make the fleet faster instead
        // of merely correct, and a speedup that defaults on before it is
        // measured cannot be distinguished from one that does not work.
        //
        // (The env is process-wide, so this asserts the shape of what is
        // built when it IS set, and leaves the default to
        // `fleet_cache_ref`'s own condition.)
        let opts = super::cache_opts("r:5000");
        if super::fleet_cache_mode() == "readwrite" {
            let o = opts.expect("enabled means Some");
            assert_eq!(o.imports.len(), 1);
            assert_eq!(o.exports.len(), 1);
            let ex = &o.exports[0].attrs;
            assert_eq!(
                ex.get("ignore-error").map(String::as_str),
                Some("true"),
                "a cache that cannot publish must not fail the build - the \
                 whole feature is an optimisation, and an optimisation that \
                 turns a green build red is a liability"
            );
            assert_eq!(
                ex.get("mode").map(String::as_str),
                Some("max"),
                "min caches final layers only, which would not warm a go mod download"
            );
            assert_eq!(o.imports[0].attrs.get("ref"), ex.get("ref"));
        } else if super::fleet_cache_mode() == "read" {
            let o = opts.expect("read means Some");
            assert_eq!(o.imports.len(), 1);
            assert!(
                o.exports.is_empty(),
                "read mode must not export: exporting per solve made six \
                 machines 165s slower than no cache at all"
            );
        } else {
            assert!(opts.is_none(), "must be off unless asked for");
        }
    }

    #[test]
    fn a_digest_ref_keeps_the_registry_port_and_drops_the_tag() {
        // Two colons, and only the last one is the tag separator. Using
        // `split_once` here would turn
        //
        //     127.0.0.1:5000/rebuck2/base:abc  ->  127.0.0.1@sha256:...
        //
        // which names a registry that does not exist, on a code path that
        // only runs when a SECOND machine is involved - so it would have
        // looked like a cross-machine networking problem.
        let d = "sha256:".to_owned() + &"ab".repeat(32);
        assert_eq!(
            by_digest_ref("127.0.0.1:5000/rebuck2/base:abc", &d),
            format!("127.0.0.1:5000/rebuck2/base@{d}")
        );
        assert_eq!(
            by_digest_ref("host.docker.internal:25000/rebuck2/base:x-linux-arm64", &d),
            format!("host.docker.internal:25000/rebuck2/base@{d}")
        );
        // No tag at all: leave it alone rather than eating the port. The
        // second of these is the one that catches a lone `rsplit_once`.
        assert_eq!(
            by_digest_ref("registry.example.com/rebuck2/base", &d),
            format!("registry.example.com/rebuck2/base@{d}")
        );
        assert_eq!(
            by_digest_ref("registry:5000/rebuck2/base", &d),
            format!("registry:5000/rebuck2/base@{d}"),
            "a port is not a tag"
        );
    }

    #[test]
    fn a_context_tag_survives_a_context_name_with_a_slash_in_it() {
        // Measured on `earthly +test-no-qemu`: 686 solves - over half the
        // target - were reported as "context unmirrored" because the push
        // failed with
        //
        //     failed to push .../rebuck2/context:<session>-./buildkitd:
        //     invalid reference format
        //
        // An OCI tag is [a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}. Earthly names its
        // contexts by relative path - `./buildkitd`, `./tests/config` - so
        // the slash and the leading dot make the reference illegal. Exactly
        // one context in that build had a name that happened to be legal.
        //
        // Hashed rather than sanitised: two different names must not
        // collapse to one tag, and `./a/b` and `./a-b` both sanitise to the
        // same thing under any character-substitution scheme.
        for name in [
            "./buildkitd",
            "./tests/config",
            "plain",
            "./a/b",
            "UPPER/Case",
        ] {
            let tag = context_tag("sess1", name);
            assert!(
                tag.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
                "tag for {name:?} is not a legal OCI tag: {tag}"
            );
            assert!(
                tag.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric()),
                "a tag may not start with a separator: {tag}"
            );
            assert!(tag.len() <= 128, "tag too long: {tag}");
        }

        // Distinct names, distinct tags - including the pair that any
        // substitution scheme would collide.
        assert_ne!(context_tag("s", "./a/b"), context_tag("s", "./a-b"));
        // Distinct sessions, distinct tags: two concurrent builds must not
        // publish over each other.
        assert_ne!(context_tag("s1", "./x"), context_tag("s2", "./x"));
        // Same inputs, same tag - a rebuild must reuse the reference.
        assert_eq!(context_tag("s", "./x"), context_tag("s", "./x"));
    }

    #[test]
    fn the_exporter_is_named_the_old_way_too_or_a_fork_ignores_it() {
        // earthly's buildkit fork is cut from 2024-05, and its SolveRequest
        // has NO `Exporters` field - only `Exporter = 3` and
        // `ExporterAttrs = 4`. Protobuf drops fields it does not know
        // SILENTLY, so a request carrying only the new repeated form arrives
        // there with no exporter named at all.
        //
        // The fork then does exactly what it should with a request that asks
        // for no export: it solves, exports nothing, and returns an empty
        // response with no error. Measured - the subtree pushed zero blobs
        // and zero manifests while `mirror_image`, which sets both forms,
        // pushed its base image through the same daemon on the same run.
        //
        // Newer buildkit prefers `exporters` and falls back to this pair, so
        // setting both costs nothing and is what every buildkit since 0.13
        // documents the deprecated fields as being for.
        let r = solve_request(1, pb::Definition::default(), "r:5000", "");
        assert_eq!(
            r.exporter_deprecated, "image",
            "a 2024-era daemon reads THIS field and nothing else"
        );
        assert_eq!(
            r.exporter_attrs_deprecated, r.exporters[0].attrs,
            "the two forms must describe the same export, or which one the \
             daemon happens to read changes what gets published"
        );
    }

    #[test]
    fn a_silent_export_is_a_failed_handover_not_a_tag() {
        // Measured on `earthly +code`: the worker solved, reported
        //
        //     [driver] subtree job 1 built at .../rebuck2/subtree:job-1
        //
        // and the registry held no such tag - no manifest, no blobs, nothing
        // pushed under that name at all. The requester then died on
        //
        //     failed to load cache key: .../rebuck2/subtree:job-1: not found
        //
        // The old code called that a safe degradation: no digest reported, so
        // fall back to the tag we asked to push to. But the exporter reports
        // no digest exactly WHEN it did not export, so the tag it falls back
        // to is the one name guaranteed to be absent. The fallback converts a
        // failed build into a confident lie, and the driver has no way to
        // tell - it reassigns nothing, and the fleet's own refusal machinery
        // never runs.
        //
        // A handover nobody can fetch is not a handover.
        let empty = HashMap::new();
        let e = published_reference(&empty, "r:5000", 1)
            .expect_err("a silent export must not be reported as a publish");
        let msg = format!("{e:#}");
        assert!(
            msg.contains("no digest"),
            "the error must say what was missing: {msg}"
        );

        // And when the exporter DOES report one, that is the answer - bare,
        // because a digest names content rather than a host.
        let d = "sha256:".to_owned() + &"ab".repeat(32);
        let got = published_reference(
            &HashMap::from([("containerimage.digest".to_owned(), d.clone())]),
            "r:5000",
            1,
        )
        .unwrap();
        assert_eq!(got, d, "the digest is the reference");
    }

    fn req() -> control::SolveRequest {
        let def = pb::Definition {
            def: vec![b"op-bytes".to_vec()],
            ..Default::default()
        };
        solve_request(7, def, "127.0.0.1:5000", "sess-abc")
    }

    #[test]
    fn the_result_lands_where_a_peer_can_fetch_it_and_the_driver_cannot() {
        let r = req();
        let ex = r.exporters.first().expect("one exporter");
        assert_eq!(ex.r#type, "image");

        // Pushed, or it stays in this daemon's cache and no peer can have
        // it - the export IS the handover.
        assert_eq!(ex.attrs.get("push").map(String::as_str), Some("true"));

        // To LOOPBACK, which is this worker's own mirror. Principle 6: the
        // coordinator arbitrates and carries nothing, so the layers must
        // not be routed anywhere near it.
        let name = ex.attrs.get("name").expect("a name to push to");
        assert_eq!(name, &result_ref("127.0.0.1:5000", 7));
        assert!(name.starts_with("127.0.0.1:5000/"), "{name}");

        // Plain HTTP on loopback: the mirror has no TLS and no auth, which
        // is exactly why it is bound to loopback. Without this buildkit
        // tries https and the push fails on a certificate nobody has.
        assert_eq!(
            ex.attrs.get("registry.insecure").map(String::as_str),
            Some("true")
        );

        // Republishing an unchanged input must land on the bytes already
        // there, or the tag moves and everything it used to name becomes
        // garbage. Both attrs are needed and only the second one does the
        // work - see `publish_attrs` for the measurement.
        assert_eq!(
            ex.attrs.get("source-date-epoch").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            ex.attrs.get("rewrite-timestamp").map(String::as_str),
            Some("true")
        );
    }

    /// Needs a live daemon, so it does not run by default:
    ///   docker run -d --privileged -p 11234:1234 moby/buildkit \
    ///     --addr tcp://0.0.0.0:1234
    ///   cargo test --bin rebuck2 buildkit_is_reachable -- --ignored
    ///
    /// Proves the gRPC path end to end before anything is built on it: the
    /// crate's generated client, the wire version, and the daemon all agree.
    #[tokio::test]
    #[ignore]
    async fn buildkit_is_reachable_and_reports_a_worker() {
        let mut c = connect("http://127.0.0.1:11234").await.expect("dial");
        let workers = c
            .list_workers(control::ListWorkersRequest::default())
            .await
            .expect("list_workers")
            .into_inner();
        assert!(
            !workers.record.is_empty(),
            "a daemon with no worker is no use"
        );
        let w = &workers.record[0];
        assert!(!w.id.is_empty());
        println!("[probe] worker {} platforms={:?}", w.id, w.platforms.len());
    }

    /// A real subtree, built by a real daemon, from LLB we constructed
    /// ourselves. Needs a buildkitd, so it does not run by default:
    ///   docker run -d --privileged -p 11234:1234 moby/buildkit \
    ///     --addr tcp://0.0.0.0:1234
    ///   cargo test --bin rebuck2 a_real_subtree -- --ignored --nocapture
    ///
    /// This is the claim that could not be made from the bench: rebuck2 can
    /// hand hand-built LLB to buildkit and have it pull, exec and produce a
    /// snapshot. Measured on a first run: 13.58MB of alpine plus a 12.29kB
    /// exec result in the daemon's cache.
    #[tokio::test]
    #[ignore]
    async fn a_real_subtree_builds_on_a_real_daemon() {
        use prost::Message;
        let plat = pb::Platform {
            os: "linux".into(),
            architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
            ..Default::default()
        };
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));

        let src = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: base_image(),
                ..Default::default()
            })),
            platform: Some(plat.clone()),
            ..Default::default()
        };
        let src_b = src.encode_to_vec();

        let exec = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&src_b),
                index: 0,
            }],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    args: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "echo dispatched > /out".into(),
                    ],
                    cwd: "/".into(),
                    ..Default::default()
                }),
                mounts: vec![pb::Mount {
                    input: 0,
                    dest: "/".into(),
                    output: 0,
                    ..Default::default()
                }],
                ..Default::default()
            })),
            platform: Some(plat),
            ..Default::default()
        };
        let exec_b = exec.encode_to_vec();

        // LLB's terminal op: no `op` of its own, one input naming the real
        // result. Omitting it makes buildkit solve nothing and say so with
        // a success, which is the most misleading answer available.
        let term = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&exec_b),
                index: 0,
            }],
            ..Default::default()
        };

        let def = pb::Definition {
            metadata: [&src_b, &exec_b]
                .iter()
                .map(|b| (dg(b), pb::OpMetadata::default()))
                .collect(),
            def: vec![src_b, exec_b, term.encode_to_vec()],
            ..Default::default()
        };

        // What `inspect` says about it must agree with what we then do: an
        // ordinary exec on one platform travels.
        let v = crate::dispatch::inspect(&def);
        assert!(v.dispatchable(), "{v:?}");

        let mut c = connect("http://127.0.0.1:11234").await.expect("dial");
        c.solve(control::SolveRequest {
            r#ref: "rebuck2-e2e".into(),
            definition: Some(def),
            // NO session, and that is a finding rather than an omission:
            // measured, buildkit accepts a solve with none when the build
            // has no local sources and needs no registry auth. A session
            // exists to carry filesync and credentials, and a dispatched
            // subtree has neither - its inputs are digests.
            ..Default::default()
        })
        .await
        .expect("solve");
    }

    /// The digest cascade, against a real solver.
    ///
    /// `rewrite_local_sources` rebuilds every op whose bytes changed and
    /// relinks its consumers. The unit tests check the graph still hangs
    /// together; only buildkit can say whether it is still VALID LLB - a
    /// mis-linked input is a graph that decodes fine and solves to the
    /// wrong thing, or to nothing.
    ///
    ///   docker run -d --privileged -p 11234:1234 moby/buildkit \
    ///     --addr tcp://0.0.0.0:1234
    ///   cargo test --bin rebuck2 a_rewritten_graph -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn a_rewritten_graph_still_solves() {
        use prost::Message;
        let plat = pb::Platform {
            os: "linux".into(),
            architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
            ..Default::default()
        };
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));

        // A graph rooted at a LOCAL source, which is what a `COPY`-bearing
        // build actually sends and what cannot be dispatched as-is.
        let src = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "local://context".into(),
                attrs: [("local.session".to_owned(), "stale-session-id".to_owned())]
                    .into_iter()
                    .collect(),
            })),
            platform: Some(plat.clone()),
            ..Default::default()
        };
        let src_b = src.encode_to_vec();
        let exec = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&src_b),
                index: 0,
            }],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    args: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "test -f /etc/alpine-release".into(),
                    ],
                    cwd: "/".into(),
                    ..Default::default()
                }),
                mounts: vec![pb::Mount {
                    input: 0,
                    dest: "/".into(),
                    output: 0,
                    ..Default::default()
                }],
                ..Default::default()
            })),
            platform: Some(plat),
            ..Default::default()
        };
        let exec_b = exec.encode_to_vec();
        let term = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&exec_b),
                index: 0,
            }],
            ..Default::default()
        };
        let before = pb::Definition {
            metadata: [&src_b, &exec_b]
                .iter()
                .map(|b| (dg(b), pb::OpMetadata::default()))
                .collect(),
            def: vec![src_b, exec_b, term.encode_to_vec()],
            ..Default::default()
        };

        // Undispatchable as it stands: the frontier is the client's disk.
        let v = crate::dispatch::inspect(&before);
        assert!(v.dispatchable(), "no hazard, just a local frontier");
        let a = crate::dispatch::analyse(&before, 1);
        assert_eq!(a.cuts.first().unwrap().frontier.local, 1);

        // Point it at content any peer can fetch. Alpine stands in for a
        // published context: the question here is whether the REWRITE
        // produces valid LLB, not where the bytes came from.
        let after = crate::dispatch::rewrite_local_sources(&before, &|_| {
            Some("docker-image://docker.io/library/alpine:3.20".to_owned())
        });
        let a = crate::dispatch::analyse(&after, 1);
        assert_eq!(a.cuts.first().unwrap().frontier.local, 0);
        assert!(
            a.cuts.first().unwrap().frontier.is_free(),
            "now dispatchable"
        );

        let mut c = connect("http://127.0.0.1:11234").await.expect("dial");

        // FIRST the negative, or this test has no teeth. The un-rewritten
        // graph must FAIL: there is no session, so `local://context` has no
        // filesync to resolve through. If this passes, the positive below
        // proves nothing - it would be solving something that succeeds
        // whatever we do to it.
        let unrewritten = c
            .solve(control::SolveRequest {
                r#ref: format!("norewrite-{}", std::process::id()),
                definition: Some(before),
                ..Default::default()
            })
            .await;
        assert!(
            unrewritten.is_err(),
            "a local:// graph solved with no session - the positive case \
             below would then prove nothing"
        );

        // And now the solver agrees the rewrite is a graph. The exec asserts
        // it can SEE the substituted content, so a mis-linked input fails
        // here rather than silently building nothing: /etc/alpine-release
        // exists only if the rewritten source really became the rootfs.
        c.solve(control::SolveRequest {
            r#ref: format!("rewrite-{}", std::process::id()),
            definition: Some(after),
            ..Default::default()
        })
        .await
        .expect("the rewritten graph must solve");
    }

    /// The whole point, end to end: a `COPY`-bearing build made
    /// dispatchable by publishing its context as content.
    ///
    /// The context has one holder and reaches a builder by filesync, so a
    /// peer cannot have it (measured: 32 MiB of context is 32 MiB through
    /// the proxy). Principle 9's answer for an origin is fetch once into the
    /// fleet and serve peer to peer. This does exactly that and checks the
    /// builder really got OUR bytes.
    ///
    ///   cargo test --bin rebuck2 a_published_context -- --ignored --nocapture
    ///
    /// Needs the mirror, and a daemon told it may pull from it over http:
    ///   rebuck2 registry --store /tmp/ctx/store --bind 0.0.0.0:15000
    ///   docker run -d --privileged -p 11234:1234 \
    ///     -v .../buildkitd.toml:/etc/buildkit/buildkitd.toml:ro \
    ///     moby/buildkit --addr tcp://0.0.0.0:1234
    #[tokio::test]
    #[ignore]
    async fn a_published_context_reaches_a_peer() {
        use prost::Message;
        const MIRROR: &str = "host.docker.internal:15000";
        let plat = pb::Platform {
            os: "linux".into(),
            architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
            ..Default::default()
        };
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let marker = format!("published-context-{}", std::process::id());
        let mut c = connect("http://127.0.0.1:11234").await.expect("dial");

        // 1. Publish a context AS CONTENT. A FileOp writes the marker, so
        //    the bytes originate in the graph rather than from a filesync -
        //    which is the point: this stands in for context the proxy
        //    received from the client and is now republishing.
        let mkfile = pb::Op {
            op: Some(pb::op::Op::File(pb::FileOp {
                actions: vec![pb::FileAction {
                    input: -1,
                    secondary_input: -1,
                    output: 0,
                    action: Some(pb::file_action::Action::Mkfile(pb::FileActionMkFile {
                        path: "/ctx-marker".into(),
                        mode: 0o644,
                        data: marker.clone().into_bytes(),
                        ..Default::default()
                    })),
                }],
            })),
            platform: Some(plat.clone()),
            ..Default::default()
        };
        let mk_b = mkfile.encode_to_vec();
        let term = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&mk_b),
                index: 0,
            }],
            ..Default::default()
        };
        let ctx_def = pb::Definition {
            metadata: [(dg(&mk_b), pb::OpMetadata::default())]
                .into_iter()
                .collect(),
            def: vec![mk_b, term.encode_to_vec()],
            ..Default::default()
        };
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("name".to_owned(), format!("{MIRROR}/rebuck2/ctx:probe"));
        attrs.insert("push".to_owned(), "true".to_owned());
        attrs.insert("registry.insecure".to_owned(), "true".to_owned());
        c.solve(control::SolveRequest {
            r#ref: format!("publish-ctx-{}", std::process::id()),
            definition: Some(ctx_def),
            exporters: vec![control::Exporter {
                r#type: "image".into(),
                attrs,
            }],
            ..Default::default()
        })
        .await
        .expect("publishing the context into the mirror");

        // 2. A build whose context is LOCAL - undispatchable as it stands.
        //
        // Shaped like a real COPY: the rootfs is an ordinary image and the
        // context is MOUNTED beside it. An earlier version made the context
        // the whole rootfs, which has no shell - and runc reports a missing
        // binary as "exit code: 1", which reads exactly like a command that
        // ran and failed. Two iterations were spent on that.
        let base = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: base_image(),
                ..Default::default()
            })),
            platform: Some(plat.clone()),
            ..Default::default()
        };
        let base_b = base.encode_to_vec();
        let local_src = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "local://context".into(),
                ..Default::default()
            })),
            platform: Some(plat.clone()),
            ..Default::default()
        };
        let src_b = local_src.encode_to_vec();
        let exec = pb::Op {
            inputs: vec![
                pb::Input {
                    digest: dg(&base_b),
                    index: 0,
                },
                pb::Input {
                    digest: dg(&src_b),
                    index: 0,
                },
            ],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    // Checks the CONTENTS, not just the path: presence would
                    // pass on any image that happened to have the file.
                    args: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        format!("grep -q {marker} /ctx/ctx-marker"),
                    ],
                    cwd: "/".into(),
                    ..Default::default()
                }),
                mounts: vec![
                    pb::Mount {
                        input: 0,
                        dest: "/".into(),
                        output: 0,
                        ..Default::default()
                    },
                    pb::Mount {
                        input: 1,
                        dest: "/ctx".into(),
                        output: -1,
                        readonly: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })),
            platform: Some(plat),
            ..Default::default()
        };
        let exec_b = exec.encode_to_vec();
        let term2 = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&exec_b),
                index: 0,
            }],
            ..Default::default()
        };
        let before = pb::Definition {
            metadata: [&base_b, &src_b, &exec_b]
                .iter()
                .map(|b| (dg(b), pb::OpMetadata::default()))
                .collect(),
            def: vec![base_b, src_b, exec_b, term2.encode_to_vec()],
            ..Default::default()
        };
        let cut = crate::dispatch::analyse(&before, 1)
            .cuts
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(cut.frontier.local, 1, "undispatchable: the client's disk");
        assert!(!cut.frontier.is_free());

        // 3. Rewrite it to fetch the published context, and build it on a
        //    daemon that has never spoken to a client.
        let after = crate::dispatch::rewrite_local_sources(&before, &|_| {
            Some(format!("docker-image://{MIRROR}/rebuck2/ctx:probe"))
        });
        let cut = crate::dispatch::analyse(&after, 1)
            .cuts
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(cut.frontier.local, 0);
        assert_eq!(
            cut.frontier.registry, 2,
            "alpine, and our published context"
        );
        assert!(
            cut.frontier.is_free(),
            "now dispatchable: {:?}",
            cut.frontier
        );

        c.solve(control::SolveRequest {
            r#ref: format!("use-ctx-{}", std::process::id()),
            definition: Some(after),
            ..Default::default()
        })
        .await
        .expect("the peer must build from the published context");
    }

    /// Does earthbuild's buildkitd need a session to SOLVE, or only to
    /// EXPORT? The answer decides whether a peer needs a session server or
    /// merely a different way to hand its result over.
    #[tokio::test]
    #[ignore]
    async fn probe_sessionless_solve_vs_export() {
        let mut c = connect("http://127.0.0.1:11237").await.expect("dial");
        let def = alpine_exec_definition();

        // A graph whose ONLY source is our insecure mirror. If this needs a
        // session too, then a peer needs one unconditionally and there is
        // no way round building a session server.
        let mirror_only = c
            .solve(control::SolveRequest {
                r#ref: format!("mirroronly-{}", std::process::id()),
                definition: Some(crate::dispatch::import_graph(
                    "docker-image://host.docker.internal:15000/rebuck2/base:9ae21ebe3a0f58f1bd3d7d62b7bbfe17",
                )),
                ..Default::default()
            })
            .await;
        println!(
            "[probe] sessionless solve, MIRROR-ONLY source -> {}",
            match &mirror_only {
                Ok(_) => "OK".to_string(),
                Err(e) => e.message().to_string(),
            }
        );

        // Mirror-only source AND a mirror export - exactly what adoption
        // does, and the only combination not yet tried.
        let mut ma = std::collections::HashMap::new();
        ma.insert(
            "name".to_owned(),
            "host.docker.internal:15000/rebuck2/probe:m".to_owned(),
        );
        ma.insert("push".to_owned(), "true".to_owned());
        ma.insert("registry.insecure".to_owned(), "true".to_owned());
        let both = c
            .solve(control::SolveRequest {
                r#ref: format!("both-{}", std::process::id()),
                definition: Some(crate::dispatch::import_graph(
                    "docker-image://host.docker.internal:15000/rebuck2/base:9ae21ebe3a0f58f1bd3d7d62b7bbfe17",
                )),
                exporter_deprecated: "image".to_owned(),
                exporter_attrs_deprecated: ma.clone(),
                exporters: vec![control::Exporter { r#type: "image".into(), attrs: ma }],
                ..Default::default()
            })
            .await;
        println!(
            "[probe] MIRROR-only + MIRROR export -> {}",
            match &both {
                Ok(_) => "OK".to_string(),
                Err(e) => e.message().to_string(),
            }
        );

        let no_export = c
            .solve(control::SolveRequest {
                r#ref: format!("noexp-{}", std::process::id()),
                definition: Some(def.clone()),
                ..Default::default()
            })
            .await;
        println!(
            "[probe] sessionless solve, NO exporter -> {}",
            match &no_export {
                Ok(_) => "OK".to_string(),
                Err(e) => e.message().to_string(),
            }
        );

        let mut attrs = std::collections::HashMap::new();
        attrs.insert(
            "name".to_owned(),
            "host.docker.internal:15000/rebuck2/probe:x".to_owned(),
        );
        attrs.insert("push".to_owned(), "true".to_owned());
        attrs.insert("registry.insecure".to_owned(), "true".to_owned());
        let with_export = c
            .solve(control::SolveRequest {
                r#ref: format!("exp-{}", std::process::id()),
                definition: Some(def),
                exporter_deprecated: "image".to_owned(),
                exporter_attrs_deprecated: attrs.clone(),
                exporters: vec![control::Exporter {
                    r#type: "image".into(),
                    attrs,
                }],
                ..Default::default()
            })
            .await;
        println!(
            "[probe] sessionless solve, WITH exporter -> {}",
            match &with_export {
                Ok(_) => "OK".to_string(),
                Err(e) => e.message().to_string(),
            }
        );
    }

    /// The base image every fixture builds on.
    ///
    /// Overridable so the harness can point at a pull-through cache instead
    /// of Docker Hub. Fresh daemons pull the base on every run, and a day of
    /// that earns a `429 Too Many Requests` that looks exactly like a product
    /// failure - twelve red assertions with nothing wrong in this repo.
    pub(super) fn base_image() -> String {
        std::env::var("REBUCK2_LLB_BASE")
            .unwrap_or_else(|_| "docker-image://docker.io/library/alpine:3.20".to_owned())
    }

    /// Emit N builds whose exec mounts an SSH AGENT.
    ///
    /// The exec requires the agent to ANSWER, not merely for a socket to
    /// exist: `ssh-add` exits 2 when it cannot reach one. A dead socket
    /// therefore fails the build rather than passing quietly.
    #[test]
    #[ignore]
    fn write_ssh_llb() {
        use prost::Message;
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let out = std::env::var("REBUCK2_LLB_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let n: usize = std::env::var("REBUCK2_LLB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        for i in 0..n {
            let base = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: base_image(),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let base_b = base.encode_to_vec();
            let exec = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&base_b),
                    index: 0,
                }],
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    meta: Some(pb::Meta {
                        args: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            format!(
                                "apk add --no-cache openssh-client >/dev/null 2>&1; \
                                 ssh-add -l; test $? -ne 2; mkdir -p /result; \
                                 echo task-{i} > /result/task"
                            ),
                        ],
                        env: vec!["SSH_AUTH_SOCK=/run/ssh-agent.sock".into()],
                        cwd: "/".into(),
                        ..Default::default()
                    }),
                    mounts: vec![
                        pb::Mount {
                            input: 0,
                            dest: "/".into(),
                            output: 0,
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/run/ssh-agent.sock".into(),
                            output: -1,
                            mount_type: pb::MountType::Ssh as i32,
                            ssh_opt: Some(pb::SshOpt {
                                mode: 0o600,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/result".into(),
                            output: 1,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                ..Default::default()
            };
            let exec_b = exec.encode_to_vec();
            let term = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&exec_b),
                    index: 1,
                }],
                ..Default::default()
            };
            let def = pb::Definition {
                metadata: [&base_b, &exec_b]
                    .iter()
                    .map(|b| (dg(b), pb::OpMetadata::default()))
                    .collect(),
                def: vec![base_b, exec_b, term.encode_to_vec()],
                ..Default::default()
            };
            std::fs::write(
                out.join(format!("rebuck2-fanout-{i}.llb")),
                def.encode_to_vec(),
            )
            .unwrap();
        }
    }

    /// Emit N builds whose exec mounts a CACHE.
    ///
    /// The output must NOT depend on what is in the cache - that is the
    /// contract that makes a cache mount safe to have at all, and the thing
    /// dispatch relies on. It writes a marker into the cache and derives its
    /// result from the graph, so a cold peer and a warm home produce the same
    /// bytes.
    ///
    ///   cargo test --bin rebuck2 write_cache_llb -- --ignored --nocapture
    #[test]
    #[ignore]
    fn write_cache_llb() {
        use prost::Message;
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let out = std::env::var("REBUCK2_LLB_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let n: usize = std::env::var("REBUCK2_LLB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let work: usize = std::env::var("REBUCK2_LLB_WORK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        for i in 0..n {
            let base = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: base_image(),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let base_b = base.encode_to_vec();
            let exec = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&base_b),
                    index: 0,
                }],
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    meta: Some(pb::Meta {
                        args: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            format!(
                                "echo warm >> /cache/hits; \
                                 i=0; while [ $i -lt {work} ]; do dd if=/dev/zero bs=1M \
                                 count=20 2>/dev/null | sha256sum >/dev/null; \
                                 i=$((i+1)); done; mkdir -p /result; \
                                 echo task-{i} > /result/task"
                            ),
                        ],
                        cwd: "/".into(),
                        ..Default::default()
                    }),
                    mounts: vec![
                        pb::Mount {
                            input: 0,
                            dest: "/".into(),
                            output: 0,
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/cache".into(),
                            output: -1,
                            mount_type: pb::MountType::Cache as i32,
                            cache_opt: Some(pb::CacheOpt {
                                id: "rebuck2-probe-cache".into(),
                                sharing: pb::CacheSharingOpt::Shared as i32,
                            }),
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/result".into(),
                            output: 1,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                ..Default::default()
            };
            let exec_b = exec.encode_to_vec();
            let term = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&exec_b),
                    index: 1,
                }],
                ..Default::default()
            };
            let def = pb::Definition {
                metadata: [&base_b, &exec_b]
                    .iter()
                    .map(|b| (dg(b), pb::OpMetadata::default()))
                    .collect(),
                def: vec![base_b, exec_b, term.encode_to_vec()],
                ..Default::default()
            };
            std::fs::write(
                out.join(format!("rebuck2-fanout-{i}.llb")),
                def.encode_to_vec(),
            )
            .unwrap();
        }
    }

    /// Emit N builds whose exec asks for PRIVILEGED mode.
    ///
    /// The one class of exclusion with no lift and no plan for one. Cache
    /// mounts, secrets and agents are scheduling problems with a knob each;
    /// privileged exec is a trust decision, and no session service makes a
    /// peer's `--privileged` mean what this machine's would have meant.
    ///
    /// Exists so that claim is checked against a running fleet rather than
    /// only against `inspect`. The wiring in between - env var to `Allow` to
    /// verdict - is where a lift would widen by accident, and a unit test on
    /// either end sees none of it.
    ///
    ///   cargo test --bin rebuck2 write_insecure_llb -- --ignored --nocapture
    #[test]
    #[ignore]
    fn write_insecure_llb() {
        use prost::Message;
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let out = std::env::var("REBUCK2_LLB_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let n: usize = std::env::var("REBUCK2_LLB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let work: usize = std::env::var("REBUCK2_LLB_WORK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        // Which privilege to ask for. Both are excluded for the same reason
        // and differ only in the field they set, so one fixture covers both
        // rather than eighty near-identical lines twice.
        let host_net = std::env::var("REBUCK2_LLB_HOSTNET").is_ok();
        for i in 0..n {
            let base = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: base_image(),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let base_b = base.encode_to_vec();
            let exec = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&base_b),
                    index: 0,
                }],
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    meta: Some(pb::Meta {
                        args: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            format!(
                                "i=0; while [ $i -lt {work} ]; do dd if=/dev/zero bs=1M \
                                 count=20 2>/dev/null | sha256sum >/dev/null; \
                                 i=$((i+1)); done; mkdir -p /result; \
                                 echo task-{i} > /result/task"
                            ),
                        ],
                        cwd: "/".into(),
                        ..Default::default()
                    }),
                    // The whole point of the fixture.
                    security: if host_net {
                        pb::SecurityMode::Sandbox as i32
                    } else {
                        pb::SecurityMode::Insecure as i32
                    },
                    network: if host_net {
                        pb::NetMode::Host as i32
                    } else {
                        pb::NetMode::Unset as i32
                    },
                    mounts: vec![
                        pb::Mount {
                            input: 0,
                            dest: "/".into(),
                            output: 0,
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/result".into(),
                            output: 1,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                ..Default::default()
            };
            let exec_b = exec.encode_to_vec();
            let term = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&exec_b),
                    index: 1,
                }],
                ..Default::default()
            };
            let def = pb::Definition {
                metadata: [&base_b, &exec_b]
                    .iter()
                    .map(|b| (dg(b), pb::OpMetadata::default()))
                    .collect(),
                def: vec![base_b, exec_b, term.encode_to_vec()],
                ..Default::default()
            };
            let path = out.join(format!("rebuck2-fanout-{i}.llb"));
            std::fs::write(&path, def.encode_to_vec()).unwrap();
            println!("[fixture] {}", path.display());
        }
    }

    /// Emit N builds whose exec mounts a SECRET.
    ///
    /// The shape that was undispatchable by construction until a peer could
    /// be given a session. The exec checks the value, so a build that gets
    /// the wrong secret fails rather than quietly producing wrong bytes.
    ///
    ///   REBUCK2_SECRET=the-value cargo test --bin rebuck2 write_secret_llb \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore]
    fn write_secret_llb() {
        use prost::Message;
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let out = std::env::var("REBUCK2_LLB_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let n: usize = std::env::var("REBUCK2_LLB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let work: usize = std::env::var("REBUCK2_LLB_WORK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        for i in 0..n {
            let base = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: base_image(),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let base_b = base.encode_to_vec();
            let exec = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&base_b),
                    index: 0,
                }],
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    meta: Some(pb::Meta {
                        args: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            format!(
                                "test \"$(cat /run/secrets/probe)\" = the-value; \
                                 i=0; while [ $i -lt {work} ]; do dd if=/dev/zero bs=1M \
                                 count=20 2>/dev/null | sha256sum >/dev/null; \
                                 i=$((i+1)); done; mkdir -p /result; \
                                 echo task-{i} > /result/task"
                            ),
                        ],
                        cwd: "/".into(),
                        ..Default::default()
                    }),
                    mounts: vec![
                        pb::Mount {
                            input: 0,
                            dest: "/".into(),
                            output: 0,
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/run/secrets/probe".into(),
                            output: -1,
                            mount_type: pb::MountType::Secret as i32,
                            secret_opt: Some(pb::SecretOpt {
                                id: "rebuck2_probe".into(),
                                mode: 0o444,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/result".into(),
                            output: 1,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                ..Default::default()
            };
            let exec_b = exec.encode_to_vec();
            let term = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&exec_b),
                    index: 1,
                }],
                ..Default::default()
            };
            let def = pb::Definition {
                metadata: [&base_b, &exec_b]
                    .iter()
                    .map(|b| (dg(b), pb::OpMetadata::default()))
                    .collect(),
                def: vec![base_b, exec_b, term.encode_to_vec()],
                ..Default::default()
            };
            let path = out.join(format!("rebuck2-fanout-{i}.llb"));
            std::fs::write(&path, def.encode_to_vec()).unwrap();
            println!("[fixture] {}", path.display());
        }
    }

    /// Emit N plain-LLB builds that read a LOCAL build context.
    ///
    /// Every other fixture sources only from `docker-image://`, so
    /// `contexts published: 0` in every run and the context-publishing path -
    /// the thing that unpins a subtree from the machine holding the client's
    /// disk - had never once executed. A Dockerfile build would exercise it
    /// and cannot: a named frontend's graph never crosses the proxy.
    ///
    ///   cargo test --bin rebuck2 write_context_llb -- --ignored --nocapture
    #[test]
    #[ignore]
    fn write_context_llb() {
        use prost::Message;
        // Omitted when REBUCK2_LLB_PLATFORM=any. A graph that pins a platform
        // can only run natively on machines of that architecture, so a
        // mixed-architecture fleet has nothing to gain from it - correct, and
        // useless for measuring whether a SECOND MACHINE adds capacity. An
        // unpinned graph genuinely runs anywhere.
        let plat =
            (std::env::var("REBUCK2_LLB_PLATFORM").as_deref() != Ok("any")).then(|| pb::Platform {
                os: "linux".into(),
                architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
                ..Default::default()
            });
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let out = std::env::var("REBUCK2_LLB_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let n: usize = std::env::var("REBUCK2_LLB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        // How much work each build does. The optimal home:away split is not a
        // property of the machines alone - transfer cost is roughly constant
        // per dispatched build while compute scales with this, so a bigger
        // build makes dispatch relatively cheaper.
        let work: usize = std::env::var("REBUCK2_LLB_WORK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        for i in 0..n {
            let base = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: base_image(),
                    ..Default::default()
                })),
                platform: plat.clone(),
                ..Default::default()
            };
            let base_b = base.encode_to_vec();
            // The client's disk. Nothing a peer can reach, which is the whole
            // point: it has to become content before the graph can travel.
            let ctx = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    // A name with a SLASH and a leading dot by default,
                    // because that is what earthly emits - `./buildkitd`,
                    // `./tests/config` - and because the old fixture name,
                    // `context`, is one of the few that happens to be a
                    // legal OCI tag. 686 solves on a real target failed to
                    // publish with `invalid reference format` while all 51
                    // fixture checks passed, on the strength of that.
                    identifier: format!(
                        "local://{}",
                        std::env::var("REBUCK2_LLB_CONTEXT")
                            .unwrap_or_else(|_| "./ctx/sub".to_owned())
                    ),
                    // Without an attr that distinguishes them, all N local
                    // source vertices are one vertex, and buildkit syncs the
                    // FIRST client's directory and hands it to every other
                    // build - distinct graphs, distinct contexts, and every
                    // output still `task-0`. `local.unique` exists for
                    // exactly this; real clients set it, and `publish_context`
                    // in this file already sets `local.session` for the same
                    // reason.
                    attrs: [("local.unique".to_owned(), format!("build-{i}"))]
                        .into_iter()
                        .collect(),
                })),
                platform: plat.clone(),
                ..Default::default()
            };
            let ctx_b = ctx.encode_to_vec();
            let exec = pb::Op {
                inputs: vec![
                    pb::Input {
                        digest: dg(&base_b),
                        index: 0,
                    },
                    pb::Input {
                        digest: dg(&ctx_b),
                        index: 0,
                    },
                ],
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    meta: Some(pb::Meta {
                        args: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            // `# {i}` makes the GRAPH distinct per build.
                            // Without it all N graphs are byte-identical, and
                            // buildkit correctly treats identical vertices as
                            // one build: four solves, one execution, and every
                            // client gets build 0's bytes. Verified with no
                            // proxy and one daemon, so it is buildkit's
                            // single-flight and not a dispatch bug - but it
                            // makes the fixture measure nothing, because a
                            // context that only differs in CONTENT does not
                            // change the graph that names it.
                            format!(
                                "i=0; while [ $i -lt {work} ]; do dd if=/dev/zero bs=1M \
                                 count=20 2>/dev/null | sha256sum >/dev/null; \
                                 i=$((i+1)); done; mkdir -p /result; \
                                 cp /ctx/marker /result/task  # {i}"
                            ),
                        ],
                        cwd: "/".into(),
                        ..Default::default()
                    }),
                    mounts: vec![
                        pb::Mount {
                            input: 0,
                            dest: "/".into(),
                            output: 0,
                            ..Default::default()
                        },
                        // Read-only, so `output: -1` is right here - unlike on
                        // the rootfs, where it stops runc writing resolv.conf.
                        pb::Mount {
                            input: 1,
                            dest: "/ctx".into(),
                            output: -1,
                            readonly: true,
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/result".into(),
                            output: 1,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                platform: plat.clone(),
                ..Default::default()
            };
            let exec_b = exec.encode_to_vec();
            let term = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&exec_b),
                    index: 1,
                }],
                ..Default::default()
            };
            let def = pb::Definition {
                metadata: [&base_b, &ctx_b, &exec_b]
                    .iter()
                    .map(|b| (dg(b), pb::OpMetadata::default()))
                    .collect(),
                def: vec![base_b, ctx_b, exec_b, term.encode_to_vec()],
                ..Default::default()
            };
            let path = out.join(format!("rebuck2-fanout-{i}.llb"));
            std::fs::write(&path, def.encode_to_vec()).unwrap();
            println!("[fixture] {}", path.display());
        }
    }

    /// Emit N distinct plain-LLB builds - no frontend, no secrets, no host
    /// binds. What a client that is not earthly sends.
    ///
    ///   cargo test --bin rebuck2 write_fanout_llb -- --ignored --nocapture
    #[test]
    #[ignore]
    fn write_fanout_llb() {
        use prost::Message;
        // Omitted when REBUCK2_LLB_PLATFORM=any. A graph that pins a platform
        // can only run natively on machines of that architecture, so a
        // mixed-architecture fleet has nothing to gain from it - correct, and
        // useless for measuring whether a SECOND MACHINE adds capacity. An
        // unpinned graph genuinely runs anywhere.
        let plat =
            (std::env::var("REBUCK2_LLB_PLATFORM").as_deref() != Ok("any")).then(|| pb::Platform {
                os: "linux".into(),
                architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
                ..Default::default()
            });
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        // Where and how many, so `scripts/fleet.sh` can vary the fan-out
        // without editing this file.
        let out = std::env::var("REBUCK2_LLB_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let n: usize = std::env::var("REBUCK2_LLB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        // How much work each build does. The optimal home:away split is not a
        // property of the machines alone - transfer cost is roughly constant
        // per dispatched build while compute scales with this, so a bigger
        // build makes dispatch relatively cheaper.
        let work: usize = std::env::var("REBUCK2_LLB_WORK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        for i in 0..n {
            let src = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: base_image(),
                    ..Default::default()
                })),
                platform: plat.clone(),
                ..Default::default()
            };
            let src_b = src.encode_to_vec();
            let exec = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&src_b),
                    index: 0,
                }],
                op: Some(pb::op::Op::Exec(pb::ExecOp {
                    meta: Some(pb::Meta {
                        // Distinct work per build, and slow enough that
                        // parallelism would be visible if it happened.
                        args: vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            // CPU-bound, not sleeping. A sleep measures
                            // nothing: one daemon serves four sleeps as
                            // fast as four daemons, so a fleet would look
                            // free when it is not.
                            format!(
                                "i=0; while [ $i -lt {work} ]; do dd if=/dev/zero bs=1M \
                                 count=20 2>/dev/null | sha256sum >/dev/null; \
                                 i=$((i+1)); done; echo task-{i} > /result/task"
                            ),
                        ],
                        cwd: "/".into(),
                        ..Default::default()
                    }),
                    // The result is a SCRATCH mount, not the rootfs.
                    //
                    // Exporting the rootfs to a local directory fails on
                    // `lchownat proc: permission denied` and leaves a
                    // half-written tree, which reads as a dispatch bug and is
                    // not one. It is also 13MB of alpine per build to hash
                    // for a one-line answer.
                    //
                    // The rootfs still declares `output: 0` even though
                    // nothing wants it: on a mount, `output` also decides
                    // WRITABILITY, and `-1` makes runc fail before the
                    // command runs - it cannot create /etc/resolv.conf in a
                    // read-only rootfs. So the interesting output is index 1,
                    // and the terminal op below must say so.
                    mounts: vec![
                        pb::Mount {
                            input: 0,
                            dest: "/".into(),
                            output: 0,
                            ..Default::default()
                        },
                        pb::Mount {
                            input: -1,
                            dest: "/result".into(),
                            output: 1,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                platform: plat.clone(),
                ..Default::default()
            };
            let exec_b = exec.encode_to_vec();
            let term = pb::Op {
                inputs: vec![pb::Input {
                    digest: dg(&exec_b),
                    // Index 1: the scratch result. Index 0 is the rootfs,
                    // which exists only because runc needs somewhere to
                    // write.
                    index: 1,
                }],
                ..Default::default()
            };
            let def = pb::Definition {
                metadata: [&src_b, &exec_b]
                    .iter()
                    .map(|b| (dg(b), pb::OpMetadata::default()))
                    .collect(),
                def: vec![src_b, exec_b, term.encode_to_vec()],
                ..Default::default()
            };
            let path = out.join(format!("rebuck2-fanout-{i}.llb"));
            std::fs::write(&path, def.encode_to_vec()).unwrap();
            println!("[fixture] {}", path.display());
        }
    }

    /// Emit a sample LLB Definition in the wire form `buildctl build` reads
    /// on stdin. A fixture generator, not an assertion:
    ///   cargo test --bin rebuck2 write_sample_llb -- --ignored
    ///   buildctl --addr tcp://... build < /tmp/rebuck2-sample.llb
    ///
    /// Needed because a frontend-by-NAME build (`--frontend dockerfile.v0`)
    /// sends no LLB at all - the frontend runs inside the daemon. Only a
    /// client that constructs LLB itself puts a graph on the wire, which is
    /// what earthly does and what this imitates.
    #[test]
    #[ignore]
    fn write_sample_llb() {
        use prost::Message;
        let path = std::env::temp_dir().join("rebuck2-sample.llb");
        std::fs::write(&path, alpine_exec_definition().encode_to_vec()).unwrap();
        println!(
            "[fixture] {} bytes -> {}",
            std::fs::metadata(&path).unwrap().len(),
            path.display()
        );
    }

    /// The plan's acceptance test for M4, as far as one process can take it:
    /// a subtree is built by a daemon that did not invoke it, the result
    /// lands in the BUILDER's mirror, and the coordinator's disk stays flat.
    ///
    /// Principle 6's test is deliberately blunt and hard to fake - after the
    /// build, look at what is on the driver's disk. If it is out of the data
    /// path the layer is simply not there.
    ///
    ///   docker run -d --privileged -p 11234:1234 moby/buildkit \
    ///     --addr tcp://0.0.0.0:1234
    ///   cargo test --bin rebuck2 a_peer_builds -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn a_peer_builds_it_and_the_driver_holds_nothing() {
        use crate::store::Store;
        use std::sync::Arc;

        // Two separate stores: the worker's mirror, and the "driver" that
        // arbitrates and must end up holding nothing.
        let worker_root = tempfile::tempdir().unwrap().keep();
        let driver_root = tempfile::tempdir().unwrap().keep();
        let worker_store = Arc::new(Store::new(worker_root.clone()).unwrap());
        // Constructed, not merely named: `Store::new` creates cas/, so the
        // emptiness assertion below is about a store that EXISTS and holds
        // nothing. Against a path that was never created, dir_bytes would
        // return 0 and the test would pass vacuously.
        let _driver_store = Arc::new(Store::new(driver_root.clone()).unwrap());
        assert!(driver_root.join("cas").is_dir());

        // 0.0.0.0 so the daemon, which is in a container, can reach it.
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, crate::registry::router(worker_store)).await;
        });
        // The daemon is containerised; loopback there is not this host.
        let registry = format!("host.docker.internal:{port}");

        let def = alpine_exec_definition();
        assert!(crate::dispatch::inspect(&def).dispatchable());

        let got = build_subtree("http://127.0.0.1:11234", &registry, 1, def)
            .await
            .expect("the peer builds it");
        // A DIGEST, not the tag. The tag form used to be accepted here, and
        // that is what let a silent export pass for a publish - see
        // `a_silent_export_is_a_failed_handover_not_a_tag`. If this daemon
        // stops reporting `containerimage.digest`, this test is where that
        // shows up, rather than in a build that fails minutes later.
        assert!(
            got.starts_with("sha256:"),
            "expected a bare digest, got {got}"
        );

        // The result landed in the BUILDER's mirror.
        let worker_bytes = dir_bytes(&worker_root.join("cas"));
        assert!(
            worker_bytes > 0,
            "the builder's mirror is empty - nothing was published"
        );
        println!("[e2e] builder mirror holds {worker_bytes} bytes");

        // And the coordinator holds nothing. This is the whole claim.
        let driver_bytes = dir_bytes(&driver_root.join("cas"));
        assert_eq!(
            driver_bytes, 0,
            "the driver is on the data path - it holds {driver_bytes} bytes"
        );
    }

    /// Total bytes under a directory tree.
    #[cfg(test)]
    fn dir_bytes(root: &std::path::Path) -> u64 {
        let mut total = 0;
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(m) = e.metadata() {
                    total += m.len();
                }
            }
        }
        total
    }

    /// A minimal real subtree: pull alpine, run one exec.
    #[cfg(test)]
    fn alpine_exec_definition() -> pb::Definition {
        use prost::Message;
        let plat = pb::Platform {
            os: "linux".into(),
            architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
            ..Default::default()
        };
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let src = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: base_image(),
                ..Default::default()
            })),
            platform: Some(plat.clone()),
            ..Default::default()
        };
        let src_b = src.encode_to_vec();
        let exec = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&src_b),
                index: 0,
            }],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    args: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "echo dispatched > /out".into(),
                    ],
                    cwd: "/".into(),
                    ..Default::default()
                }),
                mounts: vec![pb::Mount {
                    input: 0,
                    dest: "/".into(),
                    output: 0,
                    ..Default::default()
                }],
                ..Default::default()
            })),
            platform: Some(plat),
            ..Default::default()
        };
        let exec_b = exec.encode_to_vec();
        let term = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&exec_b),
                index: 0,
            }],
            ..Default::default()
        };
        pb::Definition {
            metadata: [&src_b, &exec_b]
                .iter()
                .map(|b| (dg(b), pb::OpMetadata::default()))
                .collect(),
            def: vec![src_b, exec_b, term.encode_to_vec()],
            ..Default::default()
        }
    }

    #[test]
    fn every_solve_is_named_and_no_two_share_a_name() {
        // Buildkit rejects a repeated solve ref. An empty one works once per
        // daemon and then fails forever, so "unset" is not a neutral choice.
        let r = req();
        assert!(!r.r#ref.is_empty(), "an unnamed solve collides with itself");
        let def = pb::Definition::default();
        assert_ne!(
            solve_request(1, def.clone(), "r:1", "").r#ref,
            solve_request(2, def.clone(), "r:1", "").r#ref
        );
        // The one that actually bit: the SAME job, twice. A worker's
        // buildkitd outlives a lap, so job 1 comes round again next run and
        // collides with its own history. Asserting only that different jobs
        // differ let that through.
        assert_ne!(
            solve_request(1, def.clone(), "r:1", "").r#ref,
            solve_request(1, def, "r:1", "").r#ref,
            "the same job twice must not reuse a solve ref"
        );
    }

    #[test]
    fn two_jobs_on_one_worker_do_not_collide() {
        // A worker can be building two offered subtrees at once - `Load`
        // has slots for exactly that - and a shared tag would have the
        // second overwrite the first's result.
        assert_ne!(result_ref("r:5000", 1), result_ref("r:5000", 2));
        assert!(result_ref("r:5000", 42).ends_with("job-42"));
    }

    #[test]
    fn we_never_ask_for_a_privilege_we_refused_to_dispatch() {
        let r = req();
        // dispatch::inspect grounds any subtree wanting insecure exec or
        // host networking, so requesting the entitlement here would be
        // incoherent - and would hand a peer's daemon a privilege on the
        // strength of an offer. Granting privileged exec is a trust
        // decision, not a scheduling one.
        assert!(
            r.entitlements.is_empty(),
            "entitlements requested: {:?}",
            r.entitlements
        );
    }

    #[test]
    fn the_definition_travels_verbatim_and_no_frontend_reinterprets_it() {
        let r = req();
        let def = r.definition.expect("the subtree");
        assert_eq!(
            def.def,
            vec![b"op-bytes".to_vec()],
            "LLB must not be rewritten"
        );

        // Empty frontend: we hand over LLB that has ALREADY been solved
        // into ops by the requester. A frontend here would re-run
        // dockerfile parsing over bytes that are not a Dockerfile.
        assert!(r.frontend.is_empty(), "frontend: {:?}", r.frontend);
        assert!(r.frontend_attrs.is_empty());
        assert!(r.frontend_inputs.is_empty());

        // The session is the caller's; buildkit rejects a solve whose
        // session id it has not seen attached.
        assert_eq!(r.session, "sess-abc");
    }
}

#[cfg(test)]
mod hostbind {
    use super::tests::base_image;
    use super::*;
    use prost::Message;

    /// How is a HOST BIND encoded, and would we notice one?
    ///
    /// `dispatch::hazard` detects cache, secret and ssh mounts, insecure exec
    /// and host networking. It does NOT detect a mount that binds a path from
    /// the worker's filesystem, and the proto has no flag for one - so the
    /// question is which combination of ordinary fields means it.
    ///
    /// Under test: `MountType::Bind` with `input = -1` (no LLB input) and a
    /// non-empty `selector`. If that mounts a real file from the worker, the
    /// detection rule follows from it.
    ///
    ///   BUILDKIT=tcp://127.0.0.1:18372 cargo test --bin rebuck2 hostbind \
    ///     -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn a_host_bind_is_input_minus_one_with_a_selector() {
        let addr = std::env::var("BUILDKIT").expect("set BUILDKIT=tcp://host:port");
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        let src = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: base_image(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let src_b = src.encode_to_vec();
        let exec = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&src_b),
                index: 0,
            }],
            op: Some(pb::op::Op::Exec(pb::ExecOp {
                meta: Some(pb::Meta {
                    args: vec!["/bin/sh".into(), "-c".into(), "test -s /probe".into()],
                    cwd: "/".into(),
                    ..Default::default()
                }),
                mounts: vec![
                    pb::Mount {
                        input: 0,
                        dest: "/".into(),
                        output: 0,
                        ..Default::default()
                    },
                    pb::Mount {
                        input: -1,
                        selector: "/usr/bin/buildctl".into(),
                        dest: "/probe".into(),
                        output: -1,
                        readonly: true,
                        mount_type: pb::MountType::Bind as i32,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let exec_b = exec.encode_to_vec();
        let term = pb::Op {
            inputs: vec![pb::Input {
                digest: dg(&exec_b),
                index: 0,
            }],
            ..Default::default()
        };
        let def = pb::Definition {
            metadata: [&src_b, &exec_b]
                .iter()
                .map(|b| (dg(b), pb::OpMetadata::default()))
                .collect(),
            def: vec![src_b, exec_b, term.encode_to_vec()],
            ..Default::default()
        };
        let mut c = connect(&addr.replace("tcp://", "http://")).await.unwrap();
        let out = c
            .solve(control::SolveRequest {
                r#ref: format!("hostbind-probe-{}", std::process::id()),
                definition: Some(def),
                ..Default::default()
            })
            .await;
        match out {
            Ok(_) => println!("RESULT: bind + input -1 + selector MOUNTED a worker file"),
            Err(e) => println!("RESULT: not a host bind that way - {}", e.message()),
        }
    }
}
