//! Remote agent registration and the services that the Datadog Agent queries.
//!
//! [`RemoteAgentBootstrap`] registers the process with the Datadog Agent over the Remote Agent Registry, keeps that
//! registration alive, and creates everything that depends on it: the configuration stream, and the workers that serve
//! the status, flare, and telemetry services and report diagnostic events back to the Datadog Agent.
//!
//! # Before the supervision tree runs
//!
//! Registration and the initial configuration have to arrive before the supervision tree exists, but both have to keep
//! going while the rest of the remote agent is built: the registration expires unless it is refreshed, and
//! configuration updates can arrive at any time. So the registration loop and the configuration stream loop each start
//! in a task, and each has a worker that takes the loop over when it first starts. The worker continues from the state
//! that the task left, with the same session, refresh schedule, and open stream, and so does each restart of the
//! worker.
//!
//! The task finishes a registration attempt in flight before it hands over, so a handoff never registers twice. A
//! worker stops at once on shutdown, even in the middle of an attempt, and a restarted worker retries an interrupted
//! attempt right away. If no worker ever takes over, the task stops once its loop has no owner left: the config stream
//! worker, or the `RemoteAgentBootstrap` and every registration worker created from it.

use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_agent_commons::ipc::{
    client::RemoteAgentClient,
    config::RemoteAgentClientConfiguration,
    session::{SessionId, SessionIdHandle},
};
use datadog_protos::agent::v1::{
    event::Details as RemoteAgentEventDetails, Event as RemoteAgentEvent, InvalidApiKeyEvent,
    RegisterRemoteAgentResponse, ReportRemoteAgentEventRequest,
};
use datadog_protos::agent::{
    flare::v1::{flare_provider_server::*, *},
    status::v1::{status_provider_server::*, *},
    telemetry::v1::{get_telemetry_response::*, telemetry_provider_server::*, *},
    ConfigEvent,
};
use futures::{Stream, StreamExt};
use process_memory::Querier as MemoryQuerier;
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_common::task::spawn_traced_named;
use saluki_config::dynamic::ConfigUpdate;
use saluki_core::{
    diagnostic::{subscribe_events, DiagnosticCollector, DiagnosticDetails, DiagnosticEvent},
    observability::metrics::{
        get_shared_metrics_state, AggregatedMetricsProcessor, AggregatedMetricsState, Reflector, RemapperRule,
        TelemetryProcessor,
    },
    runtime::{
        state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter, Subscription},
        InitializationError, Supervisable, SupervisorFuture,
    },
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{GrpcTargetAddress, ListenAddress};
use tokio::task::{spawn_blocking, AbortHandle};
use tokio::time::{sleep_until, timeout, Instant};
use tokio::{
    pin, select,
    sync::{mpsc, Mutex},
    time::{interval, Interval, MissedTickBehavior},
};
use tonic::{server::NamedService, service::Routes, Status};
use tracing::{debug, error, info, warn};

use crate::agent_stream::config_event_to_update;

mod status;
pub use self::status::{StatusBuilder, StatusSectionProvider, StatusSectionWriter};

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CONFIG_STREAM_RETRY_INTERVAL: Duration = Duration::from_secs(5);

// RAR sessions can change without restarting the process. Keep this identity for the entire process lifetime.
static PROCESS_INSTANCE_ID: OnceLock<String> = OnceLock::new();

const SESSION_ID_METADATA_KEY: &str = "session_id";

const CONFIG_STREAM_WORKER_NAME: &str = "config-stream";
const CONFIG_STREAM_CAPACITY: usize = 100;

/// Remote agent initialization.
///
/// This helper type is used to coordinate the initialization of remote agent state by registering to the Core Agent and
/// acquiring the necessary information to allow initialization of the remote agent itself to proceed.
pub struct RemoteAgentBootstrap {
    client: RemoteAgentClient,
    session_id: SessionIdHandle,
    registration: RemoteAgentRegistrationWorker,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
    dataspace: Arc<OnceLock<DataspaceRegistry>>,
}

impl RemoteAgentBootstrap {
    /// Creates a new `RemoteAgentBootstrap` from the given configurations.
    ///
    /// A remote agent client is created and immediately attempts to register with the Core Agent, advertising
    /// `secure_api_listen_address` as the address where the Core Agent reaches the status, flare, and telemetry
    /// services. This function does not return until the first registration attempt finishes, whether successful or
    /// not. `additional_service_names` advertises services the binary adds through
    /// [`create_services_worker`](Self::create_services_worker). Once registered, a task keeps the registration alive
    /// until the worker from [`create_registration_worker`][Self::create_registration_worker] takes over.
    ///
    /// # Errors
    ///
    /// Returns an error if `secure_api_listen_address` isn't a valid gRPC target address, if the client can't connect
    /// to the Core Agent, or if the first registration attempt fails. Nothing is left running when this returns an
    /// error.
    pub async fn new(
        client_config: &RemoteAgentClientConfiguration, secure_api_listen_address: &ListenAddress,
        additional_service_names: &[&str],
    ) -> Result<Self, GenericError> {
        let api_listen_addr = GrpcTargetAddress::try_from_listen_addr(secure_api_listen_address)
            .ok_or_else(|| generic_error!("Failed to get valid gRPC target address from secure API listen address."))?;

        // Generate our remote agent state, which is mostly fixed but has a few dynamic bits.
        let mut service_names = vec![
            <StatusProviderServer<()> as NamedService>::NAME.to_string(),
            <FlareProviderServer<()> as NamedService>::NAME.to_string(),
            <TelemetryProviderServer<()> as NamedService>::NAME.to_string(),
        ];
        service_names.extend(additional_service_names.iter().map(|name| (*name).to_owned()));
        let details = RegistrationDetails::new(api_listen_addr, service_names);
        let session_id = SessionIdHandle::empty();

        let client = RemoteAgentClient::connect(client_config).await?;
        let registration = Registration::new(Box::new(client.clone()), details, session_id.clone())
            .register_and_spawn()
            .await?;

        Ok(Self {
            client,
            session_id,
            registration,
            internal_metrics: get_shared_metrics_state(),
            dataspace: Arc::new(OnceLock::new()),
        })
    }

    /// Creates a worker that keeps the registration with the Core Agent alive.
    ///
    /// The worker is named `{identifier}-remote-agent-task`, where `{identifier}` is the identifier of the application
    /// details, and must be added to the supervision tree.
    pub fn create_registration_worker(&self) -> RemoteAgentRegistrationWorker {
        self.registration.share()
    }

