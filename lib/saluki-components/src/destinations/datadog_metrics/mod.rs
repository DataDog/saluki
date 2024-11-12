use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderValue, Method, Request, Response, Uri};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use metrics::Counter;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{destinations::*, ComponentContext, MetricsBuilder},
    pooling::{FixedSizeObjectPool, ObjectPool},
    task::spawn_traced,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_event::DataType;
use saluki_io::{
    buf::{BytesBuffer, ChunkedBuffer, FixedSizeVec, ReadWriteIoBuffer},
    net::{
        client::{http::HttpClient, replay::ReplayBody},
        util::retry::{DefaultHttpRetryPolicy, ExponentialBackoff},
    },
};
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, PickFirst};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tower::{BoxError, Service};
use tracing::{debug, error, trace};

mod endpoint;
use self::endpoint::endpoints::{
    calculate_api_endpoint, create_single_domain_resolvers, AdditionalEndpoints, SingleDomainResolver,
};

mod request_builder;
use self::request_builder::{MetricsEndpoint, RequestBuilder};

const DEFAULT_SITE: &str = "datadoghq.com";
const RB_BUFFER_POOL_COUNT: usize = 128;
const RB_BUFFER_POOL_BUF_SIZE: usize = 32_768;

fn default_site() -> String {
    DEFAULT_SITE.to_owned()
}

#[derive(Clone)]
struct Metrics {
    events_sent: Counter,
    bytes_sent: Counter,
    events_dropped_http: Counter,
    events_dropped_encoder: Counter,
    http_failed_send: Counter,
}

impl Metrics {
    fn from_component_context(context: ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(context);

        Self {
            events_sent: builder.register_counter("component_events_sent_total"),
            bytes_sent: builder.register_counter("component_bytes_sent_total"),
            events_dropped_http: builder.register_counter_with_labels(
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "http_failure")],
            ),
            events_dropped_encoder: builder.register_counter_with_labels(
                "component_events_dropped_total",
                &[("intentional", "false"), ("drop_reason", "encoder_failure")],
            ),
            http_failed_send: builder
                .register_counter_with_labels("component_errors_total", &[("error_type", "http_send")]),
        }
    }

    fn events_sent(&self) -> &Counter {
        &self.events_sent
    }

    #[allow(dead_code)]
    fn bytes_sent(&self) -> &Counter {
        &self.bytes_sent
    }

    fn events_dropped_http(&self) -> &Counter {
        &self.events_dropped_http
    }

    fn events_dropped_encoder(&self) -> &Counter {
        &self.events_dropped_encoder
    }

    fn http_failed_send(&self) -> &Counter {
        &self.http_failed_send
    }
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
#[serde_as]
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
    #[serde_as(as = "PickFirst<(DisplayFromStr, _)>")]
    #[serde(default, rename = "additional_endpoints")]
    endpoints: AdditionalEndpoints,
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

    fn api_base(&self) -> Result<Uri, GenericError> {
        calculate_api_endpoint(self.dd_url.as_deref(), &self.site)
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

        let api_base = self.api_base()?;

        let rb_buffer_pool = create_request_builder_buffer_pool();
        let series_request_builder = RequestBuilder::new(
            self.api_key.clone(),
            api_base.clone(),
            MetricsEndpoint::Series,
            rb_buffer_pool.clone(),
        )
        .await?;
        let sketches_request_builder = RequestBuilder::new(
            self.api_key.clone(),
            api_base,
            MetricsEndpoint::Sketches,
            rb_buffer_pool,
        )
        .await?;

        let resolvers = create_single_domain_resolvers(&self.endpoints)?;

        Ok(Box::new(DatadogMetrics {
            service,
            series_request_builder,
            sketches_request_builder,
            resolvers,
        }))
    }
}

