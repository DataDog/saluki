//! Check runner environment provider.
//!
//! This module provides the environment provider for the check-runner binary,
//! which operates in connected mode only (requires Datadog Agent).

use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_env::{
    host::providers::{BoxedHostProvider, RemoteAgentHostProvider},
    workload::providers::RemoteAgentWorkloadProvider,
    EnvironmentProvider,
};
use saluki_error::GenericError;
use saluki_health::HealthRegistry;

use crate::single_check_autodiscovery::SingleCheckAutodiscoveryProvider;

/// Check runner environment provider.
///
/// This environment provider is designed for the check-runner binary, which operates
/// in connected mode only. It communicates with the Datadog Agent to receive hostname
/// and workload metadata, but uses a `SingleCheckAutodiscoveryProvider` for autodiscovery
/// since the check configuration is provided via stdin.
#[derive(Clone)]
pub struct CheckRunnerEnvironmentProvider {
    host_provider: BoxedHostProvider,
    workload_provider: RemoteAgentWorkloadProvider,
    autodiscovery_provider: SingleCheckAutodiscoveryProvider,
}

impl CheckRunnerEnvironmentProvider {
    /// Creates a new `CheckRunnerEnvironmentProvider` from the given configuration.
    pub async fn from_configuration(
        config: &GenericConfiguration,
        component_registry: &ComponentRegistry,
        health_registry: &HealthRegistry,
        autodiscovery_provider: SingleCheckAutodiscoveryProvider,
    ) -> Result<Self, GenericError> {
        let mut provider_component = component_registry.get_or_create("env_provider");

        // Always use RemoteAgentHostProvider (connected mode only)
        let host_provider = {
            let provider = RemoteAgentHostProvider::from_configuration(config).await?;
            provider_component
                .bounds_builder()
                .with_subcomponent("host", &provider);

            BoxedHostProvider::from_provider(provider)
        };

        // Always use RemoteAgentWorkloadProvider (connected mode only)
        let workload_component = component_registry.get_or_create("workload");
        let workload_provider =
            RemoteAgentWorkloadProvider::from_configuration(config, workload_component, health_registry).await?;

        Ok(Self {
            host_provider,
            workload_provider,
            autodiscovery_provider,
        })
    }
}

impl EnvironmentProvider for CheckRunnerEnvironmentProvider {
    type Host = BoxedHostProvider;
    type Workload = RemoteAgentWorkloadProvider;
    type AutodiscoveryProvider = SingleCheckAutodiscoveryProvider;

    fn host(&self) -> &Self::Host {
        &self.host_provider
    }

    fn workload(&self) -> &Self::Workload {
        &self.workload_provider
    }

    fn autodiscovery(&self) -> &Self::AutodiscoveryProvider {
        &self.autodiscovery_provider
    }
}
