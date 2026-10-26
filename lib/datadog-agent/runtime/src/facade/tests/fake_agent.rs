//! A fake Datadog Agent that serves just enough of the IPC API for a remote agent to start in connected mode.
//!
//! It accepts registrations, sends one configuration snapshot on each configuration stream, and holds the tagger,
//! workloadmeta, and autodiscovery streams open without sending anything. Every other method is unimplemented.

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
    v1::{
        RefreshRemoteAgentRequest, RefreshRemoteAgentResponse, RegisterRemoteAgentRequest, RegisterRemoteAgentResponse,
    },
    AutodiscoveryStreamResponse, ConfigEvent, ConfigSetting, ConfigSnapshot, ConfigStreamRequest, FetchEntityRequest,
    FetchEntityResponse, HostnameReply, HostnameRequest, StreamTagsRequest, StreamTagsResponse,
    WorkloadmetaStreamRequest, WorkloadmetaStreamResponse,
};
use futures::{stream, StreamExt as _};
use hyper::{body::Incoming, server::conn::http2, service::service_fn, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost_types::{value::Kind, Value};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_rustls::TlsAcceptor;
use tonic::{body::Body, server::Grpc, Status};
use tonic_prost::ProstCodec;

/// The hostname that the fake Agent reports.
pub const FAKE_AGENT_HOSTNAME: &str = "fake-agent-host";

/// A running fake Datadog Agent, which stops serving new connections when dropped.
pub struct FakeAgent {
    client_config: RemoteAgentClientConfiguration,
    registrations: Arc<Mutex<Vec<RegisterRemoteAgentRequest>>>,
    server: JoinHandle<()>,
    _ipc_dir: TempDir,
}

impl FakeAgent {
    /// Starts a fake Agent whose configuration snapshot holds the given string settings.
    ///
    /// The default cryptography provider must already be installed.
    pub async fn start(settings: &[(&str, &str)]) -> Self {
        let ipc_dir = tempfile::tempdir().expect("IPC directory should be created");
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_owned()]).expect("IPC certificate should be generated");
        let cert_path = ipc_dir.path().join("ipc_cert.pem");
        std::fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
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

        let registrations = Arc::new(Mutex::new(Vec::new()));
        let service = FakeAgentService {
            registrations: Arc::clone(&registrations),
            snapshot: ConfigSnapshot {
                origin: "fake-agent".to_string(),
                sequence_id: 1,
                settings: settings
                    .iter()
                    .map(|(key, value)| ConfigSetting {
                        source: "file".to_string(),
                        key: key.to_string(),
                        value: Some(Value {
                            kind: Some(Kind::StringValue(value.to_string())),
                        }),
                    })
                    .collect(),
            },
        };
        let server = tokio::spawn(serve(listener, TlsAcceptor::from(Arc::new(tls_config)), service));

        let auth = IpcAuthConfiguration::new(auth_token_path, cert_path);
        let client_config = RemoteAgentClientConfiguration::from_parts(port, auth, 4 * 1024 * 1024, "")
            .expect("client configuration should be valid");

        Self {
            client_config,
            registrations,
            server,
            _ipc_dir: ipc_dir,
        }
    }

    /// Returns the configuration for connecting to this Agent.
    pub fn client_configuration(&self) -> RemoteAgentClientConfiguration {
        self.client_config.clone()
    }

    /// Returns every registration request received so far.
    pub fn registrations(&self) -> Vec<RegisterRemoteAgentRequest> {
        self.registrations.lock().unwrap().clone()
    }
}

impl Drop for FakeAgent {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn serve(listener: TcpListener, acceptor: TlsAcceptor, service: FakeAgentService) {
    loop {
        let (stream, _) = listener.accept().await.expect("fake Agent should accept connections");
        let acceptor = acceptor.clone();
        let service = service.clone();
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(stream).await else {
                return;
            };
            let handler = service_fn(move |request| {
                let service = service.clone();
                async move { Ok::<_, Infallible>(service.handle(request).await) }
            });
            let _ = http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), handler)
                .await;
        });
    }
}

