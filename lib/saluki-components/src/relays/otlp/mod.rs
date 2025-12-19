use std::sync::LazyLock;

use async_trait::async_trait;
use axum::body::Bytes;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_config::GenericConfiguration;
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::ComponentContext;
use saluki_core::data_model::payload::{GrpcPayload, Payload, PayloadMetadata, PayloadType};
use saluki_core::topology::OutputDefinition;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::select;
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::common::otlp::config::Receiver;
use crate::common::otlp::{
    build_metrics, Metrics, OtlpHandler, OtlpServerBuilder, OTLP_LOGS_GRPC_SERVICE_PATH,
    OTLP_METRICS_GRPC_SERVICE_PATH, OTLP_TRACES_GRPC_SERVICE_PATH,
};

/// Configuration for the OTLP relay.
#[derive(Deserialize, Default)]
pub struct OtlpRelayConfiguration {
    #[serde(default)]
    otlp_config: OtlpRelayConfig,
}

/// OTLP configuration for the relay.
#[derive(Deserialize, Default)]
pub struct OtlpRelayConfig {
    #[serde(default)]
    receiver: Receiver,
}

impl OtlpRelayConfiguration {
    /// Creates a new `OtlpRelayConfiguration` from the given generic configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        config.as_typed().map_err(Into::into)
    }

    fn http_endpoint(&self) -> ListenAddress {
        let transport = &self.otlp_config.receiver.protocols.http.transport;
        let endpoint = &self.otlp_config.receiver.protocols.http.endpoint;
        let address = format!("{}://{}", transport, endpoint);
        ListenAddress::try_from(address).expect("valid HTTP endpoint")
    }

    fn grpc_endpoint(&self) -> ListenAddress {
        let transport = &self.otlp_config.receiver.protocols.grpc.transport;
        let endpoint = &self.otlp_config.receiver.protocols.grpc.endpoint;
        let address = format!("{}://{}", transport, endpoint);
        ListenAddress::try_from(address).expect("valid gRPC endpoint")
    }

    fn grpc_max_recv_msg_size_bytes(&self) -> usize {
        (self.otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib * 1024 * 1024) as usize
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
        Ok(Box::new(OtlpRelay {
            http_endpoint: self.http_endpoint(),
            grpc_endpoint: self.grpc_endpoint(),
            grpc_max_recv_msg_size_bytes: self.grpc_max_recv_msg_size_bytes(),
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
    metrics: Metrics,
}

#[async_trait]
impl Relay for OtlpRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let Self {
            http_endpoint,
            grpc_endpoint,
            grpc_max_recv_msg_size_bytes,
            metrics,
        } = *self;

        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let global_thread_pool = context.topology_context().global_thread_pool().clone();
        let memory_limiter = context.topology_context().memory_limiter().clone();
        let dispatcher = context.dispatcher();

        let (payload_tx, mut payload_rx) = mpsc::channel(1024);

        let handler = RelayHandler::new(payload_tx);
        let server_builder = OtlpServerBuilder::new(
            http_endpoint.clone(),
            grpc_endpoint.clone(),
            grpc_max_recv_msg_size_bytes,
        );

        let (http_shutdown, mut http_error) = server_builder
            .build(handler, memory_limiter, global_thread_pool, metrics)
            .await?;

        health.mark_ready();
        debug!(%http_endpoint, %grpc_endpoint, "OTLP relay started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                error = &mut http_error => {
                    if let Some(error) = error {
                        debug!(%error, "HTTP server error.");
                    }
                    break;
                },
                Some(otlp_payload) = payload_rx.recv() => {
                    let payload = Payload::Grpc(otlp_payload.into_grpc_payload());
                    if let Err(e) = dispatcher.dispatch(payload).await {
                        error!(error = %e, "Failed to dispatch OTLP payload.");
                    }
                },
                _ = health.live() => continue,
            }
        }

        debug!("Stopping OTLP relay...");

        http_shutdown.shutdown();

        debug!("OTLP relay stopped.");

        Ok(())
    }
}

enum OtlpSignalType {
    Metrics,
    Logs,
    Traces,
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
