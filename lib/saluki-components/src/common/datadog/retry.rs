use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use agent_data_plane_config::{shared::Forwarder, Live, SalukiConfiguration};
use http::StatusCode;
use saluki_io::net::util::retry::{
    DefaultHttpRetryPolicy, ExponentialBackoff, HttpRetryPredicate, StandardHttpClassifier,
};

const FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES: u64 = 15 * 1024 * 1024;
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
    ///
    /// Defaults to 2.
    backoff_factor: f64,

    /// The base growth rate of the backoff duration when retrying requests, in seconds.
    ///
    /// Defaults to 2 seconds.
    backoff_base: f64,

    /// The upper bound of the backoff duration when retrying requests, in seconds.
    ///
    /// Defaults to 64 seconds.
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
    recovery_error_decrease_factor: u32,

    /// Whether or not a successful request should completely reset the error count.
    ///
    /// Defaults to `false`.
    recovery_reset: bool,

    /// The maximum in-memory size of the retry queue, in bytes.
    ///
    /// Defaults to 15MiB.
    retry_queue_payloads_max_size: Option<u64>,

    /// The maximum in-memory size of the retry queue, in bytes. (deprecated)
    ///
    /// Defaults to 0.
    retry_queue_max_size: Option<u64>,

    /// The maximum size of the retry queue on disk, in bytes.
    ///
    /// Defaults to 0 (disabled).
    storage_max_size_bytes: u64,

    /// The ratio of in-memory retry queue bytes to flush to disk when the queue is full.
    ///
    /// When disk persistence is enabled and the in-memory retry queue does not have enough room for a new transaction,
    /// this controls how much in-memory data ADP moves to disk. For example, `0.5` moves at least half of the configured
    /// in-memory retry queue size to disk during each overflow. If set to `0`, ADP moves only enough old transactions to
    /// disk to make room for the new transaction.
    ///
    /// Defaults to 0.5.
    flush_to_disk_mem_ratio: f64,

    /// The path to the directory where the retry queue will be stored on disk.
    ///
    /// Defaults to `/opt/datadog-agent/run/transactions_to_retry`.
    storage_path: PathBuf,

    /// The maximum disk usage ratio for storing transactions on disk.
    ///
    /// Defaults to 0.80.
    ///
    /// `0.8` means the Agent can store transactions on disk until `forwarder_storage_max_size_in_bytes`
    /// is reached or when the disk mount for `forwarder_storage_path` exceeds 80% of the disk capacity,
    /// whichever is lower.
    storage_max_disk_ratio: f64,

    /// Maximum age in days for retry files on disk before they are deleted at startup.
    ///
    /// When disk persistence is enabled, ADP removes any `retry-*.json` files in the
    /// per-queue subdirectory of `forwarder_storage_path` that are older than this many days
    /// each time it starts. This prevents unbounded disk growth from stale retry data left
    /// behind after long outages.
    ///
    /// Defaults to 10.
    outdated_file_in_days: u32,

    /// The time window used to estimate retry queue capacity, in seconds.
    ///
    /// ADP records incoming transaction payload bytes over this window and uses that rate to estimate how many seconds
    /// of data the retry queue can buffer. The default value is 900 seconds. Values below 10 seconds are clamped to 10
    /// seconds, matching the fixed retry queue capacity bucket size.
    capacity_time_interval_secs: u64,
}

impl RetryConfiguration {
    /// Builds retry configuration from the shared forwarder model.
    ///
    /// When the model carries no explicit storage path, the retry-queue directory is derived from
    /// `run_path`; if `run_path` is also empty the path is left empty and disk persistence stays off.
    pub(crate) fn from_model(forwarder: &Forwarder, run_path: &Path) -> Self {
        let mut storage_path = forwarder.storage_path.clone();
        if storage_path.parent().is_none() && !run_path.as_os_str().is_empty() {
            storage_path = run_path.join(RETRY_TXN_DIR);
        }

        Self {
            backoff_factor: forwarder.backoff_factor,
            backoff_base: forwarder.backoff_base,
            backoff_max: forwarder.backoff_max,
            recovery_error_decrease_factor: forwarder.recovery_interval,
            recovery_reset: forwarder.recovery_reset,
            retry_queue_payloads_max_size: forwarder.retry_queue_payloads_max_size,
            retry_queue_max_size: forwarder.retry_queue_max_size,
            storage_max_size_bytes: forwarder.storage_max_size_in_bytes,
            flush_to_disk_mem_ratio: forwarder.flush_to_disk_mem_ratio,
            storage_path,
            storage_max_disk_ratio: forwarder.storage_max_disk_ratio,
            outdated_file_in_days: forwarder.outdated_file_in_days,
            capacity_time_interval_secs: forwarder.retry_queue_capacity_time_interval_sec,
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
    /// If a live configuration view is supplied, the policy captures it and checks whether secrets
    /// management is active on every 403 Forbidden response. This allows the retry gate to pick up
    /// runtime changes pushed via the config stream without rebuilding the service. When no view is
    /// supplied, 403 responses retain their default non-retriable behavior.
    pub fn to_default_http_retry_policy<B: 'static>(
        &self, live: Option<Live<SalukiConfiguration>>,
    ) -> DefaultHttpRetryPolicy<B> {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.backoff_base),
            Duration::from_secs_f64(self.backoff_max),
            self.backoff_factor,
        );

        let classifier = if let Some(live) = live {
            let gate: HttpRetryPredicate<B> =
                Arc::new(move |response| response.status() == StatusCode::FORBIDDEN && secrets_in_use(&live));
            StandardHttpClassifier::new().with_predicate(gate)
        } else {
            StandardHttpClassifier::new()
        };

