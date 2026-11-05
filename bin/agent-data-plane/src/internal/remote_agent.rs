//! ADP's configuration, plugged into the shared remote agent runtime.
//!
//! These are thin wrappers: the runtime defines the traits, and ADP's configuration types live in other crates, so
//! the orphan rule requires a local type to implement one for the other.

use std::future::Future;

use agent_data_plane_config::control::MemoryMode;
use agent_data_plane_config_system::{ConfigurationSystem, ConfigurationUpdates, LoadedConfiguration};
use bytesize::ByteSize;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use datadog_agent_runtime::{
    control_plane::ConfigUpdatesRunner,
    env::{HostSettings, WorkloadSettings},
    facade::{AuthoritativeConfiguration, LocalConfiguration, RemoteAgentSpecification},
    logging::LoggingSettings,
};
use datadog_protos::agent::command::v1::remote_command_provider_server::RemoteCommandProviderServer;
use saluki_app::accounting::{MemoryBoundsConfiguration, MemoryMode as AppMemoryMode};
use saluki_config::dynamic::ConfigUpdate;
use saluki_error::GenericError;
use saluki_io::net::ListenAddress;
use tokio::sync::mpsc;
use tonic::server::NamedService;

use crate::{
    config::{host_settings, remote_agent_client_configuration, workload_settings, DataPlaneConfiguration},
    internal::{logging::get_logging_settings, remote_command::RemoteCommandProviderImpl},
};

/// ADP as a remote agent.
pub enum AgentDataPlane {}

impl RemoteAgentSpecification for AgentDataPlane {
    type LocalConfiguration = AdpLocalConfig;
    type AuthoritativeConfiguration = AdpConfig;
    type ConfigurationUpdates = AdpConfigUpdates;

    const ADDITIONAL_SERVICE_NAMES: &'static [&'static str] =
        &[<RemoteCommandProviderServer<RemoteCommandProviderImpl> as NamedService>::NAME];
}

/// ADP's local configuration, loaded from the configuration file and the environment.
pub struct AdpLocalConfig(LoadedConfiguration);

impl AdpLocalConfig {
    /// Wraps the given local configuration.
    pub fn new(local: LoadedConfiguration) -> Self {
        Self(local)
    }
}

impl LocalConfiguration<AgentDataPlane> for AdpLocalConfig {
    fn is_standalone(&self) -> bool {
        DataPlaneConfiguration::from_configuration(self.0.local()).standalone_mode()
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        remote_agent_client_configuration(self.0.local())
    }

    fn advertised_secure_api_address(&self) -> Result<ListenAddress, GenericError> {
        DataPlaneConfiguration::from_configuration(self.0.local()).secure_api_listen_address()
    }

    async fn connect(
        self, config_stream: mpsc::Receiver<ConfigUpdate>,
    ) -> Result<(AdpConfig, AdpConfigUpdates), GenericError> {
        let (system, updates) = self.0.run(config_stream).await?;
        let config = AdpConfig {
            system,
            standalone: false,
        };
        Ok((config, AdpConfigUpdates(updates)))
    }

    async fn into_standalone(self) -> Result<AdpConfig, GenericError> {
        let system = self.0.standalone().await?;
        Ok(AdpConfig {
            system,
            standalone: true,
        })
    }
}

/// ADP's authoritative configuration.
pub struct AdpConfig {
    system: ConfigurationSystem,
    standalone: bool,
}

impl AdpConfig {
    /// Returns the configuration system.
    pub fn system(&self) -> &ConfigurationSystem {
        &self.system
    }
}

impl AuthoritativeConfiguration for AdpConfig {
    fn is_standalone(&self) -> bool {
        self.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        remote_agent_client_configuration(&self.system.config())
    }

    fn logging(&self) -> LoggingSettings {
        get_logging_settings(&self.system.config().control.logging)
    }

    fn workload(&self) -> WorkloadSettings {
        workload_settings(&self.system.config())
    }

    fn host(&self) -> HostSettings {
        host_settings(&self.system.config())
    }

    fn remote_config_enabled(&self) -> bool {
        // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e/comp/trace/config/impl/setup.go#L114-L116
        self.system.config().domains.remote_configuration.enabled
    }

    fn memory_bounds(&self) -> MemoryBoundsConfiguration {
        let control = &self.system.config().control;
        MemoryBoundsConfiguration {
            memory_limit: control.memory_limit.map(ByteSize::b),
            memory_slop_factor: control.memory_slop_factor,
            enable_global_limiter: control.enable_global_limiter,
            memory_mode: match control.memory_mode {
                MemoryMode::Disabled => AppMemoryMode::Disabled,
                MemoryMode::Permissive => AppMemoryMode::Permissive,
                MemoryMode::Strict => AppMemoryMode::Strict,
            },
        }
    }
}

/// Applies the configuration updates that ADP's configuration system receives from the Datadog Agent.
pub struct AdpConfigUpdates(ConfigurationUpdates);

impl ConfigUpdatesRunner for AdpConfigUpdates {
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static {
        let updates = self.0.run();
        async move { updates.await.map_err(GenericError::from) }
    }
}
