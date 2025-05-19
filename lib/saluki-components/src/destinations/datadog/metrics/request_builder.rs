use std::num::NonZeroU64;

use datadog_protos::metrics::{self as proto, Resource};
use ddsketch_agent::DDSketch;
use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
use protobuf::{rt::WireType, CodedOutputStream};
use saluki_context::tags::Tagged as _;
use saluki_core::data_model::event::metric::*;

use crate::destinations::datadog::common::request_builder::EndpointEncoder;

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
}

impl MetricsEndpointEncoder {
    /// Creates a new `MetricsEndpointEncoder` for the given endpoint.
    pub const fn from_endpoint(endpoint: MetricsEndpoint) -> Self {
        Self { endpoint }
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

    fn encode(&self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
        let encoded_metric = encode_single_metric(input);
        encoded_metric.write(buffer)
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

enum EncodedMetric {
    Series(proto::MetricSeries),
    Sketch(proto::Sketch),
}

impl EncodedMetric {
    fn field_number(&self) -> u32 {
        // TODO: We _should_ derive this from the Protocol Buffers definitions themselves.
        //
        // This is more about establishing provenance than worrying about field numbers changing, though, since field
        // numbers changing is not backwards-compatible and should basically never, ever happen.
        match self {
            Self::Series(_) => 1,
            Self::Sketch(_) => 1,
        }
    }

    fn write(&self, buf: &mut Vec<u8>) -> Result<(), protobuf::Error> {
        let mut output_stream = CodedOutputStream::vec(buf);

        // Write the field tag.
        let field_number = self.field_number();
        output_stream.write_tag(field_number, WireType::LengthDelimited)?;

        // Write the message.
        match self {
            Self::Series(series) => output_stream.write_message_no_tag(series)?,
            Self::Sketch(sketch) => output_stream.write_message_no_tag(sketch)?,
        }

        Ok(())
    }
}

fn encode_single_metric(metric: &Metric) -> EncodedMetric {
    match metric.values() {
        MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
            EncodedMetric::Series(encode_series_metric(metric))
        }
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => {
            EncodedMetric::Sketch(encode_sketch_metric(metric))
        }
    }
}

fn encode_series_metric(metric: &Metric) -> proto::MetricSeries {
    let mut series = proto::MetricSeries::new();
    series.set_metric(metric.context().name().clone().into());

    // Explicitly set the host resource.
    let mut host_resource = Resource::new();
    host_resource.set_type("host".to_string().into());
    host_resource.set_name(metric.metadata().hostname().map(|h| h.into()).unwrap_or_default());
    series.mut_resources().push(host_resource);

    // Collect and set all of our metric tags.
    //
    // This involves extracting some specific tags first that have to be set on dedicated fields (resources, etc)
    // and then setting the rest as generic tags.
    let mut tags = Vec::new();

    metric.context().visit_tags_deduped(|tag| {
        // If this is a resource tag, we'll convert it directly to a resource entry.
        if tag.name() == "dd.internal.resource" {
            if let Some((resource_type, resource_name)) = tag.value().and_then(|s| s.split_once(':')) {
                let mut resource = Resource::new();
                resource.set_type(resource_type.into());
                resource.set_name(resource_name.into());
                series.mut_resources().push(resource);
            }
        } else {
            // We're dealing with a normal tag, so collect it.
            tags.push(tag.as_str().into());
        }
    });

    series.set_tags(tags);

    // Set the origin metadata, if it exists.
    if let Some(origin) = metric.metadata().origin() {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                series.set_source_type_name(source_type.as_ref().into());
            }
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => {
                series.set_metadata(origin_metadata_to_proto_metadata(
                    *product,
                    *subproduct,
                    *product_detail,
                ));
            }
        }
    }

    let (metric_type, points, maybe_interval) = match metric.values() {
        MetricValues::Counter(points) => (proto::MetricType::COUNT, points.into_iter(), None),
        MetricValues::Rate(points, interval) => (proto::MetricType::RATE, points.into_iter(), Some(interval)),
        MetricValues::Gauge(points) => (proto::MetricType::GAUGE, points.into_iter(), None),
        MetricValues::Set(points) => (proto::MetricType::GAUGE, points.into_iter(), None),
        _ => unreachable!(),
    };

