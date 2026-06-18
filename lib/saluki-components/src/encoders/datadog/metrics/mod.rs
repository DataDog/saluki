use std::{fmt, num::NonZeroU64, time::Duration};

use async_trait::async_trait;
use datadog_protos::metrics as proto;
use ddsketch::DDSketch;
use facet::Facet;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use protobuf::{rt::WireType, CodedOutputStream, Enum as _};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{iter::ReusableDeduplicator, task::HandleExt as _};
use saluki_component_config::DatadogMetricsEncoderConfig;
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
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use tokio::{pin, select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};

use crate::common::datadog::{
    clamp_payload_limits,
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT, DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT, METRICS_SERIES_V1_PATH,
    METRICS_SERIES_V2_PATH, METRICS_SKETCHES_PATH,
};

const SERIES_V2_COMPRESSED_SIZE_LIMIT: usize = 512_000; // 500 KiB
const SERIES_V2_UNCOMPRESSED_SIZE_LIMIT: usize = 5_242_880; // 5 MiB

// V1 series JSON endpoint limits match the Datadog Agent's generic serializer defaults.
const SERIES_V1_COMPRESSED_SIZE_LIMIT: usize = DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT;
const SERIES_V1_UNCOMPRESSED_SIZE_LIMIT: usize = DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT;

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";

// Protocol Buffers field numbers for series and sketch payload messages.
//
// These field numbers come from the Protocol Buffers definitions in `lib/datadog-protos/proto/agent_payload.proto`.
const RESOURCES_TYPE_FIELD_NUMBER: u32 = 1;
const RESOURCES_NAME_FIELD_NUMBER: u32 = 2;

const METADATA_ORIGIN_FIELD_NUMBER: u32 = 1;

const ORIGIN_ORIGIN_PRODUCT_FIELD_NUMBER: u32 = 4;
const ORIGIN_ORIGIN_CATEGORY_FIELD_NUMBER: u32 = 5;
const ORIGIN_ORIGIN_SERVICE_FIELD_NUMBER: u32 = 6;

const METRIC_POINT_VALUE_FIELD_NUMBER: u32 = 1;
const METRIC_POINT_TIMESTAMP_FIELD_NUMBER: u32 = 2;

const DOGSKETCH_TS_FIELD_NUMBER: u32 = 1;
const DOGSKETCH_CNT_FIELD_NUMBER: u32 = 2;
const DOGSKETCH_MIN_FIELD_NUMBER: u32 = 3;
const DOGSKETCH_MAX_FIELD_NUMBER: u32 = 4;
const DOGSKETCH_AVG_FIELD_NUMBER: u32 = 5;
const DOGSKETCH_SUM_FIELD_NUMBER: u32 = 6;
const DOGSKETCH_K_FIELD_NUMBER: u32 = 7;
const DOGSKETCH_N_FIELD_NUMBER: u32 = 8;

const SERIES_RESOURCES_FIELD_NUMBER: u32 = 1;
const SERIES_METRIC_FIELD_NUMBER: u32 = 2;
const SERIES_TAGS_FIELD_NUMBER: u32 = 3;
const SERIES_POINTS_FIELD_NUMBER: u32 = 4;
const SERIES_TYPE_FIELD_NUMBER: u32 = 5;
const SERIES_UNIT_FIELD_NUMBER: u32 = 6;
const SERIES_SOURCE_TYPE_NAME_FIELD_NUMBER: u32 = 7;
const SERIES_INTERVAL_FIELD_NUMBER: u32 = 8;
const SERIES_METADATA_FIELD_NUMBER: u32 = 9;

const SKETCH_METRIC_FIELD_NUMBER: u32 = 1;
const SKETCH_HOST_FIELD_NUMBER: u32 = 2;
const SKETCH_TAGS_FIELD_NUMBER: u32 = 4;
const SKETCH_DOGSKETCHES_FIELD_NUMBER: u32 = 7;
const SKETCH_METADATA_FIELD_NUMBER: u32 = 8;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");
static CONTENT_TYPE_JSON: HeaderValue = HeaderValue::from_static("application/json");

// JSON framing for the V1 series payload, which wraps the array of `Serie` objects in a top-level object.
const SERIES_V1_PAYLOAD_PREFIX: &[u8] = b"{\"series\":[";
const SERIES_V1_PAYLOAD_SUFFIX: &[u8] = b"]}";
const SERIES_V1_INPUT_SEPARATOR: &[u8] = b",";

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
    SERIES_V2_COMPRESSED_SIZE_LIMIT
}

const fn default_max_series_uncompressed_payload_size() -> usize {
    SERIES_V2_UNCOMPRESSED_SIZE_LIMIT
}

const fn default_max_series_points_per_payload() -> usize {
    10_000
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

const fn default_log_payloads() -> bool {
    false
}

/// Datadog Metrics encoder.
///
/// Generates Datadog metrics payloads for the Datadog platform.
#[derive(Clone, Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
#[allow(dead_code)]
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
    /// When the destination has written metrics to the in-flight request payload, but it hasn't yet reached the
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
}

impl DatadogMetricsConfiguration {
    /// Creates a metrics encoder configuration from native config.
    pub fn from_native(config: DatadogMetricsEncoderConfig) -> Self {
        Self {
            max_metrics_per_payload: config.max_metrics_per_payload,
            max_payload_size: config.max_payload_size,
            max_uncompressed_payload_size: config.max_uncompressed_payload_size,
            max_series_payload_size: config.max_series_payload_size,
            max_series_uncompressed_payload_size: config.max_series_uncompressed_payload_size,
            max_series_points_per_payload: config.max_series_points_per_payload,
            flush_timeout_secs: config.flush_timeout_secs,
            compressor_kind: config.compressor_kind,
            zstd_compressor_level: config.compression_level,
            use_v2_api_series: config.use_v2_api_series,
            log_payloads: config.log_payloads,
            additional_tags: None,
        }
    }

