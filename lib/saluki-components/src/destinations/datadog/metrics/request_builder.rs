use std::num::NonZeroU64;

use datadog_protos::metrics as proto;
use ddsketch_agent::DDSketch;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use protobuf::{rt::WireType, CodedOutputStream, Enum};
use saluki_context::tags::{Tag, Tagged as _};
use saluki_core::data_model::event::metric::*;
use tracing::warn;

use crate::destinations::datadog::common::request_builder::EndpointEncoder;

// Protocol Buffers field numbers for series and sketch payload messages.
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

/// Metrics intake endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricsEndpoint {
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

    /// Returns the compressed size limit for the given endpoint.
    pub const fn compressed_size_limit(&self) -> usize {
        match self {
            Self::Series => 512_000,     // 500 kB
            Self::Sketches => 3_200_000, // 3 MB
        }
    }

    /// Returns the uncompressed size limit for the given endpoint.
    pub const fn uncompressed_size_limit(&self) -> usize {
        match self {
            Self::Series => 5_242_880,    // 5 MiB
            Self::Sketches => 62_914_560, // 60 MiB
        }
    }
}

/// An `EndpointEncoder` for sending metrics to Datadog.
#[derive(Debug)]
pub struct MetricsEndpointEncoder {
    endpoint: MetricsEndpoint,
    primary_scratch_buf: Vec<u8>,
    secondary_scratch_buf: Vec<u8>,
    packed_scratch_buf: Vec<u8>,
}

impl MetricsEndpointEncoder {
    /// Creates a new `MetricsEndpointEncoder` for the given endpoint.
    pub const fn from_endpoint(endpoint: MetricsEndpoint) -> Self {
        Self {
            endpoint,
            primary_scratch_buf: Vec::new(),
            secondary_scratch_buf: Vec::new(),
            packed_scratch_buf: Vec::new(),
        }
    }
}

impl EndpointEncoder for MetricsEndpointEncoder {
    type Input = Metric;
    type EncodeError = protobuf::Error;

    fn encoder_name() -> &'static str {
        "metrics"
    }

    fn compressed_size_limit(&self) -> usize {
        self.endpoint.compressed_size_limit()
    }

    fn uncompressed_size_limit(&self) -> usize {
        self.endpoint.uncompressed_size_limit()
    }

    fn is_valid_input(&self, input: &Self::Input) -> bool {
        let input_endpoint = MetricsEndpoint::from_metric(input);
        input_endpoint == self.endpoint
    }

    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        // NOTE: We're passing _four_ buffers to `encode_single_metric`, which is a lot, but with good reason.
        //
        // The first buffer, `buffer`, is the overall output buffer: the caller expects us to put the full encoded
        // metric payload into this buffer.
        //
        // The second and third buffers, `primary_scratch_buf` and `secondary_scratch_buf`, are used for roughly the
        // same thing but deal with _nesting_. When writing a "message" in Protocol Buffers, the message data itself is
        // prefixed with the field number and a length delimiter that specifies how long the message is. We can't write
        // that length delimiter until we know the full size of the message, so we write the message to a scratch
        // buffer, calculate its size, and then write the field number and length delimiter to the output buffer
        // followed by the message data from the scratch buffer.
        //
        // We have _two_ scratch buffers because you need a dedicated buffer for each level of nested message. We have
        // to be able to nest up to two levels deep in our metrics payload, so we need two scratch buffers to handle
        // that.
        //
        // The fourth buffer, `packed_scratch_buf`, is used for writing out packed repeated fields. This is similar to
        // the situation describe above, except it's not _exactly_ the same as an additional level of nesting.. so I
        // just decided to give it a somewhat more descriptive name.
        encode_single_metric(
            input,
            buffer,
            &mut self.primary_scratch_buf,
            &mut self.secondary_scratch_buf,
            &mut self.packed_scratch_buf,
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "message size exceeds limit (2147483648 bytes)",
        )
        .into());
    }

    Ok(raw_msg_size as u32)
}

fn get_message_size_from_buffer(buf: &[u8]) -> Result<u32, protobuf::Error> {
    get_message_size(buf.len())
}

