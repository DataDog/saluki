use std::time::Duration;

use saluki_io::net::util::retry::{DefaultHttpRetryPolicy, ExponentialBackoff};
use serde::Deserialize;

const fn default_request_backoff_factor() -> f64 {
    2.0
}

const fn default_request_backoff_base() -> f64 {
    2.0
}

const fn default_request_backoff_max() -> f64 {
    64.0
}

const fn default_request_recovery_error_decrease_factor() -> u32 {
    2
}

const fn default_request_recovery_reset() -> bool {
    false
}

const fn default_retry_queue_max_size_bytes() -> u64 {
    15 * 1024 * 1024
}

const fn default_storage_max_size_bytes() -> u64 {
    0
}

const fn default_storage_max_disk_ratio() -> f64 {
    0.8
}

fn default_storage_path() -> String {
    "/opt/datadog-agent/run/transactions_to_retry".to_string()
}

/// Datadog Agent-specific forwarder retry configuration.
#[derive(Clone, Deserialize)]
pub struct RetryConfiguration {
    /// The minimum backoff factor to use when retrying requests.
    ///
    /// Controls the the interval range that a calculated backoff duration can fall within, such that with a minimum
    /// backoff factor of 2.0, calculated backoff durations will fall between `d/2` and `d`, where `d` is the calculated
    /// backoff duration using a purely exponential growth strategy.
    ///
    /// Defaults to 2.
    #[serde(default = "default_request_backoff_factor", rename = "forwarder_backoff_factor")]
    backoff_factor: f64,

    /// The base growth rate of the backoff duration when retrying requests, in seconds.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_request_backoff_base", rename = "forwarder_backoff_base")]
    backoff_base: f64,

    /// The upper bound of the backoff duration when retrying requests, in seconds.
    ///
    /// Defaults to 64 seconds.
    #[serde(default = "default_request_backoff_max", rename = "forwarder_backoff_max")]
    backoff_max: f64,

    /// The amount to decrease the error count by when a request is successful.
    ///
    /// This essentially controls how quickly we forget about the number of previous errors when calculating the next
    /// backoff duration for a request that must be retried.
    ///
    /// Increasing this value should be done with caution, as it can lead to more retries being attempted in the same
    /// period of time when downstream services are flapping.
    ///
    /// Defaults to 2.
    #[serde(
        default = "default_request_recovery_error_decrease_factor",
        rename = "forwarder_recovery_interval"
    )]
    recovery_error_decrease_factor: u32,

    /// Whether or not a successful request should completely reset the error count.
    ///
    /// Defaults to `false`.
    #[serde(default = "default_request_recovery_reset", rename = "forwarder_recovery_reset")]
    recovery_reset: bool,

    /// The maximum in-memory size of the retry queue, in bytes.
    ///
    /// Defaults to 15MiB.
    #[serde(
        rename = "forwarder_retry_queue_payloads_max_size",
        alias = "forwarder_retry_queue_max_size",
        default = "default_retry_queue_max_size_bytes"
    )]
    retry_queue_max_size_bytes: u64,

    /// The maximum size of the retry queue on disk, in bytes.
    ///
    /// Defaults to 0 (disabled).
    #[serde(
        rename = "forwarder_storage_max_size_in_bytes",
        default = "default_storage_max_size_bytes"
    )]
    storage_max_size_bytes: u64,

    /// The path to the directory where the retry queue will be stored on disk.
    ///
    /// Defaults to `/opt/datadog-agent/run/transactions_to_retry`.
    #[serde(default = "default_storage_path", rename = "forwarder_storage_path")]
    storage_path: String,

    /// The maximum disk usage ratio for storing transactions on disk.
    ///
    /// Defaults to 0.80.
    ///
    /// `0.8` means the Agent can store transactions on disk until `forwarder_storage_max_size_in_bytes`
    /// is reached or when the disk mount for `forwarder_storage_path` exceeds 80% of the disk capacity,
    /// whichever is lower.
    #[serde(
        default = "default_storage_max_disk_ratio",
        rename = "forwarder_storage_max_disk_ratio"
    )]
    storage_max_disk_ratio: f64,
}

impl RetryConfiguration {
    /// Returns the maximum size of the retry queue in bytes.
    pub fn queue_max_size_bytes(&self) -> u64 {
        self.retry_queue_max_size_bytes
    }

    /// Returns the maximum size of the retry queue on disk, in bytes.
    #[allow(unused)]
    pub fn storage_max_size_bytes(&self) -> u64 {
        self.storage_max_size_bytes
    }

    /// Returns the path to the directory where the retry queue will be stored on disk.
    #[allow(unused)]
    pub fn storage_path(&self) -> &str {
        &self.storage_path
    }

    /// Returns the maximum disk usage ratio for storing transactions on disk.
    #[allow(unused)]
    pub fn storage_max_disk_ratio(&self) -> f64 {
        self.storage_max_disk_ratio
    }

    /// Creates a new [`DefaultHttpRetryPolicy`] based on the forwarder configuration.
    pub fn to_default_http_retry_policy(&self) -> DefaultHttpRetryPolicy {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.backoff_base),
            Duration::from_secs_f64(self.backoff_max),
            self.backoff_factor,
        );

        let recovery_error_decrease_factor = (!self.recovery_reset).then_some(self.recovery_error_decrease_factor);
        DefaultHttpRetryPolicy::with_backoff(retry_backoff)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor)
    }
}
