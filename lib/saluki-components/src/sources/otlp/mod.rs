use std::net::ToSocketAddrs as _;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use otlp_protos::opentelemetry::proto::collector::trace::v1::ExportTraceServiceRequest;
use otlp_protos::opentelemetry::proto::logs::v1::ResourceLogs as OtlpResourceLogs;
use otlp_protos::opentelemetry::proto::trace::v1::ResourceSpans as OtlpResourceSpans;
use prost::Message;
use saluki_common::collections::FastHashSet;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        sources::{Source, SourceBuilder, SourceContext},
        BuildContext,
    },
    data_model::{
        event::{metric::context::ContextResolver, EventType},
        tags::{SharedTagSet, TagSet},
    },
    runtime,
    topology::{interconnect::BufferedDispatcher, EventsBuffer, OutputDefinition},
};
use saluki_env::WorkloadProvider;
use saluki_error::ErrorContext as _;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{server::http::Http2Config, ListenAddress};
use stringtheory::MetaString;
use tokio::pin;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error};

use crate::common::otlp::{
    build_metrics, receiver::OtlpReceiver, traces::translator::OtlpTracesSettings, CorsConfiguration, Metrics,
    OtlpHandler, OtlpServerConfiguration, OtlpTlsConfiguration,
};

mod logs;
mod metrics;
mod resolver;
use datadog_agent_apm::semantics::SemanticRegistryProvider;

use self::logs::translator::OtlpLogsTranslator;
pub use self::metrics::config::{
    CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue, OtlpMetricsSettings, SummaryMode,
};
use self::metrics::translator::OtlpMetricsTranslator;
use self::resolver::build_context_resolver;
pub use self::resolver::OtlpContextSettings;
use crate::common::otlp::origin::OtlpOriginTagResolver;
use crate::common::otlp::traces::translator::OtlpTracesTranslator;

/// Parses `otlp_config.metrics.tags` into a set of tags added to every emitted metric.
///
/// The value is a comma-separated list. An empty configuration yields no tags.
fn parse_configured_metric_tags(raw: &str) -> SharedTagSet {
    let mut tags = TagSet::default();
    for tag in raw.split(',') {
        let tag = tag.trim();
        if !tag.is_empty() {
            tags.insert_tag(tag);
        }
    }
    tags.into_shared()
}

/// Applies resolved static tags using replacement semantics.
fn apply_static_metric_tags(metrics: &mut OtlpMetricsSettings, static_tags: Vec<String>) {
    if !static_tags.is_empty() {
        metrics.tags = static_tags.join(",");
    }
}

/// Configuration for the OTLP source.
pub struct OtlpConfiguration {
    /// Hostname of metrics whose resource carries none.
    pub default_hostname: MetaString,

    /// Receiver settings.
    pub receiver: OtlpReceiver,

    /// Metrics translation settings.
    pub metrics: OtlpMetricsSettings,

    /// Whether OTLP traces are accepted.
    pub traces_enabled: bool,

    /// Trace translation settings.
    pub traces: OtlpTracesSettings,

    /// Context resolver settings.
    pub contexts: OtlpContextSettings,

    /// Maximum length of a span's resource name, in bytes.
    ///
    /// Resource names longer than this limit are truncated to the limit, on a UTF-8 boundary. If set to `0`, every
    /// resource name is truncated to an empty string. Use `usize::MAX` to never truncate them.
    pub max_resource_len: usize,

    /// Supplies attribute mappings for registry-based trace lookups.
    ///
    /// Pass a subscribed provider to receive remote mapping updates. The translator reads one registry snapshot per
    /// `ResourceSpans` batch, so an update cannot change mappings midway through that batch. Metrics and logs are
    /// unaffected.
    pub semantic_registry: SemanticRegistryProvider,

    /// Workload provider to utilize for origin detection/enrichment.
    pub workload_provider: Arc<dyn WorkloadProvider + Send + Sync>,
}

impl OtlpConfiguration {
    /// Replaces the configured metric tags when static tags are required.
    pub fn with_static_metric_tags(mut self, static_tags: Vec<String>) -> Self {
        apply_static_metric_tags(&mut self.metrics, static_tags);
        self
    }

