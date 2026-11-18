//! APM source.
//!
//! Listens on TCP and/or a Unix domain socket where the trace-agent normally listens. Requests for endpoints the source
//! does not handle itself are forwarded to the relocated trace-agent, so the documentation, metrics, and logs still call
//! it the relay.
//!
//! With [`trace_intake`](ApmConfiguration::trace_intake) set, the source serves `/v1.0/traces`, `/v0.4/traces`,
//! `/v0.3/traces` and `/v0.2/traces` itself, in every media type. It emits the traces it decodes on its `traces`
//! output, and its receiver telemetry on its `metrics` output. Without it, the source has no `metrics` output and emits
//! nothing on `traces`, and every trace payload is forwarded to the trace-agent.
//!
//! The relay serves `/info` itself, and so owns `Datadog-Agent-State`, the hash of the `/info` body that every
//! response carries: it replaces the trace-agent's on the responses it forwards.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{ConnectInfo, Request as AxumRequest, State};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use backon::{ExponentialBuilder, Retryable as _};
use datadog_agent_apm::semantics::SemanticRegistryProvider;
use datadog_agent_commons::ipc::client::{fetch_agent_version, RunningAgentVersion};
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use http::{HeaderName, HeaderValue};
use metrics::Counter;
use saluki_api::APIHandler;
use saluki_common::sync::live::Live;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::components::sources::{Source, SourceBuilder, SourceContext};
use saluki_core::components::BuildContext;
use saluki_core::data_model::event::trace::Trace;
use saluki_core::data_model::event::{Event, EventType};
use saluki_core::runtime;
use saluki_core::topology::OutputDefinition;
use saluki_env::workload::{CaptureEntityResolver, EntityId};
use saluki_env::WorkloadProvider;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::dns::resolve_host;
use saluki_io::net::server::http::HttpServer;
use saluki_io::net::{ConnectionAddress, ListenAddress};
use saluki_metrics::static_metrics;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio::{pin, select};
use tracing::{debug, error, warn};

mod container_tags;
use self::container_tags::{ContainerTagsHasher, CONTAINER_TAGS_HASH_HEADER};

mod forwarding;

mod info;
use self::info::Info;

mod intake;
pub use self::intake::TraceIntakeConfiguration;
use self::intake::{EndpointVersion, TraceIntake};

mod media_type;

mod opm;
pub use self::info::{
    InfoCacheObfuscation, InfoConfiguration, InfoJsonObfuscation, InfoObfuscation, InfoReplaceRule, InfoTagFilter,
    InfoTagFilters,
};
use self::opm::OrgPropagationMarkerFetcher;

mod proxy;
use self::proxy::{Destination, TraceAgentProxy};

mod receiver_telemetry;
use self::receiver_telemetry::{ReceiverTelemetry, WINDOW};

mod tracer_flare;
pub use self::tracer_flare::TracerFlareConfiguration;
use self::tracer_flare::TracerFlareProxy;

/// Header a tracer sends its container ID in.
const CONTAINER_ID_HEADER: HeaderName = HeaderName::from_static("datadog-container-id");

/// Header a tracer sends its origin in (for example, `ci-<container ID>` or `in-<cgroup inode>`), which the trace-agent
/// prefers over `Datadog-Container-ID`.
const ENTITY_ID_HEADER: HeaderName = HeaderName::from_static("datadog-entity-id");

/// Delay before the first retry of the org propagation marker fetch, doubled for each later retry.
const ORG_PROP_MARKER_BASE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Delay before the first retry of the Agent version fetch.
const AGENT_VERSION_MIN_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Longest delay between retries of the Agent version fetch.
const AGENT_VERSION_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Configuration for the APM source.
///
/// Holds plain values, populated by the binary that assembles the topology, plus an optional resolver for the
/// container of a tracer connected over the Unix domain socket.
///
/// `receiver_socket` is an empty `String` rather than an `Option<String>`, so that "disabled" has a single
/// representation.
#[derive(Clone)]
pub struct ApmConfiguration {
    /// Unix domain socket path the relay listens on.
    ///
    /// Empty disables the Unix domain socket listener.
    pub receiver_socket: String,

    /// URL of the trace-agent that every request not handled by ADP is proxied to: `http://host:port` or
    /// `unix:///path/to/socket`.
    pub proxy_destination: String,

    /// Resolver for mapping the process ID of a tracer connected over the Unix domain socket to its container.
    ///
    /// The trace-agent finds the container of a tracer that sends no container headers from the peer credentials of
    /// its Unix domain socket connection, but behind the relay those credentials are ADP's. With a resolver, the relay
    /// resolves the container from the tracer's credentials itself, and passes it on as `Datadog-Container-ID`.
    pub capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,

    /// The host address to bind the APM relay TCP listeners to.
    ///
    /// Accepts either an IP literal (for example, `192.168.1.50`, `::1`) or a hostname that resolves via DNS (for example, `agent.internal`).
    /// Ignored when `non_local_traffic` is `true`, and binds to `127.0.0.1` when unset.
    pub bind_host: Option<String>,

    /// Whether the TCP listener accepts traffic from other hosts.
    ///
    /// When `true`, the listener binds every interface, overriding `bind_host`, as the trace-agent does.
    pub non_local_traffic: bool,

    /// TCP port the trace receiver listens on.
    ///
    /// `0` disables the TCP listener.
    pub receiver_port: u16,

    /// Settings for forwarding tracer flares to Datadog.
    pub tracer_flare: TracerFlareConfiguration,

    /// The trace-agent settings the relay reports on `/info`.
    pub info: InfoConfiguration,

    /// Workload provider the tags of a tracer's container come from, for the `Datadog-Container-Tags-Hash` of `/info`.
    ///
    /// `None` when ADP runs without the core Agent: every tracer then gets the hash of no tags.
    pub workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,

    /// Client configuration for the core Agent's IPC API, which reports the Agent version on `/info`.
    ///
    /// `None` when ADP runs without the core Agent: `/info` then reports an empty version.
    pub agent_client: Option<RemoteAgentClientConfiguration>,

    /// Semantic registry the base peer tags on `/info` come from.
    pub semantic_registry: SemanticRegistryProvider,

    /// URL of Datadog's `/api/v2/validate`, which identifies the organization of the API key for the org propagation
    /// marker on `/info`.
    ///
    /// The request uses the HTTP client settings and the API key of [`tracer_flare`](Self::tracer_flare).
    pub validate_endpoint: String,

    /// Settings of the native trace intake, which serves the trace endpoints rather than forwarding them.
    ///
    /// `None` forwards every trace payload to the trace-agent.
    pub trace_intake: Option<TraceIntakeConfiguration>,
}

#[static_metrics(prefix = component, labels(component_id, component_type))]
#[derive(Clone)]
struct Metrics {
    /// Requests forwarded to the trace-agent that it answered, whatever the status of its answer.
    requests_relayed_total: Counter,
    /// Requests forwarded to the trace-agent that it never answered, such as when it was unreachable.
    requests_failed_total: Counter,
    /// Requests the relay handled itself rather than forwarding to the trace-agent, whatever the outcome.
    requests_served_locally_total: Counter,
}

