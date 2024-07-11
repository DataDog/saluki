use std::{io, time::Duration};

use datadog_protos::piecemeal_include::datadog::agentpayload::{
    mod_MetricPayload::{MetricSeriesBuilder, MetricType},
    mod_SketchPayload::SketchBuilder,
};
use http::{Method, Request, Uri};
use piecemeal::{ScratchBuffer, ScratchWriter};
use saluki_env::time::get_unix_timestamp;
use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, trace};

use saluki_core::pooling::ObjectPool;
use saluki_event::metric::*;
use saluki_io::{buf::ChunkedBytesBuffer, compression::*};

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
    FailedToEncode { source: piecemeal::Error },
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

impl From<piecemeal::Error> for RequestBuilderError {
    fn from(source: piecemeal::Error) -> Self {
        Self::FailedToEncode { source }
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
        match metric.value() {
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
    B: ObjectPool<Item = ChunkedBytesBuffer>,
{
    api_key: String,
    api_uri: Uri,
    endpoint: MetricsEndpoint,
    buffer_pool: B,
    // TODO: we probably just want two scratch buffers: one for the writer and one that we finish
    // into so the rest of the existing compressor logic can stay as it is and we can avoid having
    // to bake async write support into `piecemeal`
    scratch_writer: ScratchWriter<Vec<u8>>,
    scratch_output_buf: Vec<u8>,
    compressor: Compressor<ChunkedBytesBuffer>,
    compression_estimator: CompressionEstimator,
    uncompressed_len: usize,
    metrics_written: usize,
}

impl<B> RequestBuilder<B>
where
    B: ObjectPool<Item = ChunkedBytesBuffer>,
{
    /// Creates a new `RequestBuilder` for the given endpoint, using the specified API key and base URI.
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
            scratch_writer: ScratchWriter::new(Vec::with_capacity(SCRATCH_BUF_CAPACITY)),
            scratch_output_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
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
                metric_type: metric.value().as_str(),
                endpoint: self.endpoint,
            });
        }

        //println!("metric: {:?}", metric);

        // Encode the metric and then see if it will fit into the current request payload.
        //
        // If not, we return the original metric, signaling to the caller that they need to flush the current request
        // payload before encoding additional metrics.
        encode_single_metric(&metric, &mut self.scratch_writer)?;
        self.scratch_writer.finish(&mut self.scratch_output_buf, false)?;

