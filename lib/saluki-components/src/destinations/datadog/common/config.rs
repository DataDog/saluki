use std::time::Duration;

use serde::Deserialize;

use super::{endpoints::EndpointConfiguration, retry::RetryConfiguration};

const fn default_request_timeout_secs() -> u64 {
    20
}

const fn default_endpoint_buffer_size() -> usize {
    100
}

/// Forwarder configuration based on the Datadog Agent's forwarder configuration.
///
/// This adapter provides a simple way to utilize the existing configuration values that are passed to the Datadog
/// Agent, which are used to control the behavior of its forwarder, such as retries and concurrency, in conjunction with
/// with existing primitives, as such retry policies in [`saluki_io::util::retry`].
#[derive(Clone, Deserialize)]
pub struct ForwarderConfiguration {
    /// Request timeout, in seconds.
    ///
    /// Defaults to 20 seconds.
    #[serde(default = "default_request_timeout_secs", rename = "forwarder_timeout")]
    request_timeout_secs: u64,

    /// Maximum number of pending requests for an individual endpoint.
    ///
    /// Defaults to 100.
    #[serde(default = "default_endpoint_buffer_size", rename = "forwarder_high_prio_buffer_size")]
    endpoint_buffer_size: usize,

    /// Endpoint configuration.
    #[serde(flatten)]
    endpoint: EndpointConfiguration,

    /// Retry configuration.
    #[serde(flatten)]
    retry: RetryConfiguration,
}

impl ForwarderConfiguration {
    /// Returns the request timeout.
    pub const fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    /// Returns the maximum number of pending requests for an individual endpoint.
    pub const fn endpoint_buffer_size(&self) -> usize {
        self.endpoint_buffer_size
    }

    /// Returns a reference to the endpoint configuration.
    pub fn endpoint(&self) -> &EndpointConfiguration {
        &self.endpoint
    }

    /// Returns a reference to the retry configuration.
    pub fn retry(&self) -> &RetryConfiguration {
        &self.retry
    }
}
