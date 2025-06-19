use std::time::Duration;

use async_trait::async_trait;
use http::{uri::PathAndQuery, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::task::HandleExt as _;
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::{metric::Metric, EventType};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::{ElasticObjectPool, ObjectPool},
    topology::interconnect::FixedSizeEventBuffer,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::{
    buf::{BytesBuffer, FrozenChunkedBytesBuffer},
    compression::CompressionScheme,
};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{debug, error};

use super::common::{
    config::ForwarderConfiguration,
    io::{create_request_builder_buffer_pool, get_buffer_pool_min_max_size_bytes, Handle, TransactionForwarder},
    request_builder::RequestBuilder,
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction},
};

mod request_builder;
use self::request_builder::{MetricsEndpoint, MetricsEndpointEncoder};

const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;
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

/// Datadog Metrics destination.
///
/// Forwards metrics to the Datadog platform. It can handle both series and sketch metrics, and only utilizes the latest
/// API endpoints for both, which both use Protocol Buffers-encoded payloads.
///
/// ## Missing
///
/// - ability to configure either the basic site _or_ a specific endpoint (requires a full URI at the moment, even if
///   it's just something like `https`)
/// - retries, timeouts, rate limiting (no Tower middleware stack yet)
#[derive(Clone, Deserialize)]
#[allow(dead_code)]
pub struct DatadogMetricsConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    forwarder_config: ForwarderConfiguration,

    #[serde(skip)]
    config_refresher: Option<RefreshableConfiguration>,

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

    /// Endpoint path override for all metric types.
    #[serde(default, skip)]
    endpoint_path_override: Option<PathAndQuery>,

    /// Additional tags to apply to all forwarded metrics.
    #[serde(default, skip)]
    additional_tags: Option<SharedTagSet>,
}

impl DatadogMetricsConfiguration {
    /// Creates a new `DatadogMetricsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Add option to retrieve configuration values from a `RefreshableConfiguration`.
    pub fn add_refreshable_configuration(&mut self, refresher: RefreshableConfiguration) {
        self.config_refresher = Some(refresher);
    }

    /// Overrides the default endpoint that metrics are sent to.
    ///
    /// This overrides any existing endpoint configuration, and manually sets the base endpoint (e.g.,
    /// `https://api.datad0g.com`) and request path (e.g., `/api/v2/series`) to be used for all metrics payloads.
    ///
    /// This can be used to preserve other configuration settings (forwarder settings, retry, etc) while still allowing
    /// for overriding _where_ metrics are sent to.
    ///
    /// # Errors
    ///
    /// If the given request path is not valid, an error is returned.
    pub fn with_endpoint_override(
        mut self, dd_url: String, api_key: String, request_path: String,
    ) -> Result<Self, GenericError> {
        // Clear any existing additional endpoints, and set the new DD URL and API key.
        //
        // This ensures that the only endpoint we'll send to is this one.
        let endpoint = self.forwarder_config.endpoint_mut();
        endpoint.clear_additional_endpoints();
        endpoint.set_dd_url(dd_url);
        endpoint.set_api_key(api_key);

        // Set the endpoint path override to ensure that the request builders use the overridden path.
        let request_path =
            PathAndQuery::from_maybe_shared(request_path).error_context("Failed to parse request path.")?;
        self.endpoint_path_override = Some(request_path);

        Ok(self)
    }

    /// Sets additional tags to be applied uniformly to all metrics forwarded by this destination.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        // Add the additional tags to the forwarder configuration.
        self.additional_tags = Some(additional_tags);
        self
    }
}

#[async_trait]
impl DestinationBuilder for DatadogMetricsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder = TransactionForwarder::from_config(
            context,
            self.forwarder_config.clone(),
            self.config_refresher.clone(),
            get_metrics_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builders.
        let rb_buffer_pool = create_request_builder_buffer_pool(
            "metrics",
            &self.forwarder_config,
            get_maximum_compressed_payload_size(),
        )
        .await;

        let mut series_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Series);
        let mut sketches_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);

        if let Some(additional_tags) = self.additional_tags.as_ref() {
            series_encoder = series_encoder.with_additional_tags(additional_tags.clone());
            sketches_encoder = sketches_encoder.with_additional_tags(additional_tags.clone());
        }

        let mut series_rb = RequestBuilder::new(series_encoder, rb_buffer_pool.clone(), compression_scheme).await?;
        series_rb.with_max_inputs_per_payload(self.max_metrics_per_payload);

        let mut sketches_rb = RequestBuilder::new(sketches_encoder, rb_buffer_pool.clone(), compression_scheme).await?;
        sketches_rb.with_max_inputs_per_payload(self.max_metrics_per_payload);

        if let Some(override_path) = self.endpoint_path_override.as_ref() {
            series_rb.with_endpoint_uri_override(override_path.clone());
            sketches_rb.with_endpoint_uri_override(override_path.clone());
        }

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogMetrics {
            series_rb,
            sketches_rb,
            forwarder,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The request builder buffer pool is shared between both the series and the sketches request builder, so we
        // only count it once.
        let (pool_size_min_bytes, _) =
            get_buffer_pool_min_max_size_bytes(&self.forwarder_config, get_maximum_compressed_payload_size());

        builder
            .minimum()
            .with_single_value::<DatadogMetrics<ElasticObjectPool<BytesBuffer>>>("component struct")
            .with_fixed_amount("buffer pool", pool_size_min_bytes)
            .with_array::<Transaction<FrozenChunkedBytesBuffer>>("requests channel", 32);

        builder
            .firm()
            // This represents the potential growth of the buffer pool to allow for requests to continue to be built
            // while we're retrying the current request, and having to enqueue pending requests in memory.
            .with_expr(UsageExpr::sum(
                "buffer pool",
                UsageExpr::config(
                    "forwarder_retry_queue_payloads_max_size",
                    self.forwarder_config.retry().queue_max_size_bytes() as usize,
                ),
                UsageExpr::product(
                    "high priority queue",
                    UsageExpr::config(
                        "forwarder_high_prio_buffer_size",
                        self.forwarder_config.endpoint_buffer_size(),
                    ),
                    UsageExpr::constant("maximum compressed payload size", get_maximum_compressed_payload_size()),
                ),
            ))
            // Capture the size of the "split re-encode" buffers in the request builders, which is where we keep owned
            // versions of metrics that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<Metric>("series metrics split re-encode buffer", self.max_metrics_per_payload)
            .with_array::<Metric>("sketch metrics split re-encode buffer", self.max_metrics_per_payload);
    }
}