        // If the metric can't fit into the current request payload based on the uncompressed size limit, or isn't
        // likely to fit into the current request payload based on the estimated compressed size limit, then return it
        // to the caller: this indicates that a flush must happen before trying to encode the same metric again.
        //
        // TODO: Use of the estimated compressed size limit is a bit of a stopgap to avoid having to do full incremental
        // request building. We can still improve it, but the only sure-fire way to not exceed the (un)compressed
        // payload size limits is to be able to re-do the encoding/compression process in smaller chunks.
        let encoded_len = self.scratch_output_buf.len();
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
        self.compressor.write_all(&self.scratch_output_buf).await.context(Io)?;
        self.compression_estimator
            .track_write(&self.compressor, self.scratch_output_buf.len());
        self.uncompressed_len += self.scratch_output_buf.len();
        self.metrics_written += 1;
        self.scratch_output_buf.clear();

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
    pub async fn flush(&mut self) -> Result<Option<(usize, Request<ChunkedBytesBuffer>)>, RequestBuilderError> {
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

    fn create_request(&self, buffer: ChunkedBytesBuffer) -> Result<Request<ChunkedBytesBuffer>, RequestBuilderError> {
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

async fn create_compressor<B>(buffer_pool: &B) -> Compressor<ChunkedBytesBuffer>
where
    B: ObjectPool<Item = ChunkedBytesBuffer>,
{
    let write_buffer = buffer_pool.acquire().await;
    Compressor::from_scheme(CompressionScheme::zlib_default(), write_buffer)
}

fn encode_single_metric<S: ScratchBuffer>(
    metric: &Metric, scratch_writer: &mut ScratchWriter<S>,
) -> Result<(), RequestBuilderError> {
    match metric.value() {
        MetricValue::Counter { .. }
        | MetricValue::Rate { .. }
        | MetricValue::Gauge { .. }
        | MetricValue::Set { .. } => encode_series_metric(metric, scratch_writer),
        MetricValue::Distribution { .. } => encode_sketch_metric(metric, scratch_writer),
    }
}

fn encode_series_metric<S: ScratchBuffer>(
    metric: &Metric, scratch_writer: &mut ScratchWriter<S>,
) -> Result<(), RequestBuilderError> {
    let mut series_builder = MetricSeriesBuilder::new(scratch_writer);
    series_builder.metric(metric.context().name())?;

    for tag in metric.context().tags() {
        // If the tag is an internal resource tag, we actually write it as a resource entry and not
        // a tag. Otherwise... just write it as a tag.
        if tag.name() == "dd.internal.resource" {
            if let Some((resource_type, resource_name)) = tag.value().and_then(|s| s.split_once(':')) {
                series_builder.add_resources(|resource_builder| {
                    resource_builder.type_pb(resource_type)?.name(resource_name)?;
                    Ok(())
                })?;
            }
        } else {
            series_builder.tags(|tags_builder| tags_builder.add(tag))?;
        }
    }

    series_builder.add_resources(|resource_builder| {
        resource_builder
            .type_pb("host")?
            .name(metric.metadata().hostname().unwrap_or_default())?;
        Ok(())
    })?;

    // Set the origin metadata, if it exists.
    if let Some(origin) = metric.metadata().origin() {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                series_builder.source_type_name(source_type)?;
            }
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => {
                series_builder.metadata(|metadata_builder| {
                    metadata_builder.origin(|origin_builder| {
                        origin_builder
                            .origin_product(*product)?
                            .origin_category(*subproduct)?
                            .origin_service(*product_detail)?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
            }
        }
    }

    let (metric_type, metric_value, maybe_interval) = match metric.value() {
        MetricValue::Counter { value } => (MetricType::COUNT, *value, None),
        MetricValue::Rate { value, interval } => (MetricType::RATE, *value, Some(duration_to_secs(*interval))),
        MetricValue::Gauge { value } => (MetricType::GAUGE, *value, None),
        MetricValue::Set { values } => (MetricType::GAUGE, values.len() as f64, None),
        _ => unreachable!(),
    };

    series_builder.type_pb(metric_type)?;
    series_builder.add_points(|point_builder| {
        point_builder
            .value(metric_value)?
            .timestamp(metric.metadata().timestamp().unwrap_or_else(get_unix_timestamp) as i64)?;
        Ok(())
    })?;

    if let Some(interval) = maybe_interval {
        series_builder.interval(interval)?;
    }

    Ok(())
}

fn encode_sketch_metric<S: ScratchBuffer>(
    metric: &Metric, scratch_writer: &mut ScratchWriter<S>,
) -> Result<(), RequestBuilderError> {
    let mut sketch_builder = SketchBuilder::new(scratch_writer);
    sketch_builder.metric(metric.context().name())?;
    sketch_builder.host(metric.metadata().hostname().unwrap_or_default())?;

    sketch_builder.tags(|tags_builder| tags_builder.add_many_mapped(metric.context().tags(), |t| &**t))?;

    // Set the origin metadata, if it exists.
    if let Some(MetricOrigin::OriginMetadata {
        product,
        subproduct,
        product_detail,
    }) = metric.metadata().origin()
    {
        sketch_builder.metadata(|metadata_builder| {
            metadata_builder.origin(|origin_builder| {
                origin_builder
                    .origin_product(*product)?
                    .origin_category(*subproduct)?
                    .origin_service(*product_detail)?;
                Ok(())
            })?;
            Ok(())
        })?;
    }

    let ddsketch = match metric.value() {
        MetricValue::Distribution { sketch: ddsketch } => ddsketch,
        _ => unreachable!(),
    };

    sketch_builder.add_dogsketches(|dogsketch_builder| {
        dogsketch_builder
            .ts(metric.metadata().timestamp().unwrap_or_else(get_unix_timestamp) as i64)?
            .cnt(ddsketch.count() as i64)?
            .min(ddsketch.min().unwrap_or_default())?
            .max(ddsketch.max().unwrap_or_default())?
            .sum(ddsketch.sum().unwrap_or_default())?
            .avg(ddsketch.avg().unwrap_or_default())?
            .k(|b| b.add_many_mapped(ddsketch.bins(), |bin| bin.key() as i32))?
            .n(|b| b.add_many_mapped(ddsketch.bins(), |bin| bin.count() as u32))?;

        Ok(())
    })?;

    Ok(())
}

fn duration_to_secs(duration: Duration) -> i64 {
    duration.as_secs_f64() as i64
}