impl ApmConfiguration {
    async fn tcp_listen_address(&self) -> Result<Option<ListenAddress>, GenericError> {
        if self.receiver_port == 0 {
            return Ok(None);
        };

        let listen_address = if self.non_local_traffic {
            [0, 0, 0, 0].into()
        } else if let Some(bind_host) = &self.bind_host {
            resolve_host(bind_host)
                .await
                .error_context("Could not resolve bind_host for the APM relay.")?
        } else {
            [127, 0, 0, 1].into()
        };

        Ok(Some(ListenAddress::Tcp(SocketAddr::new(
            listen_address,
            self.receiver_port,
        ))))
    }

    fn uds_listen_address(&self) -> Result<Option<ListenAddress>, GenericError> {
        if self.receiver_socket.is_empty() {
            return Ok(None);
        }

        ListenAddress::try_from(format!("unix://{}", self.receiver_socket))
            .map(Some)
            .map_err(|e| {
                generic_error!(
                    "Invalid APM relay Unix domain socket path `{}`: {e}",
                    self.receiver_socket
                )
            })
    }
}

/// Returns the relay listener that `destination` reaches, if any.
///
/// This catches the obvious misconfigurations, such as the trace-agent's default port used for both, not every route
/// back to the relay: a listener on every interface is also reached through the host's other addresses, for example.
async fn listener_reached_by<'a>(
    destination: &Destination, tcp_endpoint: Option<&'a ListenAddress>, uds_endpoint: Option<&'a ListenAddress>,
) -> Option<&'a ListenAddress> {
    // Only Unix domain socket destinations can reach the Unix domain socket listener.
    #[cfg(not(unix))]
    let _ = uds_endpoint;

    match destination {
        Destination::Tcp { host, port } => {
            let endpoint = tcp_endpoint?;
            let ListenAddress::Tcp(listener) = endpoint else {
                return None;
            };
            // A destination that doesn't resolve yet can't be told apart from the listener, so it is let through.
            let mut destinations = tokio::net::lookup_host((host.as_str(), *port)).await.ok()?;
            destinations
                .any(|destination| tcp_reaches(destination, *listener))
                .then_some(endpoint)
        }
        #[cfg(unix)]
        Destination::Unix(path) => {
            uds_endpoint.filter(|endpoint| matches!(endpoint, ListenAddress::Unix(listener) if listener == path))
        }
    }
}

/// Returns whether connecting to `destination` reaches a TCP listener bound to `listener`.
///
/// A listener on the unspecified address accepts connections to any local address, and connecting to the unspecified
/// address connects to the local host, so either side being unspecified matches a loopback address of the same family.
fn tcp_reaches(destination: SocketAddr, listener: SocketAddr) -> bool {
    let is_local = |ip: IpAddr| ip.is_loopback() || ip.is_unspecified();
    let (destination_ip, listener_ip) = (destination.ip(), listener.ip());

    destination.port() == listener.port()
        && (destination_ip == listener_ip
            || ((destination_ip.is_unspecified() || listener_ip.is_unspecified())
                && destination_ip.is_ipv4() == listener_ip.is_ipv4()
                && is_local(destination_ip)
                && is_local(listener_ip)))
}

impl MemoryBounds for ApmConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<Apm>("component struct")
            .with_single_value::<ApmState>("component struct");
        if self.trace_intake.is_some() {
            builder
                .minimum()
                .with_map::<receiver_telemetry::ReceiverTags, receiver_telemetry::TagStats>(
                    "receiver telemetry window",
                    receiver_telemetry::MAX_TAG_KEYS + 1,
                );
        }
    }
}

#[async_trait]
impl SourceBuilder for ApmConfiguration {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> =
            LazyLock::new(|| vec![OutputDefinition::named_output("traces", EventType::Trace)]);
        static INTAKE_OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("traces", EventType::Trace),
                OutputDefinition::named_output("metrics", EventType::Metric),
            ]
        });
        if self.trace_intake.is_some() {
            &INTAKE_OUTPUTS
        } else {
            &OUTPUTS
        }
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let tcp_endpoint = self.tcp_listen_address().await?;
        let uds_endpoint = self.uds_listen_address()?;

        if tcp_endpoint.is_none() && uds_endpoint.is_none() {
            return Err(generic_error!(
                "APM relay requires at least one of `apm_config.receiver_port` or `apm_config.receiver_socket` to be configured."
            ));
        }

        let proxy = TraceAgentProxy::from_destination(&self.proxy_destination)?;
        if let Some(listener) =
            listener_reached_by(proxy.destination(), tcp_endpoint.as_ref(), uds_endpoint.as_ref()).await
        {
            return Err(generic_error!(
                "APM proxy destination `{}` is the relay's own listener `{listener}`, so every request would be proxied back to the relay. Set `data_plane.experimental.apm.proxy_destination` to the trace-agent's address.",
                self.proxy_destination
            ));
        }

        let tracer_flare = TracerFlareProxy::from_config(&self.tracer_flare)?;
        let org_prop_marker = OrgPropagationMarkerFetcher::new(
            &self.tracer_flare.forwarder,
            self.validate_endpoint.clone(),
            tracer_flare.api_key(),
        )?;

        // Cloned before `/info` takes its first snapshot, so that any update published after it is waited on.
        let semantic_registry_updates = self.semantic_registry.clone();
        let info = Info::new(self.info.clone(), self.semantic_registry.clone());

        // Decoded payloads wait in a short queue while the source dispatches them, so that a slow pipeline holds back
        // the decoders, as the trace-agent's unbuffered channel (`apm_config.trace_buffer` defaults to 0) does. Each
        // decoder can have one payload waiting.
        let (intake, traces_rx) = match self.trace_intake.clone() {
            Some(config) => {
                let (traces_tx, traces_rx) = mpsc::channel(config.decoders.get());
                let intake = TraceIntake::new(config, traces_tx);
                let telemetry = intake.telemetry();
                (Some(intake), Some((traces_rx, telemetry)))
            }
            None => (None, None),
        };

        Ok(Box::new(Apm {
            intake,
            traces_rx,
            tcp_endpoint,
            uds_endpoint,
            proxy,
            tracer_flare,
            tracer_flare_api_key: self.tracer_flare.api_key.clone(),
            info,
            semantic_registry_updates,
            agent_client: self.agent_client.clone(),
            org_prop_marker,
            container_tags: ContainerTagsHasher::new(
                self.capture_entity_resolver.clone(),
                self.workload_provider.clone(),
            ),
            capture_entity_resolver: self.capture_entity_resolver.clone(),
            metrics: Metrics::new(context.component_id(), context.component_type().as_category_str()),
        }))
    }
}

/// APM source.
///
/// Receives requests via HTTP, on TCP and/or a Unix domain socket, and proxies them to the trace-agent.
pub struct Apm {
    intake: Option<TraceIntake>,
    traces_rx: Option<(mpsc::Receiver<Vec<Trace>>, Arc<ReceiverTelemetry>)>,
    tcp_endpoint: Option<ListenAddress>,
    uds_endpoint: Option<ListenAddress>,
    proxy: TraceAgentProxy,
    tracer_flare: TracerFlareProxy,
    tracer_flare_api_key: Live<String>,
    info: Info,
    semantic_registry_updates: SemanticRegistryProvider,
    agent_client: Option<RemoteAgentClientConfiguration>,
    org_prop_marker: OrgPropagationMarkerFetcher,
    container_tags: ContainerTagsHasher,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    metrics: Metrics,
}