    /// Sets additional tags to be applied uniformly to all metrics forwarded by this destination.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        // Add the additional tags to the forwarder configuration.
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
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builders.
        let series_endpoint = if self.use_v2_api_series {
            MetricsEndpoint::SeriesV2
        } else {
            MetricsEndpoint::SeriesV1
        };
        let mut series_encoder = MetricsEndpointEncoder::from_endpoint(series_endpoint);
        let mut sketches_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);

        if let Some(additional_tags) = self.additional_tags.as_ref() {
            series_encoder = series_encoder.with_additional_tags(additional_tags.clone());
            sketches_encoder = sketches_encoder.with_additional_tags(additional_tags.clone());
        }

        let mut series_rb = RequestBuilder::new(series_encoder, compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        series_rb.with_max_inputs_per_payload(self.max_metrics_per_payload);
        series_rb.with_max_data_points_per_payload(self.max_series_points_per_payload);

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
                SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
                SERIES_V2_COMPRESSED_SIZE_LIMIT,
            )
        } else {
            generic_payload_limits
        };
        series_rb.with_len_limits(series_uncompressed_limit, series_compressed_limit)?;

        let mut sketches_rb = RequestBuilder::new(sketches_encoder, compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        sketches_rb.with_max_inputs_per_payload(self.max_metrics_per_payload);
        let (sketches_uncompressed_limit, sketches_compressed_limit) = generic_payload_limits;
        sketches_rb.with_len_limits(sketches_uncompressed_limit, sketches_compressed_limit)?;

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogMetrics {
            series_rb,
            sketches_rb,
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
    series_rb: RequestBuilder<MetricsEndpointEncoder>,
    sketches_rb: RequestBuilder<MetricsEndpointEncoder>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
    log_payloads: bool,
}

#[async_trait]
impl Encoder for DatadogMetrics {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            series_rb,
            sketches_rb,
            telemetry,
            flush_timeout,
            log_payloads,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our request builder task.
        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut = run_request_builder(
            series_rb,
            sketches_rb,
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

async fn run_request_builder(
    mut series_request_builder: RequestBuilder<MetricsEndpointEncoder>,
    mut sketches_request_builder: RequestBuilder<MetricsEndpointEncoder>, telemetry: ComponentTelemetry,
    mut events_rx: mpsc::Receiver<EventsBuffer>, payloads_tx: mpsc::Sender<PayloadsBuffer>, flush_timeout: Duration,
    log_payloads: bool,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    pin!(pending_flush_timeout);

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

                    // Series metrics (counters, gauges, rates, sets) and sketch metrics (histograms, distributions)
                    // route to their respective request builders. Whether the series builder targets the V1 or V2
                    // intake is decided once at builder time based on `use_v2_api_series`.
                    let request_builder = match metric.values() {
                        MetricValues::Counter(..)
                        | MetricValues::Rate(..)
                        | MetricValues::Gauge(..)
                        | MetricValues::Set(..) => &mut series_request_builder,
                        MetricValues::Histogram(..) | MetricValues::Distribution(..) => &mut sketches_request_builder,
                    };

                    // Encode the metric. If we get it back, that means the current request is full, and we need to
                    // flush it before we can try to encode the metric again... so we'll hold on to it in that case
                    // before flushing and trying to encode it again.
                    let metric_to_retry = match request_builder.encode(metric).await {
                        Ok(None) => continue,
                        Ok(Some(metric)) => metric,
                        Err(e) => {
                            error!(error = %e, "Failed to encode metric.");
                            telemetry.events_dropped_encoder().increment(1);
                            continue;
                        }
                    };

                    let maybe_requests = request_builder.flush().await;
                    if maybe_requests.is_empty() {
                        panic!("builder told us to flush, but gave us nothing");
                    }

                    for maybe_request in maybe_requests {
                        match maybe_request {
                            Ok((events, data_points, request)) => {
                                let payload_meta = PayloadMetadata::from_event_and_data_point_count(events, data_points);
                                let http_payload = HttpPayload::new(payload_meta, request);
                                let payload = Payload::Http(http_payload);

                                payloads_tx.send(payload).await
                                    .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
                            },

                            // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                            Err(e) => if e.is_recoverable() {
                                // If the error is recoverable, we'll hold on to the metric to retry it later.
                                continue;
                            } else {
                                return Err(GenericError::from(e).context("Failed to flush request."));
                            }
                        }
                    }

                    // Now try to encode the metric again. If it fails again, we'll just log it because it shouldn't
                    // be possible to fail at this point, otherwise we would have already caught that the first
                    // time.
                    if let Err(e) = request_builder.encode(metric_to_retry).await {
                        error!(error = %e, "Failed to encode metric.");
                        telemetry.events_dropped_encoder().increment(1);
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

                // Once we've encoded and written all metrics, we flush the request builders to generate a request with
                // anything left over. Again, we'll enqueue those requests to be sent immediately.
                let maybe_series_requests = series_request_builder.flush().await;
                for maybe_request in maybe_series_requests {
                    match maybe_request {
                        Ok((events, data_points, request)) => {
                            let payload_meta = PayloadMetadata::from_event_and_data_point_count(events, data_points);
                            let http_payload = HttpPayload::new(payload_meta, request);
                            let payload = Payload::Http(http_payload);

                            payloads_tx.send(payload).await
                                .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
                        },

                        // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                        Err(e) => if e.is_recoverable() {
                            // If the error is recoverable, we'll hold on to the metric to retry it later.
                            continue;
                        } else {
                            return Err(GenericError::from(e).context("Failed to flush request."));
                        }
                    }
                }

                let maybe_sketches_requests = sketches_request_builder.flush().await;
                for maybe_request in maybe_sketches_requests {
                    match maybe_request {
                        Ok((events, data_points, request)) => {
                            let payload_meta = PayloadMetadata::from_event_and_data_point_count(events, data_points);
                            let http_payload = HttpPayload::new(payload_meta, request);
                            let payload = Payload::Http(http_payload);

                            payloads_tx.send(payload).await
                                .map_err(|_| generic_error!("Failed to send payload to encoder."))?;
                        },

                        // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                        Err(e) => if e.is_recoverable() {
                            // If the error is recoverable, we'll hold on to the metric to retry it later.
                            continue;
                        } else {
                            return Err(GenericError::from(e).context("Failed to flush request."));
                        }
                    }
                }

                debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
            },

            // Event buffers channel has been closed, and we have no pending flushing, so we're all done.
            else => break,
        }
    }

    Ok(())
}

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

/// Metrics intake endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetricsEndpoint {
    /// V1 series metrics, encoded as JSON and sent to `/api/v1/series`.
    ///
    /// Includes counters, gauges, rates, and sets. Selected when `use_v2_api.series` is `false`.
    SeriesV1,

    /// V2 series metrics, encoded as Protocol Buffers and sent to `/api/v2/series`.
    ///
    /// Includes counters, gauges, rates, and sets. The default series encoding.
    SeriesV2,

    /// Sketch metrics, encoded as Protocol Buffers and sent to `/api/beta/sketches`.
    ///
    /// Includes histograms and distributions. Always uses the V2 endpoint regardless of `use_v2_api.series`.
    Sketches,
}

