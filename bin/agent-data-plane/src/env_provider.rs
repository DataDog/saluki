use saluki_config::GenericConfiguration;
use saluki_env::{
    features::FeatureDetector, host::providers::AgentLikeHostProvider, workload::providers::AgentLikeWorkloadProvider,
    EnvironmentProvider,
};

const HOSTNAME_CONFIG_KEY: &str = "hostname";
const HOSTNAME_FILE_CONFIG_KEY: &str = "hostname_file";
const TRUST_OS_HOSTNAME_CONFIG_KEY: &str = "hostname_trust_uts_namespace";

#[derive(Clone)]
pub struct ADPEnvironmentProvider {
    host_provider: AgentLikeHostProvider,
    workload_provider: AgentLikeWorkloadProvider,
}

impl ADPEnvironmentProvider {
    pub async fn with_configuration(config: &GenericConfiguration) -> Result<Self, String> {
        let feature_detector = FeatureDetector::automatic(config);

        Ok(Self {
            host_provider: AgentLikeHostProvider::new(
                config,
                HOSTNAME_CONFIG_KEY,
                HOSTNAME_FILE_CONFIG_KEY,
                TRUST_OS_HOSTNAME_CONFIG_KEY,
            )?,
            workload_provider: AgentLikeWorkloadProvider::new(config, feature_detector).await?,
        })
    }
}

impl EnvironmentProvider for ADPEnvironmentProvider {
    type Host = AgentLikeHostProvider;
    type Workload = AgentLikeWorkloadProvider;

    fn host(&self) -> &Self::Host {
        &self.host_provider
    }

    fn workload(&self) -> &Self::Workload {
        &self.workload_provider
    }
}
