use std::collections::HashMap;
use std::sync::OnceLock;
use std::{collections::hash_map::Entry, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_agent_commons::ipc::session::{SessionId, SessionIdHandle};
use datadog_protos::agent::{
    config_event,
    flare::v1::{flare_provider_server::*, *},
    status::v1::{status_provider_server::*, *},
    telemetry::v1::{get_telemetry_response::*, telemetry_provider_server::*, *},
    ConfigSnapshot,
};
use futures::StreamExt;
use process_memory::Querier as MemoryQuerier;
use prost_types::value::Kind;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_common::task::spawn_traced_named;
use saluki_config::{dynamic::ConfigUpdate, upsert, GenericConfiguration};
use saluki_core::{
    diagnostic::DiagnosticHandle,
    observability::metrics::{get_shared_metrics_state, AggregatedMetricsProcessor, Reflector, TelemetryProcessor},
    runtime::{
        state::{DataspaceRegistry, IdentifierFilter},
        InitializationError, Supervisable, SupervisorFuture,
    },
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::GrpcTargetAddress;
use serde_json::{Map, Value};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    time::{interval, MissedTickBehavior},
};
use tonic::{server::NamedService, Status};
use tracing::{debug, error, info, warn};

use crate::config::DataPlaneConfiguration;
use crate::state::metrics::get_datadog_agent_remappings;

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(5);

const EVENTS_RECEIVED: &str = "adp.component_events_received_total";
const PACKETS_RECEIVED: &str = "adp.component_packets_received_total";
const BYTES_RECEIVED: &str = "adp.component_bytes_received_total";
const ERRORS: &str = "adp.component_errors_total";
const DSD_COMP_ID: &str = "component_id:dsd_in";
const ERROR_DECODE: &str = "error_type:decode";
const ERROR_FRAMING: &str = "error_type:framing";
const TYPE_EVENTS: &str = "message_type:events";
const TYPE_METRICS: &str = "message_type:metrics";
const TYPE_SERVICE_CHECKS: &str = "message_type:service_checks";
const LISTENER_UDP: &str = "listener_type:udp";
const LISTENER_UNIX: &str = "listener_type:unix";
const LISTENER_UNIXGRAM: &str = "listener_type:unixgram";
const SESSION_ID_METADATA_KEY: &str = "session_id";

/// Remote agent initialization.
///
/// This helper type is used to coordinate the initialization of remote agent state by registering to the Core Agent and
/// acquiring the necessary information to allow initialization of ADP itself to proceed.
pub struct RemoteAgentBootstrap {
    client: RemoteAgentClient,
    session_id: SessionIdHandle,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
    dataspace: Arc<OnceLock<DataspaceRegistry>>,
}

impl RemoteAgentBootstrap {
    /// Creates a new `RemoteAgentBootstrap` from the given configurations.
    ///
    /// A remote agent client is created and immediately attempts to register with the Core Agent. This function does
    /// not return until registration finishes, whether successful or not.
    ///
    /// # Errors
    ///
    /// If the configuration is invalid, an error is returned.
    pub async fn from_configuration(
        config: &GenericConfiguration, dp_config: &DataPlaneConfiguration,
    ) -> Result<Self, GenericError> {
        let api_listen_addr = GrpcTargetAddress::try_from_listen_addr(dp_config.secure_api_listen_address())
            .ok_or_else(|| generic_error!("Failed to get valid gRPC target address from secure API listen address."))?;

        // Generate our remote agent state, which is mostly fixed but has a few dynamic bits.
        let service_names = vec![
            <StatusProviderServer<()> as NamedService>::NAME.to_string(),
            <FlareProviderServer<()> as NamedService>::NAME.to_string(),
            <TelemetryProviderServer<()> as NamedService>::NAME.to_string(),
        ];

        let (state, init_reg_rx) = RemoteAgentState::new(api_listen_addr, service_names);
        let session_id = state.session_id.clone();

        // Create our client, and then immediately start the registration loop.
        //
        // Wait for the result of the initial registration attempt before proceeding.
        let client = RemoteAgentClient::from_configuration(config).await?;
        spawn_traced_named(
            "adp-remote-agent-task",
            run_remote_agent_registration_loop(client.clone(), state),
        );

        match init_reg_rx.await {
            Ok(Ok(())) => (),
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(generic_error!(
                    "Failed to initialize remote agent state. Registration task failed unexpectedly."
                ))
            }
        }

        Ok(Self {
            client,
            session_id,
            internal_metrics: get_shared_metrics_state().await,
            dataspace: Arc::new(OnceLock::new()),
        })
    }

    fn build_impl(&self) -> RemoteAgentImpl {
        RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            processor: Mutex::new(TelemetryProcessor::new().with_remapper_rules(get_datadog_agent_remappings())),
            session_id: self.session_id.clone(),
            dataspace: Arc::clone(&self.dataspace),
        }
    }

    /// Creates a worker that captures the dataspace from the supervisor context and makes it
    /// available to the diagnostic artifact collection service.
    ///
    /// This worker must be added to the control plane supervisor. When it initializes, it runs
    /// inside the supervisor process where the dataspace task-local is set, and fills the shared
    /// [`OnceLock`] so that `get_flare_files` can collect diagnostic artifacts at request time.
    /// Without this, the gRPC handler tasks spawned by tonic would not have access to the
    /// dataspace since tonic's `tokio::spawn` does not propagate task-locals.
    pub fn create_dataspace_anchor(&self) -> DataspaceAnchorWorker {
        DataspaceAnchorWorker {
            dataspace: Arc::clone(&self.dataspace),
        }
    }

    /// Creates a new `StatusProviderServer` tied to this remote agent.
    pub fn create_status_service(&self) -> StatusProviderServer<RemoteAgentImpl> {
        StatusProviderServer::new(self.build_impl())
    }

    /// Creates a new `TelemetryProviderServer` tied to this remote agent.
    pub fn create_telemetry_service(&self) -> TelemetryProviderServer<RemoteAgentImpl> {
        TelemetryProviderServer::new(self.build_impl())
    }

    /// Creates a new `FlareProviderServer` tied to this remote agent.
    pub fn create_flare_service(&self) -> FlareProviderServer<RemoteAgentImpl> {
        FlareProviderServer::new(self.build_impl())
    }

    /// Creates a config stream that receives configuration events from the Core Agent.
    pub fn create_config_stream(&self) -> mpsc::Receiver<ConfigUpdate> {
        let (sender, receiver) = mpsc::channel(100);

        let client = self.client.clone();
        let session_id = self.session_id.clone();

        tokio::spawn(run_config_stream_event_loop(client, sender, session_id));

        receiver
    }
}

