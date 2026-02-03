use std::{collections::hash_map::Entry, time::Duration};
use std::{collections::HashMap, net::SocketAddr};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_protos::agent::{
    config_event,
    flare::v1::{flare_provider_server::*, *},
    status::v1::{status_provider_server::*, *},
    telemetry::v1::{get_telemetry_response::*, telemetry_provider_server::*, *},
    ConfigSnapshot,
};
use futures::StreamExt;
use http::{Request, Uri};
use http_body_util::BodyExt;
use prost_types::value::Kind;
use saluki_common::task::spawn_traced_named;
use saluki_config::{dynamic::ConfigUpdate, upsert, GenericConfiguration};
use saluki_core::state::reflector::Reflector;
use saluki_env::helpers::remote_agent::{RemoteAgentClient, SessionId, SessionIdHandle};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::{client::http::HttpClient, GrpcTargetAddress};
use serde_json::{Map, Value};
use tokio::{
    sync::{mpsc, oneshot},
    time::{interval, MissedTickBehavior},
};
use tonic::{server::NamedService, Status};
use tracing::{debug, error, info, warn};

use crate::config::DataPlaneConfiguration;
use crate::state::metrics::{get_shared_metrics_state, AggregatedMetricsProcessor};

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
    prometheus_listen_addr: Option<SocketAddr>,
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

        let mut prometheus_listen_addr = None;
        if dp_config.telemetry_enabled() {
            prometheus_listen_addr = dp_config
                .telemetry_listen_addr()
                .as_local_connect_addr()
                .ok_or_else(|| generic_error!("Telemetry listen address present but not valid for local connections."))
                .map(Some)?;
        }

        // Generate our remote agent state, which is mostly fixed but has a few dynamic bits.
        let service_names = vec![
            <StatusProviderServer<()> as NamedService>::NAME.to_string(),
            <TelemetryProviderServer<()> as NamedService>::NAME.to_string(),
            <FlareProviderServer<()> as NamedService>::NAME.to_string(),
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
            prometheus_listen_addr,
        })
    }

    /// Creates a new `StatusProviderServer` tied to this remote agent.
    pub fn create_status_service(&self) -> StatusProviderServer<RemoteAgentImpl> {
        StatusProviderServer::new(RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            prometheus_listen_addr: self.prometheus_listen_addr,
            session_id: self.session_id.clone(),
        })
    }

    /// Creates a new `TelemetryProviderServer` tied to this remote agent.
    ///
    /// Returns `None` if telemetry is not enabled.
    pub fn create_telemetry_service(&self) -> Option<TelemetryProviderServer<RemoteAgentImpl>> {
        self.prometheus_listen_addr.is_some().then(|| {
            TelemetryProviderServer::new(RemoteAgentImpl {
                started: Utc::now(),
                internal_metrics: self.internal_metrics.clone(),
                prometheus_listen_addr: self.prometheus_listen_addr,
                session_id: self.session_id.clone(),
            })
        })
    }

    /// Creates a new `FlareProviderServer` tied to this remote agent.
    pub fn create_flare_service(&self) -> FlareProviderServer<RemoteAgentImpl> {
        FlareProviderServer::new(RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            prometheus_listen_addr: self.prometheus_listen_addr,
            session_id: self.session_id.clone(),
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
        let display_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();

        let (init_reg_tx, init_reg_rx) = oneshot::channel();

        let state = Self {
            pid: std::process::id(),
            display_name,
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

                if client.refresh_remote_agent_request(&session_id).await.is_err() {
                    loop_timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                    state.session_id.update(None);
                    warn!("Failed to refresh registration with the Datadog Agent. Resetting session ID and attempting to re-register shortly.");

                    continue;
                }
            }
            None => {
                match client
                    .register_remote_agent_request(
                        state.pid,
                        &state.display_name,
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
    prometheus_listen_addr: Option<SocketAddr>,
    session_id: SessionIdHandle,
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
                // Telemetry is not enabled.
                if self.prometheus_listen_addr.is_none() {
                    return Ok(tonic::Response::new(GetTelemetryResponse { payload: None }));
                }

                let prometheus_listen_addr = self.prometheus_listen_addr.unwrap();
                let mut client: HttpClient<String> = HttpClient::builder().build().unwrap();

                let uri_string = format!("http://{}", prometheus_listen_addr);
                let uri: Uri = uri_string.parse().unwrap();
                let request = Request::builder()
                    .uri(uri)
                    .body(String::new())
                    .map_err(|e| Status::internal(e.to_string()))?;

                let resp = client.send(request).await.unwrap();

                match resp.into_body().collect().await {
                    Ok(body) => {
                        let body = body.to_bytes();
                        let body_str = String::from_utf8_lossy(&body[..]);
                        let response = GetTelemetryResponse {
                            payload: Some(Payload::PromText(body_str.to_string())),
                        };
                        Ok(tonic::Response::new(response))
                    }
                    Err(e) => Err(Status::internal(e.to_string())),
                }
            })
            .await;
    }
}

#[async_trait]
impl FlareProvider for RemoteAgentImpl {
    async fn get_flare_files(
        &self, _request: tonic::Request<GetFlareFilesRequest>,
    ) -> Result<tonic::Response<GetFlareFilesResponse>, Status> {
        return self
            .session_id_middleware(async || {
                let response = GetFlareFilesResponse {
                    files: HashMap::default(),
                };
                Ok(tonic::Response::new(response))
            })
            .await;
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
