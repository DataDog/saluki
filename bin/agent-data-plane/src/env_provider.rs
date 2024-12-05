use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_env::{
    host::providers::{BoxedHostProvider, FixedHostProvider, RemoteAgentHostProvider},
    workload::providers::RemoteAgentWorkloadProvider,
    EnvironmentProvider,
};
use saluki_error::GenericError;
use saluki_health::HealthRegistry;
use tracing::debug;

/// Agent Data Plane-specific environment provider.
///
/// This environment provider is designed for ADP's normal deployment environment, which is running alongside the
/// Datadog Agent. The underlying providers will communicate directly with the Datadog Agent to receive information such
/// as the hostname, entity tags, workload metadata events, and more.
///
/// # Opting out for testing/benchmarking
///
/// In order to facilitate testing/benchmarking where running the Datadog Agent is not desirable, the underlying
/// providers can be effectively disabled by setting the configuration values of `adp.use_fixed_host_provider` and
/// `adp.use_noop_workload_provider` to `true`.
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

        // We allow disabling the normal environment provider functionality via configuration, since in some cases we
        // don't actually care about having a real environment provider as we may simply be running in a benchmark/test
        // environment, etc.
        let use_fixed_host_provider = config.get_typed_or_default::<bool>("adp.use_fixed_host_provider");
        let host_provider = if use_fixed_host_provider {
            debug!("Using fixed host provider as instructed by configuration.");
            BoxedHostProvider::from_provider(FixedHostProvider::from_configuration(config)?)
        } else {
            let provider = RemoteAgentHostProvider::from_configuration(config).await?;
            provider_component.bounds_builder().with_subcomponent("host", &provider);

            BoxedHostProvider::from_provider(provider)
        };

        let use_noop_workload_provider = config.get_typed_or_default::<bool>("adp.use_noop_workload_provider");
        let workload_provider = if use_noop_workload_provider {
            debug!("Using no-op workload provider as instructed by configuration.");
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
