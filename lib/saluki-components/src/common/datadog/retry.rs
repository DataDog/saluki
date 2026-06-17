use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use saluki_component_config::forwarder as leaf;
use saluki_io::net::util::retry::{DefaultHttpRetryPolicy, ExponentialBackoff, StandardHttpClassifier};

const FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES: u64 = 15 * 1024 * 1024;
const RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS: u64 = 10;

/// Datadog Agent-specific forwarder retry configuration.
///
/// Behavior-carrying runtime type built from its leaf mirror via [`RetryConfiguration::from_native`].
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct RetryConfiguration {
    backoff_factor: f64,

    backoff_base: f64,

    backoff_max: f64,

    recovery_error_decrease_factor: u32,

    recovery_reset: bool,

    retry_queue_payloads_max_size: Option<u64>,

    retry_queue_max_size: Option<u64>,

    storage_max_size_bytes: u64,

    flush_to_disk_mem_ratio: f64,

    storage_path: PathBuf,

    storage_max_disk_ratio: f64,

    outdated_file_in_days: u32,

    capacity_time_interval_secs: u64,
}

impl RetryConfiguration {
    /// Builds the runtime retry configuration from its leaf mirror.
    ///
    /// The leaf `storage_path` is already resolved (the configuration system applies the `run_path`
    /// default fixup that used to live here), so this is a direct field-by-field mapping.
    pub fn from_native(cfg: &leaf::RetryConfiguration) -> Self {
        Self {
            backoff_factor: cfg.backoff_factor,
            backoff_base: cfg.backoff_base,
            backoff_max: cfg.backoff_max,
            recovery_error_decrease_factor: cfg.recovery_error_decrease_factor,
            recovery_reset: cfg.recovery_reset,
            retry_queue_payloads_max_size: cfg.retry_queue_payloads_max_size,
            retry_queue_max_size: cfg.retry_queue_max_size,
            storage_max_size_bytes: cfg.storage_max_size_bytes,
            flush_to_disk_mem_ratio: cfg.flush_to_disk_mem_ratio,
            storage_path: cfg.storage_path.clone(),
            storage_max_disk_ratio: cfg.storage_max_disk_ratio,
            outdated_file_in_days: cfg.outdated_file_in_days,
            capacity_time_interval_secs: cfg.capacity_time_interval_secs,
        }
    }

    /// Returns the maximum size of the retry queue in bytes.
    ///
    /// Preferentially uses `forwarder_retry_queue_payloads_max_size` if set, otherwise uses `forwarder_retry_queue_max_size`. If neither
    /// are set, defaults to 15MiB.
    pub fn queue_max_size_bytes(&self) -> u64 {
        self.retry_queue_payloads_max_size
            .or(self.retry_queue_max_size)
            .unwrap_or(FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES)
    }

    /// Returns the maximum size of the retry queue on disk, in bytes.
    pub const fn storage_max_size_bytes(&self) -> u64 {
        self.storage_max_size_bytes
    }

    /// Returns the ratio of in-memory retry queue bytes to flush to disk when the queue is full.
    pub const fn flush_to_disk_mem_ratio(&self) -> f64 {
        self.flush_to_disk_mem_ratio
    }

    /// Returns the path to the directory where the retry queue will be stored on disk.
    pub fn storage_path(&self) -> &Path {
        &self.storage_path
    }

    /// Returns the maximum disk usage ratio for storing transactions on disk.
    pub const fn storage_max_disk_ratio(&self) -> f64 {
        self.storage_max_disk_ratio
    }

    /// Returns the maximum age in days for retry files on disk before they are deleted at startup.
    pub const fn outdated_file_in_days(&self) -> u32 {
        self.outdated_file_in_days
    }

    /// Returns the time window used to estimate retry queue capacity, in seconds.
    pub const fn capacity_time_interval_secs(&self) -> u64 {
        if self.capacity_time_interval_secs < RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS {
            RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS
        } else {
            self.capacity_time_interval_secs
        }
    }

    /// Creates a new [`DefaultHttpRetryPolicy`] based on the forwarder configuration.
    ///
    /// # Missing
    ///
    /// The previous implementation accepted a live raw-map handle and retried 403 Forbidden
    /// responses whenever secrets management (`secret_backend_command` /
    /// `secret_refresh_on_api_key_failure_interval`) was active. Those keys are not part of the
    /// forwarder config slice and the raw-map watch has been removed in the typed-config cutover, so
    /// 403 responses now always retain their default non-retriable behavior.
    pub fn to_default_http_retry_policy<B: 'static>(&self) -> DefaultHttpRetryPolicy<B> {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.backoff_base),
            Duration::from_secs_f64(self.backoff_max),
            self.backoff_factor,
        );

        let classifier = StandardHttpClassifier::new();

        let recovery_error_decrease_factor = (!self.recovery_reset).then_some(self.recovery_error_decrease_factor);
        DefaultHttpRetryPolicy::with_backoff_and_classifier(retry_backoff, classifier)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor)
    }
}

#[cfg(test)]
mod tests {
    use saluki_component_config::forwarder as leaf;

    use super::*;

    #[test]
    fn from_native_maps_storage_path_directly() {
        let cfg = leaf::RetryConfiguration {
            storage_path: PathBuf::from("/custom/path/to/storage"),
            ..Default::default()
        };
        let retry_config = RetryConfiguration::from_native(&cfg);
        assert_eq!(retry_config.storage_path(), PathBuf::from("/custom/path/to/storage"));
    }

    #[test]
    fn queue_max_size_bytes_fallback_behavior() {
        const OVERRIDE_FALLBACK_SIZE_BYTES: u64 = 1024;
        const OVERRIDE_PRIMARY_SIZE_BYTES: u64 = 2048;

        // When neither field is set, returns the default (15 MiB).
        let retry_config = RetryConfiguration::from_native(&leaf::RetryConfiguration::default());
        assert_eq!(
            retry_config.queue_max_size_bytes(),
            FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES
        );

        // When only the deprecated field is set, uses it.
        let retry_config = RetryConfiguration::from_native(&leaf::RetryConfiguration {
            retry_queue_max_size: Some(OVERRIDE_FALLBACK_SIZE_BYTES),
            ..Default::default()
        });
        assert_eq!(retry_config.queue_max_size_bytes(), OVERRIDE_FALLBACK_SIZE_BYTES);

        // When both fields are set, the newer field takes priority.
        let retry_config = RetryConfiguration::from_native(&leaf::RetryConfiguration {
            retry_queue_payloads_max_size: Some(OVERRIDE_PRIMARY_SIZE_BYTES),
            retry_queue_max_size: Some(OVERRIDE_FALLBACK_SIZE_BYTES),
            ..Default::default()
        });
        assert_eq!(retry_config.queue_max_size_bytes(), OVERRIDE_PRIMARY_SIZE_BYTES);
    }

    #[test]
    fn capacity_time_interval_secs_uses_value_and_minimum() {
        let retry_config = RetryConfiguration::from_native(&leaf::RetryConfiguration {
            capacity_time_interval_secs: 60,
            ..Default::default()
        });
        assert_eq!(retry_config.capacity_time_interval_secs(), 60);

        // Values below the minimum are clamped.
        let retry_config = RetryConfiguration::from_native(&leaf::RetryConfiguration {
            capacity_time_interval_secs: 1,
            ..Default::default()
        });
        assert_eq!(
            retry_config.capacity_time_interval_secs(),
            RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS
        );
    }
}
