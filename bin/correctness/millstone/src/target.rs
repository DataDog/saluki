use std::{
    io::Write as _,
    net::{Ipv4Addr, TcpStream, UdpSocket},
    os::unix::net::{UnixDatagram, UnixStream},
};

use prost::bytes::Bytes;
use saluki_error::{ErrorContext as _, GenericError};
use tonic::{client::Grpc, transport::Channel};

use crate::config::{Config, TargetAddress};

enum TargetBackend {
    Tcp(TcpStream),
    Udp(UdpSocket),
    UnixDatagram(UnixDatagram),
    Unix(UnixStream),
    Grpc(GrpcBackend),
}

struct GrpcBackend {
    channel: Channel,
    /// The full gRPC service/method path (e.g., "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export")
    service_method_path: String,
}

pub struct TargetSender {
    backend: TargetBackend,
    // Runtime for gRPC operations (only used when backend is Grpc)
    runtime: Option<tokio::runtime::Runtime>,
}

impl TargetSender {
    /// Creates a new `TargetSender` based on the given configuration.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the socket/stream necessary for the target address, it will be returned.
    pub fn from_config(config: &Config) -> Result<Self, GenericError> {
        let (backend, runtime) = match &config.target {
            TargetAddress::Tcp(addr) => {
                let stream = TcpStream::connect(addr)
                    .with_error_context(|| format!("Failed to connect to TCP target '{}'.", addr))?;
                (TargetBackend::Tcp(stream), None)
            }
            TargetAddress::Udp(addr) => {
                // We have to bind the socket first before we can "connect" it.
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).error_context("Failed to bind UDP socket.")?;
                socket
                    .connect(addr)
                    .with_error_context(|| format!("Failed to connect to UDP target '{}'.", addr))?;

                (TargetBackend::Udp(socket), None)
            }
            TargetAddress::UnixDatagram(path) => {
                let datagram = UnixDatagram::unbound().error_context("Failed to bind Unix datagram socket.")?;
                datagram.connect(path).with_error_context(|| {
                    format!("Failed to connect to Unix datagram target '{}'.", path.display())
                })?;

                (TargetBackend::UnixDatagram(datagram), None)
            }
            TargetAddress::Unix(path) => {
                let stream = UnixStream::connect(path)
                    .with_error_context(|| format!("Failed to connect to Unix stream target '{}'.", path.display()))?;
                (TargetBackend::Unix(stream), None)
            }
            TargetAddress::Grpc(url) => create_grpc_client(url)?,
        };

        Ok(Self { backend, runtime })
    }

    /// Sends a single payload to the target.
    ///
    /// Attempts to send the entire payload to the the target, but may only partially write a payload if the underlying
    /// target transport does not support ordered delivery of messages and fragmented sends cannot be achieved.
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
            TargetBackend::UnixDatagram(datagram) => datagram.send(payload)?,
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
        };

        Ok(n)
    }
}

/// Sends a payload via gRPC using the provided runtime and channel.
///
/// The payload is sent as raw bytes without encoding, using a no-op codec.
/// This is necessary because millstone receives already-encoded protobuf messages.
///
/// # Errors
///
/// Returns an error if the gRPC call fails for any reason.
fn send_grpc_payload(
    runtime: &tokio::runtime::Runtime, channel: Channel, service_method_path: &str, payload: &[u8],
) -> Result<(), GenericError> {
    runtime.block_on(async move {
        let mut grpc_client = Grpc::new(channel);

        grpc_client
            .ready()
            .await
            .map_err(|e| saluki_error::generic_error!("gRPC client not ready: {}", e))?;

        let codec = NoopCodec {};
        let request = tonic::Request::new(Bytes::copy_from_slice(payload));
        let path = tonic::codegen::http::uri::PathAndQuery::try_from(service_method_path)
            .map_err(|e| saluki_error::generic_error!("Invalid gRPC path: {}", e))?;

        grpc_client
            .unary(request, path, codec)
            .await
            .map_err(|e| saluki_error::generic_error!("gRPC call failed: {}", e))?;

        Ok(())
    })
}

/// Creates a generic gRPC backend with a tokio runtime for the given gRPC URL.
///
/// The URL should be in the format: `<host>:<port>/<service>/<method>`.
///
/// # Errors
///
/// Returns an error if the runtime cannot be created or the connection cannot be established.
fn create_grpc_client(url: &str) -> Result<(TargetBackend, Option<tokio::runtime::Runtime>), GenericError> {
    // Split the URL into host:port and service/method path
    let (host_and_port, path) = url
        .split_once('/')
        .ok_or_else(|| saluki_error::generic_error!("Invalid gRPC URL format: {}", url))?;
    let service_method_path = format!("/{}", path);

    let runtime = tokio::runtime::Runtime::new().error_context("Failed to create tokio runtime for gRPC client.")?;
    let endpoint = format!("http://{}", host_and_port);

    let channel = runtime
        .block_on(async {
            Channel::from_shared(endpoint.clone())
                .map_err(|e| saluki_error::generic_error!("Invalid gRPC endpoint: {}", e))?
                .connect()
                .await
                .map_err(|e| saluki_error::generic_error!("Failed to connect to gRPC endpoint: {}", e))
        })
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
