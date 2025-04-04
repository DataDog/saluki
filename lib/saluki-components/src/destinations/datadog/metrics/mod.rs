use std::time::Duration;

use async_trait::async_trait;
use http::Uri;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::{ElasticObjectPool, ObjectPool},
    task::spawn_traced,
};
use saluki_error::GenericError;
use saluki_event::{metric::Metric, DataType};
use saluki_io::buf::{BytesBuffer, FixedSizeVec, FrozenChunkedBytesBuffer};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, time::sleep};
use tracing::{debug, error};

use super::common::{
    config::ForwarderConfiguration,
    io::TransactionForwarder,
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction},
};

mod request_builder;
use self::request_builder::{MetricsEndpoint, RequestBuilder};

const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;

const fn default_max_metrics_per_payload() -> usize {
    10_000
}

const fn default_flush_timeout_secs() -> u64 {
    2
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
#[derive(Deserialize)]
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
}

#[async_trait]
impl DestinationBuilder for DatadogMetricsConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let metrics_builder = MetricsBuilder::from_component_context(context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let forwarder = TransactionForwarder::from_config(
            self.forwarder_config.clone(),
            self.config_refresher.clone(),
            get_metrics_endpoint_name,
            telemetry.clone(),
            metrics_builder,
        )?;

        // Create our request builders.
        let rb_buffer_pool = create_request_builder_buffer_pool(&self.forwarder_config).await;
        let series_request_builder = RequestBuilder::new(
            MetricsEndpoint::Series,
            rb_buffer_pool.clone(),
            self.max_metrics_per_payload,
        )
        .await?;
        let sketches_request_builder =
            RequestBuilder::new(MetricsEndpoint::Sketches, rb_buffer_pool, self.max_metrics_per_payload).await?;

        let flush_timeout = match self.flush_timeout_secs {
            // We always give ourselves a minimum flush timeout of 10ms to allow for some very minimal amount of
            // batching, while still practically flushing things almost immediately.
            0 => Duration::from_millis(10),
            secs => Duration::from_secs(secs),
        };

        Ok(Box::new(DatadogMetrics {
            series_request_builder,
            sketches_request_builder,
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
        let (pool_size_min_bytes, _) = get_buffer_pool_minimum_maximum_size_bytes(&self.forwarder_config);

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
    series_request_builder: RequestBuilder<O>,
    sketches_request_builder: RequestBuilder<O>,
    forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
    telemetry: ComponentTelemetry,
    flush_timeout: Duration,
}

#[allow(unused)]
#[async_trait]
impl<O> Destination for DatadogMetrics<O>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            mut series_request_builder,
            mut sketches_request_builder,
            forwarder,
            telemetry,
            flush_timeout,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our forwarder task to handle sending requests.
        let forwarder_handle = forwarder.spawn().await;

        health.mark_ready();
        debug!("Datadog Metrics destination started.");

        let mut pending_flush = false;
        let mut pending_flush_timeout = sleep(flush_timeout);
        tokio::pin!(pending_flush_timeout);

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(event_buffer) => {
                        for event in event_buffer {
                            if let Some(metric) = event.try_into_metric() {
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
                                        Err(e) => {
                                            // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                                            if e.is_recoverable() {
                                                // If the error is recoverable, we'll hold on to the metric to retry it later.
                                                continue;
                                            } else {
                                                return Err(GenericError::from(e).context("Failed to flush request."));
                                            }
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
                        }


                        debug!("All event buffers processed.");

                        // If we're not already pending a flush, we'll start the countdown.
                        if !pending_flush {
                            pending_flush_timeout.as_mut().reset(tokio::time::Instant::now() + flush_timeout);
                            pending_flush = true;
                        }
                    },
                    None => break,
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
                            Err(e) => {
                                // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                                if e.is_recoverable() {
                                    // If the error is recoverable, we'll hold on to the metric to retry it later.
                                    continue;
                                } else {
                                    return Err(GenericError::from(e).context("Failed to flush request."));
                                }
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
                            Err(e) => {
                                // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                                if e.is_recoverable() {
                                    // If the error is recoverable, we'll hold on to the metric to retry it later.
                                    continue;
                                } else {
                                    return Err(GenericError::from(e).context("Failed to flush request."));
                                }
                            }
                        }
                    }

                    debug!("All flushed requests sent to I/O task. Waiting for next event buffer...");
                }
            }
        }

        // Signal the forwarder to shutdown, and wait until it does so.
        forwarder_handle.shutdown().await;

        debug!("Datadog Metrics destination stopped.");

        Ok(())
    }
}

fn get_metrics_endpoint_name(uri: &Uri) -> Option<MetaString> {
    match uri.path() {
        "/api/v2/series" => Some(MetaString::from_static("series_v2")),
        "/api/beta/sketches" => Some(MetaString::from_static("sketches_v2")),
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

fn get_buffer_pool_minimum_maximum_size(config: &ForwarderConfiguration) -> (usize, usize) {
    // Just enough to build a single instance of the largest possible request.
    let max_request_size = get_maximum_compressed_payload_size();
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

fn get_buffer_pool_minimum_maximum_size_bytes(config: &ForwarderConfiguration) -> (usize, usize) {
    let (minimum_size, maximum_size) = get_buffer_pool_minimum_maximum_size(config);
    (
        minimum_size * RB_BUFFER_POOL_BUF_SIZE,
        maximum_size * RB_BUFFER_POOL_BUF_SIZE,
    )
}

async fn create_request_builder_buffer_pool(config: &ForwarderConfiguration) -> ElasticObjectPool<BytesBuffer> {
    // Create the underlying buffer pool for the individual chunks.
    //
    // We size this buffer pool in the following way:
    //
    // - regardless of the size, we split it up into many smaller chunks which can be allocated incrementally by the
    //   request builders as needed
    // - the minimum pool size is such that we can handle encoding the biggest possible request (3.2MB) in one go
    //   without needing another buffer to be allocated, so that when we're building big requests, we hopefully don't
    //   need to allocate on demand
    // - the maximum pool size is an additional increase over the minimum size based on the allowable in-memory size of
    //   the retry queue: if we're enqueuing requests in-memory due to retry backoff, we want to allow the request
    //   builders to keep building new requests without being blocked by the buffer pool, such that there's enough
    //   capacity to build requests until the retry queue starts throwing away the oldest ones to make room
    //
    // Our chunk size is 32KB: no strong reason for this, just a decent balance between being big enough to allow
    // compressor output blocks to fit entirely but small enough to not be too wasteful.

    let (minimum_size, maximum_size) = get_buffer_pool_minimum_maximum_size(config);
    let (pool, shrinker) =
        ElasticObjectPool::with_builder("dd_metrics_request_buffer", minimum_size, maximum_size, || {
            FixedSizeVec::with_capacity(RB_BUFFER_POOL_BUF_SIZE)
        });

    spawn_traced(shrinker);

    debug!(minimum_size, maximum_size, "Created request builder buffer pool.");

    pool
}
