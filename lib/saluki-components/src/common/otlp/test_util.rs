//! Fake upstream receivers, handlers, and clients for tests of the OTLP servers.

#[cfg(unix)]
use std::path::Path;
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::response::Response;
use axum::Router;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};
use http_body::Frame;
use http_body_util::{BodyExt as _, Full, StreamBody};
use rustls::RootCertStore;
use saluki_core::accounting::MemoryLimiter;
use saluki_core::runtime::{
    state::{DataspaceUpdate, IdentifierFilter},
    test_util::TestSupervisor,
};
use saluki_error::GenericError;
use saluki_io::net::client::http::{HttpClient, HttpProtocol};
use saluki_io::net::{BoundListenAddress, ListenAddress};
use saluki_metrics::MetricsBuilder;
use tokio::net::TcpListener;

use super::proxy::{OtlpProxy, OtlpProxyDestinations};
use super::{Metrics, OtlpHandler, OtlpServerConfiguration, Signal};
use crate::common::datadog::test_util::init_crypto_provider;
use crate::common::proxy::Destination;

/// A request that a fake upstream receiver got.
#[derive(Clone, Debug)]
pub(super) struct SeenRequest {
    pub(super) method: Method,
    pub(super) path_and_query: String,
    pub(super) version: Version,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
}

type Respond = Arc<dyn Fn(&SeenRequest) -> Response + Send + Sync>;

/// A fake upstream OTLP receiver that records each request and answers it with a responder.
///
/// It serves HTTP/1.1 and HTTP/2 with prior knowledge, like the receivers that the proxy forwards to.
pub(super) struct FakeUpstream {
    pub(super) destination: Destination,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl FakeUpstream {
    /// Starts a fake upstream on an ephemeral loopback port.
    pub(super) async fn tcp<F>(respond: F) -> Self
    where
        F: Fn(&SeenRequest) -> Response + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let port = listener.local_addr().expect("listener should have an address").port();
        let (router, seen) = recording_router(Arc::new(respond));
        tokio::spawn(async move { axum::serve(listener, router).await });

        Self {
            destination: Destination::Tcp {
                host: "127.0.0.1".to_string(),
                port,
            },
            seen,
        }
    }

    /// Starts a fake upstream on a Unix domain socket at `path`.
    #[cfg(unix)]
    pub(super) async fn unix<F>(path: &Path, respond: F) -> Self
    where
        F: Fn(&SeenRequest) -> Response + Send + Sync + 'static,
    {
        let listener = tokio::net::UnixListener::bind(path).expect("should bind the socket");
        let (router, seen) = recording_router(Arc::new(respond));
        tokio::spawn(async move { axum::serve(listener, router).await });

        Self {
            destination: Destination::Unix(path.to_path_buf()),
            seen,
        }
    }

    /// Returns the requests received so far, in order.
    pub(super) fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("lock should not be poisoned").clone()
    }
}

fn recording_router(respond: Respond) -> (Router, Arc<Mutex<Vec<SeenRequest>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new().fallback({
        let seen = Arc::clone(&seen);
        move |request: Request| {
            let seen = Arc::clone(&seen);
            let respond = Arc::clone(&respond);
            async move {
                let (parts, body) = request.into_parts();
                let body = body
                    .collect()
                    .await
                    .expect("request body should be readable")
                    .to_bytes();
                let request = SeenRequest {
                    method: parts.method,
                    path_and_query: parts.uri.path_and_query().map(ToString::to_string).unwrap_or_default(),
                    version: parts.version,
                    headers: parts.headers,
                    body,
                };
                let response = respond(&request);
                seen.lock().expect("lock should not be poisoned").push(request);
                response
            }
        }
    });
    (router, seen)
}

