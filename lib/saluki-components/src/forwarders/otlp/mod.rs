use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use otlp_protos::opentelemetry::proto::collector::logs::v1::logs_service_client::LogsServiceClient;
use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use prost::Message;
use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_config::GenericConfiguration;
use saluki_core::data_model::payload::Payload;
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::PayloadType,
};
use saluki_error::ErrorContext as _;
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::select;
use tonic::transport::Channel;
use tracing::{debug, error, warn};

use crate::common::otlp::{OTLP_LOGS_GRPC_SERVICE_PATH, OTLP_METRICS_GRPC_SERVICE_PATH, OTLP_TRACES_GRPC_SERVICE_PATH};

/// OTLP forwarder configuration.
///
/// Forwards OTLP metrics and logs to the Core Agent, and traces to the Trace Agent.
#[derive(Clone)]
pub struct OtlpForwarderConfiguration {
    core_agent_otlp_grpc_endpoint: String,
    core_agent_traces_internal_port: u16,
}

impl OtlpForwarderConfiguration {
    /// Creates a new `OtlpForwarderConfiguration` from the given configuration.
    pub fn from_configuration(
        config: &GenericConfiguration, core_agent_otlp_grpc_endpoint: String,
    ) -> Result<Self, GenericError> {
        let core_agent_traces_internal_port = config
            .try_get_typed("otlp_config.traces.internal_port")?
            .unwrap_or(5003);
        Ok(Self {
            core_agent_otlp_grpc_endpoint,
            core_agent_traces_internal_port,
        })
    }
}

#[async_trait]
impl ForwarderBuilder for OtlpForwarderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Grpc
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        let trace_agent_endpoint = format!("http://localhost:{}", self.core_agent_traces_internal_port);
        let trace_agent_channel = Channel::from_shared(trace_agent_endpoint.clone())
            .error_context("Failed to construct gRPC channel due to an invalid endpoint.")?
            .connect_lazy();
        let trace_agent_client = TraceServiceClient::new(trace_agent_channel);

        let normalized_endpoint = normalize_endpoint(&self.core_agent_otlp_grpc_endpoint);
        let core_agent_grpc_channel = Channel::from_shared(normalized_endpoint)
            .error_context("Failed to construct gRPC channel due to an invalid endpoint.")?
            .connect_lazy();
        let core_agent_metrics_client = MetricsServiceClient::new(core_agent_grpc_channel.clone());
        let core_agent_logs_client = LogsServiceClient::new(core_agent_grpc_channel);

        Ok(Box::new(OtlpForwarder {
            trace_agent_client,
            core_agent_metrics_client,
            core_agent_logs_client,
        }))
    }
}

impl MemoryBounds for OtlpForwarderConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<OtlpForwarder>("component struct");
    }
}

struct OtlpForwarder {
    trace_agent_client: TraceServiceClient<Channel>,
    core_agent_metrics_client: MetricsServiceClient<Channel>,
    core_agent_logs_client: LogsServiceClient<Channel>,
}

#[async_trait]
impl Forwarder for OtlpForwarder {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let Self {
            mut trace_agent_client,
            mut core_agent_metrics_client,
            mut core_agent_logs_client,
        } = *self;

        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!("OTLP forwarder started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_payload = context.payloads().next() => match maybe_payload {
                    Some(payload) => match payload {
                        Payload::Grpc(grpc_payload) => {
                            // Extract the parts of the payload, and make sure we have an OTLP payload, otherwise
                            // we skip it and move on.
                            let (_, endpoint, service_path, body) = grpc_payload.into_parts();
                            match &service_path {
                                path if *path == OTLP_TRACES_GRPC_SERVICE_PATH => {
                                    export_traces(&mut trace_agent_client, &endpoint, &service_path, body).await;
                                }
                                path if *path == OTLP_METRICS_GRPC_SERVICE_PATH => {
                                    export_metrics(&mut core_agent_metrics_client, &endpoint, &service_path, body).await;
                                }
                                path if *path == OTLP_LOGS_GRPC_SERVICE_PATH => {
                                    export_logs(&mut core_agent_logs_client, &endpoint, &service_path, body).await;
                                }
                                _ => {
                                    warn!(service_path = %service_path, "Received gRPC payload with unknown service path. Skipping.");
                                    continue;
                                }
                            }

                        },
                        _ => continue,
                    },
                    None => break,
                },
            }
        }

        debug!("OTLP forwarder stopped.");

        Ok(())
    }
}

async fn export_traces(
    trace_agent_client: &mut TraceServiceClient<Channel>, endpoint: &MetaString, service_path: &MetaString,
    body: FrozenChunkedBytesBuffer,
) {
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
            return;
        }
    };

    match trace_agent_client.export(request).await {
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
            error!(error = %e, %endpoint, %service_path, "Failed to export traces to Trace Agent.");
        }
    }
}

async fn export_metrics(
    core_agent_grpc_client: &mut MetricsServiceClient<Channel>, endpoint: &MetaString, service_path: &MetaString,
    body: FrozenChunkedBytesBuffer,
) {
    // Decode the raw request payload into a typed body so we can export it.
    //
    // TODO: This is suboptimal since we know the payload should be valid as it was decoded when it
    // was ingested, and only after that converted to raw bytes. It would be nice to just forward
    // the bytes as-is without decoding it again here just to satisfy the client interface, but no
    // such API currently exists.
    let body = body.into_bytes();

    let request = match ExportMetricsServiceRequest::decode(body) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to decode metrics or logs export request from payload.");
            return;
        }
    };

    match core_agent_grpc_client.export(request).await {
        Ok(response) => {
            let resp = response.into_inner();
            if let Some(partial_success) = resp.partial_success {
                if partial_success.rejected_data_points > 0 {
                    warn!(
                        rejected_data_points = partial_success.rejected_data_points,
                        error = %partial_success.error_message,
                        "Metrics export partially failed."
                    );
                }
            }
        }
        Err(e) => {
            error!(error = %e, %endpoint, %service_path, "Failed to export metrics to Core Agent.");
        }
    }
}

async fn export_logs(
    core_agent_grpc_client: &mut LogsServiceClient<Channel>, endpoint: &MetaString, service_path: &MetaString,
    body: FrozenChunkedBytesBuffer,
) {
    // Decode the raw request payload into a typed body so we can export it.
    //
    // TODO: This is suboptimal since we know the payload should be valid as it was decoded when it
    // was ingested, and only after that converted to raw bytes. It would be nice to just forward
    // the bytes as-is without decoding it again here just to satisfy the client interface, but no
    // such API currently exists.
    let body = body.into_bytes();

    let request = match ExportLogsServiceRequest::decode(body) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "Failed to decode logs export request from payload.");
            return;
        }
    };

    match core_agent_grpc_client.export(request).await {
        Ok(response) => {
            let resp = response.into_inner();
            if let Some(partial_success) = resp.partial_success {
                if partial_success.rejected_log_records > 0 {
                    warn!(
                        rejected_log_records = partial_success.rejected_log_records,
                        error = %partial_success.error_message,
                        "Trace export partially failed."
                    );
                }
            }
        }
        Err(e) => {
            error!(error = %e, %endpoint, %service_path, "Failed to export metrics to Core Agent.");
        }
    }
}

fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("https://{}", endpoint)
    }
}
