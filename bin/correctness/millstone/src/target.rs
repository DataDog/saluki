#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::{
    fs::File,
    io::Write as _,
    net::{Ipv4Addr, TcpStream, UdpSocket},
    path::Path,
    time::Duration,
};

use backon::{Backoff, Retryable as _};
use prost::bytes::Bytes;
use saluki_error::{ErrorContext as _, GenericError};
use tonic::{client::Grpc, transport::Channel};
use tracing::warn;

use crate::config::{Config, TargetAddress};

const GRPC_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
];
const MAX_RETRIES: usize = GRPC_RETRY_DELAYS.len();

enum TargetBackend {
    Tcp(TcpStream),
    Udp(UdpSocket),
    #[cfg(unix)]
    UnixDatagram(UnixDatagram),
    #[cfg(unix)]
    Unix(UnixStream),
    Grpc(GrpcBackend),
    File(File),
}

struct GrpcBackend {
    channel: Channel,
    /// The full gRPC service/method path (for example, "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export")
    service_method_path: String,
}

pub struct TargetSender {
    backend: TargetBackend,
    // Runtime for gRPC operations (only used when backend is Grpc)
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
            TargetAddress::Unix(path) => create_unix_stream_backend(path)?,
            TargetAddress::Grpc(url) => create_grpc_client(url)?,
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
            .retry(grpc_retry_backoff())
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

fn grpc_retry_backoff() -> impl Backoff {
    GRPC_RETRY_DELAYS.into_iter()
}

/// Creates a generic gRPC backend with a tokio runtime for the given gRPC URL.
///
/// The URL should be in the format: `<host>:<port>/<service>/<method>`.
///
/// # Errors
///
/// Returns an error if the runtime can't be created or the connection can't be established after retrying transient
/// startup failures.
fn create_grpc_client(url: &str) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
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
        .block_on(connect.retry(grpc_retry_backoff()).notify(|error, delay| {
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
    use std::time::Duration;

    use super::{create_grpc_client, grpc_retry_backoff};

    #[test]
    fn preserves_the_final_grpc_connection_error() {
        let error = match create_grpc_client("127.0.0.1:0/test.Service/Call") {
            Ok(_) => panic!("connection should fail"),
            Err(error) => error,
        };
        let error_chain = error.chain().map(ToString::to_string).collect::<Vec<_>>();

        assert!(
            error_chain.iter().any(|cause| cause == "tcp connect error"),
            "expected TCP connection failure in error chain, got: {error_chain:?}"
        );
    }

    #[test]
    fn grpc_retry_backoff_uses_bounded_exponential_delays() {
        let delays = grpc_retry_backoff().collect::<Vec<_>>();

        assert_eq!(delays, [100, 200, 400, 800, 1600].map(Duration::from_millis));
    }
}
