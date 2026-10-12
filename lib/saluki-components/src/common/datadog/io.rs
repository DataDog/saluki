//! Datadog forwarder I/O.
//!
//! # Supervision
//!
//! Every task here is a supervised child of the forwarder component's supervisor, spawned on the ambient supervisor
//! that the component's own run-future executes under. They are siblings rather than a subtree: the main I/O loop, one
//! I/O loop per endpoint, and the background refreshers all sit side by side under the one supervisor, which is also
//! what bounds them -- a single [shutdown budget][saluki_core::runtime::Supervisor::with_shutdown_budget] covering the
//! component and everything it spawned.
//!
//! Shutdown needs no explicit handshake between them, because the tasks are connected by channels and each stops at
//! its own terminal condition. Dropping [`Handle`] closes the transaction channel, so the main I/O loop finishes and
//! drops its per-endpoint senders, so each endpoint loop completes its in-flight requests and flushes its retry queue.
//! The supervisor waits for all of them, which is what makes the drain ordered without any task having to know the
//! order. The refreshers have no terminal condition at all and are therefore
//! [`Brutal`][saluki_core::runtime::ShutdownStrategy::Brutal].
//!
//! In-flight requests remain an ordinary [`JoinSet`] owned by their endpoint loop rather than supervised children,
//! because the loop consumes each result to decide whether the transaction is done or needs re-enqueuing. Their
//! lifetime is already nested inside a supervised task, so they are torn down with it.
//!
//! # Recipient loops
//!
//! A delivery host runs the same endpoint loop per recipient, as a [`RecipientLoop`], which a host controls while it
//! runs: a close request retires it, draining its queued work for a bounded time, or closes it as above; a publication
//! changes its settings between requests; and it reports how it exited, once. Without a host, a forwarder's endpoint
//! loops are not controlled, and behave as described above.
//!
//! # Missing
//!
//! - Avoid breaking apart `TransactionForwarder` only to work around `#[allow(clippy::too_many_arguments)]`.
//! - Avoid initializing the process-wide crypto provider from tests.

