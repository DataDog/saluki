use std::collections::HashMap;

use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_metadata;
use tracing::warn;

// Cache execution information from datadog agent for Python checks
#[derive(Clone)]
pub struct ExecutionContext {
    hostname: String,
    http_headers: HashMap<String, String>,
}

impl Default for ExecutionContext {
    fn default() -> Self {
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
            hostname: "".to_string(),
            http_headers,
        }
    }
}

impl ExecutionContext {
    pub async fn from_environment_provider<E>(environment_provider: &E) -> Self
    where
        E: EnvironmentProvider,
        <E::Host as HostProvider>::Error: std::fmt::Debug,
    {
        let default = Self::default();
        let hostname = environment_provider.host().get_hostname().await.unwrap_or_else(|e| {
            warn!("Failed to get hostname: {:?}", e);
            "".to_string()
        });

        Self { hostname, ..default }
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    #[allow(dead_code)]
    pub fn set_hostname<S: AsRef<str>>(self, hostname: S) -> Self {
        ExecutionContext {
            hostname: hostname.as_ref().to_string(),
            ..self
        }
    }

    pub fn http_headers(&self) -> &HashMap<String, String> {
        &self.http_headers
    }
}
