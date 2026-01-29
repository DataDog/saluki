use std::time::Duration;

use async_trait::async_trait;
use ddsketch_agent::DDSketch;
use http::{HeaderValue, Method, Request};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::{
    buf::{ChunkedBytesBuffer, FrozenChunkedBytesBuffer},
    task::HandleExt as _,
};
use saluki_config::GenericConfiguration;
use saluki_context::tags::SharedTagSet;
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
use saluki_io::compression::{CompressionScheme, Compressor};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use tokio::{io::AsyncWriteExt as _, select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{
    common::datadog::{io::RB_BUFFER_CHUNK_SIZE, request_builder::RequestBuilder, telemetry::ComponentTelemetry},
    encoders::datadog::metrics::v2::MetricsEndpointEncoder,
};

mod endpoint;
use self::endpoint::{EndpointConfiguration, MetricsEndpoint};

mod v2;
mod v3;

const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";

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

    /// Enable V3 columnar format for series metrics.
    ///
    /// When enabled, series metrics (counters, gauges, rates, sets) are encoded using the V3 columnar format and sent
    /// to `/api/v3/series` instead of `/api/v2/series`.
    ///
    /// Defaults to `false`.
    #[serde(rename = "serializer_experimental_use_v3_api_series_enabled", default)]
    use_v3_series: bool,

    /// Enable V3 columnar format for sketch metrics.
    ///
    /// When enabled, sketch metrics (histograms, distributions) are encoded using the V3 columnar format and sent to
    /// `/api/v3/sketches` instead of `/api/beta/sketches`.
    ///
    /// Defaults to `false`.
    #[serde(rename = "serializer_experimental_use_v3_api_sketches_enabled", default)]
    use_v3_sketches: bool,

    /// Enable validation mode for V3 payloads.
    ///
    /// When enabled along with V3 series/sketches, both V2 and V3 payloads are generated and sent for the same metrics,
    /// allowing backend comparison for validation.
    ///
    /// Defaults to `false`.
    #[serde(rename = "serializer_experimental_use_v3_api_validate", default)]
    use_v3_validate: bool,

    /// Override compression level for V3 payloads.
    ///
    /// When set to a value > 0, this compression level is used specifically for V3 payloads
    /// instead of the default `zstd_compressor_level`.
    ///
    /// Defaults to 0 (use default compression level).
    #[serde(rename = "serializer_experimental_use_v3_api_compression_level", default)]
    v3_compression_level: i32,
}

impl DatadogMetricsConfiguration {
    /// Creates a new `DatadogMetricsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Sets additional tags to be applied uniformly to all metrics forwarded by this destination.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
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

