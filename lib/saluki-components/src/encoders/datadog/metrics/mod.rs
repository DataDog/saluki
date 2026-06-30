use std::{collections::VecDeque, ops::Range, time::Duration};

use async_trait::async_trait;
use ddsketch::DDSketch;
use facet::Facet;
use http::{HeaderValue, Method, Request};
use protobuf::{rt::WireType, CodedOutputStream};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{
    buf::{ChunkedBytesBuffer, FrozenChunkedBytesBuffer},
    iter::ReusableDeduplicator,
    task::HandleExt as _,
};
use saluki_config::GenericConfiguration;
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_core::{
    components::{encoders::*, ComponentContext},
    data_model::{
        event::{
            metric::{Metric, MetricOrigin, MetricValues},
            EventType,
        },
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
    },
    observability::ComponentMetricsExt as _,
    topology::{EventsBuffer, PayloadsBuffer},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::compression::{CompressionScheme, Compressor};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use tokio::{io::AsyncWriteExt as _, select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};
use url::Url;
use uuid::Uuid;

#[cfg(test)]
use self::shadow::shadow_sample_matches;
use self::{
    shadow::{SeriesShadowConfig, SeriesShadowState},
    v3::{
        V3EncodedMetrics, V3EncodedRequest, V3EncoderStats, V3MetricType, V3PayloadLimits, V3PayloadRequest,
        V3PayloadSplitReason, V3SerializerTelemetry, V3Writer,
    },
};
use crate::{
    common::datadog::{
        clamp_payload_limits, default_serializer_compressor_kind,
        endpoints::{series_v3_config_can_enable_v3, AdditionalEndpoints},
        io::RB_BUFFER_CHUNK_SIZE,
        protocol::{MetricsPayloadInfo, UseV3ApiSeriesConfig, V3ApiConfig},
        request_builder::RequestBuilder,
        telemetry::ComponentTelemetry,
        DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT, DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT, METRICS_SERIES_V3_PATH,
        METRICS_SKETCHES_V3_PATH,
    },
    encoders::datadog::metrics::v2::MetricsEndpointEncoder,
};

mod endpoint;
use self::endpoint::{EndpointConfiguration, MetricsEndpoint};

mod shadow;
mod v1;
mod v2;
mod v3;

const V3_SERIES_ENDPOINT_URI: &str = METRICS_SERIES_V3_PATH;
const V3_SKETCHES_ENDPOINT_URI: &str = METRICS_SKETCHES_V3_PATH;

const fn default_max_metrics_per_payload() -> usize {
    10_000
}

const fn default_max_payload_size() -> usize {
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT
}

const fn default_max_uncompressed_payload_size() -> usize {
    DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT
}

const fn default_max_series_payload_size() -> usize {
    v2::SERIES_V2_COMPRESSED_SIZE_LIMIT
}

const fn default_max_series_uncompressed_payload_size() -> usize {
    v2::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT
}

const fn default_max_series_points_per_payload() -> usize {
    10_000
}

const fn default_flush_timeout_secs() -> u64 {
    2
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

const fn default_use_v2_api_series() -> bool {
    true
}

const fn default_log_payloads() -> bool {
    false
}

fn series_shadow_config_for_endpoint(
    series_endpoint: MetricsEndpoint, sample_rate: f64, metrics_v3_disabled_by_compressor: bool,
) -> SeriesShadowConfig {
    SeriesShadowConfig::new(
        if !metrics_v3_disabled_by_compressor && series_endpoint == MetricsEndpoint::SeriesV2 {
            sample_rate
        } else {
            0.0
        },
    )
}

/// Encoding mode for a metrics endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricsEncoderMode {
    /// Send V2 payloads only.
    V2Only,
    /// V3 is enabled for at least one endpoint; generate tagged V2 and V3 payloads so each endpoint
    /// receives the protocol version configured for it.
    V3Enabled,
    /// Send both V2 and V3 payloads simultaneously with a shared batch ID for backend validation.
    Validation,
}

impl MetricsEncoderMode {
    fn from_config(use_v3: bool, validate: bool) -> Self {
        match (use_v3, validate) {
            (false, _) => Self::V2Only,
            (true, false) => Self::V3Enabled,
            (true, true) => Self::Validation,
        }
    }

    fn needs_v3(self) -> bool {
        matches!(self, Self::V3Enabled | Self::Validation)
    }

    fn needs_batch_id(self) -> bool {
        matches!(self, Self::Validation)
    }

    fn needs_tagging(self) -> bool {
        matches!(self, Self::V3Enabled | Self::Validation)
    }
}

fn metrics_encoder_mode_for_config(
    use_v3: bool, validate: bool, metrics_v3_disabled_by_compressor: bool,
) -> MetricsEncoderMode {
    let use_v3 = use_v3 && !metrics_v3_disabled_by_compressor;
    MetricsEncoderMode::from_config(use_v3, use_v3 && validate)
}

fn selected_metrics_primary_v3_override(
    opw_enabled: bool, opw_url: &str, opw_use_v3_series: bool, vector_enabled: bool, vector_url: &str,
    vector_use_v3_series: bool,
) -> Option<bool> {
    if opw_enabled {
        metrics_primary_v3_override_for_url(opw_url, opw_use_v3_series)
    } else if vector_enabled {
        metrics_primary_v3_override_for_url(vector_url, vector_use_v3_series)
    } else {
        None
    }
}

fn metrics_primary_v3_override_for_url(url: &str, use_v3_series: bool) -> Option<bool> {
    metrics_primary_url_can_resolve(url).then_some(use_v3_series)
}

fn metrics_primary_url_can_resolve(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() {
        return false;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        Url::parse(url).is_ok_and(|url| url.host_str().is_some())
    } else {
        Url::parse(&format!("https://{url}")).is_ok_and(|url| url.host_str().is_some())
    }
}

fn series_v3_can_be_enabled_for_config(
    serializer_use_v3_series: bool, metrics_primary_v3_override: Option<bool>, has_additional_endpoints: bool,
    series_config: &UseV3ApiSeriesConfig,
) -> bool {
    serializer_use_v3_series
        || metrics_primary_v3_override == Some(true)
        || ((metrics_primary_v3_override != Some(false) || has_additional_endpoints)
            && series_v3_config_can_enable_v3(series_config))
}

/// Datadog Metrics encoder.
///
/// Generates Datadog metrics payloads for the Datadog platform.
#[derive(Clone, Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct DatadogMetricsConfiguration {
    /// Maximum number of input metrics to encode into a single request payload.
    ///
    /// This applies both to the series and sketches endpoints.
    ///
    /// Defaults to 10,000.
    #[serde(
        rename = "serializer_max_metrics_per_payload",
        default = "default_max_metrics_per_payload"
    )]
    max_metrics_per_payload: usize,

    /// Maximum compressed size, in bytes, of generic payloads.
    ///
    /// This applies to V1 JSON series payloads and sketch payloads, matching the Datadog Agent's generic payload
    /// builder. V2 series payloads use `serializer_max_series_payload_size` instead. The effective value is clamped to
    /// the Agent's default intake-safe limit of 2,621,440 bytes, so larger configured values do not allow payloads that
    /// intake may reject. If set to `0`, every non-empty compressed payload exceeds the limit and is dropped during
    /// flush.
    ///
    /// Defaults to 2,621,440 bytes.
    #[serde(rename = "serializer_max_payload_size", default = "default_max_payload_size")]
    max_payload_size: usize,

    /// Maximum uncompressed size, in bytes, of generic payloads.
    ///
    /// This applies to V1 JSON series payloads and sketch payloads, matching the Datadog Agent's generic payload
    /// builder. V2 series payloads use `serializer_max_series_uncompressed_payload_size` instead. The effective value
    /// is clamped to the Agent's default intake-safe limit of 4,194,304 bytes, so larger configured values do not allow
    /// payloads that intake may reject. Values smaller than the minimum endpoint framing size prevent the request
    /// builder from starting.
    ///
    /// Defaults to 4,194,304 bytes.
    #[serde(
        rename = "serializer_max_uncompressed_payload_size",
        default = "default_max_uncompressed_payload_size"
    )]
    max_uncompressed_payload_size: usize,

    /// Maximum compressed size, in bytes, of a V2 series payload.
    ///
    /// This applies only when `use_v2_api.series` is `true`. V1 series and sketches use `serializer_max_payload_size`
    /// instead. The effective value is clamped to the V2 series API limit of 512,000 bytes, so larger configured values
    /// do not allow payloads that intake would reject. High-throughput workloads may increase this up to that API limit
    /// to reduce request count, at the cost of larger individual requests. If set to `0`, every non-empty compressed
    /// payload exceeds the limit and is dropped during flush.
    ///
    /// Defaults to 512,000 bytes.
    #[serde(
        rename = "serializer_max_series_payload_size",
        default = "default_max_series_payload_size"
    )]
    max_series_payload_size: usize,

    /// Maximum uncompressed size, in bytes, of a V2 series payload.
    ///
    /// This applies only when `use_v2_api.series` is `true`. V1 series and sketches use
    /// `serializer_max_uncompressed_payload_size` instead. The effective value is clamped to the V2 series API limit of
    /// 5,242,880 bytes, so larger configured values do not allow payloads that intake would reject. This limit protects
    /// the encoder before compression, so compressed payload size may still force a separate flush. Values smaller than
    /// the minimum endpoint framing size prevent the request builder from starting.
    ///
    /// Defaults to 5,242,880 bytes.
    #[serde(
        rename = "serializer_max_series_uncompressed_payload_size",
        default = "default_max_series_uncompressed_payload_size"
    )]
    max_series_uncompressed_payload_size: usize,

    /// Maximum number of data points, across all series, to encode into a single series request payload.
    ///
    /// This applies only to series metrics (counters, gauges, rates, sets) and not to sketch metrics (histograms,
    /// distributions). A single metric series may contribute multiple data points when it carries more than one
    /// timestamp/value pair. When encoding an input would cause the running data point total to exceed this limit, the
    /// current payload is flushed first and the input is placed in the next payload.
    ///
    /// Defaults to 10,000.
    #[serde(
        rename = "serializer_max_series_points_per_payload",
        default = "default_max_series_points_per_payload"
    )]
    max_series_points_per_payload: usize,

    /// Flush timeout for pending requests, in seconds.
    ///
    /// When the destination has written metrics to the in-flight request payload, but it has not yet reached the
    /// payload size limits that would force the payload to be flushed, the destination will wait for a period of time
    /// before flushing the in-flight request payload. This allows for the possibility of other events to be processed
    /// and written into the request payload, thereby maximizing the payload size and reducing the number of requests
    /// generated and sent overall.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_flush_timeout_secs")]
    flush_timeout_secs: u64,

    /// Compression kind to use for the request payloads.
    ///
    /// Defaults to `zstd`.
    #[serde(
        rename = "serializer_compressor_kind",
        default = "default_serializer_compressor_kind"
    )]
    compressor_kind: String,

    /// Compressor level to use when the compressor kind is `zstd`.
    ///
    /// Defaults to 3.
    #[serde(
        rename = "serializer_zstd_compressor_level",
        default = "default_zstd_compressor_level"
    )]
    zstd_compressor_level: i32,

    /// Whether to use the V2 API for series metrics.
    ///
    /// When `true` (the default), series metrics are sent to the V2 protobuf endpoint (`/api/v2/series`). When
    /// `false`, series metrics are sent to the legacy V1 JSON endpoint (`/api/v1/series`). Sketch metrics always use
    /// the V2 endpoint (`/api/beta/sketches`) regardless of this setting.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_use_v2_api_series")]
    use_v2_api_series: bool,

    /// Whether to log metric payload contents before encoding.
    ///
    /// This logs decoded metric objects, not the encoded JSON/protobuf HTTP body.
    ///
    /// Defaults to `false`.
    #[serde(default = "default_log_payloads")]
    log_payloads: bool,

    /// Additional tags to apply to all forwarded metrics.
    #[serde(default, skip)]
    #[facet(opaque)]
    additional_tags: Option<SharedTagSet>,

    /// V3 API configuration for per-endpoint V3 support.
    ///
    /// Configures which endpoints receive V3 payloads and whether validation mode is enabled.
    #[serde(rename = "serializer_experimental_use_v3_api", default)]
    v3_api: V3ApiConfig,

    /// Agent-compatible V3 API configuration for series metrics.
    #[serde(flatten)]
    use_v3_api_series: UseV3ApiSeriesConfig,

    /// ADP safety gate for authoritative V3 series metrics.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "data_plane_metrics_v3_series_enabled")]
    data_plane_metrics_v3_series_enabled: bool,

    /// Enables routing all metrics to Observability Pipelines Worker.
    #[serde(default, rename = "observability_pipelines_worker_metrics_enabled")]
    observability_pipelines_worker_metrics_enabled: bool,

    /// Endpoint of the Observability Pipelines Worker instance to route metrics to.
    #[serde(default, rename = "observability_pipelines_worker_metrics_url")]
    observability_pipelines_worker_metrics_url: String,

    /// Enables V3 series metrics when routing to Observability Pipelines Worker.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "observability_pipelines_worker_metrics_use_v3_api_series")]
    observability_pipelines_worker_metrics_use_v3_api_series: bool,

    /// Enables routing all metrics to Vector.
    #[serde(default, rename = "vector_metrics_enabled")]
    vector_metrics_enabled: bool,

    /// Endpoint of the Vector instance to route metrics to.
    #[serde(default, rename = "vector_metrics_url")]
    vector_metrics_url: String,

    /// Enables V3 series metrics when routing to Vector.
    ///
    /// Deprecated in favor of `observability_pipelines_worker.metrics.use_v3_api.series`.
    ///
    /// Defaults to `false`.
    #[serde(default, rename = "vector_metrics_use_v3_api_series")]
    vector_metrics_use_v3_api_series: bool,

    /// Additional endpoints that metrics may be dual-shipped to.
    #[serde(default)]
    additional_endpoints: AdditionalEndpoints,
}

