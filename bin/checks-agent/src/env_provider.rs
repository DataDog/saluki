use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_env::autodiscovery::providers::{
    BoxedAutodiscoveryProvider, LocalAutodiscoveryProvider, RemoteAgentAutodiscoveryProvider,
};
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_env::host::providers::{BoxedHostProvider, FixedHostProvider, RemoteAgentHostProvider};
use saluki_error::GenericError;
use tracing::{debug, warn};

/// Checks-Agent specific environment provider.
///
/// This environment provider is designed for Check Agent's normal deployment environment, which is running alongside the
/// Datadog Agent. The underlying providers will communicate directly with the Datadog Agent to receive information such
/// as the hostname, entity tags, workload metadata events, and more.
///
/// # Opting out for testing/benchmarking
///
/// In order to facilitate testing/benchmarking where running the Datadog Agent is not desirable, the underlying
/// providers can be effectively disabled by setting the `adp.use_fixed_host_provider` configuration value to `true`.
///
/// This will effectively disable origin enrichment (no entity tags) and cause metrics to be tagged with a fixed
/// hostname based on the configuration value of `hostname`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct ChecksAgentEnvProvider {
    host_provider: BoxedHostProvider,
    autodiscovery_provider: BoxedAutodiscoveryProvider,
}

impl ChecksAgentEnvProvider {
    pub async fn from_configuration(
        config: &GenericConfiguration, component_registry: &ComponentRegistry,
    ) -> Result<Self, GenericError> {
        let mut provider_component = component_registry.get_or_create("env_provider");

        let in_standalone_mode = config.get_typed_or_default::<bool>("adp.standalone_mode");
        if in_standalone_mode {
            warn!("Running in standalone mode. Origin detection/enrichment and other features dependent upon the Datadog Agent will not be available.");
        }

        // We allow disabling the normal environment provider functionality via configuration, since in some cases we
        // don't actually care about having a real environment provider as we may simply be running in a benchmark/test
        // environment, etc.
        let host_provider = if in_standalone_mode {
            debug!("Using fixed host provider due to standalone mode. Hostname must be set via `hostname` configuration setting.");
            BoxedHostProvider::from_provider(FixedHostProvider::from_configuration(config)?)
        } else {
            let provider = RemoteAgentHostProvider::from_configuration(config).await?;
            provider_component.bounds_builder().with_subcomponent("host", &provider);

            BoxedHostProvider::from_provider(provider)
        };

        let autodiscovery_provider = if in_standalone_mode {
            debug!("Using local autodiscovery provider due to standalone mode.");
            let mut config_dir = config.get_typed_or_default::<String>("checks_config_dir");
            if config_dir.is_empty() {
                config_dir = "/etc/datadog-agent/conf.d".to_string();
            }
            BoxedAutodiscoveryProvider::from_provider(LocalAutodiscoveryProvider::new(vec![config_dir]))
        } else {
            let client = RemoteAgentClient::from_configuration(config).await?;
            BoxedAutodiscoveryProvider::from_provider(RemoteAgentAutodiscoveryProvider::new(client))
        };

        Ok(Self {
            host_provider,
            autodiscovery_provider,
        })
    }

    pub fn autodiscovery_provider(&self) -> &BoxedAutodiscoveryProvider {
        &self.autodiscovery_provider
    }
}