        let recovery_error_decrease_factor = (!self.recovery_reset).then_some(self.recovery_error_decrease_factor);
        DefaultHttpRetryPolicy::with_backoff_and_classifier(retry_backoff, classifier)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor)
    }
}

fn secrets_in_use(live: &Live<SalukiConfiguration>) -> bool {
    let config = live.current();
    let secrets = &config.shared.secrets;
    secrets.refresh_on_api_key_failure_interval > 0 || !secrets.backend_command.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use http::{Request, Response};
    use tokio::sync::watch;
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
        RetryConfiguration::from_model(
            &Forwarder {
                backoff_base: 0.001,
                backoff_max: 0.01,
                backoff_factor: 2.0,
                ..Default::default()
            },
            Path::new(""),
        )
    }

    fn config_with_secrets(backend_command: &str) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.secrets.backend_command = backend_command.to_string();
        config
    }

    fn would_retry(policy: &mut DefaultHttpRetryPolicy, mut response: TestResponse) -> bool {
        let mut request = test_request();
        Policy::<TestRequest, Response<()>, BoxError>::retry(policy, &mut request, &mut response).is_some()
    }

    #[test]
    fn from_model_sets_storage_path_from_run_path() {
        let retry = RetryConfiguration::from_model(&Forwarder::default(), Path::new("/my/little/run_path"));
        assert_eq!(
            retry.storage_path(),
            PathBuf::from("/my/little/run_path").join(RETRY_TXN_DIR)
        );
    }

    #[test]
    fn from_model_keeps_explicit_storage_path() {
        let forwarder = Forwarder {
            storage_path: PathBuf::from("/custom/path/to/storage"),
            ..Default::default()
        };
        let retry = RetryConfiguration::from_model(&forwarder, Path::new("/my/little/run_path"));
        assert_eq!(retry.storage_path(), PathBuf::from("/custom/path/to/storage"));
    }

    #[test]
    fn from_model_leaves_storage_path_empty_when_run_path_missing() {
        let retry = RetryConfiguration::from_model(&Forwarder::default(), Path::new(""));
        assert_eq!(retry.storage_path(), PathBuf::new());
    }

    #[test]
    fn queue_max_size_bytes_fallback_behavior() {
        // When neither field is set, returns the default (15 MiB).
        let retry = RetryConfiguration::from_model(&Forwarder::default(), Path::new(""));
        assert_eq!(
            retry.queue_max_size_bytes(),
            FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES
        );

        // When only the deprecated field is set, uses it.
        let forwarder = Forwarder {
            retry_queue_max_size: Some(1024),
            ..Default::default()
        };
        assert_eq!(
            RetryConfiguration::from_model(&forwarder, Path::new("")).queue_max_size_bytes(),
            1024
        );

        // When both fields are set, the newer field takes priority.
        let forwarder = Forwarder {
            retry_queue_payloads_max_size: Some(2048),
            retry_queue_max_size: Some(1024),
            ..Default::default()
        };
        assert_eq!(
            RetryConfiguration::from_model(&forwarder, Path::new("")).queue_max_size_bytes(),
            2048
        );
    }

    #[test]
    fn flush_to_disk_mem_ratio_passthrough() {
        let forwarder = Forwarder {
            flush_to_disk_mem_ratio: 0.25,
            ..Default::default()
        };
        assert_eq!(
            RetryConfiguration::from_model(&forwarder, Path::new("")).flush_to_disk_mem_ratio(),
            0.25
        );
    }

    #[test]
    fn capacity_time_interval_secs_clamps_to_minimum() {
        let forwarder = Forwarder {
            retry_queue_capacity_time_interval_sec: 60,
            ..Default::default()
        };
        assert_eq!(
            RetryConfiguration::from_model(&forwarder, Path::new("")).capacity_time_interval_secs(),
            60
        );

        let forwarder = Forwarder {
            retry_queue_capacity_time_interval_sec: 1,
            ..Default::default()
        };
        assert_eq!(
            RetryConfiguration::from_model(&forwarder, Path::new("")).capacity_time_interval_secs(),
            RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS
        );
    }

    #[tokio::test]
    async fn policy_without_config_does_not_retry_403() {
        let mut policy = test_retry_config().to_default_http_retry_policy(None);

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_config_but_no_secrets_does_not_retry_403() {
        let live = Live::fixed(SalukiConfiguration::default());
        let mut policy = test_retry_config().to_default_http_retry_policy(Some(live));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_secrets_retries_403() {
        let live = Live::fixed(config_with_secrets("/bin/true"));
        let mut policy = test_retry_config().to_default_http_retry_policy(Some(live));

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_secrets_does_not_affect_other_status_codes() {
        let live = Live::fixed(config_with_secrets("/bin/true"));
        let mut policy = test_retry_config().to_default_http_retry_policy(Some(live));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::OK)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::BAD_REQUEST)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::UNAUTHORIZED)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::PAYLOAD_TOO_LARGE)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::TOO_MANY_REQUESTS)));
    }

    #[tokio::test]
    async fn policy_403_gate_reflects_dynamic_secrets_config_change() {
        // The gate reads the live view on every 403, so storing a new configuration flips it without
        // rebuilding the policy.
        let cell = Arc::new(ArcSwap::from_pointee(SalukiConfiguration::default()));
        let (tx, rx) = watch::channel(());
        let live = Live::dynamic(Arc::clone(&cell), rx, |c| c);

        let mut policy = test_retry_config().to_default_http_retry_policy(Some(live));

        // Before secrets are configured, 403 must not be retried.
        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        // Enabling secrets management through the live view flips the gate on.
        cell.store(Arc::new(config_with_secrets("/bin/true")));
        tx.send(()).expect("live cell should have a receiver");

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }
}