    fn build_impl(
        &self, status_sections: Vec<Arc<dyn StatusSectionProvider>>, remapper_rules: Vec<RemapperRule>,
    ) -> RemoteAgentImpl {
        RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            processor: telemetry_processor(remapper_rules),
            status_sections,
            session_id: self.session_id.clone(),
            dataspace: Arc::clone(&self.dataspace),
        }
    }

    /// Creates a worker that captures the dataspace from the supervisor context and makes it available to the
    /// diagnostic artifact collection service.
    ///
    /// This worker must be added to the supervision tree. When it initializes, it runs inside a supervisor process
    /// where the dataspace task-local is set, and fills the shared [`OnceLock`] so that `get_flare_files` can
    /// collect diagnostic artifacts at request time. Without this, the gRPC handler tasks spawned by tonic would not
    /// have access to the dataspace since tonic's `tokio::spawn` does not propagate task-locals.
    pub fn create_dataspace_anchor(&self) -> DataspaceAnchorWorker {
        DataspaceAnchorWorker {
            dataspace: Arc::clone(&self.dataspace),
        }
    }

    /// Creates a worker that reports diagnostic events to the Core Agent as remote agent events.
    pub fn create_event_reporter(&self) -> RemoteAgentEventReporter {
        RemoteAgentEventReporter {
            client: self.client.clone(),
            session_id: self.session_id.clone(),
        }
    }

    /// Creates a worker that serves the status, flare, and telemetry services on the privileged API endpoint.
    ///
    /// The status service always reports the built-in main section fields (version, Git commit, architecture, start
    /// time, and the Agent version this build was compiled against), followed by the fields written by each of the
    /// given `status_sections`, in order.
    ///
    /// The telemetry service reports the internal metrics that match the given `telemetry_rules`, renamed to the names
    /// the Datadog Agent expects. Metrics that match no rule are not reported, so if `telemetry_rules` is empty, the
    /// service reports no metrics at all.
    ///
    /// `extend_services` adds binary-specific services using the live registration session handle. Their names must
    /// be included in the `additional_service_names` passed to [`new`](Self::new).
    pub fn create_services_worker(
        &self, status_sections: Vec<Arc<dyn StatusSectionProvider>>, telemetry_rules: Vec<RemapperRule>,
        extend_services: impl FnOnce(Routes, SessionIdHandle) -> Routes,
    ) -> RemoteAgentServicesWorker {
        let routes = Routes::new(StatusProviderServer::new(self.build_impl(status_sections, Vec::new())))
            .add_service(FlareProviderServer::new(self.build_impl(Vec::new(), Vec::new())))
            .add_service(TelemetryProviderServer::new(
                self.build_impl(Vec::new(), telemetry_rules),
            ));

        RemoteAgentServicesWorker {
            routes: extend_services(routes, self.session_id.clone()),
        }
    }

    /// Creates a config stream that receives configuration events from the Core Agent, along with the worker that
    /// feeds it.
    ///
    /// A task feeds the stream until the worker takes over, so that the initial configuration can arrive before the
    /// supervision tree runs. The worker is named `config-stream`, and must be added to the supervision tree.
    pub fn create_config_stream(&self) -> (mpsc::Receiver<ConfigUpdate>, ConfigStreamWorker) {
        ConfigStreamWorker::spawn(Box::new(self.client.clone()), self.session_id.clone())
    }
}

/// A loop that has to start before the supervision tree exists, and continue in a worker once it runs.
trait BootstrapLoop: Send + 'static {
    /// Runs the loop until `handoff` resolves, stopping only where that loses no work.
    ///
    /// Each call continues from wherever the previous call stopped, including a call that was dropped partway through.
    fn run(&mut self, handoff: ShutdownHandle) -> impl Future<Output = ()> + Send + '_;
}

/// The task that runs a [`BootstrapLoop`] until a worker takes it over.
struct BootstrapTask {
    handoff: ShutdownCoordinator,
    task: AbortHandle,
}

/// Hands a [`BootstrapLoop`] over from the task that starts it to the worker that continues it.
struct Handoff<L> {
    name: String,
    state: Arc<Mutex<L>>,
    task: std::sync::Mutex<Option<BootstrapTask>>,
}

impl<L: BootstrapLoop> Handoff<L> {
    /// Starts the loop in a task named `name`.
    fn spawn(name: String, state: L) -> Self {
        let state = Arc::new(Mutex::new(state));
        // Locked before the task is spawned, so that a worker can't take the state first.
        let mut task_state = Arc::clone(&state)
            .try_lock_owned()
            .expect("a new mutex should be unlocked");
        let (handoff, stop) = ShutdownHandle::paired();
        let task = spawn_traced_named(name.clone(), async move { task_state.run(stop).await }).abort_handle();

        Self {
            name,
            state,
            task: std::sync::Mutex::new(Some(BootstrapTask { handoff, task })),
        }
    }

    /// Hands the loop over from the task, if no worker has yet, and returns the future of a worker that continues it.
    fn take_over(&self, process_shutdown: ShutdownHandle) -> SupervisorFuture {
        let task =
            self.task
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .map(|BootstrapTask { handoff, task }| {
                    handoff.shutdown();
                    task
                });

        let state = Arc::clone(&self.state);
        let run = async move {
            let mut state = state.lock_owned().await;
            state.run(ShutdownHandle::noop()).await;
        };

        Box::pin(async move {
            select! {
                biased;
                _ = process_shutdown => {
                    // The task may still be in the middle of an attempt, which a restart retries.
                    if let Some(task) = task {
                        task.abort();
                    }
                }
                _ = run => {}
            }
            Ok(())
        })
    }
}

impl<L> Drop for Handoff<L> {
    fn drop(&mut self) {
        // No worker can take the loop over any more, so the task has nothing to finish.
        if let Some(BootstrapTask { task, .. }) = self.task.get_mut().unwrap_or_else(PoisonError::into_inner).take() {
            task.abort();
        }
    }
}

/// What the remote agent advertises when it registers.
struct RegistrationDetails {
    pid: u32,
    process_instance_id: &'static str,
    display_name: String,
    flavor: String,
    api_listen_addr: String,
    service_names: Vec<String>,
}

impl RegistrationDetails {
    fn new(api_listen_addr: GrpcTargetAddress, service_names: Vec<String>) -> Self {
        let app_details = saluki_metadata::get_app_details();

        Self {
            pid: std::process::id(),
            process_instance_id: PROCESS_INSTANCE_ID
                .get_or_init(|| uuid::Uuid::new_v4().to_string())
                .as_str(),
            display_name: app_details.full_name().to_string(),
            flavor: app_details.full_name().replace(" ", "_").to_lowercase(),
            api_listen_addr: api_listen_addr.to_string(),
            service_names,
        }
    }
}

/// The Remote Agent Registry calls that registration makes, so that tests can stand in for the Core Agent.
#[async_trait]
trait RegistryClient: Send {
    async fn register(&mut self, details: &RegistrationDetails) -> Result<RegisterRemoteAgentResponse, GenericError>;

    async fn refresh(&mut self, session_id: &SessionId) -> Result<(), GenericError>;
}

#[async_trait]
impl RegistryClient for RemoteAgentClient {
    async fn register(&mut self, details: &RegistrationDetails) -> Result<RegisterRemoteAgentResponse, GenericError> {
        let response = self
            .register_remote_agent(
                details.pid,
                details.process_instance_id,
                &details.display_name,
                &details.flavor,
                &details.api_listen_addr,
                details.service_names.clone(),
            )
            .await?;
        Ok(response.into_inner())
    }

    async fn refresh(&mut self, session_id: &SessionId) -> Result<(), GenericError> {
        self.refresh_remote_agent(session_id).await.map(|_| ())
    }
}

/// The registration loop, which refreshes the registration periodically and registers again whenever a refresh fails.
struct Registration {
    client: Box<dyn RegistryClient>,
    details: RegistrationDetails,
    session_id: SessionIdHandle,
    timer: Interval,
    attempt_in_flight: bool,
}