/// Error returned when a metric fails to encode for either the V1 JSON or V2 protobuf intake.
#[derive(Debug)]
pub enum MetricsEncodeError {
    /// Protobuf encoding failed.
    Protobuf(protobuf::Error),

    /// JSON encoding failed.
    Json(serde_json::Error),
}

impl fmt::Display for MetricsEncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protobuf(e) => write!(f, "protobuf encode error: {}", e),
            Self::Json(e) => write!(f, "json encode error: {}", e),
        }
    }
}

impl std::error::Error for MetricsEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protobuf(e) => Some(e),
            Self::Json(e) => Some(e),
        }
    }
}

impl From<protobuf::Error> for MetricsEncodeError {
    fn from(value: protobuf::Error) -> Self {
        Self::Protobuf(value)
    }
}

impl From<serde_json::Error> for MetricsEncodeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug)]
struct MetricsEndpointEncoder {
    endpoint: MetricsEndpoint,
    primary_scratch_buf: Vec<u8>,
    secondary_scratch_buf: Vec<u8>,
    packed_scratch_buf: Vec<u8>,
    additional_tags: SharedTagSet,
    tags_deduplicator: ReusableDeduplicator<Tag>,
}

impl MetricsEndpointEncoder {
    /// Creates a new `MetricsEndpointEncoder` for the given endpoint.
    pub fn from_endpoint(endpoint: MetricsEndpoint) -> Self {
        Self {
            endpoint,
            primary_scratch_buf: Vec::new(),
            secondary_scratch_buf: Vec::new(),
            packed_scratch_buf: Vec::new(),
            additional_tags: SharedTagSet::default(),
            tags_deduplicator: ReusableDeduplicator::new(),
        }
    }

    /// Sets the additional tags to be included with every metric encoded by this encoder.
    ///
    /// These tags are added in a deduplicated fashion, the same as instrumented tags and origin tags. This is an
    /// optimized codepath for tag inclusion in high-volume scenarios, where creating new additional contexts
    /// through the traditional means (for example, `ContextResolver`) would be too expensive.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        self.additional_tags = additional_tags;
        self
    }
}

impl EndpointEncoder for MetricsEndpointEncoder {
    type Input = Metric;
    type EncodeError = MetricsEncodeError;

    fn encoder_name() -> &'static str {
        "metrics"
    }

    fn compressed_size_limit(&self) -> usize {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => SERIES_V1_COMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::SeriesV2 => SERIES_V2_COMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::Sketches => DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT,
        }
    }

    fn uncompressed_size_limit(&self) -> usize {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => SERIES_V1_UNCOMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::SeriesV2 => SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::Sketches => DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
        }
    }

    fn input_data_point_count(&self, input: &Self::Input) -> usize {
        input.values().len()
    }

    fn is_valid_input(&self, input: &Self::Input) -> bool {
        let is_series_input = matches!(
            input.values(),
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..)
        );

        match self.endpoint {
            MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => is_series_input,
            MetricsEndpoint::Sketches => !is_series_input,
        }
    }

    fn get_payload_prefix(&self) -> Option<&'static [u8]> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => Some(SERIES_V1_PAYLOAD_PREFIX),
            _ => None,
        }
    }

    fn get_payload_suffix(&self) -> Option<&'static [u8]> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => Some(SERIES_V1_PAYLOAD_SUFFIX),
            _ => None,
        }
    }

    fn get_input_separator(&self) -> Option<&'static [u8]> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => Some(SERIES_V1_INPUT_SEPARATOR),
            _ => None,
        }
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => {
                encode_series_v1_metric(input, &self.additional_tags, buffer, &mut self.tags_deduplicator)?;
                Ok(())
            }
            MetricsEndpoint::SeriesV2 | MetricsEndpoint::Sketches => {
                // NOTE: We're passing _four_ buffers to `encode_single_metric`, which is a lot, but with good reason.
                //
                // The first buffer, `buffer`, is the overall output buffer: the caller expects us to put the full
                // encoded metric payload into this buffer.
                //
                // The second and third buffers, `primary_scratch_buf` and `secondary_scratch_buf`, are used for
                // roughly the same thing but deal with _nesting_. When writing a "message" in Protocol Buffers, the
                // message data itself is prefixed with the field number and a length delimiter that specifies how
                // long the message is. We can't write that length delimiter until we know the full size of the
                // message, so we write the message to a scratch buffer, calculate its size, and then write the field
                // number and length delimiter to the output buffer followed by the message data from the scratch
                // buffer.
                //
                // We have _two_ scratch buffers because you need a dedicated buffer for each level of nested message.
                // We have to be able to nest up to two levels deep in our metrics payload, so we need two scratch
                // buffers to handle that.
                //
                // The fourth buffer, `packed_scratch_buf`, is used for writing out packed repeated fields. This is
                // similar to the situation describe above, except it's not _exactly_ the same as an additional level
                // of nesting.. so I just decided to give it a somewhat more descriptive name.
                encode_single_metric(
                    input,
                    &self.additional_tags,
                    buffer,
                    &mut self.primary_scratch_buf,
                    &mut self.secondary_scratch_buf,
                    &mut self.packed_scratch_buf,
                    &mut self.tags_deduplicator,
                )?;
                Ok(())
            }
        }
    }

    fn endpoint_uri(&self) -> Uri {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => PathAndQuery::from_static(METRICS_SERIES_V1_PATH).into(),
            MetricsEndpoint::SeriesV2 => PathAndQuery::from_static(METRICS_SERIES_V2_PATH).into(),
            MetricsEndpoint::Sketches => PathAndQuery::from_static(METRICS_SKETCHES_PATH).into(),
        }
    }

    fn endpoint_method(&self) -> Method {
        // All endpoints use POST.
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => CONTENT_TYPE_JSON.clone(),
            MetricsEndpoint::SeriesV2 | MetricsEndpoint::Sketches => CONTENT_TYPE_PROTOBUF.clone(),
        }
    }
}

fn field_number_for_metric_type(metric: &Metric) -> u32 {
    match metric.values() {
        MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => 1,
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => 1,
    }
}

