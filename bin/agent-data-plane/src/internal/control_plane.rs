use std::path::PathBuf;

use async_trait::async_trait;
use datadog_protos::agent::{
    agent_secure_server::{AgentSecure, AgentSecureServer},
    config::{
        ClientGetConfigsRequest, ClientGetConfigsResponse, ConfigSubscriptionRequest, GetStateConfigResponse,
        ResetStateConfigResponse,
    },
    v1::{
        RefreshRemoteAgentRequest, RefreshRemoteAgentResponse, RegisterRemoteAgentRequest, RegisterRemoteAgentResponse,
    },
    AutodiscoveryStreamResponse, CaptureTriggerRequest, CaptureTriggerResponse, ConfigEvent, ConfigStreamRequest,
    FetchEntityRequest, FetchEntityResponse, GenerateContainerIdFromOriginInfoRequest,
    GenerateContainerIdFromOriginInfoResponse, HostTagReply, HostTagRequest, StreamTagsRequest, StreamTagsResponse,
    TaggerState, TaggerStateResponse, WorkloadmetaStreamRequest, WorkloadmetaStreamResponse,
};
use futures::stream::Empty;
use memory_accounting::ComponentRegistry;
use saluki_api::EndpointType;
use saluki_app::{
    api::APIBuilder,
    config::ConfigAPIHandler,
    dynamic_api::DynamicAPIBuilder,
    logging::{acquire_logging_api_handler, LoggingOverrideController},
    memory::AllocationTelemetryWorker,
    metrics::acquire_metrics_api_handler,
};
use saluki_components::{
    destinations::DogStatsDStatisticsConfiguration,
    sources::{DogStatsDCaptureControl, DogStatsDReplayState},
};
use saluki_config::{parse_duration, GenericConfiguration};
use saluki_core::{
    health::HealthRegistry,
    runtime::{
        InitializationError, ProcessShutdown, RestartStrategy, RuntimeConfiguration, Supervisable, Supervisor,
        SupervisorFuture,
    },
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::{build_datadog_agent_server_tls_config, get_ipc_cert_file_path, ServerConfig};
use tonic::{Request, Response, Status, Streaming};
use tracing::info;

use crate::{
    config::DataPlaneConfiguration,
    env_provider::ADPEnvironmentProvider,
    internal::{logging::DynamicLogLevelWorker, platform::PlatformSettings, remote_agent::RemoteAgentBootstrap},
};

/// Gets the IPC certificate file path from the configuration.
fn get_cert_path_from_config(config: &GenericConfiguration) -> Result<PathBuf, GenericError> {
    let auth_token_file_path = config
        .try_get_typed::<PathBuf>("auth_token_file_path")
        .error_context("Failed to get Agent auth token file path.")?
        .unwrap_or_else(PlatformSettings::get_auth_token_path);

    let ipc_cert_file_path = config
        .try_get_typed::<Option<PathBuf>>("ipc_cert_file_path")
        .error_context("Failed to get Agent IPC cert file path.")?
        .flatten();

    Ok(get_ipc_cert_file_path(
        ipc_cert_file_path.as_ref(),
        &auth_token_file_path,
    ))
}

struct DogStatsDCaptureApi {
    capture_control: DogStatsDCaptureControl,
    replay_state: DogStatsDReplayState,
}

impl DogStatsDCaptureApi {
    fn new(capture_control: DogStatsDCaptureControl, replay_state: DogStatsDReplayState) -> Self {
        Self {
            capture_control,
            replay_state,
        }
    }
}

#[async_trait]
impl AgentSecure for DogStatsDCaptureApi {
    type TaggerStreamEntitiesStream = Empty<Result<StreamTagsResponse, Status>>;
    type CreateConfigSubscriptionStream =
        Empty<Result<datadog_protos::agent::config::ConfigSubscriptionResponse, Status>>;
    type WorkloadmetaStreamEntitiesStream = Empty<Result<WorkloadmetaStreamResponse, Status>>;
    type AutodiscoveryStreamConfigStream = Empty<Result<AutodiscoveryStreamResponse, Status>>;
    type StreamConfigEventsStream = Empty<Result<ConfigEvent, Status>>;

    async fn tagger_stream_entities(
        &self, _request: Request<StreamTagsRequest>,
    ) -> Result<Response<Self::TaggerStreamEntitiesStream>, Status> {
        Err(Status::unimplemented(
            "TaggerStreamEntities is not implemented by the Agent Data Plane.",
        ))
    }

    async fn tagger_generate_container_id_from_origin_info(
        &self, _request: Request<GenerateContainerIdFromOriginInfoRequest>,
    ) -> Result<Response<GenerateContainerIdFromOriginInfoResponse>, Status> {
        Err(Status::unimplemented(
            "TaggerGenerateContainerIDFromOriginInfo is not implemented by the Agent Data Plane.",
        ))
    }

    async fn tagger_fetch_entity(
        &self, _request: Request<FetchEntityRequest>,
    ) -> Result<Response<FetchEntityResponse>, Status> {
        Err(Status::unimplemented(
            "TaggerFetchEntity is not implemented by the Agent Data Plane.",
        ))
    }

    async fn dogstatsd_capture_trigger(
        &self, request: Request<CaptureTriggerRequest>,
    ) -> Result<Response<CaptureTriggerResponse>, Status> {
        let request = request.into_inner();
        let duration = parse_duration(&request.duration).map_err(|e| Status::invalid_argument(e.to_string()))?;
        let requested_dir = (!request.path.is_empty()).then(|| std::path::Path::new(&request.path));

        let capture_path = self
            .capture_control
            .start_capture(requested_dir, duration, request.compressed)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;

        Ok(Response::new(CaptureTriggerResponse {
            path: capture_path.display().to_string(),
        }))
    }

    async fn dogstatsd_set_tagger_state(
        &self, request: Request<TaggerState>,
    ) -> Result<Response<TaggerStateResponse>, Status> {
        let loaded = self
            .replay_state
            .load(request.into_inner())
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(TaggerStateResponse { loaded }))
    }

    async fn client_get_configs(
        &self, _request: Request<ClientGetConfigsRequest>,
    ) -> Result<Response<ClientGetConfigsResponse>, Status> {
        Err(Status::unimplemented(
            "ClientGetConfigs is not implemented by the Agent Data Plane.",
        ))
    }

    async fn get_config_state(&self, _request: Request<()>) -> Result<Response<GetStateConfigResponse>, Status> {
        Err(Status::unimplemented(
            "GetConfigState is not implemented by the Agent Data Plane.",
        ))
    }

    async fn client_get_configs_ha(
        &self, _request: Request<ClientGetConfigsRequest>,
    ) -> Result<Response<ClientGetConfigsResponse>, Status> {
        Err(Status::unimplemented(
            "ClientGetConfigsHA is not implemented by the Agent Data Plane.",
        ))
    }

    async fn get_config_state_ha(&self, _request: Request<()>) -> Result<Response<GetStateConfigResponse>, Status> {
        Err(Status::unimplemented(
            "GetConfigStateHA is not implemented by the Agent Data Plane.",
        ))
    }

    async fn create_config_subscription(
        &self, _request: Request<Streaming<ConfigSubscriptionRequest>>,
    ) -> Result<Response<Self::CreateConfigSubscriptionStream>, Status> {
        Err(Status::unimplemented(
            "CreateConfigSubscription is not implemented by the Agent Data Plane.",
        ))
    }

    async fn reset_config_state(&self, _request: Request<()>) -> Result<Response<ResetStateConfigResponse>, Status> {
        Err(Status::unimplemented(
            "ResetConfigState is not implemented by the Agent Data Plane.",
        ))
    }

    async fn workloadmeta_stream_entities(
        &self, _request: Request<WorkloadmetaStreamRequest>,
    ) -> Result<Response<Self::WorkloadmetaStreamEntitiesStream>, Status> {
        Err(Status::unimplemented(
            "WorkloadmetaStreamEntities is not implemented by the Agent Data Plane.",
        ))
    }

    async fn register_remote_agent(
        &self, _request: Request<RegisterRemoteAgentRequest>,
    ) -> Result<Response<RegisterRemoteAgentResponse>, Status> {
        Err(Status::unimplemented(
            "RegisterRemoteAgent is not implemented by the Agent Data Plane.",
        ))
    }

    async fn refresh_remote_agent(
        &self, _request: Request<RefreshRemoteAgentRequest>,
    ) -> Result<Response<RefreshRemoteAgentResponse>, Status> {
        Err(Status::unimplemented(
            "RefreshRemoteAgent is not implemented by the Agent Data Plane.",
        ))
    }

    async fn autodiscovery_stream_config(
        &self, _request: Request<()>,
    ) -> Result<Response<Self::AutodiscoveryStreamConfigStream>, Status> {
        Err(Status::unimplemented(
            "AutodiscoveryStreamConfig is not implemented by the Agent Data Plane.",
        ))
    }

    async fn get_host_tags(&self, _request: Request<HostTagRequest>) -> Result<Response<HostTagReply>, Status> {
        Err(Status::unimplemented(
            "GetHostTags is not implemented by the Agent Data Plane.",
        ))
    }

    async fn stream_config_events(
        &self, _request: Request<ConfigStreamRequest>,
    ) -> Result<Response<Self::StreamConfigEventsStream>, Status> {
        Err(Status::unimplemented(
            "StreamConfigEvents is not implemented by the Agent Data Plane.",
        ))
    }
}

