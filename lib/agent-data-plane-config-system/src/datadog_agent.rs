//! Typed Datadog Agent attachment layer.
//!
//! This module separates pure typed configuration parsing from the side-effectful IPC connection,
//! and provides per-consumer typed attachments built atop one shared connection. It is the
//! config-system's realization of the design's "Remote Agent and Provider Attachments" section.
//!
//! The layering is deliberate:
//!
//! - [`RemoteAgentClientConfiguration`] is pure, typed config. It is parsed at the config-system
//!   boundary from the typed [`BootstrapConfiguration`] (specifically its
//!   [`AgentIpcBootstrap`] slice). It performs no I/O.
//! - [`connect`] performs the side effects: it constructs the underlying `datadog-agent-commons`
//!   `RemoteAgentClient`, registers with the Core Agent, and spawns the registration-refresh loop.
//!   Per the design, `connect` accepts only the typed [`RemoteAgentClientConfiguration`], never
//!   `GenericConfiguration`.
//! - [`DatadogAgentConnection`] is the retained, [`Arc`]-shared session capability. It owns the
//!   underlying client and the live session id, and exposes the config-event stream plus accessors
//!   used to build attachments.
//! - [`Attachments`] is the typed bundle handed to the binary: one thin typed capability per
//!   consumer (status, flare, telemetry, metrics/remote tagger, autodiscovery, host tags), each
//!   built atop the shared connection.
//!
//! # Design
//!
//! The underlying commons `RemoteAgentClient` only constructs from a
//! `saluki_config_tools::GenericConfiguration` (there is no typed constructor in commons). To keep
//! this crate's public surface typed, [`connect`] materializes a `GenericConfiguration` internally,
//! from the typed [`RemoteAgentClientConfiguration`], and confines that raw-map type entirely to
//! this crate (which is the only ADP production crate permitted raw-map APIs). No
//! `GenericConfiguration` appears in any public signature here.

use std::sync::Arc;
use std::time::Duration;

use agent_data_plane_config::{AgentIpcBootstrap, BootstrapConfiguration};
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_agent_commons::ipc::session::{SessionId, SessionIdHandle};
use datadog_protos::agent::{config_event, ConfigSnapshot};
use futures::StreamExt as _;
use saluki_config_tools::dynamic::ConfigUpdate;
use saluki_config_tools::{upsert, ConfigurationLoader};
use saluki_error::{generic_error, GenericError};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CONFIG_STREAM_CHANNEL_DEPTH: usize = 100;

/// Pure, typed configuration needed to connect to the Datadog Agent IPC endpoint.
///
/// This struct holds everything required to establish the IPC connection. It is parsed once, at the
/// config-system boundary, from the typed [`BootstrapConfiguration`] (its
/// [`AgentIpcBootstrap`] slice) via
/// [`from_bootstrap`](Self::from_bootstrap). It is the typed analogue of the commons
/// `RemoteAgentClientConfiguration`, which is parsed from a raw map; the fields mirror the Agent IPC
/// keys that commons reads (`cmd_port`, `agent_ipc_endpoint`, `auth_token_file_path`).
///
/// Fields not surfaced by the bootstrap allowlist (the IPC certificate path, the gRPC max message
/// size, connect-retry tuning, and `vsock_addr`) are intentionally left to the defaults applied by
/// commons when it parses the materialized map. The bootstrap slice is deliberately narrow; adding a
/// field here requires adding it to [`AgentIpcBootstrap`]
/// first.
///
/// This type performs no I/O. All side effects happen in [`connect`].
#[derive(Clone, Debug, Default)]
pub struct RemoteAgentClientConfiguration {
    /// The local command port used to reach the Agent IPC endpoint.
    ///
    /// Takes precedence over `agent_ipc_endpoint` when set. Defaults to unset (commons then uses the
    /// endpoint, or its own `https://127.0.0.1:5001` default).
    cmd_port: Option<u16>,

    /// The Agent IPC endpoint URI.
    ///
    /// Ignored when `cmd_port` is set. Defaults to unset (commons then uses `https://127.0.0.1:5001`).
    agent_ipc_endpoint: Option<String>,

    /// Path to the Agent authentication token file.
    ///
    /// Defaults to unset (commons then derives the platform-specific default path).
    auth_token_file_path: Option<String>,
}

