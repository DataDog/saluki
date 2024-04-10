use std::{io, time::Duration};

use datadog_protos::metrics::{self as proto, Resource};
use http::{Method, Request, Uri};
use protobuf::CodedOutputStream;
use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tracing::debug;

use saluki_core::buffers::BufferPool;
use saluki_event::metric::*;
use saluki_io::{buf::ChunkedBytesBuffer, compression::*};

const SCRATCH_BUF_CAPACITY: usize = 8192;

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
        match &metric.value {
            MetricValue::Counter { .. }
            | MetricValue::Rate { .. }
            | MetricValue::Gauge { .. }
            | MetricValue::Set { .. } => Self::Series,
            MetricValue::Distribution { .. } => Self::Sketches,
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

pub struct RequestBuilder<B>
where
    B: BufferPool<Buffer = ChunkedBytesBuffer>,
{
    api_key: String,
    api_uri: Uri,
    endpoint: MetricsEndpoint,
    buffer_pool: B,
    scratch_buf: Vec<u8>,
    compressor: Compressor<ChunkedBytesBuffer>,
    written_uncompressed: usize,
}

impl<B> RequestBuilder<B>
where
    B: BufferPool<Buffer = ChunkedBytesBuffer>,
{
    pub async fn new(
        api_key: String, api_base_uri: Uri, endpoint: MetricsEndpoint, buffer_pool: B,
    ) -> Result<Self, RequestBuilderError> {
        let compressor = create_compressor(&buffer_pool).await;
        let api_uri = build_uri_for_endpoint(api_base_uri, endpoint)?;

        Ok(Self {
            api_key,
            api_uri,
            endpoint,
            buffer_pool,
            scratch_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
            compressor,
            written_uncompressed: 0,
        })
    }

    pub async fn encode(&mut self, metric: Metric) -> Result<Option<Metric>, RequestBuilderError> {
        // Make sure this metric is valid for the endpoint this request builder is configured for.
        let endpoint = MetricsEndpoint::from_metric(&metric);
        if endpoint != self.endpoint {
            return Err(RequestBuilderError::InvalidMetricForEndpoint {
                metric_type: metric.value.as_str(),
                endpoint: self.endpoint,
            });
        }

        // Encode the metric and then see if it will fit into the current request payload.
        //
        // If not, we return the original metric, signaling to the caller that they need to flush the current request
        // payload before encoding additional metrics.
        let encoded_metric = encode_single_metric(&metric);
        encoded_metric.write(&mut self.scratch_buf)?;

        // TODO: Figure out if `ZlibEncoder::total_out` (and friends) actually track the number of bytes being produced
        // by the compressor prior to flushing, insofar as understanding what the final output size of the compressor
        // would be if we immediately called `flush`.
        //
        // This would give us a fairly accurate way to answer the question of: if we assume the current metric is
        // written and cannot be compressed at all, what would the resulting compressed output size be? And would that
        // exceed the compressed size limit?
        //
        // Future work might be able to improve on that with heuristics based on the current compression ratio to be
        // more risky with writing more into the current request payload, but even just the above would be a useful
        // invariant to have, and would allow us to be more confident that we're generating request payloads that fit
        // within the limits.

        // If the metric can't fit into the current request payload, in terms of the uncompressed size limit, then
        // return it to the caller to signal that they need to flush the current request payload first.
        if self.written_uncompressed + self.scratch_buf.len() > self.endpoint.uncompressed_size_limit() {
            return Ok(Some(metric));
        }

        // Write the scratch buffer to the compressor.
        //
        // We do a small bit of looping to extend our chunked buffer as necessary while writing our scratch buffer to
        // the compressor.
        self.compressor.write_all(&self.scratch_buf).await.context(Io)?;
        self.written_uncompressed += self.scratch_buf.len();

        Ok(None)
    }

    pub async fn flush(&mut self) -> Result<Option<Request<ChunkedBytesBuffer>>, RequestBuilderError> {
        if self.written_uncompressed == 0 {
            return Ok(None);
        }

        // Finalize the compressor and then reset our state to take ownership of it.
        let new_compressor = create_compressor(&self.buffer_pool).await;
        let mut old_compressor = std::mem::replace(&mut self.compressor, new_compressor);
        old_compressor.shutdown().await.context(Io)?;
        let old_buffer = old_compressor.into_inner();

        let compressed_limit = self.endpoint.compressed_size_limit();
        if old_buffer.len() > compressed_limit {
            return Err(RequestBuilderError::PayloadTooLarge {
                compressed_size_bytes: old_buffer.len(),
                compressed_limit_bytes: compressed_limit,
            });
        }

        let written_uncompressed = self.written_uncompressed;
        self.written_uncompressed = 0;

        debug!(endpoint = ?self.endpoint, uncompressed_len = written_uncompressed, compressed_len = old_buffer.len(), "Flushing request.");

        self.create_request(old_buffer).map(Some)
    }

    fn create_request(&self, buffer: ChunkedBytesBuffer) -> Result<Request<ChunkedBytesBuffer>, RequestBuilderError> {
        Request::builder()
            .method(Method::POST)
            .uri(self.api_uri.clone())
            .header("Content-Type", "application/x-protobuf")
            .header("Content-Encoding", "deflate")
            .header("DD-API-KEY", self.api_key.clone())
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

async fn create_compressor<B>(buffer_pool: &B) -> Compressor<ChunkedBytesBuffer>
where
    B: BufferPool<Buffer = ChunkedBytesBuffer>,
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
        // numbers changing is not backwards-compatible  and should basically never, ever happen.
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
    match &metric.value {
        MetricValue::Counter { .. }
        | MetricValue::Rate { .. }
        | MetricValue::Gauge { .. }
        | MetricValue::Set { .. } => EncodedMetric::Series(encode_series_metric(metric)),
        MetricValue::Distribution { .. } => EncodedMetric::Sketch(encode_sketch_metric(metric)),
    }
}

fn encode_series_metric(metric: &Metric) -> proto::MetricSeries {
    let mut series = proto::MetricSeries::new();
    series.set_metric(metric.context.name.clone().into());

    // Set our tags.
    //
    // This involves extracting some specific tags first that have to be set on dedicated fields (host, resources, etc)
    // and then setting the rest as generic tags.
    let mut tags = metric.context.tags.clone();

    let host = tags
        .remove_tag("host")
        .and_then(|tag| tag.into_values().next())
        .unwrap_or_default();
    let mut host_resource = Resource::new();
    host_resource.set_type("host".to_string().into());
    host_resource.set_name(host.into());
    series.mut_resources().push(host_resource);

    if let Some(internal_resource_tag) = tags.remove_tag("dd.internal.resource") {
        let tag_values = internal_resource_tag.into_values();
        for tag_value in tag_values {
            if let Some((resource_type, resource_name)) = tag_value.split_once(':') {
                let mut resource = Resource::new();
                resource.set_type(resource_type.into());
                resource.set_name(resource_name.into());
                series.mut_resources().push(resource);
            }
        }
    }

    // TODO: For tags with multiple values, should we actually be emitting one `key:value` pair per value? I'm guessing
    // the answer is "yes".
    series.set_tags(tags.into_iter().map(|tag| tag.into_string().into()).collect());

    // Set the origin metadata, if it exists.
    if let Some(origin) = &metric.metadata.origin {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                series.set_source_type_name(source_type.clone().into());
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

    let (metric_type, metric_value, maybe_interval) = match &metric.value {
        MetricValue::Counter { value } => (proto::MetricType::COUNT, *value, None),
        MetricValue::Rate { value, interval } => (proto::MetricType::RATE, *value, Some(duration_to_secs(*interval))),
        MetricValue::Gauge { value } => (proto::MetricType::GAUGE, *value, None),
        MetricValue::Set { values } => (proto::MetricType::GAUGE, values.len() as f64, None),
        _ => unreachable!(),
    };

    series.set_type(metric_type);

    let mut point = proto::MetricPoint::new();
    point.set_value(metric_value);
    point.set_timestamp(metric.metadata.timestamp as i64);

    series.mut_points().push(point);

    if let Some(interval) = maybe_interval {
        series.set_interval(interval);
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
    sketch.set_metric(metric.context.name.clone().into());

    let mut tags = metric.context.tags.clone();
    let host = tags
        .remove_tag("host")
        .and_then(|tag| tag.into_values().next())
        .unwrap_or_default();

    sketch.set_host(host.into());

    // TODO: For tags with multiple values, should we actually be emitting one `key:value` pair per value? I'm guessing
    // the answer is "yes".
    sketch.set_tags(tags.into_iter().map(|tag| tag.into_string().into()).collect());

    // Set the origin metadata, if it exists.
    if let Some(MetricOrigin::OriginMetadata {
        product,
        subproduct,
        product_detail,
    }) = &metric.metadata.origin
    {
        sketch.set_metadata(origin_metadata_to_proto_metadata(
            *product,
            *subproduct,
            *product_detail,
        ));
    }

    let ddsketch = match &metric.value {
        MetricValue::Distribution { sketch: ddsketch } => ddsketch,
        _ => unreachable!(),
    };

    let mut dogsketch = proto::Dogsketch::new();
    dogsketch.set_ts(metric.metadata.timestamp as i64);
    ddsketch.merge_to_dogsketch(&mut dogsketch);
    sketch.mut_dogsketches().push(dogsketch);

    sketch
}

fn origin_metadata_to_proto_metadata(product: u32, subproduct: u32, product_detail: u32) -> proto::Metadata {
    // NOTE: The naming discrepancies here -- category vs subproduct and service vs product detail -- are a consequence
    // of how these fields are named in the Datadog Platform, and the Protocol Buffers definitions used by the Datadog
    // Agent -- which we build off of -- has not yet caught up with renaming them to match.
    let mut metadata = proto::Metadata::new();
    let proto_origin = metadata.mut_origin();
    proto_origin.set_origin_product(product);
    proto_origin.set_origin_category(subproduct);
    proto_origin.set_origin_service(product_detail);
    metadata
}

fn duration_to_secs(duration: Duration) -> i64 {
    duration.as_secs_f64() as i64
}
