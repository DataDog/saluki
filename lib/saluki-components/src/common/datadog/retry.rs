use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use facet::Facet;
use http::StatusCode;
use saluki_config_tools::GenericConfiguration;
use saluki_io::net::util::retry::{
    DefaultHttpRetryPolicy, ExponentialBackoff, HttpRetryPredicate, StandardHttpClassifier,
};
use serde::Deserialize;
use tracing::debug;

const FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES: u64 = 15 * 1024 * 1024;
const FORWARDER_FLUSH_TO_DISK_MEM_RATIO: f64 = 0.5;
const RETRY_TXN_DIR: &str = "transactions_to_retry";
const RETRY_QUEUE_CAPACITY_DEFAULT_HISTORY_DURATION_SECS: u64 = 15 * 60;
const RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS: u64 = 10;

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

const fn default_storage_max_size_bytes() -> u64 {
    0
}

const fn default_flush_to_disk_mem_ratio() -> f64 {
    FORWARDER_FLUSH_TO_DISK_MEM_RATIO
}

const fn default_storage_max_disk_ratio() -> f64 {
    0.8
}

const fn default_outdated_file_in_days() -> u32 {
    10
}

const fn default_retry_queue_capacity_time_interval_secs() -> u64 {
    RETRY_QUEUE_CAPACITY_DEFAULT_HISTORY_DURATION_SECS
}

/// Datadog Agent-specific forwarder retry configuration.
#[derive(Clone, Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct RetryConfiguration {
    /// The minimum backoff factor to use when retrying requests.
    ///
    /// Controls the interval range that a calculated backoff duration can fall within, such that with a minimum
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
    #[serde(rename = "forwarder_retry_queue_payloads_max_size")]
    retry_queue_payloads_max_size: Option<u64>,

    /// The maximum in-memory size of the retry queue, in bytes. (deprecated)
    ///
    /// Defaults to 0.
    #[serde(rename = "forwarder_retry_queue_max_size")]
    retry_queue_max_size: Option<u64>,

    /// The maximum size of the retry queue on disk, in bytes.
    ///
    /// Defaults to 0 (disabled).
    #[serde(
        rename = "forwarder_storage_max_size_in_bytes",
        default = "default_storage_max_size_bytes"
    )]
    storage_max_size_bytes: u64,

    /// The ratio of in-memory retry queue bytes to flush to disk when the queue is full.
    ///
    /// When disk persistence is enabled and the in-memory retry queue does not have enough room for a new transaction,
    /// this controls how much in-memory data ADP moves to disk. For example, `0.5` moves at least half of the configured
    /// in-memory retry queue size to disk during each overflow. If set to `0`, ADP moves only enough old transactions to
    /// disk to make room for the new transaction.
    ///
    /// Defaults to 0.5.
    #[serde(
        default = "default_flush_to_disk_mem_ratio",
        rename = "forwarder_flush_to_disk_mem_ratio"
    )]
    flush_to_disk_mem_ratio: f64,

    /// The path to the directory where the retry queue will be stored on disk.
    ///
    /// Defaults to `/opt/datadog-agent/run/transactions_to_retry`.
    #[serde(default, rename = "forwarder_storage_path")]
    storage_path: PathBuf,

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

    /// Maximum age in days for retry files on disk before they are deleted at startup.
    ///
    /// When disk persistence is enabled, ADP removes any `retry-*.json` files in the
    /// per-queue subdirectory of `forwarder_storage_path` that are older than this many days
    /// each time it starts. This prevents unbounded disk growth from stale retry data left
    /// behind after long outages.
    ///
    /// Defaults to 10.
    #[serde(
        default = "default_outdated_file_in_days",
        rename = "forwarder_outdated_file_in_days"
    )]
    outdated_file_in_days: u32,

    /// The time window used to estimate retry queue capacity, in seconds.
    ///
    /// ADP records incoming transaction payload bytes over this window and uses that rate to estimate how many seconds
    /// of data the retry queue can buffer. The default value is 900 seconds. Values below 10 seconds are clamped to 10
    /// seconds, matching the fixed retry queue capacity bucket size.
    #[serde(
        default = "default_retry_queue_capacity_time_interval_secs",
        rename = "forwarder_retry_queue_capacity_time_interval_sec"
    )]
    capacity_time_interval_secs: u64,
}

