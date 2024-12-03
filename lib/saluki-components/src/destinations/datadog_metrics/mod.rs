use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use http::{
    uri::{Authority, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::{GenericConfiguration, RefreshableConfiguration};
use saluki_core::{
    components::{destinations::*, ComponentContext},
    pooling::{FixedSizeObjectPool, ObjectPool},
    task::spawn_traced,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_event::DataType;
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec, FrozenChunkedBytesBuffer},
    net::{
        client::http::{EndpointTelemetryLayer, HttpClient},
        util::retry::{DefaultHttpRetryPolicy, ExponentialBackoff},
    },
};
use serde::Deserialize;
use tokio::{
    select,
    sync::{mpsc, oneshot, Barrier},
};
use tower::{BoxError, Service, ServiceBuilder};
use tracing::{debug, error};

mod endpoint;
use self::endpoint::endpoints::{calculate_resolved_endpoint, AdditionalEndpoints, ResolvedEndpoint};

mod request_builder;
use self::request_builder::{MetricsEndpoint, RequestBuilder};

mod telemetry;
use self::telemetry::ComponentTelemetry;

const DEFAULT_SITE: &str = "datadoghq.com";
const RB_BUFFER_POOL_COUNT: usize = 128;
const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;

static DD_AGENT_VERSION_HEADER: HeaderName = HeaderName::from_static("dd-agent-version");
static DD_API_KEY_HEADER: HeaderName = HeaderName::from_static("dd-api-key");

fn default_site() -> String {
    DEFAULT_SITE.to_owned()
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
    /// The API key to use.
    api_key: String,

    /// The site to send metrics to.
    ///
    /// This is the base domain for the Datadog site in which the API key originates from. This will generally be a
    /// portion of the domain used to access the Datadog UI, such as `datadoghq.com` or `us5.datadoghq.com`.
    ///
    /// Defaults to `datadoghq.com`.
    #[serde(default = "default_site")]
    site: String,

    /// The full URL base to send metrics to.
    ///
    /// This takes precedence over `site`, and is not altered in any way. This can be useful to specifying the exact
    /// endpoint used, such as when looking to change the scheme (e.g. `http` vs `https`) or specifying a custom port,
    /// which are both useful when proxying traffic to an intermediate destination before forwarding to Datadog.
    ///
    /// Defaults to unset.
    #[serde(default)]
    dd_url: Option<String>,

    /// The request timeout for forwarding metrics, in seconds.
    ///
    /// Defaults to 20 seconds.
    #[serde(default = "default_request_timeout_secs", rename = "forwarder_timeout")]
    request_timeout_secs: u64,

    /// The minimum backoff factor to use when retrying requests.
    ///
    /// Controls the the interval range that a calculated backoff duration can fall within, such that with a minimum
    /// backoff factor of 2.0, calculated backoff durations will fall between `d/2` and `d`, where `d` is the calculated
    /// backoff duration using a purely exponential growth strategy.
    ///
    /// Defaults to 2.
    #[serde(default = "default_request_backoff_factor", rename = "forwarder_backoff_factor")]
    request_backoff_factor: f64,

    /// The base growth rate of the backoff duration when retrying requests, in seconds.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_request_backoff_base", rename = "forwarder_backoff_base")]
    request_backoff_base: f64,

    /// The upper bound of the backoff duration when retrying requests, in seconds.
    ///
    /// Defaults to 64 seconds.
    #[serde(default = "default_request_backoff_max", rename = "forwarder_backoff_max")]
    request_backoff_max: f64,

    /// The amount to decrease the error count by when a request is successful.
    ///
    /// This essentially controls how quickly we forget about the number of previous errors when calculating the next
    /// backoff duration for a request that must be retried.
    ///
    /// Increasing this value should be done with caution, as it can lead to more retries being attempted in the same
    /// period of time when downstream services are flapping.
    ///
    /// Defaults to 2.
    #[serde(
        default = "default_request_recovery_error_decrease_factor",
        rename = "forwarder_recovery_interval"
    )]
    request_recovery_error_decrease_factor: u32,

    /// Whether or not a successful request should completely the error count.
    ///
    /// Defaults to false.
    #[serde(default = "default_request_recovery_reset", rename = "forwarder_recovery_reset")]
    request_recovery_reset: bool,

    /// Enables sending data to multiple endpoints and/or with multiple API keys via dual shipping.
    ///
    /// Defaults to empty.
    #[serde(default, rename = "additional_endpoints")]
    endpoints: AdditionalEndpoints,

    #[serde(skip)]
    config_refresher: Arc<RefreshableConfiguration>,
    additional_endpoints: AdditionalEndpoints,
}

fn default_request_timeout_secs() -> u64 {
    20
}

fn default_request_backoff_factor() -> f64 {
    2.0
}

fn default_request_backoff_base() -> f64 {
    2.0
}

fn default_request_backoff_max() -> f64 {
    64.0
}

fn default_request_recovery_error_decrease_factor() -> u32 {
    2
}

fn default_request_recovery_reset() -> bool {
    false
}

impl DatadogMetricsConfiguration {
    /// Creates a new `DatadogMetricsConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Add option to retrieve configuration values from a `RefreshableConfiguration`.
    pub fn add_refreshable_configuration(&mut self, refresher: Arc<RefreshableConfiguration>) {
        self.config_refresher = refresher;
    }
}