struct RemoteAgentState {
    pid: u32,
    display_name: String,
    flavor: String,
    api_listen_addr: String,
    session_id: SessionIdHandle,
    service_names: Vec<String>,
    initial_registration_tx: Option<oneshot::Sender<Result<(), GenericError>>>,
}

impl RemoteAgentState {
    fn new(
        api_listen_addr: GrpcTargetAddress, service_names: Vec<String>,
    ) -> (Self, oneshot::Receiver<Result<(), GenericError>>) {
        let app_details = saluki_metadata::get_app_details();
        let display_name = app_details.full_name().to_string();
        let flavor = app_details.full_name().replace(" ", "_").to_lowercase();

        let (init_reg_tx, init_reg_rx) = oneshot::channel();

        let state = Self {
            pid: std::process::id(),
            display_name,
            flavor,
            api_listen_addr: api_listen_addr.to_string(),
            session_id: SessionIdHandle::empty(),
            service_names,
            initial_registration_tx: Some(init_reg_tx),
        };

        (state, init_reg_rx)
    }
}

async fn run_remote_agent_registration_loop(mut client: RemoteAgentClient, mut state: RemoteAgentState) {
    let mut loop_timer = interval(DEFAULT_REFRESH_INTERVAL);
    loop_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    debug!("Remote Agent registration task started.");

    loop {
        loop_timer.tick().await;

        match state.session_id.get() {
            Some(session_id) => {
                debug!(%session_id, "Refreshing registration with Datadog Agent.");

                if client.refresh_remote_agent(&session_id).await.is_err() {
                    loop_timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                    state.session_id.update(None);
                    warn!("Failed to refresh registration with the Datadog Agent. Resetting session ID and attempting to re-register shortly.");

                    continue;
                }
            }
            None => {
                match client
                    .register_remote_agent(
                        state.pid,
                        &state.display_name,
                        &state.flavor,
                        &state.api_listen_addr,
                        state.service_names.clone(),
                    )
                    .await
                {
                    Ok(resp) => {
                        let resp = resp.into_inner();
                        let new_session_id = match SessionId::new(&resp.session_id) {
                            Ok(session_id) => session_id,
                            Err(e) => {
                                warn!(error = %e, "Received invalid session ID from Datadog Agent after registation. Registration will be retried periodically in the background.");
                                loop_timer.reset_after(DEFAULT_REFRESH_INTERVAL);
                                continue;
                            }
                        };
                        let new_refresh_interval = resp.recommended_refresh_interval_secs;
                        info!(session_id = %new_session_id, "Successfully registered with the Datadog Agent. Refreshing every {} seconds.", new_refresh_interval);

                        state.session_id.update(Some(new_session_id));
                        loop_timer.reset_after(Duration::from_secs(new_refresh_interval as u64));

                        if let Some(tx) = state.initial_registration_tx.take() {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to register with the Datadog Agent. Registration will be retried periodically in the background.");
                        loop_timer.reset_after(DEFAULT_REFRESH_INTERVAL);

                        if let Some(tx) = state.initial_registration_tx.take() {
                            let _ = tx.send(Err(e));
                        }
                    }
                }
            }
        }
    }
}

async fn run_config_stream_event_loop(
    mut client: RemoteAgentClient, sender: mpsc::Sender<ConfigUpdate>, session_id: SessionIdHandle,
) {
    loop {
        debug!("Establishing a new config stream connection to the Core Agent.");

        // Read the current session ID.
        //
        // We do this every loop since it can change in the background due to re-registration.
        let current_session_id = session_id.wait_for_update().await;

        let mut stream = client.stream_config_events(&current_session_id);
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    let update = match event.event {
                        Some(config_event::Event::Snapshot(snapshot)) => {
                            let map = snapshot_to_map(&snapshot);
                            Some(ConfigUpdate::Snapshot(map))
                        }
                        Some(config_event::Event::Update(update)) => {
                            if let Some(setting) = update.setting {
                                let v = proto_value_to_serde_value(&setting.value);
                                Some(ConfigUpdate::Partial {
                                    key: setting.key,
                                    value: v,
                                })
                            } else {
                                None
                            }
                        }
                        None => {
                            error!("Received a configuration update event with no data.");
                            None
                        }
                    };

                    if let Some(update) = update {
                        if sender.send(update).await.is_err() {
                            warn!("Dynamic configuration channel closed. Config stream shutting down.");
                            return;
                        }
                    }
                }
                Err(e) => {
                    error!("Error while reading config event stream: {}.", e);
                }
            }
        }

        debug!("Config stream ended, retrying in 5 seconds...");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Converts a `ConfigSnapshot` into a nested `serde_json::Value::Object`.