impl RemoteAgentClientConfiguration {
    /// Builds the connection configuration from the typed bootstrap slice.
    ///
    /// This reads the [`AgentIpcBootstrap`] slice of the
    /// bootstrap configuration. That slice is the typed source for Agent IPC connection parameters:
    /// it is loaded once from local Datadog sources (`datadog.yaml` / `DD_*`) before runtime
    /// authority exists, which is exactly where IPC connection params must come from.
    pub fn from_bootstrap(bootstrap: &BootstrapConfiguration) -> Self {
        Self::from_agent_ipc(bootstrap.datadog.agent_ipc())
    }

    /// Builds the connection configuration directly from the Agent IPC bootstrap slice.
    ///
    /// Prefer [`from_bootstrap`](Self::from_bootstrap) at the config-system boundary; this exists for
    /// callers that already hold the narrower slice.
    pub fn from_agent_ipc(agent_ipc: &AgentIpcBootstrap) -> Self {
        Self {
            cmd_port: agent_ipc.cmd_port,
            agent_ipc_endpoint: agent_ipc.agent_ipc_endpoint.clone(),
            auth_token_file_path: agent_ipc.auth_token_file_path.clone(),
        }
    }

    /// Materializes the typed config into a `GenericConfiguration` for the commons client.
    ///
    /// This is the single, confined point where the typed config crosses into the raw-map world that
    /// `datadog-agent-commons` requires. The raw-map type never escapes this crate.
    async fn into_generic(self) -> Result<saluki_config_tools::GenericConfiguration, GenericError> {
        let mut map = Map::new();
        if let Some(cmd_port) = self.cmd_port {
            map.insert("cmd_port".to_string(), Value::from(cmd_port));
        }
        if let Some(endpoint) = self.agent_ipc_endpoint {
            map.insert("agent_ipc_endpoint".to_string(), Value::from(endpoint));
        }
        if let Some(path) = self.auth_token_file_path {
            map.insert("auth_token_file_path".to_string(), Value::from(path));
        }

        ConfigurationLoader::default()
            .add_providers([figment::providers::Serialized::defaults(Value::Object(map))])
            .into_generic()
            .await
            .map_err(|e| generic_error!("Failed to materialize Agent IPC client configuration: {}", e))
    }
}

/// Registration parameters for the remote-agent session.
///
/// These describe the ADP process to the Core Agent during registration. They are typed inputs to
/// [`connect`]; resolving them (for example, deriving the gRPC listen address from
/// `ControlConfiguration`) is the binary's job, not this module's.
#[derive(Clone, Debug)]
pub struct RemoteAgentRegistration {
    /// The process id of this ADP process.
    pub pid: u32,
    /// The human-readable display name reported to the Agent.
    pub display_name: String,
    /// The flavor string reported to the Agent.
    pub flavor: String,
    /// The gRPC API endpoint the Agent should call back on (the secure API listen address).
    pub api_endpoint: String,
    /// The fully qualified gRPC service names this process serves to the Agent.
    pub service_names: Vec<String>,
}

/// Connects to the Datadog Agent IPC endpoint and registers a remote-agent session.
///
/// This is the side-effectful entry point. It constructs the underlying commons `RemoteAgentClient`,
/// performs the initial registration synchronously (returning only after the first attempt
/// completes), and spawns the background registration-refresh loop. The returned
/// [`DatadogAgentConnection`] is the retained capability shared by all attachments.
///
/// Per the design, this accepts only the typed [`RemoteAgentClientConfiguration`]; it must not
/// accept `GenericConfiguration`.
///
/// # Errors
///
/// Returns an error if the client cannot be created (invalid endpoint, missing or invalid auth
/// token, unreachable Agent) or if the initial registration fails.
pub async fn connect(
    config: RemoteAgentClientConfiguration, registration: RemoteAgentRegistration,
) -> Result<DatadogAgentConnection, GenericError> {
    let generic = config.into_generic().await?;
    let client = RemoteAgentClient::from_configuration(&generic).await?;

    let session_id = SessionIdHandle::empty();
    let (init_tx, init_rx) = oneshot::channel();

    tokio::spawn(run_registration_loop(
        client.clone(),
        session_id.clone(),
        registration,
        init_tx,
    ));

    match init_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(generic_error!(
                "Remote agent registration task failed before reporting a result."
            ))
        }
    }

    Ok(DatadogAgentConnection {
        inner: Arc::new(ConnectionInner { client, session_id }),
    })
}

/// The retained, shared Datadog Agent session capability.
///
/// Owns the underlying commons `RemoteAgentClient` and the live session id. It is cheaply cloneable:
/// internally it is an [`Arc`], so all clones (and all [`Attachments`]) share one connection and one
/// session. Config-system establishes this once and hands out typed attachments built atop it.
#[derive(Clone)]
pub struct DatadogAgentConnection {
    inner: Arc<ConnectionInner>,
}