impl Registration {
    fn new(client: Box<dyn RegistryClient>, details: RegistrationDetails, session_id: SessionIdHandle) -> Self {
        let mut timer = interval(DEFAULT_REFRESH_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        Self {
            client,
            details,
            session_id,
            timer,
            attempt_in_flight: false,
        }
    }

    /// Registers, and then starts the registration loop in a task until the returned worker takes it over.
    ///
    /// # Errors
    ///
    /// If a registration attempt fails before one succeeds, an error is returned, and no task is started.
    async fn register_and_spawn(mut self) -> Result<RemoteAgentRegistrationWorker, GenericError> {
        debug!("Remote Agent registration task started.");

        // An invalid session ID leaves us unregistered without failing the attempt, so we try again on the next tick.
        while self.session_id.get().is_none() {
            self.timer.tick().await;
            self.register().await?;
        }

        let name = format!("{}-remote-agent-task", saluki_metadata::get_app_details().identifier());
        Ok(RemoteAgentRegistrationWorker {
            registration: Arc::new(Handoff::spawn(name, self)),
        })
    }

    async fn refresh_or_register(&mut self) {
        match self.session_id.get() {
            Some(session_id) => {
                debug!(%session_id, "Refreshing registration with Datadog Agent.");

                if self.client.refresh(&session_id).await.is_err() {
                    self.timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                    self.session_id.update(None);
                    warn!("Failed to refresh registration with the Datadog Agent. Resetting session ID and attempting to re-register shortly.");
                }
            }
            None => {
                // `register` logs its own failures.
                let _ = self.register().await;
            }
        }
    }

    async fn register(&mut self) -> Result<(), GenericError> {
        let resp = match self.client.register(&self.details).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!(error = %e, "Failed to register with the Datadog Agent. Registration will be retried periodically in the background.");
                self.timer.reset_after(DEFAULT_REFRESH_INTERVAL);
                return Err(e);
            }
        };

        match SessionId::new(&resp.session_id) {
            Ok(new_session_id) => {
                let new_refresh_interval = resp.recommended_refresh_interval_secs;
                info!(
                    session_id = %new_session_id,
                    process_instance_id = %self.details.process_instance_id,
                    "Successfully registered with the Datadog Agent. Refreshing every {} seconds.",
                    new_refresh_interval
                );

                self.session_id.update(Some(new_session_id));
                self.timer.reset_after(Duration::from_secs(new_refresh_interval as u64));
            }
            Err(e) => {
                warn!(error = %e, "Received invalid session ID from Datadog Agent after registation. Registration will be retried periodically in the background.");
                self.timer.reset_after(DEFAULT_REFRESH_INTERVAL);
            }
        }

        Ok(())
    }
}

impl BootstrapLoop for Registration {
    async fn run(&mut self, handoff: ShutdownHandle) {
        pin!(handoff);

        loop {
            // An attempt that a dropped call interrupted is retried right away.
            if !self.attempt_in_flight {
                select! {
                    biased;
                    _ = &mut handoff => return,
                    _ = self.timer.tick() => {}
                }
                self.attempt_in_flight = true;
            }

            self.refresh_or_register().await;
            self.attempt_in_flight = false;
        }
    }
}

type ConfigEventStream = Pin<Box<dyn Stream<Item = Result<ConfigEvent, Status>> + Send>>;

/// Opens configuration event streams, so that tests can stand in for the Core Agent.
trait ConfigEventSource: Send {
    fn open(&mut self, session_id: &SessionId) -> ConfigEventStream;
}

impl ConfigEventSource for RemoteAgentClient {
    fn open(&mut self, session_id: &SessionId) -> ConfigEventStream {
        Box::pin(self.stream_config_events(session_id))
    }
}

/// The configuration stream loop, which forwards configuration events from the Core Agent to a channel.
struct ConfigStream {
    source: Box<dyn ConfigEventSource>,
    sender: mpsc::Sender<ConfigUpdate>,
    session_id: SessionIdHandle,
    events: Option<ConfigEventStream>,
    reconnect_at: Option<Instant>,
    pending: Option<ConfigUpdate>,
}

impl ConfigStream {
    fn new(
        source: Box<dyn ConfigEventSource>, sender: mpsc::Sender<ConfigUpdate>, session_id: SessionIdHandle,
    ) -> Self {
        Self {
            source,
            sender,
            session_id,
            events: None,
            reconnect_at: None,
            pending: None,
        }
    }

    /// Forwards configuration events until the receiver is dropped.
    ///
    /// Every await point is cancel-safe: the open stream, the reconnect deadline, and an update waiting for room in the
    /// channel are all kept in `self`.
    async fn forward(&mut self) {
        if self.sender.is_closed() {
            return;
        }

        loop {
            if self.pending.is_some() {
                let Ok(permit) = self.sender.reserve().await else {
                    warn!("Dynamic configuration channel closed. Config stream shutting down.");
                    self.events = None;
                    self.pending = None;
                    return;
                };
                permit.send(self.pending.take().expect("pending update should be present"));
            }

            let events = match &mut self.events {
                Some(events) => events,
                None => {
                    if let Some(reconnect_at) = self.reconnect_at {
                        sleep_until(reconnect_at).await;
                        self.reconnect_at = None;
                    }

                    debug!("Establishing a new config stream connection to the Core Agent.");

                    // Read the session ID on every reconnect, since it can change in the background due to
                    // re-registration.
                    let session_id = self.session_id.wait_for_update().await;
                    self.events.insert(self.source.open(&session_id))
                }
            };

            match events.next().await {
                Some(Ok(event)) => self.pending = config_event_to_update(event),
                Some(Err(e)) => error!("Error while reading config event stream: {}.", e),
                None => {
                    debug!("Config stream ended, retrying in 5 seconds...");
                    self.events = None;
                    self.reconnect_at = Some(Instant::now() + CONFIG_STREAM_RETRY_INTERVAL);
                }
            }
        }
    }
}

impl BootstrapLoop for ConfigStream {
    async fn run(&mut self, handoff: ShutdownHandle) {
        pin!(handoff);

        select! {
            biased;
            _ = &mut handoff => return,
            _ = self.forward() => {}
        }

        // The receiver is gone. Wait to be stopped rather than return, which would make a worker restart.
        handoff.await;
    }
}

/// The implementation behind the status, flare, and telemetry services that the Datadog Agent queries.
struct RemoteAgentImpl {
    started: DateTime<Utc>,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
    processor: Option<Mutex<TelemetryProcessor>>,
    status_sections: Vec<Arc<dyn StatusSectionProvider>>,
    session_id: SessionIdHandle,
    dataspace: Arc<OnceLock<DataspaceRegistry>>,
}

/// Creates the processor behind the telemetry service, or `None` if there are no remapper rules.
///
/// A `TelemetryProcessor` without rules renders every metric under its raw name, so we skip it entirely rather than
/// report metrics that no rule selected.
fn telemetry_processor(remapper_rules: Vec<RemapperRule>) -> Option<Mutex<TelemetryProcessor>> {
    (!remapper_rules.is_empty()).then(|| Mutex::new(TelemetryProcessor::new().with_remapper_rules(remapper_rules)))
}

/// Renders the telemetry payload for the given internal metrics.
///
/// Without a processor, the payload is empty.
async fn render_telemetry(processor: Option<&Mutex<TelemetryProcessor>>, state: &AggregatedMetricsState) -> String {
    match processor {
        Some(processor) => processor.lock().await.process(state),
        None => String::new(),
    }
}

impl RemoteAgentImpl {
    async fn session_id_middleware<Resp, Next>(&self, next: Next) -> Result<tonic::Response<Resp>, Status>
    where
        Next: AsyncFnOnce() -> Result<tonic::Response<Resp>, Status>,
    {
        let metadata_session_id = self
            .session_id
            .get()
            .ok_or(Status::failed_precondition(
                "session ID not set; must be registered with Core Agent",
            ))?
            .to_grpc_header_value();

        next().await.map(|mut resp| {
            resp.metadata_mut().append(SESSION_ID_METADATA_KEY, metadata_session_id);
            resp
        })
    }
}