#[async_trait]
impl DestinationBuilder for DatadogMetricsConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.request_backoff_base),
            Duration::from_secs_f64(self.request_backoff_max),
            self.request_backoff_factor,
        );

        let recovery_error_decrease_factor =
            (!self.request_recovery_reset).then_some(self.request_recovery_error_decrease_factor);
        let retry_policy = DefaultHttpRetryPolicy::with_backoff(retry_backoff)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor);

        let service = HttpClient::builder().with_retry_policy(retry_policy).build()?;

        // Resolve all endpoints that we'll be forwarding metrics to.
        let primary_endpoint = calculate_resolved_endpoint(self.dd_url.as_deref(), &self.site, &self.api_key)
            .error_context("Failed parsing/resolving the primary destination endpoint.")?;

        let additional_endpoints = self
            .additional_endpoints
            .resolved_endpoints()
            .error_context("Failed parsing/resolving the additional destination endpoints.")?;

        let mut endpoints = additional_endpoints;
        endpoints.insert(0, primary_endpoint);

        // Create our request builders.
        let rb_buffer_pool = create_request_builder_buffer_pool();
        let series_request_builder = RequestBuilder::new(MetricsEndpoint::Series, rb_buffer_pool.clone()).await?;
        let sketches_request_builder = RequestBuilder::new(MetricsEndpoint::Sketches, rb_buffer_pool).await?;

        let refresher = self.config_refresher.clone();

        Ok(Box::new(DatadogMetrics {
            service,
            series_request_builder,
            sketches_request_builder,
            endpoints,
            refresher,
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
    series_request_builder: RequestBuilder<O>,
    sketches_request_builder: RequestBuilder<O>,
    endpoints: Vec<ResolvedEndpoint>,
    refresher: Arc<RefreshableConfiguration>,
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
            endpoints,
            refresher,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our IO task to handle sending requests.
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();
        let (requests_tx, requests_rx) = mpsc::channel(32);
        let telemetry = ComponentTelemetry::from_component_context(context.component_context());
        spawn_traced(run_io_loop(
            requests_rx,
            io_shutdown_tx,
            service,
            telemetry.clone(),
            context.component_context(),
            endpoints,
            refresher,
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
    telemetry: ComponentTelemetry, component_context: ComponentContext, resolved_endpoints: Vec<ResolvedEndpoint>,
    refresher: Arc<RefreshableConfiguration>,
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
            component_context.clone(),
            resolved_endpoint,
            refresher.clone(),
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
    telemetry: ComponentTelemetry, context: ComponentContext, endpoint: ResolvedEndpoint,
    refresher: Arc<RefreshableConfiguration>,
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
        .map_request(for_resolved_endpoint::<B>(endpoint, refresher.clone()))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info::<B>())
        // Add telemetry about the requests.
        .layer(EndpointTelemetryLayer::from_component_context(context).with_endpoint_name_fn(get_metrics_endpoint_name))
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

#[allow(unused)]
fn for_resolved_endpoint<B>(
    endpoint: ResolvedEndpoint, refresher: Arc<RefreshableConfiguration>,
) -> impl Fn(Request<B>) -> Request<B> + Clone {
    let new_uri_authority = Authority::try_from(endpoint.endpoint().authority())
        .expect("should not fail to construct new endpoint authority");
    let new_uri_scheme =
        Scheme::try_from(endpoint.endpoint().scheme()).expect("should not fail to construct new endpoint scheme");
    let api_key_value =
        HeaderValue::from_str(endpoint.api_key()).expect("should not fail to construct API key header value");
    let api_key = refresher
        .get_typed::<String>("api_key")
        .expect("should not fail to retrieve API key from refreshable configuration");
    let api_key_value = HeaderValue::from_str(&api_key).expect("should not fail to construct API key header value");
    move |mut request| {
        // Build an updated URI by taking the endpoint URL and slapping the request's URI path on the end of it.
        let new_uri = Uri::builder()
            .scheme(new_uri_scheme.clone())
            .authority(new_uri_authority.clone())
            .path_and_query(request.uri().path_and_query().expect("request path must exist").clone())
            .build()
            .expect("should not fail to construct new URI");

        *request.uri_mut() = new_uri;

        // Add the API key as a header.
        request
            .headers_mut()
            .insert(DD_API_KEY_HEADER.clone(), api_key_value.clone());

        request
    }
}

fn with_version_info<B>() -> impl Fn(Request<B>) -> Request<B> + Clone {
    let app_details = saluki_metadata::get_app_details();
    let formatted_full_name = app_details
        .full_name()
        .replace(" ", "-")
        .replace("_", "-")
        .to_lowercase();

    let agent_version_header_value = HeaderValue::from_static(app_details.version().raw());
    let raw_user_agent_header_value = format!("{}/{}", formatted_full_name, app_details.version().raw());
    let user_agent_header_value = HeaderValue::from_maybe_shared(raw_user_agent_header_value)
        .expect("should not fail to construct User-Agent header value");

    move |mut request| {
        request
            .headers_mut()
            .insert(DD_AGENT_VERSION_HEADER.clone(), agent_version_header_value.clone());
        request
            .headers_mut()
            .insert(http::header::USER_AGENT, user_agent_header_value.clone());
        request
    }
}

fn get_metrics_endpoint_name(uri: &Uri) -> Option<&'static str> {
    match uri.path() {
        "/api/v2/series" => Some("series_v2"),
        "/api/beta/sketches" => Some("sketches_v2"),
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
