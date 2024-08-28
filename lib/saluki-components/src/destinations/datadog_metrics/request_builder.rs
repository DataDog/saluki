use std::io;

use datadog_protos::metrics::{self as proto, Resource};
use http::{Method, Request, Uri};
use protobuf::CodedOutputStream;
use saluki_core::pooling::ObjectPool;
use saluki_event::metric::*;
use saluki_io::{
    buf::{ChunkedBuffer, ChunkedBufferObjectPool, ReadWriteIoBuffer},
    compression::*,
};
use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, trace};

pub(super) const SCRATCH_BUF_CAPACITY: usize = 8192;

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
            MetricValues::Distribution(..) => Self::Sketches,
        }
    }

    /// Gets the path for this endpoint.
    pub const fn endpoint_path(&self) -> &'static str {
        match self {
            Self::Series => "/api/v2/series",
            Self::Sketches => "/api/beta/sketches",
        }
    }
}

pub struct RequestBuilder<O>
where
    O: ObjectPool + 'static,
    O::Item: ReadWriteIoBuffer,
{
    api_key: String,
    api_uri: Uri,
    endpoint: MetricsEndpoint,
    buffer_pool: ChunkedBufferObjectPool<O>,
    scratch_buf: Vec<u8>,
    compressor: Compressor<ChunkedBuffer<O>>,
    compression_estimator: CompressionEstimator,
    uncompressed_len: usize,
    metrics_written: usize,
}

impl<O> RequestBuilder<O>
where
    O: ObjectPool + 'static,
    O::Item: ReadWriteIoBuffer,
{
    /// Creates a new `RequestBuilder` for the given endpoint, using the specified API key and base URI.
    pub async fn new(
        api_key: String, api_base_uri: Uri, endpoint: MetricsEndpoint, buffer_pool: O,
    ) -> Result<Self, RequestBuilderError> {
        let chunked_buffer_pool = ChunkedBufferObjectPool::new(buffer_pool);
        let compressor = create_compressor(&chunked_buffer_pool).await;
        let api_uri = build_uri_for_endpoint(api_base_uri, endpoint)?;

        Ok(Self {
            api_key,
            api_uri,
            endpoint,
            buffer_pool: chunked_buffer_pool,
            scratch_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
            compressor,
            compression_estimator: CompressionEstimator::default(),
            uncompressed_len: 0,
            metrics_written: 0,
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
        encoded_metric.write(&mut self.scratch_buf)?;

        // If the metric can't fit into the current request payload based on the uncompressed size limit, or isn't
        // likely to fit into the current request payload based on the estimated compressed size limit, then return it
        // to the caller: this indicates that a flush must happen before trying to encode the same metric again.
        //
        // TODO: Use of the estimated compressed size limit is a bit of a stopgap to avoid having to do full incremental
        // request building. We can still improve it, but the only sure-fire way to not exceed the (un)compressed
        // payload size limits is to be able to re-do the encoding/compression process in smaller chunks.
        let encoded_len = self.scratch_buf.len();
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
        self.compressor.write_all(&self.scratch_buf).await.context(Io)?;
        self.compression_estimator
            .track_write(&self.compressor, self.scratch_buf.len());
        self.uncompressed_len += self.scratch_buf.len();
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
    /// ## Errors
    ///
    /// If an error occurs while finalizing the compressor or creating the request, an error will be returned.
    pub async fn flush(&mut self) -> Result<Option<(usize, Request<ChunkedBuffer<O>>)>, RequestBuilderError> {
        if self.uncompressed_len == 0 {
            return Ok(None);
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
        compressor.shutdown().await.context(Io)?;
        let buffer = compressor.into_inner();

        let compressed_len = buffer.len();
        let compressed_limit = self.endpoint.compressed_size_limit();
        if compressed_len > compressed_limit {
            return Err(RequestBuilderError::PayloadTooLarge {
                compressed_size_bytes: compressed_len,
                compressed_limit_bytes: compressed_limit,
            });
        }

        debug!(endpoint = ?self.endpoint, uncompressed_len, compressed_len, "Flushing request.");

        self.create_request(buffer).map(|req| Some((metrics_written, req)))
    }

    fn create_request(&self, buffer: ChunkedBuffer<O>) -> Result<Request<ChunkedBuffer<O>>, RequestBuilderError> {
        Request::builder()
            .method(Method::POST)
            .uri(self.api_uri.clone())
            .header("Content-Type", "application/x-protobuf")
            .header("Content-Encoding", "deflate")
            .header("DD-API-KEY", self.api_key.clone())
            // TODO: We can't access the version number of the package being built that _includes_ this library, so
            // using CARGO_PKG_VERSION or something like that would always be the version of `saluki-components`, which
            // isn't what we want... maybe we can figure out some way to shove it in a global somewhere or something?
            .header("DD-Agent-Version", "0.1.0")
            .header("User-Agent", "agent-data-plane/0.1.0")
            .body(buffer)
            .context(Http)
    }
}

fn build_uri_for_endpoint(api_base_uri: Uri, endpoint: MetricsEndpoint) -> Result<Uri, RequestBuilderError> {
    let mut builder = Uri::builder().path_and_query(endpoint.endpoint_path());

    if let Some(scheme) = api_base_uri.scheme() {
        builder = builder.scheme(scheme.clone());
    }

    if let Some(authority) = api_base_uri.authority() {
        builder = builder.authority(authority.clone());
    }

    builder.build().context(Http)
}

async fn create_compressor<O>(buffer_pool: &ChunkedBufferObjectPool<O>) -> Compressor<ChunkedBuffer<O>>
where
    O: ObjectPool + 'static,
    O::Item: ReadWriteIoBuffer,
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
        MetricValues::Distribution(..) => EncodedMetric::Sketch(encode_sketch_metric(metric)),
    }
}

fn encode_series_metric(metric: &Metric) -> proto::MetricSeries {
    let mut series = proto::MetricSeries::new();
    series.set_metric(metric.context().name().clone().into());

    // Set our tags.
    //
    // This involves extracting some specific tags first that have to be set on dedicated fields (host, resources, etc)
    // and then setting the rest as generic tags.
    let mut tags = metric.context().tags().clone();

    let mut host_resource = Resource::new();
    host_resource.set_type("host".to_string().into());
    host_resource.set_name(metric.metadata().hostname().map(|h| h.into()).unwrap_or_default());
    series.mut_resources().push(host_resource);

    if let Some(ir_tags) = tags.remove_tags("dd.internal.resource") {
        for ir_tag in ir_tags {
            if let Some((resource_type, resource_name)) = ir_tag.value().and_then(|s| s.split_once(':')) {
                let mut resource = Resource::new();
                resource.set_type(resource_type.into());
                resource.set_name(resource_name.into());
                series.mut_resources().push(resource);
            }
        }
    }

    series.set_tags(tags.into_iter().map(|tag| tag.into_inner().into()).collect());

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
    sketch.set_tags(
        metric
            .context()
            .tags()
            .into_iter()
            .cloned()
            .map(|tag| tag.into_inner().into())
            .collect(),
    );

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

    let sketches = match metric.values() {
        MetricValues::Distribution(sketches) => sketches,
        _ => unreachable!(),
    };

    for (timestamp, value) in sketches {
        let timestamp = timestamp.map(|ts| ts.get()).unwrap_or(0) as i64;

        let mut dogsketch = proto::Dogsketch::new();
        dogsketch.set_ts(timestamp);
        value.merge_to_dogsketch(&mut dogsketch);

        sketch.mut_dogsketches().push(dogsketch);
    }

    sketch
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
