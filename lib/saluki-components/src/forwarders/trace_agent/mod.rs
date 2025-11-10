use async_trait::async_trait;
use bytes::Buf;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use otlp_protos::opentelemetry::proto::collector::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use prost::Message;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::PayloadType,
};
use saluki_error::ErrorContext as _;
use saluki_error::GenericError;
use serde::Deserialize;
use tokio::select;
use tonic::transport::Channel;
use tracing::{debug, error, warn};

use crate::common::otlp::config::Traces;

/// Configuration for the trace-agent forwarder.
#[derive(Clone, Deserialize, Default)]
pub struct TraceAgentForwarderConfiguration {
    #[serde(default)]
    otlp_config: TraceAgentOtlpConfig,
}

#[derive(Clone, Deserialize, Default)]
struct TraceAgentOtlpConfig {
    #[serde(default)]
    traces: Traces,
}

impl TraceAgentForwarderConfiguration {
    /// Creates a new `TraceAgentForwarderConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        config.as_typed().map_err(Into::into)
    }
}

#[async_trait]
impl ForwarderBuilder for TraceAgentForwarderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Grpc
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        let endpoint = format!("http://localhost:{}", self.otlp_config.traces.internal_port);
        let channel = Channel::from_shared(endpoint)
            .error_context("Invalid gRPC endpoint")?
            .connect_lazy();

        let client = TraceServiceClient::new(channel);

        Ok(Box::new(TraceAgentForwarder { client }))
    }
}

impl MemoryBounds for TraceAgentForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<TraceAgentForwarder>("component struct");
    }
}

/// Trace-agent forwarder.
pub struct TraceAgentForwarder {
    client: TraceServiceClient<Channel>,
}

#[async_trait]
impl Forwarder for TraceAgentForwarder {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!("Trace-agent forwarder started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_payload = context.payloads().next() => match maybe_payload {
                    Some(payload) => if let Some(grpc_payload) = payload.try_into_grpc_payload() {
                        let (_, endpoint, _, mut body) = grpc_payload.into_parts();

                        let remaining = body.remaining();
                        let body_bytes = body.copy_to_bytes(remaining);
                        let request = match ExportTraceServiceRequest::decode(body_bytes) {
                            Ok(req) => req,
                            Err(e) => {
                                error!(error = %e, "Failed to decode ExportTraceServiceRequest");
                                continue;
                            }
                        };

                        match self.client.export(request).await {
                            Ok(response) => {
                                let resp = response.into_inner();
                                if let Some(partial_success) = resp.partial_success {
                                    if partial_success.rejected_spans > 0 {
                                        warn!(
                                            rejected_spans = partial_success.rejected_spans,
                                            error_message = %partial_success.error_message,
                                            "Trace export partially failed"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(error = %e, endpoint = %endpoint, "Failed to export traces to trace-agent");
                            }
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("Trace-agent forwarder stopped.");
        Ok(())
    }
}
