use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use otel_protos::{
    collector::{
        logs::v1::{logs_service_server::*, *},
        metrics::v1::{metrics_service_server::*, *},
        trace::v1::{trace_service_server::*, *},
    },
    logs::v1::{LogRecord, ResourceLogs},
    metrics::v1::{Metric as OtelMetric, ResourceMetrics},
    trace::v1::{ResourceSpans, Span as OtelSpan},
};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::sources::*,
    pooling::ObjectPool as _,
    spawn_traced,
    topology::{shutdown::DynamicShutdownCoordinator, OutputDefinition},
};
use saluki_error::GenericError;
use saluki_event::{DataType, Event};
use saluki_io::net::{listener::ConnectionOrientedListener, GrpcListenAddress, ListenAddress};
use serde::Deserialize;
use tonic::{transport::Server, Request, Response, Status};
use tonic_web::GrpcWebLayer;
use tower::util::option_layer;
use tracing::{error, info};

fn default_listen_addresses() -> Vec<GrpcListenAddress> {
    vec![GrpcListenAddress::Binary(ListenAddress::Tcp(
        ([127, 0, 0, 1], 8080).into(),
    ))]
}

/// OpenTelemetry source.
///
/// Accepts OpenTelemetry data (OTLP) over gRPC.
#[derive(Deserialize)]
pub struct OpenTelemetryConfiguration {
    /// Addresses to listen on.
    ///
    /// Multiple addresses can be specified to create multiple listeners, where the mode (HTTP vs gRPC) is determined by
    /// the address scheme. When HTTP/HTTPS is used, the listener will be a OTLP/HTTP-compatible endpoint. For TCP and
    /// UDS, the listener will be a OTLP/gRPC-compatible endpoint.
    ///
    /// Defaults to `grpc://127.0.0.1:4317`.
    #[serde(default = "default_listen_addresses")]
    listen_addresses: Vec<GrpcListenAddress>,
}

impl OpenTelemetryConfiguration {
    /// Creates a new `OpenTelemetryConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
}

#[async_trait]
impl SourceBuilder for OpenTelemetryConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        let mut listeners = Vec::with_capacity(self.listen_addresses.len());
        for listen_address in &self.listen_addresses {
            let (is_grpc_web, address) = match listen_address {
                GrpcListenAddress::Binary(addr) => (false, addr),
                GrpcListenAddress::Web(addr) => (true, addr),
            };

            let listener = ConnectionOrientedListener::from_listen_address(address.clone()).await?;
            listeners.push((is_grpc_web, listener));
        }

        Ok(Box::new(OpenTelemetry { listeners }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("logs", DataType::Log),
                OutputDefinition::named_output("metrics", DataType::Metric),
                OutputDefinition::named_output("traces", DataType::Trace),
            ]
        });

        &OUTPUTS
    }
}

impl MemoryBounds for OpenTelemetryConfiguration {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {
        todo!()
    }
}

pub struct OpenTelemetry {
    listeners: Vec<(bool, ConnectionOrientedListener)>,
}

#[async_trait]
impl Source for OpenTelemetry {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");

        // For each listener, create a corresponding gRPC server (with the appropriate gRPC-Web support, if enabled) to
        // go with it, which will accept logs, metrics, and traces. Each server gets spawned as a dedicated task.
        let mut shutdown_coordinator = DynamicShutdownCoordinator::default();
        let server_inner = Arc::new(ServerInner {
            context: context.clone(),
        });

        for (is_grpc_web, listener) in self.listeners {
            let shutdown_handle = shutdown_coordinator.register();
            let server = Server::builder()
                .layer(option_layer(is_grpc_web.then(GrpcWebLayer::new)))
                .add_service(LogsServiceServer::from_arc(Arc::clone(&server_inner)))
                .add_service(MetricsServiceServer::from_arc(Arc::clone(&server_inner)))
                .add_service(TraceServiceServer::from_arc(Arc::clone(&server_inner)));

            spawn_traced(async move {
                if let Err(e) = server.serve_with_incoming_shutdown(listener, shutdown_handle).await {
                    error!(error = %e, "Error serving OpenTelemetry collector endpoint.");
                }
            });
        }

        info!("OpenTelemetry source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        global_shutdown.await;

        info!("Stopping OpenTelemetry source...");
        shutdown_coordinator.shutdown().await;

        info!("OpenTelemetry source stopped.");

        Ok(())
    }
}

struct ServerInner {
    context: SourceContext,
}

