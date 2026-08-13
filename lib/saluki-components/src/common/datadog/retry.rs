use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use agent_data_plane_config::shared;
use http::StatusCode;
use saluki_config::GenericConfiguration;
use saluki_io::net::util::retry::{
    DefaultHttpRetryPolicy, ExponentialBackoff, HttpRetryPredicate, StandardHttpClassifier,
};
use tracing::debug;

const RETRY_TXN_DIR: &str = "transactions_to_retry";
const RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS: u64 = 10;

/// Datadog Agent-specific forwarder retry configuration.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct RetryConfiguration {
    /// The minimum backoff factor to use when retrying requests.
    ///
    /// Controls the interval range that a calculated backoff duration can fall within, such that with a minimum
    /// backoff factor of 2.0, calculated backoff durations will fall between `d/2` and `d`, where `d` is the calculated
    /// backoff duration using a purely exponential growth strategy.
    backoff_factor: f64,

    /// The base growth rate of the backoff duration when retrying requests, in seconds.
    backoff_base: f64,

    /// The upper bound of the backoff duration when retrying requests, in seconds.
    backoff_max: f64,

    /// The amount to decrease the error count by when a request is successful.
    ///
    /// This essentially controls how quickly we forget about the number of previous errors when calculating the next
    /// backoff duration for a request that must be retried.
    recovery_error_decrease_factor: u32,

    /// Whether or not a successful request should completely reset the error count.
    recovery_reset: bool,

    /// The maximum in-memory size of the retry queue, in bytes.
    queue_max_size_bytes: u64,

    /// The maximum size of the retry queue on disk, in bytes.
    ///
    /// A value of `0` disables disk persistence.
    storage_max_size_bytes: u64,

    /// The ratio of in-memory retry queue bytes to flush to disk when the queue is full.
    ///
    /// When disk persistence is enabled and the in-memory retry queue does not have enough room for a new transaction,
    /// this controls how much in-memory data ADP moves to disk. For example, `0.5` moves at least half of the configured
    /// in-memory retry queue size to disk during each overflow. If set to `0`, ADP moves only enough old transactions to
    /// disk to make room for the new transaction.
    flush_to_disk_mem_ratio: f64,

    /// The path to the directory where the retry queue will be stored on disk.
    ///
    /// This is empty when neither `forwarder_storage_path` nor `run_path` is configured, in which case
    /// no default path can be calculated.
    storage_path: PathBuf,

    /// The maximum disk usage ratio for storing transactions on disk.
    ///
    /// `0.8` means the Agent can store transactions on disk until `forwarder_storage_max_size_in_bytes`
    /// is reached or when the disk mount for `forwarder_storage_path` exceeds 80% of the disk capacity,
    /// whichever is lower.
    storage_max_disk_ratio: f64,

    /// Maximum age in days for retry files on disk before they are deleted at startup.
    ///
    /// When disk persistence is enabled, ADP removes any `retry-*.json` files in the
    /// per-queue subdirectory of the storage path that are older than this many days
    /// each time it starts. This prevents unbounded disk growth from stale retry data left
    /// behind after long outages.
    outdated_file_in_days: u32,

    /// The time window used to estimate retry queue capacity, in seconds.
    ///
    /// ADP records incoming transaction payload bytes over this window and uses that rate to estimate how many seconds
    /// of data the retry queue can buffer. Values below 10 seconds are clamped to 10 seconds, matching the fixed retry
    /// queue capacity bucket size.
    capacity_time_interval_secs: u64,
}

