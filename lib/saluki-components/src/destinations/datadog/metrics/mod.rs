use std::sync::Arc;

use async_trait::async_trait;
use http::{Request, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    observability::ComponentMetricsExt as _,
    pooling::{FixedSizeObjectPool, ObjectPool},
    task::spawn_traced,
};
use saluki_error::{generic_error, GenericError};
use saluki_event::DataType;
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec, FrozenChunkedBytesBuffer},
    net::{client::http::HttpClient, util::retry::agent::DatadogAgentForwarderRetryConfiguration},
};
use saluki_metrics::MetricsBuilder;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot, Barrier},
};
use tower::{BoxError, Service, ServiceBuilder};
use tracing::{debug, error};

use super::common::{
    endpoints::{EndpointConfiguration, ResolvedEndpoint},
    middleware::{for_resolved_endpoint, with_version_info},
    telemetry::ComponentTelemetry,
};

mod request_builder;
use self::request_builder::{MetricsEndpoint, RequestBuilder};

const RB_BUFFER_POOL_COUNT: usize = 128;
const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;

fn default_request_timeout_secs() -> u64 {
    20
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
    /// Endpoint configuration settings
    ///
    /// See [`EndpointConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    endpoint_config: EndpointConfiguration,

    /// The request timeout for forwarding metrics, in seconds.
    ///
    /// Defaults to 20 seconds.
    #[serde(default = "default_request_timeout_secs", rename = "forwarder_timeout")]
    request_timeout_secs: u64,

    /// Retry configuration settings.
    ///
    /// See [`DatadogAgentForwarderRetryConfiguration`] for more information about the available settings.
    #[serde(flatten)]
    retry_config: DatadogAgentForwarderRetryConfiguration,

    #[serde(skip)]
    config_refresher: Option<RefreshableConfiguration>,
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

        let service = HttpClient::builder()
            .with_retry_policy(self.retry_config.into_default_http_retry_policy())
            .with_bytes_sent_counter(metrics_builder.register_debug_counter("component_bytes_sent_total"))
            .with_endpoint_telemetry(metrics_builder, Some(get_metrics_endpoint_name))
            .build()?;

        // Resolve all endpoints that we'll be forwarding metrics to.
        let endpoints = self
            .endpoint_config
            .build_resolved_endpoints(self.config_refresher.clone())?;

        // Create our request builders.
        let rb_buffer_pool = create_request_builder_buffer_pool();
        let series_request_builder = RequestBuilder::new(MetricsEndpoint::Series, rb_buffer_pool.clone()).await?;
        let sketches_request_builder = RequestBuilder::new(MetricsEndpoint::Sketches, rb_buffer_pool).await?;

        Ok(Box::new(DatadogMetrics {
            service,
            telemetry,
            series_request_builder,
            sketches_request_builder,
            endpoints,
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
            .with_single_value::<DatadogMetrics<
                FixedSizeObjectPool<BytesBuffer>,
                HttpClient<FrozenChunkedBytesBuffer>,
            >>()
            // Capture the size of our buffer pool.
            .with_fixed_amount(rb_buffer_pool_size)
            // Capture the size of the scratch buffer which may grow up to the uncompressed limit.
            .with_fixed_amount(MetricsEndpoint::Series.uncompressed_size_limit())
            .with_fixed_amount(MetricsEndpoint::Sketches.uncompressed_size_limit())
            // Capture the size of the requests channel.
            //
            // TODO: This type signature is _ugly_, and it would be nice to improve it somehow.
            .with_array::<(
                usize,
                Request<FrozenChunkedBytesBuffer>,
            )>(32);
    }
}

pub struct DatadogMetrics<O, S>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
    S: Service<Request<FrozenChunkedBytesBuffer>> + 'static,
{
    service: S,
    telemetry: ComponentTelemetry,
    series_request_builder: RequestBuilder<O>,
    sketches_request_builder: RequestBuilder<O>,
    endpoints: Vec<ResolvedEndpoint>,
}

#[allow(unused)]
#[async_trait]
impl<O, S> Destination for DatadogMetrics<O, S>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
    S: Service<Request<FrozenChunkedBytesBuffer>, Response = hyper::Response<Incoming>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
{
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            mut series_request_builder,
            mut sketches_request_builder,
            service,
            telemetry,
            endpoints,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our IO task to handle sending requests.
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();
        let (requests_tx, requests_rx) = mpsc::channel(32);
        spawn_traced(run_io_loop(
            requests_rx,
            io_shutdown_tx,
            service,
            telemetry.clone(),
            endpoints,
        ));

        health.mark_ready();
        debug!("Datadog Metrics destination started.");

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
                                            Ok(request) => {
                                                if requests_tx.send(request).await.is_err() {
                                                    return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                                }
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
                        }

                        debug!("All event buffers processed.");

                        // Once we've encoded and written all metrics, we flush the request builders to generate a request with
                        // anything left over. Again, we'll  enqueue those requests to be sent immediately.
                        let maybe_series_requests = series_request_builder.flush().await;
                        for maybe_request in maybe_series_requests {
                            match maybe_request {
                                Ok(request) => {
                                    debug!("Flushed request from series request builder. Sending to I/O task...");
                                    if requests_tx.send(request).await.is_err() {
                                        return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                    }
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
                                Ok(request) => {
                                debug!("Flushed request from sketches request builder. Sending to I/O task...");
                                    if requests_tx.send(request).await.is_err() {
                                        return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                    }
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
                    },
                    None => break,
                }
            }
        }

        // Drop the requests channel, which allows the IO task to naturally shut down once it has received and sent all
        // requests. We then wait for it to signal back to us that it has stopped before exiting ourselves.
        drop(requests_tx);
        let _ = io_shutdown_rx.await;

        debug!("Datadog Metrics destination stopped.");

        Ok(())
    }
}

async fn run_io_loop<S, B>(
    mut requests_rx: mpsc::Receiver<(usize, Request<B>)>, io_shutdown_tx: oneshot::Sender<()>, service: S,
    telemetry: ComponentTelemetry, resolved_endpoints: Vec<ResolvedEndpoint>,
) where
    S: Service<Request<B>, Response = hyper::Response<Incoming>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
    B: Body + Clone + Send + 'static,
{
    // Spawn an endpoint I/O task for each endpoint we're configured to send to.
    //
    // This gives us granularity at just the right level, since the places we expect there to be problems (and thus
    // where concurrency can help make forward progress) are at the endpoint/API key level.
    let mut endpoint_txs = Vec::new();
    let task_barrier = Arc::new(Barrier::new(resolved_endpoints.len() + 1));

    for resolved_endpoint in resolved_endpoints {
        let endpoint_url = resolved_endpoint.endpoint().to_string();

        let (endpoint_tx, endpoint_rx) = mpsc::channel(32);
        let task_barrier = Arc::clone(&task_barrier);
        spawn_traced(run_endpoint_io_loop(
            endpoint_rx,
            task_barrier,
            service.clone(),
            telemetry.clone(),
            resolved_endpoint,
        ));

        endpoint_txs.push((endpoint_url, endpoint_tx));
    }

    // Listen for requests to forward, and send a copy of each one to each endpoint I/O task.
    while let Some((metrics_count, request)) = requests_rx.recv().await {
        for (endpoint_url, endpoint_tx) in &endpoint_txs {
            if endpoint_tx.try_send((metrics_count, request.clone())).is_err() {
                error!(
                    endpoint = endpoint_url,
                    "Failed to send request to endpoint I/O task: receiver dropped."
                );
            }
        }
    }

    debug!("Requests channel for main I/O task complete. Stopping endpoint I/O tasks and synchronizing on shutdown.");

    // Drop our endpoint I/O task channels, which will cause them to shut down once they've processed all outstanding
    // requests in their respective channel. We wait for that to happen by synchronizing on the task barrier.
    //
    // Once all tasks have completed, we signal back to the main component task that the I/O loop has shutdown.
    drop(endpoint_txs);
    task_barrier.wait().await;

    debug!("All endpoint I/O tasks have stopped. Main I/O task shutting down.");

    let _ = io_shutdown_tx.send(());
}

async fn run_endpoint_io_loop<S, B>(
    mut requests_rx: mpsc::Receiver<(usize, Request<B>)>, task_barrier: Arc<Barrier>, service: S,
    telemetry: ComponentTelemetry, endpoint: ResolvedEndpoint,
) where
    S: Service<Request<B>, Response = hyper::Response<Incoming>> + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
    B: Body,
{
    let endpoint_url = endpoint.endpoint().to_string();
    debug!(endpoint = endpoint_url, "Starting endpoint I/O task.");

    // Build our endpoint service.
    //
    // This is where we'll modify the incoming request for our our specific endpoint, such as setting the host portion
    // of the URI, adding the API key as a header, and so on.
    let mut service = ServiceBuilder::new()
        // Set the request's URI to the endpoint's URI, and add the API key as a header.
        .map_request(for_resolved_endpoint::<B>(endpoint))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info::<B>())
        .service(service);

    // Process all requests until the channel is closed.
    while let Some((metrics_count, request)) = requests_rx.recv().await {
        match service.call(request).await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    debug!(endpoint = endpoint_url, %status, "Request sent.");

                    telemetry.events_sent().increment(metrics_count as u64);
                } else {
                    telemetry.http_failed_send().increment(1);
                    telemetry.events_dropped_http().increment(metrics_count as u64);

                    match response.into_body().collect().await {
                        Ok(body) => {
                            let body = body.to_bytes();
                            let body_str = String::from_utf8_lossy(&body[..]);
                            error!(endpoint = endpoint_url, %status, "Received non-success response. Body: {}", body_str);
                        }
                        Err(e) => {
                            error!(endpoint = endpoint_url, %status, error = %e, "Failed to read response body of non-success response.");
                        }
                    }
                }
            }
            Err(e) => {
                let e: tower::BoxError = e.into();
                error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
            }
        }
    }

    debug!(
        endpoint = endpoint_url,
        "Requests channel for endpoint I/O task complete. Synchronizing on shutdown."
    );

    // Signal to the main I/O task that we've finished.
    task_barrier.wait().await;
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
