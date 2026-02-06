use std::{collections::HashMap, sync::Arc};

use saluki_config::GenericConfiguration;
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_metadata;
use tracing::warn;

/// Global/shared configuration for checks.
///
/// This provides information to checks, either from the configuration or computed from the EnvironmentProvider if one
/// is provided.
#[derive(Clone)]
pub struct ExecutionContext {
    configuration: GenericConfiguration,
    hostname: Arc<str>,
    http_headers: HashMap<String, String>,
}

#[allow(dead_code)]
impl ExecutionContext {
    pub fn new(configuration: GenericConfiguration) -> Self {
        let http_headers = HashMap::from([
            (
                "User-Agent".to_string(),
                format!("Datadog Agent/{}", saluki_metadata::get_app_details().version().raw()).to_string(),
            ),
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "text/html, */*".to_string()),
        ]);

        Self {
            configuration,
            hostname: Arc::from("unknown"),
            http_headers,
        }
    }

    /// Create an `ExecutionContext` from an `EnvironmentProvider`.
    ///
    /// The `EnvironmentProvider` is used to compute information from the environment, which are cached into the
    /// `ExecutionContext`.
    pub async fn from_environment_provider<E>(configuration: GenericConfiguration, environment_provider: &E) -> Self
    where
        E: EnvironmentProvider,
        <E::Host as HostProvider>::Error: std::fmt::Debug,
    {
        let execution_context = Self::new(configuration);
        let hostname = environment_provider.host().get_hostname().await.unwrap_or_else(|e| {
            warn!(error = ?e, "Failed to get hostname");
            "".to_string()
        });

        Self {
            hostname: Arc::from(hostname),
            ..execution_context
        }
    }

    /// Get a reference to the configuration used to create this `ExecutionContext`.
    pub fn configuration(&self) -> &GenericConfiguration {
        &self.configuration
    }

    /// Get the hostname.
    ///
    /// Computed from the `EnvironmentProvider` if one has been provided to the `ExecutionContext` constructor.
    pub fn hostname(&self) -> Arc<str> {
        self.hostname.clone()
    }

    /// Override the hostname.
    #[allow(dead_code)]
    pub fn with_hostname<S: AsRef<str>>(self, hostname: S) -> Self {
        Self {
            hostname: Arc::from(hostname.as_ref()),
            ..self
        }
    }

    /// Get a reference to the HTTP headers.
    pub fn http_headers(&self) -> &HashMap<String, String> {
        &self.http_headers
    }
}
