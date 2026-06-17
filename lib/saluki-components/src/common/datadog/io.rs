//! Datadog forwarder I/O.
//!
//! # Missing
//!
//! - Avoid breaking apart `TransactionForwarder` only to work around `#[allow(clippy::too_many_arguments)]`.
//! - Avoid initializing the process-wide crypto provider from tests.

use std::{
    collections::VecDeque,
    error::Error as _,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Buf;
use futures::FutureExt as _;
use http::{Request, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::{body::Incoming, Response};
use saluki_common::{hash::hash_single_stable, task::spawn_traced_named, time::get_unix_timestamp};
use saluki_core::components::ComponentContext;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{
    client::http::{into_client_body, HttpClient, HttpClientBuilder},
    util::{
        middleware::{RetryCircuitBreakerError, RetryCircuitBreakerLayer},
        retry::{DiskUsageRetrieverImpl, PersistedQueueArgs, PushResult, RetryQueue, Retryable},
    },
};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot, Barrier},
    task::JoinSet,
};
use tower::{Service, ServiceBuilder, ServiceExt as _};
use tracing::{debug, error, warn};

use super::{
    config::ForwarderConfiguration,
    endpoints::{EndpointRoute, LiveForwarderConfig, ResolvedEndpoint, RoutableEndpoint},
    middleware::{for_resolved_endpoint, with_allow_arbitrary_tags, with_version_info},
    retry_capacity::{TrafficRateWindow, RETRY_QUEUE_CAPACITY_BUCKET_DURATION_SECS},
    telemetry::{ComponentTelemetry, SharedTransactionQueueTelemetry, TransactionQueueTelemetry},
    transaction::{Metadata, Transaction, TransactionBody},
    validation::ApiKeyValidator,
    METRIC_INTAKE_PATHS,
};

/// Size of buffer chunks for request builder buffers.
///
/// Used to influence the size of chunks in `ChunkedBytesBuffer`.
pub const RB_BUFFER_CHUNK_SIZE: usize = 32 * 1024; // 32 KB

/// A handle to the transaction forwarder.
pub struct Handle<B>
where
    B: Buf + Clone,
{
    transactions_tx: mpsc::Sender<Transaction<B>>,
    io_shutdown_rx: oneshot::Receiver<()>,
}

impl<B> Handle<B>
where
    B: Buf + Clone,
{
    /// Sends a transaction to the forwarder.
    ///
    /// # Errors
    ///
    /// If the endpoint I/O task has unexpectedly stopped and can no longer accept transactions, an error will be returned.
    pub async fn send_transaction(&self, transaction: Transaction<B>) -> Result<(), GenericError> {
        match self.transactions_tx.send(transaction).await {
            Ok(()) => Ok(()),
            Err(_) => Err(generic_error!("Failed to send request to I/O task: receiver dropped.")),
        }
    }

    /// Triggers the forwarder to shutdown and waits to shutdown to complete.
    pub async fn shutdown(self) {
        let Self {
            transactions_tx,
            io_shutdown_rx,
        } = self;

        // Drop the sender side of the transaction channel, which will propagate the actual closure to the main I/O task.
        drop(transactions_tx);

        // Wait for the main I/O task to signal that it has shutdown.
        io_shutdown_rx.await.expect("I/O task has already shutdown.");
    }
}

/// Transaction forwarder for Datadog endpoints.
pub struct TransactionForwarder<B> {
    context: ComponentContext,
    config: ForwarderConfiguration,
    live_config: Option<LiveForwarderConfig>,
    telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder,
    client: HttpClient,
    endpoints: Vec<RoutableEndpoint>,
    endpoint_request_mapper_factory: EndpointRequestMapperFactory<B>,
    _marker: std::marker::PhantomData<B>,
}

/// Builds request mappers for resolved endpoints.
pub(crate) type EndpointRequestMapperFactory<B> =
    Arc<dyn Fn(ResolvedEndpoint) -> EndpointRequestMapper<B> + Send + Sync>;

/// Maps requests for a resolved endpoint before they are sent.
pub(crate) type EndpointRequestMapper<B> =
    Box<dyn FnMut(Request<TransactionBody<B>>) -> Request<TransactionBody<B>> + Send>;

fn default_endpoint_request_mapper<B: 'static>(endpoint: ResolvedEndpoint) -> EndpointRequestMapper<B> {
    let mapper = for_resolved_endpoint(endpoint);
    Box::new(mapper)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TlsCertificateValidation {
    Enabled,
    Disabled,
}

impl TlsCertificateValidation {
    const fn from_forwarder_config(config: &ForwarderConfiguration) -> Self {
        if config.skip_ssl_validation() {
            Self::Disabled
        } else {
            Self::Enabled
        }
    }

    fn apply_to(self, client_builder: HttpClientBuilder) -> Result<HttpClientBuilder, GenericError> {
        self.ensure_supported()?;

        Ok(match self {
            Self::Enabled => client_builder,
            Self::Disabled => {
                warn!(
                    config_key = "skip_ssl_validation",
                    "TLS certificate validation is disabled for Datadog intake forwarding."
                );
                client_builder.with_tls_config(|builder| builder.danger_accept_invalid_certs())
            }
        })
    }

    fn ensure_supported(self) -> Result<(), GenericError> {
        #[cfg(feature = "fips")]
        if matches!(self, Self::Disabled) {
            return Err(generic_error!(
                "`skip_ssl_validation: true` is unsupported in FIPS mode because disabling TLS certificate validation is not FIPS-compliant."
            ));
        }

        Ok(())
    }
}