fn get_message_size(raw_msg_size: usize) -> Result<u32, protobuf::Error> {
    const MAX_MESSAGE_SIZE: u64 = i32::MAX as u64;

    // Individual messages cannot be larger than `i32::MAX`, so check that here before proceeding.
    if raw_msg_size as u64 > MAX_MESSAGE_SIZE {
        return Err(std::io::Error::other("message size exceeds limit (2147483648 bytes)").into());
    }

    Ok(raw_msg_size as u32)
}

fn get_message_size_from_buffer(buf: &[u8]) -> Result<u32, protobuf::Error> {
    get_message_size(buf.len())
}

fn encode_single_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_buf: &mut Vec<u8>, primary_scratch_buf: &mut Vec<u8>,
    secondary_scratch_buf: &mut Vec<u8>, packed_scratch_buf: &mut Vec<u8>,
    tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), protobuf::Error> {
    let mut output_stream = CodedOutputStream::vec(output_buf);
    let field_number = field_number_for_metric_type(metric);

    write_nested_message(&mut output_stream, primary_scratch_buf, field_number, |os| {
        // Depending on the metric type, we write out the appropriate fields.
        match metric.values() {
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
                encode_series_v2_metric(metric, additional_tags, os, secondary_scratch_buf, tags_deduplicator)
            }
            MetricValues::Histogram(..) | MetricValues::Distribution(..) => encode_sketch_metric(
                metric,
                additional_tags,
                os,
                secondary_scratch_buf,
                packed_scratch_buf,
                tags_deduplicator,
            ),
        }
    })
}

fn encode_series_v2_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_stream: &mut CodedOutputStream<'_>,
    scratch_buf: &mut Vec<u8>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), protobuf::Error> {
    // Write the metric name and tags.
    output_stream.write_string(SERIES_METRIC_FIELD_NUMBER, metric.context().name())?;

    let deduplicated_tags = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
    write_series_tags(deduplicated_tags, output_stream, scratch_buf)?;

    // Set the host resource.
    write_resource(
        output_stream,
        scratch_buf,
        "host",
        metric.metadata().hostname().unwrap_or_default(),
    )?;

    // Write the origin metadata, if it exists.
    if let Some(origin) = metric.metadata().origin() {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                output_stream.write_string(SERIES_SOURCE_TYPE_NAME_FIELD_NUMBER, source_type.as_ref())?;
            }
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => {
                write_origin_metadata(
                    output_stream,
                    scratch_buf,
                    SERIES_METADATA_FIELD_NUMBER,
                    *product,
                    *subproduct,
                    *product_detail,
                )?;
            }
        }
    }

    // Now write out our metric type, points, and interval (if applicable).
    let (metric_type, points, maybe_interval) = match metric.values() {
        MetricValues::Counter(points) => (proto::MetricType::COUNT, points.into_iter(), None),
        MetricValues::Rate(points, interval) => (proto::MetricType::RATE, points.into_iter(), Some(interval)),
        MetricValues::Gauge(points) => (proto::MetricType::GAUGE, points.into_iter(), None),
        MetricValues::Set(points) => (proto::MetricType::GAUGE, points.into_iter(), None),
        _ => unreachable!("encode_series_v2_metric called with non-series metric"),
    };

    output_stream.write_enum(SERIES_TYPE_FIELD_NUMBER, metric_type.value())?;

    if let Some(unit) = metric.metadata().unit() {
        output_stream.write_string(SERIES_UNIT_FIELD_NUMBER, unit)?;
    }

    for (timestamp, value) in points {
        // If this is a rate metric, scale our value by the interval, in seconds.
        let value = maybe_interval
            .map(|interval| value / interval.as_secs_f64())
            .unwrap_or(value);
        let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

        write_point(output_stream, scratch_buf, value, timestamp)?;
    }

    if let Some(interval) = maybe_interval {
        output_stream.write_int64(SERIES_INTERVAL_FIELD_NUMBER, interval.as_secs() as i64)?;
    }

    Ok(())
}

fn encode_series_v1_metric(
    metric: &Metric, additional_tags: &SharedTagSet, buffer: &mut Vec<u8>,
    tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), serde_json::Error> {
    let mut obj = JsonMap::new();

    obj.insert("metric".into(), JsonValue::String(metric.context().name().to_string()));

    let (type_str, points_iter, maybe_interval) = match metric.values() {
        MetricValues::Counter(points) => ("count", points.into_iter(), None),
        MetricValues::Rate(points, interval) => ("rate", points.into_iter(), Some(*interval)),
        MetricValues::Gauge(points) => ("gauge", points.into_iter(), None),
        MetricValues::Set(points) => ("gauge", points.into_iter(), None),
        _ => unreachable!("encode_series_v1_metric called with non-series metric"),
    };

    let mut points = Vec::new();
    for (timestamp, value) in points_iter {
        // For rates, value is scaled by interval seconds — same as the V2 encoder.
        let value = maybe_interval
            .map(|interval| value / interval.as_secs_f64())
            .unwrap_or(value);
        let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

        // V1 emits each point as a [timestamp, value] tuple — not a nested object.
        let value_json = JsonNumber::from_f64(value)
            .map(JsonValue::Number)
            .unwrap_or_else(|| JsonValue::from(0));
        points.push(JsonValue::Array(vec![JsonValue::from(timestamp), value_json]));
    }
    obj.insert("points".into(), JsonValue::Array(points));

    // Walk the deduplicated tag set once, extracting the first `device:<value>` tag into the device JSON field while
    // dropping `dd.internal.resource` (which is a V2-protobuf-only concept with no V1 representation).
    let deduplicated = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
    let mut tags_out = Vec::new();
    let mut device: Option<String> = None;
    for tag in deduplicated {
        if tag.name() == "dd.internal.resource" {
            continue;
        }
        if device.is_none() && tag.name() == "device" {
            if let Some(v) = tag.value() {
                device = Some(v.to_string());
                continue;
            }
        }
        tags_out.push(JsonValue::String(tag.as_str().to_string()));
    }
    obj.insert("tags".into(), JsonValue::Array(tags_out));

    // V1 always emits `host` and `interval`, even when empty/zero — matches the Agent encoder.
    obj.insert(
        "host".into(),
        JsonValue::String(metric.metadata().hostname().unwrap_or_default().to_string()),
    );

    if let Some(d) = device.filter(|s| !s.is_empty()) {
        obj.insert("device".into(), JsonValue::String(d));
    }

    obj.insert("type".into(), JsonValue::String(type_str.into()));

    let interval_secs = maybe_interval.map(|iv| iv.as_secs() as i64).unwrap_or(0);
    obj.insert("interval".into(), JsonValue::from(interval_secs));

    // V1 only emits `source_type_name` from `MetricOrigin::SourceType`.
    if let Some(MetricOrigin::SourceType(s)) = metric.metadata().origin() {
        obj.insert("source_type_name".into(), JsonValue::String(s.as_ref().to_string()));
    }

    if let Some(unit) = metric.metadata().unit() {
        if !unit.is_empty() {
            obj.insert("unit".into(), JsonValue::String(unit.to_string()));
        }
    }

    serde_json::to_writer(buffer, &JsonValue::Object(obj))
}

