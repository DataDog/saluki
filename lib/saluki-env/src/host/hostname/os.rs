use async_trait::async_trait;
use tracing::debug;

use super::{
    util::{get_os_hostname, is_os_hostname_trustworthy},
    HostnameProvider,
};

/// A hostname provider that returns the hostname provided by the operating system.
pub struct OperatingSystemHostnameProvider {
    always_trust: bool,
}

impl OperatingSystemHostnameProvider {
    /// Create a new `OperatingSystemHostnameProvider`.
    ///
    /// If `always_trust` is `true`, the hostname provided by the operating system will always be used. Otherwise, the
    /// provider will never return a hostname.
    pub fn new(always_trust: bool) -> Self {
        Self { always_trust }
    }
}

#[async_trait]
impl HostnameProvider for OperatingSystemHostnameProvider {
    async fn get_hostname(&self) -> Option<String> {
        if self.always_trust || is_os_hostname_trustworthy().await {
            get_os_hostname()
        } else {
            debug!("OS hostname cannot be determined reliably. Skipping use of OS hostname.");
            None
        }
    }
}
