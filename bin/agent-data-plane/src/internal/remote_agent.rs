use std::sync::{Arc, Mutex};
use std::{collections::hash_map::Entry, time::Duration};
use std::{collections::HashMap, net::SocketAddr};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_protos::agent::flare::v1::{
    flare_provider_server::FlareProvider, flare_provider_server::FlareProviderServer, GetFlareFilesRequest,
    GetFlareFilesResponse,
};
use datadog_protos::agent::status::v1::{
    status_provider_server::StatusProvider, status_provider_server::StatusProviderServer, GetStatusDetailsRequest,
    GetStatusDetailsResponse, StatusSection,
};
use datadog_protos::agent::telemetry::v1::{
    get_telemetry_response::Payload, telemetry_provider_server::TelemetryProvider,
    telemetry_provider_server::TelemetryProviderServer, GetTelemetryRequest, GetTelemetryResponse,
};
use http::{Request, Uri};
use http_body_util::BodyExt;
use saluki_common::task::spawn_traced_named;
use saluki_config::GenericConfiguration;
use saluki_core::state::reflector::Reflector;
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::GenericError;
use saluki_io::net::client::http::HttpClient;
use saluki_io::net::GrpcTargetAddress;
use tokio::time::{interval, MissedTickBehavior};
use tonic::server::NamedService;
use tonic::Status;
use tracing::{debug, info, warn};

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

/// A handle for updating the session ID at runtime.
///
/// This handle allows you to dynamically update the session ID that is added to gRPC responses
/// even after the server has started.
#[derive(Clone, Debug, Default)]
pub struct SessionIdHandle {
    session_id: Arc<Mutex<Option<String>>>,
}

impl SessionIdHandle {
    /// Creates a new `SessionIdHandle` with no session ID.
    pub fn new() -> Self {
        Self {
            session_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Updates the session ID to a new value.
    ///
    /// This change will be reflected in all subsequent gRPC responses.
    pub fn update(&self, new_session_id: Option<String>) {
        if let Ok(mut session_id) = self.session_id.lock() {
            *session_id = new_session_id;
        }
    }

    /// Gets the current session ID.
    pub fn get(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|s| s.clone())
    }
}

/// Remote Agent helper configuration.
pub struct RemoteAgentHelperConfiguration {
    pid: u32,
    display_name: String,
    api_listen_addr: GrpcTargetAddress,
    client: RemoteAgentClient,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
    prometheus_listen_addr: Option<SocketAddr>,
    session_id: SessionIdHandle,
    service_names: Vec<String>,
}

impl RemoteAgentHelperConfiguration {
    /// Creates a new `RemoteAgentHelperConfiguration` from the given configuration.
    pub async fn from_configuration(
        config: &GenericConfiguration, api_listen_addr: GrpcTargetAddress, prometheus_listen_addr: Option<SocketAddr>,
    ) -> Result<Self, GenericError> {
        let app_details = saluki_metadata::get_app_details();
        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            pid: std::process::id(),
            display_name: formatted_full_name,
            api_listen_addr,
            client,
            internal_metrics: get_shared_metrics_state().await,
            prometheus_listen_addr,
            session_id: SessionIdHandle::new(),
            service_names: Vec::new(),
        })
    }

    /// Creates a new `StatusProviderServer` for the remote agent helper.
    ///
    /// The service name is automatically tracked for registration.
    /// The service is wrapped with a layer that adds session_id to responses.
    pub fn create_status_service(&mut self) -> StatusProviderServer<RemoteAgentImpl> {
        self.service_names
            .push(<StatusProviderServer<RemoteAgentImpl> as NamedService>::NAME.to_string());

        StatusProviderServer::new(RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            prometheus_listen_addr: self.prometheus_listen_addr,
            session_id: self.session_id.clone(),
        })
    }

    /// Creates a new `TelemetryProviderServer` for the remote agent helper.
    ///
    /// The service name is automatically tracked for registration.
    /// The service is wrapped with a layer that adds session_id to responses.
    pub fn create_telemetry_service(&mut self) -> TelemetryProviderServer<RemoteAgentImpl> {
        self.service_names
            .push(<TelemetryProviderServer<RemoteAgentImpl> as NamedService>::NAME.to_string());

        TelemetryProviderServer::new(RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            prometheus_listen_addr: self.prometheus_listen_addr,
            session_id: self.session_id.clone(),
        })
    }

    /// Creates a new `FlareProviderServer` for the remote agent helper.
    ///
    /// The service name is automatically tracked for registration.
    /// The service is wrapped with a layer that adds session_id to responses.
    pub fn create_flare_service(&mut self) -> FlareProviderServer<RemoteAgentImpl> {
        self.service_names
            .push(<FlareProviderServer<RemoteAgentImpl> as NamedService>::NAME.to_string());

        FlareProviderServer::new(RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            prometheus_listen_addr: self.prometheus_listen_addr,
            session_id: self.session_id.clone(),
        })
    }

    /// Spawns the remote agent helper task.
    ///
    /// The spawned task ensures that this process is registered as a Remote Agent with the configured Datadog Agent
    /// instance and maintains the registration with periodic refreshes.
    pub fn spawn(self) {
        spawn_traced_named(
            "adp-remote-agent-task",
            run_remote_agent_helper(
                self.pid,
                self.display_name,
                self.api_listen_addr,
                self.client,
                self.session_id,
                self.service_names,
            ),
        );
    }
}

async fn run_remote_agent_helper(
    pid: u32, display_name: String, api_listen_addr: GrpcTargetAddress, mut client: RemoteAgentClient,
    session_id: SessionIdHandle, service_names: Vec<String>,
) {
    let api_listen_addr = api_listen_addr.to_string();

    let mut loop_timer = interval(DEFAULT_REFRESH_INTERVAL);
    loop_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    debug!("Remote Agent helper started.");

    loop {
        loop_timer.tick().await;

        match session_id.get() {
            Some(id) => {
                debug!(session_id = %id, "Refreshing registration with Datadog Agent.");

                if client.refresh_remote_agent_request(&id).await.is_err() {
                    loop_timer.reset_after(REFRESH_FAILED_RETRY_INTERVAL);
                    session_id.update(None);
                    // Clear environment variable when session expires
                    std::env::remove_var("DD_ADP_SESSION_ID");

                    warn!("Failed to refresh registration with the Datadog Agent. Resetting session ID and attempting to re-register shortly.");

                    continue;
                }
            }
            None => {
                match client
                    .register_remote_agent_request(pid, &display_name, &api_listen_addr, service_names.clone())
                    .await
                {
                    Ok(resp) => {
                        let resp = resp.into_inner();
                        let new_refresh_interval = resp.recommended_refresh_interval_secs;
                        info!(session_id = %resp.session_id, "Successfully registered with the Datadog Agent. Refreshing every {} seconds.", new_refresh_interval);

                        session_id.update(Some(resp.session_id.clone()));
                        // Set environment variable so config stream can access it
                        std::env::set_var("DD_ADP_SESSION_ID", &resp.session_id);
                        loop_timer.reset_after(Duration::from_secs(new_refresh_interval as u64));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to register with the Datadog Agent. Registration will be retried periodically in the background.");
                        loop_timer.reset_after(DEFAULT_REFRESH_INTERVAL);
                    }
                }
            }
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
            .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
            .or(Err(Status::internal(
                "unable to convert session ID into valid gRPC metadata",
            )))?;

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
