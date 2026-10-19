#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::{
    fs::File,
    io::Write as _,
    net::{Ipv4Addr, TcpStream, UdpSocket},
    path::Path,
    time::Duration,
};
#[cfg(unix)]
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use backon::{Backoff, BackoffBuilder as _, ExponentialBuilder, Retryable as _};
use http_body_util::{BodyExt as _, Full};
use hyper::{body::Incoming, Method, Request, Response, StatusCode, Uri};
#[cfg(unix)]
use hyper_util::rt::TokioIo;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use prost::bytes::Bytes;
use saluki_error::{ErrorContext as _, GenericError};
use tonic::{client::Grpc, transport::Channel};
use tracing::warn;

use crate::config::{Config, TargetAddress};

const MAX_RETRIES: usize = 5;

enum TargetBackend {
    Tcp(TcpStream),
    Udp(UdpSocket),
    #[cfg(unix)]
    UnixDatagram(UnixDatagram),
    #[cfg(unix)]
    Unix(UnixStream),
    Grpc(GrpcBackend),
    Http(Box<HttpBackend>),
    File(File),
}

struct GrpcBackend {
    channel: Channel,
    /// The full gRPC service/method path (for example, "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export")
    service_method_path: String,
}

struct HttpBackend {
    client: HttpClient,
    uri: Uri,
    /// Human-readable target used in error messages.
    ///
    /// For a Unix socket target, `uri` only carries the request path, so this names the socket as well.
    display_target: String,
    /// Headers sent with every request, which depend on the payload type.
    headers: &'static [(&'static str, &'static str)],
}

/// HTTP client over either TCP or a Unix domain socket.
enum HttpClient {
    Tcp(Client<HttpConnector, Full<Bytes>>),
    #[cfg(unix)]
    Unix(Client<UnixConnector, Full<Bytes>>),
}

impl HttpClient {
    async fn request(
        &self, request: Request<Full<Bytes>>,
    ) -> Result<Response<Incoming>, hyper_util::client::legacy::Error> {
        match self {
            Self::Tcp(client) => client.request(request).await,
            #[cfg(unix)]
            Self::Unix(client) => client.request(request).await,
        }
    }
}

/// Connector that opens every connection to the same Unix domain socket, regardless of the request URI.
#[cfg(unix)]
#[derive(Clone)]
struct UnixConnector {
    path: Arc<Path>,
}

#[cfg(unix)]
impl tower::Service<Uri> for UnixConnector {
    type Response = TokioIo<tokio::net::UnixStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _dst: Uri) -> Self::Future {
        let path = Arc::clone(&self.path);
        Box::pin(async move { tokio::net::UnixStream::connect(&*path).await.map(TokioIo::new) })
    }
}

pub struct TargetSender {
    backend: TargetBackend,
    // Runtime for gRPC and HTTP operations (only used when backend is Grpc or Http)
    runtime: Option<tokio::runtime::Runtime>,
}

impl TargetSender {
    /// Creates a new `TargetSender` that writes to a file.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the file, it will be returned.
    pub fn from_file(path: &Path) -> Result<Self, GenericError> {
        let file =
            File::create(path).with_error_context(|| format!("Failed to create output file '{}'.", path.display()))?;
        Ok(Self {
            backend: TargetBackend::File(file),
            runtime: None,
        })
    }

    /// Creates a new `TargetSender` based on the given configuration.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the socket/stream necessary for the target address, it will be returned.
    pub fn from_config(config: &Config) -> Result<Self, GenericError> {
        let (backend, runtime) = match &config.target {
            TargetAddress::Tcp(addr) => {
                let stream = TcpStream::connect(addr.as_str())
                    .with_error_context(|| format!("Failed to connect to TCP target '{}'.", addr))?;
                (TargetBackend::Tcp(stream), None)
            }
            TargetAddress::Udp(addr) => {
                // We have to bind the socket first before we can "connect" it.
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).error_context("Failed to bind UDP socket.")?;
                socket
                    .connect(addr.as_str())
                    .with_error_context(|| format!("Failed to connect to UDP target '{}'.", addr))?;

                (TargetBackend::Udp(socket), None)
            }
            TargetAddress::UnixDatagram(path) => create_unix_datagram_backend(path)?,
            TargetAddress::Unix(path) => match &config.http_path {
                Some(http_path) => create_unix_http_client(path, http_path, config.corpus.payload.http_headers())?,
                None => create_unix_stream_backend(path)?,
            },
            TargetAddress::Grpc(url) => create_grpc_client(url)?,
            TargetAddress::Http(url) => create_http_client(url, config.corpus.payload.http_headers())?,
        };