struct ConnectionInner {
    client: RemoteAgentClient,
    session_id: SessionIdHandle,
}

impl DatadogAgentConnection {
    /// Returns a clone of the underlying commons client.
    ///
    /// The commons client is itself cheaply cloneable (it wraps shared gRPC channels). Attachments
    /// use this to hand each consumer its own client handle.
    pub fn client(&self) -> RemoteAgentClient {
        self.inner.client.clone()
    }

    /// Returns a handle to the live session id.
    ///
    /// The handle reflects re-registration that happens in the background; readers always observe the
    /// current session.
    pub fn session_id(&self) -> SessionIdHandle {
        self.inner.session_id.clone()
    }

    /// Opens the config-event stream and returns a receiver of typed [`ConfigUpdate`]s.
    ///
    /// This spawns a background task that owns a clone of the client, waits for a valid session,
    /// streams config events from the Core Agent, converts each event into a [`ConfigUpdate`], and
    /// forwards it on the returned channel. The task reconnects on stream errors and re-reads the
    /// session id each loop so it follows background re-registration.
    ///
    /// The returned receiver is what the config-system update router consumes in stream mode.
    pub fn config_stream(&self) -> mpsc::Receiver<ConfigUpdate> {
        let (sender, receiver) = mpsc::channel(CONFIG_STREAM_CHANNEL_DEPTH);
        tokio::spawn(run_config_stream_loop(
            self.inner.client.clone(),
            self.inner.session_id.clone(),
            sender,
        ));
        receiver
    }

    /// Builds the typed per-consumer attachment bundle atop this shared connection.
    pub fn attachments(&self) -> Attachments {
        Attachments::new(self.clone())
    }
}

/// A typed bundle of per-consumer attachments, each built atop one shared [`DatadogAgentConnection`].
///
/// The binary owns the service implementations (status, flare, telemetry, metrics/remote tagger,
/// autodiscovery, host tags) but must not resolve authority or touch raw config. Each field here is
/// the typed capability for one consumer; the binary builds each service on its specific attachment
/// instead of reaching for a shared client or the raw map.
///
/// All attachments share the one underlying connection, so this bundle is cheap to construct and
/// clone.
#[derive(Clone)]
pub struct Attachments {
    /// Attachment for the status provider service.
    pub status: StatusAttachment,
    /// Attachment for the flare provider service.
    pub flare: FlareAttachment,
    /// Attachment for the telemetry provider service.
    pub telemetry: TelemetryAttachment,
    /// Attachment for the metrics / remote tagger consumer.
    pub metrics: MetricsAttachment,
    /// Attachment for the autodiscovery consumer.
    pub autodiscovery: AutodiscoveryAttachment,
    /// Attachment for the host-tags consumer.
    pub host_tags: HostTagsAttachment,
}

impl Attachments {
    fn new(connection: DatadogAgentConnection) -> Self {
        Self {
            status: StatusAttachment {
                connection: connection.clone(),
            },
            flare: FlareAttachment {
                connection: connection.clone(),
            },
            telemetry: TelemetryAttachment {
                connection: connection.clone(),
            },
            metrics: MetricsAttachment {
                connection: connection.clone(),
            },
            autodiscovery: AutodiscoveryAttachment {
                connection: connection.clone(),
            },
            host_tags: HostTagsAttachment { connection },
        }
    }
}

/// Generates a thin typed attachment that exposes only what its consumer needs from the shared
/// connection.
macro_rules! attachment {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub struct $name {
            connection: DatadogAgentConnection,
        }

        impl $name {
            /// Returns a client handle for this consumer.
            pub fn client(&self) -> RemoteAgentClient {
                self.connection.client()
            }

            /// Returns the live session id handle.
            pub fn session_id(&self) -> SessionIdHandle {
                self.connection.session_id()
            }

            /// Returns the shared connection backing this attachment.
            pub fn connection(&self) -> &DatadogAgentConnection {
                &self.connection
            }
        }
    };
}

attachment! {
    /// Typed attachment for the status provider service.
    ///
    /// The binary builds its `StatusProviderServer` on this attachment; it needs the session id to
    /// stamp responses and (indirectly) the connection it belongs to.
    StatusAttachment
}

attachment! {
    /// Typed attachment for the flare provider service.
    ///
    /// The binary builds its `FlareProviderServer` on this attachment.
    FlareAttachment
}