impl RetryConfiguration {
    /// Creates a new `RetryConfiguration` from the resolved forwarder configuration.
    ///
    /// When no retry-queue storage path is configured, one is derived from `run_path`, matching the
    /// Datadog Agent's own layout. Both may be absent, in which case there is no storage path and
    /// disk persistence cannot be used.
    pub(super) fn from_configuration(forwarder: &shared::Forwarder, config: &GenericConfiguration) -> Self {
        Self {
            backoff_factor: forwarder.backoff_factor,
            backoff_base: forwarder.backoff_base,
            backoff_max: forwarder.backoff_max,
            recovery_error_decrease_factor: forwarder.recovery_interval,
            recovery_reset: forwarder.recovery_reset,
            queue_max_size_bytes: forwarder.effective_retry_queue_max_size_bytes(),
            storage_max_size_bytes: forwarder.storage_max_size_in_bytes,
            flush_to_disk_mem_ratio: forwarder.flush_to_disk_mem_ratio,
            storage_path: resolve_storage_path(&forwarder.storage_path, config),
            storage_max_disk_ratio: forwarder.storage_max_disk_ratio,
            outdated_file_in_days: forwarder.outdated_file_in_days,
            capacity_time_interval_secs: forwarder
                .retry_queue_capacity_time_interval_sec
                .max(RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS),
        }
    }

