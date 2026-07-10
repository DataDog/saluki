//! Datadog forwarder I/O.
//!
//! # Missing
//!
//! - Avoid breaking apart `TransactionForwarder` only to work around `#[allow(clippy::too_many_arguments)]`.
//! - Avoid initializing the process-wide crypto provider from tests.

use std::{
    collections::VecDeque,
    error::Error as _,
    marker::PhantomData,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Buf;
use futures::FutureExt as _;
use http::{Request, StatusCode, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::{body::Incoming, Response};
use saluki_common::{
    collections::FastHashMap, hash::hash_single_stable, task::spawn_traced_named, time::get_unix_timestamp,
};
use saluki_config::GenericConfiguration;
use saluki_core::components::ComponentContext;
use saluki_core::diagnostic::{DiagnosticDetails, DiagnosticEvent, DiagnosticsEmitter};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{
    client::http::{into_client_body, HttpClient, HttpClientBuilder},
    util::{
        middleware::{HttpInspectionLayer, RetryCircuitBreakerError, RetryCircuitBreakerLayer},
        retry::{DiskUsageRetrieverImpl, PersistedQueueArgs, PushResult, RetryQueue, Retryable},
    },
};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, oneshot, Barrier},
    task::{JoinError, JoinSet},
};
use tower::{BoxError, Service, ServiceBuilder, ServiceExt as _};
use tracing::{debug, error, warn};

use super::{
    config::ForwarderConfiguration,
    endpoints::{EndpointRoute, EndpointV3Settings, ResolvedEndpoint, RoutableEndpoint, V3EndpointConfig},
    middleware::{for_resolved_endpoint, with_allow_arbitrary_tags, with_version_info},
    retry_capacity::{TrafficRateWindow, RETRY_QUEUE_CAPACITY_BUCKET_DURATION_SECS},
    telemetry::{
        ComponentTelemetry, SharedTransactionQueueTelemetry, TransactionInputTelemetry, TransactionQueueTelemetry,
        TransactionRetryCounters, TransactionRetryTelemetry,
    },
    transaction::{Metadata, Transaction, TransactionBody},
    validation::ApiKeyValidator,
    METRIC_INTAKE_PATHS,
};

type EndpointNameFn = dyn Fn(&Uri) -> Option<MetaString> + Send + Sync;

struct InFlightTransaction<R> {
    metadata: Metadata,
    retry_counters: Option<TransactionRetryCounters>,
    result: R,
}

type InFlightTransactionResult<B> =
    InFlightTransaction<Result<Response<Incoming>, RetryCircuitBreakerError<BoxError, Request<TransactionBody<B>>>>>;
type InFlightTaskResult<B> = Result<InFlightTransactionResult<B>, JoinError>;

fn prepare_transaction_for_dispatch<B>(
    pending_txn: PendingTransaction<Transaction<B>>, retry_telemetry: &mut TransactionRetryTelemetry,
    endpoint_name: &EndpointNameFn,
) -> (Metadata, Request<TransactionBody<B>>, Option<TransactionRetryCounters>)
where
    B: Buf + Clone,
{
    let (txn, retry_counters) = match pending_txn {
        PendingTransaction::HighPriority(txn) => (txn, None),
        PendingTransaction::LowPriority(txn) => {
            // Low-priority provenance drives Core Agent-compatible retry accounting, including input overflow.
            let resolved = endpoint_name(txn.request_uri());
            let logical = resolved.as_deref().unwrap_or_else(|| txn.request_uri().path());
            let counters = retry_telemetry.counters_for(logical);
            (txn, Some(counters))
        }
    };
    let (metadata, request) = txn.into_parts();

    (metadata, request, retry_counters)
}

async fn requeue_transaction<B>(
    metadata: Metadata, request: Request<TransactionBody<B>>, pending_txns: &mut PendingTransactions<Transaction<B>>,
    requeued_counters: Option<&TransactionRetryCounters>,
) -> Result<PushResult, GenericError>
where
    B: Buf + Clone,
{
    let push_result = pending_txns
        .push_low_priority(Transaction::reassemble(metadata, request))
        .await?;
    if let Some(counters) = requeued_counters {
        counters.increment_requeued();
    }

    Ok(push_result)
}

