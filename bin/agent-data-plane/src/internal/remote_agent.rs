use std::collections::hash_map::Entry;
use std::collections::HashMap;

use agent_data_plane_config_system::Attachments;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_agent_commons::ipc::session::SessionIdHandle;
use datadog_protos::agent::{
    flare::v1::{flare_provider_server::*, *},
    status::v1::{status_provider_server::*, *},
    telemetry::v1::{get_telemetry_response::*, telemetry_provider_server::*, *},
};
use saluki_core::observability::metrics::{
    get_shared_metrics_state, AggregatedMetricsProcessor, Reflector, TelemetryProcessor,
};
use tokio::sync::Mutex;
use tonic::{server::NamedService, Status};

use crate::state::metrics::get_datadog_agent_remappings;

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

/// Builds the remote-agent gRPC service implementations (status, flare, telemetry).
///
/// The Agent connection, registration, and config stream are owned by the configuration system; this
/// type only needs the live session id (from a typed attachment) to stamp responses and the shared
/// internal-metrics state to report status/telemetry. The gRPC service names registered with the
/// Agent are exposed via [`service_names`](RemoteAgentServices::service_names) so the binary can pass
/// them into the config-system's `RemoteAgentRegistration`.
pub struct RemoteAgentServices {
    session_id: SessionIdHandle,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
}

impl RemoteAgentServices {
    /// Builds the service factory from the typed status attachment's session id.
    ///
    /// All three remote-agent attachments share the one underlying connection and session, so the
    /// status attachment's session id is representative.
    pub async fn from_attachments(attachments: &Attachments) -> Self {
        Self {
            session_id: attachments.status.session_id(),
            internal_metrics: get_shared_metrics_state().await,
        }
    }

    /// Returns the fully qualified gRPC service names this process serves to the Agent.
    ///
    /// The binary passes these into the config-system's `RemoteAgentRegistration` so the Agent knows
    /// which services to call back on.
    pub fn service_names() -> Vec<String> {
        vec![
            <StatusProviderServer<()> as NamedService>::NAME.to_string(),
            <FlareProviderServer<()> as NamedService>::NAME.to_string(),
            <TelemetryProviderServer<()> as NamedService>::NAME.to_string(),
        ]
    }

    fn build_impl(&self) -> RemoteAgentImpl {
        RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
            processor: Mutex::new(TelemetryProcessor::new().with_remapper_rules(get_datadog_agent_remappings())),
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
}

pub struct RemoteAgentImpl {
    started: DateTime<Utc>,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
    processor: Mutex<TelemetryProcessor>,
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