fn snapshot_to_map(snapshot: &ConfigSnapshot) -> Value {
    let mut root = Value::Object(Map::new());

    for setting in &snapshot.settings {
        let value = proto_value_to_serde_value(&setting.value);
        upsert(&mut root, &setting.key, value);
    }

    root
}

/// Recursively converts a `google::protobuf::Value` into a `serde_json::Value`.
fn proto_value_to_serde_value(proto_val: &Option<prost_types::Value>) -> Value {
    let Some(kind) = proto_val.as_ref().and_then(|v| v.kind.as_ref()) else {
        return Value::Null;
    };

    match kind {
        Kind::NullValue(_) => Value::Null,
        Kind::NumberValue(n) => {
            if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Value::from(*n as i64)
            } else {
                Value::from(*n)
            }
        }
        Kind::StringValue(s) => Value::String(s.clone()),
        Kind::BoolValue(b) => Value::Bool(*b),
        Kind::StructValue(s) => {
            let json_map: Map<String, Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), proto_value_to_serde_value(&Some(v.clone()))))
                .collect();
            Value::Object(json_map)
        }
        Kind::ListValue(l) => {
            let json_list: Vec<Value> = l
                .values
                .iter()
                .map(|v| proto_value_to_serde_value(&Some(v.clone())))
                .collect();
            Value::Array(json_list)
        }
    }
}