#[async_trait]
impl Source for Apm {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let Self {
            intake,
            traces_rx,
            tcp_endpoint,
            uds_endpoint,
            proxy,
            tracer_flare,
            mut tracer_flare_api_key,
            info,
            mut semantic_registry_updates,
            agent_client,
            org_prop_marker,
            container_tags,
            capture_entity_resolver,
            metrics,
        } = *self;

        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let worker_pool = context.topology_context().global_thread_pool().clone();
        let api_handler = ApmAPIHandler::new(ApmState {
            intake,
            metrics,
            proxy,
            tracer_flare: tracer_flare.clone(),
            info: info.clone(),
            container_tags,
            capture_entity_resolver,
        });
        let router = api_handler
            .generate_routes()
            .with_state(api_handler.generate_initial_state());

        // The listeners are needed: one that fails to bind, for example because the trace-agent still owns its address,
        // fails the relay rather than leaving it ready without serving that endpoint.
        if let Some(endpoint) = tcp_endpoint {
            debug!(%endpoint, "Binding APM relay TCP listener.");
            let server = HttpServer::from_listen_address(endpoint)
                .with_routes(router.clone())
                .with_worker_pool(worker_pool.clone());
            runtime::nested_supervisor(server.into_supervisor())
                .needed()
                .stops_with_owner()
                .spawn();
        }
        if let Some(endpoint) = uds_endpoint {
            debug!(%endpoint, "Binding APM relay Unix domain socket listener.");
            let server = HttpServer::from_listen_address(endpoint)
                .with_routes(router.clone())
                .with_worker_pool(worker_pool);
            runtime::nested_supervisor(server.into_supervisor())
                .needed()
                .stops_with_owner()
                .spawn();
        }

        // The dispatcher stops once the servers exit and drop the intake's senders, so that the traces of every
        // payload already accepted reach the pipeline. Thus, it has no timeout of its own, and is halted with the source
        // if the component's shutdown timeout expires.
        if let Some((traces_rx, telemetry)) = traces_rx {
            runtime::worker(
                "traces_dispatcher",
                dispatch_traces(traces_rx, telemetry, context.clone()),
            )
            .with_shutdown_timeout(Duration::MAX)
            .needed()
            .spawn();
        }

        health.mark_ready();
        debug!("APM relay started.");

        // `/info` reports an empty version until the core Agent reports its own.
        let agent_version = fetch_agent_version_with_retry(agent_client);
        pin!(agent_version);
        let mut agent_version_pending = true;

        // As in the trace-agent, the marker is fetched once, with the API key current at each attempt.
        let org_prop_marker = org_prop_marker.fetch_with_retries(ORG_PROP_MARKER_BASE_RETRY_DELAY);
        pin!(org_prop_marker);
        let mut org_prop_marker_pending = true;

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break
                },
                _ = health.live() => continue,
                api_key = tracer_flare_api_key.changed() => tracer_flare.refresh_api_key(&api_key),
                // Peer tags follow the registry, so tracers must see its updates, unlike the trace-agent's `/info`.
                _ = semantic_registry_updates.changed() => info.refresh_semantic_registry(),
                version = &mut agent_version, if agent_version_pending => {
                    agent_version_pending = false;
                    if let Some(version) = version {
                        debug!(version = %version.version, "Fetched the Agent version.");
                        info.set_agent_version(version);
                    }
                },
                marker = &mut org_prop_marker, if org_prop_marker_pending => {
                    org_prop_marker_pending = false;
                    if let Some(marker) = marker {
                        info.set_org_prop_marker(marker);
                    }
                },
            }
        }

        debug!("APM relay stopped.");

        Ok(())
    }
}

/// Dispatches the traces of each accepted payload on the `traces` output, and the receiver telemetry of each window on
/// the `metrics` output, until every sender is gone.
async fn dispatch_traces(
    mut traces_rx: mpsc::Receiver<Vec<Trace>>, telemetry: Arc<ReceiverTelemetry>, context: SourceContext,
) -> Result<(), GenericError> {
    // A dispatch that blocks past a tick delays the next window rather than bursting short ones.
    let mut window = tokio::time::interval_at(tokio::time::Instant::now() + WINDOW, WINDOW);
    window.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        select! {
            maybe_traces = traces_rx.recv() => {
                let Some(traces) = maybe_traces else { break };
                let mut dispatcher = context
                    .dispatcher()
                    .buffered_named("traces")
                    .error_context("The traces output should exist.")?;
                for trace in traces {
                    if let Err(error) = dispatcher.push(Event::Trace(trace)).await {
                        error!(%error, "Failed to dispatch trace event.");
                    }
                }
                if let Err(error) = dispatcher.flush().await {
                    error!(%error, "Failed to flush trace events.");
                }
            },
            _ = window.tick() => dispatch_receiver_telemetry(&telemetry, &context).await?,
        }
    }
    // The servers are gone, so the last window is complete.
    dispatch_receiver_telemetry(&telemetry, &context).await?;
    debug!("Traces dispatcher stopped.");
    Ok(())
}

async fn dispatch_receiver_telemetry(
    telemetry: &ReceiverTelemetry, context: &SourceContext,
) -> Result<(), GenericError> {
    let events = telemetry.take_window_events();
    if events.is_empty() {
        return Ok(());
    }
    let dispatcher = context
        .dispatcher()
        .buffered_named("metrics")
        .error_context("The metrics output should exist.")?;
    if let Err(error) = dispatcher.send_all(events).await {
        error!(%error, "Failed to dispatch receiver telemetry.");
    }
    Ok(())
}

/// Fetches the version of the core Agent, retrying until the Agent answers.
///
/// Returns `None` without a client configuration, when ADP runs without the core Agent.
async fn fetch_agent_version_with_retry(
    agent_client: Option<RemoteAgentClientConfiguration>,
) -> Option<RunningAgentVersion> {
    let agent_client = agent_client?;
    let fetch = || fetch_agent_version(&agent_client);
    let version = fetch
        .retry(
            ExponentialBuilder::default()
                .with_min_delay(AGENT_VERSION_MIN_RETRY_DELAY)
                .with_max_delay(AGENT_VERSION_MAX_RETRY_DELAY)
                .without_max_times(),
        )
        .notify(|error, delay| {
            warn!(%error, "Failed to fetch the Agent version for `/info`. Retrying in {delay:?}...");
        })
        .await;
    version.ok()
}

/// Shared HTTP handler state.
#[derive(Clone)]
struct ApmState {
    intake: Option<TraceIntake>,
    proxy: TraceAgentProxy,
    tracer_flare: TracerFlareProxy,
    info: Info,
    container_tags: ContainerTagsHasher,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>,
    metrics: Metrics,
}

struct ApmAPIHandler {
    state: ApmState,
}

impl ApmAPIHandler {
    fn new(state: ApmState) -> Self {
        Self { state }
    }

