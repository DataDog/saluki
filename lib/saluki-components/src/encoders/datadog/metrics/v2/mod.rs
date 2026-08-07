use std::{fmt, num::NonZeroU64};

use datadog_protos::metrics as proto;
use ddsketch::DDSketch;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use protobuf::{rt::WireType, CodedOutputStream, Enum};
use saluki_common::iter::ReusableDeduplicator;
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_core::data_model::event::metric::{Metric, MetricOrigin, MetricValues};
use saluki_error::GenericError;
use tracing::warn;

use super::{
    endpoint::{EndpointConfiguration, MetricsEndpoint},
    metric_has_emittable_values, sketch_has_emittable_values, v1,
};
use crate::common::datadog::{
    io::RB_BUFFER_CHUNK_SIZE,
    request_builder::{EndpointEncoder, RequestBuilder},
    DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT, DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT, METRICS_SERIES_V1_PATH,
    METRICS_SERIES_V2_PATH, METRICS_SKETCHES_PATH,
};

mod constants;
pub(super) use constants::{SERIES_V2_COMPRESSED_SIZE_LIMIT, SERIES_V2_UNCOMPRESSED_SIZE_LIMIT};

/// Creates a V2 request builder for the given endpoint.
///
/// # Errors
///
/// If the request builder cannot be created, an error is returned.
pub async fn create_v2_request_builder(
    endpoint: MetricsEndpoint, endpoint_config: &EndpointConfiguration,
) -> Result<RequestBuilder<MetricsEndpointEncoder>, GenericError> {
    let encoder =
        MetricsEndpointEncoder::from_endpoint(endpoint).with_additional_tags(endpoint_config.additional_tags().clone());

    let mut request_builder =
        RequestBuilder::new(encoder, endpoint_config.compression_scheme(), RB_BUFFER_CHUNK_SIZE).await?;
    request_builder.with_max_inputs_per_payload(endpoint_config.max_metrics_per_payload());
    if matches!(endpoint, MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2) {
        request_builder.with_max_data_points_per_payload(endpoint_config.max_series_points_per_payload());
    }

    Ok(request_builder)
}

/// An encoder for V2 metrics.
///
/// This also handles the legacy V1 JSON series endpoint when `use_v2_api_series` is disabled.
#[derive(Debug)]
pub struct MetricsEndpointEncoder {
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

impl EndpointEncoder for MetricsEndpointEncoder {
    type Input = Metric;
    type EncodeError = MetricsEncodeError;

    fn encoder_name() -> &'static str {
        "metrics"
    }

