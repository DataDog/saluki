use std::{collections::VecDeque, error::Error as _, sync::Arc};

use futures::FutureExt as _;
use http::{Request, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use saluki_config::RefreshableConfiguration;
use saluki_core::task::spawn_traced;
use saluki_error::{generic_error, GenericError};
use saluki_io::{
    buf::ReadIoBuffer,
    net::{
        client::http::HttpClient,
        util::retry::{RetryQueue, Retryable},
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
    telemetry::ComponentTelemetry,
    transaction::{Transaction, TransactionBody},
};
use crate::destinations::datadog::common::middleware::{for_resolved_endpoint, with_version_info};

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
    pub async fn send_transaction(&self, transaction: Transaction<B>) -> Result<(), GenericError> {
        match self.transactions_tx.send(transaction).await {
            Ok(()) => Ok(()),
            Err(_) => Err(generic_error!("Failed to send request to I/O task: receiver dropped.")),
        }
    }

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
    config: ForwarderConfiguration,
    telemetry: ComponentTelemetry,
    client: HttpClient<TransactionBody<B>>,
    endpoints: Vec<ResolvedEndpoint>,
}

impl<B> TransactionForwarder<B>
where
    B: Body + ReadIoBuffer + Clone + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    pub fn from_config<F>(
        config: ForwarderConfiguration, maybe_refreshable_config: Option<RefreshableConfiguration>, endpoint_name: F,
        telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let endpoints = config.endpoint().build_resolved_endpoints(maybe_refreshable_config)?;
        let mut client_builder = HttpClient::builder()
            .with_request_timeout(config.request_timeout())
            .with_retry_policy(config.retry().to_default_http_retry_policy())
            .with_bytes_sent_counter(telemetry.bytes_sent().clone())
            .with_endpoint_telemetry(metrics_builder, Some(endpoint_name));
        if let Some(proxy) = config.proxy() {
            client_builder = client_builder.with_proxies(proxy.build()?);
        }
        let client = client_builder.build()?;

        Ok(Self {
            config,
            telemetry,
            client,
            endpoints,
        })
    }

    pub async fn spawn(self) -> Handle<B> {
        let (transactions_tx, transactions_rx) = mpsc::channel(8);
        let (io_shutdown_tx, io_shutdown_rx) = oneshot::channel();

        let Self {
            config,
            telemetry,
            client,
            endpoints,
        } = self;

        spawn_traced(run_io_loop(
            transactions_rx,
            io_shutdown_tx,
            config,
            client,
            telemetry,
            endpoints,
        ));

        Handle {
            transactions_tx,
            io_shutdown_rx,
        }
    }
}

