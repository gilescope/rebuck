//! Serve BuildKit session attachables to a daemon you do not own.
//!
//! A buildkit client does not send secrets, credentials or files to the
//! daemon. It opens `Control.Session`, and over that ONE bidirectional stream
//! it serves gRPC services back down to the daemon, which calls them when a
//! build needs something only the client has:
//!
//! ```text
//! us                                        daemon
//!  |-- Control.Session (bidi BytesMessage) ---->|
//!  |                                            |
//!  |  we SERVE, the daemon CALLS:               |
//!  |    moby.buildkit.secrets.v1.Secrets  <-----|
//!  |    moby.filesync.v1.Auth             <-----|
//!  |    moby.sshforward.v1.SSH            <-----|
//! ```
//!
//! The direction is the whole trick and the reason this crate exists: the
//! daemon dials nothing, and a secret is pulled by the party that needs it
//! rather than pushed by the party that has it.
//!
//! # Why not bollard
//!
//! Bollard implements this and implements it well - including fsutil, which
//! is the genuinely unpleasant part. But `GrpcServer` and its `Driver` trait
//! are `pub(crate)`, so none of it can be reached from outside, and it is
//! Apache-2.0 against this tree's MIT. What is here was written from the
//! protocol rather than from their source: the header names below are facts
//! about buildkit's wire format, not borrowed expression.
//!
//! No filesync here, deliberately. A build context can be published as
//! content once and pulled by every peer, which is better than syncing it N
//! times - so the hard half of the protocol is the half we do not need.

use std::pin::Pin;
use std::task::{Context, Poll};

use bollard_buildkit_proto::moby::buildkit::v1 as control;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// One end of a byte pipe tunnelled inside a stream of `BytesMessage`.
///
/// gRPC needs an ordered, reliable, bidirectional byte stream; a gRPC
/// bidirectional stream of length-delimited blobs is one, if you squint and
/// write this. Reading concatenates the payloads of inbound messages; writing
/// emits one message per `poll_write`.
pub struct Transport {
    read: Pin<Box<dyn AsyncRead + Send>>,
    write: Pin<Box<dyn AsyncWrite + Send>>,
}

impl Transport {
    /// Build a transport from the two halves of an already-open session.
    ///
    /// `inbound` is what the daemon sends us; `outbound` is where our bytes
    /// go. Kept generic so the same type serves a session we dialled and a
    /// session someone handed us.
    pub fn new(
        inbound: impl AsyncRead + Send + 'static,
        outbound: impl AsyncWrite + Send + 'static,
    ) -> Self {
        Self {
            read: Box::pin(inbound),
            write: Box::pin(outbound),
        }
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.read.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.write.as_mut().poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.write.as_mut().poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.write.as_mut().poll_shutdown(cx)
    }
}

impl tonic::transport::server::Connected for Transport {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

/// The headers a daemon needs to route calls back to this session.
///
/// `uuid` names the session; every `grpc-method` we advertise tells the
/// daemon that this session can answer that method. Advertise nothing and the
/// daemon will never call us - the session attaches, the build runs, and the
/// first secret mount fails as though no session existed at all.
pub fn session_headers(uuid: &str, methods: &[&str]) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("x-docker-expose-session-uuid", uuid.to_owned()),
        ("x-docker-expose-session-name", "rebuck2".to_owned()),
        ("x-docker-expose-session-sharedkey", String::new()),
    ];
    for m in methods {
        h.push(("x-docker-expose-session-grpc-method", (*m).to_owned()));
    }
    h
}

/// The wire message a session is made of. Re-exported so callers need not
/// depend on the proto crate to hold one.
pub type Frame = control::BytesMessage;

#[cfg(test)]
mod tests {
    use super::*;
    use bollard_buildkit_proto::moby::buildkit::secrets::v1 as secrets;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A byte pipe is a byte pipe, whichever end you hold.
    #[tokio::test]
    async fn a_transport_carries_bytes_both_ways() {
        let (a_in, mut b_out) = tokio::io::duplex(64);
        let (b_in, a_out) = tokio::io::duplex(64);
        let mut t = Transport::new(a_in, a_out);

        t.write_all(b"ping").await.unwrap();
        t.flush().await.unwrap();
        let mut got = [0u8; 4];
        // b_in is the far end of a_out.
        let mut far = b_in;
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");

        b_out.write_all(b"pong").await.unwrap();
        let mut back = [0u8; 4];
        t.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"pong");
    }

    /// Every method we advertise must appear, because a method we forget is
    /// a method the daemon will never call - and the failure looks exactly
    /// like having no session.
    #[test]
    fn headers_advertise_every_method() {
        let h = session_headers(
            "abc",
            &[
                "/moby.buildkit.secrets.v1.Secrets/GetSecret",
                "/moby.filesync.v1.Auth/Credentials",
            ],
        );
        let advertised: Vec<&String> = h
            .iter()
            .filter(|(k, _)| *k == "x-docker-expose-session-grpc-method")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(advertised.len(), 2);
        assert!(h
            .iter()
            .any(|(k, v)| *k == "x-docker-expose-session-uuid" && v == "abc"));
    }

    /// The proto we need is present and names the method we will advertise.
    #[test]
    fn the_secrets_service_is_available() {
        assert_eq!(
            <secrets::secrets_server::SecretsServer<Dummy> as tonic::server::NamedService>::NAME,
            "moby.buildkit.secrets.v1.Secrets"
        );
    }

    #[derive(Default)]
    struct Dummy;

    #[tonic::async_trait]
    impl secrets::secrets_server::Secrets for Dummy {
        async fn get_secret(
            &self,
            _: tonic::Request<secrets::GetSecretRequest>,
        ) -> Result<tonic::Response<secrets::GetSecretResponse>, tonic::Status> {
            Err(tonic::Status::not_found("no"))
        }
    }
}
