use std::future::Future;

use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
use saluki_core::accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder};
use saluki_env::HostProvider;
use saluki_error::{generic_error, ErrorContext, GenericError};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::env::root_provider_id;

/// Datadog Agent-based host provider.
///
/// This provider is based on the internal gRPC API exposed by the Datadog Agent which exposes a way to get the hostname
/// as detected by the Datadog Agent. If the RPC fails, this provider uses the OS hostname when the Core Agent is known to
/// run on the same host. Both results are cached, so every caller gets the same hostname.
#[derive(Clone)]
pub struct RemoteAgentHostProvider {
    client: RemoteAgentClient,
    cached_hostname: OnceCell<String>,
    os_fallback_allowed: bool,
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
            os_fallback_allowed: client_config.core_agent_is_local(),
        };

        let host_provider_id = root_provider_id().child("host").child("remote_agent");
        let mut bounds_builder = component_registry.bounds_builder(&host_provider_id);
        host_provider.specify_bounds(&mut bounds_builder);

        Ok(host_provider)
    }

    async fn get_or_fetch_hostname(&self) -> Result<String, GenericError> {
        let mut client = self.client.clone();
        let fallback = self.os_fallback_allowed.then_some(os_hostname);
        resolve_hostname(&self.cached_hostname, client.get_hostname(), fallback).await
    }
}

/// Returns the cached hostname, or the hostname that the RPC reports.
///
/// When the RPC fails and `fallback` is `Some`, uses the hostname that `fallback` reads. A failed lookup isn't cached.
async fn resolve_hostname<F>(
    cached_hostname: &OnceCell<String>, rpc: impl Future<Output = Result<String, GenericError>>, fallback: Option<F>,
) -> Result<String, GenericError>
where
    F: FnOnce() -> Result<String, GenericError> + Send + 'static,
{
    cached_hostname
        .get_or_try_init(|| async move {
            let rpc_error = match rpc.await {
                Ok(hostname) => return Ok(hostname),
                Err(error) => error,
            };
            let Some(fallback) = fallback else {
                return Err(rpc_error);
            };

            match tokio::task::spawn_blocking(fallback).await {
                Ok(Ok(hostname)) => {
                    warn!(error = %rpc_error, %hostname, "Hostname RPC failed. Using the OS hostname, which can differ from the hostname of the Agent.");
                    Ok(hostname)
                }
                Ok(Err(fallback_error)) => Err(rpc_error)
                    .with_error_context(|| format!("The OS hostname fallback failed too: {fallback_error}")),
                Err(join_error) => Err(rpc_error)
                    .with_error_context(|| format!("The OS hostname fallback did not finish: {join_error}")),
            }
        })
        .await
        .cloned()
}

/// Reads the OS hostname.
///
/// The Datadog trace-agent also uses the OS hostname when its hostname RPC fails, after it tries the Agent binary, and
/// skips it outside the initial UTS namespace:
/// https://github.com/DataDog/datadog-agent/blob/062c8b92d31865a737b1070322175dc2d4dd525c/comp/trace/config/impl/hostname.go#L84-L120
fn os_hostname() -> Result<String, GenericError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        let inode = std::fs::metadata("/proc/self/ns/uts").map(|metadata| metadata.ino());
        if !os_hostname_usable(inode) {
            return Err(generic_error!(
                "The OS hostname isn't usable because this process isn't in the initial UTS namespace. \
                 Set `hostname` in the Agent configuration, or restore access to the Agent."
            ));
        }
    }

    hostname::get()
        .error_context("Failed to read the OS hostname.")?
        .into_string()
        .map_err(|_| generic_error!("The OS hostname isn't valid UTF-8."))
}

#[cfg(any(target_os = "linux", test))]
fn os_hostname_usable(uts_inode: std::io::Result<u64>) -> bool {
    use tracing::debug;

    // Linux gives the initial UTS namespace this fixed inode number. Other UTS namespaces get dynamic inode numbers.
    const INITIAL_UTS_NAMESPACE_INODE: u64 = 0xEFFF_FFFE;

    match uts_inode {
        Ok(inode) => inode == INITIAL_UTS_NAMESPACE_INODE,
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

    fn os_fallback(hostname: &'static str) -> Option<impl FnOnce() -> Result<String, GenericError> + Send + 'static> {
        Some(move || Ok(hostname.to_owned()))
    }

    fn no_fallback() -> Option<fn() -> Result<String, GenericError>> {
        None
    }

    #[tokio::test]
    async fn rpc_hostname_is_cached_without_using_the_fallback() {
        let cache = OnceCell::new();
        let hostname = resolve_hostname(
            &cache,
            async { Ok("rpc-host".to_owned()) },
            Some(|| panic!("a successful RPC must not use the OS hostname")),
        )
        .await
        .unwrap();
        assert_eq!(hostname, "rpc-host");

        let cached = resolve_hostname(
            &cache,
            async { panic!("a cached hostname must not call the RPC") },
            Some(|| panic!("a cached hostname must not use the OS hostname")),
        )
        .await
        .unwrap();
        assert_eq!(cached, "rpc-host");
    }

    #[tokio::test]
    async fn fallback_hostname_is_cached_like_an_rpc_result() {
        // Every pipeline must report the same hostname, as the Datadog Agent does after it resolves it once (#2416).
        let cache = OnceCell::new();
        let hostname = resolve_hostname(
            &cache,
            async { Err(generic_error!("RPC unavailable")) },
            os_fallback("os-host"),
        )
        .await
        .unwrap();
        assert_eq!(hostname, "os-host");

        let cached = resolve_hostname(
            &cache,
            async { Ok("rpc-host".to_owned()) },
            Some(|| panic!("a cached fallback must not read the OS hostname again")),
        )
        .await
        .unwrap();
        assert_eq!(cached, "os-host");
    }

    #[tokio::test]
    async fn failed_fallback_keeps_the_rpc_error_as_context() {
        let cache = OnceCell::new();
        let error = resolve_hostname(
            &cache,
            async { Err(generic_error!("RPC unavailable")) },
            Some(|| Err(generic_error!("OS hostname unavailable"))),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("OS hostname unavailable"), "message: {message}");
        assert!(message.contains("RPC unavailable"), "message: {message}");
        assert!(cache.get().is_none());

        let hostname = resolve_hostname(&cache, async { Ok("recovered-host".to_owned()) }, no_fallback())
            .await
            .unwrap();
        assert_eq!(hostname, "recovered-host");
    }

    #[tokio::test]
    async fn rpc_error_is_returned_when_the_fallback_is_disabled() {
        // With `vsock_addr` set to `host`, the OS hostname belongs to the guest, not to the Core Agent's host.
        let cache = OnceCell::new();
        let error = resolve_hostname(&cache, async { Err(generic_error!("RPC unavailable")) }, no_fallback())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "RPC unavailable");
        assert!(cache.get().is_none());
    }

    #[test]
    fn uts_namespace_check_accepts_only_the_initial_namespace() {
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