use std::{
    collections::VecDeque,
    error::Error as _,
    future,
    marker::PhantomData,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use agent_data_plane_config::shared::Secrets;
use agent_data_plane_config::Live;
use bytes::Buf;
use futures::{future::BoxFuture, FutureExt as _};
use http::{Request, StatusCode, Uri};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::{body::Incoming, Response};
use saluki_common::{collections::FastHashMap, hash::hash_single_stable, time::get_unix_timestamp};
use saluki_core::components::ComponentContext;
use saluki_core::diagnostic::{DiagnosticDetails, DiagnosticEvent, DiagnosticsEmitter};
use saluki_core::runtime::{self, get_sanitized_name};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{
    client::http::{into_client_body, ClientBody, HttpClient, HttpClientBuilder},
    util::{
        middleware::{HttpInspectionLayer, RetryCircuitBreakerError, RetryCircuitBreakerLayer},
        retry::{
            DiskUsageRetrieverImpl, FlushError, PersistedQueueArgs, PushResult, RetryCauseTelemetry, RetryQueue,
            Retryable,
        },
    },
};
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tokio::{
    select,
    sync::{mpsc, mpsc::error::TryRecvError, oneshot, watch, Semaphore},
    task::{JoinError, JoinSet},
};
use tower::{limit::GlobalConcurrencyLimitLayer, retry::Policy, BoxError, Service, ServiceBuilder, ServiceExt as _};
use tracing::{debug, error, warn};
use url::Url;

use super::{
    api_key::{ApiKeyRefresher, LiveApiKeys},
    config::ForwarderConfiguration,
    delivery::{
        CloseRequest, DeliverySettings, DrainReport, RecipientExit, RecipientOrigin, RecipientPolicy,
        RecipientProtocol, RecipientSpec, ReopenReply, RetireClosure, SpillPolicy, V3Acceptance,
    },
    endpoints::{EndpointRoute, EndpointV3Settings, ResolvedEndpoint, RoutableEndpoint},
    middleware::{for_resolved_endpoint, with_allow_arbitrary_tags, with_version_info},
    retry::{RetryConfiguration, SecretsGate, SecretsGateRefresher, SecretsHttpRetryPolicy},
    retry_capacity::{TrafficRateWindow, RETRY_QUEUE_CAPACITY_BUCKET_DURATION_SECS},
    telemetry::{
        ComponentTelemetry, SharedTransactionQueueTelemetry, TransactionInputTelemetry, TransactionQueueTelemetry,
        TransactionRetryCounters, TransactionRetryTelemetry,
    },
    transaction::{Metadata, Transaction, TransactionBody},
    validation::{ApiKeyValidator, ValidationTargets},
    METRICS_SERIES_V3_PATH, METRIC_INTAKE_PATHS,
};

type EndpointNameFn = dyn Fn(&Uri) -> Option<MetaString> + Send + Sync;

struct InFlightTransaction<R> {
    metadata: Metadata,
    body_size: u64,
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

/// Queues a transaction again to be retried, and returns whether it was: one the retry queue refuses, being larger than
/// its bound, is dropped and counted.
async fn requeue_transaction<B>(
    metadata: Metadata, request: Request<TransactionBody<B>>, pending_txns: &mut PendingTransactions<Transaction<B>>,
    requeued_counters: Option<&TransactionRetryCounters>, telemetry: &ComponentTelemetry, endpoint_url: &str,
    endpoint_domain: &str,
) -> RequestOutcome
where
    B: Buf + Clone,
{
    let transaction = Transaction::reassemble(metadata, request);
    let mut refused = PushResult::default();
    refused.track_dropped_item(&transaction);
    match pending_txns.push_low_priority(transaction).await {
        Ok(push_result) => {
            if let Some(counters) = requeued_counters {
                counters.increment_requeued();
            }
            track_queue_drops(telemetry, endpoint_domain, &push_result);
            RequestOutcome::Requeued
        }
        Err(e) => {
            error!(
                endpoint_url,
                error = %e,
                "Failed to re-enqueue failed transaction. Events may be permanently lost."
            );
            track_queue_drops(telemetry, endpoint_domain, &refused);
            RequestOutcome::Failed
        }
    }
}

/// What became of one dispatched transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestOutcome {
    /// The intake accepted it.
    Delivered,

    /// It failed for good: the intake refused it, the request failed, or it could not be queued again.
    Failed,

    /// It was queued again to be retried.
    Requeued,
}

async fn handle_in_flight_transaction_result<B>(
    task_result: InFlightTaskResult<B>, pending_txns: &mut PendingTransactions<Transaction<B>>,
    telemetry: &ComponentTelemetry, endpoint_url: &str, endpoint_domain: &str,
) -> RequestOutcome
where
    B: Buf + Clone,
{
    let InFlightTransaction {
        metadata,
        body_size,
        retry_counters,
        result,
    } = match task_result {
        Ok(in_flight_txn) => in_flight_txn,
        Err(e) => {
            error!(endpoint_url, error = %e, error_source = ?e.source(), "Request task failed to run to completion.");
            return RequestOutcome::Failed;
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
            let delivered = process_http_response(
                http_response,
                metadata,
                body_size,
                telemetry,
                endpoint_url,
                endpoint_domain,
            )
            .await;
            if delivered {
                RequestOutcome::Delivered
            } else {
                RequestOutcome::Failed
            }
        }

        // The service itself encountered an error while sending the request or receiving the response:
        // connection reset by peer, I/O error, etc.
        Err(RetryCircuitBreakerError::Service(e)) => {
            telemetry.track_permanently_failed_transaction(&metadata, None, endpoint_domain);
            error!(endpoint_url, error = %e, error_source = ?e.source(), "Failed to send request.");
            RequestOutcome::Failed
        }

        // The request completed and needs to be retried, so re-enqueue it to the low-priority queue.
        Err(RetryCircuitBreakerError::Retry(req)) => {
            requeue_transaction(
                metadata,
                req,
                pending_txns,
                None,
                telemetry,
                endpoint_url,
                endpoint_domain,
            )
            .await
        }

        // The request was rejected before inward dispatch, so re-enqueue it without counting another retry.
        Err(RetryCircuitBreakerError::Open(req)) => {
            requeue_transaction(
                metadata,
                req,
                pending_txns,
                retry_counters.as_ref(),
                telemetry,
                endpoint_url,
                endpoint_domain,
            )
            .await
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

    /// Triggers the forwarder to shut down.
    ///
    /// Dropping the transaction channel is what starts the drain: the main I/O loop finishes, its per-endpoint senders
    /// drop, and each endpoint loop completes its in-flight requests and flushes its retry queue. This doesn't wait for
    /// any of that. The forwarder's tasks are children of the component's supervisor, so waiting for them -- bounded by
    /// that supervisor's shutdown budget -- is its job rather than the caller's.
    pub fn shutdown(self) {
        drop(self.transactions_tx);
    }
}

/// Minimum interval between successive `InvalidApiKey` diagnostic events emitted from the request I/O path for a single
/// endpoint.
///
/// We want to ensure there's a steady stream of these events while the invalid API key condition is still present, but
/// we want to avoid sending a deluge of events if there happens to be some bug that causes us to rapidly fire off
/// requests at a rate of more than one per second.
const INVALID_API_KEY_EMIT_INTERVAL: Duration = Duration::from_secs(1);

/// The live configuration a forwarder follows while it runs.
///
/// These are the inputs that can change after the topology is built, and they are not interchangeable.
#[derive(Clone)]
pub(crate) struct LiveForwarderConfiguration {
    /// The views the endpoints refresh their API keys from.
    pub(crate) api_keys: LiveApiKeys,

    /// The view the retry policy's gate reads to decide whether a rejected API key is worth retrying.
    pub(crate) secrets: Live<Secrets>,
}

impl Default for LiveForwarderConfiguration {
    /// Returns configuration that never changes: no key is refreshed and no secret can replace a rejected key.
    fn default() -> Self {
        Self {
            api_keys: LiveApiKeys::default(),
            secrets: Live::new_fixed(Secrets::default()),
        }
    }
}

/// Transaction forwarder for Datadog endpoints.
pub struct TransactionForwarder<B> {
    context: ComponentContext,
    config: ForwarderConfiguration,
    live: LiveForwarderConfiguration,
    telemetry: ComponentTelemetry,
    metrics_builder: MetricsBuilder,
    client: HttpClient,
    endpoint_name: Arc<EndpointNameFn>,
    endpoints: Vec<RoutableEndpoint>,
    api_key_refresher: Option<ApiKeyRefresher>,
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
    ///
    /// `live` carries the configuration the forwarder follows while it runs; see
    /// [`LiveForwarderConfiguration`].
    ///
    /// # Errors
    ///
    /// Returns an error if an endpoint cannot be resolved, if the proxy or TLS settings cannot be applied to the HTTP
    /// client, or if the diagnostics emitter cannot be created.
    pub fn from_config<F>(
        context: ComponentContext, config: ForwarderConfiguration, live: LiveForwarderConfiguration, endpoint_name: F,
        telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        Self::from_config_with_endpoint_request_mapper(
            context,
            config,
            live,
            endpoint_name,
            telemetry,
            metrics_builder,
            Arc::new(default_endpoint_request_mapper::<B>),
        )
    }

    /// Creates a new `TransactionForwarder` with a custom endpoint request mapper.
    pub(crate) fn from_config_with_endpoint_request_mapper<F>(
        context: ComponentContext, config: ForwarderConfiguration, live: LiveForwarderConfiguration, endpoint_name: F,
        telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
        endpoint_request_mapper_factory: EndpointRequestMapperFactory<B>,
    ) -> Result<Self, GenericError>
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let endpoints = config.build_routable_endpoints()?;
        let api_key_refresher = ApiKeyRefresher::new(&endpoints, &live.api_keys);
        let endpoint_name: Arc<EndpointNameFn> = Arc::new(endpoint_name);
        let endpoint_name_for_client = Arc::clone(&endpoint_name);
        let mut client_builder = HttpClient::builder()
            .with_request_timeout(config.request_timeout())
            .with_max_idle_conns_per_host(config.max_idle_connections_per_host())
            .with_min_tls_version(config.min_tls_version())
            .with_tls_handshake_timeout(config.tls_handshake_timeout())
            .with_http_protocol(config.http_protocol())
            .with_endpoint_telemetry(
                metrics_builder.clone(),
                Some(move |uri: &Uri| endpoint_name_for_client(uri)),
            );
        if let Some(path) = config.ssl_key_log_file_path() {
            client_builder = client_builder.with_tls_config(|builder| builder.with_key_log_file(path));
        }
        client_builder = client_builder.with_proxies(config.proxy().build()?);

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
            live,
            telemetry,
            metrics_builder,
            client,
            endpoint_name,
            endpoints,
            api_key_refresher,
            endpoint_request_mapper_factory,
            emitter,
            _marker: PhantomData,
        })
    }

    /// Spawns the I/O task for the forwarder, and any associated endpoint I/O tasks.
    ///
    /// All of them are spawned as children of the ambient supervisor -- in practice, the supervisor of the forwarder
    /// component calling this -- which is what stops them, and bounds their drain, once the returned `Handle` is
    /// dropped. See the module documentation for how the drain is ordered.
    ///
    /// Returns a `Handle` that can be used to send transactions to the forwarder, as well as eventually shut it down in
    /// an orderly fashion.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a supervision tree.
    pub fn spawn(self) -> Handle<B> {
        let (transactions_tx, transactions_rx) = mpsc::channel(8);

        // TODO: do not destructure self as a way to fix the #[allow(clippy::too_many_arguments)] annotations
        let Self {
            context,
            config,
            live,
            telemetry,
            metrics_builder,
            client,
            endpoint_name,
            endpoints,
            api_key_refresher,
            endpoint_request_mapper_factory,
            emitter,
            _marker,
        } = self;

        // The endpoints already hold the keys configuration reports, stored when the refresher was
        // built; this task carries the changes that come after that.
        if let Some(api_key_refresher) = api_key_refresher {
            api_key_refresher.spawn();
        }

        // The retry classifiers cannot await, so a task carries secrets changes into the gate they read.
        let secrets_refresher = SecretsGateRefresher::new(live.secrets);
        let secrets = secrets_refresher.gate.clone();
        secrets_refresher.spawn();

        runtime::worker(
            "io_loop",
            run_io_loop(
                transactions_rx,
                context,
                config,
                secrets,
                client,
                telemetry,
                metrics_builder,
                endpoint_name,
                endpoints,
                endpoint_request_mapper_factory,
                emitter,
            ),
        )
        .spawn();

        Handle { transactions_tx }
    }

    /// Returns API key validation for the startup endpoint set.
    ///
    /// The forwarder's diagnostics emitter (see [`with_diagnostics_emitter`][Self::with_diagnostics_emitter]) is used to
    /// surface a diagnostic event when validation determines that every configured API key is invalid, allowing
    /// interested subscribers to react to the credentials being rejected.
    pub(crate) fn api_key_validator(&self) -> ApiKeyValidator {
        ApiKeyValidator::new(
            ValidationTargets::startup(
                self.endpoints
                    .iter()
                    .map(|routable| routable.endpoint().clone())
                    .collect(),
            ),
            self.client.clone(),
            self.api_key_refresher.as_ref().map(ApiKeyRefresher::changes),
            self.config.api_key_validation_interval(),
            self.emitter.clone(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_io_loop<B>(
    mut transactions_rx: mpsc::Receiver<Transaction<B>>, context: ComponentContext, config: ForwarderConfiguration,
    secrets: SecretsGate, service: HttpClient, telemetry: ComponentTelemetry, metrics_builder: MetricsBuilder,
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
    let shared_txnq_telemetry = SharedTransactionQueueTelemetry::from_builder(&metrics_builder);

    for routable_endpoint in resolved_endpoints {
        let (route, resolved_endpoint) = routable_endpoint.into_parts();
        let endpoint_url = resolved_endpoint.endpoint().to_string();
        let endpoint_domain = resolved_endpoint.endpoint().origin().ascii_serialization();
        let policy_endpoint = config.metrics_policy_endpoint(route, &resolved_endpoint).to_string();
        let queue_id = generate_retry_queue_id(context.clone(), &resolved_endpoint);

        // Keyed by the queue, which two keys on one URL do not share.
        let txnq_telemetry = TransactionQueueTelemetry::from_builder(
            &metrics_builder,
            &endpoint_url,
            &queue_id,
            shared_txnq_telemetry.clone(),
        );
        let retry_telemetry = TransactionRetryTelemetry::from_builder(&metrics_builder, &endpoint_domain);
        let retry_cause_telemetry = RetryCauseTelemetry::from_builder(&metrics_builder, &endpoint_domain);

        let (endpoint_tx, endpoint_rx) = mpsc::channel(8);
        let task_name = endpoint_io_loop_task_name(&resolved_endpoint);
        let settings = LoopSettings {
            limits: LoopLimits::from_configuration(&config, route, resolved_endpoint.configured_endpoint()),
            client: SharedClient::new(service.clone()),
        };
        let endpoint_loop = EndpointLoop {
            telemetry: telemetry.clone(),
            queue_telemetry: txnq_telemetry,
            retry_telemetry,
            retry_cause_telemetry,
            endpoint_name: Arc::clone(&endpoint_name),
            route,
            endpoint: resolved_endpoint,
            request_mapper: endpoint_request_mapper_factory.clone(),
            secrets: secrets.clone(),
            emitter: emitter.clone(),
            queue_id,
            queue: None,
            settings,
        };

        runtime::worker(task_name, async move {
            // Nothing controls a native forwarder's endpoint loop, so nothing waits to hear how it exited.
            let _ = run_endpoint_io_loop(endpoint_rx, endpoint_loop, None).await;
        })
        .spawn();

        endpoint_txs.push(EndpointSender {
            endpoint_url,
            endpoint_domain,
            policy_endpoint,
            route,
            tx: endpoint_tx,
        });
    }

    // Listen for transactions to forward, and send a copy of each one to the matching endpoint I/O tasks.
    while let Some(transaction) = transactions_rx.recv().await {
        let is_metrics_request = is_metrics_request_uri(transaction.request_uri(), METRICS_SERIES_V3_PATH);
        for endpoint_sender in &endpoint_txs {
            if !should_route_to_endpoint(is_metrics_request, has_metrics_primary, endpoint_sender.route) {
                continue;
            }
            if !matches_metrics_endpoint_routing(&endpoint_sender.policy_endpoint, transaction.metadata()) {
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

    debug!("Requests channel for main I/O task complete. Stopping endpoint I/O tasks.");

    // Drop our endpoint I/O task channels, which will cause them to shut down once they've processed all outstanding
    // requests in their respective channel. We don't wait for that here: the endpoint tasks are our siblings under the
    // component's supervisor, and it waits for every child, so the drain stays ordered without this task tracking it.
    drop(endpoint_txs);
}

struct EndpointSender<B>
where
    B: Buf + Clone,
{
    endpoint_url: String,
    endpoint_domain: String,
    policy_endpoint: String,
    route: EndpointRoute,
    tx: mpsc::Sender<Transaction<B>>,
}

fn is_metrics_request_uri(uri: &Uri, v3_beta_series_route: &str) -> bool {
    METRIC_INTAKE_PATHS.contains(&uri.path()) || uri.path() == v3_beta_series_route
}

/// Returns whether a request goes to an endpoint of the given route: metrics requests go to the alternate metrics
/// intake in place of the primary endpoint when there is one, and other requests never go to it.
pub(crate) fn should_route_to_endpoint(
    is_metrics_request: bool, has_metrics_primary: bool, route: EndpointRoute,
) -> bool {
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

fn matches_metrics_endpoint_routing(configured_endpoint: &str, metadata: &Metadata) -> bool {
    metadata
        .metrics_endpoint_routing
        .as_ref()
        .is_none_or(|routing| routing.should_route_to(configured_endpoint))
}

/// Returns the process name for the endpoint I/O loop serving `endpoint`.
///
/// Two things shape the name beyond the endpoint itself.
///
/// The host comes from the *configured* endpoint rather than the resolved one. The resolved URL carries the data plane
/// version as a host prefix, so naming the task from it would rename the task on every release -- and a process name
/// is a dimension of the per-task runtime telemetry, which makes it worth keeping stable. This is also why the name
/// doesn't line up with the `domain` label on the forwarder's own metrics, which is version-prefixed.
///
/// The host is then folded into a single segment with [`get_sanitized_name`] rather than interpolated directly. A
/// period separates segments of a process name, so `app.datadoghq.com` interpolated as-is would spread one task across
/// three levels of the process tree instead of naming it.
fn endpoint_io_loop_task_name(endpoint: &ResolvedEndpoint) -> String {
    // An additional endpoint is configured as a bare host rather than a URL, which is why this falls back to the
    // configured string as a whole instead of requiring it to parse.
    let configured = endpoint.configured_endpoint();
    let host = Url::parse(configured)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| configured.to_owned());

    format!("endpoint_io_loop_{}", get_sanitized_name(&host))
}

/// The pieces of one recipient's I/O loop: everything it runs with besides its specification.
///
/// A delivery host's [`RecipientFactory`][super::delivery::RecipientFactory] builds them off the data path, and
/// [`RecipientLoop::spawn`] runs the loop with them.
pub struct RecipientPieces<B> {
    /// The component the loop belongs to: its telemetry and its queue directory are named after it.
    pub(crate) context: ComponentContext,

    /// The forwarder settings the loop starts with; publications change them while it runs
    /// ([`RecipientSettingsSender`]).
    pub(crate) config: ForwarderConfiguration,

    /// The gate that decides whether a rejected API key is worth retrying.
    pub(crate) secrets: SecretsGate,

    /// The HTTP client the loop sends through.
    pub(crate) client: HttpClient,

    /// The component's telemetry.
    pub(crate) telemetry: ComponentTelemetry,

    /// The telemetry of the loop's queue.
    pub(crate) queue_telemetry: TransactionQueueTelemetry,

    /// The telemetry of the loop's retries.
    pub(crate) retry_telemetry: TransactionRetryTelemetry,

    /// The telemetry of why the loop retries.
    pub(crate) retry_cause_telemetry: RetryCauseTelemetry,

    /// Names the endpoint of a request for the client's telemetry.
    pub(crate) endpoint_name: Arc<EndpointNameFn>,

    /// How the forwarder came to send to the endpoint.
    pub(crate) route: EndpointRoute,

    /// The endpoint, with its credential cell.
    pub(crate) endpoint: ResolvedEndpoint,

    /// Builds the request mapper of the endpoint.
    pub(crate) request_mapper: EndpointRequestMapperFactory<B>,

    /// Where the loop reports diagnostic events.
    pub(crate) emitter: DiagnosticsEmitter,

    /// The recipient's retry queue, opened on its directory by [`open_queue`][Self::open_queue], or `None` for the
    /// loop to open when it starts.
    pub(crate) queue: Option<RetryQueue<Transaction<B>>>,
}

impl<B> RecipientPieces<B>
where
    B: Buf + Clone,
{
    /// Returns the name of the recipient's queue directory under its host's retry storage.
    ///
    /// The name is a function of the recipient's identity, so every incarnation of one recipient, and a recipient
    /// that returns after it retired, uses the same directory, and adopts what an earlier one persisted: the native
    /// name for a native recipient, and for a declared one the same form, hashed over the component, the recipient's
    /// canonical endpoint, and its credential's name.
    pub fn queue_directory(&self, spec: &RecipientSpec) -> String {
        recipient_queue_id(&self.context, spec, &self.endpoint)
    }

    /// Opens the recipient's retry queue on its directory, so that a directory that cannot be used fails the
    /// recipient's preparation, rather than leaving its loop with a queue in memory only.
    ///
    /// Opening a directory creates it, lists it, and removes its outdated files, though it reads none
    /// ([`RetryQueue::with_disk_persistence`]), so it must happen only while no loop owns the directory: after the
    /// previous incarnation of the same identity exited. Pieces whose queue is not opened here open it when the loop
    /// starts, and fall back to a queue in memory if the directory cannot be used, as a native endpoint loop does.
    ///
    /// # Errors
    ///
    /// If the recipient spills to disk and its directory cannot be created or listed, an error naming the recipient
    /// is returned.
    pub async fn open_queue(&mut self, spec: &RecipientSpec) -> Result<(), GenericError> {
        let limits = LoopLimits::for_recipient(&self.config, spec);
        let queue = open_retry_queue(&limits, self.queue_directory(spec))
            .await
            .with_error_context(|| format!("Failed to open the retry queue of recipient '{}'.", spec.id))?;
        self.queue = Some(queue);

        Ok(())
    }

    /// Returns the name of the recipient's loop task, and what its loop runs with.
    fn into_endpoint_loop(self, spec: &RecipientSpec) -> (String, EndpointLoop<B, HttpClient>) {
        let queue_id = self.queue_directory(spec);
        let Self {
            context: _,
            config,
            secrets,
            client,
            telemetry,
            queue_telemetry,
            retry_telemetry,
            retry_cause_telemetry,
            endpoint_name,
            route,
            endpoint,
            request_mapper,
            emitter,
            queue,
        } = self;
        let name = match &spec.origin {
            RecipientOrigin::Native(_) => endpoint_io_loop_task_name(&endpoint),
            RecipientOrigin::Declared { name, .. } => format!("recipient_io_loop_{}", get_sanitized_name(name)),
        };
        let endpoint_loop = EndpointLoop {
            telemetry,
            queue_telemetry,
            retry_telemetry,
            retry_cause_telemetry,
            endpoint_name,
            route,
            endpoint,
            request_mapper,
            secrets,
            emitter,
            queue_id,
            queue,
            settings: LoopSettings {
                limits: LoopLimits::for_recipient(&config, spec),
                client: SharedClient::new(client),
            },
        };

        (name, endpoint_loop)
    }
}

/// What a delivery host holds of a running recipient I/O loop.
pub struct RecipientHandle<B>
where
    B: Buf + Clone,
{
    /// Sends transactions to the loop.
    ///
    /// Once every sender is dropped, a running loop closes as at process shutdown; a retiring loop keeps draining, and
    /// can no longer be reopened. A host that may reopen the recipient keeps its sender until the loop exits.
    pub sender: mpsc::Sender<Transaction<B>>,

    /// Asks the loop to retire or to close, and reopens it after a release.
    pub close: RecipientCloser<B>,

    /// Publishes new settings to the loop.
    pub settings: RecipientSettingsSender,

    /// Resolves with how the loop exited, once it did. Every loop resolves it exactly once.
    pub exited: oneshot::Receiver<RecipientExit>,
}

impl<B> RecipientHandle<B>
where
    B: Buf + Clone,
{
    /// Returns a handle for an I/O loop that is not a [`RecipientLoop`], such as a test's double of one, and the ends
    /// of it that such a loop holds, with room for `capacity` transactions in its input.
    ///
    /// Settings published to the handle reach no loop.
    pub fn detached(capacity: usize) -> (Self, DetachedLoop<B>) {
        let (sender, transactions) = mpsc::channel(capacity);
        let (close, close_requests) = close_channel(&sender);
        let (exited_tx, exited) = oneshot::channel();
        let handle = Self {
            sender,
            close,
            settings: RecipientSettingsSender { settings: None },
            exited,
        };
        let detached = DetachedLoop {
            transactions,
            close: close_requests,
            exit: exited_tx,
        };

        (handle, detached)
    }
}

#[cfg(test)]
impl<B> RecipientHandle<B>
where
    B: Buf + Clone,
{
    /// Returns a detached handle, as [`detached`][Self::detached] does, whose published settings a test can read: the
    /// settings start as a loop built from `pieces` for `spec` would start.
    pub(crate) fn detached_with_settings(
        spec: &RecipientSpec, pieces: &RecipientPieces<B>, capacity: usize,
    ) -> (Self, DetachedLoop<B>, PublishedSettings) {
        let (mut handle, detached) = Self::detached(capacity);
        let (settings, published) = watch::channel(LoopSettings {
            limits: LoopLimits::for_recipient(&pieces.config, spec),
            client: SharedClient::new(pieces.client.clone()),
        });
        handle.settings = RecipientSettingsSender {
            settings: Some(settings),
        };

        (handle, detached, PublishedSettings(published))
    }
}

/// What a test reads of the settings published to a detached handle ([`RecipientHandle::detached_with_settings`]).
#[cfg(test)]
pub(crate) struct PublishedSettings(watch::Receiver<LoopSettings<HttpClient>>);

#[cfg(test)]
impl PublishedSettings {
    /// Returns the concurrency limit, the high-priority queue's bound, and the retry queue's bound in bytes that the
    /// loop runs with now.
    pub(crate) fn limits(&self) -> (usize, usize, u64) {
        let settings = self.0.borrow();
        (
            settings.limits.concurrency,
            settings.limits.buffer_size,
            settings.limits.queue_bytes(),
        )
    }

    /// Returns whether settings were published since the last call that saw them.
    pub(crate) fn changed(&mut self) -> bool {
        let changed = self.0.has_changed().unwrap_or(false);
        self.0.mark_unchanged();
        changed
    }
}

/// The ends of a [`RecipientHandle`] that its I/O loop holds, for a loop that is not a [`RecipientLoop`].
pub struct DetachedLoop<B> {
    /// Receives the transactions sent to the handle.
    pub transactions: mpsc::Receiver<Transaction<B>>,

    /// Follows the close requests made through the handle.
    pub close: CloseRequests,

    /// Reports the loop's exit on the handle.
    pub exit: oneshot::Sender<RecipientExit>,
}

/// What a host asked a recipient I/O loop about closing, which the loop and its host share under one lock, so that a
/// reopen and the loop's decision to close for good never cross.
#[derive(Clone, Copy, Debug)]
struct CloseState {
    /// The latest request.
    request: CloseRequest,

    /// How many retirements a reopen cancelled. A loop that finds the count changed applies a reopen before the
    /// request, so that it applies both a reopen and a request that follows it before the loop looked.
    reopens: u64,

    /// Whether the loop's close became final, after which a reopen is refused.
    sealed: bool,
}

/// Returns the two ends of a recipient I/O loop's close requests: the host's, which follows whether the loop's input
/// is still open, and the loop's.
fn close_channel<B>(input: &mpsc::Sender<Transaction<B>>) -> (RecipientCloser<B>, CloseRequests) {
    let (state, changes) = watch::channel(CloseState {
        request: CloseRequest::None,
        reopens: 0,
        sealed: false,
    });
    let closer = RecipientCloser {
        state: state.clone(),
        input: input.downgrade(),
    };
    let requests = CloseRequests {
        state,
        changes,
        reopens_seen: 0,
    };

    (closer, requests)
}

/// Asks a recipient's I/O loop to retire or to close as at process shutdown, and reopens it after a release.
///
/// Dropping the closer closes the loop as at process shutdown, so that a loop whose host has gone persists or drops
/// its queued work at once, rather than draining to its deadline.
pub struct RecipientCloser<B> {
    state: watch::Sender<CloseState>,
    input: mpsc::WeakSender<Transaction<B>>,
}

impl<B> RecipientCloser<B> {
    /// Asks the loop to retire: it takes nothing more from its input, after queueing what is in it already, and drains
    /// its queued work until `deadline`. A loop that retires already takes the new deadline. A loop that closes as at
    /// shutdown, or whose close is final, keeps closing.
    pub fn retire(&self, deadline: tokio::time::Instant) {
        self.request(CloseRequest::Retire { deadline });
    }

    /// Asks the loop to close as at process shutdown, which nothing cancels.
    pub fn shutdown(&self) {
        self.request(CloseRequest::Shutdown);
    }

    fn request(&self, request: CloseRequest) {
        self.state.send_if_modified(|state| {
            let closes = state.sealed || state.request == CloseRequest::Shutdown;
            if !closes {
                state.request = request;
            }
            !closes
        });
    }

    /// Cancels the loop's retirement, as for a recipient declared again while it drains: the loop takes transactions
    /// again, with the same queue, and a later retirement reports only its own drain.
    ///
    /// Returns [`ReopenReply::Reopened`] when the loop continues, which a loop that was not retiring does too, and
    /// [`ReopenReply::Exited`] when it cannot: its close is final (it began its final flush, it closes as at shutdown,
    /// or it failed), its task stopped, or every sender of its input was dropped. A loop answered so reports its exit
    /// on its handle, as every loop does.
    pub fn reopen(&self) -> ReopenReply {
        // The loop's end holds the only receiver of the close state, so a loop whose task stopped has none.
        if self.input.strong_count() == 0 || self.state.receiver_count() == 0 {
            return ReopenReply::Exited;
        }

        let mut reply = ReopenReply::Exited;
        self.state.send_if_modified(|state| {
            if state.sealed {
                return false;
            }
            match state.request {
                CloseRequest::None => {
                    reply = ReopenReply::Reopened;
                    false
                }
                CloseRequest::Retire { .. } => {
                    state.request = CloseRequest::None;
                    state.reopens += 1;
                    reply = ReopenReply::Reopened;
                    true
                }
                CloseRequest::Shutdown => false,
            }
        });

        reply
    }

    /// Returns the close request the loop follows.
    pub fn current(&self) -> CloseRequest {
        self.state.borrow().request
    }
}

impl<B> Drop for RecipientCloser<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A recipient I/O loop's end of its close requests.
pub struct CloseRequests {
    state: watch::Sender<CloseState>,
    changes: watch::Receiver<CloseState>,
    reopens_seen: u64,
}

/// A change of close request as a loop applies it: whether a reopen came since the loop last looked, then the latest
/// request.
#[derive(Clone, Copy, Debug)]
struct CloseChange {
    reopened: bool,
    request: CloseRequest,
}

impl CloseRequests {
    /// Returns the close request the loop follows now.
    pub fn current(&self) -> CloseRequest {
        self.changes.borrow().request
    }

    /// Waits until the close request is one that `wanted` accepts, and returns it.
    pub async fn wait_for(&mut self, mut wanted: impl FnMut(CloseRequest) -> bool) -> CloseRequest {
        loop {
            let change = self.take();
            if wanted(change.request) {
                return change.request;
            }
            if self.changes.changed().await.is_err() {
                // The loop's end holds a sender of its own, so the watch never closes.
                return change.request;
            }
        }
    }

    /// Returns the change since the loop last looked, and marks it seen.
    fn take(&mut self) -> CloseChange {
        let state = *self.changes.borrow_and_update();
        let reopened = state.reopens != self.reopens_seen;
        self.reopens_seen = state.reopens;

        CloseChange {
            reopened,
            request: state.request,
        }
    }

    /// Waits for the next change, and returns it.
    async fn changed(&mut self) -> CloseChange {
        // The loop's end holds a sender of its own, so the watch never closes: the host's end going is a shutdown
        // request, which its closer makes as it drops.
        if self.changes.changed().await.is_err() {
            return CloseChange {
                reopened: false,
                request: CloseRequest::Shutdown,
            };
        }

        self.take()
    }

    /// Makes the loop's close final, so that a reopen is refused from now on, unless the close ends a retirement
    /// (`reopenable`) and a reopen the loop has not applied yet cancelled it, in which case that change is returned for
    /// the loop to apply.
    fn seal(&mut self, reopenable: bool) -> Result<(), CloseChange> {
        let reopens_seen = self.reopens_seen;
        let mut reopened = false;
        self.state.send_if_modified(|state| {
            if reopenable && state.reopens != reopens_seen {
                reopened = true;
            } else {
                state.sealed = true;
            }
            // Only the host reads whether the close is final, and it does so under the lock.
            false
        });

        if reopened {
            Err(self.take())
        } else {
            Ok(())
        }
    }
}

/// Publishes new settings to a running recipient I/O loop, which applies them between requests.
pub struct RecipientSettingsSender {
    /// The loop's settings, or `None` for a handle that no loop follows.
    settings: Option<watch::Sender<LoopSettings<HttpClient>>>,
}

impl RecipientSettingsSender {
    /// Publishes the host's settings and the recipient's declaration, as a resource transaction's publication does.
    ///
    /// The loop applies them before its next request, without building its service again, so its circuit breaker
    /// keeps the backoff it is in, and its retry policy its count of errors: its concurrency (a lower limit applies as
    /// requests in flight finish: the loop sends nothing more until fewer than the new limit are in flight), its retry
    /// policy, its high-priority and retry queue bounds (a smaller bound moves the newest high-priority work to the
    /// retry queue, and evicts the retry queue's oldest, counted as dropped where it is not persisted), its disk limits
    /// between nonzero sizes, the metrics variants it takes in their V3 form, and its delivery policy. Requests in
    /// flight finish as they started.
    ///
    /// Two settings apply only when a queue is opened, at the recipient's next incarnation: turning disk persistence on
    /// or off, which is not applied, with a warning, and the age past which persisted files are removed
    /// (`outdated_file_in_days`), since outdated files are removed only when a queue is opened. A publication that
    /// changes nothing does not wake the loop.
    pub fn publish(&self, settings: &DeliverySettings, spec: &RecipientSpec) {
        if let Some(loop_settings) = &self.settings {
            loop_settings.send_if_modified(|current| {
                let limits = current.limits.published(settings, spec);
                let changed = limits != current.limits;
                current.limits = limits;
                changed
            });
        }
    }

    /// Swaps the client the loop sends through, as when its host rebuilt the client for a new client profile or proxy
    /// credential. The loop sends each request after this one through the new client; requests in flight finish on the
    /// old one.
    #[cfg(test)]
    pub(crate) fn swap_client(&self, client: HttpClient) {
        if let Some(loop_settings) = &self.settings {
            loop_settings.send_modify(|current| current.client.set(client));
        }
    }
}

/// The I/O loop of one recipient: one endpoint, its retry queue, and its persisted directory.
///
/// The loop runs as a native endpoint I/O loop does, and follows its handle's close requests
/// ([`RecipientCloser`]):
///
/// - A retirement stops it taking transactions, after queueing those already in its input, and it keeps dispatching
///   its queued work, persisted entries included, until the queue and its requests in flight are empty. At the
///   deadline it closes as at process shutdown. A recipient whose delivery policy closes with
///   [`RetireClosure::Persist`] closes so at once, and one with [`RetireClosure::Discard`] drops its queued work in
///   memory and counts it; entries already persisted stay on disk.
/// - A reopen cancels a retirement until the loop's close becomes final, as its final flush begins: the loop takes
///   transactions again, with the same queue, its deadline and its closure cancelled. After that, a reopen is refused.
/// - A shutdown request, the closer dropping, or its input closing while it runs, closes it as at process shutdown:
///   it dispatches nothing more, finishes its requests in flight, and persists its queued work where disk storage is
///   configured, or drops and counts it. Nothing cancels such a close.
///
/// As its close becomes final, the loop closes its input, so that a later send fails rather than being lost
/// uncounted, and queues what the input still holds, which its close persists or drops with the rest.
///
/// Its exit is reported once, on the handle: [`RecipientExit::Retired`] with what it delivered while retiring and
/// what its close persisted and dropped, both after a retirement and after a close as at shutdown, or
/// [`RecipientExit::Failed`] when it ended on an error while running, after its native close. An error while retiring
/// ends the drain as its deadline would. A loop whose task panics or is aborted reports a failure that says so.
pub struct RecipientLoop {
    name: String,
    task: BoxFuture<'static, ()>,
}

impl RecipientLoop {
    /// Prepares the I/O loop of the recipient `spec` describes, with its pieces, and returns it with its handle, for
    /// the caller to run where it chooses.
    pub fn prepare<B>(spec: &RecipientSpec, pieces: RecipientPieces<B>) -> (Self, RecipientHandle<B>)
    where
        B: Body + Buf + Clone + Send + Sync + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (name, endpoint_loop) = pieces.into_endpoint_loop(spec);

        let (sender, transactions) = mpsc::channel(8);
        let (close, close_requests) = close_channel(&sender);
        let (settings_tx, settings_rx) = watch::channel(endpoint_loop.settings.clone());
        let (exited_tx, exited) = oneshot::channel();
        let reporter = ExitReporter(Some(exited_tx));
        let control = LoopControl {
            close: close_requests,
            settings: settings_rx,
        };
        let task = async move {
            let exit = run_endpoint_io_loop(transactions, endpoint_loop, Some(control)).await;
            reporter.report(exit);
        }
        .boxed();

        let handle = RecipientHandle {
            sender,
            close,
            settings: RecipientSettingsSender {
                settings: Some(settings_tx),
            },
            exited,
        };
        (Self { name, task }, handle)
    }

    /// Spawns the I/O loop of the recipient `spec` describes, with its pieces, as a task on the ambient supervisor, and
    /// returns its handle.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a supervision tree.
    pub fn spawn<B>(spec: &RecipientSpec, pieces: RecipientPieces<B>) -> RecipientHandle<B>
    where
        B: Body + Buf + Clone + Send + Sync + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (recipient_loop, handle) = Self::prepare(spec, pieces);
        let Self { name, task } = recipient_loop;
        runtime::worker(name, task).spawn();

        handle
    }

    /// Returns the name of the loop's task: `endpoint_io_loop_<host>` for a native recipient, and
    /// `recipient_io_loop_<declared name>` for a declared one.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the loop's task, which runs until the loop exits and reports its exit on the handle.
    pub fn into_task(self) -> BoxFuture<'static, ()> {
        self.task
    }
}

/// The exit a recipient I/O loop reports when it stops before it finished, as when its task panics or is aborted.
pub(crate) const INTERRUPTED_EXIT: &str =
    "the recipient's I/O loop stopped before it finished, as when its task panics or is aborted at shutdown";

/// Reports a recipient I/O loop's exit exactly once: with its outcome, or, if the loop stops before it has one, as a
/// failure that says so.
struct ExitReporter(Option<oneshot::Sender<RecipientExit>>);

impl ExitReporter {
    fn report(mut self, exit: RecipientExit) {
        if let Some(exited) = self.0.take() {
            // The host may have stopped waiting, as when it dropped an aborted preparation.
            let _ = exited.send(exit);
        }
    }
}

impl Drop for ExitReporter {
    fn drop(&mut self) {
        if let Some(exited) = self.0.take() {
            let _ = exited.send(RecipientExit::Failed(INTERRUPTED_EXIT.to_string()));
        }
    }
}

/// The settings of an endpoint I/O loop that publications change while it runs, which it applies between requests.
#[derive(Clone, Debug, PartialEq)]
struct LoopLimits {
    /// The most concurrent requests.
    concurrency: usize,

    /// The most transactions the high-priority queue holds.
    buffer_size: usize,

    /// The retry policy, and the retry queue's bounds.
    retry: RetryConfiguration,

    /// The metrics variants the endpoint takes in their V3 form.
    v3: V3Acceptance,

    /// Whether every request carries `Allow-Arbitrary-Tag-Value: true`.
    allow_arbitrary_tags: bool,

    /// The recipient's delivery policy.
    policy: RecipientPolicy,
}

impl LoopLimits {
    /// Returns the settings of a native forwarder's endpoint loop.
    fn from_configuration(config: &ForwarderConfiguration, route: EndpointRoute, configured_endpoint: &str) -> Self {
        let v3 = config.endpoint_v3_settings(route, configured_endpoint);
        Self {
            concurrency: config.endpoint_concurrency(),
            buffer_size: config.endpoint_buffer_size(),
            retry: config.retry().clone(),
            v3: V3Acceptance {
                series: v3.use_v3_series,
                sketches: v3.use_v3_sketches,
            },
            allow_arbitrary_tags: config.allow_arbitrary_tags(),
            policy: RecipientPolicy::default(),
        }
    }

    /// Returns the settings a recipient's loop starts with: its host's, as configured, and its own declaration's.
    fn for_recipient(config: &ForwarderConfiguration, spec: &RecipientSpec) -> Self {
        Self {
            concurrency: config.endpoint_concurrency(),
            buffer_size: config.endpoint_buffer_size(),
            retry: config.retry().clone(),
            v3: spec.protocol.v3(),
            allow_arbitrary_tags: allow_arbitrary_tags(spec, config.allow_arbitrary_tags()),
            policy: spec.policy,
        }
    }

    /// Returns these settings with what a publication says: the host's settings and the recipient's declaration.
    fn published(&self, settings: &DeliverySettings, spec: &RecipientSpec) -> Self {
        Self {
            concurrency: effective_concurrency(settings.endpoint_concurrency, settings.endpoint_concurrency_multiplier),
            buffer_size: settings.endpoint_buffer_size,
            retry: self.retry.with_settings(&settings.retry),
            v3: spec.protocol.v3(),
            allow_arbitrary_tags: allow_arbitrary_tags(spec, self.allow_arbitrary_tags),
            policy: spec.policy,
        }
    }

    /// Returns the bound of the retry queue in memory: the recipient's own when it declares one, within its host's.
    fn queue_bytes(&self) -> u64 {
        let host = self.retry.queue_max_size_bytes();
        self.policy.queue_bytes.map_or(host, |bytes| bytes.min(host))
    }

    /// Returns whether the retry queue spills to disk: where disk storage is configured, unless the recipient's policy
    /// says never.
    fn spills_to_disk(&self) -> bool {
        self.retry.storage_max_size_bytes() > 0 && self.policy.spill == SpillPolicy::Native
    }
}

/// Returns whether a recipient's requests carry `Allow-Arbitrary-Tag-Value: true`: as its Datadog intake protocol
/// says, and otherwise as `default` says.
fn allow_arbitrary_tags(spec: &RecipientSpec, default: bool) -> bool {
    match &spec.protocol {
        RecipientProtocol::DatadogIntake {
            allow_arbitrary_tags, ..
        } => *allow_arbitrary_tags,
        RecipientProtocol::ClusterAgent => default,
    }
}

/// Returns the most concurrent requests of one endpoint, as [`ForwarderConfiguration::endpoint_concurrency`] computes
/// it: the concurrency times its multiplier, each at least one.
const fn effective_concurrency(concurrency: usize, multiplier: usize) -> usize {
    let concurrency = if concurrency == 0 { 1 } else { concurrency };
    let multiplier = if multiplier == 0 { 1 } else { multiplier };

    concurrency.saturating_mul(multiplier)
}

/// What an endpoint I/O loop sends with, and the settings it applies between requests.
#[derive(Clone)]
struct LoopSettings<C> {
    limits: LoopLimits,
    client: SharedClient<C>,
}

/// A client in an endpoint I/O loop's settings, which its host and the loop share.
///
/// A watch shares its value between threads, which a client cannot be, so the settings hold the client behind a lock,
/// with a version that every client set here takes, so that the loop's service takes the new one at its next request.
struct SharedClient<C>(Arc<Mutex<(u64, C)>>);

impl<C: Clone> SharedClient<C> {
    fn new(client: C) -> Self {
        Self(Arc::new(Mutex::new((0, client))))
    }

    /// Returns the client, with its version.
    fn get(&self) -> (u64, C) {
        let shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (shared.0, shared.1.clone())
    }

    /// Returns the client, with its version, if it is newer than `version`.
    fn newer_than(&self, version: u64) -> Option<(u64, C)> {
        let shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (shared.0 != version).then(|| (shared.0, shared.1.clone()))
    }

    /// Sets the client, which the loop's service takes at its next request. A delivery host switches its clients
    /// through a switchable client instead ([`HttpClient::switchable`]), which reaches every loop and its validator
    /// at once; tests drive the loop's own cell.
    #[cfg(test)]
    fn set(&self, client: C) {
        let mut shared = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *shared = (shared.0.wrapping_add(1), client);
    }
}

impl<C> Clone for SharedClient<C> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// What a delivery host controls a recipient's loop through.
struct LoopControl<C> {
    close: CloseRequests,
    settings: watch::Receiver<LoopSettings<C>>,
}

/// What an endpoint I/O loop runs with, besides its input and its control: the pieces of a recipient, or those a
/// native forwarder builds for one endpoint.
struct EndpointLoop<B, C> {
    telemetry: ComponentTelemetry,
    queue_telemetry: TransactionQueueTelemetry,
    retry_telemetry: TransactionRetryTelemetry,
    retry_cause_telemetry: RetryCauseTelemetry,
    endpoint_name: Arc<EndpointNameFn>,
    route: EndpointRoute,
    endpoint: ResolvedEndpoint,
    request_mapper: EndpointRequestMapperFactory<B>,
    secrets: SecretsGate,
    emitter: DiagnosticsEmitter,
    queue_id: String,
    queue: Option<RetryQueue<Transaction<B>>>,
    settings: LoopSettings<C>,
}

/// Where an endpoint I/O loop is in its life, which decides what it takes, what it dispatches, and when it exits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Taking transactions from its input, and dispatching queued work.
    Running,

    /// Retiring: dispatching queued work, and taking nothing more from its input, until the queue and the requests in
    /// flight are empty, or the deadline passes.
    Retiring { deadline: tokio::time::Instant },

    /// Closing as at process shutdown: dispatching nothing more, and waiting for the requests in flight, after which
    /// queued work is persisted where disk storage is configured, and otherwise dropped.
    ///
    /// A close that ends a retirement (its deadline, its closure, or an error while it drained) is `reopenable`: a
    /// reopen cancels it until the loop's close becomes final. Any other close is final already.
    Closing { reopenable: bool },
}

impl Phase {
    /// Returns whether a reopen cancels this phase: a retirement's, until the loop's close becomes final.
    const fn is_reopenable(self) -> bool {
        matches!(self, Self::Retiring { .. } | Self::Closing { reopenable: true })
    }

    const fn is_closing(self) -> bool {
        matches!(self, Self::Closing { .. })
    }
}

/// What an endpoint I/O loop's retirement did, for its drain report.
#[derive(Default)]
struct Retirement {
    /// Whether a release asked the loop to retire, and no reopen cancelled it since.
    requested: bool,

    /// Requests delivered since the retirement began.
    delivered: u64,

    /// Whether the deadline passed before the queue was empty.
    deadline_reached: bool,

    /// Whether queued work in memory is dropped at the close, rather than persisted.
    discard: bool,
}

/// Applies a change of close request to an endpoint I/O loop's phase: a reopen first, when one came, then the request.
///
/// A loop the change leaves not running takes nothing more from its input, so the caller queues what is in it already.
fn apply_close_change(
    phase: &mut Phase, retirement: &mut Retirement, change: CloseChange, on_retire: RetireClosure, input_open: bool,
) {
    // A reopen the loop sees only as its request, with no count of it, is applied the same way.
    if phase.is_reopenable() && (change.reopened || change.request == CloseRequest::None) {
        // The loop takes transactions again, and a later retirement reports only its own drain. Without an input to
        // take them from, it closes as at shutdown, as a running loop whose input closes does.
        *phase = if input_open {
            Phase::Running
        } else {
            Phase::Closing { reopenable: false }
        };
        *retirement = Retirement::default();
    }

    match (*phase, change.request) {
        // A final close cannot be cancelled or changed.
        (Phase::Closing { reopenable: false }, _) => {}
        (_, CloseRequest::Shutdown) => *phase = Phase::Closing { reopenable: false },
        // Nothing to do, or a reopen applied above.
        (_, CloseRequest::None) => {}
        (Phase::Running, CloseRequest::Retire { deadline }) => {
            *retirement = Retirement {
                requested: true,
                ..Retirement::default()
            };
            *phase = match on_retire {
                RetireClosure::Native | RetireClosure::Attempt => Phase::Retiring { deadline },
                RetireClosure::Persist => Phase::Closing { reopenable: true },
                RetireClosure::Discard => {
                    retirement.discard = true;
                    Phase::Closing { reopenable: true }
                }
            };
        }
        // An abort that returns a reopened recipient to draining sends its original deadline again.
        (Phase::Retiring { .. }, CloseRequest::Retire { deadline }) => *phase = Phase::Retiring { deadline },
        // A retirement that closes keeps closing: its deadline, or its closure, stands until a reopen cancels it.
        (Phase::Closing { reopenable: true }, CloseRequest::Retire { .. }) => {}
    }
}

/// Waits for the next value of a watch, and returns it; never returns once the watch is gone, or when there is none.
async fn next_change<T: Clone>(receiver: &mut Option<watch::Receiver<T>>) -> T {
    if let Some(changes) = receiver.as_mut() {
        if changes.changed().await.is_ok() {
            return changes.borrow_and_update().clone();
        }
        *receiver = None;
    }

    future::pending().await
}

/// Waits for the next change of close request, or forever when nothing controls the loop.
async fn next_close_change(requests: &mut Option<CloseRequests>) -> CloseChange {
    match requests.as_mut() {
        Some(requests) => requests.changed().await,
        None => future::pending().await,
    }
}

/// Waits until `deadline`, or forever when there is none.
async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => future::pending().await,
    }
}

/// The permits an endpoint I/O loop's requests in flight hold.
///
/// Every request the loop sends takes its permit from the one semaphore, which a publication resizes in place, so the
/// loop never has more in flight than its limit allows. A lower limit takes effect as requests in flight finish: the
/// loop sends nothing more until the permits the limit owes are removed.
struct ConcurrencyBudget {
    semaphore: Arc<Semaphore>,
    limit: usize,

    /// Permits still to remove, as requests in flight return them, since the limit was lowered below what was in use.
    owed: usize,
}

impl ConcurrencyBudget {
    fn new(limit: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limit)),
            limit,
            owed: 0,
        }
    }

    /// Changes the limit: a higher one adds permits at once, and a lower one removes them as they are free.
    fn resize(&mut self, limit: usize) {
        if limit >= self.limit {
            let added = limit - self.limit;
            let repaid = added.min(self.owed);
            self.owed -= repaid;
            self.semaphore.add_permits(added - repaid);
        } else {
            self.owed += self.limit - limit;
        }
        self.limit = limit;
        self.settle();
    }

    /// Removes the free permits the limit still owes.
    fn settle(&mut self) {
        if self.owed > 0 {
            self.owed -= self.semaphore.forget_permits(self.owed);
        }
    }

    /// Returns whether the limit owes no permit, so that the loop may send another request.
    ///
    /// While it owes some, the loop must not wait for a permit: a waiter would take each permit a finishing request
    /// returns before the limit could remove it, and the loop would stay above its limit for as long as work is queued.
    const fn is_settled(&self) -> bool {
        self.owed == 0
    }
}

