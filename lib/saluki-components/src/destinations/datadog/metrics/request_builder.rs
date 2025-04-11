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
use tracing::{debug, error, trace};

pub(super) const SCRATCH_BUF_CAPACITY: usize = 8192;

static CONTENT_TYPE_PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");

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
    compressed_len_limit: usize,
    uncompressed_len_limit: usize,
    max_metrics_per_payload: usize,
    encoded_metrics: Vec<Metric>,
    compression_scheme: CompressionScheme,
}

impl<O> RequestBuilder<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    /// Creates a new `RequestBuilder` for the given endpoint.
    pub async fn new(
        endpoint: MetricsEndpoint, buffer_pool: O, max_metrics_per_payload: usize,
        compression_scheme: CompressionScheme,
    ) -> Result<Self, RequestBuilderError> {
        let chunked_buffer_pool = ChunkedBytesBufferObjectPool::new(buffer_pool);
        let compressor = create_compressor(&chunked_buffer_pool, compression_scheme).await;
        Ok(Self {
            endpoint,
            endpoint_uri: endpoint.endpoint_uri(),
            buffer_pool: chunked_buffer_pool,
            scratch_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
            compressor,
            compression_estimator: CompressionEstimator::default(),
            uncompressed_len: 0,
            compressed_len_limit: endpoint.compressed_size_limit(),
            uncompressed_len_limit: endpoint.uncompressed_size_limit(),
            max_metrics_per_payload,
            encoded_metrics: Vec::new(),
            compression_scheme,
        })
    }

    /// Configures custom (un)compressed length limits for the request builder.
    ///
    /// Used specifically for testing purposes.
    #[cfg(test)]
    fn set_custom_len_limits(&mut self, uncompressed_len_limit: usize, compressed_len_limit: usize) {
        self.uncompressed_len_limit = uncompressed_len_limit;
        self.compressed_len_limit = compressed_len_limit;
    }

    /// Attempts to encode a metric and write it to the current request payload.
    ///
    /// If the metric can't be encoded due to size constraints, `Ok(Some(metric))` will be returned, and the caller must
    /// call `flush` before attempting to encode the same metric again. Otherwise, `Ok(None)` is returned.
    ///
    /// # Errors
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

        // Make sure we haven't hit the maximum number of metrics per payload.
        if self.encoded_metrics.len() >= self.max_metrics_per_payload {
            return Ok(Some(metric));
        }

        // Encode the metric and then see if it will fit into the current request payload.
        //
        // If not, we return the original metric, signaling to the caller that they need to flush the current request
        // payload before encoding additional metrics.
        let encoded_metric = encode_single_metric(&metric);
        encoded_metric.write(&mut self.scratch_buf)?;

        // If the metric can't fit into the current request payload based on the uncompressed size limit, or isn't
        // likely to fit into the current request payload based on the estimated compressed size limit, then return it
        // to the caller: this indicates that a flush must happen before trying to encode the same metric again.
        let encoded_len = self.scratch_buf.len();
        let new_uncompressed_len = self.uncompressed_len + encoded_len;
        if new_uncompressed_len > self.uncompressed_len_limit
            || self
                .compression_estimator
                .would_write_exceed_threshold(encoded_len, self.compressed_len_limit)
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
        self.compressor.write_all(&self.scratch_buf[..]).await.context(Io)?;
        self.compression_estimator.track_write(&self.compressor, encoded_len);
        self.uncompressed_len += encoded_len;
        self.encoded_metrics.push(metric);

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
    /// # Errors
    ///
    /// If an error occurs while finalizing the compressor or creating the request, an error will be returned.
    pub async fn flush(&mut self) -> Vec<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError>> {
        if self.uncompressed_len == 0 {
            return vec![];
        }

        // Clear our internal state and finalize the compressor. We do it in this order so that if finalization fails,
        // somehow, the request builder is in a default state and encoding can be attempted again.
        let uncompressed_len = self.uncompressed_len;
        self.uncompressed_len = 0;

        self.compression_estimator.reset();

        let new_compressor = create_compressor(&self.buffer_pool, self.compression_scheme).await;
        let mut compressor = std::mem::replace(&mut self.compressor, new_compressor);
        if let Err(e) = compressor.flush().await.context(Io) {
            let metrics_dropped = self.clear_encoded_metrics();

            // TODO: Propagate the number of metrics dropped in the returned error itself rather than logging here.
            error!(
                metrics_dropped,
                "Failed to finalize compressor while building request. Metrics have been dropped."
            );

            return vec![Err(e)];
        }

        if let Err(e) = compressor.shutdown().await.context(Io) {
            let metrics_dropped = self.clear_encoded_metrics();

            // TODO: Propagate the number of metrics dropped in the returned error itself rather than logging here.
            error!(
                metrics_dropped,
                "Failed to finalize compressor while building request. Metrics have been dropped."
            );

            return vec![Err(e)];
        }

        let buffer = compressor.into_inner().freeze();

        let compressed_len = buffer.len();
        let compressed_limit = self.compressed_len_limit;
        if compressed_len > compressed_limit {
            // Single metric is unable to be split.
            if self.encoded_metrics.len() == 1 {
                let _ = self.clear_encoded_metrics();

                return vec![Err(RequestBuilderError::PayloadTooLarge {
                    compressed_size_bytes: compressed_len,
                    compressed_limit_bytes: compressed_limit,
                })];
            }

            return self.split_request().await;
        }

        let metrics_written = self.clear_encoded_metrics();
        debug!(endpoint = ?self.endpoint, uncompressed_len, compressed_len, metrics_written, "Flushing request.");

        vec![self.create_request(buffer).map(|req| (metrics_written, req))]
    }

    fn clear_encoded_metrics(&mut self) -> usize {
        let len = self.encoded_metrics.len();
        self.encoded_metrics.clear();
        len
    }

    async fn split_request(&mut self) -> Vec<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError>> {
        // Nothing to do if we have no encoded metrics.
        let mut requests = Vec::new();
        if self.encoded_metrics.is_empty() {
            return requests;
        }

        // We're going to attempt to split all of the previously-encoded metrics between two _new_ compressed payloads,
        // with the goal that each payload will be under the compressed size limit.
        //
        // We achieve this by temporarily consuming the "encoded metrics" buffer, feeding the first half of it back to
        // ourselves by re-encoding and then flushing, and then doing the same thing with the second half.  If either
        // half fails to properly encode, we give up entirely.
        //
        // We specifically manage the control flow so that we always restore the original "encoded metrics" buffer to
        // the builder (albeit cleared) before returning, so that we don't waste its allocation as it's been sized up
        // over time.
        //
        // We can do this by swapping it out with a new `Vec<Metric>` since empty vectors don't allocate at all.
        let mut encoded_metrics = std::mem::take(&mut self.encoded_metrics);
        let encoded_metrics_pivot = encoded_metrics.len() / 2;

        let first_half_encoded_metrics = &encoded_metrics[0..encoded_metrics_pivot];
        let second_half_encoded_metrics = &encoded_metrics[encoded_metrics_pivot..];

        // TODO: We're duplicating functionality here between `encode`/`flush`, but this makes it a lot easier to skip
        // over the normal behavior that would do all the storing of encoded metrics, trying to split the payload, etc,
        // since we want to avoid that and avoid any recursion in general.
        //
        // We should consider if there's a better way to split out some of this into common methods or something.
        if let Some(request) = self.try_split_request(first_half_encoded_metrics).await {
            requests.push(request);
        }

        if let Some(request) = self.try_split_request(second_half_encoded_metrics).await {
            requests.push(request);
        }

        // Restore our original "encoded metrics" buffer before finishing up, but also clear it.
        encoded_metrics.clear();
        self.encoded_metrics = encoded_metrics;

        requests
    }

    async fn try_split_request(
        &mut self, metrics: &[Metric],
    ) -> Option<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError>> {
        let mut uncompressed_len = 0;
        let mut compressor = create_compressor(&self.buffer_pool, self.compression_scheme).await;

        for metric in metrics {
            // Encode each metric and write it to our compressor.
            //
            // We skip any of the typical payload size checks here, because we already know we at least fit these
            // metrics into the previous attempted payload, so there's no reason to redo all of that here.
            let encoded_metric = encode_single_metric(metric);

            if let Err(e) = encoded_metric.write(&mut self.scratch_buf) {
                return Some(Err(e));
            }

            if let Err(e) = compressor.write_all(&self.scratch_buf[..]).await.context(Io) {
                return Some(Err(e));
            }

            uncompressed_len += self.scratch_buf.len();
        }

        // Make sure we haven't exceeded our uncompressed size limit.
        //
        // Again, this should never happen since we've already gone through this the first time but we're just being
        // extra sure here since the interface allows for it to happen. :shrug:
        if uncompressed_len > self.uncompressed_len_limit {
            let metrics_dropped = metrics.len();

            // TODO: Propagate the number of metrics dropped in the returned error itself rather than logging here.
            error!(uncompressed_len, metrics_dropped, "Uncompressed size limit exceeded while splitting request. This should never occur. Metrics have been dropped.");

            return None;
        }

        Some(
            self.finalize(compressor)
                .await
                .and_then(|buffer| self.create_request(buffer).map(|request| (metrics.len(), request))),
        )
    }

    async fn finalize(
        &self, mut compressor: Compressor<ChunkedBytesBuffer<O>>,
    ) -> Result<FrozenChunkedBytesBuffer, RequestBuilderError> {
        compressor.shutdown().await.context(Io)?;
        let buffer = compressor.into_inner().freeze();
        let compressed_len = buffer.len();
        let compressed_limit = self.compressed_len_limit;
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
            .header(http::header::CONTENT_ENCODING, self.compressor.header_value())
            .body(buffer)
            .context(Http)
    }
}