impl RetryConfiguration {
    pub(super) fn fix_empty_storage_path(&mut self, config: &GenericConfiguration) {
        // If `forwarder_storage_path` is empty, try setting it to a default path based on `run_path`.
        if self.storage_path.parent().is_none() {
            let storage_path = match config.try_get_typed::<PathBuf>("run_path") {
                Ok(Some(mut run_path)) => {
                    run_path.push(RETRY_TXN_DIR);
                    run_path
                }
                Ok(None) => {
                    debug!("`forwarder_storage_path` and `run_path` were empty. Cannot calculate default storage path for forwarder.");
                    return;
                }
                Err(e) => {
                    debug!(error = %e, "Failed to read `run_path` from configuration. Cannot calculate default storage path for forwarder.");
                    return;
                }
            };

            self.storage_path = storage_path;
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
    /// If a [`GenericConfiguration`] is supplied, the policy captures it and checks whether
    /// secrets management is active on every 403 Forbidden response. This allows the retry gate to
    /// pick up runtime changes pushed via the config stream without rebuilding the service. When no
    /// configuration is supplied, 403 responses retain their default non-retriable behavior.
    pub fn to_default_http_retry_policy<B: 'static>(
        &self, live_config: Option<GenericConfiguration>,
    ) -> DefaultHttpRetryPolicy<B> {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.backoff_base),
            Duration::from_secs_f64(self.backoff_max),
            self.backoff_factor,
        );

        let classifier = if let Some(config) = live_config {
            let gate: HttpRetryPredicate<B> =
                Arc::new(move |response| response.status() == StatusCode::FORBIDDEN && secrets_in_use(&config));
            StandardHttpClassifier::new().with_predicate(gate)
        } else {
            StandardHttpClassifier::new()
        };

        let recovery_error_decrease_factor = (!self.recovery_reset).then_some(self.recovery_error_decrease_factor);
        DefaultHttpRetryPolicy::with_backoff_and_classifier(retry_backoff, classifier)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor)
    }
}