attachment! {
    /// Typed attachment for the telemetry provider service.
    ///
    /// The binary builds its `TelemetryProviderServer` on this attachment.
    TelemetryAttachment
}

attachment! {
    /// Typed attachment for the metrics / remote tagger consumer.
    ///
    /// Exposes the client used to drive the tagger entity stream (`get_tagger_stream`).
    MetricsAttachment
}

attachment! {
    /// Typed attachment for the autodiscovery consumer.
    ///
    /// Exposes the client used to drive the autodiscovery config stream (`get_autodiscovery_stream`).
    AutodiscoveryAttachment
}

attachment! {
    /// Typed attachment for the host-tags consumer.
    ///
    /// Exposes the client used to query host tags (`get_host_tags`) and the hostname
    /// (`get_hostname`).
    HostTagsAttachment
}

async fn run_registration_loop(
    mut client: RemoteAgentClient, session_id: SessionIdHandle, registration: RemoteAgentRegistration,
    init_tx: oneshot::Sender<Result<(), GenericError>>,
) {
    let mut init_tx = Some(init_tx);
    let mut loop_timer = interval(DEFAULT_REFRESH_INTERVAL);
    loop_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    debug!("Remote Agent registration task started.");

    loop {
        loop_timer.tick().await;

        match session_id.get() {
            Some(current) => {
                debug!(session_id = %current, "Refreshing registration with Datadog Agent.");
                if client.refresh_remote_agent_request(&current).await.is_err() {
                    loop_timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                    session_id.update(None);
                    warn!("Failed to refresh registration with the Datadog Agent. Resetting session ID and retrying shortly.");
                }
            }
            None => {
                match client
                    .register_remote_agent_request(
                        registration.pid,
                        &registration.display_name,
                        &registration.flavor,
                        &registration.api_endpoint,
                        registration.service_names.clone(),
                    )
                    .await
                {
                    Ok(resp) => {
                        let resp = resp.into_inner();
                        let new_session_id = match SessionId::new(&resp.session_id) {
                            Ok(id) => id,
                            Err(e) => {
                                warn!(error = %e, "Received invalid session ID after registration. Will retry periodically.");
                                loop_timer.reset_after(DEFAULT_REFRESH_INTERVAL);
                                continue;
                            }
                        };
                        let refresh_secs = resp.recommended_refresh_interval_secs;
                        info!(session_id = %new_session_id, "Successfully registered with the Datadog Agent. Refreshing every {} seconds.", refresh_secs);

                        session_id.update(Some(new_session_id));
                        loop_timer.reset_after(Duration::from_secs(refresh_secs as u64));

                        if let Some(tx) = init_tx.take() {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to register with the Datadog Agent. Will retry periodically.");
                        loop_timer.reset_after(DEFAULT_REFRESH_INTERVAL);
                        if let Some(tx) = init_tx.take() {
                            let _ = tx.send(Err(e));
                        }
                    }
                }
            }
        }
    }
}

async fn run_config_stream_loop(
    mut client: RemoteAgentClient, session_id: SessionIdHandle, sender: mpsc::Sender<ConfigUpdate>,
) {
    loop {
        debug!("Establishing a new config stream connection to the Core Agent.");

        // Re-read the session id every loop: it can change in the background due to re-registration.
        let current_session_id = session_id.wait_for_update().await;

        let mut stream = client.stream_config_events(&current_session_id);
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    let update = match event.event {
                        Some(config_event::Event::Snapshot(snapshot)) => {
                            Some(ConfigUpdate::Snapshot(snapshot_to_map(&snapshot)))
                        }
                        Some(config_event::Event::Update(update)) => {
                            update.setting.map(|setting| ConfigUpdate::Partial {
                                key: setting.key,
                                value: proto_value_to_serde_value(&setting.value),
                            })
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
                Err(e) => error!("Error while reading config event stream: {}.", e),
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
        upsert(&mut root, &setting.key, proto_value_to_serde_value(&setting.value));
    }
    root
}

/// Recursively converts a `google::protobuf::Value` into a `serde_json::Value`.
fn proto_value_to_serde_value(proto_val: &Option<prost_types::Value>) -> Value {
    use prost_types::value::Kind;

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
        Kind::StructValue(s) => Value::Object(
            s.fields
                .iter()
                .map(|(k, v)| (k.clone(), proto_value_to_serde_value(&Some(v.clone()))))
                .collect(),
        ),
        Kind::ListValue(l) => Value::Array(
            l.values
                .iter()
                .map(|v| proto_value_to_serde_value(&Some(v.clone())))
                .collect(),
        ),
    }
}