/// A worker that serves the privileged HTTP API with TLS.
///
/// This worker also handles remote agent registration when not in standalone mode. The remote agent gRPC services are
/// registered on the privileged API, and a background task periodically refreshes the registration with the Datadog
/// Agent.
pub struct PrivilegedApiWorker {
    config: GenericConfiguration,
    dp_config: DataPlaneConfiguration,
    env_provider: ADPEnvironmentProvider,
    dsd_stats_config: DogStatsDStatisticsConfiguration,
    _dsd_capture_control: Option<DogStatsDCaptureControl>,
    ra_bootstrap: Option<RemoteAgentBootstrap>,
    tls_config: ServerConfig,
}

impl PrivilegedApiWorker {
    /// Creates a new `PrivilegedApiWorker`.
    ///
    /// # Errors
    ///
    /// If the TLS configuration cannot be loaded, an error is returned.
    pub async fn new(
        config: GenericConfiguration, dp_config: DataPlaneConfiguration, env_provider: ADPEnvironmentProvider,
        dsd_stats_config: DogStatsDStatisticsConfiguration, dsd_capture_control: Option<DogStatsDCaptureControl>,
        ra_bootstrap: Option<RemoteAgentBootstrap>,
    ) -> Result<Self, GenericError> {
        let cert_path = get_cert_path_from_config(&config)?;
        let tls_config = build_datadog_agent_server_tls_config(cert_path).await?;

        Ok(Self {
            config,
            dp_config,
            env_provider,
            dsd_stats_config,
            _dsd_capture_control: dsd_capture_control,
            ra_bootstrap,
            tls_config,
        })
    }
}