impl DatadogMetricsConfiguration {
    /// Creates a new `DatadogMetricsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Sets additional tags to be applied uniformly to all metrics forwarded by this destination.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        self.additional_tags = Some(additional_tags);
        self
    }

    fn v3_payload_limits(&self) -> V3PayloadLimits {
        V3PayloadLimits::new(
            self.max_series_payload_size,
            self.max_series_uncompressed_payload_size,
            self.max_metrics_per_payload,
            self.max_series_points_per_payload,
        )
    }
}

#[async_trait]
impl EncoderBuilder for DatadogMetricsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let v3_serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);

        let v2_compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);
        let v3_compression_scheme = if self.v3_api.compression_level > 0 {
            CompressionScheme::new(&self.compressor_kind, self.v3_api.compression_level)
        } else {
            v2_compression_scheme
        };
        let series_endpoint_uri = if self.v3_api.series.use_beta {
            self.v3_api.series.beta_route.clone()
        } else {
            V3_SERIES_ENDPOINT_URI.to_string()
        };
        let shadow_series_endpoint_uri = self.v3_api.series.beta_route.clone();
        let payload_limits = self.v3_payload_limits();

        let v2_endpoint_config = EndpointConfiguration::new(
            v2_compression_scheme,
            self.max_metrics_per_payload,
            self.additional_tags.clone(),
        );
        let endpoint_config = EndpointConfiguration::new(
            v3_compression_scheme,
            self.max_metrics_per_payload,
            self.additional_tags.clone(),
        );

        // Derive the encoding mode for each metric type from the configuration.
        let metrics_v3_disabled_by_compressor = matches!(v3_compression_scheme, CompressionScheme::Zlib(_));
        let metrics_primary_v3_override = selected_metrics_primary_v3_override(
            self.observability_pipelines_worker_metrics_enabled,
            &self.observability_pipelines_worker_metrics_url,
            self.observability_pipelines_worker_metrics_use_v3_api_series,
            self.vector_metrics_enabled,
            &self.vector_metrics_url,
            self.vector_metrics_use_v3_api_series,
        );
        let series_v3_can_be_enabled = series_v3_can_be_enabled_for_config(
            self.v3_api.use_v3_series(),
            metrics_primary_v3_override,
            !self.additional_endpoints.is_empty(),
            &self.use_v3_api_series,
        );
        let use_v3_series = self.data_plane_metrics_v3_series_enabled && series_v3_can_be_enabled;
        let series_mode = metrics_encoder_mode_for_config(
            use_v3_series,
            self.v3_api.series.validate,
            metrics_v3_disabled_by_compressor,
        );
        let sketches_mode = metrics_encoder_mode_for_config(
            self.v3_api.use_v3_sketches(),
            self.v3_api.sketches.validate,
            metrics_v3_disabled_by_compressor,
        );
        let series_endpoint = if self.use_v2_api_series {
            MetricsEndpoint::SeriesV2
        } else {
            MetricsEndpoint::SeriesV1
        };
        let series_shadow_config = series_shadow_config_for_endpoint(
            series_endpoint,
            self.v3_api.series.shadow_sample_rate,
            metrics_v3_disabled_by_compressor,
        );
        let v3_runtime_config = V3RuntimeConfig {
            endpoint_config,
            payload_limits,
            series_endpoint_uri,
            shadow_series_endpoint_uri,
            series_shadow_config,
            serializer_telemetry: v3_serializer_telemetry,
        };
        let generic_payload_limits = clamp_payload_limits(
            self.max_uncompressed_payload_size,
            self.max_payload_size,
            DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
            DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT,
        );
        let (series_uncompressed_limit, series_compressed_limit) = if series_endpoint == MetricsEndpoint::SeriesV2 {
            clamp_payload_limits(
                self.max_series_uncompressed_payload_size,
                self.max_series_payload_size,
                v2::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
                v2::SERIES_V2_COMPRESSED_SIZE_LIMIT,
            )
        } else {
            generic_payload_limits
        };
        let mut v2_series_builder = v2::create_v2_request_builder(series_endpoint, &v2_endpoint_config)
            .await
            .error_context("Failed to create V2 series request builder.")?;
        v2_series_builder.with_len_limits(series_uncompressed_limit, series_compressed_limit)?;
        let v2_series_builder = Some(v2_series_builder);

        let (sketches_uncompressed_limit, sketches_compressed_limit) = generic_payload_limits;
        let mut v2_sketch_builder = v2::create_v2_request_builder(MetricsEndpoint::Sketches, &v2_endpoint_config)
            .await
            .error_context("Failed to create V2 sketches request builder.")?;
        v2_sketch_builder.with_len_limits(sketches_uncompressed_limit, sketches_compressed_limit)?;
        let v2_sketch_builder = Some(v2_sketch_builder);

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        if series_mode.needs_v3() || sketches_mode.needs_v3() {
            debug!(
                ?series_mode,
                ?sketches_mode,
                v3_series_endpoints = ?self.v3_api.series.endpoints,
                v3_sketches_endpoints = ?self.v3_api.sketches.endpoints,
                "V3 encoding support is enabled."
            );
        }

        Ok(Box::new(DatadogMetrics {
            v2_series_builder,
            v2_sketch_builder,
            series_mode,
            sketches_mode,
            v3_runtime_config,
            telemetry,
            flush_timeout,
            log_payloads: self.log_payloads,
        }))
    }
}

impl MemoryBounds for DatadogMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: How do we properly represent the requests we can generate that may be sitting around in-flight?
        //
        // Theoretically, we'll end up being limited by the size of the downstream forwarder's interconnect, and however
        // many payloads it will buffer internally... so realistically the firm limit boils down to the forwarder itself
        // but we'll have a hard time in the forwarder knowing the maximum size of any given payload being sent in, which
        // then makes it hard to calculate a proper firm bound even though we know the rest of the values required to
        // calculate the firm bound.
        builder
            .minimum()
            .with_single_value::<DatadogMetrics>("component struct")
            .with_array::<EventsBuffer>("request builder events channel", 8)
            .with_array::<PayloadsBuffer>("request builder payloads channel", 8);

        builder
            .firm()
            // Capture the size of the "split re-encode" buffers in the request builders, which is where we keep owned
            // versions of metrics that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<Metric>("series metrics split re-encode buffer", self.max_metrics_per_payload)
            .with_array::<Metric>("sketch metrics split re-encode buffer", self.max_metrics_per_payload);
    }
}

pub struct DatadogMetrics {
    v2_series_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    v2_sketch_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    series_mode: MetricsEncoderMode,
    sketches_mode: MetricsEncoderMode,
    v3_runtime_config: V3RuntimeConfig,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
    log_payloads: bool,
}

struct V3RuntimeConfig {
    endpoint_config: EndpointConfiguration,
    payload_limits: V3PayloadLimits,
    series_endpoint_uri: String,
    shadow_series_endpoint_uri: String,
    series_shadow_config: SeriesShadowConfig,
    serializer_telemetry: V3SerializerTelemetry,
}

#[async_trait]
impl Encoder for DatadogMetrics {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            v2_series_builder,
            v2_sketch_builder,
            series_mode,
            sketches_mode,
            v3_runtime_config,
            telemetry,
            flush_timeout,
            log_payloads,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our request builder task.
        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut = run_request_builder(
            v2_series_builder,
            v2_sketch_builder,
            series_mode,
            sketches_mode,
            v3_runtime_config,
            telemetry,
            events_rx,
            payloads_tx,
            flush_timeout,
            log_payloads,
        );
        let request_builder_handle = context
            .topology_context()
            .global_thread_pool()
            .spawn_traced_named("dd-metrics-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("Datadog Metrics encoder started.");

        loop {
            select! {
                biased;

                _ = health.live() => continue,
                maybe_payload = payloads_rx.recv() => match maybe_payload {
                    Some(payload) => {
                        if let Err(e) = context.dispatcher().dispatch(payload).await {
                            error!("Failed to dispatch payload: {}", e);
                        }
                    }
                    None => break,
                },
                maybe_event_buffer = context.events().next() => match maybe_event_buffer {
                    Some(event_buffer) => events_tx.send(event_buffer).await
                        .error_context("Failed to send event buffer to request builder task.")?,
                    None => break,
                },
            }
        }

        // Drop the events sender, which signals the request builder task to stop.
        drop(events_tx);

        // Continue draining the payloads receiver until it is closed.
        while let Some(payload) = payloads_rx.recv().await {
            if let Err(e) = context.dispatcher().dispatch(payload).await {
                error!("Failed to dispatch payload: {}", e);
            }
        }

        // Request build task should now be stopped.
        match request_builder_handle.await {
            Ok(Ok(())) => debug!("Request builder task stopped."),
            Ok(Err(e)) => error!(error = %e, "Request builder task failed."),
            Err(e) => error!(error = %e, "Request builder task panicked."),
        }

        debug!("Datadog Metrics encoder stopped.");

        Ok(())
    }
}