        Ok(Self { backend, runtime })
    }

    /// Sends a single payload to the target.
    ///
    /// Attempts to send the entire payload to the target, but may only partially write a payload if the underlying
    /// target transport doesn't support ordered delivery of messages and fragmented sends can't be achieved.
    ///
    /// On success, `Ok(n)` is returned, where `n` is the number of bytes sent.
    ///
    /// # Errors
    ///
    /// If an error occurs while sending the payload, it will be returned.
    pub fn send(&mut self, payload: &[u8]) -> Result<usize, GenericError> {
        let n = match &mut self.backend {
            TargetBackend::Tcp(stream) => stream.write_all(payload).map(|_| payload.len())?,
            TargetBackend::Udp(socket) => socket.send(payload)?,
            #[cfg(unix)]
            TargetBackend::UnixDatagram(datagram) => datagram.send(payload)?,
            #[cfg(unix)]
            TargetBackend::Unix(stream) => stream.write_all(payload).map(|_| payload.len())?,
            TargetBackend::Grpc(backend) => {
                let channel = backend.channel.clone();
                let service_method_path = backend.service_method_path.clone();
                let runtime = self
                    .runtime
                    .as_ref()
                    .ok_or_else(|| saluki_error::generic_error!("Runtime not available for gRPC send."))?;

                send_grpc_payload(runtime, channel, &service_method_path, payload)?;
                payload.len()
            }
            TargetBackend::Http(backend) => {
                let runtime = self
                    .runtime
                    .as_ref()
                    .ok_or_else(|| saluki_error::generic_error!("Runtime not available for HTTP send."))?;

                send_http_payload(runtime, backend, payload)?;
                payload.len()
            }
            TargetBackend::File(file) => file.write_all(payload).map(|_| payload.len())?,
        };

        Ok(n)
    }
}

#[cfg(unix)]
fn create_unix_datagram_backend(path: &Path) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    let datagram = UnixDatagram::unbound().error_context("Failed to bind Unix datagram socket.")?;
    datagram
        .connect(path)
        .with_error_context(|| format!("Failed to connect to Unix datagram target '{}'.", path.display()))?;

    Ok((TargetBackend::UnixDatagram(datagram), None))
}

#[cfg(not(unix))]
fn create_unix_datagram_backend(path: &Path) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    Err(saluki_error::generic_error!(
        "Unix datagram targets are not supported on this platform: '{}'.",
        path.display()
    ))
}

#[cfg(unix)]
fn create_unix_stream_backend(path: &Path) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    let stream = UnixStream::connect(path)
        .with_error_context(|| format!("Failed to connect to Unix stream target '{}'.", path.display()))?;
    Ok((TargetBackend::Unix(stream), None))
}

#[cfg(not(unix))]
fn create_unix_stream_backend(path: &Path) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    Err(saluki_error::generic_error!(
        "Unix stream targets are not supported on this platform: '{}'.",
        path.display()
    ))
}

/// Inner async gRPC send, returning the raw `tonic::Status` on failure so the caller can inspect
/// the error code and decide whether to retry.
async fn try_grpc_unary(channel: Channel, service_method_path: &str, payload: &[u8]) -> Result<(), tonic::Status> {
    let mut grpc_client = Grpc::new(channel);

    grpc_client
        .ready()
        .await
        .map_err(|e| tonic::Status::internal(format!("gRPC client not ready: {}", e)))?;

    let codec = NoopCodec {};
    let request = tonic::Request::new(Bytes::copy_from_slice(payload));
    let path = tonic::codegen::http::uri::PathAndQuery::try_from(service_method_path)
        .map_err(|e| tonic::Status::internal(format!("Invalid gRPC path: {}", e)))?;

    grpc_client.unary(request, path, codec).await.map(|_| ())
}

