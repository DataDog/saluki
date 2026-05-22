use std::{
    fs::File,
    io::Write as _,
    net::{Ipv4Addr, TcpStream, UdpSocket},
    os::unix::net::{UnixDatagram, UnixStream},
    path::Path,
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
    File(File),
}

struct GrpcBackend {
    channel: Channel,
    /// The full gRPC service/method path (for example, "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export")
    service_method_path: String,
}

pub struct TargetSender {
    /// Named backends in declared order. `send()` writes to each in turn for every payload.
    backends: Vec<(String, TargetBackend)>,
    /// Shared tokio runtime used by every gRPC backend in this process; `None` if no backend
    /// requires gRPC. A single runtime is sufficient because `send()` is synchronous and we
    /// `block_on` one call at a time.
    runtime: Option<tokio::runtime::Runtime>,
}

impl TargetSender {
    /// Creates a new `TargetSender` that writes to a single file.
    ///
    /// Used for the `--output-file` mode of millstone, which writes the generated payload bytes
    /// to disk instead of sending them over the wire. Only one backend is needed; the entry is
    /// named `"file"` for consistency with the named-backend model.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the file, it will be returned.
    pub fn from_file(path: &Path) -> Result<Self, GenericError> {
        let file =
            File::create(path).with_error_context(|| format!("Failed to create output file '{}'.", path.display()))?;
        Ok(Self {
            backends: vec![("file".to_string(), TargetBackend::File(file))],
            runtime: None,
        })
    }

    /// Creates a new `TargetSender` based on the given configuration.
    ///
    /// Builds one backend per entry in `config.targets`, in declared order. A single shared tokio
    /// runtime is created up front if any backend is gRPC; all gRPC backends share it.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the socket/stream necessary for any target address, it
    /// will be returned. The error message identifies the failing target by name.
    pub fn from_config(config: &Config) -> Result<Self, GenericError> {
        let needs_runtime = config
            .targets
            .iter()
            .any(|(_, addr)| matches!(addr, TargetAddress::Grpc(_)));
        let runtime = if needs_runtime {
            Some(tokio::runtime::Runtime::new().error_context("Failed to create tokio runtime for gRPC client.")?)
        } else {
            None
        };

        let mut backends = Vec::with_capacity(config.targets.len());
        for (name, addr) in &config.targets {
            let backend = build_backend(name, addr, runtime.as_ref())?;
            backends.push((name.clone(), backend));
        }

        Ok(Self { backends, runtime })
    }

    /// Sends a single payload to every configured target in turn (fan-out).
    ///
    /// Writes the same payload bytes to each backend in declared order. Fails fast on the first
    /// error: subsequent backends are not written, and the error is annotated with the failing
    /// target's name. Returns the total number of bytes written across all successful backends
    /// when every backend succeeded.
    ///
    /// Fail-fast is correct for correctness testing: if one sink can't receive, the comparison
    /// is invalid and continuing would produce asymmetric counters and a misleading divergence
    /// report. See `design/millstone-fanout.md` ("Error semantics").
    ///
    /// # Errors
    ///
    /// If any backend fails to send, the error is returned with the target's name prepended.
    pub fn send(&mut self, payload: &[u8]) -> Result<usize, GenericError> {
        let mut total_bytes = 0usize;
        for (name, backend) in &mut self.backends {
            let bytes: Result<usize, GenericError> = match backend {
                TargetBackend::Tcp(stream) => stream
                    .write_all(payload)
                    .map(|_| payload.len())
                    .map_err(GenericError::from),
                TargetBackend::Udp(socket) => socket.send(payload).map_err(GenericError::from),
                TargetBackend::UnixDatagram(datagram) => datagram.send(payload).map_err(GenericError::from),
                TargetBackend::Unix(stream) => stream
                    .write_all(payload)
                    .map(|_| payload.len())
                    .map_err(GenericError::from),
                TargetBackend::Grpc(grpc) => {
                    let runtime = self
                        .runtime
                        .as_ref()
                        .ok_or_else(|| saluki_error::generic_error!("Runtime not available for gRPC send."))?;
                    let channel = grpc.channel.clone();
                    let service_method_path = grpc.service_method_path.clone();
                    send_grpc_payload(runtime, channel, &service_method_path, payload).map(|_| payload.len())
                }
                TargetBackend::File(file) => file
                    .write_all(payload)
                    .map(|_| payload.len())
                    .map_err(GenericError::from),
            };

            match bytes {
                Ok(n) => total_bytes += n,
                Err(e) => {
                    return Err(saluki_error::generic_error!(
                        "Failed to send to target '{}': {}",
                        name,
                        e
                    ))
                }
            }
        }

        Ok(total_bytes)
    }
}