impl<B> TransactionForwarder<B>
where
    B: Body + Buf + Clone + Unpin + Send + Sync + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    /// Creates a new `TransactionForwarder` instance from the given configuration.
    pub fn from_config<F>(
        context: ComponentContext, config: ForwarderConfiguration, live_config: Option<LiveForwarderConfig>,
        endpoint_name: F, telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        Self::from_config_with_endpoint_request_mapper(
            context,
            config,
            live_config,
            endpoint_name,
            telemetry,
            metrics_builder,
            Arc::new(default_endpoint_request_mapper::<B>),
        )
    }

    /// Creates a new `TransactionForwarder` with a custom endpoint request mapper.
    pub(crate) fn from_config_with_endpoint_request_mapper<F>(
        context: ComponentContext, config: ForwarderConfiguration, live_config: Option<GenericConfiguration>,
        endpoint_name: F, telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
        endpoint_request_mapper_factory: EndpointRequestMapperFactory<B>,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let endpoints = config.build_routable_endpoints(live_config.clone())?;
        let mut client_builder = HttpClient::builder()
            .with_request_timeout(config.request_timeout())
            .with_max_idle_conns_per_host(config.max_idle_connections_per_host())
            .with_min_tls_version(config.min_tls_version())
            .with_http_protocol(config.http_protocol())
            .with_bytes_sent_counter(telemetry.bytes_sent().clone())
            .with_endpoint_telemetry(metrics_builder.clone(), Some(endpoint_name));
        if let Some(path) = config.ssl_key_log_file_path() {
            client_builder = client_builder.with_tls_config(|builder| builder.with_key_log_file(path));
        }
        if let Some(proxy) = config.proxy() {
            client_builder = client_builder.with_proxies(proxy.build()?);
        }

        if config.connection_reset_interval() > Duration::ZERO {
            client_builder = client_builder.with_connection_age_limit(config.connection_reset_interval());
        }

        client_builder = TlsCertificateValidation::from_forwarder_config(&config).apply_to(client_builder)?;

        let client = client_builder.build()?;

        Ok(Self {
            context,
            config,
            live_config,
            telemetry,
            metrics_builder,
            client,
            endpoints,
            endpoint_request_mapper_factory,
            _marker: std::marker::PhantomData,
        })
    }

    /// Spawns the I/O task for the forwarder, and any associated endpoint I/O tasks.
    ///
    /// Returns a `Handle` that can be used to send transactions to the forwarder, as well as eventually shut it down in
    /// an orderly fashion.
    pub async fn spawn(self) -> Handle<B> {
        let (transactions_tx, transactions_rx) = mpsc::channel(8);
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();

        // TODO: do not destructure self as a way to fix the #[allow(clippy::too_many_arguments)] annotations
        let Self {
            context,
            config,
            // The endpoints already carry their own live configuration handles for API-key refresh,
            // so the per-loop machinery does not need the handle separately.
            live_config: _,
            telemetry,
            metrics_builder,
            client,
            endpoints,
            endpoint_request_mapper_factory,
            _marker,
        } = self;

        spawn_traced_named(
            "dd-txn-forwarder-io-loop",
            run_io_loop(
                transactions_rx,
                io_shutdown_tx,
                context,
                config,
                client,
                telemetry,
                metrics_builder,
                endpoints,
                endpoint_request_mapper_factory,
            ),
        );

        Handle {
            transactions_tx,
            io_shutdown_rx,
        }
    }

    /// Returns API key validation for the startup endpoint set.
    pub(crate) fn api_key_validator(&self) -> ApiKeyValidator {
        ApiKeyValidator::new(
            self.endpoints.clone(),
            self.client.clone(),
            self.live_config.clone(),
            self.config.api_key_validation_interval(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_io_loop<B>(
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, io_shutdown_tx: oneshot::Sender<()>,
    context: ComponentContext, config: ForwarderConfiguration, service: HttpClient, telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder, resolved_endpoints: Vec<RoutableEndpoint>,
) where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // Spawn an endpoint I/O task for each endpoint we're configured to send to, which we'll forward transactions to.
    let mut endpoint_txs = Vec::new();
    let has_metrics_primary = resolved_endpoints
        .iter()
        .any(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary);
    let task_barrier = Arc::new(Barrier::new(resolved_endpoints.len() + 1));
    let shared_txnq_telemetry = SharedTransactionQueueTelemetry::from_builder(&metrics_builder);

    for routable_endpoint in resolved_endpoints {
        let (route, resolved_endpoint) = routable_endpoint.into_parts();
        let endpoint_url = resolved_endpoint.endpoint().to_string();
        let endpoint_domain = resolved_endpoint.endpoint().origin().ascii_serialization();

        let txnq_telemetry =
            TransactionQueueTelemetry::from_builder(&metrics_builder, &endpoint_url, shared_txnq_telemetry.clone());

        let (endpoint_tx, endpoint_rx) = mpsc::channel(8);
        let task_barrier = Arc::clone(&task_barrier);

        let task_name = format!("dd-txn-forwarder-io-loop-{}", resolved_endpoint.endpoint().authority());
        spawn_traced_named(
            task_name,
            run_endpoint_io_loop(
                endpoint_rx,
                task_barrier,
                context.clone(),
                config.clone(),
                service.clone(),
                telemetry.clone(),
                txnq_telemetry,
                resolved_endpoint,
            ),
        );

        endpoint_txs.push(EndpointSender {
            endpoint_url,
            endpoint_domain,
            route,
            tx: endpoint_tx,
        });
    }

    // Listen for transactions to forward, and send a copy of each one to the matching endpoint I/O tasks.
    while let Some(transaction) = transactions_rx.recv().await {
        let is_metrics_request = is_metrics_request_uri(transaction.request_uri());
        for endpoint_sender in &endpoint_txs {
            if !should_route_to_endpoint(is_metrics_request, has_metrics_primary, endpoint_sender.route) {
                continue;
            }

            if endpoint_sender.tx.send(transaction.clone()).await.is_err() {
                telemetry.track_permanently_failed_transaction(
                    transaction.metadata(),
                    None,
                    &endpoint_sender.endpoint_domain,
                );
                error!(
                    endpoint = %endpoint_sender.endpoint_url,
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

struct EndpointSender<B>
where
    B: Buf + Clone,
{
    endpoint_url: String,
    endpoint_domain: String,
    route: EndpointRoute,
    tx: mpsc::Sender<Transaction<B>>,
}

fn is_metrics_request_uri(uri: &Uri) -> bool {
    METRIC_INTAKE_PATHS.contains(&uri.path())
}

fn should_route_to_endpoint(is_metrics_request: bool, has_metrics_primary: bool, route: EndpointRoute) -> bool {
    match (is_metrics_request, has_metrics_primary, route) {
        (true, true, EndpointRoute::Primary) => false,
        (true, true, EndpointRoute::MetricsPrimary) => true,
        (true, false, EndpointRoute::Primary) => true,
        (true, false, EndpointRoute::MetricsPrimary) => false,
        (true, _, EndpointRoute::Additional) => true,
        (false, _, EndpointRoute::Primary | EndpointRoute::Additional) => true,
        (false, _, EndpointRoute::MetricsPrimary) => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_endpoint_io_loop<B>(
    mut txns_rx: mpsc::Receiver<Transaction<B>>, task_barrier: Arc<Barrier>, context: ComponentContext,
    config: ForwarderConfiguration, service: HttpClient, telemetry: ComponentTelemetry,
    txnq_telemetry: TransactionQueueTelemetry, endpoint: ResolvedEndpoint,
) where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let queue_id = generate_retry_queue_id(context, &endpoint);
    let endpoint_url = endpoint.endpoint().to_string();
    let endpoint_domain = endpoint.endpoint().origin().ascii_serialization();
    debug!(
        endpoint_url,
        endpoint_concurrency = config.endpoint_concurrency(),
        "Starting endpoint I/O task."
    );
    // Build our endpoint service.
    //
    // This is where we'll modify the incoming transaction for our our specific endpoint, such as setting the host portion
    // of the URI, adding the API key as a header, and so on.
    //
    // The body type conversion from `TransactionBody<B>` to `ClientBody` happens as the innermost layer,
    // after the retry circuit breaker. This ensures that `RetryCircuitBreakerError::Open(req)` returns
    // the original `Request<TransactionBody<B>>` so we can reassemble it into a `Transaction<B>` for re-enqueuing.
    let mut service = ServiceBuilder::new()
        // Set the request's URI to the endpoint's URI, and add the API key as a header.
        .map_request(for_resolved_endpoint(endpoint))
        // Signal backend support for arbitrary tag values when configured.
        .map_request(with_allow_arbitrary_tags(config.allow_arbitrary_tags()))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info())
        .concurrency_limit(config.endpoint_concurrency())
        .layer(RetryCircuitBreakerLayer::new(
            config.retry().to_default_http_retry_policy(),
        ))
        .map_request(|req: Request<TransactionBody<B>>| req.map(into_client_body))
        .service(service);

    let mut retry_queue = RetryQueue::new(queue_id.clone(), config.retry().queue_max_size_bytes())
        .with_flush_to_disk_mem_ratio(config.retry().flush_to_disk_mem_ratio());

    // If the storage size is set, enable disk persistence for the retry queue.
    if config.retry().storage_max_size_bytes() > 0 {
        retry_queue = retry_queue
            .with_disk_persistence(PersistedQueueArgs {
                root_path: PathBuf::from(config.retry().storage_path()),
                max_on_disk_bytes: config.retry().storage_max_size_bytes(),
                storage_max_disk_ratio: config.retry().storage_max_disk_ratio(),
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(PathBuf::from(
                    config.retry().storage_path(),
                ))),
                max_age_days: config.retry().outdated_file_in_days(),
            })
            .await
            .unwrap_or_else(|e| {
                error!(endpoint_url, error = %e, "Failed to initialize disk persistence for retry queue. Transactions will not be persisted.");
                RetryQueue::new(queue_id, config.retry().queue_max_size_bytes())
                    .with_flush_to_disk_mem_ratio(config.retry().flush_to_disk_mem_ratio())
            });
    }
    let mut pending_txns = PendingTransactions::new(
        config.endpoint_buffer_size(),
        retry_queue,
        txnq_telemetry,
        MetaString::from(endpoint_domain.as_str()),
        config.retry().capacity_time_interval_secs(),
    );

    let mut in_flight = JoinSet::new();
    let mut done = false;

    loop {
        select! {
            // Try and drain the next transaction from our channel, and push it into the pending transactions queue.
            maybe_txn = txns_rx.recv(), if !done => match maybe_txn {
                Some(txn) => match pending_txns.push_high_priority(txn).await {
                    Ok(push_result) => track_queue_drops(&telemetry, &endpoint_domain, push_result),
                    Err(e) => error!(endpoint_url, error = %e, "Failed to enqueue transaction. Events may be permanently lost."),
                },
                None => {
                    // Our transactions channel has been closed, so mark ourselves as done which will stop any further
                    // transactions from being sent, but will allow in-flight transactions to complete.
                    done = true;
                    debug!(endpoint_url, "Requests channel for endpoint I/O task complete. Completing any in-flight requests...");
                }
            },

            // While we're not done and there are pending transactions, wait for the service to become ready and then
            // next the next available pending transaction.
            svc = service.ready(), if !done && !pending_txns.is_empty() => match svc {
                Ok(svc) => if let Some(txn) = pending_txns.pop().await {
                    let (metadata, request) = txn.into_parts();
                    in_flight.spawn(svc.call(request).map(move |result| (metadata, result)));

                    debug!(endpoint_url, "Request sent.");
                },
                Err(e) => match e {
                    RetryCircuitBreakerError::Service(e) => {
                        telemetry.track_sent_request_error();
                        error!(endpoint_url, error = %e, error_source = ?e.source(), "Unexpected error when querying service for readiness.");
                        break;
                    },
                    RetryCircuitBreakerError::Open(_) => unreachable!("should not get open error when querying service for readiness"),
                }
            },

            // Drive any in-flight transactions to completion.
            maybe_result = in_flight.join_next(), if !in_flight.is_empty() => {
                let task_result = maybe_result.expect("in_flight marked as not being empty");
                match task_result {
                    Ok((metadata, result)) => match result {
                        // We got a response -- maybe successful, maybe not -- so just process that.
                        Ok(http_response) => process_http_response(http_response, metadata, &telemetry, &endpoint_url, &endpoint_domain).await,

                        // The service itself encountered an error while sending the request or receiving the response:
                        // connection reset by peer, I/O error, etc.
                        Err(RetryCircuitBreakerError::Service(e)) => {
                            telemetry.track_permanently_failed_transaction(&metadata, None, &endpoint_domain);
                            error!(endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
                        },

                        // Our endpoint circuit breaker is open, which means this request either didn't go through at
                        // all or needs to be retried... so we'll re-enqueue it to the low-priority queue to be retried
                        // later.
                        Err(RetryCircuitBreakerError::Open(req)) => {
                            let reassembled_txn = Transaction::reassemble(metadata, req);
                            match pending_txns.push_low_priority(reassembled_txn).await {
                                Ok(push_result) => track_queue_drops(&telemetry, &endpoint_domain, push_result),
                                Err(e) => error!(endpoint_url, error = %e, "Failed to re-enqueue failed transaction. Events may be permanently lost."),
                            }
                        },
                    },

                    // Our transaction task itself failed, which means something weirdly bad happened: panic, etc.
                    Err(e) => {
                        error!(endpoint_url, error = %e, error_source = ?e.source(), "Request task failed to run to completion.");
                    }
                }
            },

            else => break,
        }
    }

    // Flush any outstanding transactions in the pending transactions queue, which will potentially enqueue them to disk
    // if we have disk persistent enabled for the retry queue.
    match pending_txns.flush().await {
        // If we successfully flushed the pending transactions, track the number of events that were dropped, if any.
        Ok(flush_result) => {
            debug!(
                items_dropped = flush_result.items_dropped,
                events_dropped = flush_result.events_dropped,
                "Flushed pending transactions prior to shutdown."
            );
            track_queue_drops(&telemetry, &endpoint_domain, flush_result);
        }
        Err(e) => {
            error!(endpoint_url, error = %e, "Failed to flush pending transactions. Events may be permanently lost.")
        }
    }

    debug!(
        endpoint_url,
        "Requests channel for endpoint I/O task complete. Synchronizing on shutdown."
    );

    // Signal to the main I/O task that we've finished.
    task_barrier.wait().await;
}

fn generate_retry_queue_id(context: ComponentContext, endpoint: &ResolvedEndpoint) -> String {
    // For additional endpoints we hash over the api_key_index (the stable position of this key in
    // the additional_endpoints config list) rather than the raw API key value. This means the queue
    // ID survives API key rotations pushed via the config stream — previously-persisted transactions
    // are still retried even after the key changes.
    //
    // For primary and OPW endpoints we omit the key entirely; a single primary endpoint per
    // component instance is sufficient to guarantee uniqueness.
    let hash = if let Some((raw_url, index)) = endpoint.additional_endpoint_queue_key() {
        hash_single_stable((context.component_id(), raw_url, index))
    } else {
        hash_single_stable((context.component_id(), endpoint.endpoint()))
    };

    let endpoint_host = endpoint
        .endpoint()
        .host_str()
        .expect("resolved endpoint must have a host");
    format!("{}/{}/{:x}", context.component_id(), endpoint_host, hash)
}

fn track_queue_drops(telemetry: &ComponentTelemetry, domain: &str, push_result: PushResult) {
    telemetry.track_dropped_items(push_result.items_dropped);
    telemetry.track_dropped_events(push_result.events_dropped);
    telemetry.track_data_points_dropped(domain, push_result.data_points_dropped);
}

async fn process_http_response(
    response: Response<Incoming>, metadata: Metadata, telemetry: &ComponentTelemetry, endpoint_url: &str, domain: &str,
) {
    let status = response.status();
    if status.is_success() {
        debug!(endpoint_url, %status, "Request completed.");

        // Reaching a successful intake response means the whole pipeline
        // ran. This is a useful signal for process health but also
        // acts as a checkpoint anchor for Antithesis replay: at this point
        // there is a nominally functional system.
        //
        // No-op outside the `antithesis` feature build.
        #[cfg(feature = "antithesis")]
        antithesis_sdk::assert_sometimes!(
            true,
            "ADP forwarded a payload to the intake",
            &serde_json::json!({ "domain": domain })
        );

        telemetry.track_successful_transaction(&metadata, domain);
    } else {
        telemetry.track_permanently_failed_transaction(&metadata, Some(status), domain);

        match response.into_body().collect().await {
            Ok(body) => {
                let body = body.to_bytes();
                let body_str = String::from_utf8_lossy(&body[..]);
                error!(endpoint_url, %status, "Received non-success response. Body: {}", body_str);
            }
            Err(e) => {
                error!(endpoint_url, %status, error = %e, "Failed to read response body of non-success response.");
            }
        }
    }
}

/// A queue of pending transactions waiting to be sent.
///
/// This queue is split into two parts: a high-priority queue and a low-priority queue. The high-priority queue is used
/// for brand-new transactions that are waiting to be processed for the first time. The low-priority queue contains
/// transactions that have either been requeued to be retried again at a later time, or that couldn't fit in the
/// high-priority queue due to it being full.
///
/// Ultimately, we use this construction to provide a fast path for new transactions, while limiting the overall number
/// of outstanding transactions that are waiting to be processed, with a bias towards preserving the most recent
/// transactions so that fresh data can be sent as soon as any temporary networking issues are resolved.
struct PendingTransactions<T> {
    high_priority: VecDeque<T>,
    low_priority: RetryQueue<T>,
    telemetry: TransactionQueueTelemetry,
    domain: MetaString,
    traffic_rate: TrafficRateWindow,
}

impl<T: Retryable> PendingTransactions<T> {
    /// Creates a new `PendingTransactions` instance.
    ///
    /// The high-priority queue will have a maximum capacity of `max_enqueued`, and the retry queue will be used as the
    /// low-priority queue.
    pub fn new(
        max_enqueued: usize, retry_queue: RetryQueue<T>, telemetry: TransactionQueueTelemetry, domain: MetaString,
        capacity_history_duration_secs: u64,
    ) -> Self {
        Self {
            high_priority: VecDeque::with_capacity(max_enqueued),
            low_priority: retry_queue,
            telemetry,
            domain,
            traffic_rate: TrafficRateWindow::new(
                capacity_history_duration_secs,
                RETRY_QUEUE_CAPACITY_BUCKET_DURATION_SECS,
            ),
        }
    }

    /// Returns `true` if there are no pending transactions.
    ///
    /// This includes both the high-priority and low-priority queues.
    pub fn is_empty(&self) -> bool {
        self.high_priority.is_empty() && self.low_priority.is_empty()
    }

    /// Pushes a high-priority transaction into the queue.
    ///
    /// If the high-priority queue is full, the transaction will be pushed into the low-priority queue.
    pub async fn push_high_priority(&mut self, transaction: T) -> Result<PushResult, GenericError> {
        self.record_incoming_transaction_size(transaction.size_bytes()).await;

        if self.high_priority.len() < self.high_priority.capacity() {
            self.high_priority.push_back(transaction);
            self.telemetry.high_prio_queue_insertions().increment(1);

            debug!(
                high_prio_queue_len = self.high_priority.len(),
                "Enqueued pending transaction to high-priority queue."
            );

            Ok(PushResult::default())
        } else {
            let push_result = self.low_priority.push(transaction).await?;
            self.telemetry.low_prio_queue_insertions().increment(1);
            self.record_retry_queue_size();

            debug!(
                low_prio_queue_len = self.low_priority.len(),
                "Enqueued pending transaction to low-priority queue."
            );

            Ok(push_result)
        }
    }

    /// Pushes a low-priority transaction into the queue.
    pub async fn push_low_priority(&mut self, transaction: T) -> Result<PushResult, GenericError> {
        let push_result = self.low_priority.push(transaction).await?;
        self.telemetry.low_prio_queue_insertions().increment(1);
        self.record_retry_queue_size();

        debug!(
            low_prio_queue_len = self.low_priority.len(),
            "Enqueued pending transaction to low-priority queue."
        );

        Ok(push_result)
    }

    /// Pops the next transaction from the queue.
    ///
    /// The high-priority queue is drained first before attempting to pop from the low-priority queue.
    pub async fn pop(&mut self) -> Option<T> {
        // We bias towards handling enqueued transactions first, since those are our "high priority" transactions, and we
        // want to keep them flowing as fast as possible.
        loop {
            if let Some(transaction) = self.high_priority.pop_front() {
                self.telemetry.high_prio_queue_removals().increment(1);

                debug!(
                    high_prio_queue_len = self.high_priority.len(),
                    "Dequeued pending transaction from high-priority queue."
                );
                return Some(transaction);
            }

            let pop_result = self.low_priority.pop().await;

            let entries_dropped = self.low_priority.take_persisted_entries_dropped();
            if entries_dropped > 0 {
                self.telemetry
                    .low_prio_queue_entries_dropped()
                    .increment(entries_dropped);
            }

            match pop_result {
                Ok(Some(transaction)) => {
                    self.telemetry.low_prio_queue_removals().increment(1);
                    self.record_retry_queue_size();

                    debug!(
                        low_prio_queue_len = self.low_priority.len(),
                        "Dequeued pending transaction from low-priority queue."
                    );
                    return Some(transaction);
                }
                Ok(None) => {
                    self.record_retry_queue_size();
                    return None;
                }
                Err(e) => {
                    error!(error = %e, "Failed to pop transaction from low-priority queue.");
                    continue;
                }
            }
        }
    }

    /// Flushes all transactions and finalizes the queue.
    ///
    /// This will flush any pending high-priority transactions to the low-priority queue, and then flush the
    /// low-priority queue, which will persist any transactions that are still in the queue to disk if the retry queue
    /// has disk persistence enabled.
    ///
    /// If disk persistence isn't enabled, all pending transactions will be dropped.
    ///
    /// # Errors
    ///
    /// If an error occurs flushing transactions to the low-priority queue, or occurs while flushing the low-priority
    /// queue itself, an error will be returned.
    pub async fn flush(mut self) -> Result<PushResult, GenericError> {
        let mut push_result = PushResult::default();

        // Push all high-priority transactions into the low-priority queue.
        while let Some(transaction) = self.high_priority.pop_front() {
            self.telemetry.high_prio_queue_removals().increment(1);

            let subpush_result = self.low_priority.push(transaction).await?;
            self.telemetry.low_prio_queue_insertions().increment(1);
            self.record_retry_queue_size();

            push_result.merge(subpush_result);
        }

        // Flush the low-priority queue.
        let flush_result = self.low_priority.flush().await?;
        push_result.merge(flush_result);

        Ok(push_result)
    }

    fn record_retry_queue_size(&self) {
        self.telemetry.record_retry_queue_size(self.low_priority.len());
    }

    async fn record_incoming_transaction_size(&mut self, bytes: u64) {
        self.record_incoming_transaction_size_at(bytes, get_unix_timestamp())
            .await;
    }

    async fn record_incoming_transaction_size_at(&mut self, bytes: u64, now_secs: u64) {
        let bytes_per_sec = self.traffic_rate.record(now_secs, bytes);
        self.telemetry.record_retry_queue_bytes_per_sec(bytes_per_sec);

        let disk_available_capacity_bytes = match self.low_priority.available_on_disk_capacity_bytes().await {
            Ok(available_capacity_bytes) => available_capacity_bytes,
            Err(e) => {
                warn!(error = %e, "Failed to calculate retry queue disk capacity for telemetry.");
                0
            }
        };

        self.telemetry.record_retry_queue_capacity_stats(
            &self.domain,
            bytes_per_sec,
            self.low_priority.available_in_memory_capacity_bytes(),
            disk_available_capacity_bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use bytes::Bytes;
    use http_body_util::Empty;
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::{
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
        version::TLS12,
        RootCertStore, ServerConfig,
    };
    use saluki_component_config::forwarder as leaf;
    use saluki_core::topology::ComponentId;
    use saluki_io::net::client::http::TlsMinimumVersion;
    use saluki_metrics::test::TestRecorder;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::mpsc,
        time::{timeout, Duration},
    };
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::common::datadog::{METRICS_SERIES_V1_PATH, METRICS_SERIES_V2_PATH, METRICS_SKETCHES_PATH};

    fn uri(path: &'static str) -> Uri {
        Uri::from_static(path)
    }

    /// Builds a routable additional-endpoint set directly from a leaf mirror.
    fn resolved_additional_endpoints(entries: &[(&str, &[&str])]) -> Vec<ResolvedEndpoint> {
        let map = entries
            .iter()
            .map(|(url, keys)| (url.to_string(), keys.iter().map(|k| k.to_string()).collect::<Vec<_>>()))
            .collect();
        let leaf_cfg = leaf::DatadogForwarderConfig {
            endpoint: leaf::EndpointConfiguration {
                additional_endpoints: leaf::AdditionalEndpoints(map),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = ForwarderConfiguration::from_native(&leaf_cfg);
        config
            .build_routable_endpoints(None)
            .expect("endpoints should resolve")
            .into_iter()
            .filter(|e| e.route() == EndpointRoute::Additional)
            .map(|e| e.into_parts().1)
            .collect()
    }

    #[test]
    fn identifies_metrics_request_paths() {
        assert!(is_metrics_request_uri(&uri(METRICS_SERIES_V1_PATH)));
        assert!(is_metrics_request_uri(&uri(METRICS_SERIES_V2_PATH)));
        assert!(is_metrics_request_uri(&uri(METRICS_SKETCHES_PATH)));
        assert!(!is_metrics_request_uri(&uri("/api/v2/logs")));
        assert!(!is_metrics_request_uri(&uri("/api/v0.2/traces")));
    }

    #[test]
    fn routes_metric_payload_to_opw_and_additional_when_opw_exists() {
        assert!(!should_route_to_endpoint(true, true, EndpointRoute::Primary));
        assert!(should_route_to_endpoint(true, true, EndpointRoute::MetricsPrimary));
        assert!(should_route_to_endpoint(true, true, EndpointRoute::Additional));
    }

    #[test]
    fn routes_metric_payload_to_primary_and_additional_without_opw() {
        assert!(should_route_to_endpoint(true, false, EndpointRoute::Primary));
        assert!(!should_route_to_endpoint(true, false, EndpointRoute::MetricsPrimary));
        assert!(should_route_to_endpoint(true, false, EndpointRoute::Additional));
    }

    #[test]
    fn routes_non_metric_payload_to_primary_and_additional_only() {
        assert!(should_route_to_endpoint(false, true, EndpointRoute::Primary));
        assert!(!should_route_to_endpoint(false, true, EndpointRoute::MetricsPrimary));
        assert!(should_route_to_endpoint(false, true, EndpointRoute::Additional));
    }

    #[test]
    fn retry_queue_id_uses_raw_additional_endpoint_url() {
        let context =
            ComponentContext::forwarder(ComponentId::try_from("test_forwarder").expect("component ID should be valid"));
        let endpoints = resolved_additional_endpoints(&[
            ("app.datadoghq.com", &["key-a"]),
            ("https://app.datadoghq.com", &["key-b"]),
        ]);

        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].endpoint(), endpoints[1].endpoint());

        let first_queue_id = generate_retry_queue_id(context.clone(), &endpoints[0]);
        let second_queue_id = generate_retry_queue_id(context, &endpoints[1]);

        assert_ne!(first_queue_id, second_queue_id);
    }

    #[test]
    fn retry_queue_id_uses_additional_endpoint_api_key_index() {
        let context =
            ComponentContext::forwarder(ComponentId::try_from("test_forwarder").expect("component ID should be valid"));
        let endpoints = resolved_additional_endpoints(&[("app.datadoghq.com", &["key-a", "key-b"])]);

        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].endpoint(), endpoints[1].endpoint());
        assert_eq!(
            endpoints[0].additional_endpoint_queue_key(),
            Some(("app.datadoghq.com", 0))
        );
        assert_eq!(
            endpoints[1].additional_endpoint_queue_key(),
            Some(("app.datadoghq.com", 1))
        );

        let first_queue_id = generate_retry_queue_id(context.clone(), &endpoints[0]);
        let second_queue_id = generate_retry_queue_id(context, &endpoints[1]);

        assert_ne!(first_queue_id, second_queue_id);
    }

    fn init_tls_crypto_provider() {
        // TODO: Figure out a better pattern for testing that doesn't involve initializing
        // the process-wide crypto provider.
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            let _ = saluki_tls::initialize_default_crypto_provider();
        });
    }

    fn http_client_for_tls_validation(validation: TlsCertificateValidation) -> HttpClient {
        http_client_for_tls_validation_with_min_tls_version(validation, TlsMinimumVersion::Tls12)
    }

    fn http_client_for_tls_validation_with_min_tls_version(
        validation: TlsCertificateValidation, min_tls_version: TlsMinimumVersion,
    ) -> HttpClient {
        let client_builder = HttpClient::builder()
            .with_min_tls_version(min_tls_version)
            .with_tls_config(|builder| builder.with_root_cert_store(RootCertStore::empty()));

        validation
            .apply_to(client_builder)
            .expect("TLS certificate validation policy should apply")
            .build()
            .expect("HTTP client should build")
    }

    async fn start_self_signed_https_server() -> (String, mpsc::Receiver<String>) {
        start_self_signed_https_server_with_versions(TestServerTlsVersions::Default).await
    }

    #[derive(Clone, Copy)]
    enum TestServerTlsVersions {
        Default,
        Tls12Only,
    }

    async fn start_self_signed_https_server_with_versions(
        versions: TestServerTlsVersions,
    ) -> (String, mpsc::Receiver<String>) {
        init_tls_crypto_provider();

        let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_string()]).unwrap();
        let cert_chain = vec![cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let server_config_builder = match versions {
            TestServerTlsVersions::Default => ServerConfig::builder(),
            TestServerTlsVersions::Tls12Only => ServerConfig::builder_with_protocol_versions(&[&TLS12]),
        };
        let server_config = server_config_builder
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (request_tx, request_rx) = mpsc::channel(4);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let request_tx = request_tx.clone();

                tokio::spawn(async move {
                    let mut stream = match acceptor.accept(stream).await {
                        Ok(stream) => stream,
                        Err(_) => return,
                    };

                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) => return,
                            Ok(n) => {
                                request.extend_from_slice(&buf[..n]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }

                    let request = String::from_utf8_lossy(&request).into_owned();
                    let _ = request_tx.send(request).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (format!("https://127.0.0.1:{port}/"), request_rx)
    }

    fn transaction_queue_telemetry() -> (TransactionQueueTelemetry, MetaString) {
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let telemetry = TransactionQueueTelemetry::from_builder(&builder, "https://example.com", shared.clone());
        let domain = MetaString::from_static("https://example.com");

        (telemetry, domain)
    }

    #[tokio::test]
    async fn retry_queue_bytes_per_sec_tracks_incoming_transaction_payloads() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry, domain, 900);

        let push_result = pending_txns.push_high_priority("payload".to_string()).await.unwrap();

        assert!(!push_result.had_drops());
        assert_eq!(recorder.gauge("network_http_retry_queue_bytes_per_sec"), Some(7.0));
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(0.0));
        assert_eq!(
            recorder.gauge((
                "network_http_retry_queue_bytes_per_sec",
                &[("domain", "https://example.com")],
            )),
            Some(7.0)
        );
        assert_eq!(
            recorder.gauge((
                "network_http_retry_queue_capacity_secs",
                &[("domain", "https://example.com")],
            )),
            Some(1024.0 / 7.0)
        );
        assert_eq!(
            recorder.gauge((
                "network_http_retry_queue_capacity_bytes",
                &[("domain", "https://example.com")],
            )),
            Some(1024.0)
        );
    }

    #[tokio::test]
    async fn retry_queue_bytes_per_sec_does_not_track_retry_drains_or_empty_queue() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry, domain, 900);

        pending_txns.record_incoming_transaction_size_at(20, 1).await;
        let push_result = pending_txns.push_low_priority("retry".to_string()).await.unwrap();

        assert!(!push_result.had_drops());
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(1.0));
        assert_eq!(recorder.gauge("network_http_retry_queue_bytes_per_sec"), Some(20.0));
        assert_eq!(pending_txns.pop().await.as_deref(), Some("retry"));
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(0.0));
        assert_eq!(recorder.gauge("network_http_retry_queue_bytes_per_sec"), Some(20.0));

        assert!(pending_txns.pop().await.is_none());
        assert_eq!(recorder.gauge("network_http_retry_queue_size"), Some(0.0));
        assert_eq!(recorder.gauge("network_http_retry_queue_bytes_per_sec"), Some(20.0));
    }

    #[tokio::test]
    async fn retry_queue_capacity_uses_remaining_in_memory_capacity() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry, domain, 900);

        let push_result = pending_txns
            .push_low_priority("retry".to_string())
            .await
            .expect("push should succeed");
        assert!(!push_result.had_drops());
        pending_txns.record_incoming_transaction_size_at(20, 1).await;

        assert_eq!(
            recorder.gauge((
                "network_http_retry_queue_capacity_bytes",
                &[("domain", "https://example.com")],
            )),
            Some(1019.0)
        );
        assert_eq!(
            recorder.gauge((
                "network_http_retry_queue_capacity_secs",
                &[("domain", "https://example.com")],
            )),
            Some(1019.0 / 20.0)
        );
    }

    #[test]
    fn tls_certificate_validation_enabled_by_default() {
        let config = ForwarderConfiguration::from_native(&leaf::DatadogForwarderConfig {
            endpoint: leaf::EndpointConfiguration {
                api_key: "test-api-key".to_string(),
                ..Default::default()
            },
            ..Default::default()
        });

        assert_eq!(
            TlsCertificateValidation::from_forwarder_config(&config),
            TlsCertificateValidation::Enabled
        );
    }

    #[test]
    fn tls_certificate_validation_disabled_when_skip_ssl_validation_enabled() {
        let config = ForwarderConfiguration::from_native(&leaf::DatadogForwarderConfig {
            endpoint: leaf::EndpointConfiguration {
                api_key: "test-api-key".to_string(),
                ..Default::default()
            },
            skip_ssl_validation: true,
            ..Default::default()
        });

        assert_eq!(
            TlsCertificateValidation::from_forwarder_config(&config),
            TlsCertificateValidation::Disabled
        );
    }

    #[cfg(feature = "fips")]
    #[test]
    fn skip_ssl_validation_rejected_in_fips_mode() {
        let error = TlsCertificateValidation::Disabled
            .ensure_supported()
            .expect_err("skip_ssl_validation should be rejected in FIPS mode");
        let message = error.to_string();

        assert!(message.contains("skip_ssl_validation"));
        assert!(message.contains("FIPS mode"));
        assert!(message.contains("disabling TLS certificate validation"));
    }

    #[tokio::test]
    async fn skip_ssl_validation_rejects_self_signed_https_endpoint_by_default() {
        let (uri, mut request_rx) = start_self_signed_https_server().await;
        let mut client = http_client_for_tls_validation(TlsCertificateValidation::Enabled);
        let request = http::Request::builder().uri(uri).body(Empty::<Bytes>::new()).unwrap();

        let result = client.send(request).await;

        assert!(result.is_err(), "self-signed certificate should be rejected");
        assert!(
            timeout(Duration::from_millis(200), request_rx.recv()).await.is_err(),
            "server should not receive an HTTP request when certificate validation fails"
        );
    }

    #[tokio::test]
    async fn skip_ssl_validation_allows_self_signed_https_endpoint_when_enabled() {
        let (uri, mut request_rx) = start_self_signed_https_server().await;
        let mut client = http_client_for_tls_validation(TlsCertificateValidation::Disabled);
        let request = http::Request::builder().uri(uri).body(Empty::<Bytes>::new()).unwrap();

        let response = client.send(request).await.expect("request should succeed");

        assert_eq!(response.status(), http::StatusCode::OK);
        let received_request = timeout(Duration::from_secs(2), request_rx.recv())
            .await
            .expect("timed out waiting for HTTPS request")
            .expect("HTTPS request channel closed");
        assert!(received_request.starts_with("GET / HTTP/1.1"));
    }

    #[tokio::test]
    async fn min_tls_version_tls13_rejects_tls12_only_endpoint() {
        let (uri, mut request_rx) =
            start_self_signed_https_server_with_versions(TestServerTlsVersions::Tls12Only).await;
        let request = http::Request::builder()
            .uri(uri.clone())
            .body(Empty::<Bytes>::new())
            .unwrap();
        let mut tls13_client = http_client_for_tls_validation_with_min_tls_version(
            TlsCertificateValidation::Disabled,
            TlsMinimumVersion::Tls13,
        );

        let result = tls13_client.send(request).await;

        assert!(
            result.is_err(),
            "TLS 1.3-only client should reject TLS 1.2-only endpoint"
        );
        assert!(
            timeout(Duration::from_millis(200), request_rx.recv()).await.is_err(),
            "server should not receive an HTTP request when TLS negotiation fails"
        );

        let request = http::Request::builder().uri(uri).body(Empty::<Bytes>::new()).unwrap();
        let mut tls12_client = http_client_for_tls_validation_with_min_tls_version(
            TlsCertificateValidation::Disabled,
            TlsMinimumVersion::Tls12,
        );

        let response = tls12_client.send(request).await.expect("request should succeed");

        assert_eq!(response.status(), http::StatusCode::OK);
        let received_request = timeout(Duration::from_secs(2), request_rx.recv())
            .await
            .expect("timed out waiting for HTTPS request")
            .expect("HTTPS request channel closed");
        assert!(received_request.starts_with("GET / HTTP/1.1"));
    }
}