fn secrets_in_use(config: &GenericConfiguration) -> bool {
    matches!(config.try_get_typed::<u64>("secret_refresh_on_api_key_failure_interval"), Ok(Some(value)) if value > 0)
        || matches!(config.try_get_typed::<String>("secret_backend_command"), Ok(Some(value)) if !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use http::{Request, Response};
    use saluki_config_tools::ConfigurationLoader;
    use serde_json::json;
    use tower::retry::Policy;

    use super::*;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;
    type TestRequest = Request<()>;
    type TestResponse = Result<Response<()>, BoxError>;

    fn ok_response(status: StatusCode) -> TestResponse {
        Ok(Response::builder().status(status).body(()).unwrap())
    }

    fn test_request() -> TestRequest {
        Request::builder()
            .method("POST")
            .uri("http://localhost/intake")
            .body(())
            .unwrap()
    }

    fn test_retry_config() -> RetryConfiguration {
        // Use small backoffs so that any returned `Sleep` futures are cheap; we never await them, but build them.
        serde_json::from_value(json!({
            "forwarder_backoff_base": 0.001,
            "forwarder_backoff_max": 0.01,
            "forwarder_backoff_factor": 2.0,
        }))
        .expect("RetryConfiguration should deserialize")
    }

    fn would_retry(policy: &mut DefaultHttpRetryPolicy, mut response: TestResponse) -> bool {
        let mut request = test_request();
        Policy::<TestRequest, Response<()>, BoxError>::retry(policy, &mut request, &mut response).is_some()
    }

    #[tokio::test]
    async fn fix_empty_storage_path_sets_path_from_run_path() {
        const RUN_PATH: &str = "/my/little/run_path";

        // Create a base configuration with only `run_path` set.
        let base_config_values = json!({ "run_path": RUN_PATH });
        let (config, _) = ConfigurationLoader::for_tests(Some(base_config_values), None, false).await;

        // Read our retry configuration, and make sure we start out with the expected empty `storage_path`.
        let mut retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(retry_config.storage_path(), PathBuf::new());

        // Try to fix up the empty storage path, and make sure the updated storage path is based on the `run_path` we
        // set on our base configuration.
        retry_config.fix_empty_storage_path(&config);

        let expected = PathBuf::from(RUN_PATH).join(RETRY_TXN_DIR);
        assert_eq!(expected, retry_config.storage_path());
    }

    #[tokio::test]
    async fn fix_empty_storage_path_does_nothing_when_path_already_set() {
        const RUN_PATH: &str = "/my/little/run_path";
        const FORWARDER_STORAGE_PATH: &str = "/custom/path/to/storage";

        // Create a base configuration with both `run_path` and `forwarder_storage_path` set.
        let base_config_values = json!({ "run_path": RUN_PATH, "forwarder_storage_path": FORWARDER_STORAGE_PATH });
        let (config, _) = ConfigurationLoader::for_tests(Some(base_config_values), None, false).await;

        // Read our retry configuration, and make sure we see the storage path that we initially set.
        let mut retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");

        let initial_storage_path = retry_config.storage_path().to_path_buf();
        assert_eq!(initial_storage_path, PathBuf::from(FORWARDER_STORAGE_PATH));

        // Try to fix up the storage path, and make sure nothing changes since it's not actually empty.
        retry_config.fix_empty_storage_path(&config);
        assert_eq!(initial_storage_path, retry_config.storage_path());
    }

    #[tokio::test]
    async fn fix_empty_storage_path_does_nothing_when_run_path_missing() {
        // Create a base configuration for _no_ values set.
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;

        // Read our retry configuration, and make sure we start out with the expected empty `storage_path`.
        let mut retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(retry_config.storage_path(), PathBuf::new());

        // Try to fix up the empty storage path, and make sure the storage path is still empty: when we have no
        // `run_path` set, we can't actually construct a valid path.
        retry_config.fix_empty_storage_path(&config);

        assert_eq!(PathBuf::new(), retry_config.storage_path());
    }

    #[tokio::test]
    async fn queue_max_size_bytes_fallback_behavior() {
        const OVERRIDE_FALLBACK_SIZE_BYTES: u64 = 1024;
        const OVERRIDE_PRIMARY_SIZE_BYTES: u64 = 2048;

        // When neither field is set, returns the default (15 MiB).
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(
            retry_config.queue_max_size_bytes(),
            FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES
        );

        // When only the deprecated field is set, uses it.
        let values = json!({ "forwarder_retry_queue_max_size": OVERRIDE_FALLBACK_SIZE_BYTES });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(retry_config.queue_max_size_bytes(), OVERRIDE_FALLBACK_SIZE_BYTES);

        // When both fields are set, the newer field takes priority.
        let values = json!({
            "forwarder_retry_queue_payloads_max_size": OVERRIDE_PRIMARY_SIZE_BYTES,
            "forwarder_retry_queue_max_size": OVERRIDE_FALLBACK_SIZE_BYTES,
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(retry_config.queue_max_size_bytes(), OVERRIDE_PRIMARY_SIZE_BYTES);
    }

    #[tokio::test]
    async fn flush_to_disk_mem_ratio_uses_agent_default() {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(
            retry_config.flush_to_disk_mem_ratio(),
            FORWARDER_FLUSH_TO_DISK_MEM_RATIO
        );
    }

    #[tokio::test]
    async fn capacity_time_interval_secs_uses_default_override_and_minimum() {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(
            retry_config.capacity_time_interval_secs(),
            RETRY_QUEUE_CAPACITY_DEFAULT_HISTORY_DURATION_SECS
        );

        let values = json!({ "forwarder_retry_queue_capacity_time_interval_sec": 60 });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(retry_config.capacity_time_interval_secs(), 60);

        let values = json!({ "forwarder_retry_queue_capacity_time_interval_sec": 1 });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(
            retry_config.capacity_time_interval_secs(),
            RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS
        );
    }

    #[tokio::test]
    async fn flush_to_disk_mem_ratio_can_be_overridden() {
        const OVERRIDE_RATIO: f64 = 0.25;

        let values = json!({ "forwarder_flush_to_disk_mem_ratio": OVERRIDE_RATIO });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config: RetryConfiguration = config.as_typed().expect("should deserialize");
        assert_eq!(retry_config.flush_to_disk_mem_ratio(), OVERRIDE_RATIO);
    }

    #[tokio::test]
    async fn policy_without_config_does_not_retry_403() {
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(None);

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_config_but_no_secrets_does_not_retry_403() {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(Some(config));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_secrets_retries_403() {
        let values = json!({ "secret_backend_command": "/bin/true" });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(Some(config));

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_secrets_does_not_affect_other_status_codes() {
        let values = json!({ "secret_backend_command": "/bin/true" });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(Some(config));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::OK)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::BAD_REQUEST)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::UNAUTHORIZED)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::PAYLOAD_TOO_LARGE)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::TOO_MANY_REQUESTS)));
    }

    #[tokio::test]
    async fn policy_403_gate_reflects_dynamic_secrets_config_change() {
        use std::time::Duration as StdDuration;

        use saluki_config_tools::dynamic::ConfigUpdate;

        let (config, sender) = ConfigurationLoader::for_tests(Some(json!({})), None, true).await;
        let sender = sender.expect("dynamic configuration sender should be present");

        // Apply an empty initial snapshot and wait for readiness.
        sender
            .send(ConfigUpdate::Snapshot(json!({})))
            .await
            .expect("should send initial snapshot");
        config.ready().await;

        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(Some(config.clone()));

        // Before secrets are configured, 403 must not be retried.
        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        // Push a config update that enables secrets management.
        let mut watcher = config.watch_for_updates("secret_backend_command");
        sender
            .send(ConfigUpdate::Partial {
                key: "secret_backend_command".to_string(),
                value: json!("/bin/true"),
            })
            .await
            .expect("should send partial update");

        tokio::time::timeout(StdDuration::from_secs(2), watcher.changed::<String>())
            .await
            .expect("timed out waiting for secret_backend_command update");

        // The same policy instance must now retry 403 because the predicate reads the live cached secrets flag.
        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }
}