#[async_trait]
impl StatusProvider for RemoteAgentImpl {
    async fn get_status_details(
        &self, _request: tonic::Request<GetStatusDetailsRequest>,
    ) -> Result<tonic::Response<GetStatusDetailsResponse>, Status> {
        return self
            .session_id_middleware(async || {
                let app_details = saluki_metadata::get_app_details();

                let mut builder = StatusBuilder::new();
                builder
                    .main_section()
                    .set_field("Version", app_details.version().raw())
                    .set_field("Git Commit", app_details.git_hash())
                    .set_field("Architecture", app_details.target_arch())
                    .set_field("Started", self.started.to_rfc3339());

                // Surface the Agent version this build was compiled against, for debugging version-gated behavior.
                if let Some(agent_version) = datadog_agent_commons::agent_version::version_string() {
                    builder
                        .main_section()
                        .set_field("Built Against Agent Version", agent_version);
                }

                for section in &self.status_sections {
                    section.write_status(&mut builder);
                }

                Ok(tonic::Response::new(builder.into_response()))
            })
            .await;
    }
}

#[async_trait]
impl TelemetryProvider for RemoteAgentImpl {
    async fn get_telemetry(
        &self, _request: tonic::Request<GetTelemetryRequest>,
    ) -> Result<tonic::Response<GetTelemetryResponse>, Status> {
        return self
            .session_id_middleware(async || {
                let prom_text = render_telemetry(self.processor.as_ref(), self.internal_metrics.state()).await;

                Ok(tonic::Response::new(GetTelemetryResponse {
                    payload: Some(Payload::PromText(prom_text)),
                }))
            })
            .await;
    }
}

/// Timeout for the tokio task dump step within [`FlareProvider::get_flare_files`].
///
/// `Handle::dump()` may never resolve if a runtime worker is blocked for more than 250 ms. We use
/// a conservative 5-second budget so that a hung runtime still allows the other diagnostic artifacts
/// to be collected and returned.
#[cfg(all(
    feature = "taskdump",
    target_os = "linux",
    any(
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "s390x"
    )
))]
const TASK_DUMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Timeout for collecting all component-owned diagnostic artifacts.
///
/// Each `collect()` closure is run on a blocking thread via `spawn_blocking`. If the overall
/// budget is exhausted, collection stops and whatever artifacts were gathered are returned.
/// This ensures that a slow or deadlocked component cannot stall the entire response.
const DIAGNOSTIC_COLLECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Maximum size in bytes for a single diagnostic artifact.
///
/// Artifacts that exceed this limit are truncated and a truncation marker is appended so that
/// readers know the content is incomplete. High-cardinality hosts can produce very large workload
/// tag dumps; this cap keeps the overall archive size predictable.
const DIAGNOSTIC_ARTIFACT_MAX_BYTES: usize = 5 * 1024 * 1024; // 5 MiB

/// Truncation marker appended to diagnostic artifacts that exceed [`DIAGNOSTIC_ARTIFACT_MAX_BYTES`] in size.
const DIAGNOSTIC_TRUNCATION_MARKER: &[u8] = b"\n\n[... truncated: artifact exceeded the 5 MiB per-file limit ...]\n";

/// Caps `data` to [`DIAGNOSTIC_ARTIFACT_MAX_BYTES`], appending [`DIAGNOSTIC_TRUNCATION_MARKER`] if truncated.
fn cap_artifact_data(mut data: Vec<u8>) -> Vec<u8> {
    if data.len() > DIAGNOSTIC_ARTIFACT_MAX_BYTES {
        data.truncate(DIAGNOSTIC_ARTIFACT_MAX_BYTES);
        data.extend_from_slice(DIAGNOSTIC_TRUNCATION_MARKER);
    }
    data
}

/// Returns the number of open file descriptors for the current process.
///
/// Reads from `/proc/self/fd` on Linux. Returns `"unavailable"` on other platforms.
#[cfg(target_os = "linux")]
fn fd_count() -> String {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count().to_string())
        .unwrap_or_else(|_| "unavailable".to_string())
}

#[cfg(not(target_os = "linux"))]
fn fd_count() -> String {
    "unavailable".to_string()
}

/// Returns the number of threads in the current process.
///
/// Reads the `Threads:` field from `/proc/self/status` on Linux. Returns `"unavailable"` on other
/// platforms.
#[cfg(target_os = "linux")]
fn thread_count() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Threads:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        })
        .unwrap_or_else(|| "unavailable".to_string())
}

#[cfg(not(target_os = "linux"))]
fn thread_count() -> String {
    "unavailable".to_string()
}

#[async_trait]
impl FlareProvider for RemoteAgentImpl {
    async fn get_flare_files(
        &self, _request: tonic::Request<GetFlareFilesRequest>,
    ) -> Result<tonic::Response<GetFlareFilesResponse>, Status> {
        return self
            .session_id_middleware(async || {
                let mut files: HashMap<String, Vec<u8>> = HashMap::new();

                // Grab and run all asserted diagnostic collectors.
                if let Some(dataspace) = self.dataspace.get() {
                    let collectors = dataspace.current_values::<DiagnosticCollector>(IdentifierFilter::all());
                    let total_collectors = collectors.len();
                    let deadline = Instant::now() + DIAGNOSTIC_COLLECT_TIMEOUT;

                    for collector in collectors {
                        if Instant::now() >= deadline {
                            break;
                        }

                        let artifact_name = collector.artifact_name().to_string();
                        let remaining = deadline.saturating_duration_since(Instant::now());

                        let collect = timeout(
                            remaining,
                            spawn_blocking(move || collector.collect()),
                        );
                        let artifact_data = match collect.await {
                            Ok(Ok(data)) => data,
                            Ok(Err(e)) => format!("collection panicked for '{artifact_name}': {e}").into_bytes(),
                            Err(_) => format!("collection timed out for '{artifact_name}'").into_bytes(),
                        };

                        files.insert(artifact_name, cap_artifact_data(artifact_data));
                    }

                    if files.len() < total_collectors {
                        warn!("Diagnostic artifact collection timed out; collected {}/{total_collectors} artifacts.", files.len());
                    }
                }

                // Process-level artifacts.
                //
                // These don't belong to any individual component — they describe the process
                // itself. They stay hardwired here since there is no natural component owner to
                // assert a handle for them.

                // Process info: pid, uptime, RSS, args, fd count, thread count.
                let pid = std::process::id();
                let uptime = Utc::now().signed_duration_since(self.started);
                let args: Vec<String> = std::env::args().collect();
                let rss_display = MemoryQuerier::default()
                    .resident_set_size()
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "unavailable".to_string());
                let process_info = format!(
                    "pid: {pid}\nuptime_seconds: {uptime}\nrss_bytes: {rss_display}\nargs: {args:?}\nfd_count: {fd_count}\nthread_count: {thread_count}\n",
                    uptime = uptime.num_seconds(),
                    fd_count = fd_count(),
                    thread_count = thread_count(),
                );
                files.insert("runtime_debug_info.log".to_string(), cap_artifact_data(process_info.into_bytes()));

                // Tokio task dump (only with the `taskdump` feature enabled, on targets where Tokio supports task dumps).
                //
                // Wrapped in an explicit timeout because `Handle::dump()`
                // may never resolve if a runtime worker is blocked for
                // more than 250ms
                #[cfg(all(
                    feature = "taskdump",
                    target_os = "linux",
                    any(
                        target_arch = "aarch64",
                        target_arch = "x86",
                        target_arch = "x86_64",
                        target_arch = "s390x"
                    )
                ))]
                {
                    let task_dump = match timeout(
                        TASK_DUMP_TIMEOUT,
                        tokio::runtime::Handle::current().dump(),
                    )
                    .await
                    {
                        Ok(dump) => {
                            let mut output = String::new();
                            for (i, task) in dump.tasks().iter().enumerate() {
                                output.push_str(&format!("task {i}:\n{}\n", task.trace()));
                            }
                            output
                        }
                        Err(_) => format!(
                            "task dump timed out after {}ms — a runtime worker may be blocked",
                            TASK_DUMP_TIMEOUT.as_millis()
                        ),
                    };
                    files.insert("runtime-dump.txt".to_string(), cap_artifact_data(task_dump.into_bytes()));
                }

                Ok(tonic::Response::new(GetFlareFilesResponse { files }))
            })
            .await;
    }
}

