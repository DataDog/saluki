use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
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
    pub fn authoritative<H>(provider: H) -> Self
    where
        H: HostnameProvider + Send + Sync + 'static,
    {
        Self {
            provider: Arc::new(provider),
            authoritative: true,
        }
    }

    pub fn non_authoritative<H>(provider: H) -> Self
    where
        H: HostnameProvider + Send + Sync + 'static,
    {
        Self {
            provider: Arc::new(provider),
            authoritative: false,
        }
    }

    pub const fn is_authoritative(&self) -> bool {
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