    async fn proxy_request(
        State(ApmState {
            proxy,
            info,
            capture_entity_resolver,
            metrics,
            ..
        }): State<ApmState>,
        mut request: AxumRequest,
    ) -> Response {
        if let Some(resolver) = capture_entity_resolver.as_deref() {
            add_peer_container_id(&mut request, resolver);
        }
        match proxy.forward(request).await {
            Ok(mut response) => {
                metrics.requests_relayed_total().increment(1);
                info.replace_headers(response.headers_mut());
                response
            }
            Err(error) => {
                metrics.requests_failed_total().increment(1);
                error.into_response()
            }
        }
    }

    async fn handle_info(State(ApmState { info, .. }): State<ApmState>) -> Response {
        info.response()
    }

    async fn handle_services() -> Response {
        "OK\n".into_response()
    }

    async fn handle_tracer_flare(
        State(ApmState { tracer_flare, .. }): State<ApmState>, request: AxumRequest,
    ) -> Response {
        tracer_flare.forward(request).await.into_response()
    }

    /// Counts a request for an endpoint the relay serves itself.
    async fn count_served_locally(
        State(ApmState { metrics, .. }): State<ApmState>, request: AxumRequest, next: Next,
    ) -> Response {
        metrics.requests_served_locally_total().increment(1);
        next.run(request).await
    }

    /// Sets `Datadog-Agent-State` and `Datadog-Agent-Version` on a response the relay served itself.
    async fn set_agent_headers(
        State(ApmState { info, .. }): State<ApmState>, request: AxumRequest, next: Next,
    ) -> Response {
        let mut response = next.run(request).await;
        info.set_headers(response.headers_mut());
        response
    }

    /// Sets `Datadog-Container-Tags-Hash` to the hash of the tags of the container of the tracer that sent `request`.
    async fn add_container_tags_hash(
        State(ApmState { container_tags, .. }): State<ApmState>, request: AxumRequest, next: Next,
    ) -> Response {
        let hash = container_tags.header_value(
            request.headers(),
            request.extensions().get::<ConnectInfo<ConnectionAddress>>(),
        );
        let mut response = next.run(request).await;
        response.headers_mut().insert(CONTAINER_TAGS_HASH_HEADER, hash);
        response
    }

    async fn handle_traces_v02(state: State<ApmState>, request: AxumRequest) -> Response {
        Self::handle_traces(state, EndpointVersion::V02, request).await
    }

    async fn handle_traces_v03(state: State<ApmState>, request: AxumRequest) -> Response {
        Self::handle_traces(state, EndpointVersion::V03, request).await
    }

    async fn handle_traces_v04(state: State<ApmState>, request: AxumRequest) -> Response {
        Self::handle_traces(state, EndpointVersion::V04, request).await
    }

    async fn handle_traces_v1(state: State<ApmState>, request: AxumRequest) -> Response {
        Self::handle_traces(state, EndpointVersion::V10, request).await
    }

    async fn handle_traces(State(state): State<ApmState>, version: EndpointVersion, request: AxumRequest) -> Response {
        let intake = state
            .intake
            .as_ref()
            .expect("trace routes are only registered with the native trace intake");
        intake.handle(version, request, &state.container_tags).await
    }
}

/// Sets `Datadog-Container-ID` to the container of the tracer that sent `request`, when the tracer is connected over the
/// Unix domain socket and sent no container headers of its own.
///
/// This stands in for the trace-agent's own lookup from the connection's peer credentials, which behind the relay
/// would find ADP rather than the tracer.
fn add_peer_container_id(request: &mut AxumRequest, resolver: &(dyn CaptureEntityResolver + Send + Sync)) {
    let headers = request.headers();
    if headers.contains_key(CONTAINER_ID_HEADER) || headers.contains_key(ENTITY_ID_HEADER) {
        return;
    }

    let Some(process_id) = request
        .extensions()
        .get::<ConnectInfo<ConnectionAddress>>()
        .and_then(|ConnectInfo(addr)| addr.process_credentials())
        .and_then(|creds| u32::try_from(creds.pid).ok())
    else {
        return;
    };

    match resolver.resolve_container_entity_for_live_pid(process_id) {
        Some(EntityId::Container(container_id)) => match HeaderValue::from_str(&container_id) {
            Ok(value) => {
                request.headers_mut().insert(CONTAINER_ID_HEADER, value);
            }
            Err(_) => debug!(process_id, %container_id, "Resolved container ID is not a valid header value."),
        },
        Some(entity_id) => debug!(process_id, %entity_id, "Tracer process resolved to a non-container entity."),
        None => debug!(process_id, "Could not resolve the container of the tracer process."),
    }
}