    fn traces_translator(&self) -> OtlpTracesTranslator {
        OtlpTracesTranslator::new(
            self.traces.clone(),
            self.max_resource_len,
            self.semantic_registry.clone(),
        )
    }

    fn metrics_translator_config(&self) -> metrics::config::OtlpMetricsTranslatorConfig {
        metrics::config::OtlpMetricsTranslatorConfig::from_settings(&self.metrics)
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

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        if !self.receiver.metrics_enabled && !self.receiver.logs_enabled && !self.traces_enabled {
            return Err(generic_error!(
                "OTLP metrics, logs and traces support is disabled. Please enable at least one of them."
            ));
        }

        let grpc_listen_str = format!(
            "{}://{}",
            self.receiver.grpc.transport.as_str(),
            self.receiver.grpc.endpoint
        );
        let grpc_endpoint = ListenAddress::try_from(grpc_listen_str.as_str())
            .map_err(|e| generic_error!("Invalid gRPC endpoint address '{}': {}", grpc_listen_str, e))?;

        let http_endpoint_str = &self.receiver.http.endpoint;
        let http_socket_addr = http_endpoint_str
            .to_socket_addrs()
            .map_err(|e| generic_error!("Invalid HTTP endpoint address '{}': {}", http_endpoint_str, e))?
            .next()
            .ok_or_else(|| generic_error!("No addresses resolved for HTTP endpoint '{}'", http_endpoint_str))?;

        let origin_tag_resolver = OtlpOriginTagResolver::new(Arc::clone(&self.workload_provider));

        // Metrics resolve their full OTLP entity list at the resource boundary. Keep the context resolver free of an
        // origin resolver so it cannot apply the legacy RawOrigin-only lookup a second time. Logs retain that resolver.
        let context_resolver = build_context_resolver(&self.contexts, context.component_context(), None)?;
        let metrics_translator_config = self.metrics_translator_config();

        let metric_tags = parse_configured_metric_tags(&self.metrics.tags);
        let traces_translator = self.traces_translator();
        let grpc_max_recv_msg_size_bytes = self.receiver.grpc.max_recv_msg_size_mib as usize * 1024 * 1024;
        let grpc_http2_config = self.receiver.grpc.http2_config();
        let http_max_request_body_size = self.receiver.http.max_request_body_size;
        let cors = self.receiver.http.cors.clone();
        let http_tls_config = self.receiver.http.tls.build()?;
        let grpc_tls_config = self.receiver.grpc.tls.build()?;
        let metrics = build_metrics(context.component_context());
        let translator_metrics =
            metrics::telemetry::OtlpMetricsTranslatorMetrics::from_component_context(context.component_context());

        Ok(Box::new(Otlp {
            context_resolver,
            origin_tag_resolver,
            grpc_endpoint,
            http_endpoint: ListenAddress::Tcp(http_socket_addr),
            grpc_max_recv_msg_size_bytes,
            grpc_http2_config,
            http_max_request_body_size,
            metrics_translator_config,
            metric_tags,
            default_hostname: self.default_hostname.clone(),
            traces_translator,
            cors,
            http_tls_config,
            grpc_tls_config,
            metrics,
            translator_metrics,
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
    origin_tag_resolver: OtlpOriginTagResolver,
    grpc_endpoint: ListenAddress,
    http_endpoint: ListenAddress,
    grpc_max_recv_msg_size_bytes: usize,
    grpc_http2_config: Http2Config,
    http_max_request_body_size: u64,
    metrics_translator_config: metrics::config::OtlpMetricsTranslatorConfig,
    metric_tags: SharedTagSet,
    default_hostname: MetaString,
    traces_translator: OtlpTracesTranslator,
    cors: CorsConfiguration,
    http_tls_config: Option<OtlpTlsConfiguration>,
    grpc_tls_config: Option<OtlpTlsConfiguration>,
    metrics: Metrics, // Telemetry metrics, not DD native metrics.
    translator_metrics: metrics::telemetry::OtlpMetricsTranslatorMetrics,
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
            grpc_http2_config,
            http_max_request_body_size,
            metrics_translator_config,
            metric_tags,
            default_hostname,
            traces_translator,
            cors,
            http_tls_config,
            grpc_tls_config,
            metrics,
            translator_metrics,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let memory_limiter = context.topology_context().memory_limiter();

        // Create the internal channel for decoupling the servers from the converter.
        let (tx, rx) = mpsc::channel::<OtlpSignal>(1024);

        let metrics_translator = OtlpMetricsTranslator::new(
            metrics_translator_config,
            default_hostname,
            context_resolver,
            origin_tag_resolver.clone(),
            metric_tags,
            translator_metrics,
        )?;

        // Build our gRPC and HTTP servers and spawn them.
        let handler = SourceHandler::new(tx, metrics.clone());
        let mut server_config =
            OtlpServerConfiguration::new(http_endpoint, grpc_endpoint, grpc_max_recv_msg_size_bytes)
                .with_cors(cors)
                .with_grpc_http2_config(grpc_http2_config)
                .with_http_max_request_body_size(http_max_request_body_size);

        if let Some(tls) = http_tls_config {
            server_config = server_config.with_http_tls(tls);
        }
        if let Some(tls) = grpc_tls_config {
            server_config = server_config.with_grpc_tls(tls);
        }

        server_config
            .build(
                handler,
                memory_limiter.clone(),
                metrics.clone(),
                context.topology_context().global_thread_pool(),
            )
            .await?;

        // Run the converter task on the worker pool: translating OTLP resources is highly compute-bound.
        //
        // The converter stops once the servers exit and its channel closes, which can take longer than the default
        // timeout of a closure-based child. Thus, it has no timeout of its own, and is halted with the source if the
        // component's shutdown timeout expires.
        let converter_context = context.clone();
        runtime::worker(
            "resource_converter",
            run_converter(
                rx,
                converter_context,
                origin_tag_resolver,
                metrics_translator,
                metrics,
                traces_translator,
            ),
        )
        .on_runtime(context.topology_context().global_thread_pool().clone())
        .with_shutdown_timeout(Duration::MAX)
        .needed()
        .spawn();

        health.mark_ready();
        debug!("OTLP source started.");

        // When this body returns, the scope stops the servers, and the converter drains what they accepted.
        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
            }
        }

        debug!("OTLP source stopped.");

        Ok(())
    }
}

enum OtlpSignal {
    Metrics(ExportMetricsServiceRequest),
    Logs(OtlpResourceLogs),
    Traces(OtlpResourceSpans),
}

/// Handler that decodes OTLP bytes and sends resources to the converter.
struct SourceHandler {
    tx: mpsc::Sender<OtlpSignal>,
    metrics: Metrics,
}

impl SourceHandler {
    fn new(tx: mpsc::Sender<OtlpSignal>, metrics: Metrics) -> Self {
        Self { tx, metrics }
    }
}

#[async_trait]
impl OtlpHandler for SourceHandler {
    async fn handle_metrics(&self, body: Bytes) -> Result<(), GenericError> {
        let request = ExportMetricsServiceRequest::decode(body).map_err(|e| {
            self.metrics.metrics_errors_decode().increment(1);
            generic_error!("Failed to decode metrics export request: {}", e)
        })?;

        // Send the entire request as a single channel message so the converter processes it
        // atomically. This preserves the request boundary for usage beacon emission without
        // needing control markers or shared state across concurrent requests.
        self.tx.send(OtlpSignal::Metrics(request)).await.map_err(|e| {
            self.metrics.metrics_errors_channel().increment(1);
            generic_error!("Failed to send metrics request to converter: channel is closed: {}", e)
        })?;
        Ok(())
    }

