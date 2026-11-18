use std::sync::LazyLock;

use async_trait::async_trait;
use axum::body::Bytes;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::BuildContext;
use saluki_core::data_model::payload::{GrpcPayload, Payload, PayloadMetadata, PayloadType};
use saluki_core::topology::OutputDefinition;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::{server::http::Http2Config, ListenAddress};
use stringtheory::MetaString;
use tokio::select;
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::common::otlp::{
    build_metrics, receiver::OtlpReceiver, CorsConfiguration, Metrics, OtlpHandler, OtlpServerConfiguration,
    OtlpTlsConfiguration, OTLP_LOGS_GRPC_SERVICE_PATH, OTLP_METRICS_GRPC_SERVICE_PATH, OTLP_TRACES_GRPC_SERVICE_PATH,
};

/// Configuration for the OTLP relay.
pub struct OtlpRelayConfiguration {
    receiver: OtlpReceiver,
}

impl OtlpRelayConfiguration {
    /// Creates a new `OtlpRelayConfiguration` that receives with the given settings.
    pub fn new(receiver: OtlpReceiver) -> Self {
        Self { receiver }
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

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Relay + Send>, GenericError> {
        let http_tls_config = self.receiver.http.tls.build()?;
        let grpc_tls_config = self.receiver.grpc.tls.build()?;

        Ok(Box::new(OtlpRelay {
            http_endpoint: self.http_endpoint(),
            grpc_endpoint: self.grpc_endpoint(),
            grpc_max_recv_msg_size_bytes: self.grpc_max_recv_msg_size_bytes(),
            grpc_http2_config: self.receiver.grpc.http2_config(),
            http_max_request_body_size: self.receiver.http.max_request_body_size,
            cors: self.receiver.http.cors.clone(),
            http_tls_config,
            grpc_tls_config,
            metrics: build_metrics(context.component_context()),
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
    grpc_http2_config: Http2Config,
    http_max_request_body_size: u64,
    cors: CorsConfiguration,
    http_tls_config: Option<OtlpTlsConfiguration>,
    grpc_tls_config: Option<OtlpTlsConfiguration>,
    metrics: Metrics,
}

#[async_trait]
impl Relay for OtlpRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let Self {
            http_endpoint,
            grpc_endpoint,
            grpc_max_recv_msg_size_bytes,
            grpc_http2_config,
            http_max_request_body_size,
            cors,
            http_tls_config,
            grpc_tls_config,
            metrics,
        } = *self;

        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter().clone();

        let (payload_tx, mut payload_rx) = mpsc::channel(1024);

        // Build our gRPC and HTTP servers and spawn them. They stop when this relay is told to stop, and the channel
        // closes once both exit. Thus, the loop below dispatches every payload that they accepted before it ends.
        let handler = RelayHandler::new(payload_tx);
        let mut server_config = OtlpServerConfiguration::new(
            http_endpoint.clone(),
            grpc_endpoint.clone(),
            grpc_max_recv_msg_size_bytes,
        )
        .with_cors(cors)
        .with_grpc_http2_config(grpc_http2_config)
        .with_http_max_request_body_size(http_max_request_body_size);

        if let Some(tls) = http_tls_config {
            server_config = server_config.with_http_tls(tls);
        }
        if let Some(tls) = grpc_tls_config {
            server_config = server_config.with_grpc_tls(tls);
        }

        server_config
            .build(
                handler,
                memory_limiter,
                metrics,
                context.topology_context().global_thread_pool(),
            )
            .await?;

        health.mark_ready();
        debug!(%http_endpoint, %grpc_endpoint, "OTLP relay started.");

        loop {
            select! {
                maybe_payload = payload_rx.recv() => {
                    let Some(otlp_payload) = maybe_payload else {
                        break;
                    };

                    let output_name = otlp_payload.signal_type.as_str();
                    let payload = Payload::Grpc(otlp_payload.into_grpc_payload());
                    if let Err(e) = context.dispatcher().dispatch_named(output_name, payload).await {
                        error!(error = %e, output = output_name, "Failed to dispatch OTLP payload.");
                    }
                },
                _ = health.live() => continue,
            }
        }

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
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use otlp_protos::opentelemetry::proto::collector::logs::v1::{
        logs_service_client::LogsServiceClient, ExportLogsServiceRequest,
    };
    use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs;
    use saluki_core::components::test_util::TestComponentDriver;
    use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
    use saluki_io::net::BoundListenAddress;

    use super::OtlpRelayConfiguration;
    use crate::common::otlp::receiver::{receiver, GrpcTransport};

    #[test]
    fn endpoints_combine_transport_and_address() {
        let mut receiver = receiver();
        receiver.grpc.endpoint = "0.0.0.0:4317".to_string();
        receiver.grpc.transport = GrpcTransport::Tcp;
        receiver.grpc.max_recv_msg_size_mib = 4;
        receiver.http.endpoint = "0.0.0.0:4318".to_string();
        receiver.http.transport = "tcp".to_string();
        let config = OtlpRelayConfiguration::new(receiver);

        assert_eq!(config.grpc_endpoint().to_string(), "tcp://0.0.0.0:4317");
        assert_eq!(config.http_endpoint().to_string(), "tcp://0.0.0.0:4318");
    }

    #[test]
    fn grpc_max_recv_msg_size_converts_mib_to_bytes() {
        let mut receiver = receiver();
        receiver.grpc.max_recv_msg_size_mib = 8;
        let config = OtlpRelayConfiguration::new(receiver);

        assert_eq!(config.grpc_max_recv_msg_size_bytes(), 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn accepted_payloads_are_dispatched_before_the_relay_stops() {
        const EXPORTS: usize = 16;

        let mut receiver = receiver();
        receiver.grpc.endpoint = "127.0.0.1:0".to_string();
        receiver.grpc.max_recv_msg_size_mib = 4;
        // The configuration translator resolves a zero keepalive to these defaults.
        receiver.grpc.keepalive.time = Duration::from_secs(2 * 60 * 60);
        receiver.grpc.keepalive.timeout = Duration::from_secs(20);
        receiver.http.endpoint = "127.0.0.1:0".to_string();
        let config = OtlpRelayConfiguration::new(receiver);

        // The output holds one payload, so the relay still has the others queued when it is told to stop.
        let (mut control, mut outputs) = TestComponentDriver::options()
            .with_interconnect_capacity(NonZeroUsize::MIN)
            .relay(config)
            .await
            .expect("relay should build");
        let mut logs = outputs.take_named_output("logs");

        let mut bound = control
            .supervisor()
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact("http-server-otlp-grpc"));
        let address = match bound.recv().await {
            Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address))) => address,
            update => panic!("expected the gRPC server to bind, got {update:?}"),
        };
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .expect("endpoint should parse")
            .connect()
            .await
            .expect("should connect to the gRPC server");
        let mut client = LogsServiceClient::new(channel);
        for _ in 0..EXPORTS {
            let request = ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs::default()],
            };
            client.export(request).await.expect("export should succeed");
        }

        control.signal_shutdown();
        let (result, payloads) = tokio::join!(control.wait(), logs.collect());
        result.expect("relay should stop cleanly");
        assert_eq!(payloads.len(), EXPORTS, "every accepted export must be dispatched");
    }
}