impl APIHandler for ApmAPIHandler {
    type State = ApmState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    /// Forwards requests for endpoints not handled here to the trace-agent.
    ///
    /// As in the trace-agent, only `/info` carries `Datadog-Container-Tags-Hash`, and every other endpoint served here
    /// carries `Datadog-Agent-State` and `Datadog-Agent-Version` instead.
    fn generate_routes(&self) -> Router<Self::State> {
        let state = &self.state;
        let with_agent_headers = Router::new();
        // Without the native trace intake, the trace endpoints are forwarded to the trace-agent like any other.
        let with_agent_headers = if state.intake.is_some() {
            with_agent_headers
                .route("/v0.2/traces", any(Self::handle_traces_v02))
                .route("/v0.3/traces", any(Self::handle_traces_v03))
                .route("/v0.4/traces", any(Self::handle_traces_v04))
                .route("/v1.0/traces", any(Self::handle_traces_v1))
        } else {
            with_agent_headers
        };
        let with_agent_headers = with_agent_headers
            .route("/services", any(Self::handle_services))
            .route("/v0.1/services", any(Self::handle_services))
            .route("/v0.2/services", any(Self::handle_services))
            .route("/v0.3/services", any(Self::handle_services))
            .route("/v0.4/services", any(Self::handle_services))
            .route("/tracer_flare/v1", any(Self::handle_tracer_flare))
            .route_layer(from_fn_with_state(state.clone(), Self::set_agent_headers));

        Router::new()
            .route(
                "/info",
                any(Self::handle_info).layer(from_fn_with_state(state.clone(), Self::add_container_tags_hash)),
            )
            .merge(with_agent_headers)
            .route_layer(from_fn_with_state(state.clone(), Self::count_served_locally))
            .fallback(Self::proxy_request)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::OnceLock;
    use std::time::Duration;

    use bytes::Bytes;
    use http::{Request, StatusCode};
    use saluki_core::components::test_util::TestComponentDriver;
    use saluki_core::runtime::state::{DataspaceUpdate, IdentifierFilter};
    use saluki_core::runtime::test_util::TestSupervisor;
    use saluki_io::net::{BoundListenAddress, ProcessCredentials};
    #[cfg(unix)]
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    #[cfg(unix)]
    use tokio::net::UnixStream;
    use tower::service_fn;
    use tower::ServiceExt;

    use super::*;
    use crate::common::datadog::test_util::init_tls;

    /// A proxy whose trace-agent is not listening, so a proxied request is answered `502 Bad Gateway`. That makes
    /// "was this request proxied?" observable without running a trace-agent.
    fn unreachable_proxy() -> TraceAgentProxy {
        TraceAgentProxy::from_destination("http://127.0.0.1:1").expect("destination should parse")
    }

    fn build_router(state: ApmState) -> Router {
        let api_handler = ApmAPIHandler::new(state);
        api_handler
            .generate_routes()
            .with_state(api_handler.generate_initial_state())
    }

    fn test_info() -> Info {
        Info::new(InfoConfiguration::default(), SemanticRegistryProvider::default())
    }

    /// A flare proxy whose intake is not listening, so a forwarded flare is answered `502 Bad Gateway`.
    fn unreachable_tracer_flare() -> TracerFlareProxy {
        tracer_flare::plain_http_proxy("http://127.0.0.1:1", "test-api-key")
    }

    fn test_router() -> Router {
        build_router(ApmState {
            intake: None,
            proxy: unreachable_proxy(),
            tracer_flare: unreachable_tracer_flare(),
            info: test_info(),
            container_tags: ContainerTagsHasher::default(),
            capture_entity_resolver: None,
            metrics: Metrics::new("dd_traces_in", "sources"),
        })
    }

    /// A router with the native trace intake, whose trace-agent is unreachable.
    fn native_intake_router(info: Info) -> (Router, mpsc::Receiver<Vec<Trace>>) {
        let (traces_tx, traces_rx) = mpsc::channel(1);
        let router = build_router(ApmState {
            intake: Some(TraceIntake::new(
                TraceIntakeConfiguration {
                    max_request_bytes: 1024,
                    decoders: std::num::NonZeroUsize::MIN,
                    decoder_timeout: Duration::from_secs(1),
                    receiver_timeout: Duration::from_secs(5),
                    reject_with_too_many_requests: false,
                    histogram: crate::transforms::HistogramConfiguration::default(),
                    service_rates: crate::transforms::ServiceRates::default(),
                },
                traces_tx,
            )),
            proxy: unreachable_proxy(),
            tracer_flare: unreachable_tracer_flare(),
            info,
            container_tags: ContainerTagsHasher::default(),
            capture_entity_resolver: None,
            metrics: Metrics::new("dd_traces_in", "sources"),
        });
        (router, traces_rx)
    }

    #[tokio::test]
    async fn the_native_intake_serves_v0_traces_in_every_media_type() {
        let send = |router: Router, path: &'static str, content_type: Option<&'static str>, body: &'static [u8]| async move {
            let mut builder = Request::builder().method("PUT").uri(path);
            if let Some(content_type) = content_type {
                builder = builder.header("Content-Type", content_type);
            }
            let response = router
                .oneshot(builder.body(axum::body::Body::from(body)).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, body)
        };
        // Empty v0.4 payloads: an array with no traces.
        const MSGPACK: &[u8] = &[0x90];
        const JSON: &[u8] = b"[]";
        const RATES: &str = "{\"rate_by_service\":{}}\n";
        let (router, _traces) = native_intake_router(test_info());

        let cases = [
            (
                "/v0.4/traces",
                Some("application/msgpack"),
                MSGPACK,
                StatusCode::OK,
                Some(RATES),
            ),
            (
                "/v0.3/traces",
                Some("application/msgpack"),
                MSGPACK,
                StatusCode::OK,
                Some("OK\n"),
            ),
            (
                "/v0.2/traces",
                Some("application/msgpack"),
                MSGPACK,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                None,
            ),
            (
                "/v0.4/traces",
                Some("application/json"),
                JSON,
                StatusCode::OK,
                Some(RATES),
            ),
            ("/v0.3/traces", None, JSON, StatusCode::OK, Some("OK\n")),
            ("/v0.2/traces", Some("text/json"), JSON, StatusCode::OK, Some("OK\n")),
            ("/v0.4/traces", Some("text/plain"), JSON, StatusCode::OK, Some(RATES)),
        ];
        for (path, content_type, body, expected_status, expected_body) in cases {
            let (status, body) = send(router.clone(), path, content_type, body).await;
            assert_eq!(status, expected_status, "{path} {content_type:?}");
            if let Some(expected_body) = expected_body {
                assert_eq!(body, expected_body.as_bytes(), "{path} {content_type:?}");
            }
        }
    }

    #[tokio::test]
    async fn serves_v1_traces_only_with_the_native_trace_intake() {
        let send = |router: Router| async move {
            router
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/v1.0/traces")
                        // An empty tracer payload: a MessagePack map with no fields.
                        .body(axum::body::Body::from(vec![0x80]))
                        .unwrap(),
                )
                .await
                .unwrap()
        };