        let v2_compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);
        let v3_compression_scheme = if self.v3_compression_level > 0 {
            CompressionScheme::new(&self.compressor_kind, self.v3_compression_level)
        } else {
            v2_compression_scheme
        };

        let v2_endpoint_config = EndpointConfiguration::new(
            v2_compression_scheme,
            self.max_metrics_per_payload,
            self.additional_tags.clone(),
        );
        let v3_endpoint_config = EndpointConfiguration::new(
            v3_compression_scheme,
            self.max_metrics_per_payload,
            self.additional_tags.clone(),
        );

        // Create our V2 request builders.
        let v2_series_builder = if !self.use_v3_series || self.use_v3_validate {
            let request_builder = v2::create_v2_request_builder(MetricsEndpoint::Series, &v2_endpoint_config)
                .await
                .error_context("Failed to create V2 series request builder.")?;
            Some(request_builder)
        } else {
            None
        };

        let v2_sketch_builder = if !self.use_v3_sketches || self.use_v3_validate {
            let request_builder = v2::create_v2_request_builder(MetricsEndpoint::Sketches, &v2_endpoint_config)
                .await
                .error_context("Failed to create V2 sketches request builder.")?;
            Some(request_builder)
        } else {
            None
        };

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        if self.use_v3_series || self.use_v3_sketches {
            debug!(
                v3_series = self.use_v3_series,
                v3_sketches = self.use_v3_sketches,
                v3_validation = self.use_v3_validate,
                "V3 encoding support is enabled."
            );
        }

        Ok(Box::new(DatadogMetrics {
            v2_series_builder,
            v2_sketch_builder,
            use_v3_series: self.use_v3_series,
            use_v3_sketches: self.use_v3_sketches,
            v3_endpoint_config,
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
    v2_series_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    v2_sketch_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    use_v3_series: bool,
    use_v3_sketches: bool,
    v3_endpoint_config: EndpointConfiguration,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl Encoder for DatadogMetrics {
    async fn run(mut self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        let Self {
            v2_series_builder,
            v2_sketch_builder,
            use_v3_series,
            use_v3_sketches,
            v3_endpoint_config,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our request builder task.
        let (events_tx, events_rx) = mpsc::channel(8);
        let (payloads_tx, mut payloads_rx) = mpsc::channel(8);
        let request_builder_fut = run_request_builder(
            v2_series_builder,
            v2_sketch_builder,
            use_v3_series,
            use_v3_sketches,
            v3_endpoint_config,
            telemetry,
            events_rx,
            payloads_tx,
            flush_timeout,
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
    mut v2_series_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>,
    mut v2_sketch_builder: Option<RequestBuilder<v2::MetricsEndpointEncoder>>, use_v3_series: bool,
    use_v3_sketches: bool, v3_endpoint_config: EndpointConfiguration, telemetry: ComponentTelemetry,
    mut events_rx: mpsc::Receiver<EventsBuffer>, mut payloads_tx: mpsc::Sender<PayloadsBuffer>,
    flush_timeout: Duration,
) -> Result<(), GenericError> {
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    // These vectors being present (or not present) are used not only to hold the metrics we need to encode, but to decide
    // whether we should encode them as V3 at all.
    let mut v3_series_metrics = use_v3_series.then(Vec::<Metric>::new);
    let mut v3_sketch_metrics = use_v3_sketches.then(Vec::<Metric>::new);

    let mut batch_id = None;
    let validation_enabled =
        (v2_series_builder.is_some() || v2_sketch_builder.is_some()) && (use_v3_series || use_v3_sketches);

    loop {
        // Ensure we have a validation batch UUID if validation is enabled.
        if validation_enabled && batch_id.is_none() {
            let new_batch_id = Uuid::now_v7();
            let new_batch_id = new_batch_id.as_hyphenated().to_string();
            match HeaderValue::from_str(&new_batch_id) {
                Ok(value) => batch_id = Some(value),
                Err(e) => {
                    debug!(error = %e, "Failed to generate validation batch UUID. Current batch will not be correlated on the backend.")
                }
            }
        }

        select! {
            Some(event_buffer) = events_rx.recv() => {
                for event in event_buffer {
                    let metric = match event.try_into_metric() {
                        Some(metric) => metric,
                        None => continue,
                    };

                    // Figure out which endpoint the metric belongs to, and grab the relevant V2 builder/V3 storage.
                    let endpoint = MetricsEndpoint::from_metric(&metric);
                    let (maybe_v2_builder, maybe_v3_metrics) = match endpoint {
                        MetricsEndpoint::Series => (&mut v2_series_builder, &mut v3_series_metrics),
                        MetricsEndpoint::Sketches => (&mut v2_sketch_builder, &mut v3_sketch_metrics),
                    };

                    // Store a copy of the metric in `maybe_v3_metrics` if it's present.
                    //
                    // We have to do this before encoding because `RequestBuilder::encode` consumes the metric. This also means we'll
                    // need to _remove_ the metric if encoding fails.
                    if let Some(metrics) = maybe_v3_metrics {
                        metrics.push(metric.clone());
                    }

                    // Attempt encoding the metric for V2 if configured.
                    //
                    // If the metric couldn't be encoded (too big, some other issue), the call returns `false` which is
                    // our signal to remove the metric from `maybe_v3_metrics` (if we added it), since we know now that
                    // the metric wasn't encoded for V2 and we want our V2/V3 payload batches to be consistent in
                    // validation mode.
                    let v2_flushed = if let Some(builder) = maybe_v2_builder {
                        let result = encode_v2_metrics(builder, metric, &telemetry, &mut payloads_tx, batch_id.as_ref()).await?;
                        if !result.encoded() {
                            if let Some(metrics) = maybe_v3_metrics {
                                let _ = metrics.pop();
                            }
                        }

                        result.flushed()
                    } else {
                        false
                    };

                    // If we flushed via V2, or we've hit our max metrics per payload limit in pure V3 mode, we need to flush our V3 metrics
                    // as well.
                    let v3_flushed = if let Some(v3_metrics) = maybe_v3_metrics {
                        if v2_flushed || v3_metrics.len() >= v3_endpoint_config.max_metrics_per_payload() {
                            encode_and_flush_v3_metrics(endpoint, &v3_endpoint_config, v3_metrics, &telemetry, &mut payloads_tx, batch_id.as_ref()).await?;
                        }
                        true
                    } else {
                        false
                    };

                    // If we flushed either V2 and/or V3, clear our validation batch UUID.
                    if v2_flushed || v3_flushed {
                        batch_id = None;
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

                // Flush any pending series metrics.
                let mut v2_series_flush_succeeded = true;
                if let Some(builder) = &mut v2_series_builder {
                    if let Err(e) = flush_v2_metrics(builder, &mut payloads_tx, batch_id.as_ref()).await {
                        error!(error = %e, "Failed to flush V2 series metrics: {}", e);
                        v2_series_flush_succeeded = false;
                    }
                }

                if let Some(metrics) = &mut v3_series_metrics {
                    if v2_series_flush_succeeded {
                        if let Err(e) = encode_and_flush_v3_series_metrics(&v3_endpoint_config, metrics, &telemetry, &mut payloads_tx, batch_id.as_ref()).await {
                            error!(error = %e, "Failed to flush V3 series metrics: {}", e);
                        }
                    } else {
                        warn!("Failed to flush V2 series metrics, skipping V3 series flush.");
                        metrics.clear();
                    }
                }

                // Flush any pending sketch metrics.
                let mut v2_sketches_flush_succeeded = true;
                if let Some(builder) = &mut v2_sketch_builder {
                    if let Err(e) = flush_v2_metrics(builder, &mut payloads_tx, batch_id.as_ref()).await {
                        error!(error = %e, "Failed to flush V2 sketch metrics: {}", e);
                        v2_sketches_flush_succeeded = false;
                    }
                }

                if let Some(metrics) = &mut v3_sketch_metrics {
                    if v2_sketches_flush_succeeded {
                        if let Err(e) = encode_and_flush_v3_sketch_metrics(&v3_endpoint_config, metrics, &telemetry, &mut payloads_tx, batch_id.as_ref()).await {
                            error!(error = %e, "Failed to flush V3 sketch metrics: {}", e);
                        }
                    } else {
                        warn!("Failed to flush V2 sketch metrics, skipping V3 sketch flush.");
                        metrics.clear();
                    }
                }

                // Clear our validation batch UUID.
                batch_id = None;

                debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
            },

            // Event buffers channel has been closed, and we have no pending flushing, so we're all done.
            else => break,
        }
    }

    Ok(())
}

struct EncodeResult {
    encoded: bool,
    flushed: bool,
}

impl EncodeResult {
    pub const fn new(encoded: bool, flushed: bool) -> Self {
        Self { encoded, flushed }
    }

    pub const fn encoded(&self) -> bool {
        self.encoded
    }

    pub const fn flushed(&self) -> bool {
        self.flushed
    }
}

async fn encode_v2_metrics(
    request_builder: &mut RequestBuilder<v2::MetricsEndpointEncoder>, metric: Metric, telemetry: &ComponentTelemetry,
    payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&HeaderValue>,
) -> Result<EncodeResult, GenericError> {
    // Encode the metric. If we get it back, that means the current request is full, and we need to
    // flush it before we can try to encode the metric again... so we'll hold on to it in that case
    // before flushing and trying to encode it again.
    let metric_to_retry = match request_builder.encode(metric).await {
        Ok(None) => return Ok(EncodeResult::new(true, false)),
        Ok(Some(metric)) => metric,
        Err(e) => {
            error!(error = %e, "Failed to encode metric.");
            telemetry.events_dropped_encoder().increment(1);
            return Ok(EncodeResult::new(false, false));
        }
    };

    flush_v2_metrics(request_builder, payloads_tx, batch_id).await?;

    // Now try to encode the metric again. If it fails again, we'll just log it because it shouldn't
    // be possible to fail at this point, otherwise we would have already caught that the first
    // time.
    match request_builder.encode(metric_to_retry).await {
        Ok(None) => Ok(EncodeResult::new(true, true)),
        Ok(Some(_)) => unreachable!(
            "failure to encode due to size should never occur after flush for metrics which aren't unencodable"
        ),
        Err(e) => {
            error!(error = %e, "Failed to encode metric.");
            telemetry.events_dropped_encoder().increment(1);
            Ok(EncodeResult::new(false, true))
        }
    }
}

async fn flush_v2_metrics(
    request_builder: &mut RequestBuilder<MetricsEndpointEncoder>, payloads_tx: &mut mpsc::Sender<Payload>,
    batch_id: Option<&HeaderValue>,
) -> Result<usize, GenericError> {
    let mut requests_flushed = 0;

    let maybe_requests = request_builder.flush().await;
    for maybe_request in maybe_requests {
        match maybe_request {
            Ok((events, request)) => {
                requests_flushed += 1;

                flush_payload(request, events, payloads_tx, batch_id).await?;
            }

            // TODO: Increment a counter here that metrics were dropped due to a flush failure.
            Err(e) => {
                if !e.is_recoverable() {
                    return Err(GenericError::from(e).context("Failed to flush request."));
                }
            }
        }
    }

    Ok(requests_flushed)
}

async fn encode_and_flush_v3_metrics(
    endpoint: MetricsEndpoint, ep_config: &EndpointConfiguration, metrics: &mut Vec<Metric>,
    telemetry: &ComponentTelemetry, payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&HeaderValue>,
) -> Result<(), GenericError> {
    match endpoint {
        MetricsEndpoint::Series => {
            encode_and_flush_v3_series_metrics(ep_config, metrics, telemetry, payloads_tx, batch_id).await
        }
        MetricsEndpoint::Sketches => {
            encode_and_flush_v3_sketch_metrics(ep_config, metrics, telemetry, payloads_tx, batch_id).await
        }
    }
}

async fn encode_and_flush_v3_series_metrics(
    ep_config: &EndpointConfiguration, metrics: &mut Vec<Metric>, telemetry: &ComponentTelemetry,
    payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&HeaderValue>,
) -> Result<(), GenericError> {
    let metrics_to_flush = std::mem::take(metrics);
    let events = metrics_to_flush.len();

    match encode_v3_metrics_batch(&metrics_to_flush, ep_config.additional_tags()) {
        Ok(encoded) => match create_v3_request("/api/v3/series", encoded, ep_config.compression_scheme()).await {
            Ok(request) => {
                flush_payload(request, events, payloads_tx, batch_id).await?;
                debug!(events, "Sent V3 series payload.");
            }
            Err(e) => {
                error!(error = %e, "Failed to create V3 series request.");
                telemetry.events_dropped_encoder().increment(events as u64);
            }
        },
        Err(e) => {
            error!(error = %e, "Failed to encode V3 series batch.");
            telemetry.events_dropped_encoder().increment(events as u64);
        }
    }

    Ok(())
}

async fn encode_and_flush_v3_sketch_metrics(
    ep_config: &EndpointConfiguration, metrics: &mut Vec<Metric>, telemetry: &ComponentTelemetry,
    payloads_tx: &mut mpsc::Sender<Payload>, batch_id: Option<&HeaderValue>,
) -> Result<(), GenericError> {
    let metrics_to_flush = std::mem::take(metrics);
    let events = metrics_to_flush.len();

    match encode_v3_metrics_batch(&metrics_to_flush, ep_config.additional_tags()) {
        Ok(encoded) => match create_v3_request("/api/v3/sketches", encoded, ep_config.compression_scheme()).await {
            Ok(request) => {
                flush_payload(request, events, payloads_tx, batch_id).await?;
                debug!(events, "Sent V3 sketches payload.");
            }
            Err(e) => {
                error!(error = %e, "Failed to create V3 sketches request.");
                telemetry.events_dropped_encoder().increment(events as u64);
            }
        },
        Err(e) => {
            error!(error = %e, "Failed to encode V3 sketches batch.");
            telemetry.events_dropped_encoder().increment(events as u64);
        }
    }

    Ok(())
}

async fn flush_payload(
    mut request: Request<FrozenChunkedBytesBuffer>, event_count: usize, payloads_tx: &mut mpsc::Sender<Payload>,
    batch_id: Option<&HeaderValue>,
) -> Result<(), GenericError> {
    // Attach the validation batch UUID if present.
    if let Some(batch_id) = batch_id {
        request
            .headers_mut()
            .insert("X-Datadog-Validation-Batch-UUID", batch_id.clone());
    }

    let payload_meta = PayloadMetadata::from_event_count(event_count);
    let http_payload = HttpPayload::new(payload_meta, request);
    let payload = Payload::Http(http_payload);

    payloads_tx
        .send(payload)
        .await
        .error_context("Failed to send V3 series payload.")?;

    Ok(())
}

// Encodes a batch of metrics to V3 columnar format.
fn encode_v3_metrics_batch(metrics: &[Metric], additional_tags: &SharedTagSet) -> Result<Vec<u8>, GenericError> {
    let mut writer = v3::V3Writer::new();

    for metric in metrics {
        write_metric_to_v3(&mut writer, metric, additional_tags);
    }

    let encoded_data = writer.close();
    let mut output = Vec::new();
    v3::serialize_v3_payload(&encoded_data, &mut output)
        .map_err(|e| generic_error!("Failed to serialize V3 payload: {}", e))?;

    Ok(output)
}

/// Writes a single metric to the V3 writer.
fn write_metric_to_v3(writer: &mut v3::V3Writer, metric: &Metric, additional_tags: &SharedTagSet) {
    let metric_type = match metric.values() {
        MetricValues::Counter(..) => v3::V3MetricType::Count,
        MetricValues::Rate(..) => v3::V3MetricType::Rate,
        MetricValues::Gauge(..) | MetricValues::Set(..) => v3::V3MetricType::Gauge,
        MetricValues::Histogram(..) | MetricValues::Distribution(..) => v3::V3MetricType::Sketch,
    };

    let mut builder = writer.write(metric_type, metric.context().name());

    // Tags - chain instrumented + additional + origin tags
    let all_tags: Vec<String> = metric
        .context()
        .tags()
        .into_iter()
        .chain(additional_tags)
        .chain(metric.context().origin_tags())
        .filter(|t| !t.name().starts_with("dd.internal.resource"))
        .map(|t| t.as_str().to_string())
        .collect();
    builder.set_tags(all_tags.iter().map(|s| s.as_str()));

    // Resources - extract host and any dd.internal.resource tags
    let mut resources: Vec<(String, String)> = Vec::new();
    if let Some(host) = metric.metadata().hostname() {
        resources.push(("host".to_string(), host.to_string()));
    }
    // Extract dd.internal.resource tags as resources
    for tag in metric.context().tags().into_iter().chain(additional_tags) {
        if tag.name() == "dd.internal.resource" {
            if let Some(value) = tag.value() {
                if let Some((rtype, rname)) = value.split_once(':') {
                    resources.push((rtype.to_string(), rname.to_string()));
                }
            }
        }
    }
    builder.set_resources(resources.into_iter());

    // Origin metadata
    if let Some(origin) = metric.metadata().origin() {
        match origin {
            MetricOrigin::SourceType(source_type) => {
                builder.set_source_type(source_type.as_ref());
            }
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => {
                builder.set_origin(*product, *subproduct, *product_detail);
            }
        }
    }

    // Points based on metric type
    match metric.values() {
        MetricValues::Counter(points) | MetricValues::Gauge(points) => {
            for (ts, val) in points {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                builder.add_point(timestamp, val);
            }
        }
        MetricValues::Rate(points, interval) => {
            builder.set_interval(interval.as_secs());
            for (ts, val) in points {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                // Scale by interval as done in V2
                let scaled = val / interval.as_secs_f64();
                builder.add_point(timestamp, scaled);
            }
        }
        MetricValues::Set(points) => {
            // Set values are already converted to count in the iterator
            for (ts, count) in points {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                builder.add_point(timestamp, count);
            }
        }
        MetricValues::Distribution(sketches) => {
            for (ts, sketch) in sketches {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                if !sketch.is_empty() {
                    let bin_keys: Vec<i32> = sketch.bins().iter().map(|b| b.key()).collect();
                    let bin_counts: Vec<u32> = sketch.bins().iter().map(|b| b.count()).collect();
                    builder.add_sketch(
                        timestamp,
                        sketch.count() as i64,
                        sketch.sum().unwrap_or(0.0),
                        sketch.min().unwrap_or(0.0),
                        sketch.max().unwrap_or(0.0),
                        &bin_keys,
                        &bin_counts,
                    );
                }
            }
        }
        MetricValues::Histogram(histograms) => {
            for (ts, histogram) in histograms {
                let timestamp = ts.map(|t| t.get() as i64).unwrap_or(0);
                // Convert histogram to DDSketch
                let mut sketch = DDSketch::default();
                for sample in histogram.samples() {
                    sketch.insert_n(sample.value.into_inner(), sample.weight as u32);
                }
                if !sketch.is_empty() {
                    let bin_keys: Vec<i32> = sketch.bins().iter().map(|b| b.key()).collect();
                    let bin_counts: Vec<u32> = sketch.bins().iter().map(|b| b.count()).collect();
                    builder.add_sketch(
                        timestamp,
                        sketch.count() as i64,
                        sketch.sum().unwrap_or(0.0),
                        sketch.min().unwrap_or(0.0),
                        sketch.max().unwrap_or(0.0),
                        &bin_keys,
                        &bin_counts,
                    );
                }
            }
        }
    }

    builder.close();
}

/// Creates a V3 HTTP request from encoded payload data.
async fn create_v3_request(
    endpoint_uri: &str, payload: Vec<u8>, compression_scheme: CompressionScheme,
) -> Result<Request<FrozenChunkedBytesBuffer>, GenericError> {
    let buffer = ChunkedBytesBuffer::new(RB_BUFFER_CHUNK_SIZE);
    let mut compressor = Compressor::from_scheme(compression_scheme, buffer);
    compressor
        .write_all(&payload)
        .await
        .error_context("Failed to compress V3 payload.")?;
    compressor
        .flush()
        .await
        .error_context("Failed to flush V3 compressor.")?;
    compressor
        .shutdown()
        .await
        .error_context("Failed to shutdown V3 compressor.")?;

    let content_encoding = compressor.content_encoding();
    let compressed_buf = compressor.into_inner().freeze();

    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(endpoint_uri)
        .header(http::header::CONTENT_TYPE, "application/x-protobuf");

    if let Some(encoding) = content_encoding {
        builder = builder.header(http::header::CONTENT_ENCODING, encoding);
    }

    builder
        .body(compressed_buf)
        .map_err(|e| generic_error!("Failed to build V3 request: {}", e))
}
