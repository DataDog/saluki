use std::collections::HashMap;
use std::pin::Pin;
use std::sync::OnceLock;
use std::{collections::hash_map::Entry, sync::Arc, time::Duration};

use agent_data_plane_config::SalukiConfiguration;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_agent_commons::ipc::{
    client::RemoteAgentClient,
    config::RemoteAgentClientConfiguration,
    session::{SessionId, SessionIdHandle},
};
use datadog_protos::agent::v1::{
    event::Details as RemoteAgentEventDetails, Event as RemoteAgentEvent, InvalidApiKeyEvent,
    ReportRemoteAgentEventRequest,
};
use datadog_protos::agent::{
    command::v1::{
        execute_command_response::Frame as ExecuteCommandFrame,
        remote_command_provider_server::{RemoteCommandProvider, RemoteCommandProviderServer},
        Command as RemoteCommand, CommandParameter, CommandProvider, ExecuteCommandRequest, ExecuteCommandResponse,
        ListCommandsRequest, ListCommandsResponse, ParameterType,
    },
    config_event,
    flare::v1::{flare_provider_server::*, *},
    status::v1::{status_provider_server::*, *},
    telemetry::v1::{get_telemetry_response::*, telemetry_provider_server::*, *},
    ConfigSetting as AgentConfigSetting, ConfigSnapshot,
};
use futures::{Stream, StreamExt};
use process_memory::Querier as MemoryQuerier;
use prost_types::{value::Kind, Struct};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_common::task::spawn_traced_named;
use saluki_config::dynamic::{ConfigSetting, ConfigUpdate, Provenance};
use saluki_core::{
    diagnostic::{subscribe_events, DiagnosticCollector, DiagnosticDetails, DiagnosticEvent},
    observability::metrics::{get_shared_metrics_state, AggregatedMetricsProcessor, Reflector, TelemetryProcessor},
    runtime::{
        state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter, Subscription},
        InitializationError, Supervisable, SupervisorFuture,
    },
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::GrpcTargetAddress;
use serde_json::{Map, Value};
use tokio::task::spawn_blocking;
use tokio::time::{timeout, Instant};
use tokio::{
    select,
    sync::{mpsc, oneshot, Mutex},
    time::{interval, MissedTickBehavior},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{server::NamedService, Status};
use tracing::{debug, error, info, warn};

use crate::state::metrics::get_datadog_agent_remappings;
use crate::{
    cli::dogstatsd::{parse_remote_dogstatsd_command, run_dogstatsd_command},
    config::DataPlaneConfiguration,
};

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

fn dogstatsd_command_provider() -> CommandProvider {
    CommandProvider {
        name: "dogstatsd".to_string(),
        description: "Inspect DogStatsD pipeline status".to_string(),
        commands: vec![
            remote_command(
                "stats",
                "Print basic statistics about metrics received by the data plane.",
                vec![
                    command_parameter(
                        "duration-secs",
                        "d",
                        "Amount of time to collect statistics for, in seconds.",
                        ParameterType::TypeUint,
                        true,
                    ),
                    command_parameter(
                        "mode",
                        "m",
                        "Analysis mode: summary or cardinality.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "sort-dir",
                        "s",
                        "Sort direction: asc or desc.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "filter",
                        "f",
                        "Exclude metrics whose names do not contain this value.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "limit",
                        "l",
                        "Maximum number of metrics to display.",
                        ParameterType::TypeUint,
                        false,
                    ),
                ],
            ),
            remote_command(
                "capture",
                "Start a DogStatsD traffic capture.",
                vec![
                    command_parameter(
                        "duration",
                        "d",
                        "Capture duration in Go duration syntax.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "path",
                        "p",
                        "Directory in which to write the capture.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "compressed",
                        "z",
                        "Whether to zstd-compress the capture file.",
                        ParameterType::TypeBool,
                        false,
                    ),
                ],
            ),
            remote_command(
                "replay",
                "Replay DogStatsD traffic from a capture file.",
                vec![
                    command_parameter(
                        "file",
                        "f",
                        "Path to the .dog or .dog.zstd capture file to replay.",
                        ParameterType::TypeString,
                        true,
                    ),
                    command_parameter(
                        "loops",
                        "l",
                        "Number of replay iterations; 0 repeats until cancelled.",
                        ParameterType::TypeUint,
                        false,
                    ),
                ],
            ),
            remote_command(
                "top",
                "Display DogStatsD contexts with the highest cardinality.",
                vec![
                    command_parameter(
                        "path",
                        "p",
                        "Read a context dump artifact instead of requesting one.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "num-metrics",
                        "m",
                        "Maximum number of metrics to display.",
                        ParameterType::TypeUint,
                        false,
                    ),
                    command_parameter(
                        "num-tags",
                        "t",
                        "Maximum number of tags to display per metric.",
                        ParameterType::TypeUint,
                        false,
                    ),
                ],
            ),
            remote_command(
                "dump-contexts",
                "Write currently tracked DogStatsD contexts as JSON.",
                Vec::new(),
            ),
        ],
    }
}

fn remote_command(name: &str, helper: &str, parameters: Vec<CommandParameter>) -> RemoteCommand {
    RemoteCommand {
        name: name.to_string(),
        short_name: name.to_string(),
        helper: helper.to_string(),
        parameters,
        is_runnable: true,
        ..Default::default()
    }
}

fn command_parameter(
    name: &str, short_name: &str, helper: &str, parameter_type: ParameterType, required: bool,
) -> CommandParameter {
    CommandParameter {
        name: name.to_string(),
        short_name: short_name.to_string(),
        helper: helper.to_string(),
        r#type: parameter_type.into(),
        required,
        is_flag: true,
        is_persistent: false,
    }
}

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
    pub async fn new<'a>(
        client_config: &RemoteAgentClientConfiguration, dp_config: &DataPlaneConfiguration<'a>,
    ) -> Result<Self, GenericError> {
        let secure_api_listen_address = dp_config.secure_api_listen_address()?;
        let api_listen_addr = GrpcTargetAddress::try_from_listen_addr(&secure_api_listen_address)
            .ok_or_else(|| generic_error!("Failed to get valid gRPC target address from secure API listen address."))?;

        // Generate our remote agent state, which is mostly fixed but has a few dynamic bits.
        let service_names = vec![
            <StatusProviderServer<()> as NamedService>::NAME.to_string(),
            <FlareProviderServer<()> as NamedService>::NAME.to_string(),
            <TelemetryProviderServer<()> as NamedService>::NAME.to_string(),
            <RemoteCommandProviderServer<RemoteCommandProviderImpl> as NamedService>::NAME.to_string(),
        ];

        let (state, init_reg_rx) = RemoteAgentState::new(api_listen_addr, service_names);
        let session_id = state.session_id.clone();

        // Create our client, and then immediately start the registration loop.
        //
        // Wait for the result of the initial registration attempt before proceeding.
        let client = RemoteAgentClient::connect(client_config).await?;
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
            internal_metrics: get_shared_metrics_state(),
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

    /// Creates a worker that captures the dataspace from the supervisor context and makes it available to the
    /// diagnostic artifact collection service.
    ///
    /// This worker must be added to the control plane supervisor. When it initializes, it runs inside the supervisor
    /// process where the dataspace task-local is set, and fills the shared [`OnceLock`] so that `get_flare_files` can
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

    /// Creates a remote-command service bound to the current runtime configuration.
    pub fn create_command_service(
        &self, current_config: Arc<arc_swap::ArcSwap<SalukiConfiguration>>,
    ) -> RemoteCommandProviderServer<RemoteCommandProviderImpl> {
        RemoteCommandProviderServer::new(RemoteCommandProviderImpl {
            session_id: self.session_id.clone(),
            current_config,
        })
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
                            Some(ConfigUpdate::Snapshot(snapshot_to_settings(&snapshot)))
                        }
                        Some(config_event::Event::Update(update)) => update
                            .setting
                            .as_ref()
                            .map(|setting| ConfigUpdate::Partial(setting_to_config_setting(setting))),
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

/// Sources that indicate the Agent supplied the value rather than an operator.
const AGENT_DEFAULT_SOURCE: &str = "default";
/// A value that was not set by the user nor does the schema define default value for.
const AGENT_DECLARED_ONLY_SOURCE: &str = "schema";
const AGENT_UNSET_SOURCES: [&str; 2] = [AGENT_DEFAULT_SOURCE, AGENT_DECLARED_ONLY_SOURCE];

/// Converts a setting from the Agent's RPC wire protocol to our `ConfigSetting` type.
fn setting_to_config_setting(setting: &AgentConfigSetting) -> ConfigSetting {
    let provenance = if AGENT_UNSET_SOURCES.contains(&setting.source.as_str()) {
        Provenance::Default
    } else {
        Provenance::Explicit
    };

    ConfigSetting::new(
        setting.key.clone(),
        proto_value_to_serde_value(&setting.value),
        provenance,
    )
}

/// Converts a `ConfigSnapshot` into the settings it carries.
fn snapshot_to_settings(snapshot: &ConfigSnapshot) -> Vec<ConfigSetting> {
    snapshot.settings.iter().map(setting_to_config_setting).collect()
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

pub(crate) struct RemoteCommandProviderImpl {
    session_id: SessionIdHandle,
    current_config: Arc<arc_swap::ArcSwap<SalukiConfiguration>>,
}

impl RemoteCommandProviderImpl {
    fn response_with_session_id<T>(&self, value: T) -> Result<tonic::Response<T>, Status> {
        let session_id = self
            .session_id
            .get()
            .ok_or(Status::failed_precondition(
                "session ID not set; must be registered with Core Agent",
            ))?
            .to_grpc_header_value();
        let mut response = tonic::Response::new(value);
        response.metadata_mut().append(SESSION_ID_METADATA_KEY, session_id);
        Ok(response)
    }
}

const MAX_REMOTE_COMMAND_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

struct RemoteCommandOutput {
    bytes: Vec<u8>,
}

impl RemoteCommandOutput {
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl std::io::Write for RemoteCommandOutput {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let new_len = self.bytes.len().saturating_add(buffer.len());
        if new_len > MAX_REMOTE_COMMAND_OUTPUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "remote command output exceeds the 16 MiB limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CancellableCommandStream {
    inner: ReceiverStream<Result<ExecuteCommandResponse, Status>>,
    cancellation: CancellationToken,
}

impl Stream for CancellableCommandStream {
    type Item = Result<ExecuteCommandResponse, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>, context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}

impl Drop for CancellableCommandStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[async_trait]
impl RemoteCommandProvider for RemoteCommandProviderImpl {
    type ExecuteCommandStream = Pin<Box<dyn Stream<Item = Result<ExecuteCommandResponse, Status>> + Send>>;

    async fn list_commands(
        &self, _request: tonic::Request<ListCommandsRequest>,
    ) -> Result<tonic::Response<ListCommandsResponse>, Status> {
        self.response_with_session_id(ListCommandsResponse {
            providers: vec![dogstatsd_command_provider()],
        })
    }

    async fn execute_command(
        &self, request: tonic::Request<ExecuteCommandRequest>,
    ) -> Result<tonic::Response<Self::ExecuteCommandStream>, Status> {
        let (sender, receiver) = mpsc::channel(128);
        let cancellation = CancellationToken::new();
        let response = self.response_with_session_id(Box::pin(CancellableCommandStream {
            inner: ReceiverStream::new(receiver),
            cancellation: cancellation.clone(),
        }) as Self::ExecuteCommandStream)?;
        let command = if request.get_ref().provider_name != "dogstatsd" {
            Err(generic_error!(
                "unknown remote command provider `{}`",
                request.get_ref().provider_name
            ))
        } else {
            parse_remote_dogstatsd_command(
                &request.get_ref().command_path,
                request.get_ref().arguments.as_ref().unwrap_or(&Struct::default()),
            )
        };

        let current_config = Arc::clone(&self.current_config);
        tokio::spawn(async move {
            let exit_code = match command {
                Ok(command) => {
                    let mut output = RemoteCommandOutput { bytes: Vec::new() };
                    let result =
                        run_dogstatsd_command(&current_config.load_full(), command, &mut output, &cancellation, true)
                            .await;
                    let stdout = output.into_bytes();
                    if !stdout.is_empty() {
                        let _ = sender
                            .send(Ok(ExecuteCommandResponse {
                                frame: Some(ExecuteCommandFrame::Stdout(
                                    String::from_utf8_lossy(&stdout).into_owned(),
                                )),
                            }))
                            .await;
                    }
                    match result {
                        Ok(()) => 0,
                        Err(error) => {
                            let _ = sender
                                .send(Ok(ExecuteCommandResponse {
                                    frame: Some(ExecuteCommandFrame::Stderr(format!("{error:#}\n"))),
                                }))
                                .await;
                            1
                        }
                    }
                }
                Err(error) => {
                    let _ = sender
                        .send(Ok(ExecuteCommandResponse {
                            frame: Some(ExecuteCommandFrame::Stderr(format!("{error:#}\n"))),
                        }))
                        .await;
                    1
                }
            };
            let _ = sender
                .send(Ok(ExecuteCommandResponse {
                    frame: Some(ExecuteCommandFrame::ExitCode(exit_code)),
                }))
                .await;
        });

        Ok(response)
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
                files.insert("runtime_debug_info.log".to_string(), cap_artifact_data(process_info.into_bytes()));

                // Tokio task dump (Linux-only).
                //
                // Wrapped in an explicit timeout because `Handle::dump()`
                // may never resolve if a runtime worker is blocked for
                // more than 250ms
                #[cfg(target_os = "linux")]
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

/// A worker that reports diagnostic events to the Core Agent as remote agent events.
///
/// It subscribes to [`DiagnosticEvent`]s emitted anywhere in the process converts the ones it recognizes into the
/// Datadog Agent's remote agent event representation, and reports them to the Core Agent over the remote agent gRPC
/// API.
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

        let request = ReportRemoteAgentEventRequest {
            session_id: session_id.as_str().to_string(),
            events: vec![remote_agent_event],
        };

        if let Err(e) = client.report_remote_agent_event(request).await {
            warn!(error = %e, "Failed to report remote agent event to the Datadog Agent.");
        }
    }

    debug!("Remote Agent event reporter stopped.");
    Ok(())
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

    fn agent_setting(source: &str, key: &str, value: &str) -> AgentConfigSetting {
        AgentConfigSetting {
            source: source.to_string(),
            key: key.to_string(),
            value: Some(prost_types::Value {
                kind: Some(Kind::StringValue(value.to_string())),
            }),
        }
    }

    #[test]
    fn an_agent_default_is_marked_as_a_default() {
        let setting = setting_to_config_setting(&agent_setting(
            AGENT_DEFAULT_SOURCE,
            "dd_url",
            "https://app.datadoghq.com",
        ));

        assert_eq!(setting.key, "dd_url");
        assert_eq!(setting.value, Value::from("https://app.datadoghq.com"));
        assert_eq!(setting.provenance, Provenance::Default);
    }

    #[test]
    fn a_schema_setting_is_marked_as_a_default() {
        let setting = setting_to_config_setting(&AgentConfigSetting {
            source: "schema".to_string(),
            key: "api_key".to_string(),
            value: None,
        });

        assert_eq!(setting.value, Value::Null);
        assert_eq!(setting.provenance, Provenance::Default);
    }

    #[test]
    fn null_values_are_preserved_with_their_provenance() {
        for source in [AGENT_DEFAULT_SOURCE, "schema", "file", "remote-config"] {
            let setting = setting_to_config_setting(&AgentConfigSetting {
                source: source.to_string(),
                key: "api_key".to_string(),
                value: Some(prost_types::Value {
                    kind: Some(Kind::NullValue(0)),
                }),
            });

            let expected_provenance = if [AGENT_DEFAULT_SOURCE, "schema"].contains(&source) {
                Provenance::Default
            } else {
                Provenance::Explicit
            };
            assert_eq!(setting.value, Value::Null);
            assert_eq!(setting.provenance, expected_provenance, "source {source}");
        }
    }

    #[test]
    fn an_empty_string_value_is_kept_with_its_provenance() {
        // An empty string is still a value; provenance comes from its source, not its content.
        for (source, provenance) in [
            ("file", Provenance::Explicit),
            ("default", Provenance::Default),
            ("schema", Provenance::Default),
        ] {
            let setting = setting_to_config_setting(&agent_setting(source, "site", ""));

            assert_eq!(setting.value, Value::from(""));
            assert_eq!(setting.provenance, provenance);
        }
    }

    #[test]
    fn operator_supplied_sources_are_marked_as_explicit() {
        // Unknown sources are treated as explicit inputs rather than defaults.
        for source in [
            "file",
            "environment-variable",
            "remote-config",
            "cli",
            "source-from-the-future",
        ] {
            let setting = setting_to_config_setting(&agent_setting(source, "dd_url", "https://app.datadoghq.eu"));

            assert_eq!(
                setting.provenance,
                Provenance::Explicit,
                "source {source} should be explicit"
            );
        }
    }

    #[test]
    fn snapshot_settings_keep_order_values_and_provenance() {
        let snapshot = ConfigSnapshot {
            origin: "core-agent".to_string(),
            sequence_id: 1,
            settings: vec![
                agent_setting("file", "site", "datadoghq.eu"),
                agent_setting("default", "dd_url", "https://app.datadoghq.com"),
                agent_setting(AGENT_DECLARED_ONLY_SOURCE, "api_key", ""),
            ],
        };

        let settings = snapshot_to_settings(&snapshot);

        assert_eq!(
            settings,
            vec![
                ConfigSetting::explicit("site", Value::from("datadoghq.eu")),
                ConfigSetting::new("dd_url", Value::from("https://app.datadoghq.com"), Provenance::Default),
                ConfigSetting::new("api_key", Value::from(""), Provenance::Default),
            ]
        );
    }

    #[test]
    fn dogstatsd_command_provider_describes_every_remote_command() {
        let provider = dogstatsd_command_provider();

        assert_eq!(provider.name, "dogstatsd");
        assert_eq!(provider.description, "Inspect DogStatsD pipeline status");
        assert_eq!(
            provider
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["stats", "capture", "replay", "top", "dump-contexts"]
        );

        let stats = &provider.commands[0];
        assert!(stats.is_runnable);
        assert_eq!(stats.parameters[0].name, "duration-secs");
        assert_eq!(stats.parameters[0].short_name, "d");
        assert!(stats.parameters[0].required);
        assert!(stats.parameters[0].is_flag);
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
}
