use std::net::ToSocketAddrs as _;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use agent_data_plane_config::domains::otlp::Domain as OtlpDomain;
use agent_data_plane_config::domains::traces::OtlpTraces;
use async_trait::async_trait;
use axum::body::Bytes;
use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics as OtlpResourceMetrics;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans as OtlpResourceSpans;
use prost::Message;
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_common::task::HandleExt as _;
use saluki_context::ContextResolver;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::topology::interconnect::BufferedDispatcher;
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::EventType,
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_env::WorkloadProvider;
use saluki_error::ErrorContext as _;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;
use stringtheory::MetaString;
use tokio::pin;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error};

use crate::common::otlp::{build_metrics, grpc_max_recv_msg_size_bytes, Metrics, OtlpHandler, OtlpServerBuilder};

mod logs;
mod metrics;
mod resolver;
use self::logs::translator::OtlpLogsTranslator;
use self::metrics::translator::OtlpMetricsTranslator;
use self::resolver::build_context_resolver;
use crate::common::otlp::origin::OtlpOriginTagResolver;
use crate::common::otlp::traces::translator::OtlpTracesTranslator;

/// Configuration for the OTLP source.
pub struct OtlpConfiguration {
    /// Fallback hostname for metrics that arrive without a resource hostname; set from the
    /// environment via [`Self::with_default_hostname`].
    default_hostname: MetaString,

    // Receiver per-signal activation and endpoints.
    metrics_enabled: bool,
    logs_enabled: bool,
    traces_enabled: bool,
    grpc_endpoint: String,
    grpc_transport: String,
    grpc_max_recv_msg_size_mib: u64,
    http_endpoint: String,

    // Metric context resolver sizing.
    context_string_interner_bytes: u64,
    cached_contexts_limit: usize,
    cached_tagsets_limit: usize,
    allow_context_heap_allocations: bool,

    // OTLP trace ingestion.
    traces_string_interner_bytes: u64,
    traces_ignore_missing_datadog_fields: bool,
    traces_enable_compute_top_level_by_span_kind: bool,