/// Sends a payload via gRPC using the provided runtime and channel.
///
/// The payload is sent as raw bytes without encoding, using a no-op codec.
/// This is necessary because millstone receives already-encoded protobuf messages.
///
/// `UNAVAILABLE` responses are retried with exponential backoff (up to `MAX_RETRIES` retries)
/// because gRPC semantics define `UNAVAILABLE` as a transient, retriable condition. The most
/// common cause in correctness tests is a momentary "sending queue is full" response from the
/// target's pipeline when the millstone burst briefly outpaces the downstream consumer.
///
/// # Errors
///
/// Returns an error if a non-retriable gRPC status is received, or if the call fails after all
/// retry attempts are exhausted.
fn send_grpc_payload(
    runtime: &tokio::runtime::Runtime, channel: Channel, service_method_path: &str, payload: &[u8],
) -> Result<(), GenericError> {
    let result = runtime.block_on(
        (|| try_grpc_unary(channel.clone(), service_method_path, payload))
            .retry(retry_backoff())
            .when(|status| status.code() == tonic::Code::Unavailable)
            .notify(|status, delay| {
                warn!(
                    delay_ms = delay.as_millis(),
                    status = %status,
                    "Retriable gRPC error, retrying after backoff."
                );
            }),
    );

    result.map_err(|status| {
        if status.code() == tonic::Code::Unavailable {
            saluki_error::generic_error!("gRPC call failed after {} retries: {}", MAX_RETRIES, status)
        } else {
            saluki_error::generic_error!("gRPC call failed: {}", status)
        }
    })
}

/// Backoff used when retrying transient connection and send failures for the gRPC and HTTP targets.
fn retry_backoff() -> impl Backoff {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(100))
        .with_max_delay(Duration::from_millis(1600))
        .with_max_times(MAX_RETRIES)
        .build()
}

/// Creates a generic gRPC backend with a tokio runtime for the given gRPC URL.
///
/// The URL should be in the format: `<host>:<port>/<service>/<method>`.
///
/// # Errors
///
/// Returns an error if the runtime can't be created or the connection can't be established after exhausting the
/// configured retries.
fn create_grpc_client(url: &str) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    create_grpc_client_with_backoff(url, retry_backoff())
}

fn create_grpc_client_with_backoff(
    url: &str, backoff: impl Backoff,
) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    // Split the URL into host:port and service/method path
    let (host_and_port, path) = url
        .split_once('/')
        .ok_or_else(|| saluki_error::generic_error!("Invalid gRPC URL format: {}", url))?;
    let service_method_path = format!("/{}", path);

    let runtime = tokio::runtime::Runtime::new().error_context("Failed to create tokio runtime for gRPC client.")?;
    let endpoint = format!("http://{}", host_and_port);

    let grpc_endpoint = Channel::from_shared(endpoint.clone())
        .map_err(|e| saluki_error::generic_error!("Invalid gRPC endpoint: {}", e))?;
    let connect = || {
        let grpc_endpoint = grpc_endpoint.clone();
        async move { grpc_endpoint.connect().await }
    };
    let channel = runtime
        .block_on(connect.retry(backoff).notify(|error, delay| {
            warn!(
                delay_ms = delay.as_millis(),
                error = %error,
                "Failed to connect to gRPC endpoint. Retrying after backoff."
            );
        }))
        .error_context("Failed to connect to gRPC endpoint.")
        .with_error_context(|| format!("Failed to connect to gRPC target '{}'.", endpoint))?;

    let backend = GrpcBackend {
        channel,
        service_method_path,
    };
    Ok((TargetBackend::Grpc(backend), Some(runtime)))
}

/// Creates an HTTP backend with a tokio runtime for the given HTTP URL.
///
/// No connection is made until the first payload is sent, so connection failures surface (and are retried) when
/// sending.
///
/// # Errors
///
/// Returns an error if the URL is invalid or the runtime can't be created.
fn create_http_client(
    url: &str, headers: &'static [(&'static str, &'static str)],
) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    let uri = url
        .parse::<Uri>()
        .with_error_context(|| format!("Invalid HTTP target URL '{}'.", url))?;
    let runtime = tokio::runtime::Runtime::new().error_context("Failed to create tokio runtime for HTTP client.")?;
    let client = HttpClient::Tcp(Client::builder(TokioExecutor::new()).build_http());

    let backend = HttpBackend {
        client,
        uri,
        display_target: url.to_string(),
        headers,
    };
    Ok((TargetBackend::Http(Box::new(backend)), Some(runtime)))
}