async fn run_io_loop<S, B>(
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, io_shutdown_tx: oneshot::Sender<()>,
    config: ForwarderConfiguration, service: S, telemetry: ComponentTelemetry,
    resolved_endpoints: Vec<ResolvedEndpoint>,
) where
    S: Service<Request<TransactionBody<B>>, Response = hyper::Response<Incoming>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send + Into<BoxError>,
    B: Body + ReadIoBuffer + Clone + Send + 'static,
{
    // Spawn an endpoint I/O task for each endpoint we're configured to send to.
    //
    // This gives us granularity at just the right level, since the places we expect there to be problems (and thus
    // where concurrency can help make forward progress) are at the endpoint/API key level.
    let mut endpoint_txs = Vec::new();
    let task_barrier = Arc::new(Barrier::new(resolved_endpoints.len() + 1));

    for resolved_endpoint in resolved_endpoints {
        let endpoint_url = resolved_endpoint.endpoint().to_string();

        let (endpoint_tx, endpoint_rx) = mpsc::channel(8);
        let task_barrier = Arc::clone(&task_barrier);
        spawn_traced(run_endpoint_io_loop(
            endpoint_rx,
            task_barrier,
            config.clone(),
            service.clone(),
            telemetry.clone(),
            resolved_endpoint,
        ));

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
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, task_barrier: Arc<Barrier>, config: ForwarderConfiguration,
    service: S, telemetry: ComponentTelemetry, endpoint: ResolvedEndpoint,
) where
    S: Service<Request<TransactionBody<B>>, Response = hyper::Response<Incoming>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<BoxError> + Send + 'static,
    B: Body + ReadIoBuffer + Clone + Send + 'static,
{
    let endpoint_url = endpoint.endpoint().to_string();
    debug!(
        endpoint = endpoint_url,
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
        .service(service);

    let retry_queue = RetryQueue::new(endpoint_url.clone(), config.retry().retry_queue_max_size_bytes);
    let mut pending_transactions = PendingTransactions::new(config.endpoint_buffer_size(), retry_queue);

    let mut in_flight = JoinSet::new();
    let mut done = false;

    loop {
        select! {
            // Try and drain the next transaction from our channel, and push it into the pending transactions queue.
            //
            // Our goal is to read transactions off the channel as fast as possible and stick them into the pending
            // transactions queue so that we can use that to manage our high-priority vs low-priority transactions.
            maybe_transaction = transactions_rx.recv(), if !done => match maybe_transaction {
                Some(transaction) => if let Err(e) = pending_transactions.push(transaction).await {
                    error!(endpoint = endpoint_url, error = %e, "Failed to enqueue transaction.");
                },
                None => {
                    done = true;
                    debug!(endpoint = endpoint_url, "Requests channel for endpoint I/O task complete. Completing any in-flight requests...");
                }
            },

            // While we're not done and there are pending transactions, wait for the service to become ready and then
            // grab the next transaction to send, and start the request.
            svc = service.ready(), if !done && !pending_transactions.is_empty() => match svc {
                // We might not get an item from the pending transactions queue if there was only entries in the retry
                // queue and an error occurred while trying to pop the next one out, so we just ignore that case know
                // that we'll try again with the next transaction if pending transactions isn't empty.
                Ok(svc) => if let Some(transaction) = pending_transactions.pop().await {
                    let (event_count, request) = transaction.into_parts();
                    let fut = svc.call(request).map(move |result| (event_count, result));
                    in_flight.spawn(fut);
                },
                Err(e) => {
                    let e: tower::BoxError = e.into();
                    error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Unexpected error when querying service for readiness.");
                    break;
                }
            },

            // Drive any in-flight requests to completion.
            maybe_result = in_flight.join_next(), if !in_flight.is_empty() => {
                let task_result = maybe_result.expect("in_flight marked as not being empty");
                match task_result {
                    Ok((events, Ok(response))) => {
                        let status = response.status();
                        if status.is_success() {
                            debug!(endpoint = endpoint_url, %status, "Request sent.");

                            telemetry.events_sent().increment(events as u64);
                            telemetry.events_sent_batch_size().record(events as f64);
                        } else {
                            telemetry.http_failed_send().increment(1);
                            telemetry.events_dropped_http().increment(events as u64);

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
                    Ok((_, Err(e))) => {
                        let e: tower::BoxError = e.into();
                        error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
                    }
                    Err(e) => {
                        error!(endpoint = endpoint_url, error = %e, error_source = ?e.source(), "Request task failed to run to completion.");
                    }
                }
            },

            else => break,
        }
    }

    debug!(
        endpoint = endpoint_url,
        "Requests channel for endpoint I/O task complete. Synchronizing on shutdown."
    );

    // Signal to the main I/O task that we've finished.
    task_barrier.wait().await;
}

struct PendingTransactions<T> {
    high_priority: VecDeque<T>,
    low_priority: RetryQueue<T>,
}

impl<T: Retryable> PendingTransactions<T> {
    /// Creates a new `PendingTransactions` instance.
    ///
    /// The high-priority queue will have a maximum capacity of `max_enqueued`, and the retry queue will be used as the
    /// low-priority queue.
    pub fn new(max_enqueued: usize, retry_queue: RetryQueue<T>) -> Self {
        Self {
            high_priority: VecDeque::with_capacity(max_enqueued),
            low_priority: retry_queue,
        }
    }

    /// Returns `true` if there are no pending transactions.
    ///
    /// This includes both the high-priority and low-priority queues.
    pub fn is_empty(&self) -> bool {
        self.high_priority.is_empty() && self.low_priority.is_empty()
    }

    /// Pushes a transaction into the queue.
    ///
    /// If the high-priority queue is full, the transaction will be pushed into the low-priority queue.
    pub async fn push(&mut self, transaction: T) -> Result<(), GenericError> {
        if self.high_priority.len() < self.high_priority.capacity() {
            self.high_priority.push_back(transaction);
        } else {
            self.low_priority.push(transaction).await?;
        }

        Ok(())
    }

    /// Pops the next transaction from the queue.
    ///
    /// The high-priority queue is drained first before attempting to pop from the low-priority queue.
    pub async fn pop(&mut self) -> Option<T> {
        // We bias towards handling enqueued transactions first, since those are our "high priority" transactions, and we
        // want to keep them flowing as fast as possible.
        loop {
            if let Some(transaction) = self.high_priority.pop_front() {
                return Some(transaction);
            }

            match self.low_priority.pop().await {
                Ok(Some(transaction)) => return Some(transaction),
                Ok(None) => return None,
                Err(e) => {
                    error!(error = %e, "Failed to pop transaction from low-priority queue.");
                    continue;
                }
            }
        }
    }
}
