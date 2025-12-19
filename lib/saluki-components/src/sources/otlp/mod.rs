use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use otlp_protos::opentelemetry::proto::metrics::v1::ResourceMetrics as OtlpResourceMetrics;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans as OtlpResourceSpans;
use prost::Message;
use saluki_common::task::HandleExt as _;
use saluki_config::GenericConfiguration;
use saluki_context::ContextResolver;
use saluki_core::topology::interconnect::EventBufferManager;
use saluki_core::topology::shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle};
use saluki_core::{
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
    topology::{EventsBuffer, OutputDefinition},
};
use saluki_env::WorkloadProvider;
use saluki_error::ErrorContext as _;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::ListenAddress;
use serde::Deserialize;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error};

use crate::common::otlp::config::Receiver;
use crate::common::otlp::{build_metrics, Metrics, OtlpHandler, OtlpServerBuilder};

mod attributes;
mod logs;
mod metrics;
mod origin;
mod resolver;
pub mod traces;
use self::logs::translator::OtlpLogsTranslator;
use self::metrics::translator::OtlpMetricsTranslator;
use self::origin::OtlpOriginTagResolver;
use self::resolver::build_context_resolver;
use self::traces::translator::OtlpTracesTranslator;

const fn default_context_string_interner_size() -> ByteSize {
    ByteSize::mib(2)
}

const fn default_cached_contexts_limit() -> usize {
    500_000
}

const fn default_cached_tagsets_limit() -> usize {
    500_000
}

const fn default_allow_context_heap_allocations() -> bool {
    true
}

/// Configuration for the OTLP source.
#[derive(Deserialize, Default)]
pub struct OtlpConfiguration {
    otlp_config: OtlpConfig,

    /// Total size of the string interner used for contexts.
    ///
    /// This controls the amount of memory that can be used to intern metric names and tags. If the interner is full,
    /// metrics with contexts that have not already been resolved may or may not be dropped, depending on the value of
    /// `allow_context_heap_allocations`.
    #[serde(
        rename = "otlp_string_interner_size",
        default = "default_context_string_interner_size"
    )]
    context_string_interner_bytes: ByteSize,

    /// The maximum number of cached contexts to allow.
    ///
    /// This is the maximum number of resolved contexts that can be cached at any given time. This limit does not affect
    /// the total number of contexts that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(rename = "otlp_cached_contexts_limit", default = "default_cached_contexts_limit")]
    cached_contexts_limit: usize,

    /// The maximum number of cached tagsets to allow.
    ///
    /// This is the maximum number of resolved tagsets that can be cached at any given time. This limit does not affect
    /// the total number of tagsets that can be _alive_ at any given time, which is dependent on the interner capacity
    /// and whether or not heap allocations are allowed.
    ///
    /// Defaults to 500,000.
    #[serde(rename = "otlp_cached_tagsets_limit", default = "default_cached_tagsets_limit")]
    cached_tagsets_limit: usize,

    /// Whether or not to allow heap allocations when resolving contexts.
    ///
    /// When resolving contexts during parsing, the metric name and tags are interned to reduce memory usage. The
    /// interner has a fixed size, however, which means some strings can fail to be interned if the interner is full.
    /// When set to `true`, we allow these strings to be allocated on the heap like normal, but this can lead to
    /// increased (unbounded) memory usage. When set to `false`, if the metric name and all of its tags cannot be
    /// interned, the metric is skipped.
    ///
    /// Defaults to `true`.
    #[serde(
        rename = "otlp_allow_context_heap_allocs",
        default = "default_allow_context_heap_allocations"
    )]
    allow_context_heap_allocations: bool,

    /// Workload provider to utilize for origin detection/enrichment.
    #[serde(skip)]
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

#[derive(Deserialize, Debug, Default)]
pub struct OtlpConfig {
    #[serde(default)]
    receiver: Receiver,
    #[serde(default)]
    metrics: MetricsConfig,
    #[serde(default)]
    logs: LogsConfig,
    #[serde(default)]
    traces: TracesConfig,
}