    async fn handle_logs(&self, body: Bytes) -> Result<(), GenericError> {
        let request = ExportLogsServiceRequest::decode(body).error_context("Failed to decode logs export request.")?;

        for resource_logs in request.resource_logs {
            self.tx
                .send(OtlpSignal::Logs(resource_logs))
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
                .send(OtlpSignal::Traces(resource_spans))
                .await
                .error_context("Failed to send resource spans to converter: channel is closed.")?;
        }
        Ok(())
    }
}

async fn run_converter(
    mut receiver: mpsc::Receiver<OtlpSignal>, source_context: SourceContext,
    origin_tag_resolver: OtlpOriginTagResolver, mut metrics_translator: OtlpMetricsTranslator, metrics: Metrics,
    mut traces_translator: OtlpTracesTranslator,
) {
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
            maybe_signal = receiver.recv() => {
                let Some(otlp_signal) = maybe_signal else {
                    debug!("Converter task's input closed.");
                    break;
                };

                match otlp_signal {
                    OtlpSignal::Metrics(request) => {
                        let mut detected_languages = FastHashSet::default();

                        for resource_metrics in request.resource_metrics {
                            match metrics_translator.translate_metrics(resource_metrics, &metrics) {
                                Ok((events, languages)) => {
                                    detected_languages.extend(languages);
                                    for event in events {
                                        let dispatcher = metrics_dispatcher.get_or_insert_with(|| {
                                            source_context
                                                .dispatcher()
                                                .buffered_named("metrics")
                                                .expect("metrics output should exist")
                                        });
                                        if let Err(e) = dispatcher.push(event).await {
                                            error!(error = %e, "Failed to dispatch metric event.");
                                            metrics.metrics_errors_dispatch().increment(1);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!(error = %e, "Failed to handle resource metrics.");
                                }
                            }
                        }

                        // Emit usage beacon metrics for the completed request.
                        for event in metrics_translator.emit_usage_beacons(detected_languages) {
                            let dispatcher = metrics_dispatcher.get_or_insert_with(|| {
                                source_context
                                    .dispatcher()
                                    .buffered_named("metrics")
                                    .expect("metrics output should exist")
                            });
                            if let Err(e) = dispatcher.push(event).await {
                                error!(error = %e, "Failed to dispatch usage beacon metric event.");
                            }
                        }
                    }
                    OtlpSignal::Logs(resource_logs) => {
                        let translator = OtlpLogsTranslator::from_resource_logs(resource_logs, &origin_tag_resolver);
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
                    OtlpSignal::Traces(resource_spans) => {
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
                        metrics.metrics_errors_flush().increment(1);
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
        }
    }

    if let Some(dispatcher) = metrics_dispatcher.take() {
        if let Err(e) = dispatcher.flush().await {
            error!(error = %e, "Failed to flush metric events.");
            metrics.metrics_errors_flush().increment(1);
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
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use datadog_agent_apm::semantics::test_util::TestRegistryPublisher;
    use datadog_agent_apm::semantics::SemanticRegistryProvider;
    use otlp_protos::opentelemetry::proto::collector::logs::v1::logs_service_client::LogsServiceClient;
    use otlp_protos::opentelemetry::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use prost::Message;
    use saluki_core::components::{test_util::TestComponentDriver, ComponentContext};
    use saluki_core::data_model::origin::OriginTagCardinality;
    use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
    use saluki_io::net::BoundListenAddress;
    use saluki_metrics::test::TestRecorder;
    use stringtheory::MetaString;

    use super::{
        apply_static_metric_tags, parse_configured_metric_tags, CumulativeMonotonicMode, ExportLogsServiceRequest,
        HistogramMode, InitialCumulativeMonotonicValue, OtlpConfiguration, OtlpContextSettings, OtlpMetricsSettings,
        SummaryMode,
    };
    use crate::common::otlp::receiver::receiver;
    use crate::common::otlp::traces::translator::{
        custom_status_code_registry, traces_settings, translate_custom_status_code,
    };
    use crate::common::otlp::{build_metrics, Metrics, OtlpHandler};

    /// Returns metrics settings carrying the configuration defaults.
    fn metrics_settings() -> OtlpMetricsSettings {
        OtlpMetricsSettings {
            tag_cardinality: OriginTagCardinality::Low,
            histogram_mode: HistogramMode::Distributions,
            send_histogram_aggregations: false,
            resource_attributes_as_tags: false,
            instrumentation_scope_metadata_as_tags: true,
            cumulative_monotonic_mode: CumulativeMonotonicMode::ToDelta,
            initial_cumulative_monotonic_value: InitialCumulativeMonotonicValue::Auto,
            tags: String::new(),
            summary_mode: SummaryMode::Gauges,
            delta_ttl: Duration::from_secs(3600),
        }
    }

    /// Returns source settings carrying the configuration defaults.
    fn source_configuration() -> OtlpConfiguration {
        OtlpConfiguration {
            default_hostname: MetaString::default(),
            receiver: receiver(),
            metrics: metrics_settings(),
            traces_enabled: false,
            traces: traces_settings(),
            contexts: OtlpContextSettings {
                allow_context_heap_allocs: true,
                cached_contexts_limit: 500_000,
                cached_tagsets_limit: 500_000,
                string_interner_size: 2 * 1024 * 1024,
            },
            max_resource_len: usize::MAX,
            semantic_registry: SemanticRegistryProvider::default(),
            workload_provider: Arc::new(saluki_env::workload::providers::NoopWorkloadProvider),
        }
    }

    fn tags(raw: &str) -> Vec<String> {
        parse_configured_metric_tags(raw)
            .into_iter()
            .map(|t| t.to_string())
            .collect()
    }

    fn config_with_metrics(metrics: OtlpMetricsSettings) -> OtlpConfiguration {
        OtlpConfiguration {
            metrics,
            ..source_configuration()
        }
    }

    #[test]
    fn injected_provider_reaches_the_traces_translator() {
        let (publisher, provider) = TestRegistryPublisher::new();
        let mut config = source_configuration();
        config.traces.string_interner_size = NonZeroUsize::new(64 * 1024).unwrap();
        config.semantic_registry = provider;
        let mut translator = config.traces_translator();
        let metrics = Metrics::for_tests();
        assert_eq!(translate_custom_status_code(&mut translator, &metrics), None);

        publisher.assign([("a", custom_status_code_registry())]);
        assert_eq!(translate_custom_status_code(&mut translator, &metrics), Some(418.0));
    }

    #[test]
    fn empty_static_tags_preserve_explicit_otlp_metric_tags() {
        let mut metrics = metrics_settings();
        metrics.tags = "configured:true".to_string();

        apply_static_metric_tags(&mut metrics, Vec::new());

        assert_eq!(metrics.tags, "configured:true");
    }

    #[test]
    fn static_metric_tags_replace_explicit_otlp_metric_tags() {
        let config = config_with_metrics(OtlpMetricsSettings {
            tags: "configured:true".to_string(),
            ..metrics_settings()
        })
        .with_static_metric_tags(vec!["provider_kind:autopilot".to_string()]);

        assert_eq!(config.metrics.tags, "provider_kind:autopilot");
    }

    #[test]
    fn histogram_mode_flows_to_metrics_translator() {
        for mode in [
            HistogramMode::NoBuckets,
            HistogramMode::Counters,
            HistogramMode::Distributions,
        ] {
            let config = config_with_metrics(OtlpMetricsSettings {
                histogram_mode: mode,
                ..metrics_settings()
            });

            assert_eq!(config.metrics_translator_config().hist_mode, mode);
        }
    }

    #[test]
    fn summary_mode_flows_to_metrics_translator() {
        // `gauges` emits one gauge per quantile (quantiles on); `noquantiles` omits them.
        for (mode, expected_quantiles) in [(SummaryMode::Gauges, true), (SummaryMode::NoQuantiles, false)] {
            let config = config_with_metrics(OtlpMetricsSettings {
                summary_mode: mode,
                ..metrics_settings()
            });

            assert_eq!(config.metrics_translator_config().quantiles, expected_quantiles);
        }
    }

    #[test]
    fn histogram_aggregation_flows_to_metrics_translator() {
        for send in [false, true] {
            let config = config_with_metrics(OtlpMetricsSettings {
                send_histogram_aggregations: send,
                ..metrics_settings()
            });

            assert_eq!(config.metrics_translator_config().send_histogram_aggregations, send);
        }
    }

    #[test]
    fn nobuckets_with_histogram_aggregations_is_valid() {
        let config = config_with_metrics(OtlpMetricsSettings {
            histogram_mode: HistogramMode::NoBuckets,
            send_histogram_aggregations: true,
            ..metrics_settings()
        });

        assert!(config.metrics_translator_config().validate().is_ok());
    }

    #[test]
    fn nobuckets_without_histogram_aggregations_is_invalid() {
        // Match the Agent: `nobuckets` without aggregation metrics emits nothing and is invalid.
        let config = config_with_metrics(OtlpMetricsSettings {
            histogram_mode: HistogramMode::NoBuckets,
            send_histogram_aggregations: false,
            ..metrics_settings()
        });

        assert!(config.metrics_translator_config().validate().is_err());
    }

    #[test]
    fn cumulative_monotonic_mode_flows_to_metrics_translator() {
        for mode in [CumulativeMonotonicMode::ToDelta, CumulativeMonotonicMode::RawValue] {
            let config = config_with_metrics(OtlpMetricsSettings {
                cumulative_monotonic_mode: mode,
                ..metrics_settings()
            });

            assert_eq!(config.metrics_translator_config().cumulative_monotonic_mode, mode);
        }
    }

    #[test]
    fn initial_cumulative_monotonic_value_flows_to_metrics_translator() {
        for value in [
            InitialCumulativeMonotonicValue::Auto,
            InitialCumulativeMonotonicValue::Drop,
            InitialCumulativeMonotonicValue::Keep,
        ] {
            let config = config_with_metrics(OtlpMetricsSettings {
                initial_cumulative_monotonic_value: value,
                ..metrics_settings()
            });

            assert_eq!(
                config.metrics_translator_config().initial_cumulative_monotonic_value,
                value
            );
        }
    }

    #[test]
    fn delta_ttl_flows_to_metrics_translator() {
        // An explicit TTL overrides the 3600s default end-to-end into the translator config.
        let config = config_with_metrics(OtlpMetricsSettings {
            delta_ttl: Duration::from_secs(7200),
            ..metrics_settings()
        });

        assert_eq!(config.metrics_translator_config().delta_ttl, Duration::from_secs(7200));
    }

    #[test]
    fn tag_settings_flow_to_metrics_translator() {
        let config = config_with_metrics(OtlpMetricsSettings {
            tag_cardinality: OriginTagCardinality::High,
            resource_attributes_as_tags: true,
            ..metrics_settings()
        });
        let translator_config = config.metrics_translator_config();

        assert_eq!(translator_config.tag_cardinality, OriginTagCardinality::High);
        assert!(translator_config.resource_attributes_as_tags);
    }

    #[test]
    fn instrumentation_scope_metadata_as_tags_flows_to_metrics_translator() {
        let config = config_with_metrics(OtlpMetricsSettings {
            instrumentation_scope_metadata_as_tags: false,
            ..metrics_settings()
        });

        assert!(
            !config
                .metrics_translator_config()
                .instrumentation_scope_metadata_as_tags
        );
    }

    #[test]
    fn empty_configuration_yields_no_tags() {
        assert!(tags("").is_empty());
    }

    #[test]
    fn single_tag_is_parsed() {
        assert_eq!(tags("env:prod"), vec!["env:prod".to_string()]);
    }

    #[test]
    fn multiple_tags_are_split_on_comma() {
        assert_eq!(
            tags("env:prod,team:core"),
            vec!["env:prod".to_string(), "team:core".to_string()]
        );
    }

    #[test]
    fn duplicate_tags_are_deduplicated() {
        assert_eq!(tags("env:prod,env:prod"), vec!["env:prod".to_string()]);
    }

    #[test]
    fn whitespace_around_commas_is_stripped() {
        assert_eq!(
            tags("env:prod, team:core"),
            vec!["env:prod".to_string(), "team:core".to_string()]
        );
    }

    #[test]
    fn trailing_and_doubled_commas_produce_no_empty_tags() {
        assert_eq!(tags("env:prod,"), vec!["env:prod".to_string()]);
        assert_eq!(
            tags("env:prod,,team:core"),
            vec!["env:prod".to_string(), "team:core".to_string()]
        );
    }

    // -----------------------------------------------------------------------------------------------
    // Self-telemetry: server-level decode and channel error counters.
    // -----------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn source_handler_increments_decode_error_on_malformed_body() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let metrics = build_metrics(&ComponentContext::test_source("otlp_test"));
        let (tx, _rx) = tokio::sync::mpsc::channel::<super::OtlpSignal>(1);
        let handler = super::SourceHandler::new(tx, metrics);

        // Invalid protobuf bytes cause decode to fail.
        let result = handler.handle_metrics(bytes::Bytes::from_static(b"not protobuf")).await;
        assert!(result.is_err());

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("reason", "decode"),
        ];
        assert_eq!(recorder.counter(("component_errors_total", tags)), Some(1));
    }

    #[tokio::test]
    async fn source_handler_increments_channel_error_on_closed_channel() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        let metrics = build_metrics(&ComponentContext::test_source("otlp_test"));
        // Create a channel with no receiver, then drop the receiver so the send fails.
        let (tx, rx) = tokio::sync::mpsc::channel::<super::OtlpSignal>(1);
        drop(rx);
        let handler = super::SourceHandler::new(tx, metrics);

        // A valid (empty) request that decodes fine but can't be sent because the channel is closed.
        let request = otlp_protos::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest::default();
        let body = bytes::Bytes::from(request.encode_to_vec());
        let result = handler.handle_metrics(body).await;
        assert!(result.is_err());

        let tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("reason", "channel"),
        ];
        assert_eq!(recorder.counter(("component_errors_total", tags)), Some(1));

        // The decode counter should not have been incremented.
        let decode_tags: &[(&str, &str)] = &[
            ("component_id", "otlp_test"),
            ("component_type", "source"),
            ("reason", "decode"),
        ];
        assert_eq!(recorder.counter(("component_errors_total", decode_tags)), Some(0));
    }

    #[tokio::test]
    async fn accepted_logs_are_dispatched_before_the_source_stops() {
        // Many more records than two event buffers hold, so that the converter blocks on the output with most of the
        // requests still queued.
        const REQUESTS: usize = 16;
        const RECORDS: usize = 512;

        let mut config = source_configuration();
        config.receiver.logs_enabled = true;
        config.receiver.grpc.endpoint = "127.0.0.1:0".to_string();
        config.receiver.grpc.max_recv_msg_size_mib = 4;
        // The configuration translator resolves a zero keepalive to these defaults.
        config.receiver.grpc.keepalive.time = Duration::from_secs(2 * 60 * 60);
        config.receiver.grpc.keepalive.timeout = Duration::from_secs(20);
        config.receiver.http.endpoint = "127.0.0.1:0".to_string();

        let (mut control, mut outputs) = TestComponentDriver::options()
            .with_interconnect_capacity(NonZeroUsize::MIN)
            .source(config)
            .await
            .expect("source should build");
        let mut logs = outputs.take_named_output("logs");

        let mut bound = control
            .supervisor()
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact("http-server-otlp-grpc"));
        let address = match bound.recv().await {
            Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address))) => address,
            update => panic!("expected the gRPC server to bind, got {update:?}"),
        };
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .expect("endpoint should parse")
            .connect()
            .await
            .expect("should connect to the gRPC server");
        let mut client = LogsServiceClient::new(channel);
        for _ in 0..REQUESTS {
            let request = ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs {
                    scope_logs: vec![ScopeLogs {
                        log_records: vec![LogRecord::default(); RECORDS],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            };
            client.export(request).await.expect("export should succeed");
        }

        control.signal_shutdown();
        let (result, events) = tokio::join!(control.wait(), logs.collect_events());
        result.expect("source should stop cleanly");
        assert_eq!(
            events.len(),
            REQUESTS * RECORDS,
            "every accepted log must be dispatched"
        );
    }
}