pub struct DatadogMetrics<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    series_rb: RequestBuilder<MetricsEndpointEncoder, O>,
    sketches_rb: RequestBuilder<MetricsEndpointEncoder, O>,
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[async_trait]
impl<O> Destination for DatadogMetrics<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            series_rb,
            sketches_rb,
            forwarder,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our forwarder task to handle sending requests.
        let forwarder_handle = forwarder.spawn().await;

        // Spawn our request builder task.
        let (builder_tx, builder_rx) = mpsc::channel(8);

        let request_builder_fut = run_request_builder(
            series_rb,
            sketches_rb,
            telemetry,
            builder_rx,
            forwarder_handle,
            flush_timeout,
        );
        let request_builder_handle = context
            .global_thread_pool()
            .spawn_traced_named("dd-metrics-request-builder", request_builder_fut);

        health.mark_ready();
        debug!("Datadog Metrics destination started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_event_buffer = context.events().next() => match maybe_event_buffer {
                    Some(event_buffer) => builder_tx.send(event_buffer).await
                        .error_context("Failed to send event buffer to request builder task.")?,
                    None => break,
                },
            }
        }

        // Drop the request builder channel, which allows the request builder task to naturally shut down once it has
        // received and built all requests. This includes also triggering the forwarder to shutdown, and waiting for it
        // to do so.
        //
        // We wait for the request builder task to signal back to us that it has stopped before letting ourselves return.
        drop(builder_tx);
        match request_builder_handle.await {
            Ok(Ok(())) => debug!("Request builder task stopped."),
            Ok(Err(e)) => error!(error = %e, "Request builder task failed."),
            Err(e) => error!(error = %e, "Request builder task panicked."),
        }

        debug!("Datadog Metrics destination stopped.");

        Ok(())
    }
}

async fn run_request_builder<O>(
    mut series_request_builder: RequestBuilder<MetricsEndpointEncoder, O>,
    mut sketches_request_builder: RequestBuilder<MetricsEndpointEncoder, O>, telemetry: ComponentTelemetry,
    mut request_builder_rx: mpsc::Receiver<FixedSizeEventBuffer>, forwarder_handle: Handle<FrozenChunkedBytesBuffer>,
    flush_timeout: Duration,
) -> Result<(), GenericError>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    let mut pending_flush = false;
    let pending_flush_timeout = sleep(flush_timeout);
    tokio::pin!(pending_flush_timeout);

    loop {
        select! {
            Some(event_buffer) = request_builder_rx.recv() => {
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
                                let transaction = Transaction::from_original(Metadata::from_event_count(events), request);
                                forwarder_handle.send_transaction(transaction).await?
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
                            debug!("Flushed request from series request builder. Sending to I/O task...");
                            let transaction = Transaction::from_original(Metadata::from_event_count(events), request);
                            forwarder_handle.send_transaction(transaction).await?
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
                            debug!("Flushed request from sketches request builder. Sending to I/O task...");
                            let transaction = Transaction::from_original(Metadata::from_event_count(events), request);
                            forwarder_handle.send_transaction(transaction).await?
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

    debug!("Waiting for forwarder to shutdown...");

    // Signal the forwarder to shutdown, and wait for it to do so.
    forwarder_handle.shutdown().await;

    Ok(())
}

fn get_metrics_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v2/series" => Some(MetaString::from_static("series_v2")),
        "/api/beta/sketches" => Some(MetaString::from_static("sketches_v2")),
        "/api/intake/pipelines/ddseries" => Some(MetaString::from_static("preaggregation")),
        _ => None,
    }
}

const fn get_maximum_compressed_payload_size() -> usize {
    let series_max_size = MetricsEndpoint::Series.compressed_size_limit();
    let sketches_max_size = MetricsEndpoint::Sketches.compressed_size_limit();

    let max_request_size = if series_max_size > sketches_max_size {
        series_max_size
    } else {
        sketches_max_size
    };
    max_request_size.next_multiple_of(RB_BUFFER_POOL_BUF_SIZE)
}