    fn compressed_size_limit(&self) -> usize {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => v1::SERIES_COMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::SeriesV2 => constants::SERIES_V2_COMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::Sketches => DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT,
        }
    }

    fn uncompressed_size_limit(&self) -> usize {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => v1::SERIES_UNCOMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::SeriesV2 => constants::SERIES_V2_UNCOMPRESSED_SIZE_LIMIT,
            MetricsEndpoint::Sketches => DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT,
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
            MetricsEndpoint::SeriesV1 | MetricsEndpoint::SeriesV2 => is_series_input && v1::has_emittable_point(input),
            MetricsEndpoint::Sketches => !is_series_input && metric_has_emittable_values(input),
        }
    }

    fn get_payload_prefix(&self) -> Option<&'static [u8]> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => Some(v1::SERIES_PAYLOAD_PREFIX),
            _ => None,
        }
    }

    fn get_payload_suffix(&self) -> Option<&'static [u8]> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => Some(v1::SERIES_PAYLOAD_SUFFIX),
            _ => None,
        }
    }

    fn get_input_separator(&self) -> Option<&'static [u8]> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => Some(v1::SERIES_INPUT_SEPARATOR),
            _ => None,
        }
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        match self.endpoint {
            MetricsEndpoint::SeriesV1 => {
                v1::encode_series_metric(input, &self.additional_tags, buffer, &mut self.tags_deduplicator)?;
                Ok(())
            }
            MetricsEndpoint::SeriesV2 | MetricsEndpoint::Sketches => {
                // NOTE: We're passing _four_ buffers to `encode_single_metric`, which is a lot, but with good reason.
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
            MetricsEndpoint::SeriesV1 => v1::CONTENT_TYPE.clone(),
            MetricsEndpoint::SeriesV2 | MetricsEndpoint::Sketches => HeaderValue::from_static("application/x-protobuf"),
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
                encode_series_metric(metric, additional_tags, os, secondary_scratch_buf, tags_deduplicator)
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

fn encode_series_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_stream: &mut CodedOutputStream<'_>,
    scratch_buf: &mut Vec<u8>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), protobuf::Error> {
    // Write the metric name and tags.
    output_stream.write_string(constants::SERIES_METRIC_FIELD_NUMBER, metric.context().name())?;

    let deduplicated_tags = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
    write_series_tags(deduplicated_tags, output_stream, scratch_buf)?;

    // Set the host resource.
    write_resource(
        output_stream,
        scratch_buf,
        "host",
        metric.context().host().unwrap_or_default(),
    )?;

    // Write the origin metadata, if it exists.
    if let Some(origin) = metric.metadata().origin() {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                output_stream.write_string(constants::SERIES_SOURCE_TYPE_NAME_FIELD_NUMBER, source_type.as_ref())?;
            }
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => {
                write_origin_metadata(
                    output_stream,
                    scratch_buf,
                    constants::SERIES_METADATA_FIELD_NUMBER,
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
        _ => unreachable!(),
    };

    output_stream.write_enum(constants::SERIES_TYPE_FIELD_NUMBER, metric_type.value())?;

    if let Some(unit) = metric.metadata().unit() {
        output_stream.write_string(constants::SERIES_UNIT_FIELD_NUMBER, unit)?;
    }

    for (timestamp, value) in points {
        // If this is a rate metric with an interval, scale our value by the interval in seconds.
        // A zero interval represents an unnormalized rate and is emitted without scaling.
        let value = maybe_interval
            .filter(|interval| !interval.is_zero())
            .map(|interval| value / interval.as_secs_f64())
            .unwrap_or(value);
        if !emittable(value) {
            continue;
        }

        let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

        write_point(output_stream, scratch_buf, value, timestamp)?;
    }

    if let Some(interval) = maybe_interval {
        output_stream.write_int64(constants::SERIES_INTERVAL_FIELD_NUMBER, interval.as_secs() as i64)?;
    }

    Ok(())
}

#[inline]
fn emittable(point: f64) -> bool {
    point.is_finite()
}

fn encode_sketch_metric(
    metric: &Metric, additional_tags: &SharedTagSet, output_stream: &mut CodedOutputStream<'_>,
    scratch_buf: &mut Vec<u8>, packed_scratch_buf: &mut Vec<u8>, tags_deduplicator: &mut ReusableDeduplicator<Tag>,
) -> Result<(), protobuf::Error> {
    // Write the metric name and tags.
    output_stream.write_string(constants::SKETCH_METRIC_FIELD_NUMBER, metric.context().name())?;

    let deduplicated_tags = get_deduplicated_tags(metric, additional_tags, tags_deduplicator);
    write_sketch_tags(deduplicated_tags, output_stream, scratch_buf)?;

    // Write the host.
    output_stream.write_string(
        constants::SKETCH_HOST_FIELD_NUMBER,
        metric.context().host().unwrap_or_default(),
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
            constants::SKETCH_METADATA_FIELD_NUMBER,
            *product,
            *subproduct,
            *product_detail,
        )?;
    }

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
        _ => unreachable!(),
    }

    Ok(())
}

fn write_resource(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, resource_type: &str, resource_name: &str,
) -> Result<(), protobuf::Error> {
    write_nested_message(
        output_stream,
        scratch_buf,
        constants::SERIES_RESOURCES_FIELD_NUMBER,
        |os| {
            os.write_string(constants::RESOURCES_TYPE_FIELD_NUMBER, resource_type)?;
            os.write_string(constants::RESOURCES_NAME_FIELD_NUMBER, resource_name)
        },
    )
}

