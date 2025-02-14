use std::time::Duration;

use async_trait::async_trait;
use http::{Request, Uri};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::{FixedSizeObjectPool, ObjectPool},
};
use saluki_error::GenericError;
use saluki_event::{metric::Metric, DataType};
use saluki_io::buf::{BytesBuffer, FixedSizeVec, FrozenChunkedBytesBuffer};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{select, time::sleep};
use tracing::{debug, error};

use super::common::{config::ForwarderConfiguration, io::TransactionForwarder, telemetry::ComponentTelemetry};

mod request_builder;
use self::request_builder::{MetricsEndpoint, RequestBuilder};

const RB_BUFFER_POOL_COUNT: usize = 128;
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

    /// Flush timeout for  pending requests, in seconds.
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
        let rb_buffer_pool = create_request_builder_buffer_pool();
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
        let rb_buffer_pool_size = RB_BUFFER_POOL_COUNT * RB_BUFFER_POOL_BUF_SIZE;

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            //
            // TODO: This type signature is _ugly_, and it would be nice to improve it somehow.
            .with_single_value::<DatadogMetrics<FixedSizeObjectPool<BytesBuffer>>>("component struct")
            // Capture the size of our buffer pool.
            .with_fixed_amount("buffer pool", rb_buffer_pool_size)
            // Capture the size of the requests channel.
            //
            // TODO: This type signature is _ugly_, and it would be nice to improve it somehow.
            .with_array::<(usize, Request<FrozenChunkedBytesBuffer>)>("requests channel", 32);

        builder
            .firm()
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
                maybe_events = context.events().next_ready() => match maybe_events {
                    Some(event_buffers) => {
                        debug!(event_buffers_len = event_buffers.len(), "Received event buffers.");

                        for event_buffer in event_buffers {
                            debug!(events_len = event_buffer.len(), "Processing event buffer.");

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
                                            Ok((events, request)) => forwarder_handle.send_request(events, request).await?,
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
                                forwarder_handle.send_request(events, request).await?;
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
                                forwarder_handle.send_request(events, request).await?;
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

fn create_request_builder_buffer_pool() -> FixedSizeObjectPool<BytesBuffer> {
    // Create the underlying fixed-size buffer pool for the individual chunks.
    //
    // This is 4MB total, in 32KB chunks, which ensures we have enough to simultaneously encode a request for the
    // Series/Sketch V1 endpoint (max of 3.2MB) as well as the Series V2 endpoint (max 512KB).
    //
    // We chunk it up into 32KB segments mostly to allow for balancing fragmentation vs acquisition overhead.
    FixedSizeObjectPool::with_builder("dd_metrics_request_buffer", RB_BUFFER_POOL_COUNT, || {
        FixedSizeVec::with_capacity(RB_BUFFER_POOL_BUF_SIZE)
    })
}