pub struct RemoteAgentImpl {
    started: DateTime<Utc>,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
    processor: Mutex<TelemetryProcessor>,
    session_id: SessionIdHandle,
    dataspace: Arc<OnceLock<DataspaceRegistry>>,
}

impl RemoteAgentImpl {
    fn write_dsd_metrics(&self, builder: &mut StatusBuilder) {
        // Grab some simple metrics from the DogStatsD source.
        let metrics = self.internal_metrics.state();

        let event_packets = metrics.get_aggregated_with_tags(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_EVENTS]);
        let metric_packets = metrics.get_aggregated_with_tags(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_METRICS]);
        let scheck_packets = metrics.get_aggregated_with_tags(EVENTS_RECEIVED, &[DSD_COMP_ID, TYPE_SERVICE_CHECKS]);

        let event_parse_errors = metrics.get_aggregated_with_tags(ERRORS, &[DSD_COMP_ID, ERROR_DECODE, TYPE_EVENTS]);
        let metric_parse_errors = metrics.get_aggregated_with_tags(ERRORS, &[DSD_COMP_ID, ERROR_DECODE, TYPE_METRICS]);
        let scheck_parse_errors =
            metrics.get_aggregated_with_tags(ERRORS, &[DSD_COMP_ID, ERROR_DECODE, TYPE_SERVICE_CHECKS]);

        let get_listener_metrics = |listener_type: &str| {
            (
                metrics.get_aggregated_with_tags(BYTES_RECEIVED, &[DSD_COMP_ID, listener_type]),
                metrics
                    .find_single_with_tags(ERRORS, &[DSD_COMP_ID, listener_type, ERROR_FRAMING])
                    .unwrap_or(0.0),
                metrics
                    .find_single_with_tags(PACKETS_RECEIVED, &[DSD_COMP_ID, listener_type, "state:ok"])
                    .unwrap_or(0.0),
            )
        };

        let (udp_bytes, udp_errors, udp_packets) = get_listener_metrics(LISTENER_UDP);
        let (unix_bytes, unix_errors, unix_packets) = get_listener_metrics(LISTENER_UNIX);
        let (unixgram_bytes, unixgram_errors, unixgram_packets) = get_listener_metrics(LISTENER_UNIXGRAM);

        let uds_bytes = unix_bytes + unixgram_bytes;
        let uds_errors = unix_errors + unixgram_errors;
        let uds_packets = unix_packets + unixgram_packets;

        builder
            .named_section("DogStatsD")
            .set_field("Event Packets", event_packets.to_string())
            .set_field("Event Parse Errors", event_parse_errors.to_string())
            .set_field("Metric Packets", metric_packets.to_string())
            .set_field("Metric Parse Errors", metric_parse_errors.to_string())
            .set_field("Service Check Packets", scheck_packets.to_string())
            .set_field("Service Check Parse Errors", scheck_parse_errors.to_string())
            .set_field("Udp Bytes", udp_bytes.to_string())
            .set_field("Udp Packet Reading Errors", udp_errors.to_string())
            .set_field("Udp Packets", udp_packets.to_string())
            .set_field("Uds Bytes", uds_bytes.to_string())
            .set_field("Uds Packet Reading Errors", uds_errors.to_string())
            .set_field("Uds Packets", uds_packets.to_string());
    }

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

                self.write_dsd_metrics(&mut builder);

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
                let state = self.internal_metrics.state();
                let mut processor = self.processor.lock().await;
                let prom_text = processor.process(state);

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
#[cfg(target_os = "linux")]
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