impl MemoryBounds for DatadogMetricsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The request builder buffer pool is shared between both the series and the sketches request builder, so we
        // only count it once.
        let rb_buffer_pool_size = RB_BUFFER_POOL_COUNT * RB_BUFFER_POOL_BUF_SIZE;

        // Each request builder has a scratch buffer for encoding.
        //
        // TODO: Since it's just a `Vec<u8>`, it could trivially be expanded/grown for encoding larger payloads... which
        // we don't really have a good answer to here. Best thing would be to change the encoding logic to write
        // directly to the compressor but we have the current intermediate step to cope with avoiding writing more than
        // the (un)compressed payload limits and it will take a little work to eliminate that, I believe.
        let scratch_buffer_size = request_builder::SCRATCH_BUF_CAPACITY * 2;

        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            //
            // TODO: This type signature is _ugly_, and it would be nice to improve it somehow.
            .with_single_value::<DatadogMetrics<
                FixedSizeObjectPool<BytesBuffer>,
                HttpClient<ReplayBody<ChunkedBuffer<FixedSizeObjectPool<BytesBuffer>>>>,
            >>()
            // Capture the size of our buffer pool and scratch buffer.
            .with_fixed_amount(rb_buffer_pool_size)
            .with_fixed_amount(scratch_buffer_size)
            // Capture the size of the requests channel.
            //
            // TODO: This type signature is _ugly_, and it would be nice to improve it somehow.
            .with_array::<(
                usize,
                Request<ReplayBody<ChunkedBuffer<FixedSizeObjectPool<BytesBuffer>>>>,
            )>(32);
    }
}

pub struct DatadogMetrics<O, S>
where
    O: ObjectPool + 'static,
    O::Item: ReadWriteIoBuffer,
    S: Service<Request<ReplayBody<ChunkedBuffer<O>>>> + 'static,
{
    service: S,
    series_request_builder: RequestBuilder<O>,
    sketches_request_builder: RequestBuilder<O>,
    resolvers: Vec<SingleDomainResolver>,
}