#[derive(Deserialize, Debug)]
pub struct LogsConfig {
    /// Whether to enable OTLP logs support.
    ///
    /// Defaults to true.
    #[serde(default = "default_logs_enabled")]
    pub enabled: bool,
}

fn default_logs_enabled() -> bool {
    true
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            enabled: default_logs_enabled(),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct MetricsConfig {
    /// Whether to enable OTLP metrics support.
    ///
    /// Defaults to true.
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
}

fn default_metrics_enabled() -> bool {
    true
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct TracesConfig {
    /// Whether to enable OTLP traces support.
    ///
    /// Defaults to true.
    #[serde(default = "default_traces_enabled")]
    pub enabled: bool,
    /// Whether to skip deriving Datadog fields from standard OTLP attributes.
    ///
    /// Mirrors the agent's `otlp_config.traces.ignore_missing_datadog_fields`.
    #[serde(default)]
    pub ignore_missing_datadog_fields: bool,
    /// When true, `_top_level` and `_dd.measured` are derived using the OTLP span kind,
    #[serde(default = "default_enable_otlp_compute_top_level_by_span_kind")]
    pub enable_otlp_compute_top_level_by_span_kind: bool,
}

const fn default_enable_otlp_compute_top_level_by_span_kind() -> bool {
    true
}

fn default_traces_enabled() -> bool {
    true
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: default_traces_enabled(),
            ignore_missing_datadog_fields: false,
            enable_otlp_compute_top_level_by_span_kind: default_enable_otlp_compute_top_level_by_span_kind(),
        }
    }
}
impl OtlpConfiguration {
    /// Creates a new `OTLPConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Sets the workload provider to use for configuring origin detection/enrichment.
    ///
    /// A workload provider must be set otherwise origin detection/enrichment will not be enabled.
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
        if !self.otlp_config.metrics.enabled && !self.otlp_config.logs.enabled && !self.otlp_config.traces.enabled {
            return Err(generic_error!(
                "OTLP metrics, logs and traces support is disabled. Please enable at least one of them."
            ));
        }

        let grpc_listen_str = format!(
            "{}://{}",
            self.otlp_config.receiver.protocols.grpc.transport, self.otlp_config.receiver.protocols.grpc.endpoint
        );
        let grpc_endpoint = ListenAddress::try_from(grpc_listen_str.as_str())
            .map_err(|e| generic_error!("Invalid gRPC endpoint address '{}': {}", grpc_listen_str, e))?;

        // Enforce the current limitation that we only support TCP for gRPC.
        if !matches!(grpc_endpoint, ListenAddress::Tcp(_)) {
            return Err(generic_error!("Only 'tcp' transport is supported for OTLP gRPC"));
        }

        let http_socket_addr = self.otlp_config.receiver.protocols.http.endpoint.parse().map_err(|e| {
            generic_error!(
                "Invalid HTTP endpoint address '{}': {}",
                self.otlp_config.receiver.protocols.http.endpoint,
                e
            )
        })?;

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(OtlpOriginTagResolver::new);

        let context_resolver = build_context_resolver(self, &context, maybe_origin_tags_resolver.clone())?;
        let metrics_translator_config = metrics::config::OtlpMetricsTranslatorConfig::default().with_remapping(true);
        let traces_translator_config = traces::config::OtlpTracesTranslatorConfig::default()
            .with_ignore_missing_datadog_fields(self.otlp_config.traces.ignore_missing_datadog_fields)
            .with_compute_top_level_by_span_kind(self.otlp_config.traces.enable_otlp_compute_top_level_by_span_kind);
        let grpc_max_recv_msg_size_bytes =
            self.otlp_config.receiver.protocols.grpc.max_recv_msg_size_mib as usize * 1024 * 1024;
        let metrics = build_metrics(&context);

