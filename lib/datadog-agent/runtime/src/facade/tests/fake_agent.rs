//! A fake Datadog Agent that serves just enough of the IPC API for a remote agent to start in connected mode.
//!
//! It accepts registrations, sends an empty configuration snapshot on each configuration stream, and holds the tagger,
//! workloadmeta, and autodiscovery streams open. Every other method is unimplemented.

use std::{
    convert::Infallible,
    future::ready,
    sync::{Arc, Mutex},
};

use datadog_agent_commons::ipc::{
    config::{IpcAuthConfiguration, RemoteAgentClientConfiguration},
    tls::build_ipc_server_tls_config,
};
use datadog_protos::agent::{
    config_event,
    v1::{RegisterRemoteAgentRequest, RegisterRemoteAgentResponse},
    ConfigEvent, ConfigSnapshot, HostnameReply,
};
use futures::{stream, StreamExt as _};
use hyper::{body::Incoming, server::conn::http2, service::service_fn, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use saluki_tls::test_util::SelfSignedCert;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tonic::{body::Body, server::Grpc, Status};
use tonic_prost::{prost::Message, ProstCodec};

pub const FAKE_AGENT_HOSTNAME: &str = "fake-agent-host";

type Registrations = Arc<Mutex<Vec<RegisterRemoteAgentRequest>>>;

pub struct FakeAgent {
    client_config: RemoteAgentClientConfiguration,
    registrations: Registrations,
    _ipc_dir: TempDir,
}

impl FakeAgent {
    /// Starts serving on a task of the current runtime. The default cryptography provider must already be installed.
    pub async fn start() -> Self {
        let ipc_dir = tempfile::tempdir().expect("IPC directory should be created");
        let cert = SelfSignedCert::localhost();
        let cert_path = ipc_dir.path().join("ipc_cert.pem");
        std::fs::write(&cert_path, format!("{}{}", cert.cert_pem(), cert.key_pem()))
            .expect("IPC certificate should be written");
        let auth_token_path = ipc_dir.path().join("auth_token");
        std::fs::write(&auth_token_path, "fake-agent-token").expect("auth token should be written");

        let mut tls_config = build_ipc_server_tls_config(&cert_path)
            .await
            .expect("server TLS configuration should be built");
        tls_config.alpn_protocols = vec![b"h2".to_vec()];

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should be bound");
        let port = listener.local_addr().expect("listener should have an address").port();
        let registrations = Registrations::default();
        tokio::spawn(serve(
            listener,
            TlsAcceptor::from(Arc::new(tls_config)),
            Arc::clone(&registrations),
        ));

        let auth = IpcAuthConfiguration::new(auth_token_path, cert_path);
        let client_config = RemoteAgentClientConfiguration::from_parts("127.0.0.1", port, auth, 4 * 1024 * 1024, "")
            .expect("client configuration should be valid");

        Self {
            client_config,
            registrations,
            _ipc_dir: ipc_dir,
        }
    }

    pub fn client_configuration(&self) -> RemoteAgentClientConfiguration {
        self.client_config.clone()
    }

    pub fn registrations(&self) -> Vec<RegisterRemoteAgentRequest> {
        self.registrations.lock().unwrap().clone()
    }
}

async fn serve(listener: TcpListener, acceptor: TlsAcceptor, registrations: Registrations) {
    loop {
        let (stream, _) = listener.accept().await.expect("fake Agent should accept connections");
        let acceptor = acceptor.clone();
        let registrations = Arc::clone(&registrations);
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(stream).await else {
                return;
            };
            let handler = service_fn(move |request| {
                let registrations = Arc::clone(&registrations);
                async move { Ok::<_, Infallible>(handle(request, registrations).await) }
            });
            let _ = http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), handler)
                .await;
        });
    }
}

async fn handle(request: Request<Incoming>, registrations: Registrations) -> Response<Body> {
    match request.uri().path() {
        // `RemoteAgentClient::connect` checks each new connection with this call.
        "/datadog.api.v1.AgentSecure/TaggerFetchEntity" => unary(request, |()| ()).await,
        "/datadog.api.v1.Agent/GetHostname" => {
            unary(request, |()| HostnameReply {
                hostname: FAKE_AGENT_HOSTNAME.to_string(),
            })
            .await
        }
        "/datadog.remoteagent.v1.RemoteAgent/RegisterRemoteAgent" => {
            unary(request, move |registration: RegisterRemoteAgentRequest| {
                registrations.lock().unwrap().push(registration);
                RegisterRemoteAgentResponse {
                    session_id: "fake-session".to_string(),
                    recommended_refresh_interval_secs: 60,
                }
            })
            .await
        }
        "/datadog.api.v1.AgentSecure/StreamConfigEvents" => {
            let snapshot = ConfigEvent {
                event: Some(config_event::Event::Snapshot(ConfigSnapshot::default())),
            };
            stream_then_hold(request, vec![snapshot]).await
        }
        "/datadog.api.v1.AgentSecure/TaggerStreamEntities"
        | "/datadog.api.v1.AgentSecure/WorkloadmetaStreamEntities"
        | "/datadog.api.v1.AgentSecure/AutodiscoveryStreamConfig" => stream_then_hold::<()>(request, Vec::new()).await,
        _ => Status::unimplemented("not served by the fake Datadog Agent").into_http(),
    }
}

async fn unary<Req, Resp>(
    request: Request<Incoming>, mut handler: impl FnMut(Req) -> Resp + Send + 'static,
) -> Response<Body>
where
    Req: Message + Default + Send + 'static,
    Resp: Message + Send + 'static,
{
    let service = tower::service_fn(move |request: tonic::Request<Req>| {
        ready(Ok::<_, Status>(tonic::Response::new(handler(request.into_inner()))))
    });
    Grpc::new(ProstCodec::<Resp, Req>::default())
        .unary(service, request)
        .await
}

/// Answers a server-streaming call with `messages`, and then keeps the stream open until the client goes away.
async fn stream_then_hold<Resp>(request: Request<Incoming>, messages: Vec<Resp>) -> Response<Body>
where
    Resp: Message + Clone + Send + 'static,
{
    let service = tower::service_fn(move |_: tonic::Request<()>| {
        let messages = stream::iter(messages.clone().into_iter().map(Ok)).chain(stream::pending());
        ready(Ok::<_, Status>(tonic::Response::new(messages)))
    });
    Grpc::new(ProstCodec::<Resp, ()>::default())
        .server_streaming(service, request)
        .await
}