    series.set_type(metric_type);

    for (timestamp, value) in points {
        // If this is a rate metric, scale our value by the interval, in seconds.
        let value = maybe_interval
            .map(|interval| value / interval.as_secs_f64())
            .unwrap_or(value);
        let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

        let mut point = proto::MetricPoint::new();
        point.set_value(value);
        point.set_timestamp(timestamp);

        series.mut_points().push(point);
    }

    if let Some(interval) = maybe_interval {
        series.set_interval(interval.as_secs() as i64);
    }

    series
}

fn encode_sketch_metric(metric: &Metric) -> proto::Sketch {
    // TODO: I wonder if it would be more efficient to keep a `proto::Sketch` around and just clear it before encoding a
    // sketch metric. We'd avoid a few allocations for things that involve `Vec<T>`, although we wouldn't save anything
    // on string fields, since we're still allocating owned copies to convert into `Chars`, and clearing the existing
    // `Chars` ends up as a call to `Bytes::clear`... which seems like it has very little overhead, but it might be more
    // overhead than just creating a fresh `proto::Sketch` each time.
    //
    // Something to benchmark in the future.
    let mut sketch = proto::Sketch::new();
    sketch.set_metric(metric.context().name().into());
    sketch.set_host(metric.metadata().hostname().map(|h| h.into()).unwrap_or_default());

    // Collect and set all of our metric tags.
    let mut tags = Vec::new();

    metric.context().visit_tags_deduped(|tag| {
        tags.push(tag.as_str().into());
    });

    sketch.set_tags(tags);

    // Set the origin metadata, if it exists.
    if let Some(MetricOrigin::OriginMetadata {
        product,
        subproduct,
        product_detail,
    }) = metric.metadata().origin()
    {
        sketch.set_metadata(origin_metadata_to_proto_metadata(
            *product,
            *subproduct,
            *product_detail,
        ));
    }

    match metric.values() {
        MetricValues::Distribution(sketches) => {
            for (timestamp, value) in sketches {
                // Distributions already have sketch points natively, so we just write them as-is.
                write_ddsketch_to_proto_sketch(timestamp, value, &mut sketch);
            }
        }
        MetricValues::Histogram(points) => {
            for (timestamp, histogram) in points {
                // We convert histograms to sketches to be able to write them out in the payload.
                let mut ddsketch = DDSketch::default();
                for sample in histogram.samples() {
                    ddsketch.insert_n(sample.value.into_inner(), sample.weight as u32);
                }

                write_ddsketch_to_proto_sketch(timestamp, &ddsketch, &mut sketch);
            }
        }
        _ => unreachable!(),
    }

    sketch
}

fn write_ddsketch_to_proto_sketch(timestamp: Option<NonZeroU64>, sketch: &DDSketch, proto_sketch: &mut proto::Sketch) {
    let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

    let mut dogsketch = proto::Dogsketch::new();
    dogsketch.set_ts(timestamp);
    sketch.merge_to_dogsketch(&mut dogsketch);

    proto_sketch.mut_dogsketches().push(dogsketch);
}

fn origin_metadata_to_proto_metadata(product: u32, subproduct: u32, product_detail: u32) -> proto::Metadata {
    // NOTE: The naming discrepancies here -- category vs subproduct and service vs product detail -- are a consequence
    // of how these fields are named in the Datadog Platform, and the Protocol Buffers definitions used by the Datadog
    // Agent -- which we build off of -- have not yet caught up with renaming them to match.
    let mut metadata = proto::Metadata::new();
    let proto_origin = metadata.mut_origin();
    proto_origin.set_origin_product(product);
    proto_origin.set_origin_category(subproduct);
    proto_origin.set_origin_service(product_detail);
    metadata
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

        let histogram_payload = encode_sketch_metric(&histogram);
        let distribution_payload = encode_sketch_metric(&distribution);

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
