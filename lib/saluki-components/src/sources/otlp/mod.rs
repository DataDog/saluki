use std::net::ToSocketAddrs as _;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use agent_data_plane_config::domains;
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
use saluki_context::tags::{SharedTagSet, TagSet};
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

use crate::common::otlp::config::TracesConfig;
use crate::common::otlp::{build_metrics, Metrics, OtlpHandler, OtlpServerBuilder};

mod logs;
mod metrics;
mod resolver;
use self::logs::translator::OtlpLogsTranslator;
use self::metrics::translator::OtlpMetricsTranslator;
use self::resolver::build_context_resolver;
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

/// Configuration for the OTLP source.
#[derive(Default)]
pub struct OtlpConfiguration {
    default_hostname: MetaString,

    /// Resolved OTLP domain slice.
    otlp: domains::otlp::Domain,

    /// Workload provider to utilize for origin detection/enrichment.
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl OtlpConfiguration {
    /// Creates a new `OtlpConfiguration` from the resolved OTLP configuration.
    pub fn from_configuration(otlp: &domains::otlp::Domain) -> Self {
        Self {
            default_hostname: MetaString::default(),
            otlp: otlp.clone(),
            workload_provider: None,
        }
    }

    fn metrics_translator_config(&self) -> metrics::config::OtlpMetricsTranslatorConfig {
        metrics::config::OtlpMetricsTranslatorConfig::default()
            .with_remapping(true)
            .with_summary_mode(self.otlp.metrics.summaries.mode)
            .with_histogram_mode(self.otlp.metrics.histogram_mode)
            .with_send_histogram_aggregations(self.otlp.metrics.send_histogram_aggregations)
            .with_cumulative_monotonic_mode(self.otlp.metrics.sums.cumulative_monotonic_mode)
            .with_initial_cumulative_monotonic_value(self.otlp.metrics.sums.initial_cumulative_monotonic_value)
            .with_resource_attributes_as_tags(self.otlp.metrics.resource_attributes_as_tags)
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
        if !self.otlp.receiver.metrics_enabled && !self.otlp.receiver.logs_enabled && !self.otlp.traces.enabled {
            return Err(generic_error!(
                "OTLP metrics, logs and traces support is disabled. Please enable at least one of them."
            ));
        }

        let grpc_listen_str = format!(
            "{}://{}",
            self.otlp.receiver.grpc.transport, self.otlp.receiver.grpc.endpoint
        );
        let grpc_endpoint = ListenAddress::try_from(grpc_listen_str.as_str())
            .map_err(|e| generic_error!("Invalid gRPC endpoint address '{}': {}", grpc_listen_str, e))?;

        // Enforce the current limitation that we only support TCP for gRPC.
        if !matches!(grpc_endpoint, ListenAddress::Tcp(_)) {
            return Err(generic_error!("Only 'tcp' transport is supported for OTLP gRPC"));
        }

        let http_endpoint_str = &self.otlp.receiver.http.endpoint;
        let http_socket_addr = http_endpoint_str
            .to_socket_addrs()
            .map_err(|e| generic_error!("Invalid HTTP endpoint address '{}': {}", http_endpoint_str, e))?
            .next()
            .ok_or_else(|| generic_error!("No addresses resolved for HTTP endpoint '{}'", http_endpoint_str))?;

        let maybe_origin_tags_resolver = self.workload_provider.clone().map(OtlpOriginTagResolver::new);

        let context_resolver =
            build_context_resolver(&self.otlp.contexts, &context, maybe_origin_tags_resolver.clone())?;
        let metrics_translator_config = self.metrics_translator_config();

        let metric_tags = parse_configured_metric_tags(&self.otlp.metrics.tags);
        let traces_interner_size = std::num::NonZeroUsize::new(self.otlp.traces.string_interner_size as usize)
            .ok_or_else(|| generic_error!("otlp_config.traces.string_interner_size must be greater than 0"))?;

        // The OTLP decoder also uses this translator through its raw configuration path.
        let traces_config = TracesConfig {
            enable_otlp_compute_top_level_by_span_kind: self.otlp.traces.enable_compute_top_level_by_span_kind,
            ignore_missing_datadog_fields: self.otlp.traces.ignore_missing_datadog_fields,
            ..Default::default()
        };
        let traces_translator = OtlpTracesTranslator::new(traces_config, traces_interner_size);
        let grpc_max_recv_msg_size_bytes = self.otlp.receiver.grpc.max_recv_msg_size_mib as usize * 1024 * 1024;
        let metrics = build_metrics(&context);

        Ok(Box::new(Otlp {
            context_resolver,
            origin_tag_resolver: maybe_origin_tags_resolver,
            grpc_endpoint,
            http_endpoint: ListenAddress::Tcp(http_socket_addr),
            grpc_max_recv_msg_size_bytes,
            metrics_translator_config,
            metric_tags,
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
    metric_tags: SharedTagSet,
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
            metric_tags,
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

        let metrics_translator = OtlpMetricsTranslator::new(
            metrics_translator_config,
            default_hostname,
            context_resolver,
            metric_tags,
        )?;

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
    use agent_data_plane_config::domains;
    use agent_data_plane_config::domains::otlp::{
        CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue, SummaryMode,
    };

    use super::{parse_configured_metric_tags, OtlpConfiguration};

    fn tags(raw: &str) -> Vec<String> {
        parse_configured_metric_tags(raw)
            .into_iter()
            .map(|t| t.to_string())
            .collect()
    }

    fn config_with_metrics(metrics: domains::otlp::Metrics) -> OtlpConfiguration {
        let otlp = domains::otlp::Domain {
            metrics,
            ..Default::default()
        };
        OtlpConfiguration::from_configuration(&otlp)
    }

    #[test]
    fn histogram_mode_flows_to_metrics_translator() {
        for mode in [
            HistogramMode::NoBuckets,
            HistogramMode::Counters,
            HistogramMode::Distributions,
        ] {
            let config = config_with_metrics(domains::otlp::Metrics {
                histogram_mode: mode,
                ..Default::default()
            });

            assert_eq!(config.metrics_translator_config().hist_mode, mode);
        }
    }

    #[test]
    fn summary_mode_flows_to_metrics_translator() {
        // `gauges` emits one gauge per quantile (quantiles on); `noquantiles` omits them.
        for (mode, expected_quantiles) in [(SummaryMode::Gauges, true), (SummaryMode::NoQuantiles, false)] {
            let config = config_with_metrics(domains::otlp::Metrics {
                summaries: domains::otlp::Summaries { mode },
                ..Default::default()
            });

            assert_eq!(config.metrics_translator_config().quantiles, expected_quantiles);
        }
    }

    #[test]
    fn histogram_aggregation_flows_to_metrics_translator() {
        for send in [false, true] {
            let config = config_with_metrics(domains::otlp::Metrics {
                send_histogram_aggregations: send,
                ..Default::default()
            });

            assert_eq!(config.metrics_translator_config().send_histogram_aggregations, send);
        }
    }

    #[test]
    fn nobuckets_with_histogram_aggregations_is_valid() {
        let config = config_with_metrics(domains::otlp::Metrics {
            histogram_mode: HistogramMode::NoBuckets,
            send_histogram_aggregations: true,
            ..Default::default()
        });

        assert!(config.metrics_translator_config().validate().is_ok());
    }

    #[test]
    fn nobuckets_without_histogram_aggregations_is_invalid() {
        // Match the Agent: `nobuckets` without aggregation metrics emits nothing and is invalid.
        let config = config_with_metrics(domains::otlp::Metrics {
            histogram_mode: HistogramMode::NoBuckets,
            send_histogram_aggregations: false,
            ..Default::default()
        });

        assert!(config.metrics_translator_config().validate().is_err());
    }

    #[test]
    fn cumulative_monotonic_sum_mode_defaults_to_delta_conversion() {
        assert_eq!(
            OtlpConfiguration::default()
                .metrics_translator_config()
                .cumulative_monotonic_mode,
            CumulativeMonotonicMode::ToDelta
        );
    }

    #[test]
    fn cumulative_monotonic_mode_flows_to_metrics_translator() {
        for mode in [CumulativeMonotonicMode::ToDelta, CumulativeMonotonicMode::RawValue] {
            let config = config_with_metrics(domains::otlp::Metrics {
                sums: domains::otlp::Sums {
                    cumulative_monotonic_mode: mode,
                    ..Default::default()
                },
                ..Default::default()
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
            let config = config_with_metrics(domains::otlp::Metrics {
                sums: domains::otlp::Sums {
                    initial_cumulative_monotonic_value: value,
                    ..Default::default()
                },
                ..Default::default()
            });

            assert_eq!(
                config.metrics_translator_config().initial_cumulative_monotonic_value,
                value
            );
        }
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
}
