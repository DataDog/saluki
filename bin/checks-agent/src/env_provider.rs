use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_env::{
    host::providers::{BoxedHostProvider, FixedHostProvider, RemoteAgentHostProvider},
    workload::providers::RemoteAgentWorkloadProvider,
    EnvironmentProvider,
};
use saluki_error::GenericError;
use saluki_health::HealthRegistry;
use tracing::{debug, warn};

/// Agent Data Plane-specific environment provider.
///
/// This environment provider is designed for ADP's normal deployment environment, which is running alongside the
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
pub struct ADPEnvironmentProvider {
    host_provider: BoxedHostProvider,
    workload_provider: Option<RemoteAgentWorkloadProvider>,
}

impl ADPEnvironmentProvider {
    pub async fn from_configuration(
        config: &GenericConfiguration, component_registry: &ComponentRegistry, health_registry: &HealthRegistry,
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

        let workload_provider = if in_standalone_mode {
            debug!("Using no-op workload provider due to standalone mode. Origin detection/enrichment will not be available.");
            None
        } else {
            let component = component_registry.get_or_create("workload");
            let provider = RemoteAgentWorkloadProvider::from_configuration(config, component, health_registry).await?;
            Some(provider)
        };

        Ok(Self {
            host_provider,
            workload_provider,
        })
    }

    // We do not use this in the Check Agent, for now, so we can safely comment it, to make clippy happy :)
    // /// Returns an API handler for interacting with the underlying data stores powering the workload provider, if one
    // /// has been configured.
    // ///
    // /// See [`RemoteAgentWorkloadAPIHandler`] for more information about routes and responses.
    // pub fn workload_api_handler(&self) -> Option<RemoteAgentWorkloadAPIHandler> {
    //     self.workload_provider.as_ref().map(|provider| provider.api_handler())
    // }
}

impl EnvironmentProvider for ADPEnvironmentProvider {
    type Host = BoxedHostProvider;
    type Workload = Option<RemoteAgentWorkloadProvider>;

    fn host(&self) -> &Self::Host {
        &self.host_provider
    }

    fn workload(&self) -> &Self::Workload {
        &self.workload_provider
    }
}