fn encode_sketch_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_stream: &mut CodedOutputStream<'_>,
    scratch_buf: &mut Vec<u8>, packed_scratch_buf: &mut Vec<u8>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), protobuf::Error> {
    // Write the metric name and tags.
    output_stream.write_string(SKETCH_METRIC_FIELD_NUMBER, metric.context().name())?;

    let deduplicated_tags = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
    write_sketch_tags(deduplicated_tags, output_stream, scratch_buf)?;

    // Write the host.
    output_stream.write_string(
        SKETCH_HOST_FIELD_NUMBER,
        metric.metadata().hostname().unwrap_or_default(),
    )?;

    // Set the origin metadata, if it exists.
    if let Some(MetricOrigin::OriginMetadata {
        product,
        subproduct,
        product_detail,
    }) = metric.metadata().origin()
    {
        write_origin_metadata(
            output_stream,
            scratch_buf,
            SKETCH_METADATA_FIELD_NUMBER,
            *product,
            *subproduct,
            *product_detail,
        )?;
    }

    // TODO: emit `metric.metadata().unit()` in the sketch payload once the upstream `agent-payload` proto defines a
    // unit field on `SketchPayload.Sketch`.

    // Write out our sketches.
    match metric.values() {
        MetricValues::Distribution(sketches) => {
            for (timestamp, value) in sketches {
                write_dogsketch(output_stream, scratch_buf, packed_scratch_buf, timestamp, value)?;
            }
        }
        MetricValues::Histogram(points) => {
            for (timestamp, histogram) in points {
                // We convert histograms to sketches to be able to write them out in the payload.
                let mut ddsketch = DDSketch::default();
                for sample in histogram.samples() {
                    ddsketch.insert_n(sample.value.into_inner(), sample.weight.0 as u64);
                }

                write_dogsketch(output_stream, scratch_buf, packed_scratch_buf, timestamp, &ddsketch)?;
            }
        }
        _ => unreachable!("encode_sketch_metric called with non-sketch metric"),
    }

    Ok(())
}

fn write_resource(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, resource_type: &str, resource_name: &str,
) -> Result<(), protobuf::Error> {
    write_nested_message(output_stream, scratch_buf, SERIES_RESOURCES_FIELD_NUMBER, |os| {
        os.write_string(RESOURCES_TYPE_FIELD_NUMBER, resource_type)?;
        os.write_string(RESOURCES_NAME_FIELD_NUMBER, resource_name)
    })
}

fn write_origin_metadata(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, field_number: u32, origin_product: u32,
    origin_category: u32, origin_service: u32,
) -> Result<(), protobuf::Error> {
    // TODO: Figure out how to cleanly use `write_nested_message` here.

    scratch_buf.clear();

    {
        let mut origin_output_stream = CodedOutputStream::vec(scratch_buf);
        origin_output_stream.write_uint32(ORIGIN_ORIGIN_PRODUCT_FIELD_NUMBER, origin_product)?;
        origin_output_stream.write_uint32(ORIGIN_ORIGIN_CATEGORY_FIELD_NUMBER, origin_category)?;
        origin_output_stream.write_uint32(ORIGIN_ORIGIN_SERVICE_FIELD_NUMBER, origin_service)?;
        origin_output_stream.flush()?;
    }

    // We do a little song and dance here because the `Origin` message is embedded inside of `Metadata`, so we need to
    // write out field numbers/length delimiters in order: `Metadata`, and then `Origin`... but we write out origin
    // message to the scratch buffer first... so we write out our `Metadata` preamble stuff to get its length, and then
    // use that in conjunction with the `Origin` message size to write out the full `Metadata` message.
    let origin_message_size = get_message_size_from_buffer(scratch_buf)?;

    let mut metadata_preamble_buf = [0; 64];
    let metadata_preamble_len = {
        let mut metadata_output_stream = CodedOutputStream::bytes(&mut metadata_preamble_buf[..]);
        metadata_output_stream.write_tag(METADATA_ORIGIN_FIELD_NUMBER, WireType::LengthDelimited)?;
        metadata_output_stream.write_raw_varint32(origin_message_size)?;
        metadata_output_stream.flush()?;
        metadata_output_stream.total_bytes_written() as usize
    };

    let metadata_message_size = get_message_size(scratch_buf.len() + metadata_preamble_len)?;

    output_stream.write_tag(field_number, WireType::LengthDelimited)?;
    output_stream.write_raw_varint32(metadata_message_size)?;
    output_stream.write_raw_bytes(&metadata_preamble_buf[..metadata_preamble_len])?;
    output_stream.write_raw_bytes(scratch_buf)
}

fn write_point(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, value: f64, timestamp: i64,
) -> Result<(), protobuf::Error> {
    write_nested_message(output_stream, scratch_buf, SERIES_POINTS_FIELD_NUMBER, |os| {
        os.write_double(METRIC_POINT_VALUE_FIELD_NUMBER, value)?;
        os.write_int64(METRIC_POINT_TIMESTAMP_FIELD_NUMBER, timestamp)
    })
}