/// A retry policy that a publication changes in place, shared with the circuit breaker that holds it, so that the
/// breaker keeps the backoff it is in, and the policy its count of errors.
struct SharedPolicy<P>(Arc<Mutex<P>>);

impl<P> SharedPolicy<P> {
    fn new(policy: P) -> Self {
        Self(Arc::new(Mutex::new(policy)))
    }

    fn lock(&self) -> MutexGuard<'_, P> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<P> Clone for SharedPolicy<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<P, Req, Res, E> Policy<Req, Res, E> for SharedPolicy<P>
where
    P: Policy<Req, Res, E>,
{
    type Future = P::Future;

    fn retry(&mut self, request: &mut Req, result: &mut Result<Res, E>) -> Option<Self::Future> {
        self.lock().retry(request, result)
    }

    fn clone_request(&mut self, request: &Req) -> Option<Req> {
        self.lock().clone_request(request)
    }
}

/// The innermost service of an endpoint I/O loop: it sends each request through the client the loop's settings hold
/// when the request is readied, so that a swapped client takes over between requests without the service, and the
/// circuit breaker in it, being built again.
struct FollowingClient<C> {
    shared: SharedClient<C>,
    version: u64,
    current: C,
}

impl<C: Clone> FollowingClient<C> {
    fn new(shared: SharedClient<C>) -> Self {
        let (version, current) = shared.get();

        Self {
            shared,
            version,
            current,
        }
    }
}

impl<C, R> Service<R> for FollowingClient<C>
where
    C: Service<R> + Clone,
{
    type Response = C::Response;
    type Error = C::Error;
    type Future = C::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Take a swapped client only here, so that a request is sent through the client that was readied for it.
        if let Some((version, client)) = self.shared.newer_than(self.version) {
            self.version = version;
            self.current = client;
        }

        self.current.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        self.current.call(request)
    }
}

/// What an endpoint I/O loop builds its service from, once, and the settings in it that a publication changes in
/// place.
struct ServiceParts<B> {
    emitter: DiagnosticsEmitter,
    endpoint_url: String,
    request_mapper: Arc<Mutex<EndpointRequestMapper<B>>>,
    concurrency: ConcurrencyBudget,
    retry_policy: SharedPolicy<SecretsHttpRetryPolicy<Incoming>>,
    allow_arbitrary_tags: Arc<AtomicBool>,
}

impl<B> ServiceParts<B>
where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn new(
        limits: &LoopLimits, secrets: SecretsGate, retry_causes: RetryCauseTelemetry, emitter: DiagnosticsEmitter,
        endpoint_url: String, request_mapper: EndpointRequestMapper<B>,
    ) -> Self {
        Self {
            emitter,
            endpoint_url,
            request_mapper: Arc::new(Mutex::new(request_mapper)),
            concurrency: ConcurrencyBudget::new(limits.concurrency),
            retry_policy: SharedPolicy::new(limits.retry.to_default_http_retry_policy(secrets, retry_causes)),
            allow_arbitrary_tags: Arc::new(AtomicBool::new(limits.allow_arbitrary_tags)),
        }
    }

    /// Applies a publication's settings to the service, in place: its concurrency limit, its retry policy, and whether
    /// its requests allow arbitrary tag values.
    fn apply(&mut self, limits: &LoopLimits) {
        self.concurrency.resize(limits.concurrency);
        limits.retry.update_http_retry_policy(&mut self.retry_policy.lock());
        self.allow_arbitrary_tags
            .store(limits.allow_arbitrary_tags, Ordering::Relaxed);
    }

    /// Builds an endpoint's service, sending through `client`: the request mapper, the version headers, the
    /// concurrency limit, the retry circuit breaker, the inspection of rejected API keys, and the client.
    ///
    /// The service is built once per loop, and follows what [`apply`][Self::apply] changes, so the circuit breaker
    /// keeps its state for as long as the loop runs.
    ///
    /// The body type conversion from `TransactionBody<B>` to `ClientBody` happens as the innermost layer, after the
    /// retry circuit breaker, so that retry and open errors return the original `Request<TransactionBody<B>>`, which is
    /// reassembled into a `Transaction<B>` for re-enqueuing.
    ///
    /// The invalid API key inspection layer sits *below* the retry circuit breaker so that it observes the raw response
    /// from the client. When secrets management is enabled, a 403 is classified as retriable and the circuit breaker
    /// converts it into a `RetryCircuitBreakerError::Retry` error that only re-enqueues the transaction (it never
    /// surfaces the response to the loop), so this layer is the only place that reliably sees the rejection.
    fn build<C>(
        &self, client: SharedClient<C>,
    ) -> impl Service<
        Request<TransactionBody<B>>,
        Response = Response<Incoming>,
        Error = RetryCircuitBreakerError<BoxError, Request<TransactionBody<B>>>,
        Future: Send + 'static,
    > + Send
    where
        C: Service<Request<ClientBody>, Response = Response<Incoming>, Error = BoxError> + Clone + Send + 'static,
        C::Future: Send + 'static,
    {
        ServiceBuilder::new()
            // Set the request's URI and endpoint-specific headers, such as the API key, from the endpoint's current
            // state.
            .map_request({
                let request_mapper = Arc::clone(&self.request_mapper);
                move |request| {
                    let mut mapper = request_mapper.lock().expect("endpoint request mapper mutex poisoned");
                    mapper(request)
                }
            })
            // Signal backend support for arbitrary tag values when configured.
            .map_request({
                let allow_arbitrary_tags = Arc::clone(&self.allow_arbitrary_tags);
                move |request| with_allow_arbitrary_tags(allow_arbitrary_tags.load(Ordering::Relaxed))(request)
            })
            // Set the User-Agent and DD-Agent-Version headers indicating the version of the data plane sending the
            // request.
            .map_request(with_version_info())
            .layer(GlobalConcurrencyLimitLayer::with_semaphore(Arc::clone(
                &self.concurrency.semaphore,
            )))
            .layer(RetryCircuitBreakerLayer::new(self.retry_policy.clone()))
            .layer(build_diagnostics_layer(self.emitter.clone(), self.endpoint_url.clone()))
            .map_request(|req: Request<TransactionBody<B>>| req.map(into_client_body))
            .service(FollowingClient::new(client))
    }
}

/// Opens the retry queue named `queue_id` with the given settings: in memory, within the recipient's bound, and on
/// disk under the storage path when the recipient spills to disk.
async fn open_retry_queue<B>(limits: &LoopLimits, queue_id: String) -> Result<RetryQueue<Transaction<B>>, GenericError>
where
    B: Buf + Clone,
{
    let retry = &limits.retry;
    let queue =
        RetryQueue::new(queue_id, limits.queue_bytes()).with_flush_to_disk_mem_ratio(retry.flush_to_disk_mem_ratio());
    if !limits.spills_to_disk() {
        return Ok(queue);
    }

    queue
        .with_disk_persistence(PersistedQueueArgs {
            root_path: PathBuf::from(retry.storage_path()),
            max_on_disk_bytes: retry.storage_max_size_bytes(),
            storage_max_disk_ratio: retry.storage_max_disk_ratio(),
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(PathBuf::from(retry.storage_path()))),
            max_age_days: retry.outdated_file_in_days(),
        })
        .await
}

