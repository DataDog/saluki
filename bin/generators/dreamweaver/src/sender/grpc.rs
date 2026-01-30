//! gRPC OTLP sender.

use otlp_protos::opentelemetry::proto::collector::{
    logs::v1::{logs_service_client::LogsServiceClient, ExportLogsServiceRequest},
    metrics::v1::{metrics_service_client::MetricsServiceClient, ExportMetricsServiceRequest},
    trace::v1::{trace_service_client::TraceServiceClient, ExportTraceServiceRequest},
};
use saluki_error::{ErrorContext as _, GenericError};
use tonic::transport::Channel;
use tracing::debug;

/// Default concurrency limit for gRPC requests.
pub const DEFAULT_CONCURRENCY: usize = 16;

/// gRPC-based OTLP sender.
///
/// This sender is cheap to clone and can be shared across multiple tasks.
/// The underlying gRPC channel handles connection pooling and multiplexing.
#[derive(Clone)]
pub struct GrpcOtlpSender {
    trace_client: TraceServiceClient<Channel>,
    metrics_client: MetricsServiceClient<Channel>,
    logs_client: LogsServiceClient<Channel>,
}

impl GrpcOtlpSender {
    /// Creates a new gRPC OTLP sender connected to the given endpoint.
    ///
    /// Uses the default concurrency limit.
    pub async fn new(endpoint: &str) -> Result<Self, GenericError> {
        Self::with_concurrency(endpoint, DEFAULT_CONCURRENCY).await
    }

    /// Creates a new gRPC OTLP sender with a specified concurrency limit.
    ///
    /// The concurrency limit controls how many concurrent requests can be made
    /// over the underlying HTTP/2 connection.
    pub async fn with_concurrency(endpoint: &str, concurrency: usize) -> Result<Self, GenericError> {
        debug!(
            "Connecting to OTLP endpoint: {} (concurrency: {}).",
            endpoint, concurrency
        );

        let channel = Channel::from_shared(endpoint.to_string())
            .map_err(|e| saluki_error::generic_error!("Invalid OTLP endpoint '{}': {}", endpoint, e))?
            .concurrency_limit(concurrency)
            .connect()
            .await
            .error_context("Failed to connect to OTLP endpoint")?;

        Ok(Self {
            trace_client: TraceServiceClient::new(channel.clone()),
            metrics_client: MetricsServiceClient::new(channel.clone()),
            logs_client: LogsServiceClient::new(channel),
        })
    }

    /// Sends traces to the OTLP receiver. Returns the number of spans sent.
    pub async fn send_traces(&self, request: ExportTraceServiceRequest) -> Result<usize, GenericError> {
        let span_count: usize = request
            .resource_spans
            .iter()
            .flat_map(|rs| &rs.scope_spans)
            .map(|ss| ss.spans.len())
            .sum();

        debug!("Sending {} spans.", span_count);

        // Clone the client for this request (cheap, just clones the channel)
        let mut client = self.trace_client.clone();
        client.export(request).await.error_context("Failed to export traces")?;

        Ok(span_count)
    }

    /// Sends metrics to the OTLP receiver. Returns the number of metrics sent.
    pub async fn send_metrics(&self, request: ExportMetricsServiceRequest) -> Result<usize, GenericError> {
        let metric_count: usize = request
            .resource_metrics
            .iter()
            .flat_map(|rm| &rm.scope_metrics)
            .map(|sm| sm.metrics.len())
            .sum();

        debug!("Sending {} metrics.", metric_count);

        // Clone the client for this request (cheap, just clones the channel)
        let mut client = self.metrics_client.clone();
        client.export(request).await.error_context("Failed to export metrics")?;

        Ok(metric_count)
    }

    /// Sends logs to the OTLP receiver. Returns the number of log records sent.
    pub async fn send_logs(&self, request: ExportLogsServiceRequest) -> Result<usize, GenericError> {
        let log_count: usize = request
            .resource_logs
            .iter()
            .flat_map(|rl| &rl.scope_logs)
            .map(|sl| sl.log_records.len())
            .sum();

        debug!("Sending {} log records.", log_count);

        // Clone the client for this request (cheap, just clones the channel)
        let mut client = self.logs_client.clone();
        client.export(request).await.error_context("Failed to export logs")?;

        Ok(log_count)
    }
}