#[async_trait]
impl<O, S> Destination for DatadogMetrics<O, S>
where
    O: ObjectPool + 'static,
    O::Item: ReadWriteIoBuffer,
    S: Service<Request<ReplayBody<ChunkedBuffer<O>>>, Response = Response<Incoming>> + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
{
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let Self {
            mut series_request_builder,
            mut sketches_request_builder,
            service,
            resolvers,
        } = *self;

        let mut health = context.take_health_handle();

        // Spawn our IO task to handle sending requests.
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();
        let (requests_tx, requests_rx) = mpsc::channel(32);
        let metrics = Metrics::from_component_context(context.component_context());
        spawn_traced(run_io_loop(
            requests_rx,
            io_shutdown_tx,
            service,
            metrics.clone(),
            resolvers,
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
                                            metrics.events_dropped_encoder().increment(1);
                                            continue;
                                        }
                                    };

                                    // Get the flushed request and enqueue it to be sent.
                                    match request_builder.flush().await {
                                        Ok(Some(request)) => {
                                            if requests_tx.send(request).await.is_err() {
                                                return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                            }
                                        }
                                        Ok(None) => unreachable!(
                                            "request builder indicated required flush, but no request was given during flush"
                                        ),
                                        Err(e) => {
                                            // TODO: Increment a counter here that metrics were dropped due to a flush failure.
                                            if e.is_recoverable() {
                                                // If the error is recoverable, we'll hold on to the metric to retry it later.
                                                continue;
                                            } else {
                                                return Err(GenericError::from(e).context("Failed to flush request."));
                                            }
                                        }
                                    };

                                    // Now try to encode the metric again. If it fails again, we'll just log it because it shouldn't
                                    // be possible to fail at this point, otherwise we would have already caught that the first
                                    // time.
                                    if let Err(e) = request_builder.encode(metric_to_retry).await {
                                        error!(error = %e, "Failed to encode metric.");
                                        metrics.events_dropped_encoder().increment(1);
                                    }
                                }
                            }
                        }

                        debug!("All event buffers processed.");

                        // Once we've encoded and written all metrics, we flush the request builders to generate a request with
                        // anything left over. Again, we'll  enqueue those requests to be sent immediately.
                        match series_request_builder.flush().await.error_context("Failed to flush request.")? {
                            Some(request) => {
                                debug!("Flushed request from series request builder. Sending to I/O task...");
                                if requests_tx.send(request).await.is_err() {
                                    return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                }
                            },
                            None => {
                                trace!("No flushed request from series request builder.");
                            },
                        }

                        match sketches_request_builder.flush().await.error_context("Failed to flush request.")? {
                           Some(request) => {
                                debug!("Flushed request from sketches request builder. Sending to I/O task...");
                                if requests_tx.send(request).await.is_err() {
                                    return Err(generic_error!("Failed to send request to IO task: receiver dropped."));
                                }
                            }
                            None => {
                                trace!("No flushed request from sketches request builder.");
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

async fn run_io_loop<O, S>(
    mut requests_rx: mpsc::Receiver<(usize, Request<ReplayBody<ChunkedBuffer<O>>>)>,
    io_shutdown_tx: oneshot::Sender<()>, mut service: S, metrics: Metrics, resolvers: Vec<SingleDomainResolver>,
) where
    O: ObjectPool + 'static,
    O::Item: ReadWriteIoBuffer,
    S: Service<Request<ReplayBody<ChunkedBuffer<O>>>, Response = hyper::Response<Incoming>> + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
{
    // Loop and process all incoming requests.
    while let Some((metrics_count, request)) = requests_rx.recv().await {
        let cloned_request = request.clone();
        let mut requests = vec![request];
        for resolver in &resolvers {
            let mut parts = cloned_request.uri().clone().into_parts();
            parts.authority = Some(resolver.domain().parse().expect("invalid authority"));
            let new_uri = Uri::from_parts(parts).expect("Failed to construct new URI");
            for api_key in resolver.api_keys() {
                let mut headers = cloned_request.headers().clone();
                headers.insert("DD-API-KEY", HeaderValue::from_str(api_key).unwrap());
                let mut new_request = Request::builder();
                let body = cloned_request.body().clone();
                for (key, value) in headers.iter() {
                    new_request = new_request.header(key, value);
                }
                match new_request.method(Method::POST).uri(new_uri.clone()).body(body) {
                    Ok(new_request) => {
                        requests.push(new_request);
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to build request for Domain: {}", resolver.domain());
                    }
                }
            }
        }
        // TODO: We should emit the number of bytes sent, which was trivial to do prior to adding retry support via
        // `ReplayBody<B>`. Even if we could delve into the inner body that is being wrapped, what we can't track is how
        // the number of bytes sent during retry.
        //
        // This perhaps points to a need to more holistically capture bytes sent through the client itself, which not
        // only covers potential retries but also stuff like HTTP headers, and so on.

        for request in requests {
            match service.call(request).await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        debug!(%status, "Request sent.");

                        metrics.events_sent().increment(metrics_count as u64);
                    } else {
                        metrics.http_failed_send().increment(1);
                        metrics.events_dropped_http().increment(metrics_count as u64);

                        match response.into_body().collect().await {
                            Ok(body) => {
                                let body = body.to_bytes();
                                let body_str = String::from_utf8_lossy(&body[..]);
                                error!(%status, "Received non-success response. Body: {}", body_str);
                            }
                            Err(e) => {
                                error!(%status, error = %e, "Failed to read response body of non-success response.");
                            }
                        }
                    }
                }
                Err(e) => {
                    let e: tower::BoxError = e.into();
                    error!(error = %e, error_source = ?e.source(), "Failed to send request.");
                }
            }
        }
    }

    // Signal back to the main task that we've stopped.
    let _ = io_shutdown_tx.send(());
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