async fn create_compressor<O>(
    buffer_pool: &ChunkedBytesBufferObjectPool<O>, compression_scheme: CompressionScheme,
) -> Compressor<ChunkedBytesBuffer<O>>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    let write_buffer = buffer_pool.acquire().await;
    Compressor::from_scheme(compression_scheme, write_buffer)
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
        buf.clear();
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
    use saluki_core::pooling::FixedSizeObjectPool;
    use saluki_event::metric::Metric;
    use saluki_io::{
        buf::{BytesBuffer, FixedSizeVec},
        compression::CompressionScheme,
    };

    use super::{encode_sketch_metric, MetricsEndpoint, RequestBuilder};

    fn create_request_builder_buffer_pool() -> FixedSizeObjectPool<BytesBuffer> {
        FixedSizeObjectPool::with_builder("test_pool", 8, || FixedSizeVec::with_capacity(64))
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

        let histogram_payload = encode_sketch_metric(&histogram);
        let distribution_payload = encode_sketch_metric(&distribution);

        assert_eq!(histogram_payload, distribution_payload);
    }

    #[tokio::test]
    async fn split_oversized_request() {
        // Generate some metrics that will exceed the compressed size limit.
        let counter1 = Metric::counter(("abcdefg", &["345", "678"][..]), 1.0);
        let counter2 = Metric::counter(("hijklmn", &["9!@", "#$%"][..]), 1.0);
        let counter3 = Metric::counter(("opqrstu", &["^&*", "()A"][..]), 1.0);
        let counter4 = Metric::counter(("vwxyz12", &["BCD", "EFG"][..]), 1.0);

        // Create a regular ol' request builder with normal (un)compressed size limits.
        let buffer_pool = create_request_builder_buffer_pool();
        let mut request_builder = RequestBuilder::new(
            MetricsEndpoint::Series,
            buffer_pool,
            usize::MAX,
            CompressionScheme::zstd_default(),
        )
        .await
        .expect("should not fail to create request builder");

        // Encode the metrics, which should all fit into the request payload.
        let metrics = vec![counter1, counter2, counter3, counter4];
        for metric in metrics {
            match request_builder.encode(metric).await {
                Ok(None) => {}
                Ok(Some(_)) => panic!("initial encode should never fail to fit encoded metric payload"),
                Err(e) => panic!("initial encode should never fail: {}", e),
            }
        }

        // Now we attempt to flush, but first, we'll adjust our limits to force the builder to split the request.
        //
        // We've chosen 96 because it's just under where the compressor should land when compressing all four metrics.
        // This value may need to change in the future if we change to a different compression algorithm.
        request_builder.set_custom_len_limits(MetricsEndpoint::Series.compressed_size_limit(), 96);
        let requests = request_builder.flush().await;
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn obeys_max_metrics_per_payload() {
        // Generate some simple metrics.
        let counter1 = Metric::counter(("abcdefg", &["345", "678"][..]), 1.0);
        let counter2 = Metric::counter(("hijklmn", &["9!@", "#$%"][..]), 1.0);
        let counter3 = Metric::counter(("opqrstu", &["^&*", "()A"][..]), 1.0);

        // Create a regular ol' request builder with normal (un)compressed size limits, and no limit on the number of
        // metrics per payload.
        //
        // We should be able to encode the three metrics without issue.
        let buffer_pool = create_request_builder_buffer_pool();
        let mut request_builder = RequestBuilder::new(
            MetricsEndpoint::Series,
            buffer_pool,
            usize::MAX,
            CompressionScheme::zstd_default(),
        )
        .await
        .expect("should not fail to create request builder");

        assert_eq!(None, request_builder.encode(counter1.clone()).await.unwrap());
        assert_eq!(None, request_builder.encode(counter2.clone()).await.unwrap());
        assert_eq!(None, request_builder.encode(counter3.clone()).await.unwrap());

        // Now create a request builder with normal (un)compressed size limits, but a limit of 2 metrics per payload.
        //
        // We should only be able to encode two of the three metrics before we're signaled to flush.
        let buffer_pool = create_request_builder_buffer_pool();
        let mut request_builder = RequestBuilder::new(
            MetricsEndpoint::Series,
            buffer_pool,
            2,
            CompressionScheme::zstd_default(),
        )
        .await
        .expect("should not fail to create request builder");

        assert_eq!(None, request_builder.encode(counter1.clone()).await.unwrap());
        assert_eq!(None, request_builder.encode(counter2.clone()).await.unwrap());
        assert_eq!(Some(counter3.clone()), request_builder.encode(counter3).await.unwrap());

        // Since we know we could fit the same three metrics in the first request builder when there was no limit on the
        // number of metrics per payload, we know we're not being instructed to flush here due to hitting (un)compressed
        // size limits.
    }
}
