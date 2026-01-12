use std::{io, num::NonZeroU64, time::Duration};

use async_trait::async_trait;
use datadog_protos::payload::builder::{
    metric_payload::MetricType, sketch_payload::SketchBuilder, MetricPayloadBuilder, SketchPayloadBuilder,
};
use ddsketch_agent::DDSketch;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use piecemeal::ScratchWriter;
use saluki_common::{iter::ReusableDeduplicator, task::HandleExt as _};
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
use saluki_io::compression::CompressionScheme;
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};

use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    telemetry::ComponentTelemetry,
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
};

const SERIES_V2_COMPRESSED_SIZE_LIMIT: usize = 512_000; // 500 KiB
const SERIES_V2_UNCOMPRESSED_SIZE_LIMIT: usize = 5_242_880; // 5 MiB

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
const SERIES_SOURCE_TYPE_NAME_FIELD_NUMBER: u32 = 7;
const SERIES_INTERVAL_FIELD_NUMBER: u32 = 8;
const SERIES_METADATA_FIELD_NUMBER: u32 = 9;

const SKETCH_METRIC_FIELD_NUMBER: u32 = 1;
const SKETCH_HOST_FIELD_NUMBER: u32 = 2;
const SKETCH_TAGS_FIELD_NUMBER: u32 = 4;
const SKETCH_DOGSKETCHES_FIELD_NUMBER: u32 = 7;
const SKETCH_METADATA_FIELD_NUMBER: u32 = 8;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

const fn default_max_metrics_per_payload() -> usize {
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

/// Datadog Metrics encoder.
///
/// Generates Datadog metrics payloads for the Datadog platform.
#[derive(Clone, Deserialize)]
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

    /// Additional tags to apply to all forwarded metrics.
    #[serde(default, skip)]
    additional_tags: Option<SharedTagSet>,
}

impl DatadogMetricsConfiguration {
    /// Creates a new `DatadogMetricsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
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
        let mut series_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Series);
        let mut sketches_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);

        if let Some(additional_tags) = self.additional_tags.as_ref() {
            series_encoder = series_encoder.with_additional_tags(additional_tags.clone());
            sketches_encoder = sketches_encoder.with_additional_tags(additional_tags.clone());
        }

        let mut series_rb = RequestBuilder::new(series_encoder, compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        series_rb.with_max_inputs_per_payload(self.max_metrics_per_payload);

        let mut sketches_rb = RequestBuilder::new(sketches_encoder, compression_scheme, RB_BUFFER_CHUNK_SIZE).await?;
        sketches_rb.with_max_inputs_per_payload(self.max_metrics_per_payload);

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
}

#[async_trait]
impl Encoder for DatadogMetrics {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            series_rb,
            sketches_rb,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our request builder task.
        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut =
            run_request_builder(series_rb, sketches_rb, telemetry, events_rx, payloads_tx, flush_timeout);
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
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let metric = match event.try_into_metric() {
                        Some(metric) => metric,
                        None => continue,
                    };

