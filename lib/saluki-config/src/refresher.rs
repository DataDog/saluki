use std::sync::Arc;

use arc_swap::ArcSwap;
use reqwest::ClientBuilder;
use saluki_error::GenericError;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

use crate::{ConfigurationError, GenericConfiguration};

const DATADOG_AGENT_CONFIG_ENDPOINT: &str = "https://localhost:5004/config/v1/";
const DEFAULT_REFRESH_INTERVAL_SECONDS: u64 = 15;

/// Configuration for setting up `RefreshableConfiguration`.
#[derive(Default, Deserialize)]
pub struct RefresherConfiguration {
    auth_token_file_path: String,
    #[serde(default = "default_refresh_interval_seconds")]
    refresh_interval_seconds: u64,
}

fn default_refresh_interval_seconds() -> u64 {
    DEFAULT_REFRESH_INTERVAL_SECONDS
}

/// A configuration whose values are refreshed from a remote source at runtime.
#[derive(Default)]
pub struct RefreshableConfiguration {
    token: String,
    values: Arc<ArcSwap<Value>>,
    refresh_interval_seconds: u64,
}

impl RefresherConfiguration {
    /// Creates a new `RefresherConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
    /// Create `RefreshableConfiguration` from `RefresherConfiguration`.
    pub fn build(&self) -> Result<Arc<RefreshableConfiguration>, GenericError> {
        let raw_bearer_token = std::fs::read_to_string(&self.auth_token_file_path)?;
        let refreshable_configuration = Arc::new(RefreshableConfiguration {
            token: raw_bearer_token,
            values: Arc::new(ArcSwap::from_pointee(serde_json::Value::Null)),
            refresh_interval_seconds: self.refresh_interval_seconds,
        });
        refreshable_configuration.clone().spawn_refresh_task();
        Ok(refreshable_configuration)
    }
}
impl RefreshableConfiguration {
    /// Start a task that queries the datadog-agent config endpoint every 15 seconds.
    fn spawn_refresh_task(self: Arc<Self>) {
        tokio::spawn(async move {
            let client = ClientBuilder::new()
                .danger_accept_invalid_certs(true) // Allow invalid certificates
                .build()
                .expect("failed to create http client");
            loop {
                let response = client
                    .get(DATADOG_AGENT_CONFIG_ENDPOINT)
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {}", self.token))
                    .header("DD-Agent-Version", "0.1.0")
                    .header("User-Agent", "agent-data-plane/0.1.0")
                    .send()
                    .await;
                match response {
                    Ok(response) => {
                        let config_response: Value = response
                            .json()
                            .await
                            .expect("failed to deserialize configuration into json");
                        self.values.store(Arc::new(config_response));
                        debug!("Retrieved configuration from datadog-agent.");
                    }
                    Err(e) => {
                        error!("Error retrieving configuration from datadog-agent: {}", e);
                    }
                }

                sleep(Duration::from_secs(self.refresh_interval_seconds)).await;
            }
        });
    }

    /// Gets a configuration value by key.
    ///
    ///
    /// ## Errors
    ///
    /// If the key does not exist in the configuration, or if the value could not be deserialized into `T`, an error
    /// variant will be returned.
    pub fn get_typed<T>(&self, key: &str) -> Result<T, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let values = self.values.load();
        match values.get(key) {
            Some(value) => {
                // Attempt to deserialize the value to type T
                serde_json::from_value(value.clone()).map_err(|_| ConfigurationError::MissingField {
                    help_text: format!(
                        "RefreshableConfiguration could not convert key {} value into the proper type.",
                        key
                    ),
                    field: key.to_string().into(),
                })
            }
            None => Err(ConfigurationError::MissingField {
                help_text: format!("RefreshableConfiguration missing key {}", key),
                field: key.to_string().into(),
            }),
        }
    }
}