/// A worker that keeps the registration with the Core Agent alive.
///
/// It refreshes the registration periodically, and registers again whenever a refresh fails.
pub struct RemoteAgentRegistrationWorker {
    registration: Arc<Handoff<Registration>>,
}

impl RemoteAgentRegistrationWorker {
    fn share(&self) -> Self {
        Self {
            registration: Arc::clone(&self.registration),
        }
    }
}

#[async_trait]
impl Supervisable for RemoteAgentRegistrationWorker {
    fn name(&self) -> &str {
        &self.registration.name
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        Ok(self.registration.take_over(process_shutdown))
    }
}

/// A worker that feeds the config stream from [`RemoteAgentBootstrap::create_config_stream`].
///
/// It forwards configuration events from the Core Agent, and reconnects whenever the stream from the Core Agent ends.
/// The config stream stays open across restarts of the worker.
pub struct ConfigStreamWorker {
    stream: Handoff<ConfigStream>,
}

impl ConfigStreamWorker {
    /// Starts forwarding events from `source` in a task until the returned worker takes it over.
    fn spawn(source: Box<dyn ConfigEventSource>, session_id: SessionIdHandle) -> (mpsc::Receiver<ConfigUpdate>, Self) {
        let (sender, receiver) = mpsc::channel(CONFIG_STREAM_CAPACITY);
        let stream = ConfigStream::new(source, sender, session_id);
        let worker = Self {
            stream: Handoff::spawn(CONFIG_STREAM_WORKER_NAME.to_string(), stream),
        };

        (receiver, worker)
    }
}

#[async_trait]
impl Supervisable for ConfigStreamWorker {
    fn name(&self) -> &str {
        &self.stream.name
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        Ok(self.stream.take_over(process_shutdown))
    }
}

/// A worker that captures the dataspace from the supervisor context and stores it in a shared
/// [`OnceLock`] for use by the diagnostic artifact collection service.
///
/// The gRPC handler runs in tasks spawned by tonic, which do not inherit tokio task-locals.
/// This worker bridges that gap by capturing the dataspace during its own `initialize()` call,
/// which runs inside the supervisor process where the dataspace is available.
pub struct DataspaceAnchorWorker {
    dataspace: Arc<OnceLock<DataspaceRegistry>>,
}

#[async_trait]
impl Supervisable for DataspaceAnchorWorker {
    fn name(&self) -> &str {
        "flare-dataspace-anchor"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        if let Some(ds) = DataspaceRegistry::try_current() {
            let _ = self.dataspace.set(ds);
        }

        Ok(Box::pin(async move {
            process_shutdown.await;
            Ok(())
        }))
    }
}

/// A worker that serves the status, flare, and telemetry services on the privileged API endpoint.
///
/// The services are asserted as a dynamic gRPC route, so the privileged API endpoint in the same supervision tree serves
/// them for as long as this worker runs.
pub struct RemoteAgentServicesWorker {
    routes: Routes,
}

#[async_trait]
impl Supervisable for RemoteAgentServicesWorker {
    fn name(&self) -> &str {
        "remote-agent-services"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let route = DynamicRoute::grpc(EndpointType::Privileged, self.routes.clone());

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            dataspace.assert(route, "remote-agent-services");

            process_shutdown.await;
            Ok(())
        }))
    }
}

/// A worker that reports diagnostic events to the Core Agent as remote agent events.
///
/// It subscribes to [`DiagnosticEvent`]s emitted anywhere in the process converts the ones it recognizes into the
/// Datadog Agent's remote agent event representation, and reports them to the Core Agent over the remote agent gRPC
/// API. `InvalidApiKey` events are rate limited, so a sustained stream of them cannot flood the Core Agent.
pub struct RemoteAgentEventReporter {
    client: RemoteAgentClient,
    session_id: SessionIdHandle,
}

#[async_trait]
impl Supervisable for RemoteAgentEventReporter {
    fn name(&self) -> &str {
        "remote_agent_event_reporter"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let client = self.client.clone();
        let session_id = self.session_id.clone();

        Ok(Box::pin(async move {
            let subscription = subscribe_events(IdentifierFilter::all())
                .map_err(|e| generic_error!("Failed to subscribe to diagnostic events: {}", e))?;

            select! {
                _ = process_shutdown => Ok(()),
                result = run_event_reporter(client, session_id, subscription) => result,
            }
        }))
    }
}

