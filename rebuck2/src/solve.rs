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
pub fn result_ref(registry: &str, job: u64) -> String {
    format!("{registry}/rebuck2/subtree:job-{job}")
}

/// The request that builds `def` and publishes it where a peer can get it.
pub fn solve_request(
    job: u64,
    def: pb::Definition,
    registry: &str,
    session: &str,
) -> control::SolveRequest {
    let mut attrs = HashMap::new();
    attrs.insert("name".to_owned(), result_ref(registry, job));
    // Push, or the result stays in this daemon's own cache and no peer can
    // have it. The export IS the handover.
    attrs.insert("push".to_owned(), "true".to_owned());
    // Plain HTTP: the mirror has no TLS and no auth, which is precisely why
    // it is bound to loopback. Without this buildkit attempts https and the
    // push dies on a certificate nobody issued.
    attrs.insert("registry.insecure".to_owned(), "true".to_owned());

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
pub async fn connect(
    addr: &str,
) -> anyhow::Result<control::control_client::ControlClient<tonic::transport::Channel>> {
    Ok(control::control_client::ControlClient::connect(addr.to_owned()).await?)
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
    use prost::Message;

    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: format!("local://{local_name}"),
            attrs: [("local.session".to_owned(), session.to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
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
    let def = pb::Definition {
        metadata: [(
            format!("sha256:{}", crate::store::sha256_hex(&src_b)),
            pb::OpMetadata::default(),
        )]
        .into_iter()
        .collect(),
        def: vec![src_b, term.encode_to_vec()],
        ..Default::default()
    };

    // Named by the session, so two concurrent builds do not publish over
    // each other, and a rebuild of the same context is the same ref.
    let name = format!("{registry}/rebuck2/context:{session}-{local_name}");
    let mut attrs = HashMap::new();
    attrs.insert("name".to_owned(), name.clone());
    attrs.insert("push".to_owned(), "true".to_owned());
    attrs.insert("registry.insecure".to_owned(), "true".to_owned());

    let mut c = connect(bk_addr).await?;
    c.solve(control::SolveRequest {
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
    Ok(format!("docker-image://{name}"))
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
) -> anyhow::Result<String> {
    use prost::Message;
    // A one-op graph: fetch it, export it. No exec, so nothing is built -
    // this is a copy with extra steps, and the extra steps are what let the
    // daemon with the credentials do the fetching.
    let src = pb::Op {
        op: Some(pb::op::Op::Source(pb::SourceOp {
            identifier: reference.to_owned(),
            ..Default::default()
        })),
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

    let tag = &crate::store::sha256_hex(reference.as_bytes())[..32];
    let name = format!("{registry}/rebuck2/base:{tag}");
    let mut attrs = HashMap::new();
    attrs.insert("name".to_owned(), name.clone());
    attrs.insert("push".to_owned(), "true".to_owned());
    attrs.insert("registry.insecure".to_owned(), "true".to_owned());

    let mut c = connect(bk_addr).await?;
    c.solve(control::SolveRequest {
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
    Ok(format!("docker-image://{name}"))
}

/// Build a portable graph on a peer and publish the result.
///
/// Returns the image reference the requester should import. This is the
/// first half of adoption: the peer does the work and puts the answer
/// somewhere content-addressed, because its own refs and jobs cannot leave
/// it.
///
/// The tag is derived from the GRAPH, so the same work adopted twice names
/// the same image - which makes a repeat a registry hit rather than a
/// second build.
pub async fn build_and_publish(
    peer_addr: &str,
    registry: &str,
    def: pb::Definition,
) -> anyhow::Result<String> {
    use prost::Message;
    let mut bytes: Vec<u8> = Vec::new();
    for op in &def.def {
        bytes.extend_from_slice(op);
    }
    let tag = &crate::store::sha256_hex(&bytes)[..32];
    let name = format!("{registry}/rebuck2/adopted:{tag}");

    let mut attrs = HashMap::new();
    attrs.insert("name".to_owned(), name.clone());
    attrs.insert("push".to_owned(), "true".to_owned());
    attrs.insert("registry.insecure".to_owned(), "true".to_owned());

    let mut c = connect(peer_addr).await?;
    c.solve(control::SolveRequest {
        r#ref: format!(
            "rebuck2-adopt.{}.{}",
            std::process::id(),
            SOLVE_SEQ.fetch_add(1, Ordering::Relaxed)
        ),
        definition: Some(def),
        // Both forms: an older daemon reads only the deprecated one and
        // would otherwise export nothing while reporting success.
        exporter_deprecated: "image".to_owned(),
        exporter_attrs_deprecated: attrs.clone(),
        exporters: vec![control::Exporter {
            r#type: "image".to_owned(),
            attrs,
        }],
        ..Default::default()
    })
    .await
    .map_err(|e| anyhow::anyhow!("peer solve: {} {}", e.code(), e.message()))?;
    Ok(format!("docker-image://{name}"))
}

/// Build an offered subtree and publish it where a peer can fetch it.
///
/// Returns the ref the requester pulls. Everything here is I/O: the dial,
/// the solve, and the push the exporter performs. The DECISIONS - may it
/// travel, is it worth sending, will this worker take it - were all made
/// before we got here, by `crate::dispatch`.
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
    c.solve(solve_request(job, def, registry, ""))
        .await
        .map_err(|e| anyhow::anyhow!("solve: {} {}", e.code(), e.message()))?;
    Ok(result_ref(registry, job))
}

#[cfg(test)]
mod tests {
    use super::*;

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
                identifier: "docker-image://docker.io/library/alpine:3.20".into(),
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
                identifier: "docker-image://docker.io/library/alpine:3.20".into(),
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

    /// Emit N distinct plain-LLB builds - no frontend, no secrets, no host
    /// binds. What a client that is not earthly sends.
    ///
    ///   cargo test --bin rebuck2 write_fanout_llb -- --ignored --nocapture
    #[test]
    #[ignore]
    fn write_fanout_llb() {
        use prost::Message;
        let plat = pb::Platform {
            os: "linux".into(),
            architecture: std::env::consts::ARCH.replace("aarch64", "arm64"),
            ..Default::default()
        };
        let dg = |b: &[u8]| format!("sha256:{}", crate::store::sha256_hex(b));
        for i in 0..4 {
            let src = pb::Op {
                op: Some(pb::op::Op::Source(pb::SourceOp {
                    identifier: "docker-image://docker.io/library/alpine:3.20".into(),
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
                                "i=0; while [ $i -lt 90 ]; do dd if=/dev/zero bs=1M \
                                 count=20 2>/dev/null | sha256sum >/dev/null; \
                                 i=$((i+1)); done; echo task-{i} > /out"
                            ),
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
                platform: Some(plat.clone()),
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
            let path = std::env::temp_dir().join(format!("rebuck2-fanout-{i}.llb"));
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
        assert_eq!(got, result_ref(&registry, 1));

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
                identifier: "docker-image://docker.io/library/alpine:3.20".into(),
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