async fn handle_in_flight_transaction_result<B>(
    task_result: InFlightTaskResult<B>, pending_txns: &mut PendingTransactions<Transaction<B>>,
    telemetry: &ComponentTelemetry, endpoint_url: &str, endpoint_domain: &str,
) where
    B: Buf + Clone,
{
    let InFlightTransaction {
        metadata,
        retry_counters,
        result,
    } = match task_result {
        Ok(in_flight_txn) => in_flight_txn,
        Err(e) => {
            error!(endpoint_url, error = %e, error_source = ?e.source(), "Request task failed to run to completion.");
            return;
        }
    };

    // Any low-priority transaction that reached the inner service counts as a retry on success, final Service error,
    // or Retry; Open means no dispatch.
    let reached_inner_service = !matches!(&result, Err(RetryCircuitBreakerError::Open(_)));
    if reached_inner_service {
        if let Some(counters) = retry_counters.as_ref() {
            counters.increment_retries();
        }
    }

    match result {
        // We got a response -- maybe successful, maybe not -- so just process that. A rejected API key (HTTP 403) is
        // surfaced by the inspection layer in the service stack rather than here, since a retriable 403 becomes a
        // `Retry` result and never reaches this arm.
        Ok(http_response) => {
            process_http_response(http_response, metadata, telemetry, endpoint_url, endpoint_domain).await
        }

        // The service itself encountered an error while sending the request or receiving the response:
        // connection reset by peer, I/O error, etc.
        Err(RetryCircuitBreakerError::Service(e)) => {
            telemetry.track_permanently_failed_transaction(&metadata, None, endpoint_domain);
            error!(endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
        }

        // The request completed and needs to be retried, so re-enqueue it to the low-priority queue.
        Err(RetryCircuitBreakerError::Retry(req)) => {
            match requeue_transaction(metadata, req, pending_txns, None).await {
                Ok(push_result) => track_queue_drops(telemetry, endpoint_domain, push_result),
                Err(e) => {
                    error!(endpoint_url, error = %e, "Failed to re-enqueue failed transaction. Events may be permanently lost.")
                }
            }
        }

        // The request was rejected before inward dispatch, so re-enqueue it without counting another retry.
        Err(RetryCircuitBreakerError::Open(req)) => {
            match requeue_transaction(metadata, req, pending_txns, retry_counters.as_ref()).await {
                Ok(push_result) => track_queue_drops(telemetry, endpoint_domain, push_result),
                Err(e) => {
                    error!(endpoint_url, error = %e, "Failed to re-enqueue failed transaction. Events may be permanently lost.")
                }
            }
        }
    }
}

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

/// Minimum interval between successive `InvalidApiKey` diagnostic events emitted from the request I/O path for a single
/// endpoint.
///
/// We want to ensure there's a steady stream of these events while the invalid API key condition is still present, but
/// we want to avoid sending a deluge of events if there happens to be some bug that causes us to rapidly fire off
/// requests at a rate of more than one per second.
const INVALID_API_KEY_EMIT_INTERVAL: Duration = Duration::from_secs(1);

/// Transaction forwarder for Datadog endpoints.
pub struct TransactionForwarder<B> {
    context: ComponentContext,
    config: ForwarderConfiguration,
    live_config: Option<GenericConfiguration>,
    telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder,
    client: HttpClient,
    endpoint_name: Arc<EndpointNameFn>,
    endpoints: Vec<RoutableEndpoint>,
    endpoint_request_mapper_factory: EndpointRequestMapperFactory<B>,
    emitter: DiagnosticsEmitter,
    _marker: PhantomData<B>,
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

    fn apply_to(self, client_builder: HttpClientBuilder) -> HttpClientBuilder {
        match self {
            Self::Enabled => client_builder,
            Self::Disabled => {
                #[cfg(feature = "fips")]
                warn!(
                    config_key = "skip_ssl_validation",
                    "Disabling TLS certificate validation in a FIPS build means server certificates are no longer \
                     verified, even though the underlying TLS cipher suites and key exchange remain FIPS-compliant."
                );
                #[cfg(not(feature = "fips"))]
                warn!(
                    config_key = "skip_ssl_validation",
                    "TLS certificate validation is disabled for Datadog intake forwarding."
                );
                client_builder.with_tls_config(|builder| builder.danger_accept_invalid_certs())
            }
        }
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
        let endpoint_name: Arc<EndpointNameFn> = Arc::new(endpoint_name);
        let endpoint_name_for_client = Arc::clone(&endpoint_name);
        let mut client_builder = HttpClient::builder()
            .with_request_timeout(config.request_timeout())
            .with_max_idle_conns_per_host(config.max_idle_connections_per_host())
            .with_min_tls_version(config.min_tls_version())
            .with_http_protocol(config.http_protocol())
            .with_bytes_sent_counter(telemetry.bytes_sent().clone())
            .with_endpoint_telemetry(
                metrics_builder.clone(),
                Some(move |uri: &Uri| endpoint_name_for_client(uri)),
            );
        if let Some(path) = config.ssl_key_log_file_path() {
            client_builder = client_builder.with_tls_config(|builder| builder.with_key_log_file(path));
        }
        if let Some(proxy) = config.proxy() {
            client_builder = client_builder.with_proxies(proxy.build()?);
        }

        if config.connection_reset_interval() > Duration::ZERO {
            client_builder = client_builder.with_connection_age_limit(config.connection_reset_interval());
        }

        client_builder = TlsCertificateValidation::from_forwarder_config(&config).apply_to(client_builder);

        let client = client_builder.build()?;

        let emitter = DiagnosticsEmitter::from_current(context.identity())
            .error_context("Failed to create diagnostics emitter for transaction forwarder.")?;

        Ok(Self {
            context,
            config,
            live_config,
            telemetry,
            metrics_builder,
            client,
            endpoint_name,
            endpoints,
            endpoint_request_mapper_factory,
            emitter,
            _marker: PhantomData,
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
            live_config,
            telemetry,
            metrics_builder,
            client,
            endpoint_name,
            endpoints,
            endpoint_request_mapper_factory,
            emitter,
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
                endpoint_name,
                endpoints,
                endpoint_request_mapper_factory,
                emitter,
            ),
        );

        Handle {
            transactions_tx,
            io_shutdown_rx,
        }
    }

    /// Returns API key validation for the startup endpoint set.
    ///
    /// The forwarder's diagnostics emitter (see [`with_diagnostics_emitter`][Self::with_diagnostics_emitter]) is used to
    /// surface a diagnostic event when validation determines that every configured API key is invalid, allowing
    /// interested subscribers to react to the credentials being rejected.
    pub(crate) fn api_key_validator(&self) -> ApiKeyValidator {
        ApiKeyValidator::new(
            self.endpoints.clone(),
            self.client.clone(),
            self.live_config.clone(),
            self.config.api_key_validation_interval(),
            self.emitter.clone(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_io_loop<B>(
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, io_shutdown_tx: oneshot::Sender<()>,
    context: ComponentContext, config: ForwarderConfiguration, live_config: Option<GenericConfiguration>,
    service: HttpClient, telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    endpoint_name: Arc<EndpointNameFn>, resolved_endpoints: Vec<RoutableEndpoint>,
    endpoint_request_mapper_factory: EndpointRequestMapperFactory<B>, emitter: DiagnosticsEmitter,
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
        let retry_telemetry = TransactionRetryTelemetry::from_builder(&metrics_builder, &endpoint_domain);

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
                retry_telemetry,
                Arc::clone(&endpoint_name),
                route,
                resolved_endpoint,
                endpoint_request_mapper_factory.clone(),
                emitter.clone(),
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
        let is_metrics_request =
            is_metrics_request_uri(transaction.request_uri(), config.v3_api().series.beta_route.as_str());
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

fn is_metrics_request_uri(uri: &Uri, v3_beta_series_route: &str) -> bool {
    METRIC_INTAKE_PATHS.contains(&uri.path()) || uri.path() == v3_beta_series_route
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

fn track_transaction_input_for_endpoint(
    telemetry_by_endpoint: &mut FastHashMap<MetaString, TransactionInputTelemetry>, telemetry: &ComponentTelemetry,
    endpoint_domain: &str, endpoint_name: &str, transaction_size: u64,
) {
    if let Some(transaction_input_telemetry) = telemetry_by_endpoint.get(endpoint_name) {
        transaction_input_telemetry.track(transaction_size);
        return;
    }

    let endpoint_name = MetaString::from(endpoint_name);
    let transaction_input_telemetry = telemetry.register_transaction_input_telemetry(endpoint_domain, &endpoint_name);
    transaction_input_telemetry.track(transaction_size);
    telemetry_by_endpoint.insert(endpoint_name, transaction_input_telemetry);
}

#[allow(clippy::too_many_arguments)]
async fn run_endpoint_io_loop<B>(
    mut txns_rx: mpsc::Receiver<Transaction<B>>, task_barrier: Arc<Barrier>, context: ComponentContext,
    config: ForwarderConfiguration, live_config: Option<GenericConfiguration>, service: HttpClient,
    telemetry: ComponentTelemetry, txnq_telemetry: TransactionQueueTelemetry,
    mut retry_telemetry: TransactionRetryTelemetry, endpoint_name: Arc<EndpointNameFn>, route: EndpointRoute,
    endpoint: ResolvedEndpoint, endpoint_request_mapper_factory: EndpointRequestMapperFactory<B>,
    emitter: DiagnosticsEmitter,
) where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let queue_id = generate_retry_queue_id(context, &endpoint);
    let endpoint_url = endpoint.endpoint().to_string();
    let configured_endpoint = endpoint.configured_endpoint().to_string();
    let endpoint_domain = endpoint.endpoint().origin().ascii_serialization();

    // Match against the endpoint string from configuration, not the version-prefixed URL used for requests.
    let v3_api = config.v3_api();
    let metrics_primary_v3_override = (route == EndpointRoute::MetricsPrimary)
        .then(|| config.opw_metrics_v3_series_override())
        .flatten();
    let serializer_v3_configured_endpoint =
        (route == EndpointRoute::MetricsPrimary).then(|| config.primary_configured_endpoint());
    let endpoint_v3_settings = if config.compressor_disables_metrics_v3() {
        EndpointV3Settings::disabled()
    } else {
        EndpointV3Settings::from_v3_config(V3EndpointConfig {
            configured_endpoint: &configured_endpoint,
            resolved_endpoint: endpoint.endpoint(),
            serializer_v3_configured_endpoint: serializer_v3_configured_endpoint.as_deref(),
            data_plane_v3_series_enabled: config.data_plane_metrics_v3_series_enabled(),
            series_config: config.use_v3_api_series(),
            metrics_primary_v3_override,
            serializer_v3_series_endpoints: &v3_api.series.endpoints,
            serializer_v3_sketches_endpoints: &v3_api.sketches.endpoints,
            series_validate: v3_api.series.validate,
            sketches_validate: v3_api.sketches.validate,
            series_shadow_sites: &v3_api.series.shadow_sites,
        })
    };
    debug!(
        endpoint_url,
        endpoint_concurrency = config.endpoint_concurrency(),
        configured_endpoint,
        ?endpoint_v3_settings,
        "Starting endpoint I/O task."
    );
    let endpoint_request_mapper = Arc::new(Mutex::new((endpoint_request_mapper_factory)(endpoint)));

    // Build our endpoint service.
    //
    // This is where we'll modify the incoming transaction for our our specific endpoint, such as setting the host portion
    // of the URI, adding the API key as a header, and so on.
    //
    // The body type conversion from `TransactionBody<B>` to `ClientBody` happens as the innermost layer,
    // after the retry circuit breaker. This ensures that retry and open errors return the original
    // `Request<TransactionBody<B>>` so we can reassemble it into a `Transaction<B>` for re-enqueuing.
    //
    // The invalid API key inspection layer sits *below* the retry circuit breaker so that it observes the raw response
    // from the client. When secrets management is enabled, a 403 is classified as retriable and the circuit breaker
    // converts it into a `RetryCircuitBreakerError::Retry` error that only re-enqueues the transaction — it never
    // surfaces the response to the loop below — so this layer is the only place that reliably sees the rejection.
    let mut service = ServiceBuilder::new()
        // Set the request's URI and endpoint-specific headers.
        .map_request({
            let endpoint_request_mapper = Arc::clone(&endpoint_request_mapper);
            move |request| {
                let mut mapper = endpoint_request_mapper
                    .lock()
                    .expect("endpoint request mapper mutex poisoned");
                mapper(request)
            }
        })
        // Signal backend support for arbitrary tag values when configured.
        .map_request(with_allow_arbitrary_tags(config.allow_arbitrary_tags()))
        // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the request.
        .map_request(with_version_info())
        .concurrency_limit(config.endpoint_concurrency())
        .layer(RetryCircuitBreakerLayer::new(
            config.retry().to_default_http_retry_policy(live_config),
        ))
        .layer(build_diagnostics_layer(emitter, endpoint_url.clone()))
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
    let mut transaction_input_telemetry_by_endpoint = FastHashMap::<MetaString, TransactionInputTelemetry>::default();
    let mut done = false;

    loop {
        select! {
            // Try and drain the next transaction from our channel, and push it into the pending transactions queue.
            maybe_txn = txns_rx.recv(), if !done => match maybe_txn {
                Some(txn) => {
                    // Filter transactions based on endpoint's V3 settings and the transaction's payload info.
                    let payload_info = txn.metadata().payload_info;
                    if !endpoint_v3_settings.should_receive_payload(payload_info) {
                        debug!(
                            endpoint_url,
                            ?payload_info,
                            "Filtering out transaction based on endpoint V3 settings."
                        );
                        continue;
                    }
                    let txn = if endpoint_v3_settings.should_receive_validation_headers(payload_info) {
                        txn
                    } else {
                        strip_metrics_validation_headers(txn)
                    };
                    let transaction_size = txn.size_bytes();
                    let resolved = endpoint_name(txn.request_uri());
                    let logical = resolved.as_deref().unwrap_or_else(|| txn.request_uri().path());
                    track_transaction_input_for_endpoint(
                        &mut transaction_input_telemetry_by_endpoint,
                        &telemetry,
                        &endpoint_domain,
                        logical,
                        transaction_size,
                    );

                    match pending_txns.push_high_priority(txn).await {
                        Ok(push_result) => track_queue_drops(&telemetry, &endpoint_domain, push_result),
                        Err(e) => error!(endpoint_url, error = %e, "Failed to enqueue transaction. Events may be permanently lost."),
                    }
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
                Ok(svc) => if let Some(pending_txn) = pending_txns.pop().await {
                    let (metadata, request, retry_counters) = prepare_transaction_for_dispatch(
                        pending_txn,
                        &mut retry_telemetry,
                        endpoint_name.as_ref(),
                    );
                    in_flight.spawn(svc.call(request).map(move |result| InFlightTransaction {
                        metadata,
                        retry_counters,
                        result,
                    }));

                    debug!(endpoint_url, "Request sent.");
                },
                Err(e) => match e {
                    RetryCircuitBreakerError::Service(e) => {
                        telemetry.track_sent_request_error();
                        error!(endpoint_url, error = %e, error_source = ?e.source(), "Unexpected error when querying service for readiness.");
                        break;
                    },
                    RetryCircuitBreakerError::Retry(_) | RetryCircuitBreakerError::Open(_) => {
                        unreachable!("should not get retry or open error when querying service for readiness")
                    },
                }
            },

            // Drive any in-flight transactions to completion.
            maybe_result = in_flight.join_next(), if !in_flight.is_empty() => {
                let task_result = maybe_result.expect("in_flight marked as not being empty");
                handle_in_flight_transaction_result(
                    task_result,
                    &mut pending_txns,
                    &telemetry,
                    &endpoint_url,
                    &endpoint_domain,
                ).await;
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

fn strip_metrics_validation_headers<B>(txn: Transaction<B>) -> Transaction<B>
where
    B: Buf + Clone,
{
    let (metadata, mut request) = txn.into_parts();
    let headers = request.headers_mut();
    headers.remove("X-Metrics-Request-ID");
    headers.remove("X-Metrics-Request-Seq");
    headers.remove("X-Metrics-Request-Len");
    Transaction::reassemble(metadata, request)
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
    if push_result.had_drops() {
        saluki_antithesis::sometimes!(
            true,
            "ADP dropped transactions",
            {
                "domain": domain,
                "items_dropped": push_result.items_dropped,
                "events_dropped": push_result.events_dropped,
                "data_points_dropped": push_result.data_points_dropped
            }
        );
    }

    telemetry.track_dropped_items(push_result.items_dropped);
    telemetry.track_dropped_events(push_result.events_dropped);
    telemetry.track_data_points_dropped(domain, push_result.data_points_dropped);
}

/// Processes an HTTP response to a forwarded intake request, updating telemetry and logging as appropriate.
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
        saluki_antithesis::sometimes!(
            true,
            "ADP forwarded a payload to the intake",
            { "domain": domain }
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

fn build_diagnostics_layer(emitter: DiagnosticsEmitter, endpoint_url: String) -> HttpInspectionLayer {
    let last_emit = Mutex::new(None::<Instant>);
    let forbidden_inspector = move || {
        let now = Instant::now();
        {
            let mut last_emit = last_emit.lock().expect("invalid API key emit mutex poisoned");
            if last_emit.is_some_and(|last| now.duration_since(last) < INVALID_API_KEY_EMIT_INTERVAL) {
                return;
            }
            *last_emit = Some(now);
        }

        emitter.emit(DiagnosticEvent::new(
            format!("Datadog API key rejected as invalid (HTTP 403) when forwarding to '{endpoint_url}'."),
            DiagnosticDetails::InvalidApiKey,
        ));
    };

    HttpInspectionLayer::new().with_inspector(StatusCode::FORBIDDEN, forbidden_inspector)
}

enum PendingTransaction<T> {
    HighPriority(T),
    LowPriority(T),
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
    /// The high-priority queue is drained first before attempting to pop from the low-priority queue. The returned
    /// variant identifies the queue that contained the transaction.
    pub async fn pop(&mut self) -> Option<PendingTransaction<T>> {
        // We bias towards handling enqueued transactions first, since those are our "high priority" transactions, and we
        // want to keep them flowing as fast as possible.
        loop {
            if let Some(transaction) = self.high_priority.pop_front() {
                self.telemetry.high_prio_queue_removals().increment(1);

                debug!(
                    high_prio_queue_len = self.high_priority.len(),
                    "Dequeued pending transaction from high-priority queue."
                );
                return Some(PendingTransaction::HighPriority(transaction));
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
                    return Some(PendingTransaction::LowPriority(transaction));
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
    use std::{
        future::{pending, Pending},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, OnceLock,
        },
    };

    use bytes::Bytes;
    use http::StatusCode;
    use http_body_util::Empty;
    use rustls::{version::TLS12, RootCertStore, ServerConfig};
    use saluki_common::buf::FrozenChunkedBytesBuffer;
    use saluki_config::config_from;
    use saluki_core::{
        observability::ComponentMetricsExt as _,
        runtime::state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter},
    };
    use saluki_io::net::client::http::TlsMinimumVersion;
    use saluki_metrics::test::TestRecorder;
    use saluki_tls::test_util::SelfSignedCert;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
        time::{timeout, Duration},
    };
    use tokio_rustls::TlsAcceptor;
    use tower::{retry::Policy, service_fn};

    use super::*;
    use crate::common::datadog::endpoints::AdditionalEndpoints;
    use crate::common::datadog::transaction::{Metadata as TxnMetadata, Transaction};
    use crate::common::datadog::{
        METRICS_SERIES_V1_PATH, METRICS_SERIES_V2_PATH, METRICS_SERIES_V3_BETA_PATH, METRICS_SERIES_V3_PATH,
        METRICS_SKETCHES_PATH, METRICS_SKETCHES_V3_PATH,
    };

    fn test_component_context() -> ComponentContext {
        ComponentContext::test_forwarder("test_forwarder")
    }

    fn uri(path: &'static str) -> Uri {
        Uri::from_static(path)
    }

    fn is_metrics_request_path(path: &'static str) -> bool {
        is_metrics_request_uri(&uri(path), METRICS_SERIES_V3_BETA_PATH)
    }

    fn forwarder_config_from_value(value: serde_json::Value) -> ForwarderConfiguration {
        serde_json::from_value(value).expect("ForwarderConfiguration should deserialize")
    }

    #[test]
    fn identifies_metrics_request_paths() {
        assert!(is_metrics_request_path(METRICS_SERIES_V1_PATH));
        assert!(is_metrics_request_path(METRICS_SERIES_V2_PATH));
        assert!(is_metrics_request_path(METRICS_SERIES_V3_PATH));
        assert!(is_metrics_request_path(METRICS_SERIES_V3_BETA_PATH));
        assert!(is_metrics_request_path(METRICS_SKETCHES_PATH));
        assert!(is_metrics_request_path(METRICS_SKETCHES_V3_PATH));
        assert!(!is_metrics_request_path("/api/v2/logs"));
        assert!(!is_metrics_request_path("/api/v0.2/traces"));
    }

    #[test]
    fn identifies_configured_v3_beta_series_route_as_metrics_path() {
        assert!(is_metrics_request_uri(
            &uri("/custom/v3beta/series"),
            "/custom/v3beta/series"
        ));
        assert!(!is_metrics_request_uri(
            &uri("/custom/v3beta/series"),
            METRICS_SERIES_V3_BETA_PATH
        ));
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
        let context = test_component_context();
        let additional: AdditionalEndpoints = serde_yaml::from_str(
            r#"
app.datadoghq.com: [key-a]
https://app.datadoghq.com: [key-b]
"#,
        )
        .expect("additional endpoints should deserialize");
        let endpoints = additional.resolved_endpoints(None).expect("endpoints should resolve");

        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].endpoint(), endpoints[1].endpoint());

        let first_queue_id = generate_retry_queue_id(context.clone(), &endpoints[0]);
        let second_queue_id = generate_retry_queue_id(context, &endpoints[1]);

        assert_ne!(first_queue_id, second_queue_id);
    }

    #[test]
    fn retry_queue_id_uses_additional_endpoint_api_key_index() {
        let context = test_component_context();
        let additional: AdditionalEndpoints = serde_yaml::from_str(
            r#"
app.datadoghq.com: [key-a, key-b]
"#,
        )
        .expect("additional endpoints should deserialize");
        let endpoints = additional.resolved_endpoints(None).expect("endpoints should resolve");

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

        let cert = SelfSignedCert::localhost();
        let server_config_builder = match versions {
            TestServerTlsVersions::Default => ServerConfig::builder(),
            TestServerTlsVersions::Tls12Only => ServerConfig::builder_with_protocol_versions(&[&TLS12]),
        };
        let server_config = server_config_builder
            .with_no_client_auth()
            .with_single_cert(cert.cert_chain(), cert.private_key())
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

    fn test_logical_endpoint(uri: &Uri) -> Option<MetaString> {
        (uri.path() == "/api/v2/series").then(|| MetaString::from_static("series_v2"))
    }

    fn retry_metric_tags(domain: &str) -> [(&str, &str); 2] {
        [("domain", domain), ("endpoint", "series_v2")]
    }

    fn transaction_retry_telemetry(domain: &str) -> TransactionRetryTelemetry {
        TransactionRetryTelemetry::from_builder(&MetricsBuilder::default(), domain)
    }

    #[test]
    fn input_telemetry_reuses_cached_handles_for_a_long_unknown_path() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut telemetry_by_endpoint = FastHashMap::default();
        let endpoint_domain = "https://example.com";
        let unknown_uri: Uri = format!("/{}", "unknown/".repeat(128))
            .parse()
            .expect("long unknown path should parse");
        let resolved = test_logical_endpoint(&unknown_uri);
        let logical = resolved.as_deref().unwrap_or_else(|| unknown_uri.path());

        track_transaction_input_for_endpoint(&mut telemetry_by_endpoint, &telemetry, endpoint_domain, logical, 12);
        track_transaction_input_for_endpoint(&mut telemetry_by_endpoint, &telemetry, endpoint_domain, logical, 30);

        assert_eq!(telemetry_by_endpoint.len(), 1);
        let metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("domain", endpoint_domain.to_string()),
                    metrics::Label::new("endpoint", logical.to_string()),
                ],
            )
        };
        assert_eq!(
            recorder.counter(metric_key("network_http_requests_input_total")),
            Some(2)
        );
        assert_eq!(
            recorder.counter(metric_key("network_http_requests_input_bytes_total")),
            Some(42)
        );
    }

    #[tokio::test]
    async fn high_priority_overflow_is_counted_when_later_dispatched_as_retry() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let domain = "https://example.com";
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut retry_telemetry = transaction_retry_telemetry(domain);
        let (queue_telemetry, queue_domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(1, retry_queue, queue_telemetry, queue_domain, 900);
        let push_result = pending_txns
            .push_high_priority(build_test_transaction())
            .await
            .expect("first push should succeed");
        assert!(!push_result.had_drops());
        let push_result = pending_txns
            .push_high_priority(build_test_transaction())
            .await
            .expect("overflow push should succeed");
        assert!(!push_result.had_drops());

        let first = pending_txns
            .pop()
            .await
            .expect("high-priority transaction should be queued");
        let (metadata, _request, retry_counters) =
            prepare_transaction_for_dispatch(first, &mut retry_telemetry, &test_logical_endpoint);
        assert!(retry_counters.is_none());
        handle_in_flight_transaction_result::<FrozenChunkedBytesBuffer>(
            Ok(InFlightTransaction {
                metadata,
                retry_counters,
                result: Err(RetryCircuitBreakerError::Service(
                    Box::new(std::io::Error::other("request failed")) as BoxError,
                )),
            }),
            &mut pending_txns,
            &telemetry,
            domain,
            domain,
        )
        .await;
        assert_eq!(
            recorder.counter(("network_http_requests_retries_total", &retry_metric_tags(domain))),
            None
        );

        let overflow = pending_txns.pop().await.expect("overflow transaction should be queued");
        let (metadata, _request, retry_counters) =
            prepare_transaction_for_dispatch(overflow, &mut retry_telemetry, &test_logical_endpoint);
        assert!(retry_counters.is_some());
        handle_in_flight_transaction_result::<FrozenChunkedBytesBuffer>(
            Ok(InFlightTransaction {
                metadata,
                retry_counters,
                result: Err(RetryCircuitBreakerError::Service(
                    Box::new(std::io::Error::other("request failed")) as BoxError,
                )),
            }),
            &mut pending_txns,
            &telemetry,
            domain,
            domain,
        )
        .await;
        assert_eq!(
            recorder.counter(("network_http_requests_retries_total", &retry_metric_tags(domain))),
            Some(1)
        );
    }

    #[derive(Clone)]
    struct OpenOnErrorPolicy;

    impl<B> Policy<Request<TransactionBody<B>>, Response<Incoming>, BoxError> for OpenOnErrorPolicy {
        type Future = Pending<()>;

        fn retry(
            &mut self, _req: &mut Request<TransactionBody<B>>, result: &mut Result<Response<Incoming>, BoxError>,
        ) -> Option<Self::Future> {
            result.is_err().then(pending)
        }

        fn clone_request(&mut self, req: &Request<TransactionBody<B>>) -> Option<Request<TransactionBody<B>>> {
            Some(
                Request::builder()
                    .uri(req.uri().clone())
                    .body(TransactionBody::Rehydrated(None))
                    .expect("request should build"),
            )
        }
    }

    #[tokio::test]
    async fn circuit_breaker_blocked_retry_is_requeued_without_retry_dispatch() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let domain = "https://example.com";
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut retry_telemetry = transaction_retry_telemetry(domain);
        let (queue_telemetry, queue_domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(1, retry_queue, queue_telemetry, queue_domain, 900);
        let inner_calls = Arc::new(AtomicUsize::new(0));
        let mut service = ServiceBuilder::new()
            .layer(RetryCircuitBreakerLayer::new(OpenOnErrorPolicy))
            .service(service_fn({
                let inner_calls = Arc::clone(&inner_calls);
                move |_request: Request<TransactionBody<FrozenChunkedBytesBuffer>>| {
                    inner_calls.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err::<Response<Incoming>, BoxError>(Box::new(std::io::Error::other(
                        "request failed",
                    ))))
                }
            }));

        let (_, initial_request) = build_test_transaction().into_parts();
        assert!(matches!(
            service.call(initial_request).await,
            Err(RetryCircuitBreakerError::Retry(_))
        ));
        let inner_calls_before_blocked_retry = inner_calls.load(Ordering::SeqCst);
        assert_eq!(inner_calls_before_blocked_retry, 1);

        let push_result = pending_txns
            .push_low_priority(build_test_transaction())
            .await
            .expect("retry queue push should succeed");
        assert!(!push_result.had_drops());
        let pending_txn = pending_txns.pop().await.expect("retry should be queued");
        let (metadata, request, retry_counters) =
            prepare_transaction_for_dispatch(pending_txn, &mut retry_telemetry, &test_logical_endpoint);
        let result = service.call(request).await;
        assert!(matches!(&result, Err(RetryCircuitBreakerError::Open(_))));

        handle_in_flight_transaction_result::<FrozenChunkedBytesBuffer>(
            Ok(InFlightTransaction {
                metadata,
                retry_counters,
                result,
            }),
            &mut pending_txns,
            &telemetry,
            domain,
            domain,
        )
        .await;

        assert_eq!(inner_calls.load(Ordering::SeqCst), inner_calls_before_blocked_retry);
        assert_eq!(pending_txns.low_priority.len(), 1);
        assert_eq!(
            recorder.counter(("network_http_requests_retries_total", &retry_metric_tags(domain))),
            Some(0)
        );
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", &retry_metric_tags(domain))),
            Some(1)
        );
    }

    #[tokio::test]
    async fn retry_sourced_service_completion_is_counted_without_requeue() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let domain = "https://example.com";
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        let mut retry_telemetry = transaction_retry_telemetry(domain);
        let (queue_telemetry, queue_domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(1, retry_queue, queue_telemetry, queue_domain, 900);
        let push_result = pending_txns
            .push_low_priority(build_test_transaction())
            .await
            .expect("retry queue push should succeed");
        assert!(!push_result.had_drops());
        let pending_txn = pending_txns.pop().await.expect("retry should be queued");
        let (metadata, _request, retry_counters) =
            prepare_transaction_for_dispatch(pending_txn, &mut retry_telemetry, &test_logical_endpoint);

        handle_in_flight_transaction_result::<FrozenChunkedBytesBuffer>(
            Ok(InFlightTransaction {
                metadata,
                retry_counters,
                result: Err(RetryCircuitBreakerError::Service(
                    Box::new(std::io::Error::other("request failed")) as BoxError,
                )),
            }),
            &mut pending_txns,
            &telemetry,
            domain,
            domain,
        )
        .await;
        assert!(pending_txns.is_empty());
        assert_eq!(
            recorder.counter(("network_http_requests_retries_total", &retry_metric_tags(domain))),
            Some(1)
        );
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", &retry_metric_tags(domain))),
            Some(0)
        );
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
        assert!(matches!(
            pending_txns.pop().await,
            Some(PendingTransaction::LowPriority(transaction)) if transaction == "retry"
        ));
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

    async fn persisted_retry_queue(root_path: &std::path::Path, max_on_disk_bytes: u64) -> RetryQueue<String> {
        RetryQueue::new("test".to_string(), 1024)
            .with_disk_persistence(PersistedQueueArgs {
                root_path: root_path.to_path_buf(),
                max_on_disk_bytes,
                storage_max_disk_ratio: 1.0,
                disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.to_path_buf())),
                max_age_days: 10,
            })
            .await
            .expect("disk persistence should initialize")
    }

    #[tokio::test]
    async fn flush_without_disk_persistence_drops_pending_transactions() {
        // Documented shutdown behavior: flush moves high-priority transactions into the low-priority queue
        // and then flushes it. With no disk persistence configured, all pending transactions are dropped,
        // and the returned `PushResult` accounts for every dropped item and event.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1024);
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry, domain, 900);

        let _ = pending_txns
            .push_high_priority("high".to_string())
            .await
            .expect("high-priority push should succeed");
        let _ = pending_txns
            .push_low_priority("low".to_string())
            .await
            .expect("low-priority push should succeed");

        let push_result = pending_txns.flush().await.expect("flush should succeed");

        assert!(push_result.had_drops());
        assert_eq!(push_result.items_dropped, 2);
        assert_eq!(push_result.events_dropped, 2);
    }

    #[tokio::test]
    async fn flush_with_disk_persistence_persists_pending_transactions() {
        // Documented shutdown behavior: when disk persistence is enabled, flush persists all pending
        // transactions (high- and low-priority) to disk instead of dropping them.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();

        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let root_path = temp_dir.path().to_path_buf();
        let retry_queue = persisted_retry_queue(&root_path, 4096).await;
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry, domain, 900);

        let _ = pending_txns
            .push_high_priority("high".to_string())
            .await
            .expect("high-priority push should succeed");
        let _ = pending_txns
            .push_low_priority("low".to_string())
            .await
            .expect("low-priority push should succeed");

        let push_result = pending_txns.flush().await.expect("flush should succeed");

        assert!(
            !push_result.had_drops(),
            "disk persistence should retain transactions on flush, not drop them"
        );

        let mut retry_queue = persisted_retry_queue(&root_path, 4096).await;
        let mut persisted = Vec::new();
        while let Some(transaction) = retry_queue
            .pop()
            .await
            .expect("persisted transaction should be readable")
        {
            persisted.push(transaction);
        }
        persisted.sort();

        assert_eq!(persisted, ["high", "low"]);
    }

    #[tokio::test]
    async fn flush_propagates_high_priority_transfer_errors() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let retry_queue = RetryQueue::new("test".to_string(), 1);
        let mut pending_txns = PendingTransactions::new(1, retry_queue, telemetry, domain, 900);

        let _ = pending_txns
            .push_high_priority("too large".to_string())
            .await
            .expect("high-priority queue should accept the transaction");

        let error = match pending_txns.flush().await {
            Ok(_) => panic!("flush should propagate the low-priority queue size error"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("Entry too large to fit into retry queue"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn flush_propagates_disk_persistence_errors() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let retry_queue = persisted_retry_queue(temp_dir.path(), 1).await;
        let mut pending_txns = PendingTransactions::new(1, retry_queue, telemetry, domain, 900);

        let _ = pending_txns
            .push_low_priority("too large".to_string())
            .await
            .expect("in-memory low-priority queue should accept the transaction");

        let error = match pending_txns.flush().await {
            Ok(_) => panic!("flush should propagate the disk persistence size error"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("Entry is too large to persist"),
            "unexpected error: {error}"
        );
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
    fn skip_ssl_validation_allowed_in_fips_mode() {
        init_tls_crypto_provider();

        let client_builder =
            HttpClient::builder().with_tls_config(|builder| builder.with_root_cert_store(RootCertStore::empty()));

        TlsCertificateValidation::Disabled
            .apply_to(client_builder)
            .build()
            .expect("HTTP client should build with skip_ssl_validation enabled in FIPS mode");
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

    /// Starts a minimal HTTP server on `127.0.0.1:0` that records each request and cycles through
    /// `statuses` in order, replying with the last entry forever once the sequence is exhausted.
    ///
    /// Returns the server's `http://127.0.0.1:PORT/` URL and a counter that increments once per
    /// accepted/processed connection (one connection per request, since the server replies with
    /// `Connection: close`).
    async fn start_recording_http_server(statuses: Vec<StatusCode>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let counter = Arc::new(AtomicUsize::new(0));

        let statuses = Arc::new(statuses);
        let counter_for_task = Arc::clone(&counter);
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let statuses = Arc::clone(&statuses);
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
                    let idx = nth.min(statuses.len() - 1);
                    let status = statuses[idx];

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
    ) -> (DataspaceRegistry, TransactionForwarder<FrozenChunkedBytesBuffer>) {
        // The HTTP client builder requires the process-wide TLS crypto provider to be initialized, even when the
        // forwarder is pointed at a plain HTTP endpoint.
        init_tls_crypto_provider();

        // Tight timeouts and small backoffs keep the test under a couple seconds even with retries.
        let value = serde_json::json!({
            "api_key": "test-api-key",
            "dd_url": forwarder_url,
            "forwarder_timeout": 1u64,
            "forwarder_max_concurrent_requests": 1usize,
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
        let context = test_component_context();
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        let dataspace = DataspaceRegistry::new();
        let forwarder = dataspace
            .with_current(|| {
                TransactionForwarder::<FrozenChunkedBytesBuffer>::from_config(
                    context,
                    forwarder_config,
                    live_config,
                    test_logical_endpoint,
                    telemetry,
                    metrics_builder,
                )
            })
            .expect("forwarder should build");

        (dataspace, forwarder)
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
        Transaction::from_original(TxnMetadata::from_event_and_data_point_count(1, 0), request)
    }

    async fn config_with(values: serde_json::Value) -> GenericConfiguration {
        config_from(values).await
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

    #[tokio::test(flavor = "current_thread")]
    async fn forwarder_counts_only_dispatched_retries_after_server_errors() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (server_url, counter) = start_recording_http_server(vec![
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::OK,
        ])
        .await;
        let (_, forwarder) = build_test_forwarder(&server_url, None);

        let handle = forwarder.spawn().await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        let observed = wait_for_count_at_least(&counter, 3, Duration::from_secs(3)).await;
        handle.shutdown().await;
        assert!(
            observed >= 3,
            "forwarder should dispatch two retries (saw {observed} requests)"
        );

        let retry_metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("component_id", "test_forwarder"),
                    metrics::Label::new("component_type", "forwarder"),
                    metrics::Label::new("domain", server_url.trim_end_matches('/').to_string()),
                    metrics::Label::new("endpoint", "series_v2"),
                ],
            )
        };
        assert_eq!(
            recorder.counter(retry_metric_key("network_http_requests_retries_total")),
            Some(2)
        );
        assert_eq!(
            recorder.counter(retry_metric_key("network_http_requests_requeued_total")),
            Some(0)
        );
    }

    #[tokio::test]
    async fn forwarder_retries_403_when_secrets_in_use() {
        // The server returns 403 to the first request and 200 to every subsequent request; the forwarder must drive
        // at least one retry to observe the second request.
        let (server_url, counter) = start_recording_http_server(vec![StatusCode::FORBIDDEN, StatusCode::OK]).await;
        let live_config = config_with(json!({ "secret_backend_command": "/bin/true" })).await;
        let (_, forwarder) = build_test_forwarder(&server_url, Some(live_config));

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

    #[tokio::test]
    async fn forwarder_emits_invalid_api_key_diagnostic_on_403() {
        // The intake server rejects every request with a 403. Without secrets configured, a 403 is not retried, so the
        // single forwarded request flows straight through to response handling.
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::FORBIDDEN]).await;

        // Build a forwarder and then subscribe to diagnostic events from the dataspace it's attached to.
        let (dataspace, forwarder) = build_test_forwarder(&server_url, None);
        let mut events = dataspace.subscribe::<DiagnosticEvent>(IdentifierFilter::all());

        let handle = forwarder.spawn().await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        // The 403 response to the real request must produce an `InvalidApiKey` diagnostic event.
        match timeout(Duration::from_secs(3), events.recv()).await {
            Ok(Some(DataspaceUpdate::Message(_, event))) => {
                assert_eq!(event.details(), &DiagnosticDetails::InvalidApiKey);
            }
            other => panic!("expected an InvalidApiKey diagnostic event, got: {other:?}"),
        }

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_emits_invalid_api_key_diagnostic_on_403_when_secrets_in_use() {
        // With secrets management enabled, a 403 is classified as retriable, so the retry circuit breaker converts it
        // into a `Retry` error that never reaches the response-handling arm of the I/O loop. The diagnostic must still
        // be emitted, because it is produced by the inspection layer sitting below the retry circuit breaker. Without
        // that layer, the rejected key would be retried indefinitely without ever notifying anyone.
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::FORBIDDEN]).await;
        let live_config = config_with(json!({ "secret_backend_command": "/bin/true" })).await;

        // Build a forwarder and then subscribe to diagnostic events from the dataspace it's attached to.
        let (dataspace, forwarder) = build_test_forwarder(&server_url, Some(live_config));
        let mut events = dataspace.subscribe::<DiagnosticEvent>(IdentifierFilter::all());

        let handle = forwarder.spawn().await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        match timeout(Duration::from_secs(3), events.recv()).await {
            Ok(Some(DataspaceUpdate::Message(_, event))) => {
                assert_eq!(event.details(), &DiagnosticDetails::InvalidApiKey);
            }
            other => panic!("expected an InvalidApiKey diagnostic event, got: {other:?}"),
        }

        handle.shutdown().await;
    }
}
