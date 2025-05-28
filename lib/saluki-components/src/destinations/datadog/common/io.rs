use std::{collections::VecDeque, error::Error as _, path::PathBuf, sync::Arc, time::Duration};

use futures::FutureExt as _;
use http::{Request, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::{body::Incoming, Response};
use saluki_common::{hash::hash_single_stable, task::spawn_traced_named};
use saluki_config::RefreshableConfiguration;
use saluki_core::{components::ComponentContext, pooling::ElasticObjectPool};
use saluki_error::{generic_error, GenericError};
use saluki_io::{
    buf::{BytesBuffer, FixedSizeVec, ReadIoBuffer},
    net::{
        client::http::HttpClient,
        util::{
            middleware::{RetryCircuitBreakerError, RetryCircuitBreakerLayer},
            retry::{DiskUsageRetrieverImpl, PushResult, RetryQueue, Retryable},
        },
    },
};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot, Barrier},
    task::JoinSet,
};
use tower::{BoxError, Service, ServiceBuilder, ServiceExt as _};
use tracing::{debug, error};

use super::{
    config::ForwarderConfiguration,
    endpoints::ResolvedEndpoint,
    middleware::{for_resolved_endpoint, with_version_info},
    telemetry::{ComponentTelemetry, TransactionQueueTelemetry},
    transaction::{Metadata, Transaction, TransactionBody},
};

const RB_BUFFER_POOL_BUF_SIZE: usize = 32 * 1024; // 32 KB

/// A handle to the transaction forwarder.
pub struct Handle<B>
where
    B: ReadIoBuffer + Clone,
{
    transactions_tx: mpsc::Sender<Transaction<B>>,
    io_shutdown_rx: oneshot::Receiver<()>,
}

impl<B> Handle<B>
where
    B: ReadIoBuffer + Clone,
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

pub struct TransactionForwarder<B> {
    context: ComponentContext,
    config: ForwarderConfiguration,
    telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder,
    client: HttpClient<TransactionBody<B>>,
    endpoints: Vec<ResolvedEndpoint>,
}

impl<B> TransactionForwarder<B>
where
    B: Body + ReadIoBuffer + Clone + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    /// Creates a new `TransactionForwarder` instance from the given configuration.
    pub fn from_config<F>(
        context: ComponentContext, config: ForwarderConfiguration,
        maybe_refreshable_config: Option<RefreshableConfiguration>, endpoint_name: F, telemetry: ComponentTelemetry,
        metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let endpoints = config.endpoint().build_resolved_endpoints(maybe_refreshable_config)?;
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

        let client = client_builder.build()?;

        Ok(Self {
            context,
            config,
            telemetry,
            metrics_builder,
            client,
            endpoints,
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
            telemetry,
            metrics_builder,
            client,
            endpoints,
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
            ),
        );

        Handle {
            transactions_tx,
            io_shutdown_rx,
        }
    }
}