                    let request_builder = match MetricsEndpoint::from_metric(&metric) {
                        MetricsEndpoint::Series => &mut series_request_builder,
                        MetricsEndpoint::Sketches => &mut sketches_request_builder,
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
                            Ok((events, request)) => {
                                let payload_meta = PayloadMetadata::from_event_count(events);
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
                        Ok((events, request)) => {
                            let payload_meta = PayloadMetadata::from_event_count(events);
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
                        Ok((events, request)) => {
                            let payload_meta = PayloadMetadata::from_event_count(events);
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

/// Metrics intake endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetricsEndpoint {
    /// Series metrics.
    ///
    /// Includes counters, gauges, rates, and sets.
    Series,

    /// Sketch metrics.
    ///
    /// Includes histograms and distributions.
    Sketches,
}

impl MetricsEndpoint {
    /// Creates a new `MetricsEndpoint` from the given metric.
    pub fn from_metric(metric: &Metric) -> Self {
        match metric.values() {
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
                Self::Series
            }
            MetricValues::Histogram(..) | MetricValues::Distribution(..) => Self::Sketches,
        }
    }
}

#[derive(Debug)]
struct MetricsEndpointEncoder {
    endpoint: MetricsEndpoint,
    scratch_writer: ScratchWriter<Vec<u8>>,
    additional_tags: SharedTagSet,
    tags_deduplicator: ReusableDeduplicator<Tag>,
}

impl MetricsEndpointEncoder {
    /// Creates a new `MetricsEndpointEncoder` for the given endpoint.
    pub fn from_endpoint(endpoint: MetricsEndpoint) -> Self {
        Self {
            endpoint,
            scratch_writer: ScratchWriter::new(Vec::new()),
            additional_tags: SharedTagSet::default(),
            tags_deduplicator: ReusableDeduplicator::new(),
        }
    }

    /// Sets the additional tags to be included with every metric encoded by this encoder.
    ///
    /// These tags are added in a deduplicated fashion, the same as instrumented tags and origin tags. This is an
    /// optimized codepath for tag inclusion in high-volume scenarios, where creating new additional contexts
    /// through the traditional means (e.g., `ContextResolver`) would be too expensive.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        self.additional_tags = additional_tags;
        self
    }
}

impl EndpointEncoder for MetricsEndpointEncoder {
    type Input = Metric;
    type EncodeError = protobuf::Error;

    fn encoder_name() -> &'static str {
        "metrics"
    }

    fn compressed_size_limit(&self) -> usize {
        match self.endpoint {
            MetricsEndpoint::Series => SERIES_V2_COMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::Sketches => DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
        }
    }

    fn uncompressed_size_limit(&self) -> usize {
        match self.endpoint {
            MetricsEndpoint::Series => SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::Sketches => DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
        }
    }

    fn is_valid_input(&self, input: &Self::Input) -> bool {
        let input_endpoint = MetricsEndpoint::from_metric(input);
        input_endpoint == self.endpoint
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        encode_single_metric(
            input,
            &self.additional_tags,
            buffer,
            &mut self.scratch_writer,
            &mut self.tags_deduplicator,
        )?;

        Ok(())
    }

    fn endpoint_uri(&self) -> Uri {
        match self.endpoint {
            MetricsEndpoint::Series => PathAndQuery::from_static("/api/v2/series").into(),
            MetricsEndpoint::Sketches => PathAndQuery::from_static("/api/beta/sketches").into(),
        }
    }

    fn endpoint_method(&self) -> Method {
        // Both endpoints use POST.
        Method::POST
    }

    fn content_type(&self) -> HeaderValue {
        // Both endpoints encode via Protocol Buffers.
        CONTENT_TYPE_PROTOBUF.clone()
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
    metric: &Metric, additional_tags: &SharedTagSet, output_buf: &mut Vec<u8>,
    scratch_writer: &mut ScratchWriter<Vec<u8>>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> io::Result<()> {
    // Depending on the metric type, we write out the appropriate fields.
    match metric.values() {
        MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
            encode_series_metric(metric, additional_tags, output_buf, scratch_writer, tags_deduplicator)
        }
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => {
            encode_sketch_metric(metric, additional_tags, output_buf, scratch_writer, tags_deduplicator)
        }
    }
}

fn encode_series_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_buf: &mut Vec<u8>,
    scratch_writer: &mut ScratchWriter<Vec<u8>>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> io::Result<()> {
    let mut payload_builder = MetricPayloadBuilder::new(scratch_writer);
    payload_builder.add_series(|sb| {
        // Write the metric name and tags.
        //
        // We split out some particular tags, those starting with "dd.internal.resource", as they represent special
        // "resource" attributes which need to be reported in a particular way.
        sb.metric(metric.context().name())?;

        let deduplicated_tags = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
        for tag in deduplicated_tags {
            if tag.name() == "dd.internal.resource" {
                if let Some((resource_type, resource_name)) = tag.value().and_then(|s| s.split_once(':')) {
                    sb.add_resources(|rb| {
                        rb.type_(resource_type)?;
                        rb.name(resource_name)?;
                        Ok(())
                    })?;
                }
            } else {
                sb.tags(|tb| tb.add(tag.as_str()))?;
            }
        }

        // Set the host resource.
        sb.add_resources(|rb| {
            rb.type_("host")?;
            rb.name(metric.metadata().hostname().unwrap_or_default())?;
            Ok(())
        })?;

        // Write the origin metadata, if it exists.
        if let Some(origin) = metric.metadata().origin() {
            match origin {
                MetricOrigin::SourceType(source_type) => {
                    sb.source_type_name(source_type.as_ref())?;
                }
                MetricOrigin::OriginMetadata {
                    product,
                    subproduct,
                    product_detail,
                } => {
                    sb.metadata(|mb| {
                        mb.origin(|ob| {
                            ob.origin_product(*product)?;
                            ob.origin_category(*subproduct)?;
                            ob.origin_service(*product_detail)?;
                            Ok(())
                        })?;
                        Ok(())
                    })?;
                }
            }
        }

        // Now write out our metric type, points, and interval (if applicable).
        let (metric_type, points, maybe_interval) = match metric.values() {
            MetricValues::Counter(points) => (MetricType::COUNT, points.into_iter(), None),
            MetricValues::Rate(points, interval) => (MetricType::RATE, points.into_iter(), Some(interval)),
            MetricValues::Gauge(points) => (MetricType::GAUGE, points.into_iter(), None),
            MetricValues::Set(points) => (MetricType::GAUGE, points.into_iter(), None),
            _ => unreachable!(),
        };

        sb.type_(metric_type)?;

        if let Some(interval) = maybe_interval {
            sb.interval(interval.as_secs() as i64)?;
        }

        for (timestamp, value) in points {
            // If this is a rate metric, scale our value by the interval, in seconds.
            let value = maybe_interval
                .map(|interval| value / interval.as_secs_f64())
                .unwrap_or(value);
            let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

            sb.add_points(|pb| {
                pb.timestamp(timestamp)?;
                pb.value(value)?;
                Ok(())
            })?;
        }

        Ok(())
    })?;

    payload_builder.finish(output_buf)
}

fn encode_sketch_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_buf: &mut Vec<u8>,
    scratch_writer: &mut ScratchWriter<Vec<u8>>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> io::Result<()> {
    let mut payload_builder = SketchPayloadBuilder::new(scratch_writer);
    payload_builder.add_sketches(|sb| {
        // Write the metric name and tags.
        sb.metric(metric.context().name())?;

        let deduplicated_tags = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
        sb.tags(|tb| tb.add_many_mapped(deduplicated_tags, |tag| tag.as_str()))?;

        // Write the host.
        sb.host(metric.metadata().hostname().unwrap_or_default())?;

        // Write the origin metadata, if it exists.
        if let Some(MetricOrigin::OriginMetadata {
            product,
            subproduct,
            product_detail,
        }) = metric.metadata().origin()
        {
            sb.metadata(|mb| {
                mb.origin(|ob| {
                    ob.origin_product(*product)?;
                    ob.origin_category(*subproduct)?;
                    ob.origin_service(*product_detail)?;
                    Ok(())
                })?;
                Ok(())
            })?;
        }

        // Write out our sketches.
        match metric.values() {
            MetricValues::Distribution(sketches) => {
                for (timestamp, value) in sketches {
                    write_dogsketch(sb, timestamp, value)?;
                }
            }
            MetricValues::Histogram(points) => {
                // We convert histograms to sketches to be able to write them out in the payload.
                let mut ddsketch = DDSketch::default();

                for (timestamp, histogram) in points {
                    ddsketch.clear();
                    for sample in histogram.samples() {
                        ddsketch.insert_n(sample.value.into_inner(), sample.weight);
                    }

                    write_dogsketch(sb, timestamp, &ddsketch)?;
                }
            }
            _ => unreachable!(),
        }

        Ok(())
    })?;

    payload_builder.finish(output_buf)?;

    Ok(())
}

fn write_dogsketch(
    sketch_builder: &mut SketchBuilder<'_, Vec<u8>>, timestamp: Option<NonZeroU64>, sketch: &DDSketch,
) -> io::Result<()> {
    // If the sketch is empty, we don't write it out.
    if sketch.is_empty() {
        warn!("Attempted to write an empty sketch to sketches payload, skipping.");
        return Ok(());
    }

    sketch_builder.add_dogsketches(|db| {
        db.ts(timestamp.map_or(0, |ts| ts.get() as i64))?;
        db.cnt(sketch.count() as i64)?;
        db.min(sketch.min().unwrap())?;
        db.max(sketch.max().unwrap())?;
        db.avg(sketch.avg().unwrap())?;
        db.sum(sketch.sum().unwrap())?;

        let bin_keys = sketch.bins().iter().map(|bin| bin.key());
        db.k(|kb| kb.add_many(bin_keys))?;

        let bin_counts = sketch.bins().iter().map(|bin| bin.count());
        db.n(|nb| nb.add_many(bin_counts))?;

        Ok(())
    })?;

    Ok(())
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use piecemeal::ScratchWriter;
    use protobuf::CodedOutputStream;
    use saluki_common::iter::ReusableDeduplicator;
    use saluki_context::tags::SharedTagSet;
    use saluki_core::data_model::event::metric::Metric;

    use super::{encode_sketch_metric, MetricsEndpoint, MetricsEndpointEncoder};
    use crate::common::datadog::request_builder::EndpointEncoder as _;

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

        let mut scratch_writer = ScratchWriter::new(Vec::new());
        let mut tags_deduplicator = ReusableDeduplicator::new();

        let mut histogram_payload = Vec::new();
        {
            encode_sketch_metric(
                &histogram,
                &host_tags,
                &mut histogram_payload,
                &mut scratch_writer,
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
                &mut histogram_payload,
                &mut scratch_writer,
                &mut tags_deduplicator,
            )
            .expect("Failed to encode distribution as sketch");
        }

        assert_eq!(histogram_payload, distribution_payload);
    }

    #[test]
    fn input_valid() {
        // Our encoder should always consider series metrics valid when set to the series endpoint, and similarly for
        // sketch metrics when set to the sketches endpoint.
        let counter = Metric::counter("counter", 1.0);
        let rate = Metric::rate("rate", 1.0, Duration::from_secs(1));
        let gauge = Metric::gauge("gauge", 1.0);
        let set = Metric::set("set", "foo");
        let histogram = Metric::histogram("histogram", [1.0, 2.0, 3.0]);
        let distribution = Metric::distribution("distribution", [1.0, 2.0, 3.0]);

        let series_endpoint = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Series);
        let sketches_endpoint = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);

        assert!(series_endpoint.is_valid_input(&counter));
        assert!(series_endpoint.is_valid_input(&rate));
        assert!(series_endpoint.is_valid_input(&gauge));
        assert!(series_endpoint.is_valid_input(&set));
        assert!(!series_endpoint.is_valid_input(&histogram));
        assert!(!series_endpoint.is_valid_input(&distribution));

        assert!(!sketches_endpoint.is_valid_input(&counter));
        assert!(!sketches_endpoint.is_valid_input(&rate));
        assert!(!sketches_endpoint.is_valid_input(&gauge));
        assert!(!sketches_endpoint.is_valid_input(&set));
        assert!(sketches_endpoint.is_valid_input(&histogram));
        assert!(sketches_endpoint.is_valid_input(&distribution));
    }
}