/// Returns a header map with `headers`.
pub(super) fn header_map(headers: &[(&'static str, &str)]) -> HeaderMap {
    headers
        .iter()
        .map(|(name, value)| {
            (
                HeaderName::from_static(name),
                HeaderValue::from_str(value).expect("header value should be valid"),
            )
        })
        .collect()
}

/// Returns `message` in a gRPC length-prefixed message frame, uncompressed.
pub(super) fn grpc_frame(message: &[u8]) -> Bytes {
    let length = u32::try_from(message.len()).expect("message should fit a gRPC frame");
    let mut frame = Vec::with_capacity(5 + message.len());
    frame.push(0);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(message);
    frame.into()
}

/// Returns a gRPC response with `headers`, then `data`, if any, then `trailers`.
pub(super) fn grpc_response(headers: &[(&'static str, &str)], data: Option<Bytes>, trailers: HeaderMap) -> Response {
    let frames = data
        .map(Frame::data)
        .into_iter()
        .chain([Frame::trailers(trailers)])
        .map(Ok::<_, Infallible>);
    let mut response = Response::new(Body::new(StreamBody::new(futures::stream::iter(frames))));
    *response.headers_mut() = header_map(headers);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
    response
}

/// Returns a successful gRPC response with an empty message, which decodes as any default export response.
pub(super) fn grpc_ok(_: &SeenRequest) -> Response {
    grpc_response(&[], Some(grpc_frame(&[])), header_map(&[("grpc-status", "0")]))
}

/// Returns an empty successful HTTP response.
pub(super) fn http_ok(_: &SeenRequest) -> Response {
    Response::new(Body::empty())
}

/// Returns a proxy to `grpc` and `http`, with counters that no recorder sees.
pub(super) fn proxy_to(grpc: &FakeUpstream, http: &FakeUpstream) -> OtlpProxy {
    init_crypto_provider();
    let destinations = OtlpProxyDestinations {
        grpc: Some(grpc.destination.clone()),
        http: Some(http.destination.clone()),
    };
    OtlpProxy::new(&destinations, MetricsBuilder::default()).expect("proxy should build")
}

/// An OTLP handler that records which signals it handled.
#[derive(Clone, Default)]
pub(super) struct RecordingHandler {
    handled: Arc<Mutex<Vec<Signal>>>,
}

impl RecordingHandler {
    /// Returns how many requests of `signal` the handler handled.
    pub(super) fn count(&self, signal: Signal) -> usize {
        let handled = self.handled.lock().expect("lock should not be poisoned");
        handled.iter().filter(|handled| **handled == signal).count()
    }

    fn record(&self, signal: Signal) -> Result<(), GenericError> {
        self.handled.lock().expect("lock should not be poisoned").push(signal);
        Ok(())
    }
}

#[async_trait]
impl OtlpHandler for RecordingHandler {
    async fn handle_metrics(&self, _body: Bytes) -> Result<(), GenericError> {
        self.record(Signal::Metrics)
    }

    async fn handle_logs(&self, _body: Bytes) -> Result<(), GenericError> {
        self.record(Signal::Logs)
    }

    async fn handle_traces(&self, _body: Bytes) -> Result<(), GenericError> {
        self.record(Signal::Traces)
    }
}

/// OTLP servers that a test started, on ephemeral loopback ports.
pub(super) struct TestServers {
    pub(super) supervisor: TestSupervisor,
    pub(super) http_port: u16,
    pub(super) grpc_port: u16,
}

impl TestServers {
    /// Returns a server configuration for ephemeral loopback ports.
    pub(super) fn configuration() -> OtlpServerConfiguration {
        OtlpServerConfiguration::new(
            Some(ListenAddress::tcp_loopback(0)),
            Some(ListenAddress::tcp_loopback(0)),
            4 * 1024 * 1024,
        )
    }

    /// Builds `configuration` with `handler`, and waits until both servers accept connections.
    ///
    /// `id` must be unique among the tests, so that each test finds the ports of its own servers.
    pub(super) async fn start(id: &str, configuration: OtlpServerConfiguration, handler: impl OtlpHandler) -> Self {
        let supervisor = TestSupervisor::start("otlp-test").await;
        let http_id = format!("http-{id}");
        let grpc_id = format!("grpc-{id}");

        supervisor
            .scope(configuration.with_server_ids(&*http_id, &*grpc_id).build(
                handler,
                MemoryLimiter::noop(),
                Metrics::for_tests(),
                &tokio::runtime::Handle::current(),
            ))
            .await
            .expect("build should succeed");

        let http_port = bound_port(&supervisor, &http_id).await;
        let grpc_port = bound_port(&supervisor, &grpc_id).await;
        wait_for_port(http_port).await;
        wait_for_port(grpc_port).await;

        Self {
            supervisor,
            http_port,
            grpc_port,
        }
    }

    /// Stops the servers.
    pub(super) async fn shutdown(self) {
        self.supervisor
            .shutdown()
            .await
            .expect("supervisor should shut down cleanly");
    }
}

/// Returns the port that the server with identifier `id` bound to.
pub(super) async fn bound_port(supervisor: &TestSupervisor, id: &str) -> u16 {
    let id = format!("http-server-{}", id);
    let mut subscription = supervisor
        .dataspace()
        .subscribe::<BoundListenAddress>(IdentifierFilter::exact(id.clone()));

    match tokio::time::timeout(Duration::from_secs(5), subscription.recv()).await {
        Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address)))) => address.port(),
        update => panic!("expected a bound address assertion for '{id}', got {update:?}"),
    }
}

