use std::{collections::hash_map::Entry, time::Duration};
use std::{collections::HashMap, net::SocketAddr};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use datadog_protos::agent::{
    GetFlareFilesRequest, GetFlareFilesResponse, GetStatusDetailsRequest, GetStatusDetailsResponse, RemoteAgent,
    RemoteAgentServer, StatusSection,
};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use saluki_config::GenericConfiguration;
use saluki_core::state::reflector::Reflector;
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::GenericError;
use tokio::time::{interval, MissedTickBehavior};
use tracing::debug;
use uuid::Uuid;

use crate::state::metrics::AggregatedMetricsProcessor;

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

/// Remote Agent helper configuration.
pub struct RemoteAgentHelperConfiguration {
    id: String,
    display_name: String,
    local_api_listen_addr: SocketAddr,
    client: RemoteAgentClient,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
}

impl RemoteAgentHelperConfiguration {
    /// Creates a new `RemoteAgentHelperConfiguration` from the given configuration.
    pub async fn from_configuration(
        config: &GenericConfiguration, local_api_listen_addr: SocketAddr,
        internal_metrics: Reflector<AggregatedMetricsProcessor>,
    ) -> Result<Self, GenericError> {
        let app_details = saluki_metadata::get_app_details();
        let formatted_full_name = app_details
            .full_name()
            .replace(" ", "-")
            .replace("_", "-")
            .to_lowercase();
        let client = RemoteAgentClient::from_configuration(config).await?;

        Ok(Self {
            id: format!("{}-{}", formatted_full_name, Uuid::now_v7()),
            display_name: formatted_full_name,
            local_api_listen_addr,
            client,
            internal_metrics,
        })
    }

    /// Spawns the remote agent helper task.
    ///
    /// The spawned task ensures that this process is registered as a Remote Agent with the configured Datadog Agent
    /// instance. Additionally, an implementation of the `RemoteAgent` gRPC service is returned that must be installed
    /// on the API server that is listening at `local_api_listen_addr`.
    pub async fn spawn(self) -> RemoteAgentServer<RemoteAgentImpl> {
        let service_impl = RemoteAgentImpl {
            started: Utc::now(),
            internal_metrics: self.internal_metrics.clone(),
        };
        let service = RemoteAgentServer::new(service_impl);

        tokio::spawn(run_remote_agent_helper(
            self.id,
            self.display_name,
            self.local_api_listen_addr,
            self.client,
        ));

        service
    }
}

async fn run_remote_agent_helper(
    id: String, display_name: String, local_api_listen_addr: SocketAddr, mut client: RemoteAgentClient,
) {
    let local_api_listen_addr = local_api_listen_addr.to_string();
    let auth_token: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();

    let mut register_agent = interval(Duration::from_secs(10));
    register_agent.set_missed_tick_behavior(MissedTickBehavior::Delay);

    debug!("Remote Agent helper started.");

    loop {
        register_agent.tick().await;
        match client
            .register_remote_agent_request(&id, &display_name, &local_api_listen_addr, &auth_token)
            .await
        {
            Ok(resp) => {
                let new_refresh_interval = resp.into_inner().recommended_refresh_interval_secs;
                register_agent.reset_after(Duration::from_secs(new_refresh_interval as u64));
                debug!("Refreshed registration with Datadog Agent");
            }
            Err(e) => {
                debug!("Failed to refresh registration with Datadog Agent: {}", e);
            }
        }
    }
}

pub struct RemoteAgentImpl {
    started: DateTime<Utc>,
    internal_metrics: Reflector<AggregatedMetricsProcessor>,
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
}

#[async_trait]
impl RemoteAgent for RemoteAgentImpl {
    async fn get_status_details(
        &self, _request: tonic::Request<GetStatusDetailsRequest>,
    ) -> Result<tonic::Response<GetStatusDetailsResponse>, tonic::Status> {
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
    }

    async fn get_flare_files(
        &self, _request: tonic::Request<GetFlareFilesRequest>,
    ) -> Result<tonic::Response<GetFlareFilesResponse>, tonic::Status> {
        let response = GetFlareFilesResponse {
            files: HashMap::default(),
        };
        Ok(tonic::Response::new(response))
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