/// Applies new settings to an endpoint I/O loop's queues, and returns what they evicted.
///
/// Whether the retry queue persists to disk was fixed when it was opened, so settings that would turn persistence on
/// or off are not applied, with a warning, and the queue keeps what it has.
async fn apply_queue_limits<B>(
    pending_txns: &mut PendingTransactions<Transaction<B>>, current: &LoopLimits, new: &LoopLimits, endpoint_url: &str,
) -> PushResult
where
    B: Buf + Clone,
{
    let mut push_result = pending_txns.set_max_enqueued(new.buffer_size).await;
    let retry_queue = &mut pending_txns.low_priority;
    push_result.merge(retry_queue.set_max_size_bytes(new.queue_bytes()).await);
    retry_queue.set_flush_to_disk_mem_ratio(new.retry.flush_to_disk_mem_ratio());

    if new.spills_to_disk() != retry_queue.has_disk_persistence() {
        warn!(
            endpoint_url,
            spills_to_disk = new.spills_to_disk(),
            "Turning disk persistence on or off applies to a recipient's next incarnation, or after a restart. The \
             running retry queue keeps its persistence."
        );
    } else if retry_queue.has_disk_persistence()
        && (new.retry.storage_max_size_bytes(), new.retry.storage_max_disk_ratio())
            != (
                current.retry.storage_max_size_bytes(),
                current.retry.storage_max_disk_ratio(),
            )
    {
        match retry_queue
            .set_disk_limits(new.retry.storage_max_size_bytes(), new.retry.storage_max_disk_ratio())
            .await
        {
            Ok(disk_result) => push_result.merge(disk_result),
            Err(e) => warn!(endpoint_url, error = %e, "Failed to apply new disk limits to the retry queue."),
        }
    }
    pending_txns.record_retry_queue_size();

    push_result
}

/// How an endpoint I/O loop queues the transactions it takes from its input.
struct Intake {
    endpoint_url: String,
    endpoint_domain: String,
    endpoint_name: Arc<EndpointNameFn>,
    telemetry: ComponentTelemetry,
    input_telemetry: FastHashMap<MetaString, TransactionInputTelemetry>,

    /// Whether the endpoint filters metrics payloads by the protocol variants it accepts: a native forwarder's endpoint
    /// does, and a delivery host's recipient does not, since its host decides which recipients take every payload it
    /// routes (payloads that name their recipients as their lease resolved them, the others by each recipient's
    /// current protocol).
    filters_by_protocol: bool,
}

impl Intake {
    /// Queues a transaction, unless the endpoint does not take its metrics variant, and returns what queueing it
    /// persisted and dropped, which this counts.
    ///
    /// A transaction the queue refuses, being larger than the retry queue's bound when the high-priority queue is full,
    /// is dropped and counted.
    async fn accept<B>(
        &mut self, txn: Transaction<B>, pending_txns: &mut PendingTransactions<Transaction<B>>, v3: V3Acceptance,
    ) -> PushResult
    where
        B: Buf + Clone,
    {
        let endpoint_url = self.endpoint_url.as_str();
        // Endpoint targeting was checked before dispatch to this task. A native endpoint filters by protocol before
        // queuing; a delivery host's recipient takes what its host routed to it.
        let payload_info = txn.metadata().payload_info;
        let endpoint_v3_settings = EndpointV3Settings {
            use_v3_series: v3.series,
            use_v3_sketches: v3.sketches,
        };
        if self.filters_by_protocol && !endpoint_v3_settings.should_receive_payload(payload_info) {
            debug!(
                endpoint_url,
                ?payload_info,
                "Filtering out transaction based on endpoint V3 settings."
            );
            return PushResult::default();
        }
        let transaction_size = txn.size_bytes();
        let resolved = (self.endpoint_name)(txn.request_uri());
        let logical = resolved.as_deref().unwrap_or_else(|| txn.request_uri().path());
        track_transaction_input_for_endpoint(
            &mut self.input_telemetry,
            &self.telemetry,
            &self.endpoint_domain,
            logical,
            transaction_size,
        );

        let mut refused = PushResult::default();
        refused.track_dropped_item(&txn);
        let push_result = match pending_txns.push_high_priority(txn).await {
            Ok(push_result) => push_result,
            Err(e) => {
                error!(endpoint_url, error = %e, "Failed to enqueue transaction. Events may be permanently lost.");
                refused
            }
        };
        track_queue_drops(&self.telemetry, &self.endpoint_domain, &push_result);

        push_result
    }

