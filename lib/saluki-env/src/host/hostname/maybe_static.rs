use async_trait::async_trait;
use tracing::debug;

use super::{validate_hostname, HostnameProvider};

/// A hostname provider that returns a static hostname.
///
/// This is a convenience provider that allows setting a static hostname based on a configuration, where the
/// configuration value may or may not exist, so the value can be provided via `Option<String>` to avoid having to add
/// that logic at the point of specifying this provider, making it ease to provider a fluent builder.
pub struct MaybeStaticHostnameProvider {
    hostname: Option<String>,
}

impl MaybeStaticHostnameProvider {
    /// Create a new `MaybeStaticHostnameProvider` with the given hostname.
    pub fn new(hostname: Option<String>) -> Self {
        Self { hostname }
    }
}

#[async_trait]
impl HostnameProvider for MaybeStaticHostnameProvider {
    async fn get_hostname(&self) -> Option<String> {
        let hostname = match self.hostname.as_ref() {
            Some(hostname) => hostname.trim(),
            None => {
                debug!("No static hostname provided.");
                return None;
            }
        };

        if let Err(e) = validate_hostname(hostname) {
            debug!(error = %e, "Invalid static hostname.");
            return None;
        }

        Some(hostname.to_string())
    }
}
