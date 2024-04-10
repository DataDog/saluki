use saluki_config::GenericConfiguration;
use saluki_env::{
    host::agent::AgentLikeHostProvider, tagger::NoopTaggerProvider, workload::NoopWorkloadProvider, EnvironmentProvider,
};

const HOSTNAME_CONFIG_KEY: &str = "hostname";
const HOSTNAME_FILE_CONFIG_KEY: &str = "hostname_file";
const TRUST_OS_HOSTNAME_CONFIG_KEY: &str = "hostname_trust_uts_namespace";

#[derive(Clone)]
pub struct ADPEnvironmentProvider {
    host_provider: AgentLikeHostProvider,
}

impl ADPEnvironmentProvider {
    pub fn with_configuration(config: &GenericConfiguration) -> Result<Self, String> {
        Ok(Self {
            host_provider: AgentLikeHostProvider::new(
                config,
                HOSTNAME_CONFIG_KEY,
                HOSTNAME_FILE_CONFIG_KEY,
                TRUST_OS_HOSTNAME_CONFIG_KEY,
            )?,
        })
    }
}

impl EnvironmentProvider for ADPEnvironmentProvider {
    type Host = AgentLikeHostProvider;
    type Workload = NoopWorkloadProvider;
    type Tagger = NoopTaggerProvider;

    fn host(&self) -> &Self::Host {
        &self.host_provider
    }

    fn workload(&self) -> &Self::Workload {
        &NoopWorkloadProvider
    }

    fn tagger(&self) -> &Self::Tagger {
        &NoopTaggerProvider
    }
}
