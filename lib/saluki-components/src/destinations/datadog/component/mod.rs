use std::time::Duration;

use async_trait::async_trait;
use http::Uri;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_common::task::HandleExt as _;
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::{ElasticObjectPool, ObjectPool},
    topology::interconnect::FixedSizeEventBuffer,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_event::{eventd::EventD, metric::Metric, service_check::ServiceCheck, DataType};
use saluki_io::{
    buf::{BytesBuffer, FrozenChunkedBytesBuffer},
    compression::CompressionScheme,
};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{debug, error, warn};

use super::common::{
    config::ForwarderConfiguration,
    io::{Handle, TransactionForwarder},
    request_builder::RequestBuilder,
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction},
};

const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;
const DEFAULT_SERIALIZER_COMPRESSOR_KIND: &str = "zstd";
const MAX_EVENTS_PER_PAYLOAD: usize = 100;
const MAX_SERVICE_CHECKS_PER_PAYLOAD: usize = 100;

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

/// Datadog destination.
///
/// Forwards metrics to the Datadog platform. It can handle both series and sketch metrics, and only utilizes the latest
/// API endpoints for both, which both use Protocol Buffers-encoded payloads.
///
/// Capable of forwarding metrics, events, and service checks to the Datadog platform. A single destination can handle
/// all event types, or multiple instances can be created to handle different event types or allow for tailored
/// configuration of how a specific event type is handled.
///
/// Principally, this allows common configuration values to be shared as well as resources, such as object pools and
/// client connections.
#[derive(Clone, Deserialize)]
#[allow(dead_code)]
pub struct DatadogConfiguration {
    /// Forwarder configuration settings.
    ///
    /// See [`ForwarderConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    forwarder_config: ForwarderConfiguration,

    #[serde(skip)]
    config_refresher: Option<RefreshableConfiguration>,

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

	/// Endpoint path override for all event types.
    #[serde(default)]
	endpoint_path_override: Option<&'static str>,
}

impl DatadogConfiguration {
    /// Creates a new `DatadogConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Add option to retrieve configuration values from a `RefreshableConfiguration`.
    pub fn add_refreshable_configuration(&mut self, refresher: RefreshableConfiguration) {
        self.config_refresher = Some(refresher);
    }

    /// Configure the destination for metric preaggregation.
    pub fn configure_for_preaggregation(&mut self) {
        // Sanity check that we're dealing with datad0g.com (i.e. staging)
        let site = self
            .forwarder_config
            .endpoint()
            .dd_url()
            .unwrap_or(self.forwarder_config.endpoint().site());
        if site.contains("datad0g.com") {
            self.forwarder_config
                .endpoint_mut()
                .set_dd_url("https://api.datad0g.com".to_string());
            self.forwarder_config.endpoint.additional_endpoints = Default::default();
            self.endpoint_path_override = Some("/api/intake/pipelines/ddseries");
        } else {
            warn!(
                ?site,
                "Not a staging environment, skipping configuration for metric preaggregation."
            );
        }
    }
}

#[async_trait]
impl DestinationBuilder for DatadogConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric | DataType::EventD | DataType::ServiceCheck
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder = TransactionForwarder::from_config(
            self.forwarder_config.clone(),
            self.config_refresher.clone(),
            get_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;
        let compression_scheme = CompressionScheme::new(&self.compressor_kind, self.zstd_compressor_level);

        // Create our request builders.
        let buffer_pool = create_request_builder_buffer_pool(&self.forwarder_config, get_maximum_compressed_payload_size()).await;

		let series_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Series);
        let mut series_req_builder = RequestBuilder::new(series_encoder, buffer_pool.clone(), compression_scheme)
			.await?
        	.with_max_inputs_per_payload(self.max_metrics_per_payload);

		let sketches_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);
        let mut sketches_req_builder = RequestBuilder::new(sketches_encoder, buffer_pool.clone(), compression_scheme)
			.await?
        	.with_max_inputs_per_payload(self.max_metrics_per_payload);
	
		let events_encoder = EventsServiceChecksEndpointEncoder::for_events();
		let mut events_req_builder = RequestBuilder::new(events_encoder, buffer_pool.clone(), compression_scheme)
			.await?
			.with_max_inputs_per_payload(MAX_EVENTS_PER_PAYLOAD);

		let service_checks_encoder = EventsServiceChecksEndpointEncoder::for_service_checks();
		let mut service_checks_req_builder = RequestBuilder::new(service_checks_encoder, buffer_pool, compression_scheme)
			.await?
			.with_max_inputs_per_payload(MAX_SERVICE_CHECKS_PER_PAYLOAD);

		// If we have an override URI configured, set it for all request builders.
        if let Some(override_path) = self.endpoint_path_override {
            series_req_builder.set_endpoint_uri_override(override_path);
            sketches_req_builder.set_endpoint_uri_override(override_path);
			events_req_builder.set_endpoint_uri_override(override_path);
			service_checks_req_builder.set_endpoint_uri_override(override_path);
        }

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogMetrics {
            series_req_builder,
            sketches_req_builder,
			events_req_builder,
			service_checks_req_builder,
            forwarder,
            telemetry,
            flush_timeout,
        }))
    }
}

