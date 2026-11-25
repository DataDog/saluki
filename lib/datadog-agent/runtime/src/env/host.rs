use std::future::Future;

use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
use saluki_core::accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder};
use saluki_env::HostProvider;
use saluki_error::{generic_error, ErrorContext, GenericError};
use tokio::sync::OnceCell;
use tracing::debug;

use crate::env::root_provider_id;

/// Datadog Agent-based host provider.
///
/// This provider is based on the internal gRPC API exposed by the Datadog Agent which exposes a way to get the hostname
/// as detected by the Datadog Agent. If the RPC fails, this provider tries the OS hostname.
#[derive(Clone)]
pub struct RemoteAgentHostProvider {
    client: RemoteAgentClient,
    cached_hostname: OnceCell<String>,
}

impl RemoteAgentHostProvider {
    /// Creates a new `RemoteAgentHostProvider` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the Agent gRPC client can't be created (invalid API endpoint, missing authentication token, etc), or if the
    /// authentication token is invalid, an error will be returned.
    pub async fn new(
        client_config: &RemoteAgentClientConfiguration, component_registry: &ComponentRegistry,
    ) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::connect(client_config).await?;

        let host_provider = Self {
            client,
            cached_hostname: OnceCell::new(),
        };

        let host_provider_id = root_provider_id().child("host").child("remote_agent");
        let mut bounds_builder = component_registry.bounds_builder(&host_provider_id);
        host_provider.specify_bounds(&mut bounds_builder);

        Ok(host_provider)
    }

    async fn get_or_fetch_hostname(&self) -> Result<String, GenericError> {
        let mut client = self.client.clone();
        resolve_hostname(&self.cached_hostname, client.get_hostname(), os_hostname).await
    }
}

async fn resolve_hostname<F>(
    cached_hostname: &OnceCell<String>, rpc: impl Future<Output = Result<String, GenericError>>, fallback: F,
) -> Result<String, GenericError>
where
    F: FnOnce() -> Result<String, GenericError> + Send + 'static,
{
    cached_hostname
        .get_or_try_init(|| async move {
            match rpc.await {
                Ok(hostname) => Ok(hostname),
                Err(error) => {
                    debug!(%error, "Hostname RPC failed. Trying the OS hostname.");
                    let hostname = tokio::task::spawn_blocking(fallback)
                        .await
                        .error_context("Failed to join the OS hostname lookup task.")??;
                    debug!(%hostname, "Using the OS hostname after the hostname RPC failed.");
                    Ok(hostname)
                }
            }
        })
        .await
        .cloned()
}

fn os_hostname() -> Result<String, GenericError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        let inode = std::fs::metadata("/proc/self/ns/uts").map(|metadata| metadata.ino());
        if !os_hostname_usable(inode) {
            return Err(generic_error!(
                "Cannot use the OS hostname from a container UTS namespace. \
                 Restore hostname RPC access or set hostname in the Agent configuration."
            ));
        }
    }

    hostname::get()
        .error_context("Failed to read the OS hostname.")?
        .into_string()
        .map_err(|_| generic_error!("The OS hostname is not valid UTF-8."))
}

#[cfg(any(target_os = "linux", test))]
fn os_hostname_usable(uts_inode: std::io::Result<u64>) -> bool {
    // Linux assigns this fixed inode to the initial UTS namespace, not to the container's PID 1 namespace.
    const HOST_UTS_NAMESPACE_INODE: u64 = 0xEFFF_FFFE;

    match uts_inode {
        Ok(inode) => inode == HOST_UTS_NAMESPACE_INODE,
        Err(error) => {
            debug!(%error, "Could not check the UTS namespace. Allowing the OS hostname fallback.");
            true
        }
    }
}

#[async_trait]
impl HostProvider for RemoteAgentHostProvider {
    type Error = GenericError;

    async fn get_hostname(&self) -> Result<String, Self::Error> {
        self.get_or_fetch_hostname().await
    }
}

impl MemoryBounds for RemoteAgentHostProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Self>("component struct");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rpc_hostname_is_cached_without_using_the_fallback() {
        let cache = OnceCell::new();
        let hostname = resolve_hostname(&cache, async { Ok("rpc-host".to_owned()) }, || {
            panic!("a successful RPC must not use the OS hostname")
        })
        .await
        .unwrap();
        assert_eq!(hostname, "rpc-host");

        let cached = resolve_hostname(
            &cache,
            async { panic!("a cached hostname must not call the RPC") },
            || panic!("a cached hostname must not use the OS hostname"),
        )
        .await
        .unwrap();
        assert_eq!(cached, "rpc-host");
    }

    #[tokio::test]
    async fn fallback_hostname_is_cached_after_rpc_failure() {
        // A failed hostname RPC must not leave payloads without a hostname when the OS fallback succeeds (#2416).
        let cache = OnceCell::new();
        let hostname = resolve_hostname(&cache, async { Err(generic_error!("RPC unavailable")) }, || {
            Ok("os-host".to_owned())
        })
        .await
        .unwrap();
        assert_eq!(hostname, "os-host");

        let cached = resolve_hostname(
            &cache,
            async { panic!("a cached fallback must not retry the RPC") },
            || panic!("a cached fallback must not read the OS hostname again"),
        )
        .await
        .unwrap();
        assert_eq!(cached, "os-host");
    }

    #[tokio::test]
    async fn failed_fallback_is_not_cached_and_can_be_retried() {
        let cache = OnceCell::new();
        let error = resolve_hostname(&cache, async { Err(generic_error!("RPC unavailable")) }, || {
            Err(generic_error!("OS hostname unavailable"))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "OS hostname unavailable");
        assert!(cache.get().is_none());

        let hostname = resolve_hostname(&cache, async { Ok("recovered-host".to_owned()) }, || {
            panic!("a recovered RPC must not use the OS hostname")
        })
        .await
        .unwrap();
        assert_eq!(hostname, "recovered-host");
    }

    #[test]
    fn uts_namespace_check_rejects_container_hostnames() {
        assert!(os_hostname_usable(Ok(0xEFFF_FFFE)));
        assert!(!os_hostname_usable(Ok(0xF000_0001)));
        assert!(!os_hostname_usable(Ok(0)));
    }

    #[test]
    fn unavailable_uts_namespace_does_not_prevent_fallback() {
        assert!(os_hostname_usable(Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "namespace metadata unavailable",
        ))));
    }
}
