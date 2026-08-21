use std::sync::LazyLock;

use agent_data_plane_config::domains;
use async_trait::async_trait;
use axum::body::Bytes;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::ComponentContext;
use saluki_core::data_model::payload::{GrpcPayload, Payload, PayloadMetadata, PayloadType};
use saluki_core::topology::OutputDefinition;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use saluki_tls::ServerTLSConfigBuilder;
use stringtheory::MetaString;
use tokio::sync::mpsc;
use tokio::{pin, select};
use tracing::{debug, error};

use crate::common::otlp::{
    build_metrics, CorsConfiguration, Metrics, OtlpHandler, OtlpServerBuilder, OTLP_LOGS_GRPC_SERVICE_PATH,
    OTLP_METRICS_GRPC_SERVICE_PATH, OTLP_TRACES_GRPC_SERVICE_PATH,
};

/// Builds component-owned CORS settings from the resolved configuration model.
fn cors_configuration(cors: &domains::otlp::Cors) -> CorsConfiguration {
    CorsConfiguration {
        allowed_origins: cors.allowed_origins.clone(),
        allowed_headers: cors.allowed_headers.clone(),
        exposed_headers: cors.exposed_headers.clone(),
        max_age: cors.max_age,
    }
}

/// Builds a `rustls::ServerConfig` from resolved TLS settings, if TLS is enabled.
///
/// TLS is enabled when both `cert_file` and `key_file` are non-empty. When `ca_file` is also non-empty, the server
/// requests client certificates and verifies them against the CA certificates in that file, but does not require a
/// client certificate (optional verification). Mandatory client verification (`client_ca_file`) is not yet supported.
fn build_tls_config(tls: &domains::otlp::Tls) -> Result<Option<rustls::ServerConfig>, GenericError> {
    if tls.cert_file.is_empty() || tls.key_file.is_empty() {
        return Ok(None);
    }

    let mut builder = ServerTLSConfigBuilder::new()
        .with_cert_file(&tls.cert_file)
        .with_key_file(&tls.key_file);

    if !tls.ca_file.is_empty() {
        builder = builder.with_ca_file(&tls.ca_file);
    }

    builder.build().map(Some)
}

/// Configuration for the OTLP relay.
#[derive(Default)]
pub struct OtlpRelayConfiguration {
    receiver: domains::otlp::Receiver,
}

impl OtlpRelayConfiguration {
    /// Creates relay configuration from typed OTLP receiver settings.
    pub fn from_configuration(receiver: &domains::otlp::Receiver) -> Self {
        Self {
            receiver: receiver.clone(),
        }
    }

    fn http_endpoint(&self) -> ListenAddress {
        let address = format!("{}://{}", self.receiver.http.transport, self.receiver.http.endpoint);
        ListenAddress::try_from(address).expect("valid HTTP endpoint")
    }

    fn grpc_endpoint(&self) -> ListenAddress {
        let address = format!(
            "{}://{}",
            self.receiver.grpc.transport.as_str(),
            self.receiver.grpc.endpoint
        );
        ListenAddress::try_from(address).expect("valid gRPC endpoint")
    }

    fn grpc_max_recv_msg_size_bytes(&self) -> usize {
        (self.receiver.grpc.max_recv_msg_size_mib * 1024 * 1024) as usize
    }
}

impl MemoryBounds for OtlpRelayConfiguration {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl RelayBuilder for OtlpRelayConfiguration {
    fn outputs(&self) -> &[OutputDefinition<PayloadType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<PayloadType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", PayloadType::Grpc),
                OutputDefinition::named_output("logs", PayloadType::Grpc),
                OutputDefinition::named_output("traces", PayloadType::Grpc),
            ]
        });
        &OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Relay + Send>, GenericError> {
        let http_tls_config = build_tls_config(&self.receiver.http.tls)?;
        let grpc_tls_config = build_tls_config(&self.receiver.grpc.tls)?;

        Ok(Box::new(OtlpRelay {
            http_endpoint: self.http_endpoint(),
            grpc_endpoint: self.grpc_endpoint(),
            grpc_max_recv_msg_size_bytes: self.grpc_max_recv_msg_size_bytes(),
            cors: cors_configuration(&self.receiver.http.cors),
            http_tls_config,
            grpc_tls_config,
            metrics: build_metrics(&context),
        }))
    }
}

/// OTLP relay.
///
/// Receives OTLP metrics and logs via gRPC and HTTP, outputting payloads for downstream processing.
pub struct OtlpRelay {
    http_endpoint: ListenAddress,
    grpc_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
    cors: CorsConfiguration,
    http_tls_config: Option<rustls::ServerConfig>,
    grpc_tls_config: Option<rustls::ServerConfig>,
    metrics: Metrics,
}

#[async_trait]
impl Relay for OtlpRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let Self {
            http_endpoint,
            grpc_endpoint,
            grpc_max_recv_msg_size_bytes,
            cors,
            http_tls_config,
            grpc_tls_config,
            metrics,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter().clone();

        let (payload_tx, mut payload_rx) = mpsc::channel(1024);

        // Build our gRPC and HTTP servers and spawn them.
        let handler = RelayHandler::new(payload_tx);
        let mut server_builder = OtlpServerBuilder::new(
            http_endpoint.clone(),
            grpc_endpoint.clone(),
            grpc_max_recv_msg_size_bytes,
        )
        .with_cors(cors);

