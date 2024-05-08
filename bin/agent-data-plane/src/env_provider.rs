use saluki_config::GenericConfiguration;
use saluki_env::{
    host::providers::AgentLikeHostProvider, workload::providers::RemoteAgentWorkloadProvider, EnvironmentProvider,
};

const HOSTNAME_CONFIG_KEY: &str = "hostname";
const HOSTNAME_FILE_CONFIG_KEY: &str = "hostname_file";
const TRUST_OS_HOSTNAME_CONFIG_KEY: &str = "hostname_trust_uts_namespace";

#[derive(Clone)]
pub struct ADPEnvironmentProvider {
    host_provider: AgentLikeHostProvider,
    workload_provider: RemoteAgentWorkloadProvider,
}

impl ADPEnvironmentProvider {
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            host_provider: AgentLikeHostProvider::new(
                config,
                HOSTNAME_CONFIG_KEY,
                HOSTNAME_FILE_CONFIG_KEY,
                TRUST_OS_HOSTNAME_CONFIG_KEY,
            )?,
            workload_provider: RemoteAgentWorkloadProvider::from_configuration(config).await?,
        })
    }
}

impl EnvironmentProvider for ADPEnvironmentProvider {
    type Host = AgentLikeHostProvider;
    type Workload = RemoteAgentWorkloadProvider;

    fn host(&self) -> &Self::Host {
        &self.host_provider
    }

    fn workload(&self) -> &Self::Workload {
        &self.workload_provider
    }
}