    /// Workload provider to utilize for origin detection/enrichment.
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl OtlpConfiguration {
    /// Creates a new `OtlpConfiguration` from the resolved OTLP configuration.
    pub fn from_configuration(otlp: &OtlpDomain, traces_otlp: &OtlpTraces) -> Result<Self, GenericError> {
        Ok(Self {
            default_hostname: MetaString::default(),
            metrics_enabled: otlp.receiver.metrics_enabled,
            logs_enabled: otlp.receiver.logs_enabled,
            traces_enabled: traces_otlp.enabled,
            grpc_endpoint: otlp.receiver.grpc.endpoint.clone(),
            grpc_transport: otlp.receiver.grpc.transport.clone(),
            grpc_max_recv_msg_size_mib: otlp.receiver.grpc.max_recv_msg_size_mib,
            http_endpoint: otlp.receiver.http.endpoint.clone(),
            context_string_interner_bytes: otlp.contexts.string_interner_size,
            cached_contexts_limit: otlp.contexts.cached_contexts_limit,
            cached_tagsets_limit: otlp.contexts.cached_tagsets_limit,
            allow_context_heap_allocations: otlp.contexts.allow_context_heap_allocs,
            traces_string_interner_bytes: traces_otlp.string_interner_size,
            traces_ignore_missing_datadog_fields: traces_otlp.ignore_missing_datadog_fields,
            traces_enable_compute_top_level_by_span_kind: traces_otlp.enable_compute_top_level_by_span_kind,
            workload_provider: None,
        })
    }

    /// Sets the default hostname used when OTLP metrics do not carry a resource hostname.
    pub fn with_default_hostname(mut self, hostname: impl Into<MetaString>) -> Self {
        self.default_hostname = hostname.into();
        self
    }

    /// Sets the workload provider to use for configuring origin detection/enrichment.
    ///
    /// A workload provider must be set otherwise origin detection/enrichment won't be enabled.
    ///
    /// Defaults to unset.
    pub fn with_workload_provider<W>(mut self, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }
}

#[async_trait]
impl SourceBuilder for OtlpConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("logs", EventType::Log),
                OutputDefinition::named_output("traces", EventType::Trace),
            ]
        });

        &OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        if !self.metrics_enabled && !self.logs_enabled && !self.traces_enabled {
            return Err(generic_error!(
                "OTLP metrics, logs and traces support is disabled. Please enable at least one of them."
            ));
        }

        let grpc_listen_str = format!("{}://{}", self.grpc_transport, self.grpc_endpoint);
        let grpc_endpoint = ListenAddress::try_from(grpc_listen_str.as_str())
            .map_err(|e| generic_error!("Invalid gRPC endpoint address '{}': {}", grpc_listen_str, e))?;

        // Enforce the current limitation that we only support TCP for gRPC.
        if !matches!(grpc_endpoint, ListenAddress::Tcp(_)) {
            return Err(generic_error!("Only 'tcp' transport is supported for OTLP gRPC"));
        }

        let http_endpoint_str = &self.http_endpoint;
        let http_socket_addr = http_endpoint_str
            .to_socket_addrs()
            .map_err(|e| generic_error!("Invalid HTTP endpoint address '{}': {}", http_endpoint_str, e))?
            .next()
            .ok_or_else(|| generic_error!("No addresses resolved for HTTP endpoint '{}'", http_endpoint_str))?;

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(OtlpOriginTagResolver::new);

        let context_resolver = build_context_resolver(self, &context, maybe_origin_tags_resolver.clone())?;
        let metrics_translator_config = metrics::config::OtlpMetricsTranslatorConfig::default()
            .with_remapping(true)
            .with_quantiles(true);
        let traces_interner_size = std::num::NonZeroUsize::new(self.traces_string_interner_bytes as usize)
            .ok_or_else(|| generic_error!("otlp_config.traces.string_interner_size must be greater than 0"))?;
        let traces_translator = OtlpTracesTranslator::new(
            self.traces_ignore_missing_datadog_fields,
            self.traces_enable_compute_top_level_by_span_kind,
            traces_interner_size,
        );
        let grpc_max_recv_msg_size_bytes = grpc_max_recv_msg_size_bytes(self.grpc_max_recv_msg_size_mib);
        let metrics = build_metrics(&context);

        Ok(Box::new(Otlp {
            context_resolver,
            origin_tag_resolver: maybe_origin_tags_resolver,
            grpc_endpoint,
            http_endpoint: ListenAddress::Tcp(http_socket_addr),
            grpc_max_recv_msg_size_bytes,
            metrics_translator_config,
            default_hostname: self.default_hostname.clone(),
            traces_translator,
            metrics,
        }))
    }
}

impl MemoryBounds for OtlpConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<Otlp>("source struct")
            .with_single_value::<SourceHandler>("source handler");
    }
}

pub struct Otlp {
    context_resolver: ContextResolver,
    origin_tag_resolver: Option<OtlpOriginTagResolver>,
    grpc_endpoint: ListenAddress,
    http_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
    metrics_translator_config: metrics::config::OtlpMetricsTranslatorConfig,
    default_hostname: MetaString,
    traces_translator: OtlpTracesTranslator,
    metrics: Metrics, // Telemetry metrics, not DD native metrics.
}

#[async_trait]
impl Source for Otlp {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let Self {
            context_resolver,
            origin_tag_resolver,
            grpc_endpoint,
            http_endpoint,
            grpc_max_recv_msg_size_bytes,
            metrics_translator_config,
            default_hostname,
            traces_translator,
            metrics,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter();

        // Create the internal channel for decoupling the servers from the converter.
        let (tx, rx) = mpsc::channel::<OtlpResource>(1024);

        let mut converter_shutdown_coordinator = ShutdownCoordinator::default();

        let metrics_translator =
            OtlpMetricsTranslator::new(metrics_translator_config, default_hostname, context_resolver)?;

        let thread_pool_handle = context.topology_context().global_thread_pool().clone();

        // Spawn the converter task. This task is shared by both servers.
        thread_pool_handle.spawn_traced_named(
            "otlp-resource-converter",
            run_converter(
                rx,
                context.clone(),
                origin_tag_resolver,
                converter_shutdown_coordinator.register(),
                metrics_translator,
                metrics.clone(),
                traces_translator,
            ),
        );

