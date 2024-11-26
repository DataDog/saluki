use std::sync::Arc;

use crate::{ConfigurationError, GenericConfiguration};
use arc_swap::ArcSwap;
use reqwest;
use saluki_error::GenericError;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::time::{sleep, Duration};

const DATADOG_AGENT_CONFIG_ENDPOINT: &str = "https://localhost:5004/config/v1/";

/// Configuration for setting up `RefreshableConfiguration`.
#[derive(Default, Deserialize)]
pub struct RefresherConfiguration {
    auth_token_file_path: String,
}

/// The most recent configuration retrieved from the datadog-agent.
#[derive(Default)]
pub struct RefreshableConfiguration {
    token: String,
    values: ArcSwap<Value>,
}

impl RefresherConfiguration {
    /// Creates a new `ConfigRefresherConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
    /// Create `ConfigRefresher` from `ConfigRefresherConfiguration`
    pub fn build(&self) -> Result<RefreshableConfiguration, GenericError> {
        let raw_bearer_token = std::fs::read_to_string(&self.auth_token_file_path)?;
        Ok(RefreshableConfiguration {
            token: raw_bearer_token,
            values: ArcSwap::from_pointee(serde_json::Value::Null),
        })
    }
}
impl RefreshableConfiguration {
    /// Start a task that queries the datadog-agent config endpoint every 15 seconds.
    pub fn spawn_refresh_task(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                match self.query_agent().await {
                    Ok(_) => {
                        // todo()!
                    }
                    Err(_e) => {
                        // todo()!
                    }
                }
                sleep(Duration::from_secs(15)).await; // Wait for 15 seconds
            }
        });
    }
    /// Query the datadog-agent config endpoint for the latest config
    pub async fn query_agent(&self) -> Result<(), GenericError> {
        let client = reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true) // Allow invalid certificates
            .build()?;

        let response = client
            .get(DATADOG_AGENT_CONFIG_ENDPOINT)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("DD-Agent-Version", "0.1.0")
            .header("User-Agent", "agent-data-plane/0.1.0")
            .send()
            .await?;

        let config_response: Value = response.json().await?;
        self.values.store(Arc::new(config_response));

        Ok(())
    }

    /// Gets a configuration value by key.
    ///
    ///
    /// ## Errors
    ///
    /// If the key does not exist in the configuration, or if the value could not be deserialized into `T`, an error
    /// variant will be returned.
    pub fn get_typed<'a, T>(&self, key: &str) -> Result<T, ConfigurationError>
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
                    field: format!("{}", key).into(),
                })
            }
            None => Err(ConfigurationError::MissingField {
                help_text: format!("RefreshableConfiguration missing key {}", key),
                field: format!("{}", key).into(),
            }),
        }
    }
}