        Ok(Box::new(Otlp {
            context_resolver,
            origin_tag_resolver: maybe_origin_tags_resolver,
            grpc_endpoint,
            http_endpoint: ListenAddress::Tcp(http_socket_addr),
            grpc_max_recv_msg_size_bytes,
            metrics_translator_config,
            traces_translator_config,
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
    traces_translator_config: traces::config::OtlpTracesTranslatorConfig,
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
            traces_translator_config,
            metrics,
        } = *self;

        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter();

        // Create the internal channel for decoupling the servers from the converter.
        let (tx, rx) = mpsc::channel::<OtlpResource>(1024);

        let mut converter_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let metrics_translator = OtlpMetricsTranslator::new(metrics_translator_config, context_resolver);

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
                traces_translator_config,
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
        converter_shutdown_coordinator.shutdown().await;

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

async fn dispatch_events(mut events: EventsBuffer, source_context: &SourceContext) {
    if events.is_empty() {
        return;
    }

    if events.has_event_type(EventType::Trace) {
        let mut buffered_dispatcher = source_context
            .dispatcher()
            .buffered_named("traces")
            .expect("traces output should exist");
        for trace_event in events.extract(Event::is_trace) {
            if let Err(e) = buffered_dispatcher.push(trace_event).await {
                error!(error = %e, "Failed to dispatch trace(s).");
            }
        }
        if let Err(e) = buffered_dispatcher.flush().await {
            error!(error = %e, "Failed to flush trace(s).");
        }
    }

    if events.has_event_type(EventType::Log) {
        let mut buffered_dispatcher = source_context
            .dispatcher()
            .buffered_named("logs")
            .expect("logs output should exist");

        for log_event in events.extract(Event::is_log) {
            if let Err(e) = buffered_dispatcher.push(log_event).await {
                error!(error = %e, "Failed to dispatch log(s).");
            }
        }

        if let Err(e) = buffered_dispatcher.flush().await {
            error!(error = %e, "Failed to flush log(s).");
        }
    }

    let len = events.len();
    if let Err(e) = source_context.dispatcher().dispatch_named("metrics", events).await {
        error!(error = %e, "Failed to dispatch metric events.");
    } else {
        debug!(events_len = len, "Dispatched metric events.");
    }
}

async fn run_converter(
    mut receiver: mpsc::Receiver<OtlpResource>, source_context: SourceContext,
    origin_tag_resolver: Option<OtlpOriginTagResolver>, shutdown_handle: DynamicShutdownHandle,
    mut metrics_translator: OtlpMetricsTranslator, metrics: Metrics,
    traces_translator_config: traces::config::OtlpTracesTranslatorConfig,
) {
    tokio::pin!(shutdown_handle);
    debug!("OTLP resource converter task started.");

    // Set a buffer flush interval of 100ms, which will ensure we always flush buffered events at least every 100ms if
    // we're otherwise idle and not receiving packets from the client.
    let mut buffer_flush = interval(Duration::from_millis(100));
    buffer_flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut event_buffer_manager = EventBufferManager::default();

    let traces_translator = OtlpTracesTranslator::new(traces_translator_config);

    loop {
        select! {
            Some(otlp_resource) = receiver.recv() => {
                match otlp_resource {
                    OtlpResource::Metrics(resource_metrics) => {
                        match metrics_translator.map_metrics(resource_metrics, &metrics) {
                            Ok(events) => {
                                for event in events {
                                    if let Some(event_buffer) = event_buffer_manager.try_push(event) {
                                        dispatch_events(event_buffer, &source_context).await;
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

                            if let Some(event_buffer) = event_buffer_manager.try_push(log_event) {
                                dispatch_events(event_buffer, &source_context).await;
                            }
                        }
                    }
                    OtlpResource::Traces(resource_spans) => {
                        let trace_events =
                            traces_translator.translate_resource_spans(resource_spans, &metrics);
                        for trace_event in trace_events {
                            if let Some(event_buffer) = event_buffer_manager.try_push(trace_event) {
                                dispatch_events(event_buffer, &source_context).await;
                            }
                        }
                    }
                }
            },
            _ = buffer_flush.tick() => {
                if let Some(event_buffer) = event_buffer_manager.consume() {
                    dispatch_events(event_buffer, &source_context).await;
                }
            },
            _ = &mut shutdown_handle => {
                debug!("Converter task received shutdown signal.");
                break;
            }
        }
    }

    if let Some(event_buffer) = event_buffer_manager.consume() {
        dispatch_events(event_buffer, &source_context).await;
    }

    debug!("OTLP resource converter task stopped.");
}