fn write_dogsketch(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, packed_scratch_buf: &mut Vec<u8>,
    timestamp: Option<NonZeroU64>, sketch: &DDSketch,
) -> Result<(), protobuf::Error> {
    // If the sketch is empty, we don't write it out.
    if sketch.is_empty() {
        warn!("Attempted to write an empty sketch to sketches payload, skipping.");
        return Ok(());
    }

    write_nested_message(output_stream, scratch_buf, SKETCH_DOGSKETCHES_FIELD_NUMBER, |os| {
        os.write_int64(DOGSKETCH_TS_FIELD_NUMBER, timestamp.map_or(0, |ts| ts.get() as i64))?;
        os.write_int64(DOGSKETCH_CNT_FIELD_NUMBER, sketch.count() as i64)?;
        os.write_double(DOGSKETCH_MIN_FIELD_NUMBER, sketch.min().unwrap())?;
        os.write_double(DOGSKETCH_MAX_FIELD_NUMBER, sketch.max().unwrap())?;
        os.write_double(DOGSKETCH_AVG_FIELD_NUMBER, sketch.avg().unwrap())?;
        os.write_double(DOGSKETCH_SUM_FIELD_NUMBER, sketch.sum().unwrap())?;

        let bin_keys = sketch.bins().iter().map(|bin| bin.key());
        write_repeated_packed_from_iter(
            os,
            packed_scratch_buf,
            DOGSKETCH_K_FIELD_NUMBER,
            bin_keys,
            |inner_os, value| inner_os.write_sint32_no_tag(value),
        )?;

        let bin_counts = sketch.bins().iter().map(|bin| bin.count());
        write_repeated_packed_from_iter(
            os,
            packed_scratch_buf,
            DOGSKETCH_N_FIELD_NUMBER,
            bin_counts,
            |inner_os, value| inner_os.write_uint32_no_tag(value),
        )
    })
}

fn get_deduplicated_tags<'a>(
    metric: &'a Metric, additional_tags: &'a SharedTagSet, tags_deduplicator: &'a mut ReusableDeduplicator<Tag>,
) -> impl Iterator<Item = &'a Tag> {
    let chained_tags = metric
        .context()
        .tags()
        .into_iter()
        .chain(additional_tags)
        .chain(metric.context().origin_tags());

    tags_deduplicator.deduplicated(chained_tags)
}