#[async_trait]
impl LogsService for ServerInner {
    async fn export(
        &self, request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        // Convert each OTLP log into our internal representation, and write them to the event buffer.
        let inner = request.into_inner();

        let mut event_buffer = self.context.event_buffer_pool().acquire().await;
        event_buffer.extend(inner.resource_logs.into_iter().flat_map(otel_logs_event_iter));
        let logs_count = event_buffer.len();

        // Try forwarding the event buffer, returning an error if we failed to do so.
        match self.context.forwarder().forward_named("logs", event_buffer).await {
            // TODO: If/when we make this bounded, we'll want to handle partial success here.
            Ok(()) => Ok(Response::new(ExportLogsServiceResponse { partial_success: None })),
            Err(e) => {
                error!(%logs_count, error = %e, "Error forwarding logs.");
                Err(Status::data_loss(format!(
                    "Failed to forward {} log(s) to downstream component(s).",
                    logs_count
                )))
            }
        }
    }
}

#[async_trait]
impl MetricsService for ServerInner {
    async fn export(
        &self, request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        // Convert each OTLP metric into our internal representation, and write them to the event buffer.
        let inner = request.into_inner();

        let mut event_buffer = self.context.event_buffer_pool().acquire().await;
        event_buffer.extend(inner.resource_metrics.into_iter().flat_map(otel_metrics_event_iter));
        let metrics_count = event_buffer.len();

        // Try forwarding the event buffer, returning an error if we failed to do so.
        match self.context.forwarder().forward_named("metrics", event_buffer).await {
            // TODO: If/when we make this bounded, we'll want to handle partial success here.
            Ok(()) => Ok(Response::new(ExportMetricsServiceResponse { partial_success: None })),
            Err(e) => {
                error!(%metrics_count, error = %e, "Error forwarding metrics.");
                Err(Status::data_loss(format!(
                    "Failed to forward {} metric(s) to downstream component(s).",
                    metrics_count
                )))
            }
        }
    }
}

#[async_trait]
impl TraceService for ServerInner {
    async fn export(
        &self, request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        // Convert each OTLP trace into our internal representation, and write them to the event buffer.
        let inner = request.into_inner();

        let mut event_buffer = self.context.event_buffer_pool().acquire().await;
        event_buffer.extend(inner.resource_spans.into_iter().flat_map(otel_traces_event_iter));
        let traces_count = event_buffer.len();

        // Try forwarding the event buffer, returning an error if we failed to do so.
        match self.context.forwarder().forward_named("traces", event_buffer).await {
            // TODO: If/when we make this bounded, we'll want to handle partial success here.
            Ok(()) => Ok(Response::new(ExportTraceServiceResponse { partial_success: None })),
            Err(e) => {
                error!(%traces_count, error = %e, "Error forwarding traces.");
                Err(Status::data_loss(format!(
                    "Failed to forward {} trace(s) to downstream component(s).",
                    traces_count
                )))
            }
        }
    }
}

fn otel_logs_event_iter(resource_logs: ResourceLogs) -> impl Iterator<Item = Event> {
    // TODO: We probably need/want to utilize the resource data, as well as the scope data, when generating the final
    // event, but we're mostly just trying to get the basic structure laid out here first.

    resource_logs
        .scope_logs
        .into_iter()
        .flat_map(|sl| sl.log_records.into_iter())
        .map(otel_log_to_event)
}

fn otel_log_to_event(_log_record: LogRecord) -> Event {
    todo!()
}

fn otel_metrics_event_iter(resource_metrics: ResourceMetrics) -> impl Iterator<Item = Event> {
    // TODO: We probably need/want to utilize the resource data, as well as the scope data, when generating the final
    // event, but we're mostly just trying to get the basic structure laid out here first.

    resource_metrics
        .scope_metrics
        .into_iter()
        .flat_map(|sm| sm.metrics.into_iter())
        .flat_map(otel_metric_to_event)
}

fn otel_metric_to_event(_metric: OtelMetric) -> Vec<Event> {
    todo!()
}

fn otel_traces_event_iter(resource_spans: ResourceSpans) -> impl Iterator<Item = Event> {
    // TODO: We probably need/want to utilize the resource data, as well as the scope data, when generating the final
    // event, but we're mostly just trying to get the basic structure laid out here first.

    resource_spans
        .scope_spans
        .into_iter()
        .flat_map(|ss| ss.spans.into_iter())
        .map(otel_trace_to_event)
}

fn otel_trace_to_event(_span: OtelSpan) -> Event {
    todo!()
}
