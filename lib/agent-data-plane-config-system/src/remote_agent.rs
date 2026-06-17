use std::time::Duration;

use agent_data_plane_config::{ControlConfiguration, DatadogRuntimeAuthority};
use datadog_agent_commons::ipc::{
    client::RemoteAgentClient,
    config::RemoteAgentClientConfiguration,
    session::{SessionId, SessionIdHandle},
};
use datadog_protos::agent::{
    flare::v1::flare_provider_server::FlareProviderServer, status::v1::status_provider_server::StatusProviderServer,
    telemetry::v1::telemetry_provider_server::TelemetryProviderServer,
};
use saluki_common::task::spawn_traced_named;
use saluki_component_config::ListenAddress as NativeListenAddress;
use saluki_config_tools::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{GrpcTargetAddress, ListenAddress};
use tokio::{
    sync::oneshot,
    time::{interval, MissedTickBehavior},
};
use tonic::server::NamedService;
use tracing::debug;

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Typed capabilities produced while starting the configuration system.
#[derive(Clone, Default)]
pub struct Attachments {
    /// Datadog Agent connection, when remote-agent mode is active.
    pub datadog_agent: Option<DatadogAgentConnection>,
}

/// Shared Datadog Agent IPC connection and registration session.
#[derive(Clone)]
pub struct DatadogAgentConnection {
    client: RemoteAgentClient,
    session_id: SessionIdHandle,
}

impl DatadogAgentConnection {
    /// Returns a clone of the shared IPC client.
    pub fn client(&self) -> RemoteAgentClient {
        self.client.clone()
    }

    /// Returns the remote-agent session handle.
    pub fn session_id(&self) -> SessionIdHandle {
        self.session_id.clone()
    }
}

pub(crate) async fn build_attachments(
    config: &GenericConfiguration, control: &ControlConfiguration, authority: DatadogRuntimeAuthority,
) -> Result<Attachments, GenericError> {
    if authority == DatadogRuntimeAuthority::Local
        || control.standalone_mode
        || !(control.remote_agent_enabled || control.use_new_config_stream_endpoint)
    {
        return Ok(Attachments::default());
    }

    let client_config = config
        .as_typed::<RemoteAgentClientConfiguration>()
        .error_context("Failed to parse Datadog Agent IPC client configuration.")?;
    let client = RemoteAgentClient::from_client_configuration(client_config).await?;
    let api_listen_addr = grpc_target_from_native(&control.secure_api_listen_address)?;
    let service_names = remote_agent_service_names();
    let (state, initial_registration) = RemoteAgentState::new(api_listen_addr, service_names);
    let session_id = state.session_id.clone();

    spawn_traced_named(
        "adp-remote-agent-registration-task",
        run_remote_agent_registration_loop(client.clone(), state),
    );

    match initial_registration.await {
        Ok(Ok(())) => Ok(Attachments {
            datadog_agent: Some(DatadogAgentConnection { client, session_id }),
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(generic_error!(
            "Failed to initialize remote agent state. Registration task failed unexpectedly."
        )),
    }
}

fn grpc_target_from_native(address: &NativeListenAddress) -> Result<GrpcTargetAddress, GenericError> {
    let listen_address = match address {
        NativeListenAddress::Disabled => ListenAddress::any_tcp(5010),
        NativeListenAddress::Tcp(value) => parse_listen_address(value, "tcp")?,
        NativeListenAddress::Udp(value) => parse_listen_address(value, "udp")?,
        NativeListenAddress::Unix(value) => parse_listen_address(value, "unix")?,
    };
    GrpcTargetAddress::try_from_listen_addr(&listen_address)
        .ok_or_else(|| generic_error!("Failed to derive remote-agent API address from secure API listen address."))
}

fn parse_listen_address(value: &str, default_scheme: &str) -> Result<ListenAddress, GenericError> {
    let raw = if value.contains("://") {
        value.to_string()
    } else {
        format!("{default_scheme}://{value}")
    };
    ListenAddress::try_from(raw.as_str()).map_err(|error| generic_error!("Invalid listen address `{value}`: {error}"))
}

fn remote_agent_service_names() -> Vec<String> {
    vec![
        <StatusProviderServer<()> as NamedService>::NAME.to_string(),
        <FlareProviderServer<()> as NamedService>::NAME.to_string(),
        <TelemetryProviderServer<()> as NamedService>::NAME.to_string(),
    ]
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
        let flavor = app_details.full_name().replace(' ', "_").to_lowercase();
        let (initial_registration_tx, initial_registration_rx) = oneshot::channel();

        let state = Self {
            pid: std::process::id(),
            display_name,
            flavor,
            api_listen_addr: api_listen_addr.to_string(),
            session_id: SessionIdHandle::empty(),
            service_names,
            initial_registration_tx: Some(initial_registration_tx),
        };

        (state, initial_registration_rx)
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
                if client.refresh_remote_agent_request(&session_id).await.is_err() {
                    loop_timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                    state.session_id.update(None);
                }
            }
            None => {
                debug!("Registering with Datadog Agent.");
                let result = register_remote_agent(&mut client, &mut state).await;
                if result.is_err() {
                    loop_timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                }
                if let Some(initial_registration_tx) = state.initial_registration_tx.take() {
                    let _ = initial_registration_tx.send(result);
                }
            }
        }
    }
}

async fn register_remote_agent(
    client: &mut RemoteAgentClient, state: &mut RemoteAgentState,
) -> Result<(), GenericError> {
    let response = client
        .register_remote_agent_request(
            state.pid,
            &state.display_name,
            &state.flavor,
            &state.api_listen_addr,
            state.service_names.clone(),
        )
        .await?;
    let session_id = SessionId::new(&response.into_inner().session_id)?;
    state.session_id.update(Some(session_id));
    Ok(())
}
