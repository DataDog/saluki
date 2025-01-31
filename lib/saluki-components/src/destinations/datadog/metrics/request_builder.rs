use std::{io, num::NonZeroU64};

use datadog_protos::metrics::{self as proto, Resource};
use ddsketch_agent::DDSketch;
use http::{uri::PathAndQuery, HeaderValue, Method, Request, Uri};
use protobuf::CodedOutputStream;
use saluki_context::tags::Tagged as _;
use saluki_core::pooling::ObjectPool;
use saluki_event::metric::*;
use saluki_io::{
    buf::{BytesBuffer, ChunkedBytesBuffer, ChunkedBytesBufferObjectPool, FrozenChunkedBytesBuffer},
    compression::*,
};
use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, trace};

pub(super) const SCRATCH_BUF_CAPACITY: usize = 8192;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");
static CONTENT_ENCODING_DEFLATE: HeaderValue = HeaderValue::from_static("deflate");

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum RequestBuilderError {
    #[snafu(display("got metric type {} for endpoint {:?}", metric_type, endpoint))]
    InvalidMetricForEndpoint {
        metric_type: &'static str,
        endpoint: MetricsEndpoint,
    },
    #[snafu(display("failed to encode/write payload: {}", source))]
    FailedToEncode { source: protobuf::Error },
    #[snafu(display(
        "request payload was too large after compressing ({} > {})",
        compressed_size_bytes,
        compressed_limit_bytes
    ))]
    PayloadTooLarge {
        compressed_size_bytes: usize,
        compressed_limit_bytes: usize,
    },
    #[snafu(display("failed to write/compress payload: {}", source))]
    Io { source: io::Error },
    #[snafu(display("error when building API endpoint/request: {}", source))]
    Http { source: http::Error },
}

impl RequestBuilderError {
    pub fn is_recoverable(&self) -> bool {
        match self {
            // If the wrong metric type is being sent to the wrong endpoint's request builder, that's just a flat out
            // bug, so we can't possibly recover.
            Self::InvalidMetricForEndpoint { .. } => false,
            // I/O errors should only be getting created for compressor-related operations, and the scenarios in which
            // there are I/O errors should generally be very narrowly scoped to "the system is in a very bad state", so
            // we can't really recover from those... or perhaps _shouldn't_ try to recover from those.
            Self::Io { .. } => false,
            _ => true,
        }
    }
}

/// Metrics intake endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricsEndpoint {
    /// Intake for series metrics.
    Series,

    /// Intake for sketch metrics.
    Sketches,
}

impl MetricsEndpoint {
    /// Gets the uncompressed payload size limit for this endpoint.
    pub const fn uncompressed_size_limit(&self) -> usize {
        match self {
            Self::Series => 5_242_880,    // 5 MiB
            Self::Sketches => 62_914_560, // 60 MiB
        }
    }

    /// Gets the compressed payload size limit for this endpoint.
    pub const fn compressed_size_limit(&self) -> usize {
        match self {
            Self::Series => 512_000,     // 500 kB
            Self::Sketches => 3_200_000, // 3 MB
        }
    }

    /// Gets the endpoint for the given metric.
    pub fn from_metric(metric: &Metric) -> Self {
        match metric.values() {
            MetricValues::Counter(..) | MetricValues::Rate(..) | MetricValues::Gauge(..) | MetricValues::Set(..) => {
                Self::Series
            }
            MetricValues::Histogram(..) | MetricValues::Distribution(..) => Self::Sketches,
        }
    }

    /// Gets the relative URI for this endpoint.
    pub fn endpoint_uri(&self) -> Uri {
        match self {
            Self::Series => PathAndQuery::from_static("/api/v2/series").into(),
            Self::Sketches => PathAndQuery::from_static("/api/beta/sketches").into(),
        }
    }
}

pub struct RequestBuilder<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    endpoint: MetricsEndpoint,
    endpoint_uri: Uri,
    buffer_pool: ChunkedBytesBufferObjectPool<O>,
    scratch_buf: Vec<u8>,
    compressor: Compressor<ChunkedBytesBuffer<O>>,
    compression_estimator: CompressionEstimator,
    uncompressed_len: usize,
    metrics_written: usize,
    scratch_buf_lens: Vec<usize>,
}