#[derive(Clone)]
struct FakeAgentService {
    registrations: Arc<Mutex<Vec<RegisterRemoteAgentRequest>>>,
    snapshot: ConfigSnapshot,
}

impl FakeAgentService {
    async fn handle(&self, request: Request<Incoming>) -> Response<Body> {
        match request.uri().path() {
            // `RemoteAgentClient::connect` checks each new connection with this call.
            "/datadog.api.v1.AgentSecure/TaggerFetchEntity" => {
                unary(request, |_: FetchEntityRequest| FetchEntityResponse::default()).await
            }
            "/datadog.api.v1.Agent/GetHostname" => {
                unary(request, |_: HostnameRequest| HostnameReply {
                    hostname: FAKE_AGENT_HOSTNAME.to_string(),
                })
                .await
            }
            "/datadog.remoteagent.v1.RemoteAgent/RegisterRemoteAgent" => {
                let registrations = Arc::clone(&self.registrations);
                unary(request, move |registration: RegisterRemoteAgentRequest| {
                    registrations.lock().unwrap().push(registration);
                    RegisterRemoteAgentResponse {
                        session_id: "fake-session".to_string(),
                        recommended_refresh_interval_secs: 60,
                    }
                })
                .await
            }
            "/datadog.remoteagent.v1.RemoteAgent/RefreshRemoteAgent" => {
                unary(request, |_: RefreshRemoteAgentRequest| RefreshRemoteAgentResponse {}).await
            }
            "/datadog.api.v1.AgentSecure/StreamConfigEvents" => {
                let snapshot = ConfigEvent {
                    event: Some(config_event::Event::Snapshot(self.snapshot.clone())),
                };
                stream_then_hold::<ConfigStreamRequest, _>(request, vec![snapshot]).await
            }
            "/datadog.api.v1.AgentSecure/TaggerStreamEntities" => {
                stream_then_hold::<StreamTagsRequest, StreamTagsResponse>(request, Vec::new()).await
            }
            "/datadog.api.v1.AgentSecure/WorkloadmetaStreamEntities" => {
                stream_then_hold::<WorkloadmetaStreamRequest, WorkloadmetaStreamResponse>(request, Vec::new()).await
            }
            "/datadog.api.v1.AgentSecure/AutodiscoveryStreamConfig" => {
                stream_then_hold::<(), AutodiscoveryStreamResponse>(request, Vec::new()).await
            }
            _ => Status::unimplemented("not served by the fake Datadog Agent").into_http(),
        }
    }
}

async fn unary<Req, Resp>(
    request: Request<Incoming>, mut handler: impl FnMut(Req) -> Resp + Send + 'static,
) -> Response<Body>
where
    Req: prost::Message + Default + Send + 'static,
    Resp: prost::Message + Send + 'static,
{
    let service = tower::service_fn(move |request: tonic::Request<Req>| {
        ready(Ok::<_, Status>(tonic::Response::new(handler(request.into_inner()))))
    });
    Grpc::new(ProstCodec::<Resp, Req>::default())
        .unary(service, request)
        .await
}

/// Answers a server-streaming call with `messages`, and then keeps the stream open until the client goes away.
async fn stream_then_hold<Req, Resp>(request: Request<Incoming>, messages: Vec<Resp>) -> Response<Body>
where
    Req: prost::Message + Default + Send + 'static,
    Resp: prost::Message + Clone + Send + 'static,
{
    let service = tower::service_fn(move |_: tonic::Request<Req>| {
        let messages = stream::iter(messages.clone().into_iter().map(Ok)).chain(stream::pending());
        ready(Ok::<_, Status>(tonic::Response::new(messages)))
    });
    Grpc::new(ProstCodec::<Resp, Req>::default())
        .server_streaming(service, request)
        .await
}