        let handler = SourceHandler::new(tx);
        let server_builder = OtlpServerBuilder::new(http_endpoint, grpc_endpoint, grpc_max_recv_msg_size_bytes);

        let (http_shutdown, mut http_error) = server_builder
            .build(handler, memory_limiter.clone(), thread_pool_handle, metrics)
            .await?;

        health.mark_ready();
        debug!("OTLP source started.");

        // Wait for the global shutdown signal, then notify converter to shutdown.
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
                _ = health.live() => continue,
            }
        }

        debug!("Stopping OTLP source...");

        http_shutdown.shutdown();
        converter_shutdown_coordinator.shutdown_and_wait().await;

        debug!("OTLP source stopped.");

        Ok(())
    }
}

enum OtlpResource {
    Metrics(OtlpResourceMetrics),
    Logs(OtlpResourceLogs),
    Traces(OtlpResourceSpans),
}

/// Handler that decodes OTLP bytes and sends resources to the converter.
struct SourceHandler {
    tx: mpsc::Sender<OtlpResource>,
}

impl SourceHandler {
    fn new(tx: mpsc::Sender<OtlpResource>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl OtlpHandler for SourceHandler {
    async fn handle_metrics(&self, body: Bytes) -> Result<(), GenericError> {
        let request =
            ExportMetricsServiceRequest::decode(body).error_context("Failed to decode metrics export request.")?;

        for resource_metrics in request.resource_metrics {
            self.tx
                .send(OtlpResource::Metrics(resource_metrics))
                .await
                .error_context("Failed to send resource metrics to converter: channel is closed.")?;
        }
        Ok(())
    }

    async fn handle_logs(&self, body: Bytes) -> Result<(), GenericError> {
        let request = ExportLogsServiceRequest::decode(body).error_context("Failed to decode logs export request.")?;

        for resource_logs in request.resource_logs {
            self.tx
                .send(OtlpResource::Logs(resource_logs))
                .await
                .error_context("Failed to send resource logs to converter: channel is closed.")?;
        }
        Ok(())
    }

    async fn handle_traces(&self, body: Bytes) -> Result<(), GenericError> {
        let request =
            ExportTraceServiceRequest::decode(body).error_context("Failed to decode trace export request.")?;

        for resource_spans in request.resource_spans {
            self.tx
                .send(OtlpResource::Traces(resource_spans))
                .await
                .error_context("Failed to send resource spans to converter: channel is closed.")?;
        }
        Ok(())
    }
}

async fn run_converter(
    mut receiver: mpsc::Receiver<OtlpResource>, source_context: SourceContext,
    origin_tag_resolver: Option<OtlpOriginTagResolver>, shutdown_handle: ShutdownHandle,
    mut metrics_translator: OtlpMetricsTranslator, metrics: Metrics, mut traces_translator: OtlpTracesTranslator,
) {
    pin!(shutdown_handle);

    debug!("OTLP resource converter task started.");

    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut metrics_dispatcher: Option<BufferedDispatcher<'_, EventsBuffer>> = None;
    let mut logs_dispatcher: Option<BufferedDispatcher<'_, EventsBuffer>> = None;
    let mut traces_dispatcher: Option<BufferedDispatcher<'_, EventsBuffer>> = None;

    loop {
        select! {
            Some(otlp_resource) = receiver.recv() => {
                match otlp_resource {
                    OtlpResource::Metrics(resource_metrics) => {
                        match metrics_translator.translate_metrics(resource_metrics, &metrics) {
                            Ok(events) => {
                                for event in events {
                                    let dispatcher = metrics_dispatcher.get_or_insert_with(|| {
                                        source_context
                                            .dispatcher()
                                            .buffered_named("metrics")
                                            .expect("metrics output should exist")
                                    });
                                    if let Err(e) = dispatcher.push(event).await {
                                        error!(error = %e, "Failed to dispatch metric event.");
                                    }
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to handle resource metrics.");
                            }
                        }
                    }
                    OtlpResource::Logs(resource_logs) => {
                        let translator = OtlpLogsTranslator::from_resource_logs(resource_logs, origin_tag_resolver.as_ref());
                        for log_event in translator {
                            metrics.logs_received().increment(1);

                            let dispatcher = logs_dispatcher.get_or_insert_with(|| {
                                source_context
                                    .dispatcher()
                                    .buffered_named("logs")
                                    .expect("logs output should exist")
                            });
                            if let Err(e) = dispatcher.push(log_event).await {
                                error!(error = %e, "Failed to dispatch log event.");
                            }
                        }
                    }
                    OtlpResource::Traces(resource_spans) => {
                        for trace_event in traces_translator.translate_spans(resource_spans, &metrics) {
                            let dispatcher = traces_dispatcher.get_or_insert_with(|| {
                                source_context
                                    .dispatcher()
                                    .buffered_named("traces")
                                    .expect("traces output should exist")
                            });
                            if let Err(e) = dispatcher.push(trace_event).await {
                                error!(error = %e, "Failed to dispatch trace event.");
                            }
                        }
                    }
                }
            },
            _ = buffer_flush.tick() => {
                if let Some(dispatcher) = metrics_dispatcher.take() {
                    if let Err(e) = dispatcher.flush().await {
                        error!(error = %e, "Failed to flush metric events.");
                    }
                }
                if let Some(dispatcher) = logs_dispatcher.take() {
                    if let Err(e) = dispatcher.flush().await {
                        error!(error = %e, "Failed to flush log events.");
                    }
                }
                if let Some(dispatcher) = traces_dispatcher.take() {
                    if let Err(e) = dispatcher.flush().await {
                        error!(error = %e, "Failed to flush trace events.");
                    }
                }
            },
            _ = &mut shutdown_handle => {
                debug!("Converter task received shutdown signal.");
                break;
            }
        }
    }

    if let Some(dispatcher) = metrics_dispatcher.take() {
        if let Err(e) = dispatcher.flush().await {
            error!(error = %e, "Failed to flush metric events.");
        }
    }
    if let Some(dispatcher) = logs_dispatcher.take() {
        if let Err(e) = dispatcher.flush().await {
            error!(error = %e, "Failed to flush log events.");
        }
    }
    if let Some(dispatcher) = traces_dispatcher.take() {
        if let Err(e) = dispatcher.flush().await {
            error!(error = %e, "Failed to flush trace events.");
        }
    }

    debug!("OTLP resource converter task stopped.");
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::domains::{otlp, traces};

    use super::*;

    #[test]
    fn from_configuration_maps_the_resolved_model() {
        let otlp = otlp::Domain {
            receiver: otlp::Receiver {
                metrics_enabled: true,
                logs_enabled: false,
                grpc: otlp::GrpcReceiver {
                    endpoint: "0.0.0.0:4317".to_string(),
                    transport: "tcp".to_string(),
                    max_recv_msg_size_mib: 8,
                },
                http: otlp::HttpReceiver {
                    endpoint: "0.0.0.0:4318".to_string(),
                    transport: "tcp".to_string(),
                },
            },
            contexts: otlp::Contexts {
                string_interner_size: 1024,
                cached_contexts_limit: 111,
                cached_tagsets_limit: 222,
                allow_context_heap_allocs: false,
            },
            ..Default::default()
        };

        let traces_otlp = traces::OtlpTraces {
            enabled: true,
            string_interner_size: 2048,
            ignore_missing_datadog_fields: true,
            enable_compute_top_level_by_span_kind: false,
            ..Default::default()
        };

        let config = OtlpConfiguration::from_configuration(&otlp, &traces_otlp).expect("builds from model");

        assert!(config.metrics_enabled);
        assert!(!config.logs_enabled);
        assert!(config.traces_enabled);
        assert_eq!(config.grpc_endpoint, "0.0.0.0:4317");
        assert_eq!(config.grpc_transport, "tcp");
        assert_eq!(config.grpc_max_recv_msg_size_mib, 8);
        assert_eq!(config.http_endpoint, "0.0.0.0:4318");
        assert_eq!(config.context_string_interner_bytes, 1024);
        assert_eq!(config.cached_contexts_limit, 111);
        assert_eq!(config.cached_tagsets_limit, 222);
        assert!(!config.allow_context_heap_allocations);
        assert_eq!(config.traces_string_interner_bytes, 2048);
        assert!(config.traces_ignore_missing_datadog_fields);
        assert!(!config.traces_enable_compute_top_level_by_span_kind);
        assert!(config.workload_provider.is_none());
    }
}