#[async_trait]
impl Supervisable for PrivilegedApiWorker {
    fn name(&self) -> &str {
        "privileged-api"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let capture_api = AgentSecureServer::new(DogStatsDCaptureApi::new(
            self._dsd_capture_control.clone().unwrap_or_default(),
            self.env_provider.replay_state(),
        ));
        let mut api_builder = APIBuilder::new()
            .with_tls_config(self.tls_config.clone())
            // TODO: make these handlers cloneable and move them up to the config for the worker so they can
            // be cloned for each initialization
            .with_optional_handler(acquire_logging_api_handler())
            .with_optional_handler(acquire_metrics_api_handler())
            .with_handler(ConfigAPIHandler::new(self.config.clone()))
            .with_optional_handler(self.env_provider.workload_api_handler())
            .with_handler(self.dsd_stats_config.api_handler())
            .with_grpc_service(capture_api);

        // If we bootstrapped ourselves as a remote agent, add the necessary gRPC services to the API.
        if let Some(ra_bootstrap) = &self.ra_bootstrap {
            api_builder = api_builder.with_grpc_service(ra_bootstrap.create_status_service());
            api_builder = api_builder.with_grpc_service(ra_bootstrap.create_flare_service());

            // Only register the telemetry service if telemetry is actually enabled.
            if let Some(telemetry_service) = ra_bootstrap.create_telemetry_service() {
                api_builder = api_builder.with_grpc_service(telemetry_service);
            }
        }

        let listen_address = self.dp_config.secure_api_listen_address().clone();

        Ok(Box::pin(async move {
            info!("Serving privileged API on {}.", listen_address);
            api_builder.serve(listen_address, process_shutdown).await
        }))
    }
}

/// Dependencies used to assemble the control plane supervisor.
pub struct ControlPlaneDependencies {
    pub dsd_stats_config: DogStatsDStatisticsConfiguration,
    pub dsd_capture_control: Option<DogStatsDCaptureControl>,
    pub ra_bootstrap: Option<RemoteAgentBootstrap>,
    pub logging_controller: LoggingOverrideController,
}

