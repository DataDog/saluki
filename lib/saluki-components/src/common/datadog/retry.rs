use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use facet::Facet;
use http::{Response, StatusCode};
use saluki_config::GenericConfiguration;
use saluki_io::net::util::retry::{
    DefaultHttpRetryPolicy, ExponentialBackoff, HttpRetryPredicate, StandardHttpClassifier,
};
use serde::Deserialize;
use tracing::debug;

const FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES: u64 = 15 * 1024 * 1024;
const RETRY_TXN_DIR: &str = "transactions_to_retry";

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

const fn default_storage_max_disk_ratio() -> f64 {
    0.8
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

    /// Retry predicate to decide if a particular transaction will be retried.
    #[serde(skip)]
    #[facet(opaque)]
    retry_predicate: Option<RetryPredicate>,
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

    /// Returns the path to the directory where the retry queue will be stored on disk.
    pub fn storage_path(&self) -> &Path {
        &self.storage_path
    }

    /// Returns the maximum disk usage ratio for storing transactions on disk.
    pub const fn storage_max_disk_ratio(&self) -> f64 {
        self.storage_max_disk_ratio
    }

    pub fn with_retry_predicate(&mut self, predicate: HttpRetryPredicate) {
        self.retry_predicate = Some(RetryPredicate::new(predicate));
    }

    /// Creates a new [`DefaultHttpRetryPolicy`] based on the forwarder configuration.
    ///
    /// When a retry predicate is configured, it is added to the standard HTTP classifier.
    pub fn to_default_http_retry_policy<B: 'static>(&self) -> DefaultHttpRetryPolicy<B> {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.backoff_base),
            Duration::from_secs_f64(self.backoff_max),
            self.backoff_factor,
        );

        let mut classifier = StandardHttpClassifier::new();
        if let Some(predicate) = self.retry_predicate.clone() {
            classifier = classifier.with_predicate(predicate.adapt());
        }

        let recovery_error_decrease_factor = (!self.recovery_reset).then_some(self.recovery_error_decrease_factor);
        DefaultHttpRetryPolicy::with_backoff_and_classifier(retry_backoff, classifier)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor)
    }
}

#[derive(Clone)]
struct RetryPredicate {
    predicate: HttpRetryPredicate,
}

impl RetryPredicate {
    fn new(predicate: HttpRetryPredicate) -> Self {
        Self { predicate }
    }

    fn adapt<B: 'static>(self) -> HttpRetryPredicate<B> {
        Arc::new(move |response| {
            let mut unit_response = Response::new(());
            *unit_response.status_mut() = response.status();
            *unit_response.version_mut() = response.version();
            *unit_response.headers_mut() = response.headers().clone();

            (self.predicate)(&unit_response)
        })
    }
}

#[cfg(test)]
impl std::fmt::Debug for RetryPredicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RetryPredicate").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl PartialEq for RetryPredicate {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.predicate, &other.predicate)
    }
}

pub(super) fn secrets_in_use(config: &GenericConfiguration) -> bool {
    matches!(config.try_get_typed::<u64>("secret_refresh_on_api_key_failure_interval"), Ok(Some(value)) if value > 0)
        || matches!(config.try_get_typed::<String>("secret_backend_command"), Ok(Some(value)) if !value.trim().is_empty())
}

pub(super) fn retryable_forbidden_predicate_for_config(
    live_config: GenericConfiguration, retryable_forbidden_predicate: HttpRetryPredicate,
) -> HttpRetryPredicate {
    Arc::new(move |response| {
        if response.status() != StatusCode::FORBIDDEN || !secrets_in_use(&live_config) {
            return false;
        }

        retryable_forbidden_predicate(response)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::{Request, Response, StatusCode};
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
    async fn policy_without_predicate_does_not_retry_403() {
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_predicate_adds_retry_for_403() {
        let mut retry_config = test_retry_config();
        let predicate: HttpRetryPredicate = Arc::new(|response| response.status() == StatusCode::FORBIDDEN);
        retry_config.with_retry_predicate(predicate);
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_predicate_keeps_default_behavior_for_other_status_codes() {
        let mut retry_config = test_retry_config();
        let predicate: HttpRetryPredicate = Arc::new(|response| response.status() == StatusCode::FORBIDDEN);
        retry_config.with_retry_predicate(predicate);
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(!would_retry(&mut policy, ok_response(StatusCode::OK)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::BAD_REQUEST)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::UNAUTHORIZED)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::PAYLOAD_TOO_LARGE)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::TOO_MANY_REQUESTS)));
    }

    #[tokio::test]
    async fn policy_predicate_is_evaluated_for_403() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut retry_config = test_retry_config();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let predicate: HttpRetryPredicate = Arc::new(move |_| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            true
        });
        retry_config.with_retry_predicate(predicate);
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retryable_forbidden_predicate_calls_inner_when_secrets_are_in_use() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (config, _) =
            ConfigurationLoader::for_tests(Some(json!({ "secret_backend_command": "/bin/true" })), None, false).await;
        let mut retry_config = test_retry_config();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        retry_config.with_retry_predicate(retryable_forbidden_predicate_for_config(
            config,
            Arc::new(move |_| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                true
            }),
        ));
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retryable_forbidden_predicate_does_not_call_inner_without_secrets() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let mut retry_config = test_retry_config();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        retry_config.with_retry_predicate(retryable_forbidden_predicate_for_config(
            config,
            Arc::new(move |_| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                true
            }),
        ));
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retryable_forbidden_predicate_does_not_call_inner_for_other_retryable_statuses() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (config, _) =
            ConfigurationLoader::for_tests(Some(json!({ "secret_backend_command": "/bin/true" })), None, false).await;
        let mut retry_config = test_retry_config();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        retry_config.with_retry_predicate(retryable_forbidden_predicate_for_config(
            config,
            Arc::new(move |_| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                true
            }),
        ));
        let mut policy = retry_config.to_default_http_retry_policy();

        assert!(would_retry(&mut policy, ok_response(StatusCode::INTERNAL_SERVER_ERROR)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn secrets_in_use_detects_secret_backend_command() {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        assert!(!secrets_in_use(&config));

        let (config, _) =
            ConfigurationLoader::for_tests(Some(json!({ "secret_backend_command": "/bin/true" })), None, false).await;
        assert!(secrets_in_use(&config));
    }

    #[tokio::test]
    async fn secrets_in_use_detects_refresh_interval() {
        let (config, _) = ConfigurationLoader::for_tests(
            Some(json!({ "secret_refresh_on_api_key_failure_interval": 1u64 })),
            None,
            false,
        )
        .await;
        assert!(secrets_in_use(&config));
    }
}