        // Without it, the request is proxied, and the trace-agent is unreachable.
        let response = send(test_router()).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let info = test_info();
        let (router, _traces_rx) = native_intake_router(info.clone());
        let response = send(router).await;
        assert_eq!(response.status(), StatusCode::OK);
        // Served responses carry the relay's own agent state, as every trace-agent response does.
        let mut expected = http::HeaderMap::new();
        info.set_headers(&mut expected);
        for header in [info::AGENT_STATE_HEADER, info::AGENT_VERSION_HEADER] {
            assert_eq!(response.headers().get(&header), expected.get(&header), "{header}");
        }
    }

    /// Resolves every process to the same container.
    struct FixedContainerResolver;

    impl CaptureEntityResolver for FixedContainerResolver {
        fn resolve_container_entity_for_live_pid(&self, _process_id: u32) -> Option<EntityId> {
            Some(EntityId::Container("tracer-container".into()))
        }
    }

    /// Builds a request as the relay's server hands it over: with the address of the peer that sent it.
    fn request_from(peer: ConnectionAddress, headers: &[(&str, &str)]) -> AxumRequest {
        let mut builder = Request::builder().method("POST").uri("/v0.4/traces");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(axum::body::Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    }

    fn unix_peer() -> ConnectionAddress {
        ProcessCredentials {
            pid: 1234,
            uid: 0,
            gid: 0,
        }
        .into()
    }

    #[test]
    fn sets_the_container_id_of_a_unix_socket_peer() {
        let mut request = request_from(unix_peer(), &[]);

        add_peer_container_id(&mut request, &FixedContainerResolver);

        assert_eq!(request.headers()[CONTAINER_ID_HEADER], "tracer-container");
    }

    #[test]
    fn keeps_the_container_headers_a_tracer_sent() {
        let mut request = request_from(unix_peer(), &[("Datadog-Container-ID", "from-tracer")]);
        add_peer_container_id(&mut request, &FixedContainerResolver);
        assert_eq!(request.headers()[CONTAINER_ID_HEADER], "from-tracer");

        let mut request = request_from(unix_peer(), &[("Datadog-Entity-ID", "in-42")]);
        add_peer_container_id(&mut request, &FixedContainerResolver);
        assert!(!request.headers().contains_key(CONTAINER_ID_HEADER));
    }

    #[test]
    fn leaves_tcp_requests_alone() {
        let mut request = request_from(ConnectionAddress::from(SocketAddr::from(([127, 0, 0, 1], 4242))), &[]);

        add_peer_container_id(&mut request, &FixedContainerResolver);

        assert!(!request.headers().contains_key(CONTAINER_ID_HEADER));
    }

    #[tokio::test]
    async fn counts_a_request_the_trace_agent_never_received_as_failed() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let router = test_router();

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/v0.4/traces")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("component_id", "dd_traces_in"),
                    metrics::Label::new("component_type", "sources"),
                ],
            )
        };
        assert_eq!(recorder.counter(metric_key("component_requests_failed_total")), Some(1));
        assert_eq!(
            recorder.counter(metric_key("component_requests_relayed_total")),
            Some(0)
        );
    }

    #[tokio::test]
    async fn answers_the_services_endpoints_for_any_method() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let router = test_router();

        let paths = [
            "/services",
            "/v0.1/services",
            "/v0.2/services",
            "/v0.3/services",
            "/v0.4/services",
        ];
        let methods = ["GET", "PUT", "POST"];
        for path in paths {
            for method in methods {
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                assert_eq!(&body[..], b"OK\n", "{method} {path}");
            }
        }

        let metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("component_id", "dd_traces_in"),
                    metrics::Label::new("component_type", "sources"),
                ],
            )
        };
        assert_eq!(
            recorder.counter(metric_key("component_requests_served_locally_total")),
            Some((paths.len() * methods.len()) as u64)
        );
        assert_eq!(
            recorder.counter(metric_key("component_requests_relayed_total")),
            Some(0)
        );
        assert_eq!(recorder.counter(metric_key("component_requests_failed_total")), Some(0));
    }

    #[tokio::test]
    async fn proxies_requests_to_the_trace_agent() {
        let router = test_router();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/not-a-real-path")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    /// Starts a trace-agent that answers `/v0.4/traces` with its own `Datadog-Agent-State` and
    /// `Datadog-Agent-Version`, and any other path with a bare `404 Not Found`.
    async fn spawn_trace_agent_with_state() -> TraceAgentProxy {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let addr = listener.local_addr().expect("listener should have a local address");
        let router = Router::new().route(
            "/v0.4/traces",
            any(|| async {
                (
                    [
                        (info::AGENT_STATE_HEADER, "trace-agent-hash"),
                        (info::AGENT_VERSION_HEADER, "7.0.0"),
                    ],
                    "OK",
                )
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await });
        TraceAgentProxy::from_destination(&format!("http://{addr}")).expect("destination should parse")
    }

    async fn send(router: &Router, method: &str, path: &str) -> axum::response::Response {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn serves_info_itself_and_owns_the_agent_state_of_every_response() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let info = test_info();
        let router = build_router(ApmState {
            intake: None,
            proxy: spawn_trace_agent_with_state().await,
            tracer_flare: unreachable_tracer_flare(),
            info: info.clone(),
            container_tags: ContainerTagsHasher::default(),
            capture_entity_resolver: None,
            metrics: Metrics::new("dd_traces_in", "sources"),
        });

        // `/info` answers any method, without the state header, as in the trace-agent.
        let mut body = Bytes::new();
        for method in ["GET", "POST"] {
            let response = send(&router, method, "/info").await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_eq!(
                response.headers()[http::header::CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
            assert!(!response.headers().contains_key(info::AGENT_STATE_HEADER));
            // Without a workload provider, the tracer's container has no tags.
            assert_eq!(
                response.headers()[CONTAINER_TAGS_HASH_HEADER],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            );
            body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        }
        let state = faster_hex::hex_string(&<sha2::Sha256 as sha2::Digest>::digest(&body));

        // A proxied response carries the hash of the relay's `/info` body rather than the trace-agent's.
        let response = send(&router, "POST", "/v0.4/traces").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[info::AGENT_STATE_HEADER], state.as_str());
        assert_eq!(response.headers()[info::AGENT_VERSION_HEADER], "");

        // A response the trace-agent sent without them stays without them.
        let response = send(&router, "GET", "/not-a-real-path").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(info::AGENT_STATE_HEADER));

        // An endpoint the relay serves itself carries them too, but only `/info` carries the container tags hash.
        let response = send(&router, "PUT", "/v0.4/services").await;
        assert_eq!(response.headers()[info::AGENT_STATE_HEADER], state.as_str());
        assert!(!response.headers().contains_key(CONTAINER_TAGS_HASH_HEADER));

        // Once the Agent version is known, the payload and the headers follow it.
        info.set_agent_version(RunningAgentVersion {
            version: "7.81.0".to_string(),
            commit: "abc1234".to_string(),
        });
        let response = send(&router, "POST", "/v0.4/traces").await;
        assert_eq!(response.headers()[info::AGENT_VERSION_HEADER], "7.81.0");
        assert_ne!(response.headers()[info::AGENT_STATE_HEADER], state.as_str());
        let body = axum::body::to_bytes(send(&router, "GET", "/info").await.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["version"], "7.81.0");

        let metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("component_id", "dd_traces_in"),
                    metrics::Label::new("component_type", "sources"),
                ],
            )
        };
        assert_eq!(
            recorder.counter(metric_key("component_requests_served_locally_total")),
            Some(4)
        );
        assert_eq!(
            recorder.counter(metric_key("component_requests_relayed_total")),
            Some(3)
        );
    }

    #[tokio::test]
    async fn forwards_tracer_flares_to_datadog_rather_than_the_trace_agent() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let router = test_router();

        for method in ["POST", "GET"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/tracer_flare/v1")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        }

        let metric_key = |name| {
            metrics::Key::from_parts(
                name,
                vec![
                    metrics::Label::new("component_id", "dd_traces_in"),
                    metrics::Label::new("component_type", "sources"),
                ],
            )
        };
        assert_eq!(
            recorder.counter(metric_key("component_requests_served_locally_total")),
            Some(2)
        );
        assert_eq!(recorder.counter(metric_key("component_requests_failed_total")), Some(0));
        assert_eq!(
            recorder.counter(metric_key("component_requests_relayed_total")),
            Some(0)
        );
    }

    #[tokio::test]
    async fn binds_and_serves_over_tcp() {
        const BOUND_ADDRESS_ID: &str = "apm-relay-tcp-test-bound-address";

        let supervisor = TestSupervisor::start("apm-relay-tcp-test").await;

        // Bind to an OS-assigned ephemeral port, and discover it via the published `BoundListenAddress`.
        let tcp_endpoint = ListenAddress::try_from("tcp://127.0.0.1:0").expect("TCP APM endpoint should parse");

        let router = test_router();
        let server = HttpServer::from_listen_address(tcp_endpoint)
            .with_routes(router)
            .with_server_id(BOUND_ADDRESS_ID)
            .with_worker_pool(tokio::runtime::Handle::current());

        let mut bound_address_subscription = supervisor
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(format!("http-server-{BOUND_ADDRESS_ID}")));

        supervisor
            .scope(async {
                runtime::nested_supervisor(server.into_supervisor()).spawn();
            })
            .await;

        let bound_addr = match tokio::time::timeout(Duration::from_secs(5), bound_address_subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address)))) => address,
            update => panic!("expected a bound address assertion for `{BOUND_ADDRESS_ID}`, got {update:?}"),
        };

        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(bound_addr))
            .await
            .expect("should connect to TCP listener within the timeout")
            .expect("should connect to TCP listener");

        let body = b"proxied request over tcp";
        let request = format!(
            "POST /v0.4/traces HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("should write request head");
        stream.write_all(body).await.expect("should write request body");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("server should respond over TCP")
            .expect("should read response");

        assert!(
            response.starts_with(b"HTTP/1.1 502"),
            "expected a 502 Bad Gateway response, got: {}",
            String::from_utf8_lossy(&response)
        );
    }

    #[tokio::test]
    async fn answers_a_request_proxied_back_to_itself_with_loop_detected() {
        const BOUND_ADDRESS_ID: &str = "apm-relay-loop-test-bound-address";

        let supervisor = TestSupervisor::start("apm-relay-loop-test").await;

        // The relay's destination has to be its own listener, whose port is only known once it's bound. So the server
        // is given a pass-through to the relay router, which is built once the listener is bound. The relay is served
        // directly rather than built, as building rejects this configuration.
        let relay_router = Arc::new(OnceLock::<Router>::new());
        let pass_through = Router::new().fallback_service(service_fn({
            let relay_router = Arc::clone(&relay_router);
            move |request: AxumRequest| {
                let relay_router = relay_router
                    .get()
                    .cloned()
                    .expect("the relay router should be set before any request arrives");
                relay_router.oneshot(request)
            }
        }));
        let server = HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
            .with_routes(pass_through)
            .with_server_id(BOUND_ADDRESS_ID)
            .with_worker_pool(tokio::runtime::Handle::current());

        let mut bound_address_subscription = supervisor
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(format!("http-server-{BOUND_ADDRESS_ID}")));

        supervisor
            .scope(async {
                runtime::nested_supervisor(server.into_supervisor()).spawn();
            })
            .await;

        let bound_addr = match tokio::time::timeout(Duration::from_secs(5), bound_address_subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address)))) => address,
            update => panic!("expected a bound address assertion for `{BOUND_ADDRESS_ID}`, got {update:?}"),
        };

        relay_router
            .set(build_router(ApmState {
                intake: None,
                proxy: TraceAgentProxy::from_destination(&format!("http://{bound_addr}"))
                    .expect("destination should parse"),
                tracer_flare: unreachable_tracer_flare(),
                info: test_info(),
                container_tags: ContainerTagsHasher::default(),
                capture_entity_resolver: None,
                metrics: Metrics::new("dd_traces_in", "sources"),
            }))
            .expect("the relay router should be set only once");

        let mut stream = TcpStream::connect(bound_addr)
            .await
            .expect("should connect to TCP listener");
        stream
            .write_all(b"GET /v0.4/traces HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("should write request");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("the relay should answer rather than proxy the request to itself forever")
            .expect("should read response");

        assert!(
            response.starts_with(b"HTTP/1.1 508"),
            "expected a 508 Loop Detected response, got: {}",
            String::from_utf8_lossy(&response)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binds_and_serves_over_unix_domain_socket() {
        const BOUND_ADDRESS_ID: &str = "apm-relay-uds-test-bound-address";

        let supervisor = TestSupervisor::start("apm-relay-uds-test").await;

        let dir = tempdir().expect("temp dir should be created");
        let socket_path = dir.path().join("apm.sock");
        let uds_endpoint = ListenAddress::try_from(format!("unix://{}", socket_path.display()).as_str())
            .expect("Unix APM endpoint should parse");

        let router = test_router();
        let server = HttpServer::from_listen_address(uds_endpoint)
            .with_routes(router)
            .with_server_id(BOUND_ADDRESS_ID)
            .with_worker_pool(tokio::runtime::Handle::current());

        // The bound address is published once the listener accepts connections.
        let mut bound_address_subscription = supervisor
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(format!("http-server-{BOUND_ADDRESS_ID}")));

        supervisor
            .scope(async {
                runtime::nested_supervisor(server.into_supervisor()).spawn();
            })
            .await;

        let bound_path = match tokio::time::timeout(Duration::from_secs(5), bound_address_subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Unix(path)))) => path,
            update => panic!("expected a bound address assertion for `{BOUND_ADDRESS_ID}`, got {update:?}"),
        };
        assert_eq!(bound_path, socket_path);

        let mut stream = UnixStream::connect(&socket_path)
            .await
            .expect("should connect to Unix domain socket");

        let body = b"proxied request over uds";
        let request = format!(
            "POST /v0.4/traces HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("should write request head");
        stream.write_all(body).await.expect("should write request body");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("server should respond over the Unix domain socket")
            .expect("should read response");

        assert!(
            response.starts_with(b"HTTP/1.1 502"),
            "expected a 502 Bad Gateway response, got: {}",
            String::from_utf8_lossy(&response)
        );
    }

    fn test_config(bind_host: Option<&str>, non_local_traffic: bool, receiver_port: u16) -> ApmConfiguration {
        init_tls();
        ApmConfiguration {
            trace_intake: None,
            receiver_socket: String::new(),
            proxy_destination: "http://127.0.0.1:8127".to_string(),
            capture_entity_resolver: None,
            bind_host: bind_host.map(str::to_string),
            non_local_traffic,
            receiver_port,
            tracer_flare: TracerFlareConfiguration {
                site: "datadoghq.com".to_string(),
                api_key: Live::new_fixed("test-api-key".to_string()),
                forwarder: crate::common::datadog::test_util::forwarder_configuration(),
            },
            info: InfoConfiguration::default(),
            workload_provider: None,
            agent_client: None,
            semantic_registry: SemanticRegistryProvider::default(),
            validate_endpoint: "http://127.0.0.1:1/api/v2/validate".to_string(),
        }
    }

    /// Sends one single-chunk v1.0 payload over HTTP/1.1 and returns the response status line.
    async fn send_v1_payload(port: u16) -> String {
        // A tracer payload with one chunk of one span, whose service is `svc` (fields 11, 4 and 1).
        let mut body = Vec::new();
        rmp::encode::write_map_len(&mut body, 1).unwrap();
        rmp::encode::write_uint(&mut body, 11).unwrap();
        rmp::encode::write_array_len(&mut body, 1).unwrap();
        rmp::encode::write_map_len(&mut body, 1).unwrap();
        rmp::encode::write_uint(&mut body, 4).unwrap();
        rmp::encode::write_array_len(&mut body, 1).unwrap();
        rmp::encode::write_map_len(&mut body, 1).unwrap();
        rmp::encode::write_uint(&mut body, 1).unwrap();
        rmp::encode::write_str(&mut body, "service-with-a-long-enough-name").unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("should connect");
        let head = format!(
            "PUT /v1.0/traces HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn traces_accepted_before_shutdown_are_dispatched() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
            listener.local_addr().unwrap().port()
        };
        let mut config = test_config(None, false, port);
        config.trace_intake = Some(TraceIntakeConfiguration {
            max_request_bytes: 1024,
            decoders: std::num::NonZeroUsize::new(2).unwrap(),
            decoder_timeout: Duration::from_secs(5),
            receiver_timeout: Duration::from_secs(5),
            reject_with_too_many_requests: false,
            histogram: crate::transforms::HistogramConfiguration::default(),
            service_rates: crate::transforms::ServiceRates::default(),
        });
        // A full output blocks the dispatcher, so that accepted payloads wait in its queue.
        let (control, mut outputs) = TestComponentDriver::options()
            .with_interconnect_capacity(std::num::NonZeroUsize::MIN)
            .source(config)
            .await
            .expect("the source should build");

        // Wait for the listener, then send payloads until they fill the output and the dispatcher's queue.
        let mut accepted = 0;
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for _ in 0..3 {
            assert_eq!(send_v1_payload(port).await, "HTTP/1.1 200 OK");
            accepted += 1;
        }

        let shutdown = tokio::spawn(control.shutdown());
        let mut dispatched = 0;
        while dispatched < accepted {
            let buffer = outputs
                .named_output("traces")
                .next()
                .await
                .expect("the output should stay open");
            dispatched += buffer
                .into_iter()
                .filter(|event| matches!(event, Event::Trace(_)))
                .count();
        }
        assert_eq!(dispatched, accepted);
        shutdown.await.unwrap().expect("the source should stop cleanly");

        // The last telemetry window is reported once the accepted payloads are dispatched.
        let metrics = outputs.named_output("metrics").collect_events().await;
        let payload_accepted: f64 = metrics
            .iter()
            .filter_map(|event| event.try_as_metric())
            .filter(|metric| metric.context().name() == "datadog.trace_agent.receiver.payload_accepted")
            .filter_map(|metric| match metric.values() {
                saluki_core::data_model::event::metric::MetricValues::Rate(points, _) => {
                    Some(points.into_iter().map(|(_, value)| value).sum::<f64>())
                }
                _ => None,
            })
            .sum();
        assert_eq!(payload_accepted, accepted as f64, "{metrics:?}");
    }

    #[test]
    fn only_the_native_trace_intake_has_a_metrics_output() {
        // Proxied payloads are counted by the trace-agent, so the relay alone reports no receiver telemetry.
        let has_metrics = |config: &ApmConfiguration| {
            config
                .outputs()
                .iter()
                .any(|output| output.output_name() == Some("metrics"))
        };
        let mut config = test_config(None, false, 0);
        assert!(!has_metrics(&config));
        config.trace_intake = Some(TraceIntakeConfiguration {
            max_request_bytes: 1024,
            decoders: std::num::NonZeroUsize::MIN,
            decoder_timeout: Duration::from_secs(1),
            receiver_timeout: Duration::from_secs(1),
            reject_with_too_many_requests: false,
            histogram: crate::transforms::HistogramConfiguration::default(),
            service_rates: crate::transforms::ServiceRates::default(),
        });
        assert!(has_metrics(&config));
    }

    #[tokio::test]
    async fn a_listener_that_fails_to_bind_fails_the_relay() {
        // Like a trace-agent that still owns the receiver port.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
        let port = occupied.local_addr().expect("should have a local address").port();

        let (control, _outputs) = TestComponentDriver::source(test_config(None, false, port))
            .await
            .expect("the relay should build");

        let result = control.wait().await;
        assert!(result.is_err(), "the relay should fail when its listener can't bind");
    }

    async fn build_error(config: ApmConfiguration) -> GenericError {
        match config.build(BuildContext::test_source("apm_relay_build_error")).await {
            Ok(_) => panic!("build should fail"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn build_fails_when_the_destination_is_the_tcp_listener() {
        // The trace-agent's default port used for both the relay and its destination.
        let config = test_config(None, false, 8127);
        assert!(build_error(config).await.to_string().contains("relay's own listener"));

        // A listener on every interface is also reached through the loopback address.
        let config = test_config(None, true, 8127);
        assert!(build_error(config).await.to_string().contains("relay's own listener"));

        let mut config = test_config(None, false, 8127);
        config.proxy_destination = "http://localhost:8127".to_string();
        assert!(build_error(config).await.to_string().contains("relay's own listener"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_fails_when_the_destination_is_the_unix_domain_socket_listener() {
        let mut config = test_config(None, false, 0);
        config.receiver_socket = "/var/run/datadog/apm.socket".to_string();
        config.proxy_destination = "unix:///var/run/datadog/apm.socket".to_string();

        assert!(build_error(config).await.to_string().contains("relay's own listener"));
    }

    #[test]
    fn tcp_reaches_only_a_listener_on_the_same_port_and_address() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();

        assert!(tcp_reaches(addr("127.0.0.1:8126"), addr("127.0.0.1:8126")));
        assert!(tcp_reaches(addr("127.0.0.1:8126"), addr("0.0.0.0:8126")));
        assert!(tcp_reaches(addr("0.0.0.0:8126"), addr("127.0.0.1:8126")));
        assert!(tcp_reaches(addr("[::1]:8126"), addr("[::]:8126")));

        assert!(!tcp_reaches(addr("127.0.0.1:8127"), addr("127.0.0.1:8126")));
        assert!(!tcp_reaches(addr("10.0.0.2:8126"), addr("10.0.0.1:8126")));
        assert!(!tcp_reaches(addr("[::1]:8126"), addr("0.0.0.0:8126")));
        assert!(!tcp_reaches(addr("127.0.0.1:8126"), addr("10.0.0.1:8126")));
    }

    #[tokio::test]
    async fn build_fails_without_any_configured_endpoint() {
        // Listening nowhere is a configuration error, not a silent no-op: `build` must reject it.
        let config = test_config(None, false, 0);

        let error = match config.build(BuildContext::test_source("apm_relay_no_endpoints")).await {
            Ok(_) => panic!("build should fail when no endpoint is configured"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("at least one"));
    }

    #[tokio::test]
    async fn build_succeeds_with_only_tcp_endpoint_configured() {
        let config = test_config(None, true, 8126);

        assert!(config
            .build(BuildContext::test_source("apm_relay_tcp_only"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn build_fails_with_invalid_proxy_destination() {
        let mut config = test_config(None, false, 8126);
        config.proxy_destination = "ftp://127.0.0.1:8127".to_string();

        assert!(config
            .build(BuildContext::test_source("apm_relay_bad_destination"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn build_fails_with_unresolvable_bind_host() {
        let config = test_config(Some("not a valid host"), false, 8126);

        let error = match config
            .build(BuildContext::test_source("apm_relay_unresolvable_bind_host"))
            .await
        {
            Ok(_) => panic!("build should fail for an unresolvable bind host"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Could not resolve bind_host"));
    }

    #[tokio::test]
    async fn tcp_listen_address_defaults_to_loopback() {
        let config = test_config(None, false, 8127);
        assert_eq!(
            config.tcp_listen_address().await.unwrap().unwrap().to_string(),
            "tcp://127.0.0.1:8127"
        );
    }

    #[tokio::test]
    async fn tcp_listen_address_uses_bind_host() {
        let config = test_config(Some("192.168.1.50"), false, 8127);
        assert_eq!(
            config.tcp_listen_address().await.unwrap().unwrap().to_string(),
            "tcp://192.168.1.50:8127"
        );
    }

    #[tokio::test]
    async fn tcp_listen_address_non_local_traffic_overrides_bind_host() {
        let config = test_config(Some("192.168.1.50"), true, 8127);
        assert_eq!(
            config.tcp_listen_address().await.unwrap().unwrap().to_string(),
            "tcp://0.0.0.0:8127"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_listen_address_parses_path() {
        let mut config = test_config(None, false, 0);
        config.receiver_socket = "/tmp/apm.sock".to_string();
        assert_eq!(
            config.uds_listen_address().unwrap().unwrap().to_string(),
            "unix:///tmp/apm.sock"
        );
    }
}
