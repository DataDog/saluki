use saluki_config::GenericConfiguration;
use saluki_core::prelude::*;
use saluki_env::{
    host::providers::AgentLikeHostProvider, workload::providers::RemoteAgentWorkloadProvider, EnvironmentProvider,
};

const HOSTNAME_CONFIG_KEY: &str = "hostname";
const HOSTNAME_FILE_CONFIG_KEY: &str = "hostname_file";
const TRUST_OS_HOSTNAME_CONFIG_KEY: &str = "hostname_trust_uts_namespace";

#[derive(Clone)]
pub struct ADPEnvironmentProvider {
    host_provider: AgentLikeHostProvider,
    workload_provider: Option<RemoteAgentWorkloadProvider>,
}

impl ADPEnvironmentProvider {
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, ErasedError> {
        // We allow disabling the normal workload provider via configuration, since in some cases we don't actually care
        // about having a real workload provider since we know we won't be in a containerized environment, or running
        // alongside the Datadog Agent.
        let use_noop_workload_provider = config
            .get_typed::<bool>("adp.use_noop_workload_provider")
            .map_or(false, |v| v.unwrap_or_default());

        let workload_provider = if use_noop_workload_provider {
            None
        } else {
            Some(RemoteAgentWorkloadProvider::from_configuration(config).await?)
        };

        Ok(Self {
            host_provider: AgentLikeHostProvider::new(
                config,
                HOSTNAME_CONFIG_KEY,
                HOSTNAME_FILE_CONFIG_KEY,
                TRUST_OS_HOSTNAME_CONFIG_KEY,
            )?,
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
