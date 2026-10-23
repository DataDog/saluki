//! Environment providers backed by the Datadog Agent.
//!
//! [`RemoteAgentEnvironmentProvider`] gives a remote agent its hostname, autodiscovery events, and workload metadata.
//! In connected mode, each of those comes from the Datadog Agent over its own connection. In standalone mode, the
//! hostname is fixed and the rest is unavailable.

use std::{future::Future, num::NonZeroUsize, path::PathBuf, time::Duration};

use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use saluki_core::accounting::ComponentRegistry;
use saluki_core::health::HealthRegistry;
use saluki_core::runtime::Supervisor;
use saluki_core::support::SubsystemIdentifier;
use saluki_env::{
    autodiscovery::providers::BoxedAutodiscoveryProvider,
    host::providers::{BoxedHostProvider, FixedHostProvider},
    EnvironmentProvider,
};
use saluki_error::{generic_error, GenericError};
use tracing::warn;

use crate::facade::AuthoritativeConfiguration;

mod autodiscovery;
pub use self::autodiscovery::RemoteAgentAutodiscoveryProvider;

mod host;
pub use self::host::RemoteAgentHostProvider;

mod workload;
pub use self::workload::RemoteAgentWorkloadProvider;

// SAFETY: The value is demonstrably not zero.
const DEFAULT_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(512 * 1024).unwrap();
const DEFAULT_CONTAINERD_CONNECTION_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_CONTAINERD_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn root_provider_id() -> SubsystemIdentifier {
    SubsystemIdentifier::from_segments(["env_provider"])
}

/// Settings for the host provider.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct HostSettings {
    /// Hostname to report, if one is configured explicitly.
    ///
    /// Required in standalone mode, where it's reported as-is. In connected mode, the hostname comes from the Datadog
    /// Agent, which applies its own `hostname` setting, so this is unused.
    pub hostname: Option<String>,
}

/// Settings for the workload provider.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WorkloadSettings {
    /// Size, in bytes, of the string interner shared by the workload metadata collectors.
    ///
    /// The interner holds the entity IDs and tags received from the Datadog Agent and container runtimes. Hosts with
    /// many containers may need to increase it. Defaults to 512 KiB.
    pub string_interner_size_bytes: NonZeroUsize,

    /// Path to the containerd gRPC socket, if one is configured explicitly.
    ///
    /// Defaults to `None`, which probes the well-known socket paths.
    pub containerd_socket_path: Option<PathBuf>,

    /// Timeout for establishing a containerd gRPC connection. Defaults to 1 second.
    pub containerd_connection_timeout: Duration,

    /// Per-RPC timeout for containerd API calls. Defaults to 5 seconds.
    pub containerd_query_timeout: Duration,

    /// Root of the procfs filesystem to read container processes from, if one is configured explicitly.
    ///
    /// Defaults to `None`, which detects the root automatically.
    pub proc_root: Option<PathBuf>,

    /// Root of the cgroupfs filesystem to read container cgroups from, if one is configured explicitly.
    ///
    /// Defaults to `None`, which detects the root automatically.
    pub cgroup_root: Option<PathBuf>,
}

impl WorkloadSettings {
    /// Creates a new `WorkloadSettings` with the given string interner size, and every other setting at its default.
    pub fn new(string_interner_size_bytes: NonZeroUsize) -> Self {
        Self {
            string_interner_size_bytes,
            ..Self::default()
        }
    }
}

impl Default for WorkloadSettings {
    fn default() -> Self {
        Self {
            string_interner_size_bytes: DEFAULT_STRING_INTERNER_SIZE_BYTES,
            containerd_socket_path: None,
            containerd_connection_timeout: DEFAULT_CONTAINERD_CONNECTION_TIMEOUT,
            containerd_query_timeout: DEFAULT_CONTAINERD_QUERY_TIMEOUT,
            proc_root: None,
            cgroup_root: None,
        }
    }
}

/// Environment provider for a remote agent.
///
/// This environment provider is designed for a remote agent's normal deployment environment, which is running alongside
/// the Datadog Agent. The underlying providers communicate directly with the Datadog Agent to receive information such
/// as the hostname, entity tags, workload metadata events, and more.
///
/// # Opting out for testing/benchmarking
///
/// In order to facilitate testing/benchmarking where running the Datadog Agent isn't desirable, the provider runs in
/// standalone mode when the configuration is standalone.
///
/// This effectively disables origin enrichment (no entity tags) and causes metrics to be tagged with a fixed hostname.
#[derive(Clone)]
pub struct RemoteAgentEnvironmentProvider {
    client_config: Option<RemoteAgentClientConfiguration>,
    host_provider: BoxedHostProvider,
    workload_provider: Option<RemoteAgentWorkloadProvider>,
    autodiscovery_provider: Option<BoxedAutodiscoveryProvider>,
    health_registry: HealthRegistry,
}

