use async_trait::async_trait;
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

use crate::common::otlp::OTLP_TRACES_GRPC_SERVICE_PATH;

/// Trace Agent forwarder.
///
/// Forwards OTLP trace payloads to the Trace Agent.
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

#[derive(Clone, Deserialize, Default)]
struct Traces {
    internal_port: u16,
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
        let channel = Channel::from_shared(endpoint.clone())
            .error_context("Failed to construct gRPC channel due to an invalid endpoint.")?
            .connect_lazy();

        let client = TraceServiceClient::new(channel);

        Ok(Box::new(TraceAgentForwarder { client, endpoint }))
    }
}

impl MemoryBounds for TraceAgentForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<TraceAgentForwarder>("component struct");
    }
}

struct TraceAgentForwarder {
    client: TraceServiceClient<Channel>,
    endpoint: String,
}

#[async_trait]
impl Forwarder for TraceAgentForwarder {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let Self { mut client, endpoint } = *self;

        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!("Trace Agent forwarder started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_payload = context.payloads().next() => match maybe_payload {
                    Some(payload) => if let Some(grpc_payload) = payload.try_into_grpc_payload() {
                        // Extract the parts of the payload, and make sure we have an OTLP trace payload, otherwise
                        // we skip it and move on.
                        let (_, _, service_path, body) = grpc_payload.into_parts();
                        if service_path != OTLP_TRACES_GRPC_SERVICE_PATH {
                            warn!("Received unexpected non-trace gRPC payload in Trace Agent forwarder. Skipping.");
                            continue;
                        }

                        // Decode the raw request payload into a typed body so we can export it.
                        //
                        // TODO: This is suboptimal since we know the payload should be valid as it was decoded when it
                        // was ingested, and only after that converted to raw bytes. It would be nice to just forward
                        // the bytes as-is without decoding it again here just to satisfy the client interface, but no
                        // such API currently exists.
                        let body = body.into_bytes();
                        let request = match ExportTraceServiceRequest::decode(body) {
                            Ok(req) => req,
                            Err(e) => {
                                error!(error = %e, "Failed to decode trace export request from payload.");
                                continue;
                            }
                        };

                        match client.export(request).await {
                            Ok(response) => {
                                let resp = response.into_inner();
                                if let Some(partial_success) = resp.partial_success {
                                    if partial_success.rejected_spans > 0 {
                                        warn!(
                                            rejected_spans = partial_success.rejected_spans,
                                            error = %partial_success.error_message,
                                            "Trace export partially failed."
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(error = %e, %endpoint, "Failed to export traces to Trace Agent.");
                            }
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("Trace Agent forwarder stopped.");

        Ok(())
    }
}