/// Logs the decoded contents of a metric prior to encoding.
///
/// This logs the metric object itself, not the encoded JSON/protobuf HTTP body.
fn log_metric_payload(metric: &Metric) {
    match metric.values() {
        MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
            debug!(?metric, "Flushing series metric.")
        }
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => {
            debug!(?metric, "Flushing sketch metric.")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_request_builder(
    mut v2_series_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    mut v2_sketch_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>, series_mode: MetricsEncoderMode,
    sketches_mode: MetricsEncoderMode, v3_runtime_config: V3RuntimeConfig, telemetry: ComponentTelemetry,
    mut events_rx: mpsc::Receiver<EventsBuffer>, mut payloads_tx: mpsc::Sender<PayloadsBuffer>,
    flush_timeout: Duration, log_payloads: bool,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    let mut v3_series_metrics =
        (series_mode.needs_v3() || v3_runtime_config.series_shadow_config.is_enabled()).then(Vec::<Metric>::new);
    let mut v3_sketch_metrics = sketches_mode.needs_v3().then(Vec::<Metric>::new);
    let mut v3_series_points = 0usize;
    let mut v3_sketch_points = 0usize;

    let mut series_batch_id = None;
    let mut sketches_batch_id = None;
    let mut series_shadow_state = SeriesShadowState::default();
    let series_shadow_config = v3_runtime_config.series_shadow_config;

    let tag_series = series_mode.needs_tagging();
    let tag_sketches = sketches_mode.needs_tagging();
    let v3_flush_context = V3FlushContext {
        endpoint_config: &v3_runtime_config.endpoint_config,
        payload_limits: v3_runtime_config.payload_limits,
        series_endpoint_uri: &v3_runtime_config.series_endpoint_uri,
        serializer_telemetry: &v3_runtime_config.serializer_telemetry,
        telemetry: &telemetry,
    };
    let v3_shadow_flush_context = V3FlushContext {
        endpoint_config: &v3_runtime_config.endpoint_config,
        payload_limits: v3_runtime_config.payload_limits,
        series_endpoint_uri: &v3_runtime_config.shadow_series_endpoint_uri,
        serializer_telemetry: &v3_runtime_config.serializer_telemetry,
        telemetry: &telemetry,
    };

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let metric = match event.try_into_metric() {
                        Some(metric) => metric,
                        None => continue,
                    };

                    if log_payloads {
                        log_metric_payload(&metric);
                    }

                    // A series metric whose points are all non-finite would encode to a series with no points, which
                    // intake rejects as an empty value set. Drop it whole rather than emit an empty series.
                    if !v1::has_emittable_point(&metric) {
                        debug!(metric = %metric.context().name(), "Dropping series metric with no finite points.");
                        telemetry.events_dropped_encoder().increment(1);
                        continue;
                    }

                    // Figure out which endpoint the metric belongs to, and grab the relevant V2 builder/V3 storage.
                    let endpoint = MetricsEndpoint::from_metric(&metric);
                    let (endpoint_mode, maybe_v2_builder, maybe_v3_metrics, v3_points, batch_id) = match endpoint {
                        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => (
                            series_mode,
                            &mut v2_series_builder,
                            &mut v3_series_metrics,
                            &mut v3_series_points,
                            &mut series_batch_id,
                        ),
                        MetricsEndpoint::Sketches => (
                            sketches_mode,
                            &mut v2_sketch_builder,
                            &mut v3_sketch_metrics,
                            &mut v3_sketch_points,
                            &mut sketches_batch_id,
                        ),
                    };
                    let metric_point_count = metric.values().len();
                    let is_series = matches!(endpoint, MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2);
                    let series_shadow_active = is_series
                        && matches!(endpoint_mode, MetricsEncoderMode::V2Only)
                        && series_shadow_state.ensure_decision(series_shadow_config);
                    let needs_batch_id = endpoint_mode.needs_batch_id() || series_shadow_active;
                    if needs_batch_id && batch_id.is_none() {
                        *batch_id = Some(Uuid::now_v7());
                    }
                    let active_batch_id = needs_batch_id.then_some(batch_id.as_ref()).flatten();
                    let should_buffer_v3 = endpoint_mode.needs_v3() || series_shadow_active;

                    // Store a copy of the metric in `maybe_v3_metrics` if it's present.
                    //
                    // We have to do this before encoding because `RequestBuilder::encode` consumes the metric. This also means we'll
                    // need to _remove_ the metric if encoding fails.
                    if should_buffer_v3 {
                        if let Some(metrics) = maybe_v3_metrics {
                            metrics.push(metric.clone());
                            *v3_points += metric_point_count;
                        }
                    }

                    // Attempt encoding the metric for V2 if configured.
                    //
                    // If the metric couldn't be encoded (too big, some other issue), the call returns `false` which is
                    // our signal to remove the metric from `maybe_v3_metrics` (if we added it), since we know now that
                    // the metric wasn't encoded for V2 and we want our V2/V3 payload batches to be consistent in
                    // validation mode.
                    let v2_payload_info = match endpoint {
                        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => {
                            if series_shadow_active {
                                Some(MetricsPayloadInfo::v2_shadow_series())
                            } else {
                                tag_series.then(MetricsPayloadInfo::v2_series)
                            }
                        }
                        MetricsEndpoint::Sketches => tag_sketches.then(MetricsPayloadInfo::v2_sketches),
                    };
                    // If V2 flushes while this batch is not shadowed, the current metric may start the next V2 batch.
                    // Keep a clone so we can add it to the next V3 shadow batch if that next batch samples in.
                    let metric_for_next_shadow_batch = (is_series
                        && matches!(endpoint_mode, MetricsEncoderMode::V2Only)
                        && series_shadow_config.is_enabled()
                        && !series_shadow_active)
                        .then(|| metric.clone());
                    let mut v2_encoded = false;
                    let v2_flushed = if let Some(builder) = maybe_v2_builder {
                        let result =
                            encode_v2_metrics(builder, metric, &telemetry, &mut payloads_tx, active_batch_id, v2_payload_info).await?;
                        v2_encoded = result.encoded();
                        if should_buffer_v3
                            && !result.encoded()
                            && !matches!(endpoint_mode, MetricsEncoderMode::V3Enabled)
                        {
                            if let Some(metrics) = maybe_v3_metrics {
                                let _ = metrics.pop();
                                *v3_points = v3_points.saturating_sub(metric_point_count);
                            }
                        }

                        result.flushed()
                    } else {
                        false
                    };

                    // Validation and shadow payloads must keep V2/V3 batch boundaries aligned. Authoritative V3 can
                    // batch independently, so it flushes on V3-specific limits instead of following V2 flushes.
                    let v3_payload_info = match endpoint {
                        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => {
                            if series_shadow_active {
                                Some(MetricsPayloadInfo::v3_shadow_series())
                            } else {
                                tag_series.then(MetricsPayloadInfo::v3_series)
                            }
                        }
                        MetricsEndpoint::Sketches => tag_sketches.then(MetricsPayloadInfo::v3_sketches),
                    };
                    let mut split_metric = None;
                    let v3_flushed = if let Some(v3_metrics) = maybe_v3_metrics {
                        let should_flush_v3 = match endpoint_mode {
                            MetricsEncoderMode::V2Only => series_shadow_active && v2_flushed,
                            MetricsEncoderMode::V3Enabled => {
                                if v3_flush_context.payload_limits.point_count_fits(metric_point_count)
                                    && v3_flush_context.payload_limits.point_count_exceeds_limit(*v3_points)
                                    && v3_metrics.len() > 1
                                {
                                    v3_flush_context
                                        .serializer_telemetry
                                        .record_split_reason(V3PayloadSplitReason::MaxPoints);
                                    if let Some(metric) = v3_metrics.pop() {
                                        *v3_points = v3_points.saturating_sub(metric.values().len());
                                        split_metric = Some(metric);
                                    }
                                    true
                                } else {
                                    v3_flush_context.payload_limits.should_flush_point_count_limit(*v3_points)
                                        || v3_flush_context
                                            .payload_limits
                                            .should_flush_metric_count_limit(v3_metrics)
                                }
                            }
                            MetricsEncoderMode::Validation => v2_flushed,
                        };
                        if should_flush_v3 {
                            if !matches!(endpoint_mode, MetricsEncoderMode::V3Enabled) {
                                // V2 flushes the previous batch without the current metric. Pop it
                                // from V3 before flushing so both batches cover the same set of metrics.
                                if v2_flushed {
                                    if let Some(metric) = v3_metrics.pop() {
                                        *v3_points = v3_points.saturating_sub(metric.values().len());
                                        split_metric = Some(metric);
                                    }
                                }
                            }
                            let flush_context = if series_shadow_active {
                                v3_shadow_flush_context
                            } else {
                                v3_flush_context
                            };
                            encode_and_flush_v3_metrics(
                                endpoint,
                                flush_context,
                                v3_metrics,
                                &mut payloads_tx,
                                active_batch_id,
                                v3_payload_info,
                            )
                            .await?;
                            *v3_points = 0;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if matches!(endpoint_mode, MetricsEncoderMode::V3Enabled) {
                        // Authoritative V3 may flush the previous point-limit batch while keeping the current metric
                        // buffered for the next V3 payload.
                        if let Some(m) = split_metric.take() {
                            let point_count = m.values().len();
                            if let Some(metrics) = maybe_v3_metrics {
                                metrics.push(m);
                                *v3_points += point_count;
                            }
                        }
                    } else if endpoint_mode.needs_batch_id() && (v2_flushed || v3_flushed) {
                        // A validation flush completes the current V2/V3 pair. If V2 carried the current metric into
                        // the next batch, keep the V3 copy and assign that next pair a fresh validation ID.
                        *batch_id = if let Some(m) = split_metric.take() {
                            let point_count = m.values().len();
                            if let Some(metrics) = maybe_v3_metrics {
                                metrics.push(m);
                                *v3_points += point_count;
                            }
                            Some(Uuid::now_v7())
                        } else {
                            None
                        };
                    } else if is_series && series_shadow_active && (v2_flushed || v3_flushed) {
                        // A shadow flush completes the current sampled pair. The next V2 batch gets a new shadow sample
                        // decision, so only carry the split metric into V3 if that next batch samples in.
                        series_shadow_state.reset();
                        *batch_id = if let Some(m) = split_metric.take() {
                            if series_shadow_state.ensure_decision(series_shadow_config) {
                                let point_count = m.values().len();
                                if let Some(metrics) = maybe_v3_metrics {
                                    metrics.push(m);
                                    *v3_points += point_count;
                                }
                                Some(Uuid::now_v7())
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                    } else if is_series
                        && matches!(endpoint_mode, MetricsEncoderMode::V2Only)
                        && series_shadow_config.is_enabled()
                        && v2_flushed
                    {
                        // This V2 batch was not shadowed, but the flushed metric may have started the next V2 batch.
                        // Re-sample for that next batch and seed the V3 buffer only if the new decision samples in.
                        series_shadow_state.reset();
                        *batch_id = None;
                        if v2_encoded {
                            if let Some(m) = metric_for_next_shadow_batch {
                                if series_shadow_state.ensure_decision(series_shadow_config) {
                                    let point_count = m.values().len();
                                    if let Some(metrics) = maybe_v3_metrics {
                                        metrics.push(m);
                                        *v3_points += point_count;
                                    }
                                    *batch_id = Some(Uuid::now_v7());
                                }
                            }
                        }
                    }
                }

                debug!("Processed event buffer.");

                // If we're not already pending a flush, we'll start the countdown.
                if !pending_flush {
                    pending_flush_timeout.as_mut().reset(tokio::time::Instant::now() + flush_timeout);
                    pending_flush = true;
                }
            },
            _ = &mut pending_flush_timeout, if pending_flush => {
                debug!("Flushing pending request(s).");

                pending_flush = false;

                // Flush any pending series metrics.
                // Timeout flushes complete the current batch, so reuse the existing shadow decision instead of sampling
                // a new one for metrics that are already buffered.
                let series_shadow_active = matches!(series_mode, MetricsEncoderMode::V2Only)
                    && series_shadow_state.active.unwrap_or(false);
                let v2_series_payload_info = if series_shadow_active {
                    Some(MetricsPayloadInfo::v2_shadow_series())
                } else {
                    tag_series.then(MetricsPayloadInfo::v2_series)
                };
                let series_active_batch_id = (series_mode.needs_batch_id() || series_shadow_active)
                    .then_some(series_batch_id.as_ref())
                    .flatten();
                let mut v2_series_flush_succeeded = true;
                if let Some(builder) = &mut v2_series_builder {
                    if let Err(e) = flush_v2_metrics(builder, &mut payloads_tx, series_active_batch_id, v2_series_payload_info).await {
                        error!(error = %e, "Failed to flush V2 series metrics: {}", e);
                        v2_series_flush_succeeded = false;
                    }
                }

                let v3_series_payload_info = if series_shadow_active {
                    Some(MetricsPayloadInfo::v3_shadow_series())
                } else {
                    tag_series.then(MetricsPayloadInfo::v3_series)
                };
                if let Some(metrics) = &mut v3_series_metrics {
                    if v2_series_flush_succeeded || matches!(series_mode, MetricsEncoderMode::V3Enabled) {
                        // Shadow series use the V3 beta route; normal V3 series use the configured authoritative route.
                        let flush_context = if series_shadow_active {
                            v3_shadow_flush_context
                        } else {
                            v3_flush_context
                        };
                        if let Err(e) = encode_and_flush_v3_series_metrics(
                            flush_context,
                            metrics,
                            &mut payloads_tx,
                            series_active_batch_id,
                            v3_series_payload_info,
                        )
                        .await
                        {
                            error!(error = %e, "Failed to flush V3 series metrics: {}", e);
                        }
                        v3_series_points = 0;
                    } else {
                        // Validation/shadow V3 must not outlive a failed V2 baseline flush.
                        warn!("Failed to flush V2 series metrics, skipping V3 series flush.");
                        metrics.clear();
                        v3_series_points = 0;
                    }
                }
                if series_mode.needs_batch_id() {
                    series_batch_id = None;
                }
                if matches!(series_mode, MetricsEncoderMode::V2Only) && series_shadow_config.is_enabled() {
                    series_shadow_state.reset();
                    series_batch_id = None;
                }

                // Flush any pending sketch metrics.
                let v2_sketches_payload_info = tag_sketches.then(MetricsPayloadInfo::v2_sketches);
                let sketches_active_batch_id = sketches_mode.needs_batch_id().then_some(sketches_batch_id.as_ref()).flatten();
                let mut v2_sketches_flush_succeeded = true;
                if let Some(builder) = &mut v2_sketch_builder {
                    if let Err(e) = flush_v2_metrics(builder, &mut payloads_tx, sketches_active_batch_id, v2_sketches_payload_info).await {
                        error!(error = %e, "Failed to flush V2 sketch metrics: {}", e);
                        v2_sketches_flush_succeeded = false;
                    }
                }

                let v3_sketches_payload_info = tag_sketches.then(MetricsPayloadInfo::v3_sketches);
                if let Some(metrics) = &mut v3_sketch_metrics {
                    if v2_sketches_flush_succeeded || matches!(sketches_mode, MetricsEncoderMode::V3Enabled) {
                        if let Err(e) = encode_and_flush_v3_sketch_metrics(
                            v3_flush_context,
                            metrics,
                            &mut payloads_tx,
                            sketches_active_batch_id,
                            v3_sketches_payload_info,
                        )
                        .await
                        {
                            error!(error = %e, "Failed to flush V3 sketch metrics: {}", e);
                        }
                        v3_sketch_points = 0;
                    } else {
                        warn!("Failed to flush V2 sketch metrics, skipping V3 sketch flush.");
                        metrics.clear();
                        v3_sketch_points = 0;
                    }
                }
                if sketches_mode.needs_batch_id() {
                    sketches_batch_id = None;
                }

                debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
            },

            // Event buffers channel has been closed, and we have no pending flushing, so we're all done.
            else => break,
        }
    }

    Ok(())
}

struct EncodeResult {
    encoded: bool,
    flushed: bool,
}

impl EncodeResult {
    pub const fn new(encoded: bool, flushed: bool) -> Self {
        Self { encoded, flushed }
    }

    pub const fn encoded(&self) -> bool {
        self.encoded
    }

    pub const fn flushed(&self) -> bool {
        self.flushed
    }
}

async fn encode_v2_metrics(
    request_builder: &mut RequestBuilder<v2::MetricsEndpointEncoder>, metric: Metric, telemetry: &ComponentTelemetry,
    payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&Uuid>, payload_info: Option<MetricsPayloadInfo>,
) -> Result<EncodeResult, GenericError> {
    // Encode the metric. If we get it back, that means the current request is full, and we need to
    // flush it before we can try to encode the metric again... so we'll hold on to it in that case
    // before flushing and trying to encode it again.
    let metric_to_retry = match request_builder.encode(metric).await {
        Ok(None) => return Ok(EncodeResult::new(true, false)),
        Ok(Some(metric)) => metric,
        Err(e) => {
            error!(error = %e, "Failed to encode metric.");
            telemetry.events_dropped_encoder().increment(1);
            return Ok(EncodeResult::new(false, false));
        }
    };

    flush_v2_metrics(request_builder, payloads_tx, batch_id, payload_info).await?;

    // Now try to encode the metric again. If it fails again, we'll just log it because it shouldn't
    // be possible to fail at this point, otherwise we would have already caught that the first
    // time.
    match request_builder.encode(metric_to_retry).await {
        Ok(None) => Ok(EncodeResult::new(true, true)),
        Ok(Some(_)) => unreachable!(
            "failure to encode due to size should never occur after flush for metrics which aren't unencodable"
        ),
        Err(e) => {
            error!(error = %e, "Failed to encode metric.");
            telemetry.events_dropped_encoder().increment(1);
            Ok(EncodeResult::new(false, true))
        }
    }
}

async fn flush_v2_metrics(
    request_builder: &mut RequestBuilder<MetricsEndpointEncoder>, payloads_tx: &mut mpsc::Sender<Payload>,
    batch_id: Option<&Uuid>, payload_info: Option<MetricsPayloadInfo>,
) -> Result<usize, GenericError> {
    let mut requests_flushed = 0;

    let maybe_requests = request_builder.flush().await;
    let batch_len = maybe_requests.len();
    for (batch_seq, maybe_request) in maybe_requests.into_iter().enumerate() {
        match maybe_request {
            Ok((events, data_points, request)) => {
                requests_flushed += 1;

                flush_payload(
                    request,
                    events,
                    data_points,
                    payloads_tx,
                    batch_id,
                    batch_seq,
                    batch_len,
                    payload_info,
                )
                .await?;
            }

            // TODO: Increment a counter here that metrics were dropped due to a flush failure.
            Err(e) => {
                if !e.is_recoverable() {
                    return Err(GenericError::from(e).context("Failed to flush request."));
                }
            }
        }
    }

    Ok(requests_flushed)
}

#[derive(Clone, Copy)]
struct V3FlushContext<'a> {
    endpoint_config: &'a EndpointConfiguration,
    payload_limits: V3PayloadLimits,
    series_endpoint_uri: &'a str,
    serializer_telemetry: &'a V3SerializerTelemetry,
    telemetry: &'a ComponentTelemetry,
}

async fn encode_and_flush_v3_metrics(
    endpoint: MetricsEndpoint, context: V3FlushContext<'_>, metrics: &mut Vec<Metric>,
    payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&Uuid>, payload_info: Option<MetricsPayloadInfo>,
) -> Result<(), GenericError> {
    match endpoint {
        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => {
            encode_and_flush_v3_series_metrics(context, metrics, payloads_tx, batch_id, payload_info).await
        }
        MetricsEndpoint::Sketches => {
            encode_and_flush_v3_sketch_metrics(context, metrics, payloads_tx, batch_id, payload_info).await
        }
    }
}

async fn encode_and_flush_v3_series_metrics(
    context: V3FlushContext<'_>, metrics: &mut Vec<Metric>, payloads_tx: &mut mpsc::Sender<Payload>,
    batch_id: Option<&Uuid>, payload_info: Option<MetricsPayloadInfo>,
) -> Result<(), GenericError> {
    if metrics.is_empty() {
        return Ok(());
    }
    let metrics_to_flush = std::mem::take(metrics);

    let requests = encode_v3_payload_requests(context.series_endpoint_uri, &metrics_to_flush, context, "series").await;
    let batch_len = requests.len();
    for (batch_seq, payload_request) in requests.into_iter().enumerate() {
        flush_payload(
            payload_request.request,
            payload_request.event_count,
            payload_request.data_point_count,
            payloads_tx,
            batch_id,
            batch_seq,
            batch_len,
            payload_info,
        )
        .await?;
        debug!(
            events = payload_request.event_count,
            data_points = payload_request.data_point_count,
            "Sent V3 series payload."
        );
    }

    Ok(())
}

async fn encode_and_flush_v3_sketch_metrics(
    context: V3FlushContext<'_>, metrics: &mut Vec<Metric>, payloads_tx: &mut mpsc::Sender<Payload>,
    batch_id: Option<&Uuid>, payload_info: Option<MetricsPayloadInfo>,
) -> Result<(), GenericError> {
    if metrics.is_empty() {
        return Ok(());
    }
    let metrics_to_flush = std::mem::take(metrics);

    let requests = encode_v3_payload_requests(V3_SKETCHES_ENDPOINT_URI, &metrics_to_flush, context, "sketches").await;
    let batch_len = requests.len();
    for (batch_seq, payload_request) in requests.into_iter().enumerate() {
        flush_payload(
            payload_request.request,
            payload_request.event_count,
            payload_request.data_point_count,
            payloads_tx,
            batch_id,
            batch_seq,
            batch_len,
            payload_info,
        )
        .await?;
        debug!(
            events = payload_request.event_count,
            data_points = payload_request.data_point_count,
            "Sent V3 sketches payload."
        );
    }

    Ok(())
}

async fn encode_v3_payload_requests(
    endpoint_uri: &str, metrics: &[Metric], context: V3FlushContext<'_>, payload_kind: &'static str,
) -> Vec<V3PayloadRequest> {
    let mut requests = Vec::new();
    let mut pending_ranges = split_v3_metric_ranges_by_point_limit(metrics, context, payload_kind);

    while let Some(range) = pending_ranges.pop_front() {
        if range.is_empty() {
            continue;
        }

        let metrics_in_range = &metrics[range.clone()];
        let event_count = metrics_in_range.len();
        let data_point_count = metrics_in_range.iter().map(|metric| metric.values().len()).sum();

        let encoded = match encode_v3_metrics_batch(metrics_in_range, context.endpoint_config.additional_tags()) {
            Ok(encoded) => encoded,
            Err(e) => {
                error!(error = %e, payload_kind, events = event_count, "Failed to encode V3 metrics payload request.");
                context.telemetry.events_dropped_encoder().increment(event_count as u64);
                continue;
            }
        };
        let encoded_request =
            match create_v3_request(endpoint_uri, encoded, context.endpoint_config.compression_scheme()).await {
                Ok(request) => request,
                Err(e) => {
                    error!(error = %e, payload_kind, events = event_count, "Failed to create V3 metrics request.");
                    context.telemetry.events_dropped_encoder().increment(event_count as u64);
                    continue;
                }
            };

        if context.payload_limits.request_fits(&encoded_request) {
            record_v3_serializer_stats(context.serializer_telemetry, &encoded_request.stats);
            requests.push(V3PayloadRequest {
                request: encoded_request.request,
                event_count,
                data_point_count,
            });
            continue;
        }

        if range.len() == 1 {
            // The encoded request is too large and this range cannot be split any further.
            context.serializer_telemetry.record_item_too_big();
            context
                .serializer_telemetry
                .record_split_reason(V3PayloadSplitReason::ItemTooBig);
            warn!(
                payload_kind,
                compressed_len = encoded_request.compressed_len,
                compressed_limit = context.payload_limits.max_compressed_size,
                uncompressed_len = encoded_request.uncompressed_len,
                uncompressed_limit = context.payload_limits.max_uncompressed_size,
                "Dropping oversized V3 metric that cannot be split further."
            );
            context.telemetry.events_dropped_encoder().increment(1);
            continue;
        }

        // Retry this oversized range as two smaller ranges, preserving the original metric order.
        context
            .serializer_telemetry
            .record_split_reason(V3PayloadSplitReason::PayloadFull);
        let pivot = range.start + range.len() / 2;
        pending_ranges.push_front(pivot..range.end);
        pending_ranges.push_front(range.start..pivot);
    }

    requests
}

fn record_v3_serializer_stats(telemetry: &V3SerializerTelemetry, stats: &V3EncoderStats) {
    telemetry.record_values_count(stats.value_encoding_stats);

    for column in &stats.columns {
        let uncompressed_size = column.bytes.len() as u64;
        let compressed_size = column.compressed_len as u64;
        telemetry.record_column_size(column.field_number, uncompressed_size, compressed_size);
    }
}

async fn compressed_v3_len(bytes: &[u8], compression_scheme: CompressionScheme) -> Result<usize, GenericError> {
    if matches!(compression_scheme, CompressionScheme::Noop) {
        return Ok(bytes.len());
    }

    let buffer = ChunkedBytesBuffer::new(RB_BUFFER_CHUNK_SIZE);
    let mut compressor = Compressor::from_scheme(compression_scheme, buffer);
    compressor
        .write_all(bytes)
        .await
        .error_context("Failed to compress V3 bytes.")?;
    compressor
        .flush()
        .await
        .error_context("Failed to flush V3 compressor.")?;
    compressor
        .shutdown()
        .await
        .error_context("Failed to shutdown V3 compressor.")?;

    Ok(compressor.into_inner().freeze().len())
}

fn split_v3_metric_ranges_by_point_limit(
    metrics: &[Metric], context: V3FlushContext<'_>, payload_kind: &'static str,
) -> VecDeque<Range<usize>> {
    let mut ranges = VecDeque::new();
    let mut current_start = None;
    let mut current_points = 0usize;

    for (idx, metric) in metrics.iter().enumerate() {
        let metric_points = metric.values().len();
        if metric_points == 0 {
            // The Agent drops zero-point V3 metrics before writing them.
            if let Some(start) = current_start.take() {
                if start < idx {
                    ranges.push_back(start..idx);
                }
            }
            context.telemetry.events_dropped_encoder().increment(1);
            current_points = 0;
            continue;
        }

        if !context.payload_limits.point_count_fits(metric_points) {
            // This metric exceeds the point limit by itself, so it cannot fit in any V3 payload request.
            // Close the current range before dropping this oversized metric.
            context.serializer_telemetry.record_item_too_big();
            if let Some(start) = current_start.take() {
                if start < idx {
                    ranges.push_back(start..idx);
                }
            }
            warn!(
                payload_kind,
                data_points = metric_points,
                point_limit = context.payload_limits.max_points_per_payload,
                "Dropping oversized V3 metric that exceeds the point-count limit."
            );
            context.telemetry.events_dropped_encoder().increment(1);
            current_points = 0;
            continue;
        }

        let would_exceed_point_limit =
            current_points > 0 && !context.payload_limits.point_count_fits(current_points + metric_points);
        if would_exceed_point_limit {
            // This metric fits by itself, but not together with the current range.
            // Adding this metric would overflow the current range, so start a new range at this metric.
            context
                .serializer_telemetry
                .record_split_reason(V3PayloadSplitReason::MaxPoints);
            if let Some(start) = current_start {
                ranges.push_back(start..idx);
            }
            current_start = Some(idx);
            current_points = 0;
        } else if current_start.is_none() {
            current_start = Some(idx);
        }

        current_points += metric_points;
    }

    if let Some(start) = current_start {
        if start < metrics.len() {
            ranges.push_back(start..metrics.len());
        }
    }

    ranges
}

/// Converts a `Uuid` to a `HeaderValue`.
fn uuid_to_header_value(uuid: &Uuid) -> HeaderValue {
    let s = uuid.as_hyphenated().to_string();
    // SAFETY: UUID hyphenated format only contains [0-9a-f-], all valid ASCII header chars.
    unsafe { HeaderValue::from_maybe_shared_unchecked(s) }
}

/// Converts a `usize` to a `HeaderValue`.
fn usize_to_header_value(value: usize) -> HeaderValue {
    let s = value.to_string();
    // SAFETY: Integer strings only contain ASCII digits [0-9], all valid header chars.
    unsafe { HeaderValue::from_maybe_shared_unchecked(s) }
}

async fn flush_payload(
    mut request: Request<FrozenChunkedBytesBuffer>, event_count: usize, data_point_count: usize,
    payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&Uuid>, batch_seq: usize, batch_len: usize,
    payload_info: Option<MetricsPayloadInfo>,
) -> Result<(), GenericError> {
    // Attach the validation batch UUID and sequence headers if present.
    if let Some(batch_id) = batch_id {
        let headers = request.headers_mut();
        headers.insert("X-Metrics-Request-ID", uuid_to_header_value(batch_id));
        headers.insert("X-Metrics-Request-Seq", usize_to_header_value(batch_seq));
        headers.insert("X-Metrics-Request-Len", usize_to_header_value(batch_len));
    }

    let mut payload_meta = PayloadMetadata::from_event_and_data_point_count(event_count, data_point_count);
    if let Some(info) = payload_info {
        payload_meta = payload_meta.with(info);
    }
    let http_payload = HttpPayload::new(payload_meta, request);
    let payload = Payload::Http(http_payload);

    payloads_tx
        .send(payload)
        .await
        .error_context("Failed to send payload.")?;

    Ok(())
}

// Encodes a batch of metrics to V3 columnar format.
fn encode_v3_metrics_batch(
    metrics: &[Metric], additional_tags: &SharedTagSet,
) -> Result<V3EncodedMetrics, GenericError> {
    let mut writer = V3Writer::new();
    let mut tags_deduplicator = ReusableDeduplicator::new();

    for metric in metrics {
        write_metric_to_v3(&mut writer, metric, additional_tags, &mut tags_deduplicator);
    }

    writer
        .finalize()
        .map_err(|e| generic_error!("Failed to serialize V3 payload: {}", e))
}

/// Writes a single metric to the V3 writer.
fn write_metric_to_v3(
    writer: &mut V3Writer, metric: &Metric, additional_tags: &SharedTagSet,
    tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) {
    let metric_type = match metric.values() {
        MetricValues::Counter(..) => V3MetricType::Count,
        MetricValues::Rate(..) => V3MetricType::Rate,
        MetricValues::Gauge(..) | MetricValues::Set(..) => V3MetricType::Gauge,
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => V3MetricType::Sketch,
    };
    let is_sketch = metric_type == V3MetricType::Sketch;

    let mut builder = writer.write(metric_type, metric.context().name());

    // Tags - chain instrumented + additional + origin tags
    let chained_tags = metric
        .context()
        .tags()
        .into_iter()
        .chain(additional_tags)
        .chain(metric.context().origin_tags());
    let all_tags = tags_deduplicator
        .deduplicated(chained_tags)
        .filter(|t| is_sketch || !is_v3_series_resource_tag(t) && !is_v3_series_device_tag(t))
        .map(|t| t.as_str());
    builder.set_tags(all_tags);

    // Resources - extract host and, for series, promoted resource tags.
    let mut resources = Vec::new();
    if let Some(host) = metric.metadata().hostname().filter(|host| !host.is_empty()) {
        resources.push(("host", host));
    }
    if !is_sketch {
        let mut device_resource = None;
        let chained_tags = metric
            .context()
            .origin_tags()
            .into_iter()
            .chain(metric.context().tags())
            .chain(additional_tags);
        for tag in tags_deduplicator.deduplicated(chained_tags) {
            if is_v3_series_device_tag(tag) {
                device_resource = tag.value().filter(|device| !device.is_empty());
            } else if is_v3_series_resource_tag(tag) {
                if let Some((rtype, rname)) = tag.value().and_then(|value| value.split_once(':')) {
                    if !rtype.is_empty() && !rname.is_empty() {
                        resources.push((rtype, rname));
                    }
                }
            }
        }
        if let Some(device) = device_resource {
            let device_idx = usize::from(metric.metadata().hostname().is_some_and(|host| !host.is_empty()));
            resources.insert(device_idx, ("device", device));
        }
    }
    builder.set_resources(&resources);

    // Origin metadata
    if let Some(origin) = metric.metadata().origin() {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                builder.set_source_type(source_type.as_ref());
            }
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => {
                builder.set_origin(*product, *subproduct, *product_detail, false);
            }
        }
    }

    if metric_type != V3MetricType::Sketch {
        if let Some(unit) = metric.metadata().unit() {
            builder.set_unit(unit);
        }
    }

    // Points based on metric type
    match metric.values() {
        MetricValues::Counter(points) | MetricValues::Gauge(points) => {
            for (ts, val) in points {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                builder.add_point(timestamp, val);
            }
        }
        MetricValues::Rate(points, interval) => {
            builder.set_interval(interval.as_secs());
            for (ts, val) in points {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                // Scale by interval as done in V2
                let scaled = val / interval.as_secs_f64();
                builder.add_point(timestamp, scaled);
            }
        }
        MetricValues::Set(points) => {
            // Set values are already converted to count in the iterator
            for (ts, count) in points {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                builder.add_point(timestamp, count);
            }
        }
        MetricValues::Distribution(sketches) => {
            for (ts, sketch) in sketches {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                if !sketch.is_empty() {
                    let bin_keys: Vec<i32> = sketch.bins().iter().map(|b| b.key()).collect();
                    let bin_counts: Vec<u32> = sketch.bins().iter().map(|b| b.count()).collect();
                    builder.add_sketch(
                        timestamp,
                        sketch.count() as i64,
                        sketch.sum().unwrap_or(0.0),
                        sketch.min().unwrap_or(0.0),
                        sketch.max().unwrap_or(0.0),
                        &bin_keys,
                        &bin_counts,
                    );
                }
            }
        }
        MetricValues::Histogram(histograms) => {
            for (ts, histogram) in histograms {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                // Convert histogram to DDSketch
                let mut sketch = DDSketch::default();
                for sample in histogram.samples() {
                    sketch.insert_n(sample.value.into_inner(), sample.weight.0 as u64);
                }
                if !sketch.is_empty() {
                    let bin_keys: Vec<i32> = sketch.bins().iter().map(|b| b.key()).collect();
                    let bin_counts: Vec<u32> = sketch.bins().iter().map(|b| b.count()).collect();
                    builder.add_sketch(
                        timestamp,
                        sketch.count() as i64,
                        sketch.sum().unwrap_or(0.0),
                        sketch.min().unwrap_or(0.0),
                        sketch.max().unwrap_or(0.0),
                        &bin_keys,
                        &bin_counts,
                    );
                }
            }
        }
    }

    builder.close();
}