fn encode_single_metric(
    metric: &Metric, output_buf: &mut Vec<u8>, primary_scratch_buf: &mut Vec<u8>, secondary_scratch_buf: &mut Vec<u8>,
    packed_scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    let mut output_stream = CodedOutputStream::vec(output_buf);
    let field_number = field_number_for_metric_type(metric);

    write_nested_message(&mut output_stream, primary_scratch_buf, field_number, |os| {
        // Depending on the metric type, we write out the appropriate fields.
        match metric.values() {
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
                encode_series_metric(metric, os, secondary_scratch_buf)
            }
            MetricValues::Histogram(..) | MetricValues::Distribution(..) => {
                encode_sketch_metric(metric, os, secondary_scratch_buf, packed_scratch_buf)
            }
        }
    })
}

fn encode_series_metric(
    metric: &Metric, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    // Write the metric name and tags.
    output_stream.write_string(SERIES_METRIC_FIELD_NUMBER, metric.context().name())?;
    write_series_tags(metric, output_stream, scratch_buf)?;

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
        _ => unreachable!(),
    };

    output_stream.write_enum(SERIES_TYPE_FIELD_NUMBER, metric_type.value())?;

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

fn encode_sketch_metric(
    metric: &Metric, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>,
    packed_scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    // Write the metric name and tags.
    output_stream.write_string(SKETCH_METRIC_FIELD_NUMBER, metric.context().name())?;
    write_sketch_tags(metric, output_stream, scratch_buf)?;

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
                    ddsketch.insert_n(sample.value.into_inner(), sample.weight as u32);
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

fn write_tags<F>(
    metric: &Metric, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>, tag_encoder: F,
) -> Result<(), protobuf::Error>
where
    F: Fn(&Tag, &mut CodedOutputStream<'_>, &mut Vec<u8>) -> Result<(), protobuf::Error>,
{
    // This function hides the boilerplate of writing tags to the output stream through the required visitor pattern.
    //
    // The visitor pattern makes it more difficult to bail early on errors, so it tracks that error state separately as
    // it goes through the tags, and ensures that we stop processing additional tags after we hit the first error, if any.
    let mut maybe_encode_error = None;

    metric.context().visit_tags_deduped(|tag| {
        // If the last tag encoding attempt resulted in an error, we skip further processing.
        if maybe_encode_error.is_some() {
            return;
        }

        // Try to encode the tag, capturing any errors that occur.
        if let Err(e) = tag_encoder(tag, output_stream, scratch_buf) {
            maybe_encode_error = Some(e);
        }
    });

    // Make sure we didn't encounter any errors while encoding tags.
    maybe_encode_error.map(Err).unwrap_or(Ok(()))
}

fn write_series_tags(
    metric: &Metric, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    write_tags(metric, output_stream, scratch_buf, |tag, os, buf| {
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

fn write_sketch_tags(
    metric: &Metric, output_stream: &mut CodedOutputStream<'_>, scratch_buf: &mut Vec<u8>,
) -> Result<(), protobuf::Error> {
    write_tags(metric, output_stream, scratch_buf, |tag, os, _buf| {
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
    use std::time::Duration;

    use protobuf::CodedOutputStream;
    use saluki_core::data_model::event::metric::Metric;

    use super::{encode_sketch_metric, MetricsEndpoint, MetricsEndpointEncoder};
    use crate::destinations::datadog::common::request_builder::EndpointEncoder as _;

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

        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();

        let mut histogram_payload = Vec::new();
        {
            let mut histogram_writer = CodedOutputStream::vec(&mut histogram_payload);
            encode_sketch_metric(&histogram, &mut histogram_writer, &mut buf1, &mut buf2)
                .expect("Failed to encode histogram as sketch");
        }

        let mut distribution_payload = Vec::new();
        {
            let mut distribution_writer = CodedOutputStream::vec(&mut distribution_payload);
            encode_sketch_metric(&distribution, &mut distribution_writer, &mut buf1, &mut buf2)
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
