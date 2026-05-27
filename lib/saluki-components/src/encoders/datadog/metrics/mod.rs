use std::{collections::VecDeque, ops::Range, time::Duration};

use async_trait::async_trait;
use ddsketch::DDSketch;
use facet::Facet;
use http::{HeaderValue, Method, Request};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use protobuf::{rt::WireType, CodedOutputStream};
use saluki_common::{
    buf::{ChunkedBytesBuffer, FrozenChunkedBytesBuffer},
    task::HandleExt as _,
};
use saluki_config::GenericConfiguration;
use saluki_context::tags::SharedTagSet;
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
use uuid::Uuid;

use self::v3::{V3EncodedRequest, V3PayloadLimits, V3PayloadRequest};
use crate::{
    common::datadog::{
        clamp_payload_limits,
        io::RB_BUFFER_CHUNK_SIZE,
        protocol::{MetricsPayloadInfo, V3ApiConfig},
        request_builder::RequestBuilder,
        telemetry::ComponentTelemetry,
        DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT, DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
    },
    encoders::datadog::metrics::v2::MetricsEndpointEncoder,
};

mod endpoint;
use self::endpoint::{EndpointConfiguration, MetricsEndpoint};

mod v1;
mod v2;
mod v3;

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const V3_SERIES_ENDPOINT_URI: &str = "/api/intake/metrics/v3/series";
const V3_SKETCHES_ENDPOINT_URI: &str = "/api/intake/metrics/v3/sketches";

// V3 keeps the Datadog Agent's point-count limit as an internal bound, not user-facing ADP configuration.
const SERIES_V3_POINTS_PER_PAYLOAD_LIMIT: usize = 10_000;

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

const fn default_flush_timeout_secs() -> u64 {
    2
}

fn default_serializer_compressor_kind() -> String {
    DEFAULT_SERIALIZER_COMPRESSOR_KIND.to_owned()
}

const fn default_zstd_compressor_level() -> i32 {
    3
}

