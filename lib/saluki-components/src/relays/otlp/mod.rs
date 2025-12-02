use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::components::relays::{Relay, RelayBuilder, RelayContext};
use saluki_core::components::ComponentContext;
use saluki_core::data_model::payload::{GrpcPayload, HttpPayload, Payload, PayloadMetadata, PayloadType};
use saluki_error::GenericError;
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::select;
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::common::otlp::config::{Receiver, Traces};
use crate::common::otlp::{build_metrics, Metrics, OtlpHandler, OtlpServerBuilder};

const OTLP_TRACE_SERVICE_PATH: MetaString =
    MetaString::from_static("/opentelemetry.proto.collector.trace.v1.TraceService/Export");

fn default_otlp_destination_endpoint() -> String {
    "http://localhost:4319".to_string()
}

/// Configuration for the OTLP relay.
#[derive(Deserialize, Default)]
pub struct OtlpRelayConfiguration {
    #[serde(default)]
    otlp_config: OtlpRelayConfig,

    /// The destination endpoint to forward OTLP data to (metrics and logs).
    #[serde(default = "default_otlp_destination_endpoint")]
    otlp_destination_endpoint: String,
}

/// OTLP configuration for the relay.
#[derive(Deserialize, Default)]
pub struct OtlpRelayConfig {
    #[serde(default)]
    receiver: Receiver,

    #[serde(default)]
    traces: Traces,
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

    fn destination_endpoint(&self) -> &str {
        &self.otlp_destination_endpoint
    }

    fn trace_destination_endpoint(&self) -> MetaString {
        format!("localhost:{}", self.otlp_config.traces.internal_port).into()
    }
}

impl MemoryBounds for OtlpRelayConfiguration {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
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

/// OTLP relay.
///
/// Receives OTLP metrics and logs via gRPC and HTTP, outputting payloads for downstream processing.
pub struct OtlpRelay {
    http_endpoint: ListenAddress,
    grpc_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
    destination_endpoint: String,
    trace_destination_endpoint: MetaString,
    metrics: Metrics,
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
    async fn handle_metrics(&self, body: Bytes) -> Result<(), String> {
        let otlp_payload = OtlpPayload {
            signal_type: OtlpSignalType::Metrics,
            data: body,
        };

        self.tx
            .send(otlp_payload)
            .await
            .map_err(|_| "Failed to send OTLP metrics payload to dispatcher; channel is closed.".to_string())?;
        Ok(())
    }

    async fn handle_logs(&self, body: Bytes) -> Result<(), String> {
        let otlp_payload = OtlpPayload {
            signal_type: OtlpSignalType::Logs,
            data: body,
        };

        self.tx
            .send(otlp_payload)
            .await
            .map_err(|_| "Failed to send OTLP logs payload to dispatcher; channel is closed.".to_string())?;
        Ok(())
    }

    async fn handle_tracer_payloads(&self, body: Bytes) -> Result<(), String> {
        let otlp_payload = OtlpPayload {
            signal_type: OtlpSignalType::Traces,
            data: body,
        };

        self.tx
            .send(otlp_payload)
            .await
            .map_err(|_| "Failed to send OTLP traces payload to dispatcher; channel is closed.".to_string())?;
        Ok(())
    }
}

#[async_trait]
impl RelayBuilder for OtlpRelayConfiguration {
    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http | PayloadType::Grpc
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Relay + Send>, GenericError> {
        Ok(Box::new(OtlpRelay {
            http_endpoint: self.http_endpoint(),
            grpc_endpoint: self.grpc_endpoint(),
            grpc_max_recv_msg_size_bytes: self.grpc_max_recv_msg_size_bytes(),
            destination_endpoint: self.destination_endpoint().to_string(),
            trace_destination_endpoint: self.trace_destination_endpoint(),
            metrics: build_metrics(&context),
        }))
    }
}

#[async_trait]
impl Relay for OtlpRelay {
    async fn run(self: Box<Self>, mut context: RelayContext) -> Result<(), GenericError> {
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let global_thread_pool = context.topology_context().global_thread_pool().clone();
        let memory_limiter = context.topology_context().memory_limiter().clone();
        let dispatcher = context.dispatcher();

        let (payload_tx, mut payload_rx) = mpsc::channel::<OtlpPayload>(1024);

        let handler = RelayHandler::new(payload_tx);
        let server_builder = OtlpServerBuilder::new(
            self.http_endpoint.clone(),
            self.grpc_endpoint.clone(),
            self.grpc_max_recv_msg_size_bytes,
        );

        let metrics_arc = Arc::new(self.metrics);

        let (http_shutdown, mut http_error) = server_builder
            .build(
                handler,
                memory_limiter.clone(),
                global_thread_pool.clone(),
                metrics_arc.clone(),
            )
            .await?;

        health.mark_ready();
        debug!(
            http_endpoint = %self.http_endpoint,
            grpc_endpoint = %self.grpc_endpoint,
            destination = %self.destination_endpoint,
            "OTLP relay started."
        );

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
                    let metadata = PayloadMetadata::from_event_count(1);
                    let buffer = otlp_payload.data.into();
                    if matches!(otlp_payload.signal_type, OtlpSignalType::Traces) {

                        let grpc_payload = GrpcPayload::new(
                            metadata,
                            self.trace_destination_endpoint.clone(),
                            OTLP_TRACE_SERVICE_PATH,
                            buffer,
                        );
                        let payload = Payload::Grpc(grpc_payload);

                        if let Err(e) = dispatcher.dispatch(payload).await {
                            error!(error = %e, "Failed to dispatch gRPC trace payload.");
                        }
                    } else {
                        let path = match otlp_payload.signal_type {
                            OtlpSignalType::Metrics => "/v1/metrics",
                            OtlpSignalType::Logs => "/v1/logs",
                            OtlpSignalType::Traces => "/v1/traces",
                        };
                        let uri = format!("{}{}", self.destination_endpoint, path);

                        let request = match http::Request::builder()
                            .method("POST")
                            .uri(&uri)
                            .header("content-type", "application/x-protobuf")
                            .body(buffer)
                        {
                            Ok(req) => req,
                            Err(e) => {
                                error!(error = %e, "Failed to build HTTP request for OTLP payload.");
                                continue;
                            }
                        };

                        let http_payload = HttpPayload::new(metadata, request);
                        let payload = Payload::Http(http_payload);

                        if let Err(e) = dispatcher.dispatch(payload).await {
                            error!(error = %e, "Failed to dispatch HTTP OTLP payload.");
                        }
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