/// Polls `port` until something accepts a connection on it.
///
/// Spawning only queues the servers for the supervisor, so their listeners come up a moment after `build` returns
/// rather than synchronously with it.
pub(super) async fn wait_for_port(port: u16) {
    let addr = format!("127.0.0.1:{}", port);
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("nothing was listening on port {} within a second", port);
}

/// A response that a test client received, with its body and trailers read in full.
#[derive(Debug)]
pub(super) struct Received {
    pub(super) status: StatusCode,
    pub(super) version: Version,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
    pub(super) trailers: Option<HeaderMap>,
}

/// A client for plaintext test servers.
///
/// With [`HttpProtocol::Http2`] it speaks HTTP/2 with prior knowledge, as gRPC clients do.
pub(super) struct TestClient {
    client: HttpClient,
}

impl TestClient {
    pub(super) fn new(protocol: HttpProtocol) -> Self {
        let client = HttpClient::builder()
            .with_tls_config(|builder| builder.with_root_cert_store(RootCertStore::empty()))
            .with_http_protocol(protocol)
            .build()
            .expect("client should build");
        Self { client }
    }

    /// Sends `request`, and returns the response once its headers arrive.
    ///
    /// # Errors
    ///
    /// If the server doesn't answer, an error is returned.
    pub(super) async fn send_request<B>(
        &mut self, request: http::Request<B>,
    ) -> Result<http::Response<hyper::body::Incoming>, GenericError>
    where
        B: http_body::Body<Data = Bytes> + Send + Sync + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        self.client.send(request).await
    }

    /// Sends a request to `uri`, and reads the whole response.
    ///
    /// # Errors
    ///
    /// If the server doesn't answer, or the response body fails, an error is returned.
    pub(super) async fn send(
        &mut self, method: &str, uri: &str, headers: &[(&'static str, &str)], body: impl Into<Bytes>,
    ) -> Result<Received, GenericError> {
        let mut request = http::Request::builder()
            .method(method)
            .uri(uri)
            .body(Full::new(body.into()))
            .expect("request should build");
        *request.headers_mut() = header_map(headers);

        let response = self.client.send(request).await?;
        let (parts, body) = response.into_parts();
        let collected = body.collect().await?;
        Ok(Received {
            status: parts.status,
            version: parts.version,
            headers: parts.headers,
            trailers: collected.trailers().cloned(),
            body: collected.to_bytes(),
        })
    }
}