async fn run_io_loop<S, B>(
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, io_shutdown_tx: oneshot::Sender<()>,
    context: ComponentContext, config: ForwarderConfiguration, service: S, telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder, resolved_endpoints: Vec<ResolvedEndpoint>,
) where
    S: Service<Request<TransactionBody<B>>, Response = Response<Incoming>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Into<BoxError> + Send + Sync,
    B: Body + ReadIoBuffer + Clone + Send + 'static,
{
    // Spawn an endpoint I/O task for each endpoint we're configured to send to, which we'll forward transactions to.
    let mut endpoint_txs = Vec::new();
    let task_barrier = Arc::new(Barrier::new(resolved_endpoints.len() + 1));

    for resolved_endpoint in resolved_endpoints {
        let endpoint_url = resolved_endpoint.endpoint().to_string();

        let txnq_telemetry = TransactionQueueTelemetry::from_builder(&metrics_builder, &endpoint_url);

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

async fn run_endpoint_io_loop<S, B>(
    mut txns_rx: mpsc::Receiver<Transaction<B>>, task_barrier: Arc<Barrier>, context: ComponentContext,
    config: ForwarderConfiguration, service: S, telemetry: ComponentTelemetry,
    txnq_telemetry: TransactionQueueTelemetry, endpoint: ResolvedEndpoint,
) where
    S: Service<Request<TransactionBody<B>>, Response = Response<Incoming>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<BoxError> + Send + Sync + 'static,
    B: Body + ReadIoBuffer + Clone + Send + 'static,
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
    let mut service = ServiceBuilder::new()
        // Set the request's URI to the endpoint's URI, and add the API key as a header.
        .map_request(for_resolved_endpoint(endpoint))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info())
        .concurrency_limit(config.endpoint_concurrency())
        .layer(RetryCircuitBreakerLayer::new(
            config.retry().to_default_http_retry_policy(),
        ))
        .service(service.map_err(|e| e.into()));

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
                    Ok(push_result) => telemetry.track_dropped_events(push_result.events_dropped),
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
                            telemetry.track_failed_transaction(&metadata);
                            error!(endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
                        },

                        // Our endpoint circuit breaker is open, which means this request either didn't go through at
                        // all or needs to be retried... so we'll re-enqueue it to the low-priority queue to be retried
                        // later.
                        Err(RetryCircuitBreakerError::Open(req)) => {
                            let reassembled_txn = Transaction::reassemble(metadata, req);
                            match pending_txns.push_low_priority(reassembled_txn).await {
                                Ok(push_result) => telemetry.track_dropped_events(push_result.events_dropped),
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
        telemetry.track_failed_transaction(&metadata);

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

            match self.low_priority.pop().await {
                Ok(Some(transaction)) => {
                    self.telemetry.low_prio_queue_removals().increment(1);

                    debug!(
                        low_prio_queue_len = self.low_priority.len(),
                        "Enqueued pending transaction from low-priority queue."
                    );
                    return Some(transaction);
                }
                Ok(None) => return None,
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

            push_result.merge(subpush_result);
        }

        // Flush the low-priority queue.
        let flush_result = self.low_priority.flush().await?;
        push_result.merge(flush_result);

        Ok(push_result)
    }
}

/// Returns the minimum and maximum size of the request builder buffer pool for the given forwarder configuration and
/// maximum request size.
///
/// Buffer pool sizing can have an impact on performance/correctness, as we must ensure that enough buffers are
/// available to build requests when the forwarder is under backpressure/retrying. This function consolidates that logic
/// in a single path.
///
/// `max_request_size` must specify the largest possible request body size that can be built by the destination using
/// the resulting buffer pool.
pub const fn get_buffer_pool_min_max_size(config: &ForwarderConfiguration, max_request_size: usize) -> (usize, usize) {
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

/// Returns the minimum and maximum size of the request builder buffer pool for the given forwarder configuration and
/// maximum request size, in bytes.
///
/// See [`get_buffer_pool_min_max_size`] for more details.
pub const fn get_buffer_pool_min_max_size_bytes(
    config: &ForwarderConfiguration, max_request_size: usize,
) -> (usize, usize) {
    let (minimum_size, maximum_size) = get_buffer_pool_min_max_size(config, max_request_size);
    (
        minimum_size * RB_BUFFER_POOL_BUF_SIZE,
        maximum_size * RB_BUFFER_POOL_BUF_SIZE,
    )
}

/// Creates a new request builder buffer pool for the given forwarder configuration and maximum request size.
///
/// See [`get_buffer_pool_min_max_size`] for more details on how the buffer pool is sized.
pub async fn create_request_builder_buffer_pool(
    destination_type: &'static str, config: &ForwarderConfiguration, max_request_size: usize,
) -> ElasticObjectPool<BytesBuffer> {
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

    let (minimum_size, maximum_size) = get_buffer_pool_min_max_size(config, max_request_size);
    let (pool, shrinker) = ElasticObjectPool::with_builder(
        format!("dd_{}_request_buffer", destination_type),
        minimum_size,
        maximum_size,
        || FixedSizeVec::with_capacity(RB_BUFFER_POOL_BUF_SIZE),
    );

    spawn_traced_named(format!("dd-{}-buffer-pool-shrinker", destination_type), shrinker);

    debug!(
        destination_type,
        minimum_size, maximum_size, "Created request builder buffer pool."
    );

    pool
}
