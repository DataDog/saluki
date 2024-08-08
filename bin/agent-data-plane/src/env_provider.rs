use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_env::{
    host::providers::AgentLikeHostProvider, workload::providers::RemoteAgentWorkloadProvider, EnvironmentProvider,
};
use saluki_error::GenericError;
use tracing::debug;

const HOSTNAME_CONFIG_KEY: &str = "hostname";
const HOSTNAME_FILE_CONFIG_KEY: &str = "hostname_file";
const TRUST_OS_HOSTNAME_CONFIG_KEY: &str = "hostname_trust_uts_namespace";

#[derive(Clone)]
pub struct ADPEnvironmentProvider {
    host_provider: AgentLikeHostProvider,
    workload_provider: Option<RemoteAgentWorkloadProvider>,
}

impl ADPEnvironmentProvider {
    pub async fn from_configuration(
        config: &GenericConfiguration, mut component_registry: ComponentRegistry,
    ) -> Result<Self, GenericError> {
        let host_provider = AgentLikeHostProvider::new(
            config,
            HOSTNAME_CONFIG_KEY,
            HOSTNAME_FILE_CONFIG_KEY,
            TRUST_OS_HOSTNAME_CONFIG_KEY,
        )?;

        component_registry
            .bounds_builder()
            .with_subcomponent("host", &host_provider);

        // We allow disabling the normal workload provider via configuration, since in some cases we don't actually care
        // about having a real workload provider since we know we won't be in a containerized environment, or running
        // alongside the Datadog Agent.
        let use_noop_workload_provider = config.get_typed_or_default::<bool>("adp.use_noop_workload_provider");

        let workload_provider = if use_noop_workload_provider {
            debug!("Using no-op workload provider as instructed by configuration.");
            None
        } else {
            Some(
                RemoteAgentWorkloadProvider::from_configuration(config, component_registry.get_or_create("workload"))
                    .await?,
            )
        };

        Ok(Self {
            host_provider,
            workload_provider,
        })
    }
}

impl EnvironmentProvider for ADPEnvironmentProvider {
    type Host = AgentLikeHostProvider;
    type Workload = Option<RemoteAgentWorkloadProvider>;

    fn host(&self) -> &Self::Host {
        &self.host_provider
    }

    fn workload(&self) -> &Self::Workload {
        &self.workload_provider
    }
}