fn is_v3_series_device_tag(tag: &Tag) -> bool {
    tag.name() == "device" && tag.value().is_some()
}

fn is_v3_series_resource_tag(tag: &Tag) -> bool {
    tag.name() == "dd.internal.resource" && tag.value().is_some()
}

/// Creates a V3 HTTP request from encoded payload data.
async fn create_v3_request(
    endpoint_uri: &str, mut encoded: V3EncodedMetrics, compression_scheme: CompressionScheme,
) -> Result<V3EncodedRequest, GenericError> {
    // Keep the wire payload as one continuous compressed stream. Per-column compressed sizes are measured
    // independently for telemetry.
    let mut header_buf = [0; 16];
    let header_len = {
        let mut header_writer = CodedOutputStream::bytes(&mut header_buf);
        header_writer.write_tag(3, WireType::LengthDelimited)?;
        header_writer.write_uint64_no_tag(encoded.payload.len() as u64)?;
        header_writer.flush()?;
        header_writer.total_bytes_written() as usize
    };

    let uncompressed_len = header_len + encoded.payload.len();
    for column in &mut encoded.stats.columns {
        column.compressed_len = compressed_v3_len(&column.bytes, compression_scheme)
            .await
            .error_context("Failed to measure V3 column compressed size.")?;
    }

    let buffer = ChunkedBytesBuffer::new(RB_BUFFER_CHUNK_SIZE);
    let mut compressor = Compressor::from_scheme(compression_scheme, buffer);
    compressor
        .write_all(&header_buf[..header_len])
        .await
        .error_context("Failed to compress V3 payload.")?;
    compressor
        .write_all(&encoded.payload)
        .await
        .error_context("Failed to compress V3 payload.")?;
    compressor
        .flush()
        .await
        .error_context("Failed to flush V3 compressor.")?;
    compressor
        .shutdown()
        .await
        .error_context("Failed to shutdown V3 compressor.")?;

    let compressed_buf = compressor.into_inner().freeze();
    let compressed_len = compressed_buf.len();

    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(endpoint_uri)
        .header(http::header::CONTENT_TYPE, "application/x-protobuf");

    if let Some(encoding) = content_encoding_for_scheme(compression_scheme) {
        builder = builder.header(http::header::CONTENT_ENCODING, encoding);
    }

    let request = builder
        .body(compressed_buf)
        .map_err(|e| generic_error!("Failed to build V3 request: {}", e))?;

    Ok(V3EncodedRequest {
        request,
        compressed_len,
        uncompressed_len,
        stats: encoded.stats,
    })
}