/// Caps `bytes` to [`DIAGNOSTIC_ARTIFACT_MAX_BYTES`], appending [`DIAGNOSTIC_TRUNCATION_MARKER`] if truncated.
fn cap_artifact(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() > DIAGNOSTIC_ARTIFACT_MAX_BYTES {
        bytes.truncate(DIAGNOSTIC_ARTIFACT_MAX_BYTES);
        bytes.extend_from_slice(DIAGNOSTIC_TRUNCATION_MARKER);
    }
    bytes
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

                // Grab and collect all asserted diagnostic handles
                if let Some(dataspace) = self.dataspace.get() {
                    let handles = dataspace.current_values::<DiagnosticHandle>(IdentifierFilter::all());
                    let total_handles = handles.len();
                    let deadline = tokio::time::Instant::now() + DIAGNOSTIC_COLLECT_TIMEOUT;

                    for handle in handles {
                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }

                        let artifact_name = handle.artifact_name().to_string();
                        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());

                        let bytes = match tokio::time::timeout(
                            remaining,
                            tokio::task::spawn_blocking(move || handle.collect()),
                        )
                        .await
                        {
                            Ok(Ok(bytes)) => bytes,
                            Ok(Err(e)) => format!("collection panicked for '{artifact_name}': {e}").into_bytes(),
                            Err(_) => format!("collection timed out for '{artifact_name}'").into_bytes(),
                        };

                        files.insert(artifact_name, cap_artifact(bytes));
                    }

                    if files.len() < total_handles {
                        warn!("Diagnostic artifact collection timed out; collected {}/{total_handles} artifacts.", files.len());
                    }
                }

                // Process-level artifacts.
                //
                // These don't belong to any individual component — they describe the ADP process
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
                files.insert("runtime_debug_info.log".to_string(), cap_artifact(process_info.into_bytes()));

                // Tokio task dump (Linux-only).
                //
                // Wrapped in an explicit timeout because `Handle::dump()`
                // may never resolve if a runtime worker is blocked for
                // more than 250ms
                #[cfg(target_os = "linux")]
                {
                    let task_dump = match tokio::time::timeout(
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
                    files.insert("runtime-dump.txt".to_string(), cap_artifact(task_dump.into_bytes()));
                }

                Ok(tonic::Response::new(GetFlareFilesResponse { files }))
            })
            .await;
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

struct StatusBuilder {
    main_section: StatusSection,
    named_sections: HashMap<String, StatusSection>,
}

impl StatusBuilder {
    fn new() -> Self {
        Self {
            main_section: StatusSection { fields: HashMap::new() },
            named_sections: HashMap::new(),
        }
    }

    fn main_section(&mut self) -> StatusSectionWriter<'_> {
        StatusSectionWriter {
            section: &mut self.main_section,
        }
    }

    fn named_section<S: AsRef<str>>(&mut self, name: S) -> StatusSectionWriter<'_> {
        match self.named_sections.entry(name.as_ref().to_string()) {
            Entry::Occupied(entry) => StatusSectionWriter {
                section: entry.into_mut(),
            },
            Entry::Vacant(entry) => {
                let section = entry.insert(StatusSection { fields: HashMap::new() });
                StatusSectionWriter { section }
            }
        }
    }

    fn into_response(self) -> GetStatusDetailsResponse {
        GetStatusDetailsResponse {
            main_section: Some(self.main_section),
            named_sections: self.named_sections,
        }
    }
}

struct StatusSectionWriter<'a> {
    section: &'a mut StatusSection,
}

impl StatusSectionWriter<'_> {
    fn set_field<S: AsRef<str>, V: AsRef<str>>(&mut self, name: S, value: V) -> &mut Self {
        self.section
            .fields
            .insert(name.as_ref().to_string(), value.as_ref().to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_artifact_leaves_small_payloads_unchanged() {
        let input = b"hello world".to_vec();
        let output = cap_artifact(input.clone());
        assert_eq!(output, input);
    }

    #[test]
    fn cap_artifact_truncates_at_limit_and_appends_marker() {
        let input = vec![b'x'; DIAGNOSTIC_ARTIFACT_MAX_BYTES + 1024];
        let output = cap_artifact(input);

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
    fn cap_artifact_at_exact_limit_is_not_truncated() {
        let input = vec![b'y'; DIAGNOSTIC_ARTIFACT_MAX_BYTES];
        let output = cap_artifact(input.clone());
        assert_eq!(output, input);
    }
}