impl RemoteAgentEnvironmentProvider {
    /// Creates a `RemoteAgentEnvironmentProvider` from the given configuration, along with the [`Supervisor`] that
    /// drives the provider's background work, if it has any.
    ///
    /// In connected mode, the host, workload, and autodiscovery providers each open their own connection to the Datadog
    /// Agent, and the supervisor drives them. In standalone mode, the provider reports the configured hostname, everything
    /// else is a no-op, and no supervisor is returned.
    ///
    /// # Errors
    ///
    /// If any of the underlying providers can't be created, or no hostname is configured in standalone mode, an error is
    /// returned.
    pub async fn from_authoritative<C>(
        config: &C, component_registry: &ComponentRegistry, health_registry: &HealthRegistry,
    ) -> Result<(Self, Option<Supervisor>), GenericError>
    where
        C: AuthoritativeConfiguration,
    {
        if config.is_standalone() {
            let env = Self::standalone(&config.host(), health_registry)?;
            Ok((env, None))
        } else {
            let client_config = config.client_configuration()?;
            let (env, env_supervisor) =
                Self::connected(&config.workload(), client_config, component_registry, health_registry).await?;
            Ok((env, Some(env_supervisor)))
        }
    }

    fn standalone(settings: &HostSettings, health_registry: &HealthRegistry) -> Result<Self, GenericError> {
        warn!("Running in standalone mode. Origin detection/enrichment and other features dependent upon the Datadog Agent will not be available.");

        let hostname = match &settings.hostname {
            Some(hostname) if !hostname.is_empty() => hostname.clone(),
            _ => {
                return Err(generic_error!(
                    "A non-empty hostname must be configured (`hostname`) when running in standalone mode."
                ))
            }
        };

        Ok(Self {
            client_config: None,
            host_provider: BoxedHostProvider::from_provider(FixedHostProvider::new(hostname)),
            workload_provider: None,
            autodiscovery_provider: None,
            health_registry: health_registry.clone(),
        })
    }

    async fn connected(
        settings: &WorkloadSettings, client_config: RemoteAgentClientConfiguration,
        component_registry: &ComponentRegistry, health_registry: &HealthRegistry,
    ) -> Result<(Self, Supervisor), GenericError> {
        let mut env_supervisor = Supervisor::new("env-provider")?;

        let host_provider = RemoteAgentHostProvider::new(&client_config, component_registry).await?;

        let (workload_provider, workload_supervisor) =
            RemoteAgentWorkloadProvider::new(settings, &client_config, component_registry, health_registry).await?;
        env_supervisor.add_worker(workload_supervisor);

        let (autodiscovery_provider, autodiscovery_supervisor) =
            RemoteAgentAutodiscoveryProvider::new(&client_config).await?;
        env_supervisor.add_worker(autodiscovery_supervisor);

        let env = Self {
            client_config: Some(client_config),
            host_provider: BoxedHostProvider::from_provider(host_provider),
            workload_provider: Some(workload_provider),
            autodiscovery_provider: Some(BoxedAutodiscoveryProvider::from_provider(autodiscovery_provider)),
            health_registry: health_registry.clone(),
        };

        Ok((env, env_supervisor))
    }

    /// Returns the configuration for connecting to the Datadog Agent, or `None` in standalone mode.
    pub fn client_configuration(&self) -> Option<&RemoteAgentClientConfiguration> {
        self.client_config.as_ref()
    }

    /// Returns a future that resolves once the environment provider's background subsystems are ready.
    ///
    /// Specifically, this waits for the workload provider's metadata aggregator and collectors to become ready, which
    /// ensures that origin detection and entity tagging are operational before the caller begins processing data. In
    /// standalone mode -- where there is no workload provider -- the returned future resolves immediately.
    pub fn wait_for_ready(&self) -> impl Future<Output = ()> + Send + 'static {
        let health_registry = self.health_registry.clone();
        let has_workload_provider = self.workload_provider.is_some();
        async move {
            if has_workload_provider {
                health_registry.all_ready_under(root_provider_id()).await;
            }
        }
    }
}

impl EnvironmentProvider for RemoteAgentEnvironmentProvider {
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
