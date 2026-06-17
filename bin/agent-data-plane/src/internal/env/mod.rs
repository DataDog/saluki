use std::future::Future;

use agent_data_plane_config_system::Attachments;
use resource_accounting::ComponentRegistry;
use saluki_component_config::WorkloadConfig;
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::Supervisor;
use saluki_env::{
    autodiscovery::providers::BoxedAutodiscoveryProvider,
    host::providers::{BoxedHostProvider, FixedHostProvider},
    EnvironmentProvider,
};
use saluki_error::GenericError;
use tracing::warn;

use crate::config::DataPlaneConfiguration;

mod autodiscovery;
mod host;
mod workload;
use self::{
    autodiscovery::RemoteAgentAutodiscoveryProvider, host::RemoteAgentHostProvider,
    workload::RemoteAgentWorkloadProvider,
};

/// Agent Data Plane-specific environment provider.
#[derive(Clone)]
pub struct ADPEnvironmentProvider {
    host_provider: BoxedHostProvider,
    workload_provider: Option<RemoteAgentWorkloadProvider>,
    autodiscovery_provider: Option<BoxedAutodiscoveryProvider>,
    health_registry: HealthRegistry,
}

impl ADPEnvironmentProvider {
    /// Creates a typed environment provider and its optional background supervisor.
    ///
    /// # Errors
    ///
    /// If the provider supervisor can't be constructed, an error is returned.
    pub async fn from_data_plane_config(
        dp_config: &DataPlaneConfiguration, workload_config: &WorkloadConfig, attachments: &Attachments,
        component_registry: &ComponentRegistry, health_registry: &HealthRegistry,
    ) -> Result<(Self, Option<Supervisor>), GenericError> {
        if dp_config.standalone_mode() || attachments.datadog_agent.is_none() {
            if !dp_config.standalone_mode() {
                warn!("Datadog Agent attachments are unavailable; using fixed/no-op environment providers.");
            }
            let env = Self {
                host_provider: BoxedHostProvider::from_provider(FixedHostProvider::from_hostname("localhost")),
                workload_provider: None,
                autodiscovery_provider: None,
                health_registry: health_registry.clone(),
            };
            return Ok((env, None));
        }

        let connection = attachments.datadog_agent.as_ref().expect("attachment checked");
        let mut provider_component = component_registry.get_or_create("env_provider");
        let mut env_supervisor = Supervisor::new("env-provider")?;

        let host_provider = RemoteAgentHostProvider::from_client(connection.client());
        provider_component
            .bounds_builder()
            .with_subcomponent("host", &host_provider);

        let (autodiscovery_provider, autodiscovery_supervisor) =
            RemoteAgentAutodiscoveryProvider::from_client(connection.client())?;
        env_supervisor.add_worker(autodiscovery_supervisor);

        let workload_provider = if workload_config.enabled {
            let (workload_provider, workload_supervisor) =
                RemoteAgentWorkloadProvider::from_client(connection.client(), health_registry)?;
            provider_component
                .bounds_builder()
                .with_subcomponent("workload", &workload_provider);
            env_supervisor.add_worker(workload_supervisor);
            Some(workload_provider)
        } else {
            None
        };

        let env = Self {
            host_provider: BoxedHostProvider::from_provider(host_provider),
            workload_provider,
            autodiscovery_provider: Some(BoxedAutodiscoveryProvider::from_provider(autodiscovery_provider)),
            health_registry: health_registry.clone(),
        };
        Ok((env, Some(env_supervisor)))
    }

    /// Returns a future that resolves once the environment provider's background subsystems are ready.
    pub fn wait_for_ready(&self) -> impl Future<Output = ()> + Send + 'static {
        let health_registry = self.health_registry.clone();
        let has_workload_provider = self.workload_provider.is_some();
        async move {
            if has_workload_provider {
                health_registry
                    .all_ready_matching(|name| name.starts_with(workload::WORKLOAD_HEALTH_PREFIX))
                    .await;
            }
        }
    }
}

impl EnvironmentProvider for ADPEnvironmentProvider {
    type Host = BoxedHostProvider;
    type Workload = Option<RemoteAgentWorkloadProvider>;
    type AutodiscoveryProvider = Option<BoxedAutodiscoveryProvider>;

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