impl MemoryBounds for DatadogConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The request builder buffer pool is shared between both the series and the sketches request builder, so we
        // only count it once.
        let (pool_size_min_bytes, _) = get_buffer_pool_minimum_maximum_size_bytes(&self.forwarder_config, get_maximum_compressed_payload_size());

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
            // versions of events that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<Metric>("series metrics split re-encode buffer", self.max_metrics_per_payload)
            .with_array::<Metric>("sketch metrics split re-encode buffer", self.max_metrics_per_payload)
			.with_array::<EventD>("events split re-encode buffer", MAX_EVENTS_PER_PAYLOAD)
			.with_array::<ServiceCheck>("service checks split re-encode buffer", MAX_SERVICE_CHECKS_PER_PAYLOAD);
    }
}

pub struct DatadogMetrics<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    series_req_builder: RequestBuilder<MetricsEndpointEncoder, O>,
    sketches_req_builder: RequestBuilder<MetricsEndpointEncoder, O>,
	events_req_builder: RequestBuilder<EventsServiceChecksEndpointEncoder<EventD>, O>,
	service_checks_req_builder: RequestBuilder<EventsServiceChecksEndpointEncoder<ServiceCheck>, O>,
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
            series_req_builder,
            sketches_req_builder,
			events_req_builder,
			service_checks_req_builder,
            forwarder,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our forwarder task to handle sending requests.
        let forwarder_handle = forwarder.spawn().await;

        // Spawn our request builder task.
        let (builder_tx, builder_rx) = mpsc::channel(8);
        let request_builder_handle = context.global_thread_pool().spawn_traced(run_request_builder(
            series_req_builder,
            sketches_req_builder,
			events_req_builder,
			service_checks_req_builder,
            telemetry,
            builder_rx,
            forwarder_handle,
            flush_timeout,
        ));

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

fn get_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v2/series" => Some(MetaString::from_static("series_v2")),
        "/api/beta/sketches" => Some(MetaString::from_static("sketches_v2")),
		"/api/v1/events_batch" => Some(MetaString::from_static("events_batch_v1")),
        "/api/v1/check_run" => Some(MetaString::from_static("check_run_v1")),
        _ => None,
    }
}

const fn get_maximum_compressed_payload_size() -> usize {
	let series_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Series);
	let sketches_encoder = MetricsEndpointEncoder::from_endpoint(MetricsEndpoint::Sketches);
	let events_encoder = EventsServiceChecksEndpointEncoder::for_events();
	let service_checks_encoder = EventsServiceChecksEndpointEncoder::for_service_checks();

	let mut max_request_size = series_encoder.compressed_size_limit();
	if sketches_encoder.compressed_size_limit() > max_request_size {
		max_request_size = sketches_encoder.compressed_size_limit();
	}
	if events_encoder.compressed_size_limit() > max_request_size {
		max_request_size = events_encoder.compressed_size_limit();
	}
	if service_checks_encoder.compressed_size_limit() > max_request_size {
		max_request_size = service_checks_encoder.compressed_size_limit();
	}

	max_request_size
}