/// Builds a single `TargetBackend` for the given target name and address.
///
/// `runtime` must be `Some` when `addr` is `Grpc`; the gRPC channel is created on that runtime
/// so all gRPC backends share a single executor. The target `name` is used only to annotate
/// error messages.
fn build_backend(
    name: &str, addr: &TargetAddress, runtime: Option<&tokio::runtime::Runtime>,
) -> Result<TargetBackend, GenericError> {
    match addr {
        TargetAddress::Tcp(addr_str) => {
            let stream = TcpStream::connect(addr_str.as_str()).with_error_context(|| {
                format!("Failed to connect to TCP target '{}' (target='{}').", addr_str, name)
            })?;
            Ok(TargetBackend::Tcp(stream))
        }
        TargetAddress::Udp(addr_str) => {
            let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .with_error_context(|| format!("Failed to bind UDP socket (target='{}').", name))?;
            socket.connect(addr_str.as_str()).with_error_context(|| {
                format!("Failed to connect to UDP target '{}' (target='{}').", addr_str, name)
            })?;
            Ok(TargetBackend::Udp(socket))
        }
        TargetAddress::UnixDatagram(path) => {
            let datagram = UnixDatagram::unbound()
                .with_error_context(|| format!("Failed to bind Unix datagram socket (target='{}').", name))?;
            datagram.connect(path).with_error_context(|| {
                format!(
                    "Failed to connect to Unix datagram target '{}' (target='{}').",
                    path.display(),
                    name
                )
            })?;
            Ok(TargetBackend::UnixDatagram(datagram))
        }
        TargetAddress::Unix(path) => {
            let stream = UnixStream::connect(path).with_error_context(|| {
                format!(
                    "Failed to connect to Unix stream target '{}' (target='{}').",
                    path.display(),
                    name
                )
            })?;
            Ok(TargetBackend::Unix(stream))
        }
        TargetAddress::Grpc(url) => {
            let runtime = runtime.ok_or_else(|| {
                saluki_error::generic_error!(
                    "Internal error: runtime missing for gRPC target '{}' (target='{}').",
                    url,
                    name
                )
            })?;
            let backend = create_grpc_backend(runtime, url)
                .with_error_context(|| format!("Failed to create gRPC backend for target '{}'.", name))?;
            Ok(TargetBackend::Grpc(backend))
        }
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

/// Creates a gRPC backend on the shared runtime.
///
/// The URL should be in the format: `<host>:<port>/<service>/<method>`.
///
/// # Errors
///
/// Returns an error if the URL is malformed or the connection can't be established.
fn create_grpc_backend(runtime: &tokio::runtime::Runtime, url: &str) -> Result<GrpcBackend, GenericError> {
    // Split the URL into host:port and service/method path.
    let (host_and_port, path) = url
        .split_once('/')
        .ok_or_else(|| saluki_error::generic_error!("Invalid gRPC URL format: {}", url))?;
    let service_method_path = format!("/{}", path);

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

    Ok(GrpcBackend {
        channel,
        service_method_path,
    })
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