fn write_tags<'a, I, F>(
    tags: I, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, tag_encoder: F,
) -> Result<(), protobuf::Error>
where
    I: Iterator<Item = &'a Tag>,
    F: Fn(&Tag, &mut CodedOutputStream<'_>, &mut Vec<u8>) -> Result<(), protobuf::Error>,
{
    for tag in tags {
        tag_encoder(tag, output_stream, scratch_buf)?;
    }

    Ok(())
}

fn write_series_tags<'a, I>(
    tags: I, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error>
where
    I: Iterator<Item = &'a Tag>,
{
    write_tags(tags, output_stream, scratch_buf, |tag, os, buf| {
        // If this is a resource tag, we'll convert it directly to a resource entry.
        if tag.name() == "dd.internal.resource" {
            if let Some((resource_type, resource_name)) = tag.value().and_then(|s| s.split_once(':')) {
                write_resource(os, buf, resource_type, resource_name)
            } else {
                Ok(())
            }
        } else {
            // We're dealing with a normal tag.
            os.write_string(SERIES_TAGS_FIELD_NUMBER, tag.as_str())
        }
    })
}

fn write_sketch_tags<'a, I>(
    tags: I, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error>
where
    I: Iterator<Item = &'a Tag>,
{
    write_tags(tags, output_stream, scratch_buf, |tag, os, _buf| {
        // We always write the tags as-is, without any special handling for resource tags.
        os.write_string(SKETCH_TAGS_FIELD_NUMBER, tag.as_str())
    })
}

fn write_nested_message<F>(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, field_number: u32, writer: F,
) -> Result<(), protobuf::Error>
where
    F: FnOnce(&mut CodedOutputStream<'_>) -> Result<(), protobuf::Error>,
{
    scratch_buf.clear();

    {
        let mut nested_output_stream = CodedOutputStream::vec(scratch_buf);
        writer(&mut nested_output_stream)?;
        nested_output_stream.flush()?;
    }

    output_stream.write_tag(field_number, WireType::LengthDelimited)?;

    let nested_message_size = get_message_size_from_buffer(scratch_buf)?;
    output_stream.write_raw_varint32(nested_message_size)?;
    output_stream.write_raw_bytes(scratch_buf)
}

fn write_repeated_packed_from_iter<I, T, F>(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, field_number: u32, values: I, writer: F,
) -> Result<(), protobuf::Error>
where
    I: Iterator<Item = T>,
    F: Fn(&mut CodedOutputStream<'_>, T) -> Result<(), protobuf::Error>,
{
    // This is a helper function that lets us write out a packed repeated field from an iterator of values.
    // `CodedOutputStream` has similar functions to handle this, but they require a slice of values, which would mean we
    // need to either allocate a new vector each time to hold the values, or thread through two additional vectors (one
    // for `i32`, one for `u32`) to reuse the allocation... both of which are not great options.
    //
    // We've simply opted to pass through a _single_ vector that we can reuse, and write the packed values directly to
    // that, almost identically to how `CodedOutputStream::write_repeated_packed_*` methods would do it.

    scratch_buf.clear();

    {
        let mut packed_output_stream = CodedOutputStream::vec(scratch_buf);
        for value in values {
            writer(&mut packed_output_stream, value)?;
        }
        packed_output_stream.flush()?;
    }

    let data_size = get_message_size_from_buffer(scratch_buf)?;

    output_stream.write_tag(field_number, WireType::LengthDelimited)?;
    output_stream.write_raw_varint32(data_size)?;
    output_stream.write_raw_bytes(scratch_buf)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use protobuf::CodedOutputStream;
    use saluki_common::iter::ReusableDeduplicator;
    use saluki_context::{tags::SharedTagSet, Context};
    use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricOrigin, MetricValues};
    use serde_json::Value as JsonValue;
    use stringtheory::MetaString;

    use super::{
        encode_series_v1_metric, encode_series_v2_metric, encode_sketch_metric, MetricsEndpoint,
        MetricsEndpointEncoder, SERIES_V1_INPUT_SEPARATOR, SERIES_V1_PAYLOAD_PREFIX, SERIES_V1_PAYLOAD_SUFFIX,
    };
    use crate::common::datadog::{
        request_builder::EndpointEncoder as _, DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT,
        DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT,
    };

    fn encode_one_v1(metric: &Metric) -> JsonValue {
        let mut buf = Vec::new();
        let host_tags = SharedTagSet::default();
        let mut tags_deduplicator = ReusableDeduplicator::new();
        encode_series_v1_metric(metric, &host_tags, &mut buf, &mut tags_deduplicator)
            .expect("encode_series_v1_metric should succeed");
        serde_json::from_slice(&buf).expect("encoder produced invalid JSON")
    }

    #[test]
    fn histogram_vs_sketch_identical_payload() {
        // For the same exact set of points, we should be able to construct either a histogram or distribution from
        // those points, and when encoded as a sketch payload, end up with the same exact payload.
        //
        // They should be identical because the goal is that we convert histograms into sketches in the same way we
        // would have originally constructed a sketch based on the same samples.
        let samples = &[1.0, 2.0, 3.0, 4.0, 5.0];
        let histogram = Metric::histogram("simple_samples", samples);
        let distribution = Metric::distribution("simple_samples", samples);
        let host_tags = SharedTagSet::default();

        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        let mut tags_deduplicator = ReusableDeduplicator::new();

        let mut histogram_payload = Vec::new();
        {
            let mut histogram_writer = CodedOutputStream::vec(&mut histogram_payload);
            encode_sketch_metric(
                &histogram,
                &host_tags,
                &mut histogram_writer,
                &mut buf1,
                &mut buf2,
                &mut tags_deduplicator,
            )
            .expect("Failed to encode histogram as sketch");
        }

        let mut distribution_payload = Vec::new();
        {
            let mut distribution_writer = CodedOutputStream::vec(&mut distribution_payload);
            encode_sketch_metric(
                &distribution,
                &host_tags,
                &mut distribution_writer,
                &mut buf1,
                &mut buf2,
                &mut tags_deduplicator,
            )
            .expect("Failed to encode distribution as sketch");
        }

        assert_eq!(histogram_payload, distribution_payload);
    }

    #[test]
    fn input_valid() {
        // Our encoder should always consider series metrics valid when set to either series endpoint, and similarly
        // for sketch metrics when set to the sketches endpoint.
        let counter = Metric::counter("counter", 1.0);
        let rate = Metric::rate("rate", 1.0, Duration::from_secs(1));
        let gauge = Metric::gauge("gauge", 1.0);
        let set = Metric::set("set", "foo");
        let histogram = Metric::histogram("histogram", [1.0, 2.0, 3.0]);
        let distribution = Metric::distribution("distribution", [1.0, 2.0, 3.0]);

        let series_v1 = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV1);
        let series_v2 = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV2);
        let sketches_endpoint = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);

        for series_endpoint in [&series_v1, &series_v2] {
            assert!(series_endpoint.is_valid_input(&counter));
            assert!(series_endpoint.is_valid_input(&rate));
            assert!(series_endpoint.is_valid_input(&gauge));
            assert!(series_endpoint.is_valid_input(&set));
            assert!(!series_endpoint.is_valid_input(&histogram));
            assert!(!series_endpoint.is_valid_input(&distribution));
        }

        assert!(!sketches_endpoint.is_valid_input(&counter));
        assert!(!sketches_endpoint.is_valid_input(&rate));
        assert!(!sketches_endpoint.is_valid_input(&gauge));
        assert!(!sketches_endpoint.is_valid_input(&set));
        assert!(sketches_endpoint.is_valid_input(&histogram));
        assert!(sketches_endpoint.is_valid_input(&distribution));
    }

    #[test]
    fn input_data_point_count_tracks_metric_values() {
        let counter = Metric::counter("counter", [(123, 1.0), (124, 2.0)]);
        let histogram = Metric::histogram("histogram", [1.0, 2.0, 3.0]);

        let series_endpoint = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV2);
        let sketches_endpoint = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);

        assert_eq!(series_endpoint.input_data_point_count(&counter), 2);
        assert_eq!(sketches_endpoint.input_data_point_count(&histogram), 1);
    }

    #[test]
    fn series_metric_unit_encoded() {
        // A gauge with a unit in its metadata must produce a series protobuf payload that contains the unit string
        // in field 6 (MetricSeries.unit), which the Datadog backend already accepts.
        //
        // In production this state is reached when histogram aggregation flushes timer (`ms`) statistics as gauges,
        // each carrying unit = "millisecond" propagated through MetricMetadata.
        let context = Context::from_static_parts("my.timer.avg", &[]);
        let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
        let gauge = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);

        let host_tags = SharedTagSet::default();
        let mut scratch_buf = Vec::new();
        let mut tags_deduplicator = ReusableDeduplicator::new();

        let mut payload = Vec::new();
        {
            let mut writer = CodedOutputStream::vec(&mut payload);
            encode_series_v2_metric(
                &gauge,
                &host_tags,
                &mut writer,
                &mut scratch_buf,
                &mut tags_deduplicator,
            )
            .expect("Failed to encode gauge as series metric");
            writer.flush().expect("Failed to flush");
        }

        // In the protobuf wire format, a string field with field number 6 has tag byte 0x32 ((6 << 3) | 2).
        // The tag is followed by a varint length and then the UTF-8 bytes of the string.
        let expected_tag: u8 = (6 << 3) | 2; // 0x32
        let expected_value = b"millisecond";

        let tag_pos = payload
            .windows(1 + 1 + expected_value.len())
            .position(|w| w[0] == expected_tag && w[1] == expected_value.len() as u8 && &w[2..] == expected_value);

        assert!(
            tag_pos.is_some(),
            "series payload should contain unit field (field 6 = 'millisecond'), got bytes: {:?}",
            payload
        );
    }

    #[test]
    fn series_v1_basic_payload_shape() {
        // Each metric variant maps to the right `type` string, points are emitted as [ts, value] tuples,
        // and `interval`/`host` are always present (zero/empty when not set).
        let counter = Metric::counter("my.count", 5.0);
        let counter_json = encode_one_v1(&counter);
        assert_eq!(counter_json["metric"], "my.count");
        assert_eq!(counter_json["type"], "count");
        assert_eq!(counter_json["interval"], 0);
        assert_eq!(counter_json["host"], "");
        assert_eq!(counter_json["tags"], JsonValue::Array(vec![]));
        let points = counter_json["points"].as_array().expect("points is array");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0][0], 0);
        assert_eq!(points[0][1], 5.0);
        // Optional fields must be absent when not set.
        assert!(counter_json.get("unit").is_none());
        assert!(counter_json.get("source_type_name").is_none());
        assert!(counter_json.get("device").is_none());

        let rate = Metric::rate("my.rate", 30.0, Duration::from_secs(10));
        let rate_json = encode_one_v1(&rate);
        assert_eq!(rate_json["type"], "rate");
        assert_eq!(rate_json["interval"], 10);
        // Rate value scaled by interval seconds: 30 / 10 = 3.
        let rate_points = rate_json["points"].as_array().expect("rate points is array");
        assert_eq!(rate_points[0][1], 3.0);

        let gauge = Metric::gauge("my.gauge", 42.0);
        let gauge_json = encode_one_v1(&gauge);
        assert_eq!(gauge_json["type"], "gauge");

        // Sets are encoded as gauges with the set cardinality as the value (consistent with V2).
        let set = Metric::set("my.set", "alpha");
        let set_json = encode_one_v1(&set);
        assert_eq!(set_json["type"], "gauge");
        let set_points = set_json["points"].as_array().expect("set points is array");
        assert_eq!(set_points[0][1], 1.0);
    }

    #[test]
    fn series_v1_unit_and_hostname_emitted() {
        let context = Context::from_static_parts("my.timer.avg", &[]);
        let metadata = MetricMetadata::default()
            .with_unit(MetaString::from_static("millisecond"))
            .with_hostname(Some(Arc::from("host-1")));
        let gauge = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);

        let json = encode_one_v1(&gauge);
        assert_eq!(json["unit"], "millisecond");
        assert_eq!(json["host"], "host-1");
    }

    #[test]
    fn series_v1_device_tag_extraction() {
        // A `device:<value>` tag is extracted into the `device` JSON field and dropped from `tags`.
        let context = Context::from_static_parts("my.metric", &["device:eth0", "env:prod"]);
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), MetricMetadata::default());

        let json = encode_one_v1(&counter);
        assert_eq!(json["device"], "eth0");
        let tags = json["tags"].as_array().expect("tags is array");
        let tag_strs: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !tag_strs.iter().any(|t| t.starts_with("device:")),
            "device tag must be removed: {:?}",
            tag_strs
        );
        assert!(tag_strs.contains(&"env:prod"));
    }

    #[test]
    fn series_v1_source_type_name_from_source_type_origin() {
        let context = Context::from_static_parts("my.metric", &[]);
        let metadata = MetricMetadata::default().with_source_type(Some(Arc::from("integration_x")));
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), metadata);

        let json = encode_one_v1(&counter);
        assert_eq!(json["source_type_name"], "integration_x");
    }

    #[test]
    fn series_v1_origin_metadata_dropped() {
        // OriginMetadata is V2-protobuf only; V1 must drop it.
        let context = Context::from_static_parts("my.metric", &[]);
        let metadata = MetricMetadata::default().with_origin(Some(MetricOrigin::dogstatsd()));
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), metadata);

        let json = encode_one_v1(&counter);
        assert!(json.get("source_type_name").is_none());
    }

    #[test]
    fn series_v1_dd_internal_resource_dropped() {
        // `dd.internal.resource` is V2-protobuf-only; V1 must drop these tags silently.
        let context = Context::from_static_parts("my.metric", &["dd.internal.resource:host:foo", "env:prod"]);
        let counter = Metric::from_parts(context, MetricValues::counter([1.0_f64]), MetricMetadata::default());

        let json = encode_one_v1(&counter);
        let tags = json["tags"].as_array().expect("tags is array");
        let tag_strs: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !tag_strs.iter().any(|t| t.starts_with("dd.internal.resource:")),
            "dd.internal.resource tag must be dropped: {:?}",
            tag_strs
        );
        assert!(tag_strs.contains(&"env:prod"));
    }

    #[test]
    fn series_v1_endpoint_routing() {
        // SeriesV1 advertises the V1 URI, JSON content type, and the {"series":[...]} framing.
        let encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV1);
        assert_eq!(encoder.endpoint_uri().path(), "/api/v1/series");
        assert_eq!(encoder.content_type(), "application/json");
        assert_eq!(encoder.get_payload_prefix(), Some(SERIES_V1_PAYLOAD_PREFIX));
        assert_eq!(encoder.get_payload_suffix(), Some(SERIES_V1_PAYLOAD_SUFFIX));
        assert_eq!(encoder.get_input_separator(), Some(SERIES_V1_INPUT_SEPARATOR));
        assert_eq!(
            encoder.compressed_size_limit(),
            DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT
        );
        assert_eq!(
            encoder.uncompressed_size_limit(),
            DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT
        );

        // Sketches use the generic serializer payload limits in the Datadog Agent.
        let sketches = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);
        assert_eq!(
            sketches.compressed_size_limit(),
            DEFAULT_SERIALIZER_COMPRESSED_SIZE_LIMIT
        );
        assert_eq!(
            sketches.uncompressed_size_limit(),
            DEFAULT_SERIALIZER_UNCOMPRESSED_SIZE_LIMIT
        );

        // V2 series stays on protobuf with no framing.
        let v2 = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV2);
        assert_eq!(v2.endpoint_uri().path(), "/api/v2/series");
        assert_eq!(v2.content_type(), "application/x-protobuf");
        assert!(v2.get_payload_prefix().is_none());
    }
}