fn write_origin_metadata(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, field_number: u32, origin_product: u32,
    origin_category: u32, origin_service: u32,
) -> Result<(), protobuf::Error> {
    // TODO: Figure out how to cleanly use `write_nested_message` here.

    scratch_buf.clear();

    {
        let mut origin_output_stream = CodedOutputStream::vec(scratch_buf);
        origin_output_stream.write_uint32(constants::ORIGIN_ORIGIN_PRODUCT_FIELD_NUMBER, origin_product)?;
        origin_output_stream.write_uint32(constants::ORIGIN_ORIGIN_CATEGORY_FIELD_NUMBER, origin_category)?;
        origin_output_stream.write_uint32(constants::ORIGIN_ORIGIN_SERVICE_FIELD_NUMBER, origin_service)?;
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
        metadata_output_stream.write_tag(constants::METADATA_ORIGIN_FIELD_NUMBER, WireType::LengthDelimited)?;
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
    write_nested_message(
        output_stream,
        scratch_buf,
        constants::SERIES_POINTS_FIELD_NUMBER,
        |os| {
            os.write_double(constants::METRIC_POINT_VALUE_FIELD_NUMBER, value)?;
            os.write_int64(constants::METRIC_POINT_TIMESTAMP_FIELD_NUMBER, timestamp)
        },
    )
}

fn write_dogsketch(
    output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, packed_scratch_buf: &mut Vec<u8>,
    timestamp: Option<NonZeroU64>, sketch: &DDSketch,
) -> Result<(), protobuf::Error> {
    if !sketch_has_emittable_values(sketch) {
        warn!("Attempted to write an empty sketch to sketches payload, skipping.");
        return Ok(());
    }

    write_nested_message(
        output_stream,
        scratch_buf,
        constants::SKETCH_DOGSKETCHES_FIELD_NUMBER,
        |os| {
            os.write_int64(
                constants::DOGSKETCH_TS_FIELD_NUMBER,
                timestamp.map_or(0, |ts| ts.get() as i64),
            )?;
            os.write_int64(constants::DOGSKETCH_CNT_FIELD_NUMBER, sketch.count() as i64)?;
            os.write_double(constants::DOGSKETCH_MIN_FIELD_NUMBER, sketch.stored_min())?;
            os.write_double(constants::DOGSKETCH_MAX_FIELD_NUMBER, sketch.stored_max())?;
            os.write_double(constants::DOGSKETCH_AVG_FIELD_NUMBER, sketch.stored_avg())?;
            os.write_double(constants::DOGSKETCH_SUM_FIELD_NUMBER, sketch.stored_sum())?;

            let bin_keys = sketch.bins().iter().map(|bin| bin.key());
            write_repeated_packed_from_iter(
                os,
                packed_scratch_buf,
                constants::DOGSKETCH_K_FIELD_NUMBER,
                bin_keys,
                |inner_os, value| inner_os.write_sint32_no_tag(value),
            )?;

            let bin_counts = sketch.bins().iter().map(|bin| bin.count());
            write_repeated_packed_from_iter(
                os,
                packed_scratch_buf,
                constants::DOGSKETCH_N_FIELD_NUMBER,
                bin_counts,
                |inner_os, value| inner_os.write_uint32_no_tag(value),
            )
        },
    )
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
            os.write_string(constants::SERIES_TAGS_FIELD_NUMBER, tag.as_str())
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
        os.write_string(constants::SKETCH_TAGS_FIELD_NUMBER, tag.as_str())
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
    use std::time::Duration;

    use datadog_protos::metrics::Sketch;
    use ddsketch::DDSketch;
    use protobuf::{CodedOutputStream, Message};
    use saluki_common::iter::ReusableDeduplicator;
    use saluki_context::{tags::SharedTagSet, Context};
    use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricValues};
    use stringtheory::MetaString;

    use super::{encode_series_metric, encode_sketch_metric, v1, MetricsEndpoint, MetricsEndpointEncoder};
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
    fn encodes_zero_count_distribution_with_bins_and_zero_summary() {
        let mut sketch = DDSketch::default();
        sketch.insert_n(42.0, 10);
        sketch.set_count(0);
        sketch.set_sum(0.0);
        sketch.set_avg(0.0);
        sketch.set_min(0.0);
        sketch.set_max(0.0);

        let metric = Metric::from_parts(
            Context::from_static_parts("zero.count.distribution", &[]),
            MetricValues::distribution((123_u64, sketch)),
            MetricMetadata::default(),
        );

        let host_tags = SharedTagSet::default();
        let mut scratch_buf = Vec::new();
        let mut packed_scratch_buf = Vec::new();
        let mut tags_deduplicator = ReusableDeduplicator::new();
        let mut payload = Vec::new();
        {
            let mut writer = CodedOutputStream::vec(&mut payload);
            encode_sketch_metric(
                &metric,
                &host_tags,
                &mut writer,
                &mut scratch_buf,
                &mut packed_scratch_buf,
                &mut tags_deduplicator,
            )
            .expect("zero-count distribution should encode");
            writer.flush().expect("payload should flush");
        }

        let sketch_payload = Sketch::parse_from_bytes(&payload).expect("payload should decode");
        let dogsketch = sketch_payload
            .dogsketches()
            .first()
            .expect("payload should contain a sketch");
        assert_eq!(dogsketch.cnt(), 0);
        assert_eq!(dogsketch.min(), 0.0);
        assert_eq!(dogsketch.max(), 0.0);
        assert_eq!(dogsketch.avg(), 0.0);
        assert_eq!(dogsketch.sum(), 0.0);
        assert!(!dogsketch.k().is_empty());
        assert!(!dogsketch.n().is_empty());
    }

    #[test]
    fn input_valid() {
        // Our encoder should consider finite series metrics valid when set to either series endpoint, and similarly
        // for sketch metrics when set to the sketches endpoint.
        let counter = Metric::counter("counter", 1.0);
        let rate = Metric::rate("rate", 1.0, Duration::from_secs(1));
        let gauge = Metric::gauge("gauge", 1.0);
        let set = Metric::set("set", "foo");
        let non_finite_gauge = Metric::from_parts(
            Context::from_static_parts("non_finite_gauge", &[]),
            MetricValues::gauge([(1, f64::NAN), (2, f64::INFINITY)]),
            MetricMetadata::default(),
        );
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
            assert!(!series_endpoint.is_valid_input(&non_finite_gauge));
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
    fn drops_non_finite_series_points() {
        let context = Context::from_static_parts("my.gauge", &[]);
        let gauge = Metric::from_parts(
            context,
            MetricValues::gauge([(1, 1.0_f64), (2, f64::NAN), (3, f64::INFINITY), (4, 2.0)]),
            MetricMetadata::default(),
        );

        let host_tags = SharedTagSet::default();
        let mut scratch_buf = Vec::new();
        let mut tags_deduplicator = ReusableDeduplicator::new();

        let mut payload = Vec::new();
        {
            let mut writer = CodedOutputStream::vec(&mut payload);
            encode_series_metric(
                &gauge,
                &host_tags,
                &mut writer,
                &mut scratch_buf,
                &mut tags_deduplicator,
            )
            .expect("Failed to encode gauge as series metric");
            writer.flush().expect("Failed to flush");
        }

        let series = datadog_protos::metrics::MetricSeries::parse_from_bytes(&payload).expect("payload should decode");
        let values: Vec<f64> = series.points.iter().map(|point| point.value).collect();
        assert_eq!(values, vec![1.0, 2.0]);
    }

    #[test]
    fn encodes_zero_interval_rate_without_scaling() {
        let rate = Metric::rate("my.unnormalized.rate", 42.0, Duration::ZERO);
        let host_tags = SharedTagSet::default();
        let mut scratch_buf = Vec::new();
        let mut tags_deduplicator = ReusableDeduplicator::new();
        let mut payload = Vec::new();
        {
            let mut writer = CodedOutputStream::vec(&mut payload);
            encode_series_metric(&rate, &host_tags, &mut writer, &mut scratch_buf, &mut tags_deduplicator)
                .expect("zero-interval rate should encode");
            writer.flush().expect("payload should flush");
        }

        let series = datadog_protos::metrics::MetricSeries::parse_from_bytes(&payload).expect("payload should decode");
        assert_eq!(series.points.len(), 1);
        assert_eq!(series.points[0].value, 42.0);
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
        let context = Context::from_static_parts("my.timer.avg", &[]);
        let metadata = MetricMetadata::default().with_unit(MetaString::from_static("millisecond"));
        let gauge = Metric::from_parts(context, MetricValues::gauge([1.0_f64]), metadata);

        let host_tags = SharedTagSet::default();
        let mut scratch_buf = Vec::new();
        let mut tags_deduplicator = ReusableDeduplicator::new();

        let mut payload = Vec::new();
        {
            let mut writer = CodedOutputStream::vec(&mut payload);
            encode_series_metric(
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
    fn series_v1_endpoint_routing() {
        // SeriesV1 advertises the V1 URI, JSON content type, and the {"series":[...]} framing.
        let encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV1);
        assert_eq!(encoder.endpoint_uri().path(), "/api/v1/series");
        assert_eq!(encoder.content_type(), "application/json");
        assert_eq!(encoder.get_payload_prefix(), Some(v1::SERIES_PAYLOAD_PREFIX));
        assert_eq!(encoder.get_payload_suffix(), Some(v1::SERIES_PAYLOAD_SUFFIX));
        assert_eq!(encoder.get_input_separator(), Some(v1::SERIES_INPUT_SEPARATOR));

        // V2 series stays on protobuf with no framing.
        let v2 = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::SeriesV2);
        assert_eq!(v2.endpoint_uri().path(), "/api/v2/series");
        assert_eq!(v2.content_type(), "application/x-protobuf");
        assert!(v2.get_payload_prefix().is_none());
    }
}