impl<O> RequestBuilder<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    /// Creates a new `RequestBuilder` for the given endpoint, using the specified API key and base URI.
    pub async fn new(endpoint: MetricsEndpoint, buffer_pool: O) -> Result<Self, RequestBuilderError> {
        let chunked_buffer_pool = ChunkedBytesBufferObjectPool::new(buffer_pool);
        let compressor = create_compressor(&chunked_buffer_pool).await;
        Ok(Self {
            endpoint,
            endpoint_uri: endpoint.endpoint_uri(),
            buffer_pool: chunked_buffer_pool,
            scratch_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
            compressor,
            compression_estimator: CompressionEstimator::default(),
            uncompressed_len: 0,
            metrics_written: 0,
            scratch_buf_lens: Vec::new(),
        })
    }

    /// Attempts to encode a metric and write it to the current request payload.
    ///
    /// If the metric can't be encoded due to size constraints, `Ok(Some(metric))` will be returned, and the caller must
    /// call `flush` before attempting to encode the same metric again. Otherwise, `Ok(None)` is returned.
    ///
    /// ## Errors
    ///
    /// If the given metric is not valid for the endpoint this request builder is configured for, or if there is an
    /// error during compression of the encoded metric, an error will be returned.
    pub async fn encode(&mut self, metric: Metric) -> Result<Option<Metric>, RequestBuilderError> {
        // Make sure this metric is valid for the endpoint this request builder is configured for.
        let endpoint = MetricsEndpoint::from_metric(&metric);
        if endpoint != self.endpoint {
            return Err(RequestBuilderError::InvalidMetricForEndpoint {
                metric_type: metric.values().as_str(),
                endpoint: self.endpoint,
            });
        }

        // Encode the metric and then see if it will fit into the current request payload.
        //
        // If not, we return the original metric, signaling to the caller that they need to flush the current request
        // payload before encoding additional metrics.
        let encoded_metric = encode_single_metric(&metric);
        let previous_len = self.scratch_buf.len();
        encoded_metric.write(&mut self.scratch_buf)?;
        let encoded_len = self.scratch_buf.len() - previous_len;
        self.scratch_buf_lens.push(encoded_len);

        // If the metric can't fit into the current request payload based on the uncompressed size limit, or isn't
        // likely to fit into the current request payload based on the estimated compressed size limit, then return it
        // to the caller: this indicates that a flush must happen before trying to encode the same metric again.
        //
        // TODO: Use of the estimated compressed size limit is a bit of a stopgap to avoid having to do full incremental
        // request building. We can still improve it, but the only sure-fire way to not exceed the (un)compressed
        // payload size limits is to be able to re-do the encoding/compression process in smaller chunks.
        let new_uncompressed_len = self.uncompressed_len + encoded_len;
        if new_uncompressed_len > self.endpoint.uncompressed_size_limit()
            || self
                .compression_estimator
                .would_write_exceed_threshold(encoded_len, self.endpoint.compressed_size_limit())
        {
            trace!(
                encoded_len,
                uncompressed_len = self.uncompressed_len,
                estimated_compressed_len = self.compression_estimator.estimated_len(),
                endpoint = ?self.endpoint,
                "Metric would exceed endpoint size limits."
            );
            return Ok(Some(metric));
        }

        // Write the scratch buffer to the compressor.
        self.compressor
            .write_all(&self.scratch_buf[previous_len..])
            .await
            .context(Io)?;
        self.compression_estimator.track_write(&self.compressor, encoded_len);
        self.uncompressed_len += encoded_len;
        self.metrics_written += 1;

        trace!(
            encoded_len,
            uncompressed_len = self.uncompressed_len,
            estimated_compressed_len = self.compression_estimator.estimated_len(),
            "Wrote metric to compressor."
        );

        Ok(None)
    }

    /// Flushes the current request payload.
    ///
    /// This resets the internal state and prepares the request builder for further encoding. If there is no data to
    /// flush, this method will return `Ok(None)`.
    ///
    /// This attempts to split the request payload into two smaller payloads if the original request payload is too large.
    ///
    /// ## Errors
    ///
    /// If an error occurs while finalizing the compressor or creating the request, an error will be returned.
    pub async fn flush(&mut self) -> Vec<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError>> {
        if self.uncompressed_len == 0 {
            return vec![];
        }

        // Clear our internal state and finalize the compressor. We do it in this order so that if finalization fails,
        // somehow, the request builder is in a default state and encoding can be attempted again.
        let metrics_written = self.metrics_written;
        self.metrics_written = 0;

        let uncompressed_len = self.uncompressed_len;
        self.uncompressed_len = 0;

        self.compression_estimator.reset();

        let new_compressor = create_compressor(&self.buffer_pool).await;
        let mut compressor = std::mem::replace(&mut self.compressor, new_compressor);
        if let Err(e) = compressor.shutdown().await.context(Io) {
            self.clear_scratch_buffer();
            return vec![Err(e)];
        }

        let buffer = compressor.into_inner().freeze();

        let compressed_len = buffer.len();
        let compressed_limit = self.endpoint.compressed_size_limit();
        if compressed_len > compressed_limit {
            // Single metric is unable to be split.
            if self.scratch_buf_lens.len() == 1 {
                return vec![Err(RequestBuilderError::PayloadTooLarge {
                    compressed_size_bytes: compressed_len,
                    compressed_limit_bytes: compressed_limit,
                })];
            }

            return self.split_request().await;
        }

        debug!(endpoint = ?self.endpoint, uncompressed_len, compressed_len, "Flushing request.");

        self.clear_scratch_buffer();
        vec![self.create_request(buffer).map(|req| (metrics_written, req))]
    }

    fn clear_scratch_buffer(&mut self) {
        self.scratch_buf.clear();
        self.scratch_buf_lens.clear();
    }

    async fn split_request(&mut self) -> Vec<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError>> {
        let mut requests = Vec::new();

        if self.scratch_buf_lens.is_empty() {
            return requests;
        }

        let lens_pivot = self.scratch_buf_lens.len() / 2;
        let first_half_metrics_len = self.scratch_buf_lens.len() - lens_pivot;

        let scratch_buf_pivot = self.scratch_buf_lens.iter().take(first_half_metrics_len).sum();
        assert!(scratch_buf_pivot < self.scratch_buf.len());

        let first_half_scratch_buf = &self.scratch_buf[0..scratch_buf_pivot];
        let second_half_scratch_buf = &self.scratch_buf[scratch_buf_pivot..];

        let mut compressor_half_one = create_compressor(&self.buffer_pool).await;

        if let Err(e) = compressor_half_one.write_all(first_half_scratch_buf).await.context(Io) {
            requests.push(Err(e));
        }
        match self.finalize(compressor_half_one).await {
            Ok(buffer) => requests.push(self.create_request(buffer).map(|req| (1, req))),
            Err(e) => requests.push(Err(e)),
        }

        let mut compressor_half_two = create_compressor(&self.buffer_pool).await;
        if let Err(e) = compressor_half_two.write_all(second_half_scratch_buf).await.context(Io) {
            requests.push(Err(e));
        }
        match self.finalize(compressor_half_two).await {
            Ok(buffer) => requests.push(self.create_request(buffer).map(|req| (1, req))),
            Err(e) => requests.push(Err(e)),
        }
        self.clear_scratch_buffer();
        requests
    }

    async fn finalize(
        &self, mut compressor: Compressor<ChunkedBytesBuffer<O>>,
    ) -> Result<FrozenChunkedBytesBuffer, RequestBuilderError> {
        compressor.shutdown().await.context(Io)?;
        let buffer = compressor.into_inner().freeze();
        let compressed_len = buffer.len();
        let compressed_limit = self.endpoint.compressed_size_limit();
        if compressed_len > compressed_limit {
            return Err(RequestBuilderError::PayloadTooLarge {
                compressed_size_bytes: compressed_len,
                compressed_limit_bytes: compressed_limit,
            });
        }
        Ok(buffer)
    }

    fn create_request(
        &self, buffer: FrozenChunkedBytesBuffer,
    ) -> Result<Request<FrozenChunkedBytesBuffer>, RequestBuilderError> {
        Request::builder()
            .method(Method::POST)
            .uri(self.endpoint_uri.clone())
            .header(http::header::CONTENT_TYPE, CONTENT_TYPE_PROTOBUF.clone())
            .header(http::header::CONTENT_ENCODING, CONTENT_ENCODING_DEFLATE.clone())
            .body(buffer)
            .context(Http)
    }
}

async fn create_compressor<O>(buffer_pool: &ChunkedBytesBufferObjectPool<O>) -> Compressor<ChunkedBytesBuffer<O>>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    let write_buffer = buffer_pool.acquire().await;
    Compressor::from_scheme(CompressionScheme::zlib_default(), write_buffer)
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

    fn write(&self, buf: &mut Vec<u8>) -> Result<(), RequestBuilderError> {
        let mut output_stream = CodedOutputStream::vec(buf);

        // Write the field tag.
        let field_number = self.field_number();
        output_stream
            .write_tag(field_number, protobuf::rt::WireType::LengthDelimited)
            .context(FailedToEncode)?;

        // Write the message.
        match self {
            Self::Series(series) => output_stream.write_message_no_tag(series).context(FailedToEncode)?,
            Self::Sketch(sketch) => output_stream.write_message_no_tag(sketch).context(FailedToEncode)?,
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

    metric.context().visit_tags(|tag| {
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

    metric.context().visit_tags(|tag| {
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
    use saluki_event::metric::Metric;

    use super::encode_sketch_metric;

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
}