        if let Some(tls_config) = http_tls_config {
            server_builder = server_builder.with_http_tls_config(tls_config);
        }
        if let Some(tls_config) = grpc_tls_config {
            server_builder = server_builder.with_grpc_tls_config(tls_config);
        }

        server_builder
            .build(handler, memory_limiter, metrics, context.spawner())
            .await?;

        health.mark_ready();
        debug!(%http_endpoint, %grpc_endpoint, "OTLP relay started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                Some(otlp_payload) = payload_rx.recv() => {
                    let output_name = otlp_payload.signal_type.as_str();
                    let payload = Payload::Grpc(otlp_payload.into_grpc_payload());
                    if let Err(e) = context.dispatcher().dispatch_named(output_name, payload).await {
                        error!(error = %e, output = output_name, "Failed to dispatch OTLP payload.");
                    }
                },
                _ = health.live() => continue,
            }
        }

        debug!("Stopping OTLP relay...");
        debug!("OTLP relay stopped.");

        Ok(())
    }
}

enum OtlpSignalType {
    Metrics,
    Logs,
    Traces,
}

impl OtlpSignalType {
    fn as_str(&self) -> &'static str {
        match self {
            OtlpSignalType::Metrics => "metrics",
            OtlpSignalType::Logs => "logs",
            OtlpSignalType::Traces => "traces",
        }
    }
}

struct OtlpPayload {
    signal_type: OtlpSignalType,
    data: Bytes,
}

impl OtlpPayload {
    fn metrics(data: Bytes) -> Self {
        Self {
            signal_type: OtlpSignalType::Metrics,
            data,
        }
    }

    fn logs(data: Bytes) -> Self {
        Self {
            signal_type: OtlpSignalType::Logs,
            data,
        }
    }

    fn traces(data: Bytes) -> Self {
        Self {
            signal_type: OtlpSignalType::Traces,
            data,
        }
    }

    fn into_grpc_payload(self) -> GrpcPayload {
        let service_path = match self.signal_type {
            OtlpSignalType::Metrics => OTLP_METRICS_GRPC_SERVICE_PATH,
            OtlpSignalType::Logs => OTLP_LOGS_GRPC_SERVICE_PATH,
            OtlpSignalType::Traces => OTLP_TRACES_GRPC_SERVICE_PATH,
        };

        // We provide an empty endpoint because we want any consuming components to fill that in for themselves.
        GrpcPayload::new(
            PayloadMetadata::from_event_count(1),
            MetaString::empty(),
            service_path,
            FrozenChunkedBytesBuffer::from(self.data),
        )
    }
}

/// Handler that forwards OTLP payloads to a channel for downstream processing.
struct RelayHandler {
    tx: mpsc::Sender<OtlpPayload>,
}

impl RelayHandler {
    fn new(tx: mpsc::Sender<OtlpPayload>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl OtlpHandler for RelayHandler {
    async fn handle_metrics(&self, body: Bytes) -> Result<(), GenericError> {
        self.tx
            .send(OtlpPayload::metrics(body))
            .await
            .error_context("Failed to send OTLP metrics payload to relay dispatcher: channel closed.")
    }

    async fn handle_logs(&self, body: Bytes) -> Result<(), GenericError> {
        self.tx
            .send(OtlpPayload::logs(body))
            .await
            .error_context("Failed to send OTLP logs payload to relay dispatcher: channel closed.")
    }

    async fn handle_traces(&self, body: Bytes) -> Result<(), GenericError> {
        self.tx
            .send(OtlpPayload::traces(body))
            .await
            .error_context("Failed to send OTLP traces payload to relay dispatcher: channel closed.")
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains;
    use agent_data_plane_config::domains::otlp::GrpcTransport;

    use super::OtlpRelayConfiguration;

    fn relay(receiver: domains::otlp::Receiver) -> OtlpRelayConfiguration {
        OtlpRelayConfiguration::from_configuration(&receiver)
    }

    #[test]
    fn endpoints_combine_transport_and_address() {
        let config = relay(domains::otlp::Receiver {
            grpc: domains::otlp::GrpcReceiver {
                endpoint: "0.0.0.0:4317".to_string(),
                transport: GrpcTransport::Tcp,
                max_recv_msg_size_mib: 4,
                ..Default::default()
            },
            http: domains::otlp::HttpReceiver {
                endpoint: "0.0.0.0:4318".to_string(),
                transport: "tcp".to_string(),
                cors: Default::default(),
                ..Default::default()
            },
            ..Default::default()
        });

        assert_eq!(config.grpc_endpoint().to_string(), "tcp://0.0.0.0:4317");
        assert_eq!(config.http_endpoint().to_string(), "tcp://0.0.0.0:4318");
    }

    #[test]
    fn grpc_max_recv_msg_size_converts_mib_to_bytes() {
        let config = relay(domains::otlp::Receiver {
            grpc: domains::otlp::GrpcReceiver {
                max_recv_msg_size_mib: 8,
                ..Default::default()
            },
            ..Default::default()
        });

        assert_eq!(config.grpc_max_recv_msg_size_bytes(), 8 * 1024 * 1024);
    }
}
