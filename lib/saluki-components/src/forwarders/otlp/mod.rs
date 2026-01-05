use async_trait::async_trait;
use http::Uri;
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
use saluki_core::observability::ComponentMetricsExt as _;
use saluki_core::{
    components::{forwarders::*, ComponentContext},
    data_model::payload::PayloadType,
};
use saluki_error::ErrorContext as _;
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::select;
use tonic::transport::Channel;
use tracing::{debug, error, warn};

use crate::common::datadog::io::TransactionForwarder;
use crate::common::datadog::transaction::{Metadata, Transaction};
use crate::common::{
    datadog::{config::ForwarderConfiguration, telemetry::ComponentTelemetry},
    otlp::{OTLP_LOGS_GRPC_SERVICE_PATH, OTLP_METRICS_GRPC_SERVICE_PATH, OTLP_TRACES_GRPC_SERVICE_PATH},
};

/// Trace Agent forwarder.
///
/// Forwards OTLP trace payloads to the Trace Agent.
#[derive(Clone)]
pub struct OtlpForwarderConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    forwarder_config: ForwarderConfiguration,

    configuration: Option<GenericConfiguration>,

    core_agent_otlp_grpc_endpoint: String,

    core_agent_traces_internal_port: u16,
}

impl OtlpForwarderConfiguration {
    /// Creates a new `TraceAgentForwarderConfiguration` from the given configuration.
    pub fn from_configuration(
        config: &GenericConfiguration, core_agent_otlp_grpc_endpoint: String,
    ) -> Result<Self, GenericError> {
        let forwarder_config = ForwarderConfiguration::from_configuration(config)?;
        let core_agent_traces_internal_port = config
            .try_get_typed("otlp_config.traces.internal_port")?
            .unwrap_or(5003);
        Ok(Self {
            forwarder_config,
            configuration: Some(config.clone()),
            core_agent_otlp_grpc_endpoint,
            core_agent_traces_internal_port,
        })
    }
    /// Overrides the default endpoint that payloads are sent to.
    ///
    /// This overrides any existing endpoint configuration, and manually sets the base endpoint (e.g.,
    /// `https://api.datad0g.com`) to be used for all payloads.
    ///
    /// This can be used to preserve other configuration settings (forwarder settings, retry, etc) while still allowing
    /// for overriding _where_ payloads are sent to.
    ///
    /// # Errors
    ///
    /// If the given request path is not valid, an error is returned.
    pub fn with_endpoint_override(mut self, dd_url: String, api_key: String) -> Self {
        // Clear any existing additional endpoints, and set the new DD URL and API key.
        //
        // This ensures that the only endpoint we'll send to is this one.
        let endpoint = self.forwarder_config.endpoint_mut();
        endpoint.clear_additional_endpoints();
        endpoint.set_dd_url(dd_url);
        endpoint.set_api_key(api_key);

        self
    }
}

#[async_trait]
impl ForwarderBuilder for OtlpForwarderConfiguration {
    fn input_payload_type(&self) -> PayloadType {
        PayloadType::Http | PayloadType::Grpc
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
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

        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder = TransactionForwarder::from_config(
            context,
            self.forwarder_config.clone(),
            self.configuration.clone(),
            get_dd_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;

        Ok(Box::new(OtlpForwarder {
            trace_agent_client,
            core_agent_metrics_client,
            core_agent_logs_client,
            forwarder,
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
    /// Forwarder for the Datadog Agent's OTLP HTTP endpoint.
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
}

#[async_trait]
impl Forwarder for OtlpForwarder {
    async fn run(mut self: Box<Self>, mut context: ForwarderContext) -> Result<(), GenericError> {
        let Self {
            mut trace_agent_client,
            mut core_agent_metrics_client,
            mut core_agent_logs_client,
            forwarder,
        } = *self;

        let forwarder = forwarder.spawn().await;
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!("Trace Agent forwarder started.");

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
                        Payload::Http(http_payload) => {
                            let (payload_meta, request) = http_payload.into_parts();
                            let transaction_meta = Metadata::from_event_count(payload_meta.event_count());
                            let transaction = Transaction::from_original(transaction_meta, request);

                            forwarder.send_transaction(transaction).await?;
                        }
                        _ => continue,
                    },
                    None => break,
                },
            }
        }

        // Shutdown the forwarder gracefully.
        forwarder.shutdown().await;

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

fn get_dd_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/v1/traces" => Some(MetaString::from_static("traces_v1")),
        "/v1/metrics" => Some(MetaString::from_static("metrics_v1")),
        "/v1/logs" => Some(MetaString::from_static("logs_v1")),
        _ => None,
    }
}

fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("https://{}", endpoint)
    }
}
