//! BuildKit's GATEWAY wire - `moby.buildkit.v1.frontend.LLBBridge`.
//!
//! This is where the LLB actually is. A buildkit client driving a build
//! (`client.Build`, which is what buildctl and earthly both use) creates its
//! gateway client with `NewLLBBridgeClient(c.conn)` — the SAME connection it
//! speaks Control on. So `LLBBridge.Solve`, which carries a `Definition`,
//! arrives beside `Control.Solve`, which does not.
//!
//! An earlier round concluded the opposite - that a Control-layer proxy
//! could not see the graph - on the strength of `Control.Solve` arriving
//! with no definition. That was the right observation and the wrong
//! inference: the definition was not missing, it was on the other service,
//! and the `Unimplemented` we blamed on session relaying was simply this
//! service not being served.
//!
//! Generated from buildkit's `gateway.proto` by `scripts/gen-gateway.sh`
//! and COMMITTED, because generating it needs a buildkit checkout and a
//! hand-built include tree - neither of which belongs in a build.
//!
//! `extern_path` points every shared message at `bollard_buildkit_proto`,
//! so the `Definition` arriving here is the SAME type
//! [`crate::dispatch::inspect`] takes. Two copies of `pb` would compile and
//! then refuse to talk to each other.

#![allow(clippy::all)]

// Layout mirrors the proto packages: prost emits `super::apicaps` from
// inside `moby.buildkit.v1.frontend`, so apicaps has to be its SIBLING.
pub mod apicaps {
    include!("generated/moby.buildkit.v1.apicaps.rs");
}
pub mod vtproto {
    include!("generated/vtproto.rs");
}
pub mod frontend {
    include!("generated/moby.buildkit.v1.frontend.rs");
}