async fn run_event_reporter(
    mut client: RemoteAgentClient, session_id: SessionIdHandle, mut subscription: Subscription<DiagnosticEvent>,
) -> Result<(), GenericError> {
    debug!("Remote Agent event reporter started.");

    let mut invalid_api_key_reports = InvalidApiKeyReportThrottle::default();

    while let Some(update) = subscription.recv().await {
        let DataspaceUpdate::Message(_, event) = update else {
            continue;
        };

        let Some(remote_agent_event) = diagnostic_to_remote_agent_event(&event) else {
            debug!("Received diagnostic event with no remote agent event mapping; skipping.");
            continue;
        };

        let Some(session_id) = session_id.get() else {
            debug!("No active remote agent session; dropping diagnostic event.");
            continue;
        };

        let invalid_api_key = matches!(event.details(), &DiagnosticDetails::InvalidApiKey);
        if invalid_api_key && !invalid_api_key_reports.is_allowed() {
            debug!("Throttling an InvalidApiKey event report.");
            continue;
        }

        let request = ReportRemoteAgentEventRequest {
            session_id: session_id.as_str().to_string(),
            events: vec![remote_agent_event],
        };

        match client.report_remote_agent_event(request).await {
            Ok(_) => {
                // The window opens only on delivery, so a failed report never holds back the next one.
                if invalid_api_key {
                    invalid_api_key_reports.record();
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to report remote agent event to the Datadog Agent.");
            }
        }
    }

    debug!("Remote Agent event reporter stopped.");
    Ok(())
}

/// Minimum time between `InvalidApiKey` reports.
const INVALID_API_KEY_REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// Rate limits `InvalidApiKey` reports.
///
/// Checking and recording are separate so that only a delivered report opens the window: a report that never
/// reaches the Core Agent leaves the next one free to send immediately.
#[derive(Debug, Default)]
struct InvalidApiKeyReportThrottle {
    last_reported: Option<Instant>,
}

impl InvalidApiKeyReportThrottle {
    fn is_allowed(&self) -> bool {
        self.last_reported
            .is_none_or(|last| last.elapsed() >= INVALID_API_KEY_REPORT_INTERVAL)
    }

    fn record(&mut self) {
        self.last_reported = Some(Instant::now());
    }
}

/// Converts a [`DiagnosticEvent`] into the Datadog Agent's remote agent event representation.
///
/// Returns `None` for diagnostic details that have no corresponding remote agent event variant, in which case the
/// event is not reported.
fn diagnostic_to_remote_agent_event(event: &DiagnosticEvent) -> Option<RemoteAgentEvent> {
    let details = match event.details() {
        DiagnosticDetails::InvalidApiKey => RemoteAgentEventDetails::InvalidApiKey(InvalidApiKeyEvent {}),
        // `DiagnosticDetails` is `#[non_exhaustive]`; any future variant without a remote agent mapping is skipped.
        _ => return None,
    };

    Some(RemoteAgentEvent {
        message: event.message().to_string(),
        details: Some(details),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use saluki_core::{
        data_model::event::{
            metric::{context::Context, Metric},
            Event,
        },
        observability::metrics::{MetricsSnapshot, Processor as _},
        runtime::Supervisor,
    };

    use super::*;

    #[test]
    fn remote_agent_process_instance_id_is_shared_across_registrations() {
        let address = || GrpcTargetAddress::Tcp("127.0.0.1:1234".parse().unwrap());
        let first = RegistrationDetails::new(address(), vec![]);
        assert!(uuid::Uuid::parse_str(first.process_instance_id).is_ok());
        assert_eq!(first.pid, std::process::id());

        // Each bootstrap creates fresh registration details, but all of them share one process identity.
        let second = RegistrationDetails::new(address(), vec![]);
        assert_eq!(second.pid, first.pid);
        assert_eq!(second.process_instance_id, first.process_instance_id);
    }

    #[test]
    fn cap_artifact_data_leaves_small_payloads_unchanged() {
        let input = b"hello world".to_vec();
        let output = cap_artifact_data(input.clone());
        assert_eq!(output, input);
    }

    #[test]
    fn cap_artifact_data_truncates_at_limit_and_appends_marker() {
        let input = vec![b'x'; DIAGNOSTIC_ARTIFACT_MAX_BYTES + 1024];
        let output = cap_artifact_data(input);

        // Total length is capped at the limit plus the marker.
        assert_eq!(
            output.len(),
            DIAGNOSTIC_ARTIFACT_MAX_BYTES + DIAGNOSTIC_TRUNCATION_MARKER.len()
        );

        // The first DIAGNOSTIC_ARTIFACT_MAX_BYTES bytes are all 'x'.
        assert!(output[..DIAGNOSTIC_ARTIFACT_MAX_BYTES].iter().all(|&b| b == b'x'));

        // The marker is appended at the end.
        assert!(output.ends_with(DIAGNOSTIC_TRUNCATION_MARKER));
    }

    #[test]
    fn cap_artifact_data_at_exact_limit_is_not_truncated() {
        let input = vec![b'y'; DIAGNOSTIC_ARTIFACT_MAX_BYTES];
        let output = cap_artifact_data(input.clone());
        assert_eq!(output, input);
    }

    fn metrics_state(names: &[&'static str]) -> AggregatedMetricsState {
        let processor = AggregatedMetricsProcessor;
        let state = processor.build_initial_state();
        let upserts = names
            .iter()
            .map(|name| Event::Metric(Metric::counter(Context::from_static_parts(name, &[]), 1.0)))
            .collect();
        processor.process(
            MetricsSnapshot {
                upserts,
                evictions: Vec::new(),
            },
            &state,
        );
        state
    }

    #[tokio::test]
    async fn telemetry_without_remapper_rules_is_empty() {
        let state = metrics_state(&["test.events_received_total"]);
        let processor = telemetry_processor(Vec::new());

        assert_eq!(render_telemetry(processor.as_ref(), &state).await, "");
    }

    #[tokio::test]
    async fn telemetry_only_reports_metrics_matched_by_remapper_rules() {
        let state = metrics_state(&["test.events_received_total", "test.unmatched_total"]);
        let processor = telemetry_processor(vec![RemapperRule::by_name(
            "test.events_received_total",
            "events_received",
        )]);

        let prom_text = render_telemetry(processor.as_ref(), &state).await;
        assert!(prom_text.contains("events_received 1"), "{prom_text}");
        assert!(!prom_text.contains("unmatched"), "{prom_text}");
    }

    #[test]
    fn diagnostic_to_remote_agent_event_maps_invalid_api_key() {
        let event = DiagnosticEvent::new("credentials rejected", DiagnosticDetails::InvalidApiKey);
        let converted =
            diagnostic_to_remote_agent_event(&event).expect("InvalidApiKey should map to a remote agent event");

        assert_eq!(converted.message, "credentials rejected");
        assert!(matches!(
            converted.details,
            Some(RemoteAgentEventDetails::InvalidApiKey(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_api_key_reports_are_throttled_to_thirty_seconds() {
        let mut throttle = InvalidApiKeyReportThrottle::default();

        assert!(throttle.is_allowed());
        throttle.record();
        assert!(!throttle.is_allowed());

        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(throttle.is_allowed());
        throttle.record();
        assert!(!throttle.is_allowed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_slightly_before_the_window_elapses_is_held() {
        let mut throttle = InvalidApiKeyReportThrottle::default();

        throttle.record();
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(!throttle.is_allowed());
    }

    #[test]
    fn a_report_that_was_never_delivered_does_not_open_the_throttle_window() {
        let mut throttle = InvalidApiKeyReportThrottle::default();

        assert!(throttle.is_allowed());
        assert!(throttle.is_allowed());
        assert!(throttle.is_allowed());

        throttle.record();
        assert!(!throttle.is_allowed());
    }

    /// A Datadog Agent for registration and the config stream to talk to.
    ///
    /// Registration responses are scripted, and an unscripted registration is rejected. Every call is recorded with the
    /// number of seconds since the fake was created, which is exact with a paused clock.
    #[derive(Clone)]
    struct FakeAgent {
        state: Arc<std::sync::Mutex<FakeAgentState>>,
    }

    struct FakeAgentState {
        created: Instant,
        calls: Vec<(u64, Call)>,
        registrations: VecDeque<RegisterRemoteAgentResponse>,
        rejected_registrations: usize,
        failed_refreshes: usize,
        hung_refreshes: usize,
        streams: VecDeque<mpsc::UnboundedReceiver<ConfigEvent>>,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Register,
        Refresh(String),
        Open(String),
    }

    impl FakeAgent {
        fn new() -> Self {
            Self {
                state: Arc::new(std::sync::Mutex::new(FakeAgentState {
                    created: Instant::now(),
                    calls: Vec::new(),
                    registrations: VecDeque::new(),
                    rejected_registrations: 0,
                    failed_refreshes: 0,
                    hung_refreshes: 0,
                    streams: VecDeque::new(),
                })),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, FakeAgentState> {
            self.state.lock().unwrap()
        }

        fn accept_registration(&self, session_id: &str, recommended_refresh_interval_secs: u32) {
            self.lock().registrations.push_back(RegisterRemoteAgentResponse {
                session_id: session_id.to_string(),
                recommended_refresh_interval_secs,
            });
        }

        /// Rejects the next registration, ahead of any scripted response.
        fn reject_next_registration(&self) {
            self.lock().rejected_registrations += 1;
        }

        fn fail_next_refresh(&self) {
            self.lock().failed_refreshes += 1;
        }

        /// Never answers the next refresh, ahead of any failed one.
        fn hang_next_refresh(&self) {
            self.lock().hung_refreshes += 1;
        }

        /// Queues a stream for the next `open`, and returns the sender that feeds it.
        fn add_stream(&self) -> mpsc::UnboundedSender<ConfigEvent> {
            let (events_tx, events_rx) = mpsc::unbounded_channel();
            self.lock().streams.push_back(events_rx);
            events_tx
        }

        fn calls(&self) -> Vec<(u64, Call)> {
            self.lock().calls.clone()
        }

        /// Returns `true` if nothing other than this handle holds the fake.
        fn is_released(&self) -> bool {
            Arc::strong_count(&self.state) == 1
        }
    }

    impl FakeAgentState {
        fn record(&mut self, call: Call) {
            let elapsed = self.created.elapsed().as_secs();
            self.calls.push((elapsed, call));
        }
    }

    #[async_trait]
    impl RegistryClient for FakeAgent {
        async fn register(&mut self, _: &RegistrationDetails) -> Result<RegisterRemoteAgentResponse, GenericError> {
            let mut state = self.lock();
            state.record(Call::Register);
            if state.rejected_registrations > 0 {
                state.rejected_registrations -= 1;
                return Err(generic_error!("registration rejected"));
            }
            state
                .registrations
                .pop_front()
                .ok_or_else(|| generic_error!("registration rejected"))
        }

        async fn refresh(&mut self, session_id: &SessionId) -> Result<(), GenericError> {
            let hang = {
                let mut state = self.lock();
                state.record(Call::Refresh(session_id.to_string()));
                if state.hung_refreshes > 0 {
                    state.hung_refreshes -= 1;
                    true
                } else if state.failed_refreshes > 0 {
                    state.failed_refreshes -= 1;
                    return Err(generic_error!("unknown session"));
                } else {
                    false
                }
            };
            if hang {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }

    impl ConfigEventSource for FakeAgent {
        fn open(&mut self, session_id: &SessionId) -> ConfigEventStream {
            let mut state = self.lock();
            state.record(Call::Open(session_id.to_string()));
            match state.streams.pop_front() {
                Some(mut events) => Box::pin(futures::stream::poll_fn(move |cx| {
                    events.poll_recv(cx).map(|event| event.map(Ok))
                })),
                None => Box::pin(futures::stream::pending()),
            }
        }
    }

    fn registration(agent: &FakeAgent, session_id: &SessionIdHandle) -> Registration {
        let details = RegistrationDetails {
            pid: 1,
            process_instance_id: "test-instance",
            display_name: "Test Agent".to_string(),
            flavor: "test_agent".to_string(),
            api_listen_addr: "localhost:5101".to_string(),
            service_names: Vec::new(),
        };
        Registration::new(Box::new(agent.clone()), details, session_id.clone())
    }

    async fn registration_worker(agent: &FakeAgent, session_id: &SessionIdHandle) -> RemoteAgentRegistrationWorker {
        registration(agent, session_id)
            .register_and_spawn()
            .await
            .expect("registration should succeed")
    }

    fn config_stream_worker(
        agent: &FakeAgent, session_id: &SessionIdHandle,
    ) -> (mpsc::Receiver<ConfigUpdate>, ConfigStreamWorker) {
        ConfigStreamWorker::spawn(Box::new(agent.clone()), session_id.clone())
    }

    /// Starts `worker` the way a supervisor does, returning the coordinator that stops it and the task that runs it.
    async fn start(worker: &dyn Supervisable) -> (ShutdownCoordinator, tokio::task::JoinHandle<()>) {
        let (coordinator, shutdown) = ShutdownHandle::paired();
        let run = worker.initialize(shutdown).await.expect("worker should initialize");
        let task = tokio::spawn(async move { run.await.expect("worker should exit cleanly") });
        (coordinator, task)
    }

    async fn stop(coordinator: ShutdownCoordinator, task: tokio::task::JoinHandle<()>) {
        coordinator.shutdown();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker should stop promptly")
            .expect("worker task should not panic");
    }

    fn update_event(key: &str) -> ConfigEvent {
        ConfigEvent {
            event: Some(datadog_protos::agent::config_event::Event::Update(
                datadog_protos::agent::ConfigUpdate {
                    origin: "core-agent".to_string(),
                    sequence_id: 1,
                    setting: Some(datadog_protos::agent::ConfigSetting {
                        source: "file".to_string(),
                        key: key.to_string(),
                        value: Some(prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue("value".to_string())),
                        }),
                        ..Default::default()
                    }),
                },
            )),
        }
    }

    async fn next_key(updates: &mut mpsc::Receiver<ConfigUpdate>) -> String {
        let update = tokio::time::timeout(Duration::from_secs(60), updates.recv())
            .await
            .expect("an update should arrive");
        match update {
            Some(ConfigUpdate::Partial(setting)) => setting.key,
            other => panic!("expected a partial update, got {other:?}"),
        }
    }

    fn session(id: &str) -> SessionIdHandle {
        let handle = SessionIdHandle::empty();
        handle.update(Some(SessionId::new(id).unwrap()));
        handle
    }

    fn session_id_of(handle: &SessionIdHandle) -> Option<String> {
        handle.get().map(|session_id| session_id.to_string())
    }

    #[tokio::test(start_paused = true)]
    async fn registration_worker_refreshes_after_the_recommended_interval_and_then_every_thirty_seconds() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        let session_id = SessionIdHandle::empty();

        let worker = registration_worker(&agent, &session_id).await;
        let _running = start(&worker).await;
        tokio::time::sleep(Duration::from_secs(75)).await;

        // Only the first refresh follows the Agent's recommendation; the timer's own period paces the rest.
        let refresh = || Call::Refresh("session-1".to_string());
        assert_eq!(
            agent.calls(),
            vec![(0, Call::Register), (10, refresh()), (40, refresh()), (70, refresh())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn registration_worker_registers_again_after_a_failed_refresh() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        let session_id = SessionIdHandle::empty();
        let worker = registration_worker(&agent, &session_id).await;

        // The refresh at 10s fails, the registration 5s later is rejected, and the next one 30s after that succeeds.
        agent.fail_next_refresh();
        agent.reject_next_registration();
        agent.accept_registration("session-2", 10);
        let _running = start(&worker).await;

        tokio::time::sleep(Duration::from_secs(12)).await;
        assert_eq!(
            session_id_of(&session_id),
            None,
            "a failed refresh should clear the session"
        );

        tokio::time::sleep(Duration::from_secs(44)).await;
        assert_eq!(
            agent.calls(),
            vec![
                (0, Call::Register),
                (10, Call::Refresh("session-1".to_string())),
                (15, Call::Register),
                (45, Call::Register),
                (55, Call::Refresh("session-2".to_string())),
            ]
        );
        assert_eq!(session_id_of(&session_id).as_deref(), Some("session-2"));
    }

    #[tokio::test(start_paused = true)]
    async fn registration_worker_keeps_the_session_and_schedule_across_takeover_and_restart() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        let session_id = SessionIdHandle::empty();
        let worker = registration_worker(&agent, &session_id).await;

        // The worker takes over from the task at 3s, stops at 5s, and is started again at 7s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let (coordinator, task) = start(&worker).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        stop(coordinator, task).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _running = start(&worker).await;

        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(
            agent.calls(),
            vec![(0, Call::Register), (10, Call::Refresh("session-1".to_string()))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_refresh_in_flight_which_a_restart_retries_right_away() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        agent.hang_next_refresh();
        let session_id = SessionIdHandle::empty();
        let worker = registration_worker(&agent, &session_id).await;

        // The refresh at 10s never returns. The worker is stopped at 12s, and started again at 14s.
        let (coordinator, task) = start(&worker).await;
        tokio::time::sleep(Duration::from_secs(12)).await;
        stop(coordinator, task).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _running = start(&worker).await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        let refresh = || Call::Refresh("session-1".to_string());
        assert_eq!(
            agent.calls(),
            vec![(0, Call::Register), (10, refresh()), (14, refresh())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_a_worker_and_its_task_while_the_task_is_in_an_attempt() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        agent.hang_next_refresh();
        let session_id = SessionIdHandle::empty();
        let worker = registration_worker(&agent, &session_id).await;

        // The task's refresh at 10s never returns, so the worker that starts at 12s is still waiting for the task.
        tokio::time::sleep(Duration::from_secs(12)).await;
        let (coordinator, task) = start(&worker).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        stop(coordinator, task).await;

        drop(worker);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(agent.is_released(), "the task should stop with the worker");
    }

    #[tokio::test(start_paused = true)]
    async fn failed_first_registration_leaves_nothing_running() {
        let agent = FakeAgent::new();
        agent.reject_next_registration();
        let session_id = SessionIdHandle::empty();

        let result = registration(&agent, &session_id).register_and_spawn().await;
        assert!(result.is_err(), "a rejected registration should fail");

        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(agent.calls(), vec![(0, Call::Register)]);
        assert!(agent.is_released(), "nothing should still hold the client");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_the_registration_and_config_stream_workers() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        let events = agent.add_stream();
        let session_id = SessionIdHandle::empty();
        let registration = registration_worker(&agent, &session_id).await;
        let (mut updates, config_stream) = config_stream_worker(&agent, &session_id);

        let mut supervisor = Supervisor::new("test-sup").unwrap();
        supervisor.add_worker(registration);
        supervisor.add_worker(config_stream);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

        events.send(update_event("before_shutdown")).unwrap();
        assert_eq!(next_key(&mut updates).await, "before_shutdown");
        tokio::time::sleep(Duration::from_secs(15)).await;

        // A worker that ignored the signal would be halted at its shutdown timeout, which fails the shutdown.
        shutdown_tx.send(()).unwrap();
        let result = run.await.expect("supervisor task should not panic");
        assert!(result.is_ok(), "shutdown should be clean, got {result:?}");

        let calls = agent.calls();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(agent.calls(), calls, "registration should not continue after shutdown");
        assert!(events.is_closed(), "the config stream should be closed");
        assert!(
            updates.recv().await.is_none(),
            "the config stream channel should be closed"
        );
        assert!(agent.is_released(), "nothing should still hold the client");
    }

    #[tokio::test(start_paused = true)]
    async fn config_stream_worker_continues_the_same_stream_without_losing_an_update() {
        let agent = FakeAgent::new();
        let events = agent.add_stream();
        let session_id = session("session-1");

        // With the channel full, `b` is waiting for room in it when the worker takes over, and `c` hasn't been read
        // from the stream yet.
        let (mut updates, worker) = config_stream_worker(&agent, &session_id);
        let queued = (0..CONFIG_STREAM_CAPACITY)
            .map(|i| format!("queued-{i}"))
            .collect::<Vec<_>>();
        for key in queued.iter().map(String::as_str).chain(["b", "c"]) {
            events.send(update_event(key)).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let _running = start(&worker).await;
        events.send(update_event("d")).unwrap();

        for key in queued.iter().map(String::as_str).chain(["b", "c", "d"]) {
            assert_eq!(next_key(&mut updates).await, key);
        }
        assert_eq!(agent.calls(), vec![(0, Call::Open("session-1".to_string()))]);
    }

    #[tokio::test(start_paused = true)]
    async fn config_stream_worker_restarts_on_the_same_stream_and_channel() {
        let agent = FakeAgent::new();
        let events = agent.add_stream();
        let session_id = session("session-1");
        let (mut updates, worker) = config_stream_worker(&agent, &session_id);

        let (coordinator, task) = start(&worker).await;
        events.send(update_event("a")).unwrap();
        assert_eq!(next_key(&mut updates).await, "a");
        stop(coordinator, task).await;

        // `b` arrives while the worker is stopped.
        events.send(update_event("b")).unwrap();
        let _running = start(&worker).await;
        events.send(update_event("c")).unwrap();

        for key in ["b", "c"] {
            assert_eq!(next_key(&mut updates).await, key);
        }
        assert_eq!(agent.calls(), vec![(0, Call::Open("session-1".to_string()))]);
    }

    #[tokio::test(start_paused = true)]
    async fn config_stream_worker_waits_for_shutdown_once_the_receiver_is_dropped() {
        let agent = FakeAgent::new();
        let events = agent.add_stream();
        let session_id = session("session-1");
        let (mut updates, worker) = config_stream_worker(&agent, &session_id);
        let (coordinator, task) = start(&worker).await;
        events.send(update_event("a")).unwrap();
        assert_eq!(next_key(&mut updates).await, "a");

        drop(updates);
        events.send(update_event("b")).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;

        assert!(events.is_closed(), "the config stream should be closed");
        assert!(!task.is_finished(), "the worker should keep running until shutdown");
        stop(coordinator, task).await;
        assert_eq!(agent.calls(), vec![(0, Call::Open("session-1".to_string()))]);
    }

    #[tokio::test(start_paused = true)]
    async fn config_stream_reconnects_five_seconds_after_the_stream_ends_with_the_current_session() {
        let agent = FakeAgent::new();
        let first = agent.add_stream();
        let second = agent.add_stream();
        let session_id = session("session-1");
        let (mut updates, worker) = config_stream_worker(&agent, &session_id);
        let _running = start(&worker).await;

        first.send(update_event("a")).unwrap();
        assert_eq!(next_key(&mut updates).await, "a");
        session_id.update(Some(SessionId::new("session-2").unwrap()));
        drop(first);

        second.send(update_event("b")).unwrap();
        assert_eq!(next_key(&mut updates).await, "b");
        assert_eq!(
            agent.calls(),
            vec![
                (0, Call::Open("session-1".to_string())),
                (5, Call::Open("session-2".to_string()))
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tasks_stop_when_their_workers_are_dropped_without_starting() {
        let agent = FakeAgent::new();
        agent.accept_registration("session-1", 10);
        agent.hang_next_refresh();
        let events = agent.add_stream();
        let session_id = SessionIdHandle::empty();
        let registration = registration_worker(&agent, &session_id).await;
        let (mut updates, config_stream) = config_stream_worker(&agent, &session_id);

        events.send(update_event("a")).unwrap();
        assert_eq!(next_key(&mut updates).await, "a");

        // The registration task is still in the refresh at 10s, which never returns, when its worker is dropped.
        tokio::time::sleep(Duration::from_secs(12)).await;
        drop(registration);
        drop(config_stream);
        tokio::time::sleep(Duration::from_secs(120)).await;

        assert_eq!(
            agent.calls(),
            vec![
                (0, Call::Register),
                (0, Call::Open("session-1".to_string())),
                (10, Call::Refresh("session-1".to_string()))
            ]
        );
        assert!(events.is_closed(), "the config stream should be closed");
        assert!(
            updates.recv().await.is_none(),
            "the config stream channel should be closed"
        );
        assert!(agent.is_released(), "nothing should still hold the client");
    }
}
