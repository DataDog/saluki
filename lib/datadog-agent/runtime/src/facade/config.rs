//! The types a remote agent supplies to the facade.

use std::future::Future;

use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use saluki_app::accounting::MemoryBoundsConfiguration;
use saluki_config::dynamic::ConfigUpdate;
use saluki_error::GenericError;
use saluki_io::net::ListenAddress;
use tokio::sync::mpsc;

use crate::{
    control_plane::ConfigUpdatesRunner,
    env::{HostSettings, WorkloadSettings},
    logging::LoggingSettings,
};

/// The types that make up a remote agent.
///
/// A binary implements this on a marker type named after itself, such as `enum AgentDataPlane {}`, and drives the
/// startup sequence with [`RemoteAgent<AgentDataPlane>`](super::RemoteAgent).
pub trait RemoteAgentSpecification: Sized + 'static {
    /// The configuration from local sources, loaded before the configuration authority is selected.
    type LocalConfiguration: LocalConfiguration<Self>;

    /// The configuration that results once the configuration authority is selected.
    type AuthoritativeConfiguration: AuthoritativeConfiguration;

    /// Applies the configuration updates that the Datadog Agent sends after its initial snapshot.
    type ConfigurationUpdates: ConfigUpdatesRunner;

    /// Additional gRPC services advertised during registration and served on the privileged API endpoint.
    const ADDITIONAL_SERVICE_NAMES: &'static [&'static str] = &[];
}

/// A remote agent's local configuration, loaded before it connects to the Datadog Agent.
///
/// This is the configuration from local sources only, such as a configuration file and environment variables. It
/// decides whether the remote agent runs standalone, and how it reaches the Datadog Agent. It then becomes the remote
/// agent's [`AuthoritativeConfiguration`], either by layering the Datadog Agent's configuration stream on top of it or,
/// in standalone mode, on its own.
pub trait LocalConfiguration<R: RemoteAgentSpecification>: Send + Sized + 'static {
    /// Returns `true` if the remote agent runs without the Datadog Agent.
    fn is_standalone(&self) -> bool;

    /// Returns the configuration for connecting to the Datadog Agent.
    ///
    /// # Errors
    ///
    /// If the configuration is invalid, an error is returned.
    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError>;

    /// Returns the secure API address that the remote agent advertises when it registers with the Datadog Agent.
    ///
    /// # Errors
    ///
    /// If the configured address is invalid, an error is returned.
    fn advertised_secure_api_address(&self) -> Result<ListenAddress, GenericError>;

    /// Selects the Datadog Agent's configuration stream as the configuration authority.
    ///
    /// Waits for the initial snapshot from `config_stream`, and returns the resulting configuration along with the
    /// updates that apply each later snapshot.
    ///
    /// # Errors
    ///
    /// If the stream closes before its initial snapshot, or the resulting configuration is invalid, an error is
    /// returned.
    fn connect(
        self, config_stream: mpsc::Receiver<ConfigUpdate>,
    ) -> impl Future<Output = Result<(R::AuthoritativeConfiguration, R::ConfigurationUpdates), GenericError>> + Send;

    /// Selects the local configuration as the configuration authority, for standalone mode.
    ///
    /// # Errors
    ///
    /// If the local configuration is invalid, an error is returned.
    fn into_standalone(self) -> impl Future<Output = Result<R::AuthoritativeConfiguration, GenericError>> + Send;
}

/// A remote agent's authoritative configuration, available once the configuration authority is selected.
pub trait AuthoritativeConfiguration: Send + Sync + 'static {
    /// Returns `true` if the local configuration was selected as the configuration authority, for standalone mode.
    fn is_standalone(&self) -> bool;

    /// Returns the configuration for connecting to the Datadog Agent.
    ///
    /// # Errors
    ///
    /// If the configuration is invalid, an error is returned.
    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError>;

    /// Returns the logging settings.
    fn logging(&self) -> LoggingSettings;

    /// Returns the settings for the workload provider.
    fn workload(&self) -> WorkloadSettings;

    /// Returns the settings for the host provider.
    fn host(&self) -> HostSettings;

    /// Returns `true` if Remote Configuration is enabled.
    fn remote_config_enabled(&self) -> bool;

    /// Returns the memory bounds configuration.
    fn memory_bounds(&self) -> MemoryBoundsConfiguration;
}