#[cfg(test)]
mod config_smoke {
    use datadog_agent_config::{DatadogRemapper, KEY_ALIASES};
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::DatadogMetricsConfiguration;

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::DATADOG_METRICS_CONFIGURATION,
            &[],
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
    use datadog_agent_config::KEY_ALIASES;
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;

    use super::{DatadogMetricsConfiguration, SERIES_V2_COMPRESSED_SIZE_LIMIT, SERIES_V2_UNCOMPRESSED_SIZE_LIMIT};
    use crate::common::datadog::clamp_payload_limits;

    /// `use_v2_api_series` defaults to `true` (preserves V2 protobuf behavior when the flag is absent).
    /// The nested-form (`use_v2_api.series`) and env-var (`DD_USE_V2_API_SERIES`) paths to the flat key
    /// are exercised end-to-end by the `config_smoke::smoke_test` runner via `KEY_ALIASES`.
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
    }

    #[test]
    fn clamps_series_payload_limit_keys_to_api_limits() {
        let (uncompressed_limit, compressed_limit) = clamp_payload_limits(
            SERIES_V2_UNCOMPRESSED_SIZE_LIMIT + 1,
            SERIES_V2_COMPRESSED_SIZE_LIMIT + 1,
            SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            SERIES_V2_COMPRESSED_SIZE_LIMIT,
        );
        assert_eq!(uncompressed_limit, SERIES_V2_UNCOMPRESSED_SIZE_LIMIT);
        assert_eq!(compressed_limit, SERIES_V2_COMPRESSED_SIZE_LIMIT);

        let (uncompressed_limit, compressed_limit) = clamp_payload_limits(
            5678,
            1234,
            SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            SERIES_V2_COMPRESSED_SIZE_LIMIT,
        );
        assert_eq!(uncompressed_limit, 5678);
        assert_eq!(compressed_limit, 1234);
    }
}
