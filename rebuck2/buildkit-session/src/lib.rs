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

    /// Both id shapes clients actually send.
    #[test]
    fn an_id_names_its_environment_variable() {
        assert_eq!(
            EnvSecrets::var_for("name=npm_token&org=&project=&v=1"),
            "npm_token"
        );
        assert_eq!(EnvSecrets::var_for("npm_token"), "npm_token");
        // No `name=` key, so the id is the name - not the first pair.
        assert_eq!(EnvSecrets::var_for("org=acme&v=1"), "org=acme&v=1");
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
    serve(channel, secrets, false).await
}

/// Serve secrets, and optionally this process's ssh agent.
///
/// One function rather than two because the ADVERTISED METHODS and the
/// services must agree: a method advertised but not served makes the daemon
/// call something that is not there, and a service served but not advertised
/// is never called at all. Both failures look like "no active sessions" from
/// the build, which is the least informative error in this system.
pub async fn serve<S>(
    channel: tonic::transport::Channel,
    secrets: S,
    forward_agent: bool,
) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)>
where
    S: bollard_buildkit_proto::moby::buildkit::secrets::v1::secrets_server::Secrets,
{
    use bollard_buildkit_proto::moby::buildkit::secrets::v1::secrets_server::SecretsServer;
    use bollard_buildkit_proto::moby::sshforward::v1::ssh_server::SshServer;

    let uuid = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(64);
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);

    let mut methods = vec!["/moby.buildkit.secrets.v1.Secrets/GetSecret"];
    if forward_agent {
        methods.push("/moby.sshforward.v1.SSH/CheckAgent");
        methods.push("/moby.sshforward.v1.SSH/ForwardAgent");
    }
    let mut req = tonic::Request::new(outbound);
    for (k, v) in session_headers(&uuid, &methods) {
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
        // Registered only when advertised: an advertised method with no
        // service behind it is a call into nothing.
        let mut router =
            tonic::transport::Server::builder().add_service(SecretsServer::new(secrets));
        if forward_agent {
            router = router.add_service(SshServer::new(AgentForward));
        }
        let served = router
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

    /// The agent, forwarded to a daemon that has none of its own.
    ///
    /// `ssh-add -l` inside the exec talks to a socket buildkit created,
    /// which is wired back through the session to this process's agent. A
    /// non-zero exit means no agent answered; exit 0 means it did and listed
    /// keys. Either way the point is that something on the other end of the
    /// socket replied.
    ///
    /// ```text
    /// BUILDKIT=tcp://127.0.0.1:18372 cargo test -p buildkit-session \
    ///   the_daemon_reaches_our_ssh_agent -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore]
    async fn the_daemon_reaches_our_ssh_agent() {
        use bollard_buildkit_proto::pb;
        use prost::Message;

        let addr = std::env::var("BUILDKIT").expect("set BUILDKIT=tcp://host:port");
        let channel = tonic::transport::Endpoint::new(addr.replace("tcp://", "http://"))
            .unwrap()
            .connect()
            .await
            .expect("dial buildkitd");
        let (session, _task) = serve(
            channel.clone(),
            One {
                id: String::new(),
                value: vec![],
            },
            true,
        )
        .await
        .expect("open session");

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
                        // Not just "a socket exists" - the agent must
                        // ANSWER. ssh-add exits 2 when it cannot reach one,
                        // 1 when it reaches an agent holding no keys, 0 when
                        // it lists some. Anything but 2 proves the far end
                        // of that socket is our agent and not a dead file.
                        "apk add --no-cache openssh-client >/dev/null 2>&1; \
                         ssh-add -l; test $? -ne 2"
                            .into(),
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
        // `ignore_cache` on the exec, or this probe tests nothing after the
        // first run. Measured the hard way: with the agent removed it still
        // reported success, in 0.19s, because buildkit served the cached
        // result of the previous run. A probe that passes when the thing it
        // probes is absent is worse than no probe.
        let def = pb::Definition {
            metadata: [(&src_b, false), (&exec_b, true)]
                .iter()
                .map(|(b, ignore)| {
                    (
                        dg(b),
                        pb::OpMetadata {
                            ignore_cache: *ignore,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            def: vec![src_b, exec_b, term.encode_to_vec()],
            ..Default::default()
        };
        let out = control::control_client::ControlClient::new(channel)
            .solve(control::SolveRequest {
                r#ref: format!("ssh-probe-{}", std::process::id()),
                definition: Some(def),
                session,
                ..Default::default()
            })
            .await;
        match out {
            Ok(_) => println!("RESULT: the daemon got a working agent socket from us"),
            Err(e) => println!("RESULT: no agent reached the build - {}", e.message()),
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

/// Resolve secrets from this process's environment.
///
/// A buildkit secret id is an opaque string, and clients put structure in it:
/// earthly sends `name=<x>&org=&project=&v=1`, buildctl sends the bare name.
/// So: take `name` from the query if the id parses as one, otherwise take the
/// id whole, and look up that environment variable - exact case first, then
/// upper - because `--secret id=npm_token` conventionally means `NPM_TOKEN`.
///
/// Deliberately no fallback to a file, a keychain or a cloud store. Every
/// source added here is another place a secret can come from that the person
/// running the fleet did not think about.
#[derive(Debug, Default, Clone)]
pub struct EnvSecrets;

impl EnvSecrets {
    /// The environment variable an id names, if any.
    ///
    /// Split out so it can be tested without setting process environment,
    /// which is global and makes tests order-dependent.
    pub fn var_for(id: &str) -> String {
        for pair in id.split('&') {
            if let Some(name) = pair.strip_prefix("name=") {
                return name.to_owned();
            }
        }
        id.to_owned()
    }
}

#[tonic::async_trait]
impl bollard_buildkit_proto::moby::buildkit::secrets::v1::secrets_server::Secrets for EnvSecrets {
    async fn get_secret(
        &self,
        req: tonic::Request<bollard_buildkit_proto::moby::buildkit::secrets::v1::GetSecretRequest>,
    ) -> Result<
        tonic::Response<bollard_buildkit_proto::moby::buildkit::secrets::v1::GetSecretResponse>,
        tonic::Status,
    > {
        let id = req.get_ref().id.clone();
        let name = Self::var_for(&id);
        // Empty is not set. A variable that exists and holds nothing is a
        // provisioning mistake, and serving it turns that mistake into a
        // build failure on someone else's machine.
        let pick = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        let found = pick(&name).or_else(|| pick(&name.to_uppercase()));
        match found.ok_or(()) {
            // The VALUE is never logged, here or anywhere. The id is enough
            // to debug with and is already in the graph.
            Ok(v) => Ok(tonic::Response::new(
                bollard_buildkit_proto::moby::buildkit::secrets::v1::GetSecretResponse {
                    data: v.into_bytes(),
                },
            )),
            Err(()) => {
                println!("[session] no environment secret for {name:?} (id {id:?})");
                Err(tonic::Status::not_found(name))
            }
        }
    }
}

/// Forward this process's ssh agent to a daemon that asks for one.
///
/// The sharpest thing in this crate. A secret is a VALUE - handing one over
/// gives the peer that string and nothing more. An agent is a CAPABILITY:
/// for as long as the build runs, whoever holds the socket can sign with
/// your key, and nothing in the protocol constrains what they sign. Off
/// unless explicitly asked for, and worth a second thought even then.
///
/// `SSH_AUTH_SOCK` is read at call time rather than cached, so unsetting it
/// stops the forwarding rather than being ignored.
#[derive(Debug, Default, Clone)]
pub struct AgentForward;

#[tonic::async_trait]
impl bollard_buildkit_proto::moby::sshforward::v1::ssh_server::Ssh for AgentForward {
    async fn check_agent(
        &self,
        _: tonic::Request<bollard_buildkit_proto::moby::sshforward::v1::CheckAgentRequest>,
    ) -> Result<
        tonic::Response<bollard_buildkit_proto::moby::sshforward::v1::CheckAgentResponse>,
        tonic::Status,
    > {
        match std::env::var("SSH_AUTH_SOCK") {
            Ok(p) if !p.is_empty() => Ok(tonic::Response::new(Default::default())),
            _ => Err(tonic::Status::not_found("no SSH_AUTH_SOCK")),
        }
    }

    type ForwardAgentStream = std::pin::Pin<
        Box<
            dyn futures::Stream<
                    Item = Result<
                        bollard_buildkit_proto::moby::sshforward::v1::BytesMessage,
                        tonic::Status,
                    >,
                > + Send,
        >,
    >;

    async fn forward_agent(
        &self,
        req: tonic::Request<
            tonic::Streaming<bollard_buildkit_proto::moby::sshforward::v1::BytesMessage>,
        >,
    ) -> Result<tonic::Response<Self::ForwardAgentStream>, tonic::Status> {
        use bollard_buildkit_proto::moby::sshforward::v1::BytesMessage as Ssh;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let path = std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| tonic::Status::not_found("no SSH_AUTH_SOCK"))?;
        let sock = tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| tonic::Status::unavailable(format!("{path}: {e}")))?;
        let (mut rd, mut wr) = tokio::io::split(sock);

        // Daemon -> agent. Its own task: the two directions of an agent
        // conversation are not lockstep, and serialising them deadlocks on
        // the first reply that arrives before the next request is sent.
        let mut inbound = req.into_inner();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                if wr.write_all(&msg.data).await.is_err() {
                    break;
                }
            }
        });

        // Agent -> daemon.
        let out = async_stream::stream! {
            let mut buf = vec![0u8; 8 * 1024];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => yield Ok(Ssh { data: buf[..n].to_vec() }),
                }
            }
        };
        Ok(tonic::Response::new(Box::pin(out)))
    }
}