    /// Returns the maximum size of the retry queue in bytes.
    pub const fn queue_max_size_bytes(&self) -> u64 {
        self.queue_max_size_bytes
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
        self.capacity_time_interval_secs
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

/// Resolves the directory where retry payloads are persisted to disk.
///
/// A configured `forwarder_storage_path` is used as-is. Otherwise the path is derived from
/// `run_path`, which has no typed home: its schema default is an unresolved placeholder, so
/// promoting it to the typed model would put that placeholder in the model.
///
/// TODO: read `run_path` from typed configuration once the placeholder default is resolved.
fn resolve_storage_path(configured: &Path, config: &GenericConfiguration) -> PathBuf {
    if configured.parent().is_some() {
        return configured.to_path_buf();
    }

    match config.try_get_typed::<PathBuf>("run_path") {
        Ok(Some(mut run_path)) => {
            run_path.push(RETRY_TXN_DIR);
            run_path
        }
        Ok(None) => {
            debug!("`forwarder_storage_path` and `run_path` were empty. Cannot calculate default storage path for forwarder.");
            PathBuf::new()
        }
        Err(e) => {
            debug!(error = %e, "Failed to read `run_path` from configuration. Cannot calculate default storage path for forwarder.");
            PathBuf::new()
        }
    }
}

fn secrets_in_use(config: &GenericConfiguration) -> bool {
    matches!(config.try_get_typed::<u64>("secret_refresh_on_api_key_failure_interval"), Ok(Some(value)) if value > 0)
        || matches!(config.try_get_typed::<String>("secret_backend_command"), Ok(Some(value)) if !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::ConfigValue;
    use http::{Request, Response};
    use saluki_config::ConfigurationLoader;
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

    fn would_retry(policy: &mut DefaultHttpRetryPolicy, mut response: TestResponse) -> bool {
        let mut request = test_request();
        Policy::<TestRequest, Response<()>, BoxError>::retry(policy, &mut request, &mut response).is_some()
    }

    async fn empty_config() -> GenericConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        config
    }

    async fn config_from(values: serde_json::Value) -> GenericConfiguration {
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        config
    }

    async fn retry_config_from(forwarder: shared::Forwarder, config: &GenericConfiguration) -> RetryConfiguration {
        RetryConfiguration::from_configuration(&forwarder, config)
    }

    async fn test_retry_config() -> RetryConfiguration {
        // Use small backoffs so that any returned `Sleep` futures are cheap; we never await them, but build them.
        let forwarder = shared::Forwarder {
            backoff_base: 0.001,
            backoff_max: 0.01,
            backoff_factor: 2.0,
            ..Default::default()
        };

        retry_config_from(forwarder, &empty_config().await).await
    }

    #[tokio::test]
    async fn storage_path_is_derived_from_run_path_when_not_configured() {
        const RUN_PATH: &str = "/my/little/run_path";

        let config = config_from(json!({ "run_path": RUN_PATH })).await;
        let retry_config = retry_config_from(shared::Forwarder::default(), &config).await;

        assert_eq!(PathBuf::from(RUN_PATH).join(RETRY_TXN_DIR), retry_config.storage_path());
    }

    #[tokio::test]
    async fn a_configured_storage_path_wins_over_run_path() {
        const RUN_PATH: &str = "/my/little/run_path";
        const FORWARDER_STORAGE_PATH: &str = "/custom/path/to/storage";

        let config = config_from(json!({ "run_path": RUN_PATH })).await;
        let forwarder = shared::Forwarder {
            storage_path: PathBuf::from(FORWARDER_STORAGE_PATH),
            ..Default::default()
        };
        let retry_config = retry_config_from(forwarder, &config).await;

        assert_eq!(PathBuf::from(FORWARDER_STORAGE_PATH), retry_config.storage_path());
    }

    #[tokio::test]
    async fn there_is_no_storage_path_without_a_run_path() {
        // With neither setting, no valid path can be constructed, so disk persistence has nowhere to go.
        let retry_config = retry_config_from(shared::Forwarder::default(), &empty_config().await).await;

        assert_eq!(PathBuf::new(), retry_config.storage_path());
    }

    #[tokio::test]
    async fn queue_max_size_bytes_carries_the_resolved_size() {
        // Which of the two retry-queue settings applies is resolved by the configuration layer; the
        // forwarder stores only the outcome.
        let forwarder = shared::Forwarder {
            retry_queue_payloads_max_size: ConfigValue::defaulted(15 * 1024 * 1024),
            retry_queue_max_size: ConfigValue::explicit(1024),
            ..Default::default()
        };
        let retry_config = retry_config_from(forwarder, &empty_config().await).await;

        assert_eq!(1024, retry_config.queue_max_size_bytes());
    }

    #[tokio::test]
    async fn capacity_time_interval_secs_is_clamped_to_the_bucket_size() {
        let cases = [
            (900, 900),
            (60, 60),
            (1, RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS),
        ];

        for (configured, expected) in cases {
            let forwarder = shared::Forwarder {
                retry_queue_capacity_time_interval_sec: configured,
                ..Default::default()
            };
            let retry_config = retry_config_from(forwarder, &empty_config().await).await;

            assert_eq!(expected, retry_config.capacity_time_interval_secs(), "{configured}");
        }
    }

    #[tokio::test]
    async fn policy_without_config_does_not_retry_403() {
        let retry_config = test_retry_config().await;
        let mut policy = retry_config.to_default_http_retry_policy(None);

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_config_but_no_secrets_does_not_retry_403() {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let retry_config = test_retry_config().await;
        let mut policy = retry_config.to_default_http_retry_policy(Some(config));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_secrets_retries_403() {
        let values = json!({ "secret_backend_command": "/bin/true" });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config = test_retry_config().await;
        let mut policy = retry_config.to_default_http_retry_policy(Some(config));

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_secrets_does_not_affect_other_status_codes() {
        let values = json!({ "secret_backend_command": "/bin/true" });
        let (config, _) = ConfigurationLoader::for_tests(Some(values), None, false).await;
        let retry_config = test_retry_config().await;
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

        use saluki_config::dynamic::{ConfigSetting, ConfigUpdate};

        let (config, sender) = ConfigurationLoader::for_tests(Some(json!({})), None, true).await;
        let sender = sender.expect("dynamic configuration sender should be present");

        // Apply an empty initial snapshot and wait for readiness.
        sender
            .send(ConfigUpdate::snapshot([]))
            .await
            .expect("should send initial snapshot");
        config.ready().await;

        let retry_config = test_retry_config().await;
        let mut policy = retry_config.to_default_http_retry_policy(Some(config.clone()));

        // Before secrets are configured, 403 must not be retried.
        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        // Push a config update that enables secrets management.
        let mut watcher = config.watch_for_updates("secret_backend_command");
        sender
            .send(ConfigUpdate::Partial(ConfigSetting::explicit(
                "secret_backend_command",
                json!("/bin/true"),
            )))
            .await
            .expect("should send partial update");

        tokio::time::timeout(StdDuration::from_secs(2), watcher.changed::<String>())
            .await
            .expect("timed out waiting for secret_backend_command update");

        // The same policy instance must now retry 403 because the predicate reads the live cached secrets flag.
        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }
}
