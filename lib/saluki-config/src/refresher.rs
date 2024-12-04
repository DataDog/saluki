use std::sync::Arc;

use arc_swap::ArcSwap;
use reqwest::ClientBuilder;
use saluki_error::GenericError;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

use crate::{ConfigurationError, GenericConfiguration};

const DEFAULT_AGENT_IPC_HOST: &str = "localhost";
const DEFAULT_AUTH_TOKEN_FILE_PATH: &str = "/etc/datadog-agent/auth_token";
const DEFAULT_REFRESH_INTERVAL_SECONDS: u64 = 15;

/// Configuration for setting up `RefreshableConfiguration`.
#[derive(Default, Deserialize)]
pub struct RefresherConfiguration {
    /// The location of the auth token used by the Datadog Agent.
    ///
    /// Defaults to `/etc/datadog-agent/auth_token`.
    #[serde(default = "default_auth_token_file_path")]
    auth_token_file_path: String,

    /// The amount of time betweeen each request in seconds.
    ///
    /// Defaults to 15 seconds.
    #[serde(
        rename = "agent_config_refresh_internal_seconds",
        default = "default_refresh_interval_seconds"
    )]
    refresh_interval_seconds: u64,

    /// The IPC host used by the Datadog Agent.
    ///
    /// Defaults to `localhost`.
    #[serde(default = "default_agent_ipc_host")]
    agent_ipc_host: String,

    /// The IPC port used by the Datadog Agent.
    ///
    /// Defaults to `0`.
    #[serde(default = "default_agent_ipc_port")]
    agent_ipc_port: u64,
}

fn default_auth_token_file_path() -> String {
    DEFAULT_AUTH_TOKEN_FILE_PATH.to_owned()
}

fn default_refresh_interval_seconds() -> u64 {
    DEFAULT_REFRESH_INTERVAL_SECONDS
}

fn default_agent_ipc_host() -> String {
    DEFAULT_AGENT_IPC_HOST.to_owned()
}

fn default_agent_ipc_port() -> u64 {
    0
}

/// A configuration whose values are refreshed from a remote source at runtime.
#[derive(Clone, Debug, Default)]
pub struct RefreshableConfiguration {
    endpoint: String,
    token: String,
    values: Arc<ArcSwap<Value>>,
    refresh_interval_seconds: u64,
}

impl RefresherConfiguration {
    /// Builds a `RefreshableConfiguration`, spawning a background task to periodically pull
    /// configuration data and update the configuration.
    ///
    /// # Errors
    ///
    /// If the authentication token be read from the configured authentication token file
    /// path, an error will be returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Create `RefreshableConfiguration` from `RefresherConfiguration`.
    pub fn build(&self) -> Result<RefreshableConfiguration, GenericError> {
        let raw_bearer_token = std::fs::read_to_string(&self.auth_token_file_path)?;
        let endpoint = format!("https://{}:{}/config/v1", self.agent_ipc_host, self.agent_ipc_port);
        let refreshable_configuration = RefreshableConfiguration {
            endpoint,
            token: raw_bearer_token,
            values: Arc::new(ArcSwap::from_pointee(serde_json::Value::Null)),
            refresh_interval_seconds: self.refresh_interval_seconds,
        };
        refreshable_configuration.clone().spawn_refresh_task();
        Ok(refreshable_configuration)
    }
}
impl RefreshableConfiguration {
    /// Start a task that queries the datadog-agent config endpoint every 15 seconds.
    fn spawn_refresh_task(self) {
        tokio::spawn(async move {
            let client = ClientBuilder::new()
                .danger_accept_invalid_certs(true) // Allow invalid certificates
                .build()
                .expect("failed to create http client");
            loop {
                let response = client
                    .get(self.endpoint.clone())
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
                        debug!(
                            remote_endpoint = self.endpoint,
                            "Retrieved configuration from remote source."
                        );
                    }
                    Err(e) => {
                        error!(
                            remote_endpoint = self.endpoint,
                            "Failed to retrieve configuration from remote source: {}", e
                        );
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
                serde_json::from_value(value.clone()).map_err(|_| ConfigurationError::InvalidFieldType {
                    field: key.to_string(),
                    expected_ty: std::any::type_name::<T>().to_string(),
                    actual_ty: serde_json_value_type_name(value).to_string(),
                })
            }
            None => Err(ConfigurationError::MissingField {
                help_text: "Try validating remote source provides this field.".to_string(),
                field: key.to_string().into(),
            }),
        }
    }

    /// Gets a configuration value by key, if it exists.
    ///
    /// If the key exists in the configuration, and can be deserialized, `Ok(Some(value))` is returned. Otherwise,
    /// `Ok(None)` will be returned.
    ///
    /// ## Errors
    ///
    /// If the value could not be deserialized into `T`, an error will be returned.
    pub fn try_get_typed<T>(&self, key: &str) -> Result<Option<T>, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let values = self.values.load();
        match values.get(key) {
            Some(value) => {
                serde_json::from_value(value.clone())
                    .map(Some)
                    .map_err(|_| ConfigurationError::InvalidFieldType {
                        field: key.to_string(),
                        expected_ty: std::any::type_name::<T>().to_string(),
                        actual_ty: serde_json_value_type_name(value).to_string(),
                    })
            }
            None => Ok(None),
        }
    }
}

fn serde_json_value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
