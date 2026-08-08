//! Asking a buildkitd to build a subtree someone else offered us.
//!
//! Two halves, deliberately separated. Building the [`SolveRequest`] is pure
//! and is where the mistakes live — a wrong exporter sends the result to the
//! wrong place, a stray entitlement grants a privilege we refused to
//! dispatch. Talking to the daemon is I/O and cannot be honestly unit-tested;
//! it wants the e2e rig and a live buildkitd.
//!
//! The result is exported by PUSHING to this worker's own loopback registry
//! (`crate::registry`). That is principle 6 as a mechanism: the layers land
//! where a peer can fetch them directly, and the driver — which arbitrates
//! the offer — carries none of them. The test for it is deliberately blunt:
//! after the build, look at the driver's disk.

use std::collections::HashMap;

use bollard_buildkit_proto::moby::buildkit::v1 as control;
use bollard_buildkit_proto::pb;

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
