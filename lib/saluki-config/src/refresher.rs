use std::sync::Arc;

use crate::GenericConfiguration;
use reqwest;
use saluki_error::GenericError;
use serde::Deserialize;
use tokio::time::{sleep, Duration};

const DATADOG_AGENT_CONFIG_ENDPOINT: &str = "https://localhost:5004/config/v1/";

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ConfigResponse {
    api_key: String,
}

/// Configuration for setting up `ConfigRefresher`.
#[derive(Default, Deserialize)]
pub struct ConfigRefresherConfiguration {
    api_key: String,
    auth_token_file_path: String,
}

/// The most recent configuration retrieved from the datadog-agent.
#[derive(Default)]
pub struct ConfigRefresher {
    token: String,
    api_key: String,
}

#[allow(unused)]
impl ConfigRefresherConfiguration {
    /// Creates a new `ConfigRefresherConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }
    /// Create `ConfigRefresher` from `ConfigRefresherConfiguration`
    pub fn build(&self) -> Result<ConfigRefresher, GenericError> {
        let raw_bearer_token = std::fs::read_to_string(&self.auth_token_file_path)?;
        Ok(ConfigRefresher {
            token: raw_bearer_token,
            api_key: self.api_key.clone(),
        })
    }
}
impl ConfigRefresher {
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
    #[allow(unused)]
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

        let config_response: ConfigResponse = response.json().await?;

        // self.api_key = config_response.api_key;

        Ok(())
    }

    /// Most recent `api_key` retrieved from the datadog-agent.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}