const fn default_use_v2_api_series() -> bool {
    true
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

    /// Additional tags to apply to all forwarded metrics.
    #[serde(default, skip)]
    #[facet(opaque)]
    additional_tags: Option<SharedTagSet>,

    /// V3 API configuration for per-endpoint V3 support.
    ///
    /// Configures which endpoints receive V3 payloads and whether validation mode is enabled.
    #[serde(rename = "serializer_experimental_use_v3_api", default)]
    v3_api: V3ApiConfig,
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

        let v2_compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);
        let v3_compression_scheme = if self.v3_api.compression_level > 0 {
            CompressionScheme::new(&self.compressor_kind, self.v3_api.compression_level)
        } else {
            v2_compression_scheme
        };
        let v3_series_endpoint_uri = if self.v3_api.series.use_beta {
            self.v3_api.series.beta_route.clone()
        } else {
            V3_SERIES_ENDPOINT_URI.to_string()
        };
        let v3_payload_limits = V3PayloadLimits::new(
            self.max_series_payload_size,
            self.max_series_uncompressed_payload_size,
            self.max_metrics_per_payload,
            SERIES_V3_POINTS_PER_PAYLOAD_LIMIT,
        );

        let v2_endpoint_config = EndpointConfiguration::new(
            v2_compression_scheme,
            self.max_metrics_per_payload,
            self.additional_tags.clone(),
        );
        let v3_endpoint_config = EndpointConfiguration::new(
            v3_compression_scheme,
            self.max_metrics_per_payload,
            self.additional_tags.clone(),
        );

        // Derive the encoding mode for each metric type from the configuration.
        let series_mode =
            MetricsEncoderMode::from_config(self.v3_api.use_v3_series(), self.v3_api.use_v3_series_validate());
        let sketches_mode =
            MetricsEncoderMode::from_config(self.v3_api.use_v3_sketches(), self.v3_api.use_v3_sketches_validate());

        let series_endpoint = if self.use_v2_api_series {
            MetricsEndpoint::SeriesV2
        } else {
            MetricsEndpoint::SeriesV1
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
            v3_endpoint_config,
            v3_payload_limits,
            v3_series_endpoint_uri,
            telemetry,
            flush_timeout,
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
    v3_endpoint_config: EndpointConfiguration,
    v3_payload_limits: V3PayloadLimits,
    v3_series_endpoint_uri: String,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl Encoder for DatadogMetrics {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            v2_series_builder,
            v2_sketch_builder,
            series_mode,
            sketches_mode,
            v3_endpoint_config,
            v3_payload_limits,
            v3_series_endpoint_uri,
            telemetry,
            flush_timeout,
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
            v3_endpoint_config,
            v3_payload_limits,
            v3_series_endpoint_uri,
            telemetry,
            events_rx,
            payloads_tx,
            flush_timeout,
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

#[allow(clippy::too_many_arguments)]
async fn run_request_builder(
    mut v2_series_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    mut v2_sketch_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>, series_mode: MetricsEncoderMode,
    sketches_mode: MetricsEncoderMode, v3_endpoint_config: EndpointConfiguration, v3_payload_limits: V3PayloadLimits,
    v3_series_endpoint_uri: String, telemetry: ComponentTelemetry, mut events_rx: mpsc::Receiver<EventsBuffer>,
    mut payloads_tx: mpsc::Sender<PayloadsBuffer>, flush_timeout: Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    let mut v3_series_metrics = series_mode.needs_v3().then(Vec::<Metric>::new);
    let mut v3_sketch_metrics = sketches_mode.needs_v3().then(Vec::<Metric>::new);

    let mut series_batch_id = None;
    let mut sketches_batch_id = None;

    let tag_series = series_mode.needs_tagging();
    let tag_sketches = sketches_mode.needs_tagging();
    let v3_flush_context = V3FlushContext {
        endpoint_config: &v3_endpoint_config,
        payload_limits: v3_payload_limits,
        series_endpoint_uri: &v3_series_endpoint_uri,
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

                    // Figure out which endpoint the metric belongs to, and grab the relevant V2 builder/V3 storage.
                    let endpoint = MetricsEndpoint::from_metric(&metric);
                    let (endpoint_mode, maybe_v2_builder, maybe_v3_metrics, batch_id) = match endpoint {
                        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => (
                            series_mode,
                            &mut v2_series_builder,
                            &mut v3_series_metrics,
                            &mut series_batch_id,
                        ),
                        MetricsEndpoint::Sketches => (
                            sketches_mode,
                            &mut v2_sketch_builder,
                            &mut v3_sketch_metrics,
                            &mut sketches_batch_id,
                        ),
                    };
                    if endpoint_mode.needs_batch_id() && batch_id.is_none() {
                        *batch_id = Some(Uuid::now_v7());
                    }
                    let active_batch_id = endpoint_mode.needs_batch_id().then_some(batch_id.as_ref()).flatten();

                    // Store a copy of the metric in `maybe_v3_metrics` if it's present.
                    //
                    // We have to do this before encoding because `RequestBuilder::encode` consumes the metric. This also means we'll
                    // need to _remove_ the metric if encoding fails.
                    if let Some(metrics) = maybe_v3_metrics {
                        metrics.push(metric.clone());
                    }

                    // Attempt encoding the metric for V2 if configured.
                    //
                    // If the metric couldn't be encoded (too big, some other issue), the call returns `false` which is
                    // our signal to remove the metric from `maybe_v3_metrics` (if we added it), since we know now that
                    // the metric wasn't encoded for V2 and we want our V2/V3 payload batches to be consistent in
                    // validation mode.
                    let v2_payload_info = match endpoint {
                        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => tag_series.then(MetricsPayloadInfo::v2_series),
                        MetricsEndpoint::Sketches => tag_sketches.then(MetricsPayloadInfo::v2_sketches),
                    };
                    let v2_flushed = if let Some(builder) = maybe_v2_builder {
                        let result = encode_v2_metrics(builder, metric, &telemetry, &mut payloads_tx, active_batch_id, v2_payload_info).await?;
                        if !result.encoded() {
                            if let Some(metrics) = maybe_v3_metrics {
                                let _ = metrics.pop();
                            }
                        }

                        result.flushed()
                    } else {
                        false
                    };

                    // If we flushed via V2, or we've hit our max metrics per payload limit in pure V3 mode, we need to flush our V3 metrics
                    // as well.
                    let v3_payload_info = match endpoint {
                        MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => tag_series.then(MetricsPayloadInfo::v3_series),
                        MetricsEndpoint::Sketches => tag_sketches.then(MetricsPayloadInfo::v3_sketches),
                    };
                    let mut carried_metric_into_next_batch = false;
                    let v3_flushed = if let Some(v3_metrics) = maybe_v3_metrics {
                        let should_flush_v3 = match endpoint_mode {
                            MetricsEncoderMode::V2Only => false,
                            MetricsEncoderMode::V3Enabled => {
                                v2_flushed || v3_flush_context.payload_limits.should_flush_metric_count_limit(v3_metrics)
                            }
                            MetricsEncoderMode::Validation => v2_flushed,
                        };
                        if should_flush_v3 {
                            // V2 flushes the previous batch without the current metric (the metric
                            // that triggered the flush is re-encoded into the next V2 batch). Pop it
                            // from V3 before flushing so both batches cover the same set of metrics.
                            let split_metric = if v2_flushed { v3_metrics.pop() } else { None };
                            encode_and_flush_v3_metrics(
                                endpoint,
                                v3_flush_context,
                                v3_metrics,
                                &mut payloads_tx,
                                active_batch_id,
                                v3_payload_info,
                            )
                            .await?;
                            if let Some(m) = split_metric {
                                carried_metric_into_next_batch = true;
                                v3_metrics.push(m);
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    // If a V2-triggered split leaves the current metric pending in the next batch, assign that pending
                    // V2/V3 pair a fresh validation ID. Otherwise, the next timeout flush would omit validation headers.
                    if endpoint_mode.needs_batch_id() && (v2_flushed || v3_flushed) {
                        *batch_id = carried_metric_into_next_batch.then(Uuid::now_v7);
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
                let v2_series_payload_info = tag_series.then(MetricsPayloadInfo::v2_series);
                let series_active_batch_id = series_mode.needs_batch_id().then_some(series_batch_id.as_ref()).flatten();
                let mut v2_series_flush_succeeded = true;
                if let Some(builder) = &mut v2_series_builder {
                    if let Err(e) = flush_v2_metrics(builder, &mut payloads_tx, series_active_batch_id, v2_series_payload_info).await {
                        error!(error = %e, "Failed to flush V2 series metrics: {}", e);
                        v2_series_flush_succeeded = false;
                    }
                }

                let v3_series_payload_info = tag_series.then(MetricsPayloadInfo::v3_series);
                if let Some(metrics) = &mut v3_series_metrics {
                    if v2_series_flush_succeeded {
                        if let Err(e) = encode_and_flush_v3_series_metrics(
                            v3_flush_context,
                            metrics,
                            &mut payloads_tx,
                            series_active_batch_id,
                            v3_series_payload_info,
                        )
                        .await
                        {
                            error!(error = %e, "Failed to flush V3 series metrics: {}", e);
                        }
                    } else {
                        warn!("Failed to flush V2 series metrics, skipping V3 series flush.");
                        metrics.clear();
                    }
                }
                if series_mode.needs_batch_id() {
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
                    if v2_sketches_flush_succeeded {
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
                    } else {
                        warn!("Failed to flush V2 sketch metrics, skipping V3 sketch flush.");
                        metrics.clear();
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
            requests.push(V3PayloadRequest {
                request: encoded_request.request,
                event_count,
                data_point_count,
            });
            continue;
        }

        if range.len() == 1 {
            // The encoded request is too large and this range cannot be split any further.
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
        let pivot = range.start + range.len() / 2;
        pending_ranges.push_front(pivot..range.end);
        pending_ranges.push_front(range.start..pivot);
    }

    requests
}

fn split_v3_metric_ranges_by_point_limit(
    metrics: &[Metric], context: V3FlushContext<'_>, payload_kind: &'static str,
) -> VecDeque<Range<usize>> {
    let mut ranges = VecDeque::new();
    let mut current_start = None;
    let mut current_points = 0usize;

    for idx in 0..metrics.len() {
        let metric_points = metrics[idx].values().len();
        if metric_points == 0 {
            // The Agent drops zero-point V3 metrics before writing them.
            context.telemetry.events_dropped_encoder().increment(1);
            continue;
        }

        if !context.payload_limits.point_count_fits(metric_points) {
            // This metric exceeds the point limit by itself, so it cannot fit in any V3 payload request.
            // Close the current range before dropping this oversized metric.
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
fn encode_v3_metrics_batch(metrics: &[Metric], additional_tags: &SharedTagSet) -> Result<Vec<u8>, GenericError> {
    let mut writer = v3::V3Writer::new();

    for metric in metrics {
        write_metric_to_v3(&mut writer, metric, additional_tags);
    }

    let mut output = Vec::new();
    writer
        .finalize(&mut output)
        .map_err(|e| generic_error!("Failed to serialize V3 payload: {}", e))?;

    Ok(output)
}

/// Writes a single metric to the V3 writer.
fn write_metric_to_v3(writer: &mut v3::V3Writer, metric: &Metric, additional_tags: &SharedTagSet) {
    let metric_type = match metric.values() {
        MetricValues::Counter(..) => v3::V3MetricType::Count,
        MetricValues::Rate(..) => v3::V3MetricType::Rate,
        MetricValues::Gauge(..) | MetricValues::Set(..) => v3::V3MetricType::Gauge,
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => v3::V3MetricType::Sketch,
    };

    let mut builder = writer.write(metric_type, metric.context().name());

    // Tags - chain instrumented + additional + origin tags
    let all_tags = metric
        .context()
        .tags()
        .into_iter()
        .chain(additional_tags)
        .chain(metric.context().origin_tags())
        .filter(|t| !t.name().starts_with("dd.internal.resource"))
        .map(|t| t.as_str());
    builder.set_tags(all_tags);

    // Resources - extract host and any dd.internal.resource tags
    let mut resources = Vec::new();
    if let Some(host) = metric.metadata().hostname() {
        resources.push(("host", host));
    }
    // Extract dd.internal.resource tags as resources
    for tag in metric.context().tags().into_iter().chain(additional_tags) {
        if tag.name() == "dd.internal.resource" {
            if let Some(value) = tag.value() {
                if let Some((rtype, rname)) = value.split_once(':') {
                    resources.push((rtype, rname));
                }
            }
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

/// Creates a V3 HTTP request from encoded payload data.
async fn create_v3_request(
    endpoint_uri: &str, payload: Vec<u8>, compression_scheme: CompressionScheme,
) -> Result<V3EncodedRequest, GenericError> {
    // Our `payload` is the inner `MetricData` message structure at this point, so we just manually write out the
    // `Payload` message framing before writing the metric data.
    let mut header_buf = [0; 16];
    let header_len = {
        let mut header_writer = CodedOutputStream::bytes(&mut header_buf);
        header_writer.write_tag(3, WireType::LengthDelimited)?;
        header_writer.write_uint64_no_tag(payload.len() as u64)?;
        header_writer.flush()?;
        header_writer.total_bytes_written() as usize
    };

    let uncompressed_len = header_len + payload.len();
    let buffer = ChunkedBytesBuffer::new(RB_BUFFER_CHUNK_SIZE);
    let mut compressor = Compressor::from_scheme(compression_scheme, buffer);
    compressor
        .write_all(&header_buf[..header_len])
        .await
        .error_context("Failed to compress V3 payload.")?;
    compressor
        .write_all(&payload)
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

    let content_encoding = compressor.content_encoding();
    let compressed_buf = compressor.into_inner().freeze();
    let compressed_len = compressed_buf.len();

    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(endpoint_uri)
        .header(http::header::CONTENT_TYPE, "application/x-protobuf");

    if let Some(encoding) = content_encoding {
        builder = builder.header(http::header::CONTENT_ENCODING, encoding);
    }

    let request = builder
        .body(compressed_buf)
        .map_err(|e| generic_error!("Failed to build V3 request: {}", e))?;

    Ok(V3EncodedRequest {
        request,
        compressed_len,
        uncompressed_len,
    })
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::{event::Event, payload::Payload};
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
        assert_eq!(
            Some("https://app.datadoghq.eu"),
            config.v3_api.sketches.endpoints.first().map(String::as_str)
        );
    }

    #[tokio::test]
    async fn create_v3_request_uses_configured_endpoint_uri() {
        let request = create_v3_request(
            "/api/intake/metrics/custom/series",
            Vec::new(),
            CompressionScheme::noop(),
        )
        .await
        .expect("request should be created");

        assert_eq!("/api/intake/metrics/custom/series", request.request.uri());
    }

    async fn create_v3_test_request(metrics: &[Metric]) -> V3EncodedRequest {
        let encoded = encode_v3_metrics_batch(metrics, &SharedTagSet::default()).expect("metrics should encode to V3");
        create_v3_request(V3_SERIES_ENDPOINT_URI, encoded, CompressionScheme::noop())
            .await
            .expect("request should be created")
    }

    fn test_v3_flush_context<'a>(
        ep_config: &'a EndpointConfiguration, payload_limits: V3PayloadLimits, telemetry: &'a ComponentTelemetry,
    ) -> V3FlushContext<'a> {
        V3FlushContext {
            endpoint_config: ep_config,
            payload_limits,
            series_endpoint_uri: V3_SERIES_ENDPOINT_URI,
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

        let context = test_v3_flush_context(&ep_config, limits, &telemetry);
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

        let context = test_v3_flush_context(&ep_config, limits, &telemetry);
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
        let context = test_v3_flush_context(&ep_config, limits, &telemetry);

        let ranges = split_v3_metric_ranges_by_point_limit(&metrics, context, "series")
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(vec![0..1, 1..3], ranges);
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
        let batch_id = Uuid::now_v7();
        let (mut payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let context = test_v3_flush_context(&ep_config, limits, &telemetry);
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
        let batch_id = Uuid::now_v7();
        let (mut payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let context = test_v3_flush_context(&ep_config, limits, &telemetry);
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

    #[tokio::test]
    async fn validation_split_flush_assigns_batch_id_to_carried_metric() {
        let v2_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 1, None);
        let v2_series_builder = Some(
            v2::create_v2_request_builder(MetricsEndpoint::SeriesV2, &v2_endpoint_config)
                .await
                .expect("V2 request builder should be created"),
        );
        let v3_endpoint_config = EndpointConfiguration::new(CompressionScheme::noop(), 10_000, None);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
        let (payloads_tx, mut payloads_rx) = tokio::sync::mpsc::channel(8);

        let request_builder_handle = tokio::spawn(run_request_builder(
            v2_series_builder,
            None,
            MetricsEncoderMode::Validation,
            MetricsEncoderMode::V2Only,
            v3_endpoint_config,
            V3PayloadLimits::new(usize::MAX, usize::MAX, 10_000, 10_000),
            V3_SERIES_ENDPOINT_URI.to_string(),
            telemetry,
            events_rx,
            payloads_tx,
            Duration::from_millis(10),
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
}

#[cfg(test)]
mod config_smoke {
    use serde_json::json;

    use super::DatadogMetricsConfiguration;
    use crate::config_registry::structs;
    use crate::config_registry::test_support::run_config_smoke_tests;

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_METRICS_CONFIGURATION,
            &[
                "serializer_experimental_use_v3_api.compression_level",
                "serializer_experimental_use_v3_api.series.beta_route",
                "serializer_experimental_use_v3_api.series.endpoints",
                "serializer_experimental_use_v3_api.series.use_beta",
                "serializer_experimental_use_v3_api.series.validate",
                "serializer_experimental_use_v3_api.sketches.beta_route",
                "serializer_experimental_use_v3_api.sketches.endpoints",
                "serializer_experimental_use_v3_api.sketches.use_beta",
                "serializer_experimental_use_v3_api.sketches.validate",
            ],
            json!({}),
            |cfg| {
                cfg.as_typed::<DatadogMetricsConfiguration>()
                    .expect("DatadogMetricsConfiguration should deserialize")
            },
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
