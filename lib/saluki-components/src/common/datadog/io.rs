//! Datadog forwarder I/O.
//!
//! # Missing
//!
//! - Account for dynamic API key updates when deriving endpoint queue IDs.
//! - Avoid initializing the process-wide crypto provider from tests.

use std::{collections::VecDeque, error::Error as _, path::PathBuf, sync::Arc, time::Duration};

use bytes::Buf;
use futures::FutureExt as _;
use http::{Request, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::{body::Incoming, Response};
use saluki_common::{hash::hash_single_stable, task::spawn_traced_named, time::get_unix_timestamp};
use saluki_config::GenericConfiguration;
use saluki_core::components::ComponentContext;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{
    client::http::{into_client_body, HttpClient, HttpClientBuilder},
    util::{
        middleware::{RetryCircuitBreakerError, RetryCircuitBreakerLayer},
        retry::{DiskUsageRetrieverImpl, PushResult, RetryQueue, Retryable},
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
    endpoints::ResolvedEndpoint,
    middleware::{for_resolved_endpoint, with_version_info},
    telemetry::{ComponentTelemetry, SharedTransactionQueueTelemetry, TransactionQueueTelemetry},
    transaction::{Metadata, Transaction, TransactionBody},
};

/// Size of buffer chunks for request builder buffers.
///
/// Used to influence the size of chunks in `ChunkedBytesBuffer`.
pub const RB_BUFFER_CHUNK_SIZE: usize = 32 * 1024; // 32 KB

const RETRY_QUEUE_CAPACITY_HISTORY_DURATION_SECS: u64 = 15 * 60;
const RETRY_QUEUE_CAPACITY_BUCKET_DURATION_SECS: u64 = 10;

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
    config: ForwarderConfiguration,            // static snapshot of forwarder settings
    live_config: Option<GenericConfiguration>, // runtime-mutable configuration
    telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder,
    client: HttpClient,
    endpoints: Vec<ResolvedEndpoint>,
    _marker: std::marker::PhantomData<B>,
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
        context: ComponentContext, config: ForwarderConfiguration, live_config: Option<GenericConfiguration>,
        endpoint_name: F, telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let endpoints = config.endpoint().build_resolved_endpoints(live_config.clone())?;
        let mut client_builder = HttpClient::builder()
            .with_request_timeout(config.request_timeout())
            .with_bytes_sent_counter(telemetry.bytes_sent().clone())
            .with_endpoint_telemetry(metrics_builder.clone(), Some(endpoint_name));
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

        let Self {
            context,
            config,
            live_config,
            telemetry,
            metrics_builder,
            client,
            endpoints,
            _marker,
        } = self;

        spawn_traced_named(
            "dd-txn-forwarder-io-loop",
            run_io_loop(
                transactions_rx,
                io_shutdown_tx,
                context,
                config,
                live_config,
                client,
                telemetry,
                metrics_builder,
                endpoints,
            ),
        );

        Handle {
            transactions_tx,
            io_shutdown_rx,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_io_loop<B>(
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, io_shutdown_tx: oneshot::Sender<()>,
    context: ComponentContext, config: ForwarderConfiguration, live_config: Option<GenericConfiguration>,
    service: HttpClient, telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    resolved_endpoints: Vec<ResolvedEndpoint>,
) where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // Spawn an endpoint I/O task for each endpoint we're configured to send to, which we'll forward transactions to.
    let mut endpoint_txs = Vec::new();
    let task_barrier = Arc::new(Barrier::new(resolved_endpoints.len() + 1));
    let shared_txnq_telemetry = SharedTransactionQueueTelemetry::from_builder(&metrics_builder);

    for resolved_endpoint in resolved_endpoints {
        let endpoint_url = resolved_endpoint.endpoint().to_string();

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
                live_config.clone(),
                service.clone(),
                telemetry.clone(),
                txnq_telemetry,
                resolved_endpoint,
            ),
        );

        endpoint_txs.push((endpoint_url, endpoint_tx));
    }

    // Listen for transactions to forward, and send a copy of each one to each endpoint I/O task.
    while let Some(transaction) = transactions_rx.recv().await {
        for (endpoint_url, endpoint_tx) in &endpoint_txs {
            if endpoint_tx.send(transaction.clone()).await.is_err() {
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

#[allow(clippy::too_many_arguments)]
async fn run_endpoint_io_loop<B>(
    mut txns_rx: mpsc::Receiver<Transaction<B>>, task_barrier: Arc<Barrier>, context: ComponentContext,
    config: ForwarderConfiguration, live_config: Option<GenericConfiguration>, service: HttpClient,
    telemetry: ComponentTelemetry, txnq_telemetry: TransactionQueueTelemetry, endpoint: ResolvedEndpoint,
) where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let queue_id = generate_retry_queue_id(context, &endpoint);
    let endpoint_url = endpoint.endpoint().to_string();
    debug!(
        endpoint_url,
        num_workers = config.endpoint_concurrency(),
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
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info())
        .concurrency_limit(config.endpoint_concurrency())
        .layer(RetryCircuitBreakerLayer::new(
            config.retry().to_default_http_retry_policy(live_config.clone()),
        ))
        .map_request(|req: Request<TransactionBody<B>>| req.map(into_client_body))
        .service(service);

    let mut retry_queue = RetryQueue::new(queue_id.clone(), config.retry().queue_max_size_bytes());

    // If the storage size is set, enable disk persistence for the retry queue.
    if config.retry().storage_max_size_bytes() > 0 {
        retry_queue = retry_queue
            .with_disk_persistence(
                PathBuf::from(config.retry().storage_path()),
                config.retry().storage_max_size_bytes(),
                config.retry().storage_max_disk_ratio(),
                Arc::new(DiskUsageRetrieverImpl::new(PathBuf::from(
                    config.retry().storage_path(),
                ))),
            )
            .await
            .unwrap_or_else(|e| {
                error!(endpoint_url, error = %e, "Failed to initialize disk persistence for retry queue. Transactions will not be persisted.");
                RetryQueue::new(queue_id, config.retry().queue_max_size_bytes())
            });
    }
    let mut pending_txns = PendingTransactions::new(config.endpoint_buffer_size(), retry_queue, txnq_telemetry);

    let mut in_flight = JoinSet::new();
    let mut done = false;

    loop {
        select! {
            // Try and drain the next transaction from our channel, and push it into the pending transactions queue.
            maybe_txn = txns_rx.recv(), if !done => match maybe_txn {
                Some(txn) => match pending_txns.push_high_priority(txn).await {
                    Ok(push_result) => {
                        telemetry.track_dropped_items(push_result.items_dropped);
                        telemetry.track_dropped_events(push_result.events_dropped);
                    }
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
                        Ok(http_response) => process_http_response(http_response, metadata, &telemetry, &endpoint_url).await,

                        // The service itself encountered an error while sending the request or receiving the response:
                        // connection reset by peer, I/O error, etc.
                        Err(RetryCircuitBreakerError::Service(e)) => {
                            telemetry.track_failed_transaction(&metadata, None);
                            error!(endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
                        },

                        // Our endpoint circuit breaker is open, which means this request either didn't go through at
                        // all or needs to be retried... so we'll re-enqueue it to the low-priority queue to be retried
                        // later.
                        Err(RetryCircuitBreakerError::Open(req)) => {
                            let reassembled_txn = Transaction::reassemble(metadata, req);
                            match pending_txns.push_low_priority(reassembled_txn).await {
                                Ok(push_result) => {
                                    telemetry.track_dropped_items(push_result.items_dropped);
                                    telemetry.track_dropped_events(push_result.events_dropped);
                                }
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
            telemetry.track_dropped_items(flush_result.items_dropped);
            telemetry.track_dropped_events(flush_result.events_dropped);
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
    // TODO: This logic does not take into account cases where the API key is updated dynamically. While a running
    // process would just keep using the existing retry queue, based on the queue ID we generate here... the next time
    // the process restarted, the retry queue ID would change, which could leave behind old transactions that wouldn't
    // end up being retried.
    //
    // The Core Agent is also susceptible to this, I believe... but we should double check that and see what they're
    // doing if they actually _do_ handle this case.

    // We set our queue ID/name to be unique for the component/endpoint/API key combination, which ensures that two
    // instances of the same destination cannot collide with each other if they're using the same endpoint/API key
    // combination.
    let hash = hash_single_stable((context.component_id(), endpoint.endpoint(), endpoint.cached_api_key()));

    let endpoint_host = endpoint
        .endpoint()
        .host_str()
        .expect("resolved endpoint must have a host");
    format!("{}/{}/{:x}", context.component_id(), endpoint_host, hash)
}

async fn process_http_response(
    response: Response<Incoming>, metadata: Metadata, telemetry: &ComponentTelemetry, endpoint_url: &str,
) {
    let status = response.status();
    if status.is_success() {
        debug!(endpoint_url, %status, "Request completed.");

        telemetry.track_successful_transaction(&metadata);
    } else {
        telemetry.track_failed_transaction(&metadata, Some(status));

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
/// transactions that have either been requeued to be retried again at a later time, or that could not fit in the
/// high-priority queue due to it being full.
///
/// Ultimately, we use this construction to provide a fast path for new transactions, while limiting the overall number
/// of outstanding transactions that are waiting to be processed, with a bias towards preserving the most recent
/// transactions so that fresh data can be sent as soon as any temporary networking issues are resolved.
struct PendingTransactions<T> {
    high_priority: VecDeque<T>,
    low_priority: RetryQueue<T>,
    telemetry: TransactionQueueTelemetry,
    incoming_bytes_per_sec: IncomingBytesPerSec,
}

// Mirrors the Datadog Agent retry queue duration traffic-rate input:
// https://github.com/DataDog/datadog-agent/blob/main/comp/forwarder/defaultforwarder/internal/retry/queue_duration_capacity.go
// https://github.com/DataDog/datadog-agent/blob/main/comp/forwarder/defaultforwarder/internal/retry/time_interval_accumulator.go
struct IncomingBytesPerSec {
    bucket_sum: Vec<u64>,
    current_index: usize,
    current_index_time_secs: Option<u64>,
    start_index_time_secs: Option<u64>,
    sum: u64,
    bucket_duration_secs: u64,
}

impl IncomingBytesPerSec {
    fn new(history_duration_secs: u64, bucket_duration_secs: u64) -> Self {
        assert!(bucket_duration_secs > 0, "bucket duration must be at least one second");
        assert!(
            history_duration_secs >= bucket_duration_secs,
            "history duration must be greater than or equal to bucket duration"
        );

        Self {
            bucket_sum: vec![0; (history_duration_secs / bucket_duration_secs) as usize],
            current_index: 0,
            current_index_time_secs: None,
            start_index_time_secs: None,
            sum: 0,
            bucket_duration_secs,
        }
    }

    fn record(&mut self, now_secs: u64, bytes: u64) -> f64 {
        if self.start_index_time_secs.is_none() {
            self.start_index_time_secs = Some(now_secs);
            self.current_index_time_secs = Some(now_secs);
        }

        while now_secs
            >= self.current_index_time_secs.expect("current index time should be set") + self.bucket_duration_secs
        {
            self.current_index = (self.current_index + 1) % self.bucket_sum.len();
            self.sum -= self.bucket_sum[self.current_index];
            self.bucket_sum[self.current_index] = 0;

            let current_index_time_secs =
                self.current_index_time_secs.expect("current index time should be set") + self.bucket_duration_secs;
            self.current_index_time_secs = Some(current_index_time_secs);

            let start_index_time_secs = self.start_index_time_secs.expect("start index time should be set");
            if current_index_time_secs
                >= start_index_time_secs + self.bucket_sum.len() as u64 * self.bucket_duration_secs
            {
                self.start_index_time_secs = Some(start_index_time_secs + self.bucket_duration_secs);
            }
        }

        self.bucket_sum[self.current_index] += bytes;
        self.sum += bytes;
        self.bytes_per_sec(now_secs)
    }

    fn bytes_per_sec(&self, now_secs: u64) -> f64 {
        let Some(start_index_time_secs) = self.start_index_time_secs else {
            return 0.0;
        };

        let duration_secs = now_secs.saturating_sub(start_index_time_secs) + 1;
        self.sum as f64 / duration_secs as f64
    }
}

impl<T: Retryable> PendingTransactions<T> {
    /// Creates a new `PendingTransactions` instance.
    ///
    /// The high-priority queue will have a maximum capacity of `max_enqueued`, and the retry queue will be used as the
    /// low-priority queue.
    pub fn new(max_enqueued: usize, retry_queue: RetryQueue<T>, telemetry: TransactionQueueTelemetry) -> Self {
        Self {
            high_priority: VecDeque::with_capacity(max_enqueued),
            low_priority: retry_queue,
            telemetry,
            incoming_bytes_per_sec: IncomingBytesPerSec::new(
                RETRY_QUEUE_CAPACITY_HISTORY_DURATION_SECS,
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
        self.record_incoming_transaction_size(transaction.size_bytes());

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
    /// If disk persistence is not enabled, all pending transactions will be dropped.
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

    fn record_incoming_transaction_size(&mut self, bytes: u64) {
        self.record_incoming_transaction_size_at(bytes, get_unix_timestamp());
    }

    fn record_incoming_transaction_size_at(&mut self, bytes: u64, now_secs: u64) {
        let bytes_per_sec = self.incoming_bytes_per_sec.record(now_secs, bytes);
        self.telemetry.record_retry_queue_bytes_per_sec(bytes_per_sec);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    };

    use bytes::Bytes;
    use http::StatusCode;
    use http_body_util::Empty;
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::{
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
        RootCertStore, ServerConfig,
    };
    use saluki_common::buf::FrozenChunkedBytesBuffer;
    use saluki_config::ConfigurationLoader;
    use saluki_core::{observability::ComponentMetricsExt as _, topology::ComponentId};
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
        time::{timeout, Duration},
    };
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::common::datadog::transaction::{Metadata as TxnMetadata, Transaction};

    fn forwarder_config_from_value(value: serde_json::Value) -> ForwarderConfiguration {
        serde_json::from_value(value).expect("ForwarderConfiguration should deserialize")
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
        let client_builder =
            HttpClient::builder().with_tls_config(|builder| builder.with_root_cert_store(RootCertStore::empty()));

        validation
            .apply_to(client_builder)
            .expect("TLS certificate validation policy should apply")
            .build()
            .expect("HTTP client should build")
    }

    async fn start_self_signed_https_server() -> (String, mpsc::Receiver<String>) {
        init_tls_crypto_provider();

        let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_string()]).unwrap();
        let cert_chain = vec![cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let server_config = ServerConfig::builder()
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

    fn transaction_queue_telemetry() -> (SharedTransactionQueueTelemetry, TransactionQueueTelemetry) {
        let builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&builder);
        let telemetry = TransactionQueueTelemetry::from_builder(&builder, "https://example.com", shared.clone());

        (shared, telemetry)
    }

    #[test]
    fn incoming_bytes_per_sec_matches_agent_windowed_rate() {
        let mut incoming_bytes_per_sec = IncomingBytesPerSec::new(10, 1);

        assert_eq!(incoming_bytes_per_sec.record(1, 5), 5.0);
        assert_eq!(incoming_bytes_per_sec.record(2, 15), 10.0);
    }

    #[tokio::test]
    async fn retry_queue_bytes_per_sec_tracks_incoming_transaction_payloads() {
        let (shared, telemetry) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry);

        let push_result = pending_txns.push_high_priority("payload".to_string()).await.unwrap();

        assert!(!push_result.had_drops());
        assert_eq!(shared.aggregate_snapshot(), (0, 7.0));
    }

    #[tokio::test]
    async fn retry_queue_bytes_per_sec_does_not_track_retry_drains_or_empty_queue() {
        let (shared, telemetry) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry);

        pending_txns.record_incoming_transaction_size_at(20, 1);
        let push_result = pending_txns.push_low_priority("retry".to_string()).await.unwrap();

        assert!(!push_result.had_drops());
        assert_eq!(shared.aggregate_snapshot(), (1, 20.0));
        assert_eq!(pending_txns.pop().await.as_deref(), Some("retry"));
        assert_eq!(shared.aggregate_snapshot(), (0, 20.0));

        assert!(pending_txns.pop().await.is_none());
        assert_eq!(shared.aggregate_snapshot(), (0, 20.0));
    }

    #[test]
    fn tls_certificate_validation_enabled_by_default() {
        let config = forwarder_config_from_value(serde_json::json!({ "api_key": "test-api-key" }));

        assert_eq!(
            TlsCertificateValidation::from_forwarder_config(&config),
            TlsCertificateValidation::Enabled
        );
    }

    #[test]
    fn tls_certificate_validation_disabled_when_skip_ssl_validation_enabled() {
        let config = forwarder_config_from_value(serde_json::json!({
            "api_key": "test-api-key",
            "skip_ssl_validation": true,
        }));

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

    /// Mode controlling what status codes the recording HTTP server returns to incoming requests.
    enum ServerMode {
        /// Always respond with the given status code.
        AlwaysStatus(StatusCode),
        /// Respond with each status code from the sequence in turn; once exhausted, respond with the final code forever.
        StatusSequence(Vec<StatusCode>),
    }

    /// Starts a minimal HTTP server on `127.0.0.1:0` that records each request and replies based on `mode`.
    ///
    /// Returns the server's `http://127.0.0.1:PORT/` URL and a counter that increments once per accepted/processed
    /// connection (one connection per request, since the server replies with `Connection: close`).
    async fn start_recording_http_server(mode: ServerMode) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let counter = Arc::new(AtomicUsize::new(0));

        let mode = Arc::new(mode);
        let counter_for_task = Arc::clone(&counter);
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mode = Arc::clone(&mode);
                let counter = Arc::clone(&counter_for_task);

                tokio::spawn(async move {
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

                    // Drain any body bytes that arrived alongside the headers, plus whatever remains based on a
                    // simple Content-Length parse. We don't actually need to buffer it; we just need to consume it
                    // so the client doesn't get a connection reset before reading our response.
                    let request_str = String::from_utf8_lossy(&request).into_owned();
                    let content_length = parse_content_length(&request_str).unwrap_or(0);
                    let header_end = request
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map_or(request.len(), |idx| idx + 4);
                    let mut already_read_body = request.len().saturating_sub(header_end);
                    while already_read_body < content_length {
                        match stream.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => already_read_body += n,
                            Err(_) => return,
                        }
                    }

                    let nth = counter.fetch_add(1, Ordering::SeqCst);
                    let status = match mode.as_ref() {
                        ServerMode::AlwaysStatus(s) => *s,
                        ServerMode::StatusSequence(seq) => {
                            let idx = nth.min(seq.len() - 1);
                            seq[idx]
                        }
                    };

                    let response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        status.as_u16(),
                        status.canonical_reason().unwrap_or(""),
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (format!("http://127.0.0.1:{port}/"), counter)
    }

    fn parse_content_length(request: &str) -> Option<usize> {
        for line in request.lines() {
            if let Some(value) = line
                .strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
            {
                return value.trim().parse().ok();
            }
        }
        None
    }

    fn build_test_forwarder(
        forwarder_url: &str, live_config: Option<GenericConfiguration>,
    ) -> TransactionForwarder<FrozenChunkedBytesBuffer> {
        // The HTTP client builder requires the process-wide TLS crypto provider to be initialized, even when the
        // forwarder is pointed at a plain HTTP endpoint.
        init_tls_crypto_provider();

        // Tight timeouts and small backoffs keep the test under a couple seconds even with retries.
        let value = serde_json::json!({
            "api_key": "test-api-key",
            "dd_url": forwarder_url,
            "forwarder_timeout": 1u64,
            "forwarder_num_workers": 1usize,
            "forwarder_high_prio_buffer_size": 4usize,
            "forwarder_backoff_base": 0.001,
            "forwarder_backoff_max": 0.01,
            "forwarder_backoff_factor": 2.0,
            "forwarder_recovery_interval": 1u32,
            "forwarder_recovery_reset": false,
            // The HTTP client builder otherwise requires the process-wide default root certificate store to be
            // populated. We are talking to a plain HTTP endpoint anyway, so disable validation to skip that path.
            "skip_ssl_validation": true,
        });
        let forwarder_config = forwarder_config_from_value(value);
        let context =
            ComponentContext::forwarder(ComponentId::try_from("test_forwarder").expect("component ID should be valid"));
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        TransactionForwarder::<FrozenChunkedBytesBuffer>::from_config(
            context,
            forwarder_config,
            live_config,
            |_uri: &Uri| None,
            telemetry,
            metrics_builder,
        )
        .expect("forwarder should build")
    }

    fn build_test_transaction() -> Transaction<FrozenChunkedBytesBuffer> {
        let body = FrozenChunkedBytesBuffer::from(Bytes::from_static(b"test-payload"));
        let request = http::Request::builder()
            .method("POST")
            // The endpoint middleware rewrites the authority to point at our `dd_url`, but preserves the path. Use a
            // path that is not the special-cased `/api/v2/logs` or `/api/v0.2/{traces,stats}` routes, so the request
            // is dispatched to the configured `dd_url` host directly.
            .uri("http://placeholder/api/v2/series")
            .body(body)
            .expect("request should build");
        Transaction::from_original(TxnMetadata::from_event_count(1), request)
    }

    async fn config_with(values: serde_json::Value) -> GenericConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        config
    }

    async fn wait_for_count_at_least(counter: &Arc<AtomicUsize>, target: usize, deadline: Duration) -> usize {
        let start = std::time::Instant::now();
        loop {
            let current = counter.load(Ordering::SeqCst);
            if current >= target {
                return current;
            }
            if start.elapsed() > deadline {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn forwarder_does_not_retry_403_without_secrets() {
        let (server_url, counter) = start_recording_http_server(ServerMode::AlwaysStatus(StatusCode::FORBIDDEN)).await;
        let live_config = config_with(json!({})).await;
        let forwarder = build_test_forwarder(&server_url, Some(live_config));

        let handle = forwarder.spawn().await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        // Wait for the first 403 to be observed, then give the forwarder a generous window to perform any
        // (incorrect) additional retries.
        let first = wait_for_count_at_least(&counter, 1, Duration::from_secs(2)).await;
        assert!(first >= 1, "server should have received at least one request");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Tear down before asserting so any pending in-flight call is drained.
        handle.shutdown().await;

        let final_count = counter.load(Ordering::SeqCst);
        assert_eq!(
            final_count, 1,
            "without secrets configured, 403 must not be retried (saw {} requests)",
            final_count
        );
    }

    #[tokio::test]
    async fn forwarder_retries_403_when_secrets_in_use() {
        // The server returns 403 to the first request and 200 to every subsequent request; the forwarder must drive
        // at least one retry to observe the second request.
        let (server_url, counter) =
            start_recording_http_server(ServerMode::StatusSequence(vec![StatusCode::FORBIDDEN, StatusCode::OK])).await;
        let live_config = config_with(json!({ "secret_backend_command": "/bin/true" })).await;
        let forwarder = build_test_forwarder(&server_url, Some(live_config));

        let handle = forwarder.spawn().await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        let observed = wait_for_count_at_least(&counter, 2, Duration::from_secs(3)).await;
        handle.shutdown().await;

        assert!(
            observed >= 2,
            "with secrets configured, 403 must be retried at least once (saw {} requests)",
            observed
        );
    }
}