fn content_encoding_for_scheme(compression_scheme: CompressionScheme) -> Option<HeaderValue> {
    match compression_scheme {
        CompressionScheme::Noop => None,
        CompressionScheme::Gzip(_) => Some(HeaderValue::from_static("gzip")),
        CompressionScheme::Zlib(_) => Some(HeaderValue::from_static("deflate")),
        CompressionScheme::Zstd(_) => Some(HeaderValue::from_static("zstd")),
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::Arc};

    use bytes::Bytes;
    use saluki_context::{
        tags::{Tag, TagSet},
        Context,
    };
    use saluki_core::data_model::{
        event::{metric::MetricMetadata, Event},
        payload::Payload,
    };
    use saluki_metrics::test::TestRecorder;
    use stringtheory::MetaString;
    use tokio::time::timeout;

    use super::*;

    #[test]
    fn deser_agent_v3_api_nested_settings() {
        let raw = r#"
serializer_experimental_use_v3_api:
  compression_level: 7
  series:
    endpoints:
      - https://app.datadoghq.com
    validate: true
    use_beta: true
    beta_route: /api/intake/metrics/custom/series
    shadow_sample_rate: 0.25
    shadow_sites:
      - datadoghq.eu
  sketches:
    endpoints:
      - https://app.datadoghq.eu
"#;

        let config =
            serde_yaml::from_str::<DatadogMetricsConfiguration>(raw).expect("configuration should deserialize");

        assert_eq!(7, config.v3_api.compression_level);
        assert_eq!(
            Some("https://app.datadoghq.com"),
            config.v3_api.series.endpoints.first().map(String::as_str)
        );
        assert!(config.v3_api.series.validate);
        assert!(config.v3_api.series.use_beta);
        assert_eq!("/api/intake/metrics/custom/series", config.v3_api.series.beta_route);
        assert_eq!(0.25, config.v3_api.series.shadow_sample_rate);
        assert_eq!(vec!["datadoghq.eu"], config.v3_api.series.shadow_sites);
        assert_eq!(
            Some("https://app.datadoghq.eu"),
            config.v3_api.sketches.endpoints.first().map(String::as_str)
        );
    }

    #[test]
    fn agent_v3_api_shadow_defaults_match_agent() {
        let config = serde_yaml::from_str::<DatadogMetricsConfiguration>("").expect("configuration should deserialize");

        assert_eq!(0.0, config.v3_api.series.shadow_sample_rate);
        assert_eq!(vec!["datadoghq.com"], config.v3_api.series.shadow_sites);
    }

    #[test]
    fn shadow_sample_matches_agent_threshold_behavior() {
        assert!(!shadow_sample_matches(0.0, 0.0));
        assert!(shadow_sample_matches(0.5, 0.4));
        assert!(!shadow_sample_matches(0.5, 0.5));
        assert!(!shadow_sample_matches(0.5, 0.6));
    }

    #[test]
    fn shadow_sampling_is_disabled_for_v1_series_baseline_or_v3_incompatible_compressor() {
        assert!(series_shadow_config_for_endpoint(MetricsEndpoint::SeriesV2, 1.0, false).is_enabled());
        assert!(!series_shadow_config_for_endpoint(MetricsEndpoint::SeriesV1, 1.0, false).is_enabled());
        assert!(!series_shadow_config_for_endpoint(MetricsEndpoint::SeriesV2, 1.0, true).is_enabled());
    }

    #[test]
    fn metrics_v3_disabled_by_compressor_uses_v2_only() {
        assert_eq!(
            MetricsEncoderMode::V2Only,
            metrics_encoder_mode_for_config(true, false, true)
        );
        assert_eq!(
            MetricsEncoderMode::V2Only,
            metrics_encoder_mode_for_config(true, true, true)
        );
        assert_eq!(
            MetricsEncoderMode::V3Enabled,
            metrics_encoder_mode_for_config(true, false, false)
        );
        assert_eq!(
            MetricsEncoderMode::Validation,
            metrics_encoder_mode_for_config(true, true, false)
        );
    }

    #[test]
    fn agent_default_v3_does_not_enable_opw_only_encoder_mode() {
        let series_config = UseV3ApiSeriesConfig::default();
        let invalid_metrics_primary_override =
            selected_metrics_primary_v3_override(true, "http://[::1", false, true, "http://vector.example.com", false);

        assert!(!series_v3_can_be_enabled_for_config(
            false,
            Some(false),
            false,
            &series_config
        ));
        assert!(series_v3_can_be_enabled_for_config(
            false,
            Some(true),
            false,
            &series_config
        ));
        assert!(series_v3_can_be_enabled_for_config(
            false,
            Some(false),
            true,
            &series_config
        ));
        assert!(series_v3_can_be_enabled_for_config(
            true,
            Some(false),
            false,
            &series_config
        ));
        assert_eq!(None, invalid_metrics_primary_override);
        assert!(series_v3_can_be_enabled_for_config(
            false,
            invalid_metrics_primary_override,
            false,
            &series_config
        ));
    }

    #[tokio::test]
    async fn create_v3_request_uses_configured_endpoint_uri() {
        let encoded = V3Writer::new().finalize().expect("empty V3 payload should encode");
        let request = create_v3_request("/api/intake/metrics/custom/series", encoded, CompressionScheme::noop())
            .await
            .expect("request should be created");

        assert_eq!("/api/intake/metrics/custom/series", request.request.uri());
    }

    #[tokio::test]
    async fn create_v3_request_uses_single_stream_body_and_column_telemetry() {
        let metrics = vec![Metric::counter("v3.single.stream", 42.0)];
        let encoded = encode_v3_metrics_batch(&metrics, &SharedTagSet::default()).expect("metrics should encode to V3");
        let expected_payload = encoded.payload.clone();

        let request = create_v3_request(V3_SERIES_ENDPOINT_URI, encoded, CompressionScheme::noop())
            .await
            .expect("request should be created");

        for column in &request.stats.columns {
            assert_eq!(column.compressed_len, column.bytes.len());
        }

        let mut expected_body = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut expected_body);
            os.write_tag(3, WireType::LengthDelimited).unwrap();
            os.write_uint64_no_tag(expected_payload.len() as u64).unwrap();
            os.flush().unwrap();
        }
        expected_body.extend_from_slice(&expected_payload);

        assert_eq!(request.request.into_body().into_bytes(), Bytes::from(expected_body));
    }

    #[tokio::test]
    async fn create_v3_request_zstd_body_decodes_to_metric_payload() {
        let metrics = vec![Metric::counter("v3.single.stream.zstd", 42.0)];
        let encoded = encode_v3_metrics_batch(&metrics, &SharedTagSet::default()).expect("metrics should encode to V3");
        let expected_payload = encoded.payload.clone();

        let request = create_v3_request(V3_SERIES_ENDPOINT_URI, encoded, CompressionScheme::zstd_default())
            .await
            .expect("request should be created");

        for column in &request.stats.columns {
            let expected_compressed_len = compressed_v3_len(&column.bytes, CompressionScheme::zstd_default())
                .await
                .expect("column compressed size should be measured");
            assert_eq!(column.compressed_len, expected_compressed_len);
        }

        let mut expected_body = Vec::new();
        {
            let mut os = CodedOutputStream::vec(&mut expected_body);
            os.write_tag(3, WireType::LengthDelimited).unwrap();
            os.write_uint64_no_tag(expected_payload.len() as u64).unwrap();
            os.flush().unwrap();
        }
        expected_body.extend_from_slice(&expected_payload);

        let decoded_body = zstd::stream::decode_all(Cursor::new(request.request.into_body().into_bytes()))
            .expect("compressed V3 body should decode");
        assert_eq!(decoded_body, expected_body);
    }

    #[tokio::test]
    async fn v3_serializer_stats_record_agent_style_value_and_column_counters() {
        const VALUE_SINT64_FIELD_NUMBER: u32 = 17;

        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let metrics = vec![
            Metric::gauge("v3.telemetry.zero", [(123, 0.0), (124, 0.0)]),
            Metric::counter("v3.telemetry.sint64", [(123, 100.0), (124, 200.0)]),
            Metric::gauge("v3.telemetry.float32", [(123, 1.5), (124, 2.25)]),
            Metric::gauge("v3.telemetry.float64", [(123, (1i64 << 30) as f64), (124, 1.5)]),
        ];
        let encoded = encode_v3_metrics_batch(&metrics, &SharedTagSet::default()).expect("metrics should encode to V3");
        let request = create_v3_request(V3_SERIES_ENDPOINT_URI, encoded, CompressionScheme::noop())
            .await
            .expect("request should be created");

        let value_sint64_column = request
            .stats
            .columns
            .iter()
            .find(|column| column.field_number == VALUE_SINT64_FIELD_NUMBER)
            .expect("sint64 value column should be present");
        let value_sint64_len = value_sint64_column.bytes.len() as u64;
        assert_eq!(value_sint64_column.compressed_len as u64, value_sint64_len);

        record_v3_serializer_stats(&serializer_telemetry, &request.stats);

        assert_eq!(
            recorder.counter(("serializer.v3_values_count", &[("type", "zero")])),
            Some(2)
        );
        assert_eq!(
            recorder.counter(("serializer.v3_values_count", &[("type", "sint64")])),
            Some(2)
        );
        assert_eq!(
            recorder.counter(("serializer.v3_values_count", &[("type", "float32")])),
            Some(2)
        );
        assert_eq!(
            recorder.counter(("serializer.v3_values_count", &[("type", "float64")])),
            Some(2)
        );
        assert_eq!(
            recorder.counter((
                "serializer.v3_column_size",
                &[("column", "ValueSint64"), ("compressed", "uncompressed")]
            )),
            Some(value_sint64_len)
        );
        assert_eq!(
            recorder.counter((
                "serializer.v3_column_size",
                &[("column", "ValueSint64"), ("compressed", "compressed")]
            )),
            Some(value_sint64_len)
        );
    }

    async fn create_v3_test_request(metrics: &[Metric]) -> V3EncodedRequest {
        let encoded = encode_v3_metrics_batch(metrics, &SharedTagSet::default()).expect("metrics should encode to V3");
        create_v3_request(V3_SERIES_ENDPOINT_URI, encoded, CompressionScheme::noop())
            .await
            .expect("request should be created")
    }

    fn test_v3_flush_context<'a>(
        ep_config: &'a EndpointConfiguration, payload_limits: V3PayloadLimits,
        serializer_telemetry: &'a V3SerializerTelemetry, telemetry: &'a ComponentTelemetry,
    ) -> V3FlushContext<'a> {
        V3FlushContext {
            endpoint_config: ep_config,
            payload_limits,
            series_endpoint_uri: V3_SERIES_ENDPOINT_URI,
            serializer_telemetry,
            telemetry,
        }
    }

    #[tokio::test]
    async fn v3_payload_requests_split_by_compressed_size_limit() {
        let metrics = vec![
            Metric::counter("v3.compressed.split.one", 1.0),
            Metric::counter("v3.compressed.split.two", 2.0),
        ];
        let single_request = create_v3_test_request(&metrics[..1]).await;
        let combined_request = create_v3_test_request(&metrics).await;
        assert!(combined_request.compressed_len > single_request.compressed_len);

        let limits = V3PayloadLimits::new(single_request.compressed_len, usize::MAX, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());

        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);
        let requests = encode_v3_payload_requests(V3_SERIES_ENDPOINT_URI, &metrics, context, "series").await;

        assert_eq!(2, requests.len());
        assert_eq!(
            vec![1, 1],
            requests.iter().map(|request| request.event_count).collect::<Vec<_>>()
        );
        assert!(requests
            .iter()
            .all(|request| request.request.body().len() <= limits.max_compressed_size));
    }

    #[tokio::test]
    async fn v3_serializer_stats_record_payload_full_split_reason() {
        let metrics = vec![
            Metric::counter("v3.telemetry.payload_full.one", 1.0),
            Metric::counter("v3.telemetry.payload_full.two", 2.0),
        ];
        let single_request = create_v3_test_request(&metrics[..1]).await;
        let limits = V3PayloadLimits::new(single_request.compressed_len, usize::MAX, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());

        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);
        let requests = encode_v3_payload_requests(V3_SERIES_ENDPOINT_URI, &metrics, context, "series").await;

        assert_eq!(2, requests.len());
        assert_eq!(
            recorder.counter(("serializer.v3_payload_split_reason", &[("reason", "payload_full")])),
            Some(1)
        );
    }

    #[tokio::test]
    async fn v3_serializer_stats_record_item_too_big_split_reason() {
        let metrics = vec![Metric::counter("v3.telemetry.item_too_big", 1.0)];
        let request = create_v3_test_request(&metrics).await;
        let limits = V3PayloadLimits::new(request.compressed_len - 1, usize::MAX, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());

        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);
        let requests = encode_v3_payload_requests(V3_SERIES_ENDPOINT_URI, &metrics, context, "series").await;

        assert!(requests.is_empty());
        assert_eq!(recorder.counter("serializer.v3_item_too_big"), Some(1));
        assert_eq!(
            recorder.counter(("serializer.v3_payload_split_reason", &[("reason", "item_too_big")])),
            Some(1)
        );
    }

    #[tokio::test]
    async fn v3_payload_requests_split_by_uncompressed_size_limit() {
        let metrics = vec![
            Metric::counter("v3.uncompressed.split.one", 1.0),
            Metric::counter("v3.uncompressed.split.two", 2.0),
        ];
        let single_request = create_v3_test_request(&metrics[..1]).await;
        let combined_request = create_v3_test_request(&metrics).await;
        assert!(combined_request.uncompressed_len > single_request.uncompressed_len);

        let limits = V3PayloadLimits::new(usize::MAX, single_request.uncompressed_len, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());

        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);
        let requests = encode_v3_payload_requests(V3_SERIES_ENDPOINT_URI, &metrics, context, "series").await;

        assert_eq!(2, requests.len());
        assert_eq!(
            vec![1, 1],
            requests.iter().map(|request| request.event_count).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v3_metric_ranges_split_by_point_limit() {
        let metrics = vec![
            Metric::counter("v3.points.split.one", [(123, 1.0), (124, 2.0)]),
            Metric::counter("v3.points.split.two", [(123, 3.0), (124, 4.0)]),
            Metric::counter("v3.points.split.three", 5.0),
        ];
        let limits = V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 3);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);

        let ranges = split_v3_metric_ranges_by_point_limit(&metrics, context, "series")
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(vec![0..1, 1..3], ranges);
    }

    #[test]
    fn v3_serializer_stats_record_max_points_split_reason() {
        let metrics = vec![
            Metric::counter("v3.telemetry.max_points.one", [(123, 1.0), (124, 2.0)]),
            Metric::counter("v3.telemetry.max_points.two", [(123, 3.0), (124, 4.0)]),
        ];
        let limits = V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 2);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);

        let ranges = split_v3_metric_ranges_by_point_limit(&metrics, context, "series")
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(vec![0..1, 1..2], ranges);
        assert_eq!(
            recorder.counter(("serializer.v3_payload_split_reason", &[("reason", "max_points")])),
            Some(1)
        );
    }

    #[test]
    fn v3_metric_ranges_drop_oversized_metric_after_previous_range() {
        let metrics = vec![
            Metric::counter("v3.points.oversized.before", [(123, 1.0), (124, 2.0)]),
            Metric::counter(
                "v3.points.oversized.too_big",
                [(123, 3.0), (124, 4.0), (125, 5.0), (126, 6.0)],
            ),
            Metric::counter("v3.points.oversized.after", 7.0),
        ];
        let limits = V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 3);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);

        let ranges = split_v3_metric_ranges_by_point_limit(&metrics, context, "series")
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(vec![0..1, 2..3], ranges);
        assert_eq!(recorder.counter("serializer.v3_item_too_big"), Some(1));
    }

    #[test]
    fn v3_metric_ranges_skip_zero_point_metrics() {
        let metrics = vec![
            Metric::counter("v3.points.zero.before", 1.0),
            Metric::counter("v3.points.zero.empty", &[] as &[f64]),
            Metric::counter("v3.points.zero.after", 2.0),
        ];
        let limits = V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);

        let ranges = split_v3_metric_ranges_by_point_limit(&metrics, context, "series")
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(vec![0..1, 2..3], ranges);
    }

    #[tokio::test]
    async fn v3_split_flush_uses_payload_request_batch_headers() {
        let mut metrics = vec![
            Metric::counter("v3.headers.split.one", 1.0),
            Metric::counter("v3.headers.split.two", 2.0),
        ];
        let single_request = create_v3_test_request(&metrics[..1]).await;
        let combined_request = create_v3_test_request(&metrics).await;
        assert!(combined_request.compressed_len > single_request.compressed_len);

        let limits = V3PayloadLimits::new(single_request.compressed_len, usize::MAX, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let batch_id = Uuid::now_v7();
        let (mut payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);
        encode_and_flush_v3_series_metrics(
            context,
            &mut metrics,
            &mut payloads_tx,
            Some(&batch_id),
            Some(MetricsPayloadInfo::v3_series()),
        )
        .await
        .expect("V3 metrics should flush");

        for expected_seq in 0..2 {
            let payload = payloads_rx.recv().await.expect("payload should be emitted");
            let Payload::Http(http_payload) = payload else {
                panic!("expected HTTP payload");
            };
            let (_, request) = http_payload.into_parts();
            assert_eq!(
                batch_id.as_hyphenated().to_string(),
                request
                    .headers()
                    .get("X-Metrics-Request-ID")
                    .expect("batch ID header should be present")
                    .to_str()
                    .expect("batch ID header should be valid")
            );
            assert_eq!(
                expected_seq.to_string(),
                request
                    .headers()
                    .get("X-Metrics-Request-Seq")
                    .expect("batch sequence header should be present")
                    .to_str()
                    .expect("batch sequence header should be valid")
            );
            assert_eq!(
                "2",
                request
                    .headers()
                    .get("X-Metrics-Request-Len")
                    .expect("batch length header should be present")
                    .to_str()
                    .expect("batch length header should be valid")
            );
        }

        assert!(metrics.is_empty());
    }

    #[tokio::test]
    async fn v3_sketch_flush_uses_split_payload_requests() {
        let mut metrics = vec![
            Metric::distribution("v3.sketch.split.one", [1.0, 2.0, 3.0]),
            Metric::distribution("v3.sketch.split.two", [4.0, 5.0, 6.0]),
        ];
        let single_request = create_v3_test_request(&metrics[..1]).await;
        let combined_request = create_v3_test_request(&metrics).await;
        assert!(combined_request.compressed_len > single_request.compressed_len);

        let limits = V3PayloadLimits::new(single_request.compressed_len, usize::MAX, 10_000, 10_000);
        let ep_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&MetricsBuilder::default());
        let batch_id = Uuid::now_v7();
        let (mut payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let context = test_v3_flush_context(&ep_config, limits, &serializer_telemetry, &telemetry);
        encode_and_flush_v3_sketch_metrics(
            context,
            &mut metrics,
            &mut payloads_tx,
            Some(&batch_id),
            Some(MetricsPayloadInfo::v3_sketches()),
        )
        .await
        .expect("V3 sketches should flush");

        for expected_seq in 0..2 {
            let payload = payloads_rx.recv().await.expect("payload should be emitted");
            let Payload::Http(http_payload) = payload else {
                panic!("expected HTTP payload");
            };
            let (_, request) = http_payload.into_parts();
            assert_eq!(V3_SKETCHES_ENDPOINT_URI, request.uri());
            assert_eq!(
                expected_seq.to_string(),
                request
                    .headers()
                    .get("X-Metrics-Request-Seq")
                    .expect("batch sequence header should be present")
                    .to_str()
                    .expect("batch sequence header should be valid")
            );
            assert_eq!(
                "2",
                request
                    .headers()
                    .get("X-Metrics-Request-Len")
                    .expect("batch length header should be present")
                    .to_str()
                    .expect("batch length header should be valid")
            );
        }

        assert!(metrics.is_empty());
    }

    #[test]
    fn v3_series_metric_unit_refs_are_encoded_sparsely() {
        let context = Context::from_static_parts("my.timer.avg", &[]);
        let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
        let gauge = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);
        let context = Context::from_static_parts("my.counter", &[]);
        let no_unit = Metric::from_parts(context, MetricValues::gauge([2.0_f64]), MetricMetadata::default());
        let context = Context::from_static_parts("my.timer.max", &[]);
        let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
        let same_unit = Metric::from_parts(context, MetricValues::gauge([3.0_f64]), metadata);

        let payload = encode_v3_metrics_batch(&[gauge, no_unit, same_unit], &SharedTagSet::default())
            .expect("V3 metric should encode successfully")
            .payload;

        let expected_unit_dict = [
            0xca, 0x01, // field 25, length-delimited.
            0x0c, // field payload length: varint string length + string bytes.
            0x0b, b'm', b'i', b'l', b'l', b'i', b's', b'e', b'c', b'o', b'n', b'd',
        ];
        assert!(
            payload
                .windows(expected_unit_dict.len())
                .any(|window| window == expected_unit_dict),
            "V3 payload should contain DictUnitStr field for 'millisecond', got bytes: {:?}",
            payload
        );

        let expected_unit_ref = [
            0xd2, 0x01, // field 26, length-delimited.
            0x02, // packed field payload length.
            0x02, 0x00, // sparse unit refs for metrics 1 and 3 only: refs [1, 1] -> deltas [1, 0].
        ];
        assert!(
            payload
                .windows(expected_unit_ref.len())
                .any(|window| window == expected_unit_ref),
            "V3 payload should contain UnitRef field for 'millisecond', got bytes: {:?}",
            payload
        );
    }

    #[test]
    fn v3_sketch_metric_unit_not_encoded() {
        let context = Context::from_static_parts("my.histogram", &[]);
        let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
        let histogram = Metric::from_parts(context, MetricValues::histogram([1.0_f64]), metadata);

        let payload = encode_v3_metrics_batch(&[histogram], &SharedTagSet::default())
            .expect("V3 sketch metric should encode successfully")
            .payload;

        assert!(
            !payload
                .windows(b"millisecond".len())
                .any(|window| window == b"millisecond"),
            "V3 sketch payload should not contain unit bytes, matching the Agent V3 sketch builder: {:?}",
            payload
        );
    }

    #[test]
    fn v3_series_promotes_device_and_internal_resource_tags_to_resources() {
        let context = Context::from_static_parts(
            "series.resources",
            &[
                "env:prod",
                "device:switch1",
                "dd.internal.resource:pod:pod-a",
                "dd.internal.resource:malformed",
            ],
        );
        let metadata = MetricMetadata::default().with_hostname(Some(Arc::from("host-a")));
        let metric = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);

        let payload = encode_v3_metrics_batch(&[metric], &SharedTagSet::default())
            .expect("V3 series should encode successfully")
            .payload;

        assert_contains_bytes(&payload, b"env:prod");
        assert!(!contains_bytes(&payload, b"device:switch1"));
        assert!(!contains_bytes(&payload, b"dd.internal.resource:pod:pod-a"));
        assert!(!contains_bytes(&payload, b"dd.internal.resource:malformed"));

        let expected_resource_dict = [
            0x22, // field 4, length-delimited.
            0x25, // field payload length.
            0x04, b'h', b'o', b's', b't', 0x06, b'h', b'o', b's', b't', b'-', b'a', 0x06, b'd', b'e', b'v', b'i', b'c',
            b'e', 0x07, b's', b'w', b'i', b't', b'c', b'h', b'1', 0x03, b'p', b'o', b'd', 0x05, b'p', b'o', b'd', b'-',
            b'a',
        ];
        assert_contains_bytes(&payload, &expected_resource_dict);
    }

    #[test]
    fn v3_series_promotes_additional_and_origin_resource_tags_without_empty_host() {
        let context = Context::from_static_parts("series.additional_origin_resources", &["env:prod"])
            .with_origin_tags(tag_set(["dd.internal.resource:pod:pod-origin"]));
        let additional_tags = SharedTagSet::from(tag_set([
            "team:core",
            "device:switch1",
            "dd.internal.resource:container:container-a",
        ]));
        let metadata = MetricMetadata::default().with_hostname(Some(Arc::from("")));
        let metric = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);

        let payload = encode_v3_metrics_batch(&[metric], &additional_tags)
            .expect("V3 series should encode successfully")
            .payload;

        assert_contains_bytes(&payload, b"env:prod");
        assert_contains_bytes(&payload, b"team:core");
        assert!(!contains_bytes(&payload, b"device:switch1"));
        assert!(!contains_bytes(&payload, b"dd.internal.resource:container:container-a"));
        assert!(!contains_bytes(&payload, b"dd.internal.resource:pod:pod-origin"));

        let expected_resource_dict = [
            0x22, // field 4, length-delimited.
            0x34, // field payload length.
            0x06, b'd', b'e', b'v', b'i', b'c', b'e', 0x07, b's', b'w', b'i', b't', b'c', b'h', b'1', 0x03, b'p', b'o',
            b'd', 0x0a, b'p', b'o', b'd', b'-', b'o', b'r', b'i', b'g', b'i', b'n', 0x09, b'c', b'o', b'n', b't', b'a',
            b'i', b'n', b'e', b'r', 0x0b, b'c', b'o', b'n', b't', b'a', b'i', b'n', b'e', b'r', b'-', b'a',
        ];
        assert_contains_bytes(&payload, &expected_resource_dict);
        assert!(!contains_bytes(&payload, b"host"));
    }

    #[test]
    fn v3_sketch_keeps_device_and_internal_resource_tags_as_tags() {
        let context = Context::from_static_parts(
            "sketch.resources",
            &["env:prod", "device:switch1", "dd.internal.resource:pod:pod-a"],
        );
        let metadata = MetricMetadata::default().with_hostname(Some(Arc::from("host-a")));
        let metric = Metric::from_parts(context, MetricValues::histogram([1.0_f64]), metadata);

        let payload = encode_v3_metrics_batch(&[metric], &SharedTagSet::default())
            .expect("V3 sketch should encode successfully")
            .payload;

        assert_contains_bytes(&payload, b"env:prod");
        assert_contains_bytes(&payload, b"device:switch1");
        assert_contains_bytes(&payload, b"dd.internal.resource:pod:pod-a");

        let expected_resource_dict = [
            0x22, // field 4, length-delimited.
            0x0c, // field payload length.
            0x04, b'h', b'o', b's', b't', 0x06, b'h', b'o', b's', b't', b'-', b'a',
        ];
        assert_contains_bytes(&payload, &expected_resource_dict);
    }

    #[tokio::test]
    async fn validation_split_flush_assigns_batch_id_to_carried_metric() {
        let v2_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 1, None);
        let v2_series_builder = Some(
            v2::create_v2_request_builder(MetricsEndpoint::SeriesV2, &v2_endpoint_config)
                .await
                .expect("V2 request builder should be created"),
        );
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            v2_series_builder,
            None,
            MetricsEncoderMode::Validation,
            MetricsEncoderMode::V2Only,
            V3RuntimeConfig {
                endpoint_config: v3_endpoint_config,
                payload_limits: V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 10_000),
                series_endpoint_uri: V3_SERIES_ENDPOINT_URI.to_string(),
                shadow_series_endpoint_uri: "/api/intake/metrics/v3beta/series".to_string(),
                series_shadow_config: SeriesShadowConfig::new(0.0),
                serializer_telemetry,
            },
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(10),
            false,
        ));

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::counter("validation.split.one", 1.0)))
            .is_none());
        assert!(events
            .try_push(Event::Metric(Metric::counter("validation.split.two", 2.0)))
            .is_none());
        events_tx
            .send(events)
            .await
            .expect("events should be sent to request builder");

        let mut flushed_requests = Vec::new();
        for _ in 0..4 {
            let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
                .await
                .expect("payload should arrive before timeout")
                .expect("payload channel should remain open");
            let Payload::Http(http_payload) = payload else {
                panic!("expected HTTP payload");
            };
            let (_, request) = http_payload.into_parts();
            let batch_id = request
                .headers()
                .get("X-Metrics-Request-ID")
                .expect("validation batch ID header should be present")
                .to_str()
                .expect("validation batch ID should be valid header text")
                .to_string();
            flushed_requests.push((request.uri().to_string(), batch_id));
        }

        assert_eq!("/api/v2/series", flushed_requests[0].0);
        assert_eq!(V3_SERIES_ENDPOINT_URI, flushed_requests[1].0);
        assert_eq!("/api/v2/series", flushed_requests[2].0);
        assert_eq!(V3_SERIES_ENDPOINT_URI, flushed_requests[3].0);

        assert_eq!(flushed_requests[0].1, flushed_requests[1].1);
        assert_eq!(flushed_requests[2].1, flushed_requests[3].1);
        assert_ne!(flushed_requests[0].1, flushed_requests[2].1);

        drop(events_tx);
        request_builder_handle
            .await
            .expect("request builder task should complete")
            .expect("request builder should stop cleanly");
    }

    #[tokio::test]
    async fn authoritative_v3_flushes_previous_point_limit_batch() {
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            None,
            None,
            MetricsEncoderMode::V3Enabled,
            MetricsEncoderMode::V2Only,
            V3RuntimeConfig {
                endpoint_config: v3_endpoint_config,
                payload_limits: V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 3),
                series_endpoint_uri: V3_SERIES_ENDPOINT_URI.to_string(),
                shadow_series_endpoint_uri: "/api/intake/metrics/v3beta/series".to_string(),
                series_shadow_config: SeriesShadowConfig::new(0.0),
                serializer_telemetry,
            },
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(250),
            false,
        ));

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::counter(
                "authoritative.v3.points.one",
                [(123, 1.0), (124, 2.0)]
            )))
            .is_none());
        assert!(events
            .try_push(Event::Metric(Metric::counter(
                "authoritative.v3.points.two",
                [(123, 3.0), (124, 4.0)]
            )))
            .is_none());
        events_tx
            .send(events)
            .await
            .expect("events should be sent to request builder");

        let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
            .await
            .expect("point-limit payload should arrive before timeout")
            .expect("payload channel should remain open");
        let Payload::Http(http_payload) = payload else {
            panic!("expected HTTP payload");
        };
        let (_, request) = http_payload.into_parts();
        assert_eq!(V3_SERIES_ENDPOINT_URI, request.uri());
        assert!(timeout(Duration::from_millis(50), payloads_rx.recv()).await.is_err());

        let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
            .await
            .expect("timeout payload should arrive before timeout")
            .expect("payload channel should remain open");
        let Payload::Http(http_payload) = payload else {
            panic!("expected HTTP payload");
        };
        let (_, request) = http_payload.into_parts();
        assert_eq!(V3_SERIES_ENDPOINT_URI, request.uri());
        assert_eq!(
            recorder.counter(("serializer.v3_payload_split_reason", &[("reason", "max_points")])),
            Some(1)
        );

        drop(events_tx);
        request_builder_handle
            .await
            .expect("request builder task should complete")
            .expect("request builder should stop cleanly");
    }

    #[tokio::test]
    async fn authoritative_v3_sketches_flush_previous_point_limit_batch() {
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let recorder = TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            None,
            None,
            MetricsEncoderMode::V2Only,
            MetricsEncoderMode::V3Enabled,
            V3RuntimeConfig {
                endpoint_config: v3_endpoint_config,
                payload_limits: V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 3),
                series_endpoint_uri: V3_SERIES_ENDPOINT_URI.to_string(),
                shadow_series_endpoint_uri: "/api/intake/metrics/v3beta/series".to_string(),
                series_shadow_config: SeriesShadowConfig::new(0.0),
                serializer_telemetry,
            },
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(250),
            false,
        ));

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::distribution(
                "authoritative.v3.sketch.points.one",
                [(123, 1.0), (124, 2.0)]
            )))
            .is_none());
        assert!(events
            .try_push(Event::Metric(Metric::distribution(
                "authoritative.v3.sketch.points.two",
                [(123, 3.0), (124, 4.0)]
            )))
            .is_none());
        events_tx
            .send(events)
            .await
            .expect("events should be sent to request builder");

        for expected in ["point-limit", "timeout"] {
            let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{expected} sketches payload should arrive before timeout"))
                .expect("payload channel should remain open");
            let Payload::Http(http_payload) = payload else {
                panic!("expected HTTP payload");
            };
            let (_, request) = http_payload.into_parts();
            assert_eq!(V3_SKETCHES_ENDPOINT_URI, request.uri());
        }
        assert_eq!(
            recorder.counter(("serializer.v3_payload_split_reason", &[("reason", "max_points")])),
            Some(1)
        );

        drop(events_tx);
        request_builder_handle
            .await
            .expect("request builder task should complete")
            .expect("request builder should stop cleanly");
    }

    #[tokio::test]
    async fn authoritative_v3_does_not_flush_on_v2_boundary() {
        let v2_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 1, None);
        let v2_series_builder = Some(
            v2::create_v2_request_builder(MetricsEndpoint::SeriesV2, &v2_endpoint_config)
                .await
                .expect("V2 request builder should be created"),
        );
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            v2_series_builder,
            None,
            MetricsEncoderMode::V3Enabled,
            MetricsEncoderMode::V2Only,
            V3RuntimeConfig {
                endpoint_config: v3_endpoint_config,
                payload_limits: V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 10_000),
                series_endpoint_uri: V3_SERIES_ENDPOINT_URI.to_string(),
                shadow_series_endpoint_uri: "/api/intake/metrics/v3beta/series".to_string(),
                series_shadow_config: SeriesShadowConfig::new(0.0),
                serializer_telemetry,
            },
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(250),
            false,
        ));

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::counter("authoritative.v3.decouple.one", 1.0)))
            .is_none());
        assert!(events
            .try_push(Event::Metric(Metric::counter("authoritative.v3.decouple.two", 2.0)))
            .is_none());
        events_tx
            .send(events)
            .await
            .expect("events should be sent to request builder");

        let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
            .await
            .expect("V2 split payload should arrive before timeout")
            .expect("payload channel should remain open");
        let Payload::Http(http_payload) = payload else {
            panic!("expected HTTP payload");
        };
        let (_, request) = http_payload.into_parts();
        assert_eq!("/api/v2/series", request.uri());
        assert!(timeout(Duration::from_millis(50), payloads_rx.recv()).await.is_err());

        let mut timeout_flush_uris = Vec::new();
        for _ in 0..2 {
            let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
                .await
                .expect("timeout payload should arrive before timeout")
                .expect("payload channel should remain open");
            let Payload::Http(http_payload) = payload else {
                panic!("expected HTTP payload");
            };
            let (_, request) = http_payload.into_parts();
            timeout_flush_uris.push(request.uri().to_string());
        }
        assert_eq!(2, timeout_flush_uris.len());
        assert!(timeout_flush_uris.iter().any(|uri| uri == "/api/v2/series"));
        assert!(timeout_flush_uris.iter().any(|uri| uri == V3_SERIES_ENDPOINT_URI));

        drop(events_tx);
        request_builder_handle
            .await
            .expect("request builder task should complete")
            .expect("request builder should stop cleanly");
    }

    #[tokio::test]
    async fn shadow_sampled_series_flush_sends_v2_and_v3_beta_with_same_batch_id() {
        let v2_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let v2_series_builder = Some(
            v2::create_v2_request_builder(MetricsEndpoint::SeriesV2, &v2_endpoint_config)
                .await
                .expect("V2 request builder should be created"),
        );
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            v2_series_builder,
            None,
            MetricsEncoderMode::V2Only,
            MetricsEncoderMode::V2Only,
            V3RuntimeConfig {
                endpoint_config: v3_endpoint_config,
                payload_limits: V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 10_000),
                series_endpoint_uri: V3_SERIES_ENDPOINT_URI.to_string(),
                shadow_series_endpoint_uri: "/api/intake/metrics/v3beta/custom".to_string(),
                series_shadow_config: SeriesShadowConfig::new(1.0),
                serializer_telemetry,
            },
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(10),
            false,
        ));

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::counter("shadow.sampled", 1.0)))
            .is_none());
        events_tx
            .send(events)
            .await
            .expect("events should be sent to request builder");

        let mut flushed_requests = Vec::new();
        for _ in 0..2 {
            let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
                .await
                .expect("payload should arrive before timeout")
                .expect("payload channel should remain open");
            let Payload::Http(http_payload) = payload else {
                panic!("expected HTTP payload");
            };
            let (metadata, request) = http_payload.into_parts();
            let payload_info = *metadata
                .get::<MetricsPayloadInfo>()
                .expect("metrics payload info should be present");
            let batch_id = request
                .headers()
                .get("X-Metrics-Request-ID")
                .expect("shadow batch ID header should be present")
                .to_str()
                .expect("shadow batch ID should be valid header text")
                .to_string();
            flushed_requests.push((request.uri().to_string(), payload_info, batch_id));
        }

        assert_eq!("/api/v2/series", flushed_requests[0].0);
        assert_eq!(MetricsPayloadInfo::v2_shadow_series(), flushed_requests[0].1);
        assert_eq!("/api/intake/metrics/v3beta/custom", flushed_requests[1].0);
        assert_eq!(MetricsPayloadInfo::v3_shadow_series(), flushed_requests[1].1);
        assert_eq!(flushed_requests[0].2, flushed_requests[1].2);

        drop(events_tx);
        request_builder_handle
            .await
            .expect("request builder task should complete")
            .expect("request builder should stop cleanly");
    }

    #[tokio::test]
    async fn shadow_sample_rate_zero_sends_only_v2_without_validation_headers() {
        let v2_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let v2_series_builder = Some(
            v2::create_v2_request_builder(MetricsEndpoint::SeriesV2, &v2_endpoint_config)
                .await
                .expect("V2 request builder should be created"),
        );
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let serializer_telemetry = V3SerializerTelemetry::from_builder(&metrics_builder);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            v2_series_builder,
            None,
            MetricsEncoderMode::V2Only,
            MetricsEncoderMode::V2Only,
            V3RuntimeConfig {
                endpoint_config: v3_endpoint_config,
                payload_limits: V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 10_000),
                series_endpoint_uri: V3_SERIES_ENDPOINT_URI.to_string(),
                shadow_series_endpoint_uri: "/api/intake/metrics/v3beta/series".to_string(),
                series_shadow_config: SeriesShadowConfig::new(0.0),
                serializer_telemetry,
            },
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(10),
            false,
        ));

        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::counter("shadow.disabled", 1.0)))
            .is_none());
        events_tx
            .send(events)
            .await
            .expect("events should be sent to request builder");

        let payload = timeout(Duration::from_secs(1), payloads_rx.recv())
            .await
            .expect("payload should arrive before timeout")
            .expect("payload channel should remain open");
        let Payload::Http(http_payload) = payload else {
            panic!("expected HTTP payload");
        };
        let (_, request) = http_payload.into_parts();
        assert_eq!("/api/v2/series", request.uri());
        assert!(!request.headers().contains_key("X-Metrics-Request-ID"));
        assert!(timeout(Duration::from_millis(50), payloads_rx.recv()).await.is_err());

        drop(events_tx);
        request_builder_handle
            .await
            .expect("request builder task should complete")
            .expect("request builder should stop cleanly");
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    fn assert_contains_bytes(haystack: &[u8], needle: &[u8]) {
        assert!(
            contains_bytes(haystack, needle),
            "expected payload to contain bytes {:?}, got {:?}",
            needle,
            haystack
        );
    }

    fn tag_set<const N: usize>(tags: [&'static str; N]) -> TagSet {
        tags.into_iter().map(Tag::from_static).collect()
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DatadogMetricsConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_METRICS_CONFIGURATION,
            &[
                "serializer_experimental_use_v3_api.sketches.beta_route",
                "serializer_experimental_use_v3_api.sketches.shadow_sample_rate",
                "serializer_experimental_use_v3_api.sketches.shadow_sites",
                "serializer_experimental_use_v3_api.sketches.use_beta",
            ],
            json!({}),
            |cfg| {
                cfg.as_typed::<DatadogMetricsConfiguration>()
                    .expect("DatadogMetricsConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}

#[cfg(test)]
mod use_v2_api_series_default {
    use saluki_config::ConfigurationLoader;
    use serde_json::json;

    use super::{v2, DatadogMetricsConfiguration};
    use crate::{common::datadog::clamp_payload_limits, config::KEY_ALIASES};

    /// `use_v2_api_series` defaults to `true`, preserving V2 protobuf behavior when the flag is absent.
    #[tokio::test]
    async fn defaults_to_true_when_absent() {
        let cfg = ConfigurationLoader::default()
            .with_key_aliases(KEY_ALIASES)
            .add_providers([figment::providers::Serialized::defaults(json!({}))])
            .into_generic()
            .await
            .expect("config should load");
        let parsed: DatadogMetricsConfiguration = cfg.as_typed().expect("should deserialize");
        assert!(parsed.use_v2_api_series);
    }

    #[tokio::test]
    async fn deserializes_payload_limit_keys() {
        let cfg = ConfigurationLoader::default()
            .with_key_aliases(KEY_ALIASES)
            .add_providers([figment::providers::Serialized::defaults(json!({
                "serializer_max_payload_size": 4321,
                "serializer_max_uncompressed_payload_size": 8765,
                "serializer_max_series_payload_size": 1234,
                "serializer_max_series_uncompressed_payload_size": 5678,
            }))])
            .into_generic()
            .await
            .expect("config should load");
        let parsed: DatadogMetricsConfiguration = cfg.as_typed().expect("should deserialize");

        assert_eq!(parsed.max_payload_size, 4321);
        assert_eq!(parsed.max_uncompressed_payload_size, 8765);
        assert_eq!(parsed.max_series_payload_size, 1234);
        assert_eq!(parsed.max_series_uncompressed_payload_size, 5678);
    }

    #[tokio::test]
    async fn deserializes_max_series_points_per_payload() {
        // Default should be 10,000.
        let cfg = ConfigurationLoader::default()
            .with_key_aliases(KEY_ALIASES)
            .add_providers([figment::providers::Serialized::defaults(json!({}))])
            .into_generic()
            .await
            .expect("config should load");
        let parsed: DatadogMetricsConfiguration = cfg.as_typed().expect("should deserialize");
        assert_eq!(parsed.max_series_points_per_payload, 10_000);
        assert_eq!(parsed.v3_payload_limits().max_points_per_payload, 10_000);

        // Explicit value should round-trip.
        let cfg = ConfigurationLoader::default()
            .with_key_aliases(KEY_ALIASES)
            .add_providers([figment::providers::Serialized::defaults(json!({
                "serializer_max_series_points_per_payload": 500,
            }))])
            .into_generic()
            .await
            .expect("config should load");
        let parsed: DatadogMetricsConfiguration = cfg.as_typed().expect("should deserialize");
        assert_eq!(parsed.max_series_points_per_payload, 500);
        assert_eq!(parsed.v3_payload_limits().max_points_per_payload, 500);
    }

    #[test]
    fn clamps_series_payload_limit_keys_to_api_limits() {
        let (uncompressed_limit, compressed_limit) = clamp_payload_limits(
            v2::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT + 1,
            v2::SERIES_V2_COMPRESSED_SIZE_LIMIT + 1,
            v2::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            v2::SERIES_V2_COMPRESSED_SIZE_LIMIT,
        );
        assert_eq!(uncompressed_limit, v2::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT);
        assert_eq!(compressed_limit, v2::SERIES_V2_COMPRESSED_SIZE_LIMIT);

        let (uncompressed_limit, compressed_limit) = clamp_payload_limits(
            5678,
            1234,
            v2::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            v2::SERIES_V2_COMPRESSED_SIZE_LIMIT,
        );
        assert_eq!(uncompressed_limit, 5678);
        assert_eq!(compressed_limit, 1234);
    }
}