/// Returns the minimum and maximum size of the buffer pool needed for the given forwarder configuration and maximum
/// request size.
///
/// The returned sizes are the minimum and maximum number of buffers needed in the pool, respectively.
fn get_buffer_pool_minimum_maximum_size(config: &ForwarderConfiguration, max_request_size: usize) -> (usize, usize) {
    // Just enough to build a single instance of the largest possible request.
    let max_request_size = max_request_size.next_multiple_of(RB_BUFFER_POOL_BUF_SIZE);
    let minimum_size = max_request_size / RB_BUFFER_POOL_BUF_SIZE;

    // At the top end, we may create up to `forwarder_high_prio_buffer_size` requests in memory, each of which may be up
    // to `max_request_size` in size. We also need to account for the retry queue's maximum size, which is already
    // defined in bytes for us.
    let high_prio_pending_requests_size_bytes = config.endpoint_buffer_size() * max_request_size;
    let retry_queue_max_size_bytes = config.retry().queue_max_size_bytes() as usize;
    let retry_queue_max_size_bytes_rounded = retry_queue_max_size_bytes.next_multiple_of(RB_BUFFER_POOL_BUF_SIZE);
    let maximum_size = minimum_size
        + ((high_prio_pending_requests_size_bytes + retry_queue_max_size_bytes_rounded) / RB_BUFFER_POOL_BUF_SIZE);

    (minimum_size, maximum_size)
}

/// Returns the minimum and maximum size of the buffer pool needed for the given forwarder configuration and maximum
/// request size, in bytes.
///
/// This can be used to determine the size of the buffer pool in bytes when specifying the memory bounds for a destination.
fn get_buffer_pool_minimum_maximum_size_bytes(config: &ForwarderConfiguration, max_request_size: usize) -> (usize, usize) {
    let (minimum_size, maximum_size) = get_buffer_pool_minimum_maximum_size(config, max_request_size);
    (
        minimum_size * RB_BUFFER_POOL_BUF_SIZE,
        maximum_size * RB_BUFFER_POOL_BUF_SIZE,
    )
}

/// Creates a new buffer pool suitable for use with `RequestBuilder`.
///
/// This function handles the nuance of selecting the proper buffer pool size based on the maximum request size and the
/// given forwarder configuration, as care must be taken to ensure that the pool is large enough to handle building all
/// of the requests that may reside in memory when buffering/backoff is occurring.
async fn create_request_builder_buffer_pool(config: &ForwarderConfiguration, max_request_size: usize) -> ElasticObjectPool<BytesBuffer> {
    // Create the underlying buffer pool for the individual chunks.
    //
    // We size this buffer pool in the following way:
    //
    // - regardless of the size, we split it up into many smaller chunks which can be allocated incrementally by the
    //   request builders as needed
    // - the minimum pool size is such that we can handle encoding the biggest possible request (`max_request_size`) in one go
    //   without needing another buffer to be allocated, so that when we're building big requests, we hopefully don't
    //   need to allocate on demand
    // - the maximum pool size is an additional increase over the minimum size based on the allowable in-memory size of
    //   the retry queue: if we're enqueuing requests in-memory due to retry backoff, we want to allow the request
    //   builders to keep building new requests without being blocked by the buffer pool, such that there's enough
    //   capacity to build requests until the retry queue starts throwing away the oldest ones to make room
    //
    // Our chunk size is 32KB: no strong reason for this, just a decent balance between being big enough to allow
    // compressor output blocks to fit entirely but small enough to not be too wasteful.

    let (minimum_size, maximum_size) = get_buffer_pool_minimum_maximum_size(config, max_request_size);
    let (pool, shrinker) =
        ElasticObjectPool::with_builder(format!("dd_request_buffers", destination_type), minimum_size, maximum_size, || {
            FixedSizeVec::with_capacity(RB_BUFFER_POOL_BUF_SIZE)
        });

    spawn_traced_named(format!("dd-request-buffer-pool-shrinker", destination_type), shrinker);

    debug!(destination_type, minimum_size, maximum_size, "Created request builder buffer pool.");

    pool
}