/// Creates an HTTP backend that sends requests for `http_path` over the Unix domain socket at `socket_path`.
///
/// Like [`create_http_client`], no connection is made until the first payload is sent.
///
/// # Errors
///
/// Returns an error if the path doesn't form a valid URI or the runtime can't be created.
#[cfg(unix)]
fn create_unix_http_client(
    socket_path: &Path, http_path: &str, headers: &'static [(&'static str, &'static str)],
) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    // The host is never resolved, but HTTP/1.1 requires one: use `localhost`, as tracing libraries do over UDS.
    let uri = format!("http://localhost{}", http_path)
        .parse::<Uri>()
        .with_error_context(|| format!("Invalid HTTP path '{}'.", http_path))?;
    let runtime = tokio::runtime::Runtime::new().error_context("Failed to create tokio runtime for HTTP client.")?;
    let connector = UnixConnector {
        path: Arc::from(socket_path),
    };
    let client = HttpClient::Unix(Client::builder(TokioExecutor::new()).build(connector));

    let backend = HttpBackend {
        client,
        uri,
        display_target: format!("unix://{}{}", socket_path.display(), http_path),
        headers,
    };
    Ok((TargetBackend::Http(Box::new(backend)), Some(runtime)))
}

#[cfg(not(unix))]
fn create_unix_http_client(
    socket_path: &Path, _http_path: &str, _headers: &'static [(&'static str, &'static str)],
) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    Err(saluki_error::generic_error!(
        "Unix stream targets are not supported on this platform: '{}'.",
        socket_path.display()
    ))
}

/// Error from a single HTTP send attempt.
enum HttpSendError {
    /// The request could not be sent, or the response could not be read.
    Transport(String),

    /// The target answered with a non-success status.
    Status { status: StatusCode, body: String },
}

impl HttpSendError {
    /// Returns `true` if the failure is likely transient and the payload should be sent again.
    ///
    /// Connection failures are retried since the target may still be starting, as are `429` and `5xx` responses, which
    /// include a proxy reporting that its own upstream isn't reachable yet.
    fn is_retriable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, .. } => status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

impl std::fmt::Display for HttpSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{}", error),
            Self::Status { status, body } => write!(f, "unexpected status {}: {}", status, body),
        }
    }
}