    /// Queues every transaction already in `input`, as a loop does when it stops taking transactions: they were sent
    /// before it stopped. Returns whether the input is still open.
    async fn accept_buffered<B>(
        &mut self, input: &mut mpsc::Receiver<Transaction<B>>, pending_txns: &mut PendingTransactions<Transaction<B>>,
        v3: V3Acceptance,
    ) -> bool
    where
        B: Buf + Clone,
    {
        loop {
            match input.try_recv() {
                Ok(txn) => {
                    let _ = self.accept(txn, pending_txns, v3).await;
                }
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }
}

/// Runs the I/O loop of one endpoint until it exits, and returns how it exited.
///
/// Without `control`, as for a native forwarder's endpoint, the loop closes as at process shutdown once its input
/// closes, and its settings never change. With it, the loop follows the close requests and settings of
/// [`RecipientLoop`].
async fn run_endpoint_io_loop<B, C>(
    mut txns_rx: mpsc::Receiver<Transaction<B>>, endpoint_loop: EndpointLoop<B, C>, control: Option<LoopControl<C>>,
) -> RecipientExit
where
    B: Body + Buf + Clone + Send + Sync + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    C: Service<Request<ClientBody>, Response = Response<Incoming>, Error = BoxError> + Clone + Send + 'static,
    C::Future: Send + 'static,
{
    let EndpointLoop {
        telemetry,
        queue_telemetry,
        mut retry_telemetry,
        retry_cause_telemetry,
        endpoint_name,
        route,
        endpoint,
        request_mapper,
        secrets,
        emitter,
        queue_id,
        queue,
        mut settings,
    } = endpoint_loop;
    let (mut close_requests, mut setting_changes) = match control {
        Some(LoopControl { close, settings }) => (Some(close), Some(settings)),
        None => (None, None),
    };
    let endpoint_url = endpoint.endpoint().to_string();
    let configured_endpoint = endpoint.configured_endpoint().to_string();
    let endpoint_domain = endpoint.endpoint().origin().ascii_serialization();

    debug!(
        endpoint_url,
        endpoint_concurrency = settings.limits.concurrency,
        configured_endpoint,
        ?route,
        endpoint_v3 = ?settings.limits.v3,
        "Starting endpoint I/O task."
    );
    // Held until the loop has stopped counting, so that the domain's series outlive what the loop reports to them.
    let _domain_series = telemetry.register_domain(&endpoint_domain);

    // Build our endpoint service, once. This is where we'll modify the incoming transaction for our specific endpoint,
    // such as setting the host portion of the URI, adding the API key as a header, and so on. Publications change its
    // settings in place.
    let mut service_parts = ServiceParts::new(
        &settings.limits,
        secrets,
        retry_cause_telemetry,
        emitter,
        endpoint_url.clone(),
        (request_mapper)(endpoint),
    );
    let mut service = service_parts.build(settings.client.clone());

    let retry_queue = match queue {
        Some(retry_queue) => retry_queue,
        None => open_retry_queue(&settings.limits, queue_id.clone())
            .await
            .unwrap_or_else(|e| {
                error!(
                    endpoint_url,
                    error = %e,
                    "Failed to initialize disk persistence for retry queue. Transactions will not be persisted."
                );
                RetryQueue::new(queue_id, settings.limits.queue_bytes())
                    .with_flush_to_disk_mem_ratio(settings.limits.retry.flush_to_disk_mem_ratio())
            }),
    };
    let mut pending_txns = PendingTransactions::new(
        settings.limits.buffer_size,
        retry_queue,
        queue_telemetry,
        MetaString::from(endpoint_domain.as_str()),
        settings.limits.retry.capacity_time_interval_secs(),
    );

    let mut in_flight = JoinSet::new();
    let mut intake = Intake {
        endpoint_url: endpoint_url.clone(),
        endpoint_domain: endpoint_domain.clone(),
        endpoint_name: Arc::clone(&endpoint_name),
        telemetry: telemetry.clone(),
        input_telemetry: FastHashMap::default(),
        filters_by_protocol: close_requests.is_none(),
    };
    let mut phase = Phase::Running;
    let mut input_open = true;
    let mut retirement = Retirement::default();
    let mut failure = None;

    // A close request made before the loop started applies at once.
    let mut close_change = close_requests.as_mut().map(CloseRequests::take);

    loop {
        // Apply the change of close request the loop took last, if any. A loop it leaves not running takes nothing
        // more from its input, so queue what is in it already: it was sent before the change.
        if let Some(change) = close_change.take() {
            debug!(endpoint_url, ?change, "Endpoint I/O task applies a close request.");
            apply_close_change(
                &mut phase,
                &mut retirement,
                change,
                settings.limits.policy.on_retire,
                input_open,
            );
            if phase != Phase::Running && input_open {
                input_open = intake
                    .accept_buffered(&mut txns_rx, &mut pending_txns, settings.limits.v3)
                    .await;
            }
        }

        service_parts.concurrency.settle();
        let finished = match phase {
            Phase::Running => false,
            Phase::Retiring { .. } => in_flight.is_empty() && pending_txns.is_empty(),
            Phase::Closing { .. } => in_flight.is_empty(),
        };
        if finished {
            // The close becomes final here, unless a reopen the loop has not applied yet cancelled the retirement it
            // ends, in which case the loop applies it and goes on.
            match close_requests
                .as_mut()
                .map(|requests| requests.seal(phase.is_reopenable()))
            {
                Some(Err(change)) => {
                    close_change = Some(change);
                    continue;
                }
                Some(Ok(())) | None => {
                    debug!(endpoint_url, ?phase, "Endpoint I/O task finished its requests.");
                    break;
                }
            }
        }
        let deadline = match phase {
            Phase::Retiring { deadline } => Some(deadline),
            Phase::Running | Phase::Closing { .. } => None,
        };

        select! {
            // Try and drain the next transaction from our channel, and push it into the pending transactions queue.
            maybe_txn = txns_rx.recv(), if input_open && phase == Phase::Running => match maybe_txn {
                Some(txn) => {
                    let _ = intake.accept(txn, &mut pending_txns, settings.limits.v3).await;
                },
                None => {
                    // Our transactions channel has been closed. A close request set before the senders were dropped
                    // decides what that means; otherwise we close as at shutdown, which stops any further dispatch but
                    // allows in-flight transactions to complete.
                    input_open = false;
                    if let Some(close_requests) = close_requests.as_mut() {
                        let change = close_requests.take();
                        apply_close_change(
                            &mut phase,
                            &mut retirement,
                            change,
                            settings.limits.policy.on_retire,
                            input_open,
                        );
                    }
                    if phase == Phase::Running {
                        phase = Phase::Closing { reopenable: false };
                    }
                    debug!(
                        endpoint_url,
                        ?phase,
                        "Requests channel for endpoint I/O task complete. Completing any in-flight requests..."
                    );
                }
            },

            // While there are pending transactions and we are not closing, wait for the service to become ready and
            // then dispatch the next available pending transaction. A retiring loop keeps dispatching its queued work.
            // A loop whose concurrency limit was lowered waits for its requests in flight to bring it within the limit.
            svc = service.ready(),
                if !phase.is_closing() && !pending_txns.is_empty() && service_parts.concurrency.is_settled() =>
            match svc {
                Ok(svc) => {
                    if let Some(pending_txn) = pending_txns.pop().await {
                        let (metadata, request, retry_counters) = prepare_transaction_for_dispatch(
                            pending_txn,
                            &mut retry_telemetry,
                            endpoint_name.as_ref(),
                        );
                        let body_size = request.body().remaining() as u64;
                        in_flight.spawn(svc.call(request).map(move |result| InFlightTransaction {
                            metadata,
                            body_size,
                            retry_counters,
                            result,
                        }));

                        debug!(endpoint_url, "Request sent.");
                    }
                },
                Err(e) => match e {
                    RetryCircuitBreakerError::Service(e) => {
                        telemetry.track_sent_request_error();
                        error!(
                            endpoint_url,
                            error = %e,
                            error_source = ?e.source(),
                            "Unexpected error when querying service for readiness."
                        );
                        if let Phase::Retiring { .. } = phase {
                            // The drain cannot continue, so it ends as at its deadline: what is queued is persisted
                            // or dropped, and counted in the drain report.
                            phase = Phase::Closing { reopenable: true };
                        } else {
                            failure = Some(format!(
                                "the I/O loop of '{endpoint_url}' stopped on an error while querying its service for \
                                 readiness: {e}"
                            ));
                            break;
                        }
                    },
                    RetryCircuitBreakerError::Retry(_) | RetryCircuitBreakerError::Open(_) => {
                        unreachable!("should not get retry or open error when querying service for readiness")
                    },
                }
            },

            // Drive any in-flight transactions to completion.
            maybe_result = in_flight.join_next(), if !in_flight.is_empty() => {
                let task_result = maybe_result.expect("in_flight marked as not being empty");
                let outcome = handle_in_flight_transaction_result(
                    task_result,
                    &mut pending_txns,
                    &telemetry,
                    &endpoint_url,
                    &endpoint_domain,
                ).await;
                if outcome == RequestOutcome::Delivered && retirement.requested {
                    retirement.delivered += 1;
                }
            },

            change = next_close_change(&mut close_requests) => close_change = Some(change),

            () = sleep_until_deadline(deadline) => {
                debug!(endpoint_url, "Endpoint I/O task reached its drain deadline before its queue was empty.");
                retirement.deadline_reached = true;
                phase = Phase::Closing { reopenable: true };
            },

            new_settings = next_change(&mut setting_changes) => {
                debug!(endpoint_url, "Endpoint I/O task applies new settings.");
                let push_result =
                    apply_queue_limits(&mut pending_txns, &settings.limits, &new_settings.limits, &endpoint_url).await;
                track_queue_drops(&telemetry, &endpoint_domain, &push_result);
                service_parts.apply(&new_settings.limits);
                settings = new_settings;
            },
        }
    }

    // The loop's close is final: an error ended it, or its close was sealed above, after which a reopen is refused.
    if let Some(close_requests) = close_requests.as_mut() {
        let _ = close_requests.seal(false);
    }

    // Close the input, so that a later send fails rather than being lost uncounted, and queue what it still holds: it
    // was sent before the close, and is persisted or dropped with the rest.
    txns_rx.close();
    let mut closed = PushResult::default();
    while let Ok(txn) = txns_rx.try_recv() {
        closed.merge(intake.accept(txn, &mut pending_txns, settings.limits.v3).await);
    }

    // Close the pending transactions queue: flush it, which will potentially enqueue them to disk if we have disk
    // persistence enabled for the retry queue, or drop what is in memory when the recipient discards it. A transaction
    // that cannot be queued or persisted is dropped and counted, and the flush goes on with the rest.
    let close_result = if retirement.discard {
        pending_txns.discard()
    } else {
        match pending_txns.flush().await {
            Ok(flush_result) => flush_result,
            Err(FlushError { error, result }) => {
                error!(
                    endpoint_url,
                    error = %error,
                    "Failed to flush pending transactions. Events may be permanently lost."
                );
                result
            }
        }
    };
    debug!(
        items_dropped = close_result.items_dropped,
        events_dropped = close_result.events_dropped,
        items_persisted = close_result.items_persisted,
        "Flushed pending transactions prior to shutdown."
    );
    track_queue_drops(&telemetry, &endpoint_domain, &close_result);
    closed.merge(close_result);

    debug!(endpoint_url, "Endpoint I/O task stopped.");

    match failure {
        Some(reason) => RecipientExit::Failed(reason),
        None => RecipientExit::Retired(DrainReport {
            delivered: retirement.delivered,
            persisted: closed.items_persisted,
            dropped: closed.items_dropped,
            deadline_reached: retirement.deadline_reached,
        }),
    }
}

/// Returns the name of a recipient's queue directory: see [`RecipientPieces::queue_directory`].
pub(crate) fn recipient_queue_id(
    context: &ComponentContext, spec: &RecipientSpec, endpoint: &ResolvedEndpoint,
) -> String {
    match &spec.origin {
        RecipientOrigin::Native(_) => generate_retry_queue_id(context.clone(), endpoint),
        RecipientOrigin::Declared { credential, .. } => {
            let hash = hash_single_stable((context.component_id(), spec.endpoint.as_str(), &**credential));
            format!("{}/{}/{:x}", context.component_id(), resolved_host(endpoint), hash)
        }
    }
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

    format!("{}/{}/{:x}", context.component_id(), resolved_host(endpoint), hash)
}

/// Returns the host of a resolved endpoint's URL, which carries the data plane's version for a Datadog site.
fn resolved_host(endpoint: &ResolvedEndpoint) -> &str {
    endpoint
        .endpoint()
        .host_str()
        .expect("resolved endpoint must have a host")
}

fn track_queue_drops(telemetry: &ComponentTelemetry, domain: &str, push_result: &PushResult) {
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

/// Processes an HTTP response to a forwarded intake request, updating telemetry and logging as appropriate, and returns
/// whether the intake accepted the request.
async fn process_http_response(
    response: Response<Incoming>, metadata: Metadata, body_size: u64, telemetry: &ComponentTelemetry,
    endpoint_url: &str, domain: &str,
) -> bool {
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

        telemetry.track_successful_transaction(&metadata, body_size, domain);
        true
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
        false
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

        // The message text here is kept in sync with an equivalent log emitted by another implementation of
        // this forwarder, so that log-based alerting on this condition matches regardless of which implementation
        // is deployed.
        error!(
            endpoint_url,
            "API Key invalid (403 response), dropping transaction for {endpoint_url}."
        );
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
    max_enqueued: usize,
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
            max_enqueued,
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

    /// Changes the maximum capacity of the high-priority queue while the queue is in use.
    ///
    /// Transactions beyond a smaller capacity move to the low-priority queue, newest first, as a push to a full
    /// high-priority queue would; one the low-priority queue refuses is dropped, and counted in the returned
    /// `PushResult` with what the moves evicted.
    pub async fn set_max_enqueued(&mut self, max_enqueued: usize) -> PushResult {
        self.max_enqueued = max_enqueued;
        let mut push_result = PushResult::default();
        while self.high_priority.len() > max_enqueued {
            let transaction = self
                .high_priority
                .pop_back()
                .expect("the queue holds more than its capacity");
            self.telemetry.high_prio_queue_removals().increment(1);
            let (events, data_points) = (transaction.event_count(), transaction.data_point_count());
            match self.low_priority.push(transaction).await {
                Ok(moved) => {
                    self.telemetry.low_prio_queue_insertions().increment(1);
                    push_result.merge(moved);
                }
                Err(e) => {
                    error!(
                        error = %e,
                        "Failed to move a pending transaction to the retry queue. Events may be permanently lost."
                    );
                    push_result.items_dropped += 1;
                    push_result.events_dropped += events;
                    push_result.data_points_dropped += data_points;
                }
            }
        }
        self.record_retry_queue_size();

        push_result
    }

    /// Pushes a high-priority transaction into the queue.
    ///
    /// If the high-priority queue is full, the transaction will be pushed into the low-priority queue.
    pub async fn push_high_priority(&mut self, transaction: T) -> Result<PushResult, GenericError> {
        self.record_incoming_transaction_size(transaction.size_bytes()).await;

        if self.high_priority.len() < self.max_enqueued {
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
    /// A transaction the low-priority queue refuses, being larger than its bound, or that cannot be persisted, is
    /// dropped, and the flush goes on with the rest, so that one such transaction does not lose those after it. The
    /// returned `PushResult` counts every transaction, as persisted or as dropped.
    ///
    /// # Errors
    ///
    /// If a transaction cannot be moved to the low-priority queue, or cannot be persisted, the first such error is
    /// returned, with what the flush did.
    pub async fn flush(mut self) -> Result<PushResult, FlushError> {
        let mut push_result = PushResult::default();
        let mut first_error = None;

        // Push all high-priority transactions into the low-priority queue.
        while let Some(transaction) = self.high_priority.pop_front() {
            self.telemetry.high_prio_queue_removals().increment(1);

            let mut refused = PushResult::default();
            refused.track_dropped_item(&transaction);
            match self.low_priority.push(transaction).await {
                Ok(subpush_result) => {
                    self.telemetry.low_prio_queue_insertions().increment(1);
                    self.record_retry_queue_size();
                    push_result.merge(subpush_result);
                }
                Err(e) => {
                    push_result.merge(refused);
                    first_error.get_or_insert(e);
                }
            }
        }

        // Flush the low-priority queue.
        match self.low_priority.flush().await {
            Ok(flush_result) => push_result.merge(flush_result),
            Err(FlushError { error, result }) => {
                push_result.merge(result);
                first_error.get_or_insert(error);
            }
        }

        match first_error {
            None => Ok(push_result),
            Some(error) => Err(FlushError {
                error,
                result: push_result,
            }),
        }
    }

    /// Drops every pending transaction held in memory without persisting it, and finalizes the queue.
    ///
    /// The returned `PushResult` counts every transaction dropped. Transactions the low-priority queue already
    /// persisted stay on disk, for the next queue opened on its directory.
    pub fn discard(mut self) -> PushResult {
        let mut push_result = PushResult::default();
        while let Some(transaction) = self.high_priority.pop_front() {
            self.telemetry.high_prio_queue_removals().increment(1);
            push_result.track_dropped_item(&transaction);
        }
        push_result.merge(self.low_priority.discard());

        push_result
    }

    fn record_retry_queue_size(&self) {
        self.telemetry
            .record_retry_queue_size(&self.domain, self.low_priority.len());
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
    use std::collections::HashMap;
    use std::{
        future::{pending, Pending},
        sync::{
            atomic::{AtomicU16, AtomicUsize, Ordering},
            Arc, OnceLock,
        },
    };

    use agent_data_plane_config::{shared::SharedConfiguration, ConfigValue, SalukiConfiguration};
    use bytes::Bytes;
    use http::StatusCode;
    use http_body_util::Empty;
    use rustls::{version::TLS12, RootCertStore, ServerConfig};
    use saluki_common::buf::FrozenChunkedBytesBuffer;
    use saluki_core::{
        components::test_util::TestComponentSupervisor,
        observability::ComponentMetricsExt as _,
        runtime::state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter},
    };
    use saluki_io::net::client::http::TlsMinimumVersion;
    use saluki_metrics::test::TestRecorder;
    use saluki_tls::test_util::SelfSignedCert;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
        time::{timeout, Duration},
    };
    use tokio_rustls::TlsAcceptor;
    use tower::{retry::Policy, service_fn};

    use super::*;
    use crate::common::datadog::transaction::{Metadata as TxnMetadata, Transaction};
    use crate::common::datadog::{
        config::ForwarderHttpProtocol,
        delivery::{IntakeAuthorities, PayloadClasses, ProxySettings, RetrySettings},
        endpoints::resolve_additional_endpoints,
        protocol::{MetricsEndpointRouting, MetricsPayloadInfo},
        test_util::{shared_configuration, LiveConfiguration, TEST_API_KEY},
    };
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

    #[test]
    fn alternate_metrics_intakes_inherit_primary_policy_for_dispatch() {
        const ALTERNATE: &str = "https://alternate.example.com";
        for use_vector in [false, true] {
            let mut shared = shared_configuration();
            let alternate = agent_data_plane_config::shared::AltMetricsIntake {
                enabled: true,
                url: ALTERNATE.to_string(),
                use_v3_series: false,
            };
            if use_vector {
                shared.endpoints.vector_intake = alternate;
            } else {
                shared.endpoints.opw_intake = alternate;
            }
            // An additional route to the same URL must still retain its own policy identity.
            shared.endpoints.additional_endpoints = HashMap::from([(ALTERNATE.to_string(), vec!["key".to_string()])]);
            let primary = shared.endpoints.primary_endpoint();
            let config = ForwarderConfiguration::from_configuration(&shared);
            let endpoints = config.build_routable_endpoints().unwrap();
            assert!(endpoints
                .iter()
                .any(|endpoint| endpoint.route() == EndpointRoute::MetricsPrimary));
            for endpoint in endpoints {
                let policy_endpoint = config.metrics_policy_endpoint(endpoint.route(), endpoint.endpoint());
                let mut metadata = TxnMetadata::from_event_and_data_point_count(1, 1);
                // Without an endpoint policy, payloads retain ordinary delivery.
                assert!(matches_metrics_endpoint_routing(policy_endpoint, &metadata));
                metadata.metrics_endpoint_routing =
                    Some(MetricsEndpointRouting::AllExcept([primary.clone()].into()).into());
                assert_eq!(
                    matches_metrics_endpoint_routing(policy_endpoint, &metadata),
                    endpoint.route() == EndpointRoute::Additional
                );
                metadata.metrics_endpoint_routing = Some(MetricsEndpointRouting::Only([primary.clone()].into()).into());
                assert_eq!(
                    matches_metrics_endpoint_routing(policy_endpoint, &metadata),
                    endpoint.route() != EndpointRoute::Additional
                );
                if endpoint.route() == EndpointRoute::MetricsPrimary {
                    assert_eq!(endpoint.endpoint().configured_endpoint(), ALTERNATE);
                }
            }
        }
    }

    #[test]
    fn endpoint_routing_selects_whole_payloads_before_dispatch() {
        const SELECTED: &str = "https://secondary.example.com";

        let mut baseline = TxnMetadata::from_event_and_data_point_count(2, 2);
        assert!(matches_metrics_endpoint_routing(SELECTED, &baseline));
        baseline.metrics_endpoint_routing =
            Some(MetricsEndpointRouting::AllExcept([SELECTED.to_string()].into()).into());
        assert!(!matches_metrics_endpoint_routing(SELECTED, &baseline));
        assert!(matches_metrics_endpoint_routing("https://other.example.com", &baseline));

        let mut filtered = TxnMetadata::from_event_and_data_point_count(1, 1);
        filtered.metrics_endpoint_routing = Some(MetricsEndpointRouting::Only([SELECTED.to_string()].into()).into());
        assert!(matches_metrics_endpoint_routing(SELECTED, &filtered));
        assert!(!matches_metrics_endpoint_routing(
            "https://other.example.com",
            &filtered
        ));
    }

    fn is_metrics_request_path(path: &'static str) -> bool {
        is_metrics_request_uri(&uri(path), METRICS_SERIES_V3_BETA_PATH)
    }

    async fn forwarder_config_from(shared: SharedConfiguration) -> ForwarderConfiguration {
        ForwarderConfiguration::from_configuration(&shared)
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
        let additional = HashMap::from([
            ("app.datadoghq.com".to_string(), vec!["key-a".to_string()]),
            ("https://app.datadoghq.com".to_string(), vec!["key-b".to_string()]),
        ]);
        let mut endpoints = resolve_additional_endpoints(&additional).expect("endpoints should resolve");
        // The configured endpoints are a map, so fix an order to compare queue IDs against.
        endpoints.sort_by(|left, right| left.configured_endpoint().cmp(right.configured_endpoint()));

        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].endpoint(), endpoints[1].endpoint());

        let first_queue_id = generate_retry_queue_id(context.clone(), &endpoints[0]);
        let second_queue_id = generate_retry_queue_id(context, &endpoints[1]);

        assert_ne!(first_queue_id, second_queue_id);
    }

    #[test]
    fn retry_queue_id_uses_additional_endpoint_api_key_index() {
        let context = test_component_context();
        let additional = HashMap::from([(
            "app.datadoghq.com".to_string(),
            vec!["key-a".to_string(), "key-b".to_string()],
        )]);
        let endpoints = resolve_additional_endpoints(&additional).expect("endpoints should resolve");

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

    #[test]
    fn a_native_recipients_queue_endpoint_tells_two_endpoints_apart_exactly_when_their_queue_directories_differ() {
        // A recipient's identity spells its endpoint as `queue_endpoint` does, so two identities never share a queue
        // directory, and two spellings of one directory are one identity: a respelled primary endpoint is then the
        // same recipient, which a publication changes the settings of, rather than a second one waiting forever for
        // the directory the first one owns.
        use crate::common::datadog::endpoints::{queue_endpoint, MetricsRecipientRoute};

        let context = test_component_context();
        let versioned = format!(
            "https://{}.datadoghq.com",
            crate::common::datadog::endpoints::get_data_plane_version_prefix()
        );
        let spellings = [
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "http://127.0.0.1:8081",
            "https://intake.example.com",
            "https://intake.example.com:443",
            "https://INTAKE.example.com/",
            "intake.example.com",
            "https://intake.example.com/v1",
            "https://app.datadoghq.com",
            "https://app.datadoghq.com/",
            "app.datadoghq.com",
            "https://api.datadoghq.com",
            "https://app.datadoghq.eu",
            "https://app.mrf.datadoghq.com",
            versioned.as_str(),
        ];
        for a in spellings {
            for b in spellings {
                let directory = |raw: &str| {
                    let endpoint = ResolvedEndpoint::from_raw_endpoint(raw, "key").expect("endpoint resolves");
                    generate_retry_queue_id(context.clone(), &endpoint)
                };
                let same_directory = directory(a) == directory(b);
                let same_endpoint = queue_endpoint(MetricsRecipientRoute::Primary, a)
                    == queue_endpoint(MetricsRecipientRoute::Primary, b);
                assert_eq!(same_endpoint, same_directory, "{a} and {b}");
            }
        }
        assert_eq!(
            queue_endpoint(MetricsRecipientRoute::Primary, "app.datadoghq.com"),
            "https://app.datadoghq.com"
        );
        assert_eq!(
            queue_endpoint(MetricsRecipientRoute::Additional, "app.datadoghq.com"),
            "app.datadoghq.com",
            "an additional endpoint's directory hashes its URL as configured"
        );
    }

    #[test]
    fn retry_queue_directories_follow_the_component_the_resolved_host_and_the_endpoint_identity() {
        // Pins today's directory names (plan F20 and 4.9): `{component}/{resolved host}/{hash}`, the hash taken over
        // the component and the resolved URL for a primary endpoint, and over the component, the raw configured URL,
        // and the key's position for an additional endpoint, never over a key. The literals fail if the hash or its
        // inputs change, which would orphan every persisted backlog.
        let context = test_component_context();
        let primary = ResolvedEndpoint::from_raw_endpoint("http://127.0.0.1:8080", "key-a").expect("endpoint resolves");
        let rotated_primary =
            ResolvedEndpoint::from_raw_endpoint("http://127.0.0.1:8080", "key-b").expect("endpoint resolves");
        assert_eq!(
            generate_retry_queue_id(context.clone(), &primary),
            "test_forwarder/127.0.0.1/2aa975b6ce4b7341"
        );
        assert_eq!(
            generate_retry_queue_id(context.clone(), &rotated_primary),
            generate_retry_queue_id(context.clone(), &primary)
        );

        let additional = |keys: [&str; 2]| {
            resolve_additional_endpoints(&HashMap::from([(
                "app.datadoghq.eu".to_string(),
                keys.iter().map(|key| key.to_string()).collect(),
            )]))
            .expect("endpoints resolve")
        };
        let configured = additional(["key-a", "key-b"]);
        let rotated = additional(["key-c", "key-d"]);
        // The host segment is the resolved host, which carries the data plane's version for a Datadog site, so a new
        // version moves every Datadog endpoint's directory (A19), while live changes within one process do not.
        let resolved_host = configured[0].endpoint().host_str().expect("the endpoint has a host");
        assert_ne!(resolved_host, "app.datadoghq.eu");
        assert!(resolved_host.ends_with(".datadoghq.eu"), "{resolved_host}");
        assert_eq!(
            generate_retry_queue_id(context.clone(), &configured[0]),
            format!("test_forwarder/{resolved_host}/63075f438dd39a91")
        );
        assert_eq!(
            generate_retry_queue_id(context.clone(), &configured[1]),
            format!("test_forwarder/{resolved_host}/979d55a425ced344")
        );
        // A key rotated at its position keeps the directory, and with it the backlog.
        for (configured, rotated) in configured.iter().zip(&rotated) {
            assert_eq!(
                generate_retry_queue_id(context.clone(), configured),
                generate_retry_queue_id(context.clone(), rotated)
            );
        }
    }

    /// Points the forwarder's retry storage at `storage`, with room for every test transaction.
    fn persist_retries_under(shared: &mut SharedConfiguration, storage: &std::path::Path) {
        shared.endpoints.forwarder.storage_path = storage.to_path_buf();
        shared.endpoints.forwarder.storage_max_size_in_bytes = 1024 * 1024;
        // The share of the disk the queue may use is checked against the real volume, so allow all of it.
        shared.endpoints.forwarder.storage_max_disk_ratio = 1.0;
    }

    /// Returns the persisted retry files in `dir`, in name order, which is the order they were written.
    fn retry_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with("retry-"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// Returns every value of the `DD-API-KEY` header a recorded request presented.
    fn recorded_api_keys(request: &str) -> Vec<&str> {
        request
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim().eq_ignore_ascii_case("dd-api-key").then_some(value.trim())
            })
            .collect()
    }

    /// Receives the next request a test server recorded, failing if none arrives in time.
    async fn next_request(requests: &mut mpsc::Receiver<String>) -> String {
        timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("the server should record a request")
            .expect("the server should be running")
    }

    #[tokio::test]
    async fn a_persisted_retry_holds_the_api_key_it_was_sent_with_and_is_sent_again_with_the_current_key() {
        // Pins today's persistence (plan F20) before stage 7 changes the loop: a retry persists with the headers it
        // was sent with, so the file at rest holds the API key, and the request mapper replaces the key when the
        // retry is dispatched again, so the key configured after a restart is the one presented.
        let status = Arc::new(AtomicU16::new(StatusCode::INTERNAL_SERVER_ERROR.as_u16()));
        let (server_url, _counter, mut requests) = start_switchable_http_server(Arc::clone(&status)).await;
        let storage = tempfile::tempdir().expect("temporary directory should be created");
        let forwarder_with_key = |api_key: &'static str| {
            let storage = storage.path().to_path_buf();
            build_test_forwarder_with(&server_url, LiveForwarderConfiguration::default(), move |shared| {
                shared.endpoints.api_key = api_key.to_string();
                persist_retries_under(shared, &storage);
            })
        };

        let (_, forwarder) = forwarder_with_key("key-sent-first").await;
        let queue_dir = storage.path().join(generate_retry_queue_id(
            test_component_context(),
            forwarder.endpoints[0].endpoint(),
        ));
        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");
        assert_eq!(
            recorded_api_keys(&next_request(&mut requests).await),
            ["key-sent-first"]
        );
        // Every attempt fails, so the transaction is still queued when the forwarder closes, and persists.
        drain_test_forwarder(supervisor, handle).await;

        let files = retry_files(&queue_dir);
        assert_eq!(
            files.len(),
            1,
            "the transaction persists in its queue's directory: {files:?}"
        );
        let at_rest = std::fs::read_to_string(&files[0]).expect("the persisted retry should be readable");
        assert!(
            at_rest.contains("key-sent-first"),
            "the persisted retry holds the key it was sent with: {at_rest}"
        );

        // A restart with a rotated key adopts the directory and sends the retry with the key configured now.
        status.store(StatusCode::OK.as_u16(), Ordering::SeqCst);
        while requests.try_recv().is_ok() {}
        let (_, forwarder) = forwarder_with_key("key-after-rotation").await;
        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
        assert_eq!(
            recorded_api_keys(&next_request(&mut requests).await),
            ["key-after-rotation"]
        );
        drain_test_forwarder(supervisor, handle).await;
        assert_eq!(retry_files(&queue_dir), Vec::<std::path::PathBuf>::new());
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
        let telemetry =
            TransactionQueueTelemetry::from_builder(&builder, "https://example.com", "example", shared.clone());
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
                body_size: 0,
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
                body_size: 0,
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
                body_size: 0,
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
                body_size: 0,
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
        assert_eq!(
            recorder.gauge(("network_http_retry_queue_size", &[("domain", "https://example.com")])),
            Some(1.0)
        );
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
    async fn flush_moves_the_transactions_after_one_the_retry_queue_refuses_and_counts_that_one_as_dropped() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (telemetry, domain) = transaction_queue_telemetry();
        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let root_path = temp_dir.path().to_path_buf();
        let disk = PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: 4096,
            storage_max_disk_ratio: 1.0,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path.clone())),
            max_age_days: 10,
        };
        // The retry queue holds four bytes in memory, so it refuses the middle transaction alone.
        let retry_queue = RetryQueue::new("test".to_string(), 4)
            .with_disk_persistence(disk)
            .await
            .expect("disk persistence should initialize");
        let mut pending_txns = PendingTransactions::new(4, retry_queue, telemetry, domain, 900);
        for transaction in ["ab", "too large", "cd"] {
            let _ = pending_txns
                .push_high_priority(transaction.to_string())
                .await
                .expect("the high-priority queue has room");
        }

        let FlushError { error, result } = pending_txns
            .flush()
            .await
            .expect_err("the flush reports the transaction it could not move");
        assert!(
            error.to_string().contains("Entry too large to fit into retry queue"),
            "{error}"
        );
        assert_eq!((result.items_persisted, result.items_dropped), (2, 1));

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
        assert_eq!(persisted, ["ab", "cd"]);
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

    #[tokio::test]
    async fn tls_certificate_validation_enabled_by_default() {
        let config = forwarder_config_from(shared_configuration()).await;

        assert_eq!(
            TlsCertificateValidation::from_forwarder_config(&config),
            TlsCertificateValidation::Enabled
        );
    }

    #[tokio::test]
    async fn tls_certificate_validation_disabled_when_skip_ssl_validation_enabled() {
        let mut shared = shared_configuration();
        shared.endpoints.tls.skip_ssl_validation = true;
        let config = forwarder_config_from(shared).await;

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
        let (url, counter, _requests) = start_recording_http_server_with_requests(statuses).await;

        (url, counter)
    }

    /// Starts the same server as [`start_recording_http_server`], additionally handing back the text
    /// of each request it accepted, for a test that has to inspect what the forwarder sent.
    async fn start_recording_http_server_with_requests(
        statuses: Vec<StatusCode>,
    ) -> (String, Arc<AtomicUsize>, mpsc::Receiver<String>) {
        start_http_server(move |nth| Reply::Status(statuses[nth.min(statuses.len() - 1)])).await
    }

    /// How a test server answers one request.
    #[derive(Clone, Copy)]
    enum Reply {
        /// Responds with the status.
        Status(StatusCode),

        /// Never responds, holding the connection open until the client gives up.
        Hang,
    }

    /// Starts a server whose replies a test switches while it runs: it answers with the status `status` holds when a
    /// request arrives, or never answers while it holds 0.
    async fn start_switchable_http_server(
        status: Arc<AtomicU16>,
    ) -> (String, Arc<AtomicUsize>, mpsc::Receiver<String>) {
        start_http_server(move |_| match status.load(Ordering::SeqCst) {
            0 => Reply::Hang,
            code => Reply::Status(StatusCode::from_u16(code).expect("the test sets a valid status")),
        })
        .await
    }

    /// Starts a minimal HTTP server on `127.0.0.1:0` that records the text of each request it accepts and answers the
    /// `nth` request, counting from 0, as `reply(nth)` says.
    ///
    /// Returns the server's `http://127.0.0.1:PORT/` URL, a counter of the requests it accepted (one connection per
    /// request, since the server replies with `Connection: close`), and the text of each request's head.
    async fn start_http_server(
        reply: impl Fn(usize) -> Reply + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let counter = Arc::new(AtomicUsize::new(0));

        let reply = Arc::new(reply);
        let counter_for_task = Arc::clone(&counter);
        let (request_tx, request_rx) = mpsc::channel(16);
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let reply = Arc::clone(&reply);
                let counter = Arc::clone(&counter_for_task);
                let request_tx = request_tx.clone();

                tokio::spawn(async move {
                    let Some(request_str) = read_request(&mut stream).await else {
                        return;
                    };

                    // A test that does not read the requests drops the receiver, so never block here.
                    let _ = request_tx.try_send(request_str);

                    let nth = counter.fetch_add(1, Ordering::SeqCst);
                    let status = match reply(nth) {
                        Reply::Status(status) => status,
                        Reply::Hang => {
                            // Hold the connection until the client closes it.
                            let mut buf = [0u8; 1024];
                            while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                            return;
                        }
                    };

                    respond(&mut stream, status).await;
                });
            }
        });

        (format!("http://127.0.0.1:{port}/"), counter, request_rx)
    }

    /// Reads one request from `stream`: its head, which it returns as text, and its body, which it discards. Returns
    /// `None` if the client goes first.
    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<String> {
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }

        // Drain any body bytes that arrived alongside the headers, plus whatever remains based on a simple
        // Content-Length parse. We don't actually need to buffer it; we just need to consume it so the client doesn't
        // get a connection reset before reading our response.
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
                Err(_) => return None,
            }
        }

        Some(request_str)
    }

    /// Answers a request with `status`, and closes the connection.
    async fn respond(stream: &mut tokio::net::TcpStream, status: StatusCode) {
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    /// The requests a slow test server holds at once.
    #[derive(Clone, Default)]
    struct InFlight {
        now: Arc<AtomicUsize>,
        most: Arc<AtomicUsize>,
        served: Arc<AtomicUsize>,
    }

    impl InFlight {
        /// Returns how many requests the server holds now.
        fn now(&self) -> usize {
            self.now.load(Ordering::SeqCst)
        }

        /// Returns the most requests the server held at once since the last reset, and how many it served.
        fn most_and_served(&self) -> (usize, usize) {
            (self.most.load(Ordering::SeqCst), self.served.load(Ordering::SeqCst))
        }

        /// Starts counting the most requests held at once, and those served, afresh.
        fn reset(&self) {
            self.most.store(self.now(), Ordering::SeqCst);
            self.served.store(0, Ordering::SeqCst);
        }

        /// Waits until the server served `count` requests since the last reset, failing after five seconds.
        async fn wait_until_served(&self, count: usize) {
            let start = std::time::Instant::now();
            while self.served.load(Ordering::SeqCst) < count {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "the server served too few requests"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        /// Waits until the server holds a number of requests `wanted` accepts, failing after five seconds.
        async fn wait_until(&self, wanted: impl Fn(usize) -> bool) {
            let start = std::time::Instant::now();
            while !wanted(self.now()) {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "the server holds {} requests",
                    self.now()
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    /// Starts a server on `127.0.0.1:0` that answers the `nth` request it receives, counting from 0, with 200 after
    /// holding it for `delay(nth)`, and returns its URL with a count of the requests it holds.
    async fn start_slow_http_server(delay: impl Fn(usize) -> Duration + Send + Sync + 'static) -> (String, InFlight) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let in_flight = InFlight::default();
        let received = Arc::new(AtomicUsize::new(0));
        let delay = Arc::new(delay);

        let counts = in_flight.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let counts = counts.clone();
                let (received, delay) = (Arc::clone(&received), Arc::clone(&delay));
                tokio::spawn(async move {
                    if read_request(&mut stream).await.is_none() {
                        return;
                    }
                    let delay = delay(received.fetch_add(1, Ordering::SeqCst));
                    let held = counts.now.fetch_add(1, Ordering::SeqCst) + 1;
                    counts.most.fetch_max(held, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    counts.now.fetch_sub(1, Ordering::SeqCst);
                    counts.served.fetch_add(1, Ordering::SeqCst);
                    respond(&mut stream, StatusCode::OK).await;
                });
            }
        });

        (format!("http://127.0.0.1:{port}/"), in_flight)
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

    async fn build_test_forwarder(
        forwarder_url: &str, secrets: Live<Secrets>,
    ) -> (DataspaceRegistry, TransactionForwarder<FrozenChunkedBytesBuffer>) {
        let live = LiveForwarderConfiguration {
            secrets,
            ..Default::default()
        };

        build_test_forwarder_following(forwarder_url, live).await
    }

    /// Builds a forwarder pointed at `forwarder_url` that follows `live` while it runs.
    async fn build_test_forwarder_following(
        forwarder_url: &str, live: LiveForwarderConfiguration,
    ) -> (DataspaceRegistry, TransactionForwarder<FrozenChunkedBytesBuffer>) {
        build_test_forwarder_with(forwarder_url, live, |_| {}).await
    }

    /// Builds the forwarder [`build_test_forwarder_following`] builds, with the test's own changes to its
    /// configuration.
    async fn build_test_forwarder_with(
        forwarder_url: &str, live: LiveForwarderConfiguration, configure: impl FnOnce(&mut SharedConfiguration),
    ) -> (DataspaceRegistry, TransactionForwarder<FrozenChunkedBytesBuffer>) {
        // The HTTP client builder requires the process-wide TLS crypto provider to be initialized, even when the
        // forwarder is pointed at a plain HTTP endpoint.
        init_tls_crypto_provider();

        // Tight timeouts and small backoffs keep the test under a couple seconds even with retries.
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = ConfigValue::explicit(forwarder_url.to_string());
        shared.endpoints.forwarder.timeout = 1;
        shared.endpoints.forwarder.max_concurrent_requests = 1;
        shared.endpoints.forwarder.high_prio_buffer_size = 4;
        shared.endpoints.forwarder.backoff_base = 0.001;
        shared.endpoints.forwarder.backoff_max = 0.01;
        shared.endpoints.forwarder.backoff_factor = 2.0;
        shared.endpoints.forwarder.recovery_interval = 1;
        shared.endpoints.forwarder.recovery_reset = false;
        // The HTTP client builder otherwise requires the process-wide default root certificate store to be
        // populated. We are talking to a plain HTTP endpoint anyway, so disable validation to skip that path.
        shared.endpoints.tls.skip_ssl_validation = true;
        configure(&mut shared);
        let forwarder_config = forwarder_config_from(shared).await;
        let context = test_component_context();
        let metrics_builder = MetricsBuilder::from_component_context(&context);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        let dataspace = DataspaceRegistry::new();
        let forwarder = dataspace
            .with_current(|| {
                TransactionForwarder::<FrozenChunkedBytesBuffer>::from_config(
                    context,
                    forwarder_config,
                    live,
                    test_logical_endpoint,
                    telemetry,
                    metrics_builder,
                )
            })
            .expect("forwarder should build");

        (dataspace, forwarder)
    }

    /// Starts `forwarder` under a running component supervisor, the way the topology starts one.
    ///
    /// The forwarder spawns on the ambient supervisor, so it has to be started inside the supervisor's scope; outside
    /// one it would panic for want of an ambient supervisor.
    async fn spawn_test_forwarder(
        supervisor: &TestComponentSupervisor, forwarder: TransactionForwarder<FrozenChunkedBytesBuffer>,
    ) -> Handle<FrozenChunkedBytesBuffer> {
        supervisor.scope(async move { forwarder.spawn() }).await
    }

    /// Drains the forwarder and asserts that it did so on its own.
    ///
    /// `ShutdownTimedOut` would mean a task ignored shutdown and had to be aborted, which for the endpoint loops would
    /// mean the retry queue never finished flushing.
    async fn drain_test_forwarder(supervisor: TestComponentSupervisor, handle: Handle<FrozenChunkedBytesBuffer>) {
        handle.shutdown();

        let result = supervisor.shutdown().await;
        assert!(
            result.is_ok(),
            "the forwarder's tasks should have drained on their own: {result:?}"
        );
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

    /// Returns a live view of a configuration in which secret resolution can replace a rejected API key.
    fn secrets_in_use() -> Live<Secrets> {
        let mut config = SalukiConfiguration::default();
        config.shared.secrets.backend_command = Some("/bin/true".to_string());

        LiveConfiguration::new(config).live(|config| &config.shared.secrets)
    }

    /// Returns a configuration whose primary API key is `api_key`.
    fn config_with_api_key(api_key: &str) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = api_key.to_string();

        config
    }

    /// Returns the API key a recorded request presented.
    fn recorded_api_key(request: &str) -> Option<&str> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case("dd-api-key").then_some(value.trim())
        })
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

    #[test]
    fn endpoint_io_loop_task_names_name_the_endpoint_in_one_segment() {
        // Two properties at once. A period separates segments of a process name, so a host interpolated as-is would
        // spread one task across levels of the process tree -- `..._io_loop_app`, `datadoghq`, `com` -- rather than
        // naming it. And the host must be the configured one: the resolved URL is version-prefixed, so naming the task
        // from it would rename the task on every release.
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = ConfigValue::explicit("https://app.datadoghq.com".to_string());
        shared.endpoints.additional_endpoints =
            HashMap::from([("app.datadoghq.eu".to_string(), vec!["extra-key".to_string()])]);
        let endpoints = ForwarderConfiguration::from_configuration(&shared)
            .build_routable_endpoints()
            .expect("endpoints should resolve");

        let mut task_names = endpoints
            .iter()
            .map(|routable| endpoint_io_loop_task_name(routable.endpoint()))
            .collect::<Vec<_>>();
        task_names.sort();

        assert_eq!(
            vec![
                // Configured as a URL.
                "endpoint_io_loop_app_datadoghq_com".to_string(),
                // Configured as a bare host, which doesn't parse as a URL.
                "endpoint_io_loop_app_datadoghq_eu".to_string(),
            ],
            task_names
        );

        for task_name in &task_names {
            assert!(
                !task_name.contains('.'),
                "the name must be a single segment: {task_name}"
            );
        }
    }

    /// Returns the declaration of a recipient a resources artifact declares, sending to `endpoint` with `policy`.
    fn declared_recipient(endpoint: &str, policy: RecipientPolicy) -> RecipientSpec {
        RecipientSpec {
            id: "dd_out/declared/team_d/http://127.0.0.1".into(),
            endpoint: endpoint.to_string(),
            credential: "credentials/team_d".into(),
            classes: PayloadClasses::MetricsOnly,
            protocol: RecipientProtocol::DatadogIntake {
                authorities: IntakeAuthorities::default(),
                allow_arbitrary_tags: false,
                v3: V3Acceptance::default(),
            },
            policy,
            origin: RecipientOrigin::Declared {
                name: "team_d".into(),
                credential: "team_d".into(),
            },
        }
    }

    /// Builds a recipient's pieces as a delivery host's factory would: from those of a test forwarder pointed at
    /// `server_url`, with the test's changes to its configuration, for its primary endpoint. Also returns the shared
    /// queue telemetry the recipient's queue counts in.
    async fn test_recipient_pieces(
        server_url: &str, configure: impl FnOnce(&mut SharedConfiguration),
    ) -> (
        RecipientPieces<FrozenChunkedBytesBuffer>,
        SharedTransactionQueueTelemetry,
    ) {
        let (_, forwarder) =
            build_test_forwarder_with(server_url, LiveForwarderConfiguration::default(), configure).await;
        let TransactionForwarder {
            context,
            config,
            live,
            telemetry,
            metrics_builder,
            client,
            endpoint_name,
            endpoints,
            endpoint_request_mapper_factory,
            emitter,
            ..
        } = forwarder;
        let (route, endpoint) = endpoints
            .into_iter()
            .next()
            .expect("the forwarder has its primary endpoint")
            .into_parts();
        let url = endpoint.endpoint().to_string();
        let domain = endpoint.endpoint().origin().ascii_serialization();
        let shared = SharedTransactionQueueTelemetry::from_builder(&metrics_builder);
        let pieces = RecipientPieces {
            context,
            config,
            secrets: SecretsGateRefresher::new(live.secrets).gate.clone(),
            client,
            telemetry,
            queue_telemetry: TransactionQueueTelemetry::from_builder(&metrics_builder, &url, "team_d", shared.clone()),
            retry_telemetry: TransactionRetryTelemetry::from_builder(&metrics_builder, &domain),
            retry_cause_telemetry: RetryCauseTelemetry::from_builder(&metrics_builder, &domain),
            endpoint_name,
            route,
            endpoint,
            request_mapper: endpoint_request_mapper_factory,
            emitter,
            queue: None,
        };

        (pieces, shared)
    }

    /// A recipient whose I/O loop a test drives, as a delivery host would.
    struct TestRecipient {
        supervisor: TestComponentSupervisor,
        handle: RecipientHandle<FrozenChunkedBytesBuffer>,
        telemetry: ComponentTelemetry,
        shared_queue_telemetry: SharedTransactionQueueTelemetry,
    }

    impl TestRecipient {
        /// Spawns the recipient's loop under a running component supervisor.
        async fn spawn(
            spec: &RecipientSpec, pieces: RecipientPieces<FrozenChunkedBytesBuffer>,
            shared_queue_telemetry: SharedTransactionQueueTelemetry,
        ) -> Self {
            let telemetry = pieces.telemetry.clone();
            let supervisor = TestComponentSupervisor::start("test_forwarder").await;
            let spec = spec.clone();
            let handle = supervisor
                .scope(async move { RecipientLoop::spawn(&spec, pieces) })
                .await;
            supervisor.wait_for_children(1).await;

            Self {
                supervisor,
                handle,
                telemetry,
                shared_queue_telemetry,
            }
        }

        async fn send(&self, transaction: Transaction<FrozenChunkedBytesBuffer>) {
            self.handle
                .sender
                .send(transaction)
                .await
                .expect("the loop takes transactions");
        }

        fn shutdown(&self) {
            self.handle.close.shutdown();
        }

        fn reopen(&self) -> ReopenReply {
            self.handle.close.reopen()
        }

        /// Drops the loop's sender, and keeps the rest of its handle, so that only its input closing can stop it.
        fn close_input(&mut self) {
            let (unconnected, _) = mpsc::channel(1);
            drop(std::mem::replace(&mut self.handle.sender, unconnected));
        }

        fn retire_within(&self, drain: Duration) {
            self.handle.close.retire(tokio::time::Instant::now() + drain);
        }

        /// Waits for the loop's exit report and for its task to stop, and returns the report.
        async fn exit(self) -> RecipientExit {
            let Self {
                supervisor, mut handle, ..
            } = self;
            let exit = timeout(Duration::from_secs(10), &mut handle.exited)
                .await
                .expect("the loop exits in time")
                .expect("the loop reports its exit");
            drop(handle);
            let result = supervisor.shutdown().await;
            assert!(result.is_ok(), "the loop stopped on its own: {result:?}");

            exit
        }
    }

    /// Returns a report of a drain that delivered nothing.
    fn closed_with(persisted: u64, dropped: u64, deadline_reached: bool) -> RecipientExit {
        RecipientExit::Retired(DrainReport {
            delivered: 0,
            persisted,
            dropped,
            deadline_reached,
        })
    }

    /// A client that is never ready to send, as one whose connection pool failed.
    #[derive(Clone)]
    struct UnreadyClient;

    impl Service<Request<ClientBody>> for UnreadyClient {
        type Response = Response<Incoming>;
        type Error = BoxError;
        type Future = Pending<Result<Response<Incoming>, BoxError>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), BoxError>> {
            std::task::Poll::Ready(Err("the client cannot send".into()))
        }

        fn call(&mut self, _: Request<ClientBody>) -> Self::Future {
            pending()
        }
    }

    /// Returns the loop a recipient's pieces run, sending through `client` in place of the one they were built with.
    fn endpoint_loop_with_client<C: Clone>(
        spec: &RecipientSpec, pieces: RecipientPieces<FrozenChunkedBytesBuffer>, client: C,
    ) -> EndpointLoop<FrozenChunkedBytesBuffer, C> {
        let (_, endpoint_loop) = pieces.into_endpoint_loop(spec);
        let EndpointLoop {
            telemetry,
            queue_telemetry,
            retry_telemetry,
            retry_cause_telemetry,
            endpoint_name,
            route,
            endpoint,
            request_mapper,
            secrets,
            emitter,
            queue_id,
            queue,
            settings,
        } = endpoint_loop;

        EndpointLoop {
            telemetry,
            queue_telemetry,
            retry_telemetry,
            retry_cause_telemetry,
            endpoint_name,
            route,
            endpoint,
            request_mapper,
            secrets,
            emitter,
            queue_id,
            queue,
            settings: LoopSettings {
                limits: settings.limits,
                client: SharedClient::new(client),
            },
        }
    }

    /// Returns host settings like the test forwarder's, with the given high-priority and retry queue bounds.
    fn test_delivery_settings(endpoint_buffer_size: usize, queue_max_size_bytes: u64) -> DeliverySettings {
        DeliverySettings {
            endpoint_concurrency: 1,
            endpoint_concurrency_multiplier: 1,
            endpoint_buffer_size,
            request_timeout: Duration::from_secs(1),
            retry: RetrySettings {
                backoff_factor: 2.0,
                backoff_base: 0.001,
                backoff_max: 0.01,
                recovery_error_decrease_factor: 1,
                recovery_reset: false,
                queue_max_size_bytes,
                storage_max_size_bytes: 0,
                flush_to_disk_mem_ratio: 0.5,
                storage_max_disk_ratio: 0.8,
                outdated_file_in_days: 10,
            },
            proxy: ProxySettings::default(),
            http_protocol: ForwarderHttpProtocol::Auto,
            connection_reset_interval: Duration::ZERO,
            skip_ssl_validation: true,
            min_tls_version: TlsMinimumVersion::Tls12,
            tls_handshake_timeout: Duration::from_secs(10),
            api_key_validation_interval: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn a_retiring_recipient_dispatches_its_queued_work_before_it_exits_and_reports_what_it_delivered() {
        // The spike's `queue_drain_dispatches_accepted_transactions_after_input_closes`, generalized: a release that
        // arrives while every transaction is still queued must neither persist nor discard them, as a close at
        // shutdown would. The first request hangs until the client gives up, so every delivery comes after the
        // release.
        let (server_url, counter, _requests) = start_http_server(|nth| {
            if nth == 0 {
                Reply::Hang
            } else {
                Reply::Status(StatusCode::OK)
            }
        })
        .await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        for _ in 0..4 {
            recipient.send(build_test_transaction()).await;
        }
        recipient.retire_within(Duration::from_secs(30));

        let (telemetry, shared) = (recipient.telemetry.clone(), recipient.shared_queue_telemetry.clone());
        assert_eq!(
            recipient.exit().await,
            RecipientExit::Retired(DrainReport {
                delivered: 4,
                persisted: 0,
                dropped: 0,
                deadline_reached: false,
            })
        );
        assert_eq!(counter.load(Ordering::SeqCst), 5, "the hung request, then the four");
        // The loop released its share of the forwarder's series as it exited.
        assert_eq!(telemetry.domain_count(), 0);
        assert_eq!(shared.queue_count(), 0);
    }

    #[tokio::test]
    async fn a_recipient_closes_as_at_shutdown_at_its_deadline_on_request_or_with_its_retirement_closure() {
        // The server never answers, so one request is in flight until the client gives up and the others stay queued:
        // every close persists or drops all three once that request is back in the queue, and the report says which,
        // and whether the deadline passed.
        #[derive(Clone, Copy)]
        enum Close {
            Retire(Duration),
            Shutdown,
            InputClosed,
        }
        let cases = [
            (
                "the deadline, in memory",
                Close::Retire(Duration::from_millis(200)),
                RetireClosure::Native,
                false,
                closed_with(0, 3, true),
            ),
            (
                "the deadline, on disk",
                Close::Retire(Duration::from_millis(200)),
                RetireClosure::Attempt,
                true,
                closed_with(3, 0, true),
            ),
            (
                "a shutdown, in memory",
                Close::Shutdown,
                RetireClosure::Native,
                false,
                closed_with(0, 3, false),
            ),
            (
                "the input closing, on disk",
                Close::InputClosed,
                RetireClosure::Native,
                true,
                closed_with(3, 0, false),
            ),
            (
                "the persist closure",
                Close::Retire(Duration::from_secs(30)),
                RetireClosure::Persist,
                true,
                closed_with(3, 0, false),
            ),
            (
                "the discard closure",
                Close::Retire(Duration::from_secs(30)),
                RetireClosure::Discard,
                true,
                closed_with(0, 3, false),
            ),
        ];

        let runs = cases.map(|(case, close, on_retire, on_disk, expected)| async move {
            let (server_url, _counter, _requests) = start_http_server(|_| Reply::Hang).await;
            let storage = tempfile::tempdir().expect("temporary directory should be created");
            let (pieces, shared) = test_recipient_pieces(&server_url, |shared| {
                if on_disk {
                    persist_retries_under(shared, storage.path());
                }
            })
            .await;
            let policy = RecipientPolicy {
                on_retire,
                ..RecipientPolicy::default()
            };
            let spec = declared_recipient(&server_url, policy);
            let queue_dir = storage.path().join(pieces.queue_directory(&spec));
            let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
            for _ in 0..3 {
                recipient.send(build_test_transaction()).await;
            }
            let mut recipient = recipient;
            match close {
                Close::Retire(drain) => recipient.retire_within(drain),
                Close::Shutdown => recipient.shutdown(),
                Close::InputClosed => recipient.close_input(),
            }

            let persisted_files = match &expected {
                RecipientExit::Retired(report) => report.persisted as usize,
                RecipientExit::Failed(_) => 0,
            };
            assert_eq!(recipient.exit().await, expected, "{case}");
            assert_eq!(retry_files(&queue_dir).len(), persisted_files, "{case}");
        });
        futures::future::join_all(runs).await;
    }

    #[tokio::test]
    async fn a_reopened_recipient_takes_transactions_again_and_a_later_retirement_reports_only_its_own_drain() {
        // The first request hangs until the client gives up, so the loop is still busy when the release and the reopen
        // arrive; a loop that ignored the reopen would leave the second transaction unread in its input.
        let (server_url, counter, _requests) = start_http_server(|nth| {
            if nth == 0 {
                Reply::Hang
            } else {
                Reply::Status(StatusCode::OK)
            }
        })
        .await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        recipient.send(build_test_transaction()).await;
        recipient.retire_within(Duration::from_secs(30));
        // Let the idle loop see the release before it is cancelled.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(recipient.reopen(), ReopenReply::Reopened);
        recipient.send(build_test_transaction()).await;
        assert_eq!(
            wait_for_count_at_least(&counter, 3, Duration::from_secs(5)).await,
            3,
            "the hung request, its retry, and the transaction sent after the reopen"
        );

        recipient.retire_within(Duration::from_secs(30));
        assert!(matches!(
            recipient.exit().await,
            RecipientExit::Retired(DrainReport {
                delivered: 0..=1,
                persisted: 0,
                dropped: 0,
                deadline_reached: false,
            })
        ));
    }

    #[tokio::test]
    async fn a_recipient_that_fails_while_running_reports_the_failure_after_its_native_close() {
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (pieces, _) = test_recipient_pieces(&server_url, |_| {}).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let endpoint_loop = endpoint_loop_with_client(&spec, pieces, UnreadyClient);
        let (sender, input) = mpsc::channel(8);
        sender.send(build_test_transaction()).await.expect("the input has room");

        let exit = timeout(
            Duration::from_secs(10),
            run_endpoint_io_loop(input, endpoint_loop, None),
        )
        .await
        .expect("the loop stops on the error");
        match exit {
            RecipientExit::Failed(reason) => {
                assert!(reason.contains(server_url.trim_end_matches('/')), "{reason}");
                assert!(reason.contains("the client cannot send"), "{reason}");
            }
            RecipientExit::Retired(report) => panic!("a failure while running is not a retirement: {report:?}"),
        }
    }

    #[tokio::test]
    async fn a_recipient_that_fails_while_retiring_ends_its_drain_and_reports_what_it_dropped() {
        // A release set before the loop starts applies at once: the loop queues what is already in its input, and the
        // error ends the drain as its deadline would.
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (pieces, _) = test_recipient_pieces(&server_url, |_| {}).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let endpoint_loop = endpoint_loop_with_client(&spec, pieces, UnreadyClient);
        let (sender, input) = mpsc::channel(8);
        for _ in 0..2 {
            sender.send(build_test_transaction()).await.expect("the input has room");
        }
        let (close, close_requests) = close_channel(&sender);
        close.retire(tokio::time::Instant::now() + Duration::from_secs(30));
        let (_settings, setting_changes) = watch::channel(endpoint_loop.settings.clone());
        let control = LoopControl {
            close: close_requests,
            settings: setting_changes,
        };

        let exit = timeout(
            Duration::from_secs(10),
            run_endpoint_io_loop(input, endpoint_loop, Some(control)),
        )
        .await
        .expect("the loop stops on the error");
        assert_eq!(exit, closed_with(0, 2, false));
    }

    #[tokio::test]
    async fn a_recipient_loop_stopped_before_it_finished_reports_that_it_was_interrupted() {
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (pieces, _) = test_recipient_pieces(&server_url, |_| {}).await;
        let (recipient_loop, handle) =
            RecipientLoop::prepare(&declared_recipient(&server_url, RecipientPolicy::default()), pieces);
        assert_eq!(recipient_loop.name(), "recipient_io_loop_team_d");

        drop(recipient_loop.into_task());
        assert_eq!(handle.close.reopen(), ReopenReply::Exited);
        assert_eq!(
            handle.exited.await.expect("the loop reports its exit"),
            RecipientExit::Failed(INTERRUPTED_EXIT.to_string())
        );
    }

    #[tokio::test]
    async fn published_queue_bounds_apply_to_a_running_recipient() {
        // The server never answers, so the six transactions pile up: one in flight and the rest queued. With the
        // bounds a publication sets, one high-priority and one retry queue entry, the close finds the in-flight one
        // back in the retry queue, and the high-priority one evicting it: two dropped there, where the configured
        // bounds would have dropped all six. Either bound alone is checked: the host's, and the recipient's own.
        let cases = [
            (
                "the host's bounds",
                test_delivery_settings(1, 12),
                RecipientPolicy::default(),
            ),
            (
                "the recipient's own bound",
                test_delivery_settings(1, 15 * 1024 * 1024),
                RecipientPolicy {
                    queue_bytes: Some(12),
                    ..RecipientPolicy::default()
                },
            ),
        ];

        for (case, settings, policy) in cases {
            let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
            let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
            let spec = declared_recipient(&server_url, RecipientPolicy::default());
            let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
            for _ in 0..6 {
                recipient.send(build_test_transaction()).await;
            }
            // With a request in flight, the loop serves the publication even once it is closing.
            assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
            recipient
                .handle
                .settings
                .publish(&settings, &RecipientSpec { policy, ..spec.clone() });
            recipient.shutdown();

            assert_eq!(recipient.exit().await, closed_with(0, 2, false), "{case}");
        }
    }

    #[tokio::test]
    async fn a_publication_keeps_the_requests_in_flight_within_the_concurrency_limit() {
        // The request in flight never finishes, so with a limit of one, a service rebuilt by a publication that counted
        // its requests afresh would send the second transaction at once.
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
        recipient.send(build_test_transaction()).await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);

        recipient
            .handle
            .settings
            .publish(&test_delivery_settings(4, 1024 * 1024), &spec);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "the second request waits for the first"
        );

        recipient.shutdown();
        assert_eq!(recipient.exit().await, closed_with(0, 2, false));
    }

    #[test]
    fn a_concurrency_limit_rises_at_once_and_falls_as_requests_in_flight_finish() {
        let mut budget = ConcurrencyBudget::new(2);
        let held = [
            Arc::clone(&budget.semaphore)
                .try_acquire_owned()
                .expect("a permit is free"),
            Arc::clone(&budget.semaphore)
                .try_acquire_owned()
                .expect("a permit is free"),
        ];

        // Both permits are held, so a lower limit waits for them.
        budget.resize(1);
        assert_eq!(budget.owed, 1);
        let [first, second] = held;
        drop(first);
        budget.settle();
        assert_eq!((budget.owed, budget.semaphore.available_permits()), (0, 0));
        drop(second);
        assert_eq!(budget.semaphore.available_permits(), 1);

        // A higher limit adds permits at once, after repaying what a lower one still owes.
        let held = Arc::clone(&budget.semaphore)
            .try_acquire_owned()
            .expect("a permit is free");
        budget.resize(0);
        assert_eq!(budget.owed, 1);
        budget.resize(3);
        assert_eq!((budget.owed, budget.semaphore.available_permits()), (0, 2));
        drop(held);
        assert_eq!(budget.semaphore.available_permits(), 3);
    }

    #[tokio::test]
    async fn a_publication_that_changes_nothing_does_not_wake_the_loop() {
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (pieces, _) = test_recipient_pieces(&server_url, |_| {}).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let (_, endpoint_loop) = pieces.into_endpoint_loop(&spec);
        let (settings, mut setting_changes) = watch::channel(endpoint_loop.settings.clone());
        let sender = RecipientSettingsSender {
            settings: Some(settings),
        };

        let host = test_delivery_settings(4, 1024);
        sender.publish(&host, &spec);
        assert!(setting_changes.has_changed().expect("the sender is alive"));
        let published = setting_changes.borrow_and_update().limits.clone();
        assert_eq!(published.buffer_size, 4);
        assert_eq!(published.queue_bytes(), 1024);
        // What the host fixes when it is built stays.
        assert_eq!(
            published.retry.storage_path(),
            endpoint_loop.settings.limits.retry.storage_path()
        );

        sender.publish(&host, &spec);
        assert!(!setting_changes.has_changed().expect("the sender is alive"));
        sender.swap_client(endpoint_loop.settings.client.get().1);
        assert!(setting_changes.has_changed().expect("the sender is alive"));
    }

    #[tokio::test]
    async fn a_recipient_takes_every_payload_its_host_routes_to_it_whatever_variants_it_accepts() {
        // The recipient takes V2 series only. Its host decides which recipients take a payload (by the slots a lease
        // resolved, or by each recipient's current protocol), so the loop sends both V3 series payloads, with
        // resolved recipients and without, where a native endpoint would filter them.
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Status(StatusCode::OK)).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        let v3_series = || {
            let (mut metadata, request) = build_test_transaction().into_parts();
            metadata.payload_info = Some(MetricsPayloadInfo::v3_series());
            Transaction::reassemble(metadata, request)
        };
        recipient.send(v3_series()).await;
        recipient.send(v3_series()).await;
        recipient.retire_within(Duration::from_secs(30));

        assert!(matches!(
            recipient.exit().await,
            RecipientExit::Retired(DrainReport { dropped: 0, .. })
        ));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_declared_recipients_queue_directory_follows_its_component_endpoint_and_credential_name() {
        // Plan 4.9: a declared recipient's directory is the native form, hashed over the component, the canonical
        // endpoint, and the credential's name, so a recipient declared again adopts its backlog, and one declared with
        // another credential gets a queue of its own. The literal fails if the hash or its inputs change.
        let context = test_component_context();
        let endpoint =
            ResolvedEndpoint::from_raw_endpoint("http://127.0.0.1:8080", "key-a").expect("endpoint resolves");
        let spec = declared_recipient("http://127.0.0.1:8080", RecipientPolicy::default());
        assert_eq!(
            recipient_queue_id(&context, &spec, &endpoint),
            "test_forwarder/127.0.0.1/8cc43cb04d258c04"
        );

        // The key a credential holds is not part of the name; its name is.
        let rotated = ResolvedEndpoint::from_raw_endpoint("http://127.0.0.1:8080", "key-b").expect("endpoint resolves");
        assert_eq!(
            recipient_queue_id(&context, &spec, &rotated),
            recipient_queue_id(&context, &spec, &endpoint)
        );
        let other_credential = RecipientSpec {
            origin: RecipientOrigin::Declared {
                name: "team_d".into(),
                credential: "team_e".into(),
            },
            ..spec.clone()
        };
        assert_ne!(
            recipient_queue_id(&context, &other_credential, &endpoint),
            recipient_queue_id(&context, &spec, &endpoint)
        );
    }

    #[tokio::test]
    async fn a_recipient_queue_opened_at_prepare_fails_on_an_unusable_directory_and_is_used_by_its_loop() {
        let (server_url, _counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());

        // Retry storage under a file cannot hold the queue's directory.
        let storage = tempfile::tempdir().expect("temporary directory should be created");
        let not_a_directory = storage.path().join("file");
        std::fs::write(&not_a_directory, b"").expect("the file should be written");
        let (mut pieces, _) =
            test_recipient_pieces(&server_url, |shared| persist_retries_under(shared, &not_a_directory)).await;
        let error = pieces
            .open_queue(&spec)
            .await
            .expect_err("the directory cannot be created");
        assert!(error.to_string().contains(spec.id.as_ref()), "{error}");

        // A usable one is opened at prepare, and the loop persists into it.
        let (mut pieces, shared) =
            test_recipient_pieces(&server_url, |shared| persist_retries_under(shared, storage.path())).await;
        pieces.open_queue(&spec).await.expect("the directory is usable");
        let queue_dir = storage.path().join(pieces.queue_directory(&spec));
        assert!(queue_dir.is_dir(), "opening creates the directory");
        let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
        recipient.send(build_test_transaction()).await;
        recipient.shutdown();
        assert_eq!(recipient.exit().await, closed_with(1, 0, false));
        assert_eq!(retry_files(&queue_dir).len(), 1);
    }

    /// Returns the sum of a counter's series that carry `labels`, whatever their other labels.
    fn counter_total(recorder: &TestRecorder, name: &str, labels: &[(&str, &str)]) -> u64 {
        recorder
            .counters()
            .into_iter()
            .filter(|(key, _)| {
                key.name() == name
                    && labels.iter().all(|(label, value)| {
                        key.labels()
                            .any(|carried| carried.key() == *label && carried.value() == *value)
                    })
            })
            .map(|(_, value)| value)
            .sum()
    }

    /// Waits until the loop the handle holds is still running `wait` from now: it reports no exit meanwhile.
    async fn assert_still_running(handle: &mut RecipientHandle<FrozenChunkedBytesBuffer>, wait: Duration) {
        let exited = timeout(wait, &mut handle.exited).await;
        assert!(exited.is_err(), "the loop exited: {exited:?}");
    }

    #[tokio::test]
    async fn a_reopen_cancels_a_retirement_that_closes_at_its_deadline_or_with_its_closure() {
        // The first request hangs until the client gives up, a second after the release, so each retirement closes
        // while a request is in flight: at its deadline, or at once with its closure. A reopen must cancel the close,
        // as it cancels a drain: the loop retries what was in flight, takes a new transaction, and delivers both,
        // rather than persisting or dropping them.
        let cases = [
            ("the deadline", RetireClosure::Native, Duration::from_millis(100)),
            ("the persist closure", RetireClosure::Persist, Duration::from_secs(30)),
            ("the discard closure", RetireClosure::Discard, Duration::from_secs(30)),
        ];

        let runs = cases.map(|(case, on_retire, drain)| async move {
            let status = Arc::new(AtomicU16::new(0));
            let (server_url, counter, _requests) = start_switchable_http_server(Arc::clone(&status)).await;
            let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
            let policy = RecipientPolicy {
                on_retire,
                ..RecipientPolicy::default()
            };
            let mut recipient = TestRecipient::spawn(&declared_recipient(&server_url, policy), pieces, shared).await;
            recipient.send(build_test_transaction()).await;
            assert_eq!(
                wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await,
                1,
                "{case}"
            );

            recipient.retire_within(drain);
            // The loop is closing, and waits for the request in flight.
            assert_still_running(&mut recipient.handle, Duration::from_millis(300)).await;
            assert_eq!(recipient.reopen(), ReopenReply::Reopened, "{case}");
            status.store(StatusCode::OK.as_u16(), Ordering::SeqCst);
            recipient.send(build_test_transaction()).await;
            assert_eq!(
                wait_for_count_at_least(&counter, 3, Duration::from_secs(5)).await,
                3,
                "{case}: the hung request, its retry, and the transaction sent after the reopen"
            );
            assert_still_running(&mut recipient.handle, Duration::from_millis(100)).await;

            recipient.retire_within(Duration::from_secs(30));
            let exit = recipient.exit().await;
            assert!(
                matches!(
                    exit,
                    RecipientExit::Retired(DrainReport {
                        delivered: 0..=1,
                        persisted: 0,
                        dropped: 0,
                        deadline_reached: false,
                    })
                ),
                "{case}: {exit:?}"
            );
        });
        futures::future::join_all(runs).await;
    }

    #[tokio::test]
    async fn a_reopen_is_refused_once_the_loops_close_is_final() {
        // A close at shutdown is final at once, even while a request is in flight; a retirement's is final once the
        // loop exits. Either way the host learns it from the reopen's reply, and waits for the exit.
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let mut recipient = TestRecipient::spawn(&spec, pieces, shared).await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        recipient.shutdown();
        assert_eq!(recipient.reopen(), ReopenReply::Exited);
        assert_still_running(&mut recipient.handle, Duration::from_millis(100)).await;
        recipient.retire_within(Duration::from_secs(30));
        assert_eq!(recipient.handle.close.current(), CloseRequest::Shutdown);
        assert_eq!(recipient.exit().await, closed_with(0, 1, false));

        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let mut recipient = TestRecipient::spawn(&spec, pieces, shared).await;
        recipient.retire_within(Duration::from_secs(30));
        let exit = timeout(Duration::from_secs(5), &mut recipient.handle.exited)
            .await
            .expect("an empty loop retires at once")
            .expect("the loop reports its exit");
        assert_eq!(exit, closed_with(0, 0, false));
        assert_eq!(recipient.reopen(), ReopenReply::Exited);
        let result = recipient.supervisor.shutdown().await;
        assert!(result.is_ok(), "the loop stopped on its own: {result:?}");
    }

    #[test]
    fn a_reopen_racing_the_loops_final_close_either_cancels_it_or_is_refused() {
        // The reopen and the loop's final close take one lock: the reopen is answered as reopened exactly when the
        // loop finds it, and goes on running, and as exited exactly when the loop's close is final.
        let (sender, _input) = mpsc::channel::<Transaction<FrozenChunkedBytesBuffer>>(1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut outcomes = [0usize; 2];
        for _ in 0..500 {
            let (closer, mut requests) = close_channel(&sender);
            closer.retire(deadline);
            assert_eq!(requests.take().request, CloseRequest::Retire { deadline });

            let barrier = std::sync::Barrier::new(2);
            let (reply, sealed) = std::thread::scope(|scope| {
                let reopen = scope.spawn(|| {
                    barrier.wait();
                    closer.reopen()
                });
                let seal = scope.spawn(|| {
                    barrier.wait();
                    requests.seal(true)
                });
                (reopen.join().unwrap(), seal.join().unwrap())
            });
            match (reply, sealed) {
                (ReopenReply::Reopened, Err(change)) => {
                    assert!(change.reopened);
                    assert_eq!(change.request, CloseRequest::None);
                    outcomes[0] += 1;
                }
                (ReopenReply::Exited, Ok(())) => {
                    assert_eq!(closer.current(), CloseRequest::Retire { deadline });
                    outcomes[1] += 1;
                }
                (reply, sealed) => panic!("the reopen and the close crossed: {reply:?}, {sealed:?}"),
            }
        }
        assert_eq!(outcomes.iter().sum::<usize>(), 500);

        // A final close is final whatever came before it.
        let (closer, mut requests) = close_channel(&sender);
        closer.retire(deadline);
        assert_eq!(closer.reopen(), ReopenReply::Reopened);
        requests.seal(false).expect("a final close is never cancelled");
        assert_eq!(closer.reopen(), ReopenReply::Exited);
    }

    #[tokio::test]
    async fn a_reopen_and_a_release_that_follow_each_other_before_the_loop_looks_are_both_applied() {
        // An abort returns a reopened recipient to draining with its original deadline, and the host routed to it
        // meanwhile. The loop sees only the second release, but must apply the reopen before it: queue what was sent
        // meanwhile and drain it, rather than leave it unread in its input.
        let (server_url, counter, _requests) = start_http_server(|nth| {
            if nth == 0 {
                Reply::Hang
            } else {
                Reply::Status(StatusCode::OK)
            }
        })
        .await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        recipient.handle.close.retire(deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // No await between these, so the loop cannot run before the second release.
        assert_eq!(recipient.reopen(), ReopenReply::Reopened);
        recipient
            .handle
            .sender
            .try_send(build_test_transaction())
            .expect("the input has room");
        recipient.handle.close.retire(deadline);

        assert_eq!(
            recipient.exit().await,
            RecipientExit::Retired(DrainReport {
                delivered: 2,
                persisted: 0,
                dropped: 0,
                deadline_reached: false,
            })
        );
    }

    #[tokio::test]
    async fn a_closing_retirement_reopened_and_released_again_before_the_loop_looks_drains_to_its_new_deadline() {
        // The retirement reached its deadline and closes, waiting for the request in flight. A reopen and a new
        // release follow each other before the loop looks, so it sees only the release; it must still apply the reopen
        // first, which cancels the close, and drain to the new deadline, rather than go on closing.
        let status = Arc::new(AtomicU16::new(0));
        let (server_url, counter, _requests) = start_switchable_http_server(Arc::clone(&status)).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let mut recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        recipient.retire_within(Duration::from_millis(100));
        assert_still_running(&mut recipient.handle, Duration::from_millis(300)).await;

        // No await between these, so the loop cannot run before the second release.
        assert_eq!(recipient.reopen(), ReopenReply::Reopened);
        recipient
            .handle
            .sender
            .try_send(build_test_transaction())
            .expect("the input has room");
        recipient.retire_within(Duration::from_secs(30));
        status.store(StatusCode::OK.as_u16(), Ordering::SeqCst);

        assert_eq!(
            recipient.exit().await,
            RecipientExit::Retired(DrainReport {
                delivered: 2,
                persisted: 0,
                dropped: 0,
                deadline_reached: false,
            })
        );
    }

    #[tokio::test]
    async fn a_loop_that_closes_queues_what_its_input_still_holds_and_counts_it() {
        // The loop takes nothing from its input while it closes, but a host may still have sent to it: what the input
        // holds when the close is final is queued, then dropped and counted with the rest, never lost uncounted.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        recipient.shutdown();
        // Let the loop see the shutdown, so that it no longer reads its input.
        tokio::time::sleep(Duration::from_millis(100)).await;
        recipient.send(build_test_transaction()).await;

        assert_eq!(recipient.exit().await, closed_with(0, 2, false));
        assert_eq!(counter_total(&recorder, "component_items_dropped_total", &[]), 2);
    }

    #[tokio::test]
    async fn a_reopen_after_every_sender_of_the_input_dropped_is_refused_and_the_drain_ends() {
        // A reopened loop with no input could take nothing and would never exit, so the reopen is refused, and the
        // drain goes on to its end.
        let (server_url, counter, _requests) = start_http_server(|nth| {
            if nth == 0 {
                Reply::Hang
            } else {
                Reply::Status(StatusCode::OK)
            }
        })
        .await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let mut recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        recipient.retire_within(Duration::from_secs(30));
        recipient.close_input();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(recipient.reopen(), ReopenReply::Exited);
        assert_eq!(
            recipient.exit().await,
            RecipientExit::Retired(DrainReport {
                delivered: 1,
                persisted: 0,
                dropped: 0,
                deadline_reached: false,
            })
        );
    }

    #[tokio::test]
    async fn a_retiring_loop_whose_host_dropped_its_handle_closes_as_at_shutdown() {
        // Every request hangs until the client gives up, so a drain would run to its deadline. A host that has gone
        // closes the loop as at shutdown instead: it finishes the request in flight, and its queued work is persisted
        // or dropped at once.
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |_| {}).await;
        let recipient = TestRecipient::spawn(
            &declared_recipient(&server_url, RecipientPolicy::default()),
            pieces,
            shared,
        )
        .await;
        recipient.send(build_test_transaction()).await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        recipient.retire_within(Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let TestRecipient { supervisor, handle, .. } = recipient;
        let RecipientHandle {
            sender,
            close,
            settings,
            exited,
        } = handle;
        drop((sender, close, settings));
        let exit = timeout(Duration::from_secs(5), exited)
            .await
            .expect("the loop closes without waiting for its deadline")
            .expect("the loop reports its exit");
        assert_eq!(exit, closed_with(0, 2, false));
        let result = supervisor.shutdown().await;
        assert!(result.is_ok(), "the loop stopped on its own: {result:?}");
    }

    #[tokio::test]
    async fn a_lowered_concurrency_limit_holds_while_work_is_queued() {
        // The server holds each request for a while, so work stays queued throughout, and the requests finish one at a
        // time. A loop that went on waiting for permits would take each one a finishing request returns before the
        // lowered limit could remove it, and stay above the limit for as long as the backlog lasted.
        let (server_url, in_flight) =
            start_slow_http_server(|nth| Duration::from_millis(100 + 30 * (nth % 4) as u64)).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |shared| {
            shared.endpoints.forwarder.max_concurrent_requests = 4;
            shared.endpoints.forwarder.high_prio_buffer_size = 100;
        })
        .await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
        for _ in 0..40 {
            recipient.send(build_test_transaction()).await;
        }
        in_flight.wait_until(|held| held == 4).await;

        // A limit of one, once the four requests in flight at the publication finish: the loop sends nothing before
        // then, so each one served from then on was the only one in flight.
        recipient
            .handle
            .settings
            .publish(&test_delivery_settings(100, 15 * 1024 * 1024), &spec);
        in_flight.reset();
        in_flight.wait_until_served(4).await;
        in_flight.reset();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let (most, served) = in_flight.most_and_served();
        assert!(most <= 1, "a limit of one let {most} requests be in flight at once");
        assert!(served >= 2, "the loop went on sending: {served} served");

        recipient.shutdown();
        let _ = recipient.exit().await;
    }

    #[tokio::test]
    async fn a_publication_or_a_swapped_client_keeps_the_circuit_breaker_open() {
        // Every request fails and is retried after a backoff of three seconds, during which the circuit breaker holds
        // the queued work back. A publication, or a swapped client, during the backoff must not send it at once, as a
        // circuit breaker built afresh would.
        for swap_client in [false, true] {
            let (server_url, counter, _requests) =
                start_http_server(|_| Reply::Status(StatusCode::SERVICE_UNAVAILABLE)).await;
            let (pieces, shared) = test_recipient_pieces(&server_url, |shared| {
                shared.endpoints.forwarder.backoff_base = 3.0;
                shared.endpoints.forwarder.backoff_max = 3.0;
            })
            .await;
            let client = pieces.client.clone();
            let spec = declared_recipient(&server_url, RecipientPolicy::default());
            let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
            recipient.send(build_test_transaction()).await;
            recipient.send(build_test_transaction()).await;
            assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
            // Let the loop take the failure, which opens the circuit breaker.
            tokio::time::sleep(Duration::from_millis(100)).await;

            if swap_client {
                recipient.handle.settings.swap_client(client);
            } else {
                recipient
                    .handle
                    .settings
                    .publish(&test_delivery_settings(4, 1024 * 1024), &spec);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "the queued work waits out the backoff (client swapped: {swap_client})"
            );

            recipient.shutdown();
            assert_eq!(recipient.exit().await, closed_with(0, 2, false));
        }
    }

    /// A client that counts the requests sent through it, and fails each one, which the loop retries.
    #[derive(Clone)]
    struct CountingClient(Arc<AtomicUsize>);

    impl Service<Request<ClientBody>> for CountingClient {
        type Response = Response<Incoming>;
        type Error = BoxError;
        type Future = std::future::Ready<Result<Response<Incoming>, BoxError>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), BoxError>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<ClientBody>) -> Self::Future {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err("the connection was refused".into()))
        }
    }

    #[tokio::test]
    async fn a_swapped_client_carries_the_requests_after_it() {
        // Every request fails and is retried within milliseconds, so the loop keeps sending: through the first client
        // until the swap, and through the second from then on.
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (pieces, _) = test_recipient_pieces(&server_url, |_| {}).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let endpoint_loop = endpoint_loop_with_client(&spec, pieces, CountingClient(Arc::clone(&first)));
        let (settings, setting_changes) = watch::channel(endpoint_loop.settings.clone());
        let (sender, input) = mpsc::channel(8);
        let (close, close_requests) = close_channel(&sender);
        let control = LoopControl {
            close: close_requests,
            settings: setting_changes,
        };
        sender.send(build_test_transaction()).await.expect("the input has room");
        let running = tokio::spawn(run_endpoint_io_loop(input, endpoint_loop, Some(control)));
        assert!(wait_for_count_at_least(&first, 1, Duration::from_secs(5)).await >= 1);

        settings.send_modify(|current| current.client.set(CountingClient(Arc::clone(&second))));
        assert!(wait_for_count_at_least(&second, 3, Duration::from_secs(5)).await >= 3);
        let sent_through_first = first.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            first.load(Ordering::SeqCst),
            sent_through_first,
            "the first client sends nothing more"
        );

        close.shutdown();
        let exit = timeout(Duration::from_secs(5), running)
            .await
            .expect("the loop closes")
            .expect("the loop's task runs to its end");
        assert_eq!(exit, closed_with(0, 1, false));
    }

    #[tokio::test]
    async fn transactions_larger_than_the_retry_queue_bound_are_dropped_and_counted() {
        // The recipient's own bound is below the size of every transaction, so the retry queue refuses each one: as it
        // overflows the high-priority queue, as the request in flight comes back to be retried, and as the close moves
        // the high-priority queue into it. Every one is dropped and counted, and the close goes on past the first.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let (pieces, shared) = test_recipient_pieces(&server_url, |shared| {
            shared.endpoints.forwarder.high_prio_buffer_size = 1;
        })
        .await;
        let policy = RecipientPolicy {
            queue_bytes: Some(10),
            ..RecipientPolicy::default()
        };
        let recipient = TestRecipient::spawn(&declared_recipient(&server_url, policy), pieces, shared).await;
        for _ in 0..3 {
            recipient.send(build_test_transaction()).await;
        }
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);
        recipient.shutdown();

        let exit = recipient.exit().await;
        assert!(
            matches!(exit, RecipientExit::Retired(DrainReport { persisted: 0, .. })),
            "{exit:?}"
        );
        assert_eq!(counter_total(&recorder, "component_items_dropped_total", &[]), 3);
        assert_eq!(
            counter_total(
                &recorder,
                "component_events_dropped_total",
                &[("drop_reason", "queue_limit")]
            ),
            3
        );
    }

    #[tokio::test]
    async fn a_publication_that_turns_disk_persistence_off_leaves_a_disk_backed_queue_persisting() {
        // Turning persistence on or off is restart-only: a publication that says never to spill, or that sets no disk
        // storage, leaves the running queue persisting what it holds at its close.
        let mut no_storage = test_delivery_settings(4, 15 * 1024 * 1024);
        no_storage.retry.storage_max_size_bytes = 0;
        let mut storage = test_delivery_settings(4, 15 * 1024 * 1024);
        storage.retry.storage_max_size_bytes = 1024 * 1024;
        storage.retry.storage_max_disk_ratio = 1.0;
        let never = RecipientPolicy {
            spill: SpillPolicy::Never,
            ..RecipientPolicy::default()
        };
        let cases = [
            ("a policy that never spills", storage, never),
            ("no disk storage", no_storage, RecipientPolicy::default()),
        ];

        for (case, settings, policy) in cases {
            let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
            let storage_dir = tempfile::tempdir().expect("temporary directory should be created");
            let (pieces, shared) =
                test_recipient_pieces(&server_url, |shared| persist_retries_under(shared, storage_dir.path())).await;
            let spec = declared_recipient(&server_url, RecipientPolicy::default());
            let queue_dir = storage_dir.path().join(pieces.queue_directory(&spec));
            let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
            recipient.send(build_test_transaction()).await;
            recipient.send(build_test_transaction()).await;
            assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);

            recipient
                .handle
                .settings
                .publish(&settings, &RecipientSpec { policy, ..spec.clone() });
            recipient.shutdown();
            assert_eq!(recipient.exit().await, closed_with(2, 0, false), "{case}");
            assert_eq!(retry_files(&queue_dir).len(), 2, "{case}");
        }
    }

    #[tokio::test]
    async fn a_disk_limit_published_after_persistence_was_turned_off_applies_to_the_queue_that_kept_it() {
        // The queue kept its persistence through a publication that turned it off, so a later one that spills again
        // applies its disk limit to it: one byte, which no transaction fits, so the close drops both and counts them.
        let (server_url, counter, _requests) = start_http_server(|_| Reply::Hang).await;
        let storage_dir = tempfile::tempdir().expect("temporary directory should be created");
        let (pieces, shared) =
            test_recipient_pieces(&server_url, |shared| persist_retries_under(shared, storage_dir.path())).await;
        let spec = declared_recipient(&server_url, RecipientPolicy::default());
        let queue_dir = storage_dir.path().join(pieces.queue_directory(&spec));
        let recipient = TestRecipient::spawn(&spec, pieces, shared).await;
        recipient.send(build_test_transaction()).await;
        recipient.send(build_test_transaction()).await;
        assert_eq!(wait_for_count_at_least(&counter, 1, Duration::from_secs(5)).await, 1);

        let mut settings = test_delivery_settings(4, 15 * 1024 * 1024);
        settings.retry.storage_max_disk_ratio = 1.0;
        settings.retry.storage_max_size_bytes = 1024 * 1024;
        let never = RecipientPolicy {
            spill: SpillPolicy::Never,
            ..RecipientPolicy::default()
        };
        recipient.handle.settings.publish(
            &settings,
            &RecipientSpec {
                policy: never,
                ..spec.clone()
            },
        );
        // Let the loop apply the first publication before the second replaces it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        settings.retry.storage_max_size_bytes = 1;
        recipient.handle.settings.publish(&settings, &spec);
        recipient.shutdown();

        assert_eq!(recipient.exit().await, closed_with(0, 2, false));
        assert!(retry_files(&queue_dir).is_empty());
    }

    #[test]
    fn a_reopen_of_a_loop_whose_input_closed_closes_it_as_at_shutdown() {
        // A reopened loop with no input could take nothing and would never exit.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let reopen = CloseChange {
            reopened: true,
            request: CloseRequest::None,
        };
        for input_open in [true, false] {
            let mut phase = Phase::Retiring { deadline };
            let mut retirement = Retirement {
                requested: true,
                ..Retirement::default()
            };
            apply_close_change(&mut phase, &mut retirement, reopen, RetireClosure::Native, input_open);
            let expected = if input_open {
                Phase::Running
            } else {
                Phase::Closing { reopenable: false }
            };
            assert_eq!(phase, expected, "input open: {input_open}");
            assert!(!retirement.requested);
        }
    }

    #[tokio::test]
    async fn a_detached_handle_carries_its_requests_to_the_ends_its_loop_holds() {
        let (handle, mut detached) = RecipientHandle::<FrozenChunkedBytesBuffer>::detached(1);
        handle
            .sender
            .try_send(build_test_transaction())
            .expect("the input has room");
        assert!(detached.transactions.try_recv().is_ok());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        handle.close.retire(deadline);
        let request = timeout(
            Duration::from_secs(5),
            detached
                .close
                .wait_for(|request| matches!(request, CloseRequest::Retire { .. })),
        )
        .await
        .expect("the release reaches the loop's end");
        assert_eq!(request, CloseRequest::Retire { deadline });
        assert_eq!(handle.close.reopen(), ReopenReply::Reopened);
        assert_eq!(detached.close.current(), CloseRequest::None);

        let RecipientHandle { close, exited, .. } = handle;
        drop(close);
        assert_eq!(
            detached.close.current(),
            CloseRequest::Shutdown,
            "a dropped closer shuts the loop down"
        );
        detached
            .exit
            .send(closed_with(0, 0, false))
            .expect("the handle waits for the exit");
        assert_eq!(exited.await.expect("the exit is reported"), closed_with(0, 0, false));
    }

    #[tokio::test]
    async fn forwarder_io_runs_as_supervised_children() {
        // The main I/O loop, the per-endpoint I/O loop, and the secrets gate refresher are supervised children rather
        // than detached tasks, so all of them are accounted for while the forwarder runs.
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::OK]).await;
        let (_, forwarder) = build_test_forwarder(&server_url, Live::new_fixed(Secrets::default())).await;

        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;

        // One main I/O loop, one endpoint I/O loop for the single configured endpoint, and the secrets gate refresher.
        // No API key refresher: this forwarder follows no keys configuration.
        supervisor.wait_for_children(3).await;

        // Closing the transaction channel is the whole shutdown protocol. The main I/O loop finishes and drops its
        // endpoint sender, so the endpoint loop drains and flushes, and both are gone -- without either of them having
        // waited on the other, and without this test having waited on either.
        handle.shutdown();
        supervisor.wait_for_children(1).await;

        // The refresher is what remains, deliberately: it has no terminal condition of its own, so it is brutally
        // stopped when the supervisor drops it rather than being waited on.
        let result = supervisor.shutdown().await;
        assert!(result.is_ok(), "every child should have stopped on its own: {result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forwarder_counts_successful_body_bytes_once_after_retries() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let (server_url, counter) = start_recording_http_server(vec![
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::OK,
        ])
        .await;
        let (_, forwarder) = build_test_forwarder(&server_url, Live::new_fixed(Secrets::default())).await;

        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        let observed = wait_for_count_at_least(&counter, 3, Duration::from_secs(3)).await;

        // The per-transaction telemetry asserted below is recorded by the endpoint I/O loop, so the drain has to finish
        // before any of it can be read back.
        drain_test_forwarder(supervisor, handle).await;
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

        let component_metric_key = |name: &str| {
            metrics::Key::from_parts(
                name.to_string(),
                vec![
                    metrics::Label::new("component_id", "test_forwarder"),
                    metrics::Label::new("component_type", "forwarder"),
                ],
            )
        };
        assert_eq!(
            recorder.counter(component_metric_key("component_events_sent_total")),
            Some(1)
        );
        // Regression: socket-write accounting counted every retry. Successful bytes must count the request body once.
        assert_eq!(
            recorder.counter(component_metric_key("component_bytes_sent_total")),
            Some(12)
        );
    }

    #[tokio::test]
    async fn a_rotated_api_key_reaches_the_requests_a_spawned_forwarder_sends() {
        let (server_url, _counter, mut requests) =
            start_recording_http_server_with_requests(vec![StatusCode::OK]).await;
        let live = LiveConfiguration::new(config_with_api_key(TEST_API_KEY));
        let following = LiveForwarderConfiguration {
            api_keys: live.api_keys(),
            ..Default::default()
        };
        let (_, forwarder) = build_test_forwarder_following(&server_url, following).await;

        // Spawning the forwarder is what starts key refreshing in production.
        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");
        let request = timeout(Duration::from_secs(3), requests.recv())
            .await
            .expect("the server should record the request")
            .expect("the server should be running");
        assert_eq!(Some(TEST_API_KEY), recorded_api_key(&request));

        live.store(config_with_api_key("rotated-api-key"));

        // A key is installed by a task of its own, so keep sending until a request carries the new one.
        let rotated = timeout(Duration::from_secs(3), async {
            loop {
                handle
                    .send_transaction(build_test_transaction())
                    .await
                    .expect("send should succeed");
                let request = requests.recv().await.expect("the server should be running");
                if recorded_api_key(&request) == Some("rotated-api-key") {
                    return;
                }
            }
        })
        .await;

        drain_test_forwarder(supervisor, handle).await;
        assert!(rotated.is_ok(), "a rotated API key should reach the request path");
    }

    #[tokio::test]
    async fn forwarder_retries_403_when_secrets_in_use() {
        // The server returns 403 to the first request and 200 to every subsequent request; the forwarder must drive
        // at least one retry to observe the second request.
        let (server_url, counter) = start_recording_http_server(vec![StatusCode::FORBIDDEN, StatusCode::OK]).await;
        let (_, forwarder) = build_test_forwarder(&server_url, secrets_in_use()).await;

        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
        handle
            .send_transaction(build_test_transaction())
            .await
            .expect("send should succeed");

        let observed = wait_for_count_at_least(&counter, 2, Duration::from_secs(3)).await;
        drain_test_forwarder(supervisor, handle).await;

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
        let (dataspace, forwarder) = build_test_forwarder(&server_url, Live::new_fixed(Secrets::default())).await;
        let mut events = dataspace.subscribe::<DiagnosticEvent>(IdentifierFilter::all());

        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
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

        drain_test_forwarder(supervisor, handle).await;
    }

    #[tokio::test]
    async fn forwarder_emits_invalid_api_key_diagnostic_on_403_when_secrets_in_use() {
        // With secrets management enabled, a 403 is classified as retriable, so the retry circuit breaker converts it
        // into a `Retry` error that never reaches the response-handling arm of the I/O loop. The diagnostic must still
        // be emitted, because it is produced by the inspection layer sitting below the retry circuit breaker. Without
        // that layer, the rejected key would be retried indefinitely without ever notifying anyone.
        let (server_url, _counter) = start_recording_http_server(vec![StatusCode::FORBIDDEN]).await;
        let secrets = secrets_in_use();

        // Build a forwarder and then subscribe to diagnostic events from the dataspace it's attached to.
        let (dataspace, forwarder) = build_test_forwarder(&server_url, secrets).await;
        let mut events = dataspace.subscribe::<DiagnosticEvent>(IdentifierFilter::all());

        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        let handle = spawn_test_forwarder(&supervisor, forwarder).await;
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

        drain_test_forwarder(supervisor, handle).await;
    }
}
