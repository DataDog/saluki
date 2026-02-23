use std::time::Duration;

use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use super::{endpoints::EndpointConfiguration, proxy::ProxyConfiguration, retry::RetryConfiguration};

/// Canonical key for retry queue max size; the deprecated env/key is an alias.
const RETRY_QUEUE_MAX_SIZE_KEY: &str = "forwarder_retry_queue_payloads_max_size";
/// Deprecated key that maps to the same field as `RETRY_QUEUE_MAX_SIZE_KEY`.
const RETRY_QUEUE_MAX_SIZE_ALIAS_KEY: &str = "forwarder_retry_queue_max_size";

const fn default_endpoint_concurrency() -> usize {
    1
}

const fn default_request_timeout_secs() -> u64 {
    20
}

const fn default_endpoint_buffer_size() -> usize {
    16
}

const fn default_forwarder_connection_reset_interval() -> u64 {
    0
}
/// Forwarder configuration based on the Datadog Agent's forwarder configuration.
///
/// This adapter provides a simple way to utilize the existing configuration values that are passed to the Datadog
/// Agent, which are used to control the behavior of its forwarder, such as retries and concurrency, in conjunction with
/// with existing primitives, as such retry policies in [`saluki_io::util::retry`].
#[derive(Clone, Deserialize)]
pub struct ForwarderConfiguration {
    /// Maximum number of concurrent requests for an individual endpoint.
    ///
    /// Defaults to 1.
    #[serde(default = "default_endpoint_concurrency", rename = "forwarder_num_workers")]
    endpoint_concurrency: usize,

    /// Request timeout, in seconds.
    ///
    /// Defaults to 20 seconds.
    #[serde(default = "default_request_timeout_secs", rename = "forwarder_timeout")]
    request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    ///
    /// Defaults to 16.
    #[serde(default = "default_endpoint_buffer_size", rename = "forwarder_high_prio_buffer_size")]
    endpoint_buffer_size: usize,

    /// Endpoint configuration.
    #[serde(flatten)]
    pub(crate) endpoint: EndpointConfiguration,

    /// Retry configuration.
    #[serde(flatten)]
    retry: RetryConfiguration,

    /// Proxy configuration.
    #[serde(flatten)]
    proxy: Option<ProxyConfiguration>,

    /// Connection reset interval, in seconds.
    ///
    /// Defaults to 0.
    #[serde(
        default = "default_forwarder_connection_reset_interval",
        rename = "forwarder_connection_reset_interval"
    )]
    connection_reset_interval_secs: u64,
}

impl ForwarderConfiguration {
    /// Creates a new `ForwarderConfiguration` from the given configuration.
    ///
    /// Uses [`GenericConfiguration::merged_config_as_json_map`] so that merged config (e.g. config
    /// stream snapshot and environment) with duplicate keys or alias keys mapping to the same field
    /// is tolerated; the canonical key wins over the deprecated alias.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut map = config
            .merged_config_as_json_map()
            .map_err(|e| saluki_error::generic_error!("{}", e))?;

        // Collapse deprecated alias so only one key maps to retry_queue_max_size_bytes; avoids
        // "duplicate field" when both keys are present (e.g. from Agent config).
        if map.contains_key(RETRY_QUEUE_MAX_SIZE_KEY) {
            map.remove(RETRY_QUEUE_MAX_SIZE_ALIAS_KEY);
        }

        // Deserialize from the full map: ForwarderConfiguration uses #[serde(flatten)] and does not
        // use deny_unknown_fields, so unknown keys are ignored. Do not add deny_unknown_fields.
        let mut forwarder_config: Self = serde_json::from_value(JsonValue::Object(map)).map_err(GenericError::from)?;

        // Handle fixing up the forwarder storage path if it's empty.
        forwarder_config.retry.fix_empty_storage_path(config);

        Ok(forwarder_config)
    }

    /// Returns the maximum number of concurrent requests for an individual endpoint.
    pub const fn endpoint_concurrency(&self) -> usize {
        self.endpoint_concurrency
    }

    /// Returns the request timeout.
    pub const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    /// Returns the maximum number of pending requests for an individual endpoint.
    pub const fn endpoint_buffer_size(&self) -> usize {
        self.endpoint_buffer_size
    }

    /// Returns a reference to the endpoint configuration.
    pub const fn endpoint(&self) -> &EndpointConfiguration {
        &self.endpoint
    }

    /// Returns a mutable reference to the endpoint configuration.
    pub fn endpoint_mut(&mut self) -> &mut EndpointConfiguration {
        &mut self.endpoint
    }

    /// Returns a reference to the retry configuration.
    pub const fn retry(&self) -> &RetryConfiguration {
        &self.retry
    }

    /// Returns a reference to the proxy configuration.
    pub const fn proxy(&self) -> &Option<ProxyConfiguration> {
        &self.proxy
    }

    /// Returns the connection reset interval.
    pub const fn connection_reset_interval(&self) -> Duration {
        Duration::from_secs(self.connection_reset_interval_secs)
    }
}