/// Creates the control plane supervisor.
///
/// This supervisor manages the health registry, unprivileged and privileged APIs, and optionally the remote agent
/// registration task.
///
/// It runs on a dedicated single-threaded runtime.
///
/// # Errors
///
/// If the supervisor cannot be created, an error is returned.
pub async fn create_control_plane_supervisor(
    config: &GenericConfiguration, dp_config: &DataPlaneConfiguration, component_registry: &ComponentRegistry,
    health_registry: HealthRegistry, env_provider: ADPEnvironmentProvider, dependencies: ControlPlaneDependencies,
) -> Result<Supervisor, GenericError> {
    let ControlPlaneDependencies {
        dsd_stats_config,
        dsd_capture_control,
        ra_bootstrap,
        logging_controller,
    } = dependencies;

    let mut supervisor = Supervisor::new("ctrl-pln")?
        .with_dedicated_runtime(RuntimeConfiguration::single_threaded())
        .with_restart_strategy(RestartStrategy::one_to_one());

    supervisor.add_worker(health_registry.worker());
    supervisor.add_worker(AllocationTelemetryWorker::new(component_registry));
    supervisor.add_worker(DynamicLogLevelWorker::new(config, logging_controller));

    supervisor.add_worker(DynamicAPIBuilder::new(
        EndpointType::Unprivileged,
        dp_config.api_listen_address().clone(),
    ));
    supervisor.add_worker(
        PrivilegedApiWorker::new(
            config.clone(),
            dp_config.clone(),
            env_provider,
            dsd_stats_config,
            dsd_capture_control,
            ra_bootstrap,
        )
        .await?,
    );

    Ok(supervisor)
}

#[cfg(test)]
mod tests {
    use datadog_protos::agent::{
        agent_secure_server::AgentSecure, CaptureTriggerRequest, Entity, EntityId as RemoteEntityId, TaggerState,
    };
    use saluki_components::sources::DogStatsDReplayState;
    use tonic::{Code, Request};

    use super::DogStatsDCaptureApi;

    #[tokio::test]
    async fn capture_trigger_returns_failed_precondition_when_source_is_unavailable() {
        let api = DogStatsDCaptureApi::new(Default::default(), DogStatsDReplayState::new());
        let error = api
            .dogstatsd_capture_trigger(Request::new(CaptureTriggerRequest {
                duration: "10s".to_string(),
                path: String::new(),
                compressed: false,
            }))
            .await
            .expect_err("unbound capture control should fail");

        assert_eq!(error.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn capture_trigger_rejects_invalid_duration_before_starting_capture() {
        let api = DogStatsDCaptureApi::new(Default::default(), DogStatsDReplayState::new());
        let error = api
            .dogstatsd_capture_trigger(Request::new(CaptureTriggerRequest {
                duration: "10".to_string(),
                path: String::new(),
                compressed: false,
            }))
            .await
            .expect_err("unitless duration should be rejected");

        assert_eq!(error.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn tagger_state_rpc_loads_and_clears_replay_state() {
        let replay_state = DogStatsDReplayState::new();
        let api = DogStatsDCaptureApi::new(Default::default(), replay_state.clone());

        let response = api
            .dogstatsd_set_tagger_state(Request::new(TaggerState {
                state: [(
                    "container_id://cid-123".to_string(),
                    Entity {
                        id: Some(RemoteEntityId {
                            prefix: "container_id".to_string(),
                            uid: "cid-123".to_string(),
                        }),
                        low_cardinality_tags: vec!["env:prod".to_string()],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                pid_map: [(42, "container_id://cid-123".to_string())].into_iter().collect(),
                duration: 10_000,
            }))
            .await
            .expect("tagger state should load")
            .into_inner();

        assert!(response.loaded);
        assert!(replay_state.is_loaded());

        let response = api
            .dogstatsd_set_tagger_state(Request::new(TaggerState::default()))
            .await
            .expect("empty tagger state should clear replay state")
            .into_inner();

        assert!(!response.loaded);
        assert!(!replay_state.is_loaded());
    }

    #[tokio::test]
    async fn tagger_state_rpc_uses_expiry_fallback_for_loaded_state() {
        let replay_state = DogStatsDReplayState::new();
        let api = DogStatsDCaptureApi::new(Default::default(), replay_state.clone());

        let response = api
            .dogstatsd_set_tagger_state(Request::new(TaggerState {
                state: [(
                    "container_id://cid-123".to_string(),
                    Entity {
                        id: Some(RemoteEntityId {
                            prefix: "container_id".to_string(),
                            uid: "cid-123".to_string(),
                        }),
                        low_cardinality_tags: vec!["env:prod".to_string()],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                pid_map: [(42, "container_id://cid-123".to_string())].into_iter().collect(),
                duration: 0,
            }))
            .await
            .expect("tagger state should load")
            .into_inner();

        assert!(response.loaded);
        assert!(!replay_state.is_loaded());
    }
}
