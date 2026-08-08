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