async fn try_http_post(backend: &HttpBackend, payload: &[u8]) -> Result<(), HttpSendError> {
    let mut builder = Request::builder().method(Method::POST).uri(backend.uri.clone());
    for (name, value) in backend.headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(payload)))
        .map_err(|e| HttpSendError::Transport(format!("failed to build request: {}", e)))?;

    let response = backend
        .client
        .request(request)
        .await
        .map_err(|e| HttpSendError::Transport(format!("{:?}", e)))?;
    let status = response.status();

    // Always read the body to completion so the connection can be reused for the next payload.
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| HttpSendError::Transport(format!("failed to read response body: {}", e)))?
        .to_bytes();

    if status.is_success() {
        Ok(())
    } else {
        Err(HttpSendError::Status {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

/// Sends a payload as the body of an HTTP `POST` request using the provided runtime.
///
/// Transient failures (see [`HttpSendError::is_retriable`]) are retried with exponential backoff, up to `MAX_RETRIES`
/// retries.
///
/// # Errors
///
/// Returns an error if a non-retriable failure occurs, or if the request still fails after all retry attempts are
/// exhausted.
fn send_http_payload(
    runtime: &tokio::runtime::Runtime, backend: &HttpBackend, payload: &[u8],
) -> Result<(), GenericError> {
    runtime
        .block_on(
            (|| try_http_post(backend, payload))
                .retry(retry_backoff())
                .when(HttpSendError::is_retriable)
                .notify(|error, delay| {
                    warn!(
                        delay_ms = delay.as_millis(),
                        error = %error,
                        "Retriable HTTP error, retrying after backoff."
                    );
                }),
        )
        .map_err(|error| saluki_error::generic_error!("HTTP request to '{}' failed: {}", backend.display_target, error))
}

// No-op codec for sending raw protobuf bytes via gRPC without encoding/decoding.
#[derive(Debug, Clone, Default)]
struct NoopCodec;

impl tonic::codec::Codec for NoopCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = NoopEncoder;
    type Decoder = NoopDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        NoopEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        NoopDecoder
    }
}

#[derive(Debug, Clone, Default)]
struct NoopEncoder;

impl tonic::codec::Encoder for NoopEncoder {
    type Item = Bytes;
    type Error = tonic::Status;

    fn encode(&mut self, item: Self::Item, buf: &mut tonic::codec::EncodeBuf<'_>) -> Result<(), Self::Error> {
        use bytes::BufMut;
        buf.put_slice(&item);
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct NoopDecoder;

impl tonic::codec::Decoder for NoopDecoder {
    type Item = Bytes;
    type Error = tonic::Status;

    fn decode(&mut self, buf: &mut tonic::codec::DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        use bytes::Buf;
        let len = buf.remaining();
        if len == 0 {
            return Ok(Some(Bytes::new()));
        }
        let mut bytes = vec![0u8; len];
        buf.copy_to_slice(&mut bytes);
        Ok(Some(Bytes::from(bytes)))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::{
        io::{BufRead as _, BufReader, Read, Write},
        net::TcpListener,
        thread,
    };

    use super::{
        create_grpc_client_with_backoff, create_http_client, create_unix_http_client, retry_backoff, TargetSender,
    };

    /// Accepts a single HTTP/1.1 request, answers it with `response`, and returns the request head and body.
    fn serve_one_request(listener: TcpListener, response: &'static str) -> thread::JoinHandle<(String, Vec<u8>)> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            read_request_and_respond(stream, response)
        })
    }

    /// Unix domain socket counterpart of [`serve_one_request`].
    #[cfg(unix)]
    fn serve_one_unix_request(listener: UnixListener, response: &'static str) -> thread::JoinHandle<(String, Vec<u8>)> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            read_request_and_respond(stream, response)
        })
    }

    fn read_request_and_respond<S: Read + Write>(mut stream: S, response: &'static str) -> (String, Vec<u8>) {
        let (head, body) = {
            let mut reader = BufReader::new(&mut stream);

            let mut head = String::new();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            (head, body)
        };

        stream.write_all(response.as_bytes()).unwrap();
        (head, body)
    }

    fn http_sender(listener: &TcpListener, headers: &'static [(&'static str, &'static str)]) -> TargetSender {
        let url = format!("http://{}/v0.4/traces", listener.local_addr().unwrap());
        let (backend, runtime) = create_http_client(&url, headers).unwrap();
        TargetSender { backend, runtime }
    }

    #[test]
    fn http_target_posts_the_payload_with_its_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = http_sender(&listener, &[("content-type", "application/msgpack")]);
        let server = serve_one_request(listener, "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");

        assert_eq!(sender.send(b"payload").unwrap(), 7);

        let (head, body) = server.join().unwrap();
        assert!(
            head.starts_with("POST /v0.4/traces HTTP/1.1\r\n"),
            "unexpected request head: {head}"
        );
        assert!(
            head.contains("content-type: application/msgpack\r\n"),
            "unexpected request head: {head}"
        );
        assert_eq!(body, b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn unix_http_target_posts_the_payload_with_its_headers() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("apm.socket");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (backend, runtime) =
            create_unix_http_client(&socket_path, "/v0.4/traces", &[("content-type", "application/msgpack")]).unwrap();
        let mut sender = TargetSender { backend, runtime };
        let server = serve_one_unix_request(listener, "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");

        assert_eq!(sender.send(b"payload").unwrap(), 7);

        let (head, body) = server.join().unwrap();
        assert!(
            head.starts_with("POST /v0.4/traces HTTP/1.1\r\n"),
            "unexpected request head: {head}"
        );
        assert!(head.contains("host: localhost\r\n"), "unexpected request head: {head}");
        assert!(
            head.contains("content-type: application/msgpack\r\n"),
            "unexpected request head: {head}"
        );
        assert_eq!(body, b"payload");
    }

    #[test]
    fn http_target_does_not_retry_a_client_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = http_sender(&listener, &[("content-type", "application/octet-stream")]);
        let server = serve_one_request(listener, "HTTP/1.1 400 Bad Request\r\ncontent-length: 3\r\n\r\nbad");

        let error = sender.send(b"payload").unwrap_err().to_string();
        server.join().unwrap();

        // The listener is gone after one request, so a retry would have failed with a connection error instead.
        assert!(error.contains("400 Bad Request: bad"), "unexpected error: {error}");
    }

    #[test]
    fn preserves_the_final_grpc_connection_error() {
        let error = match create_grpc_client_with_backoff("127.0.0.1:0/test.Service/Call", std::iter::empty()) {
            Ok(_) => panic!("connection should fail"),
            Err(error) => error,
        };
        let error_chain = error.chain().map(ToString::to_string).collect::<Vec<_>>();

        assert!(
            error_chain.iter().any(|cause| cause.contains("tcp connect error")),
            "expected TCP connection failure in error chain, got: {error_chain:?}"
        );
    }

    #[test]
    fn retry_backoff_uses_bounded_exponential_delays() {
        let delays_ms = retry_backoff().map(|delay| delay.as_millis()).collect::<Vec<_>>();

        assert_eq!(delays_ms, [100, 200, 400, 800, 1600]);
    }
}
