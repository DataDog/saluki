use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, GenericError};

use crate::{
    host::hostname::{
        HostnameProvider, KubernetesHostnameProvider, MaybeFileHostnameProvider, MaybeStaticHostnameProvider,
        OperatingSystemHostnameProvider,
    },
    HostProvider,
};

#[derive(Clone)]
struct ChainedProvider {
    provider: Arc<dyn HostnameProvider + Send + Sync>,
    authoritative: bool,
}

impl ChainedProvider {
    fn authoritative<H>(provider: H) -> Self
    where
        H: HostnameProvider + Send + Sync + 'static,
    {
        Self {
            provider: Arc::new(provider),
            authoritative: true,
        }
    }

    fn non_authoritative<H>(provider: H) -> Self
    where
        H: HostnameProvider + Send + Sync + 'static,
    {
        Self {
            provider: Arc::new(provider),
            authoritative: false,
        }
    }

    const fn is_authoritative(&self) -> bool {
        self.authoritative
    }
}

/// An Agent-like host provider.
///
/// This emulates the hostname resolution logic of the Agent, which tries a number of different "providers" that are
/// specific to different environments -- config file, bare metal, cloud, container, etc -- and returns the highest
/// priority value that is valid.
///
/// ## Missing
///
/// - all cloud environments (Azure, EC2, and GCP)
/// - all cloud-lite environments (Fargate)
/// - `hostname_fqdn` configuration setting
#[derive(Clone)]
pub struct AgentLikeHostProvider {
    providers: Vec<ChainedProvider>,
}

impl AgentLikeHostProvider {
    /// Creates a new `AgentLikeHostProvider` from the given configuration.
    ///
    /// Attempts to read multiple values from the provided configuration to control specific behavior when attempting to
    /// resolve the correct hostname. The following parameters control which configuration keys are read:
    /// - `hostname_config_key`: fixed hostname to use (highest priority; optional)
    /// - `hostname_file_config_key`: file path to read hostname from (second highest priority; optional)
    ///
    /// `trust_os_hostname_config_key` describes the configuration value that determines whether or not the operating
    /// system hostname should be trusted. This is only relevant for some environments where the hostname might not be
    /// correct when queried from Saluki.
    pub fn new(
        config: &GenericConfiguration, hostname_config_key: &str, hostname_file_config_key: &str,
        trust_os_hostname_config_key: &str,
    ) -> Result<Self, GenericError> {
        let maybe_hostname = config.try_get_typed::<String>(hostname_config_key)?;
        let maybe_hostname_file_path = config.try_get_typed::<PathBuf>(hostname_file_config_key)?;
        let trust_os_hostname = config
            .try_get_typed::<bool>(trust_os_hostname_config_key)?
            .unwrap_or_default();

        Ok(Self {
            providers: vec![
                // These providers are authoritative: if they return a value, that value gets used and no further
                // providers are checked.
                ChainedProvider::authoritative(MaybeStaticHostnameProvider::new(maybe_hostname)),
                ChainedProvider::authoritative(MaybeFileHostnameProvider::new(maybe_hostname_file_path)),
                // These providers are not authoritative: they're essentially in reverse order, so if one provider
                // returns a value, we still check the next provider, and if that provider returns a value, we use the
                // value from that provider instead, and so on.
                ChainedProvider::non_authoritative(KubernetesHostnameProvider),
                ChainedProvider::non_authoritative(OperatingSystemHostnameProvider::new(trust_os_hostname)),
            ],
        })
    }
}

#[async_trait]
impl HostProvider for AgentLikeHostProvider {
    type Error = GenericError;

    async fn get_hostname(&self) -> Result<String, Self::Error> {
        let mut current_hostname = None;

        for provider in self.providers.iter() {
            match provider.provider.get_hostname().await {
                Some(hostname) => current_hostname = Some(hostname),
                None => continue,
            };

            if provider.is_authoritative() && current_hostname.is_some() {
                break;
            }
        }

        current_hostname.ok_or_else(|| generic_error!("Unable to reliably determine the host name."))
    }
}

impl MemoryBounds for AgentLikeHostProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: Doesn't account for the actual size of any the static values -- like hostname, or path to hostname file
        // -- nor does it account for the strings allocated by any of these providers.
        builder
            .firm()
            // Providers.
            .with_array::<ChainedProvider>(self.providers.len())
            // Size of the known providers being utilized.
            .with_fixed_amount(size_in_arc::<MaybeStaticHostnameProvider>())
            .with_fixed_amount(size_in_arc::<MaybeFileHostnameProvider>())
            .with_fixed_amount(size_in_arc::<KubernetesHostnameProvider>())
            .with_fixed_amount(size_in_arc::<OperatingSystemHostnameProvider>());
    }
}

const fn size_in_arc<T>() -> usize {
    std::mem::size_of::<T>() + (2 * std::mem::size_of::<usize>())
}
