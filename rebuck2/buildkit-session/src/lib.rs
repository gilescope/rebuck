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
//! # The callback rides the connection WE opened
//!
//! The daemon never dials us. It answers down the same stream we opened to
//! it, so serving attachables needs no inbound port, no published address and
//! no hole in anyone's firewall. Verified across two machines: an x86 box on
//! the LAN asked this laptop for a secret and got it.
//!
//! That is what makes this deployable rather than merely correct. A peer
//! behind NAT, a laptop with no routable address, a container that cannot
//! reach its own host - none of it matters, because the direction of the TCP
//! connection and the direction of the gRPC call are opposites.
//!
//! No filesync here, deliberately. A build context can be published as
//! content once and pulled by every peer, which is better than syncing it N
//! times - so the hard half of the protocol is the half we do not need.

use std::pin::Pin;
use std::task::{Context, Poll};

use bollard_buildkit_proto::moby::buildkit::v1 as control;
use futures::StreamExt;
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

/// An `AsyncWrite` that emits one session frame per write.
///
/// Written by hand rather than assembled from `PollSender` + `SinkWriter` +
/// `CopyToBytes`, which is the idiomatic chain and three layers of generics
/// to say "put these bytes in a channel". Thirty lines that can be read is
/// worth more here than a shorter expression that cannot.
struct FrameWriter {
    tx: tokio::sync::mpsc::Sender<Frame>,
}

impl AsyncWrite for FrameWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.tx.try_send(Frame { data: buf.to_vec() }) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // The channel drains on the session task; wake when it has.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "session closed"),
            )),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Each write is already a whole frame on the wire.
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Open a session to a daemon and serve `Secrets` on it for as long as it
/// lasts.
///
/// Returns the session id, which the caller must put in
/// `SolveRequest.session`: the daemon matches the two by that string and by
/// nothing else. A solve naming a session that was never opened fails exactly
/// like a solve naming no session, which is the confusion this signature
/// exists to prevent.
pub async fn serve_secrets<S>(
    channel: tonic::transport::Channel,
    secrets: S,
) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)>
where
    S: bollard_buildkit_proto::moby::buildkit::secrets::v1::secrets_server::Secrets,
{
    use bollard_buildkit_proto::moby::buildkit::secrets::v1::secrets_server::SecretsServer;

    let uuid = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(64);
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);

    let mut req = tonic::Request::new(outbound);
    for (k, v) in session_headers(&uuid, &["/moby.buildkit.secrets.v1.Secrets/GetSecret"]) {
        req.metadata_mut().append(k, v.parse()?);
    }

    let inbound = control::control_client::ControlClient::new(channel)
        .session(req)
        .await?
        .into_inner();

    // The daemon's frames become our read side; ours become its.
    let reader = tokio_util::io::StreamReader::new(inbound.map(|m| {
        m.map(|f| bytes::Bytes::from(f.data))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }));
    let transport = Transport::new(reader, FrameWriter { tx });

    let task = tokio::spawn(async move {
        let served = tonic::transport::Server::builder()
            .add_service(SecretsServer::new(secrets))
            .serve_with_incoming(futures::stream::once(async {
                Ok::<_, std::io::Error>(transport)
            }))
            .await;
        if let Err(e) = served {
            println!("[session] stopped serving: {e}");
        }
    });
    Ok((uuid, task))
}

#[cfg(test)]
mod e2e {
    use super::*;
    use bollard_buildkit_proto::moby::buildkit::secrets::v1 as secrets;

    /// A provider that answers one id and refuses everything else.
    struct One {
        id: String,
        value: Vec<u8>,
    }

    #[tonic::async_trait]
    impl secrets::secrets_server::Secrets for One {
        async fn get_secret(
            &self,
            req: tonic::Request<secrets::GetSecretRequest>,
        ) -> Result<tonic::Response<secrets::GetSecretResponse>, tonic::Status> {
            let asked = &req.get_ref().id;
            println!("[session] daemon asked for secret {asked:?}");
            if *asked == self.id {
                Ok(tonic::Response::new(secrets::GetSecretResponse {
                    data: self.value.clone(),
                }))
            } else {
                Err(tonic::Status::not_found(asked.clone()))
            }
        }
    }

    /// The whole claim, against a real daemon:
    ///
    /// ```text
    /// BUILDKIT=tcp://127.0.0.1:18372 \
    ///   cargo test -p buildkit-session a_daemon_asks_us_for_a_secret -- --ignored --nocapture
    /// ```
    ///
    /// A solve whose exec mounts a secret must succeed BECAUSE the daemon
    /// called back into this process for the value. If the callback never
    /// happens the solve fails with "no active sessions", which is precisely
    /// the failure this crate exists to remove.
    #[tokio::test]
    #[ignore]
    async fn a_daemon_asks_us_for_a_secret() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let addr = std::env::var("BUILDKIT").expect("set BUILDKIT=tcp://host:port");
        let addr = addr.replace("tcp://", "http://");
        let channel = tonic::transport::Endpoint::new(addr)
            .unwrap()
            .connect()
            .await
            .expect("dial buildkitd");

        let (session, _task) = serve_secrets(
            channel.clone(),
            One {
                id: "rebuck2-probe".into(),
                value: b"the-value".to_vec(),
            },
        )
        .await
        .expect("open session");
        println!("[session] opened {session}");

        // alpine + one exec that reads the secret and fails if it is wrong.
        let dg = |b: &[u8]| {
            use sha2::{Digest, Sha256};
            format!("sha256:{:x}", Sha256::digest(b))
        };
        let src = pb::Op {
            op: Some(pb::op::Op::Source(pb::SourceOp {
                identifier: "docker-image://docker.io/library/alpine:3.20".into(),
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
                    args: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "test \"$(cat /run/secrets/probe)\" = the-value".into(),
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
                            id: "rebuck2-probe".into(),
                            mode: 0o444,
                            ..Default::default()
                        }),
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

        let out = control::control_client::ControlClient::new(channel)
            .solve(control::SolveRequest {
                r#ref: format!("session-probe-{}", std::process::id()),
                definition: Some(def),
                session: session.clone(),
                ..Default::default()
            })
            .await;
        match out {
            Ok(_) => println!("[session] SOLVED - the daemon got the secret from us"),
            Err(e) => panic!("solve failed: {e}"),
        }
    }
}
