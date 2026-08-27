use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use agent_data_plane_config::shared::{self, Secrets};
use agent_data_plane_config::Live;
use http::{Response, StatusCode};
use saluki_common::task::spawn_traced_named;
use saluki_io::net::util::retry::{
    ExponentialBackoff, RetryClassifier, RollingExponentialBackoffRetryPolicy, StandardHttpClassifier,
    StandardHttpRetryLifecycle,
};
use tracing::debug;

const RETRY_TXN_DIR: &str = "transactions_to_retry";
const RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS: u64 = 10;

pub(crate) type SecretsHttpRetryPolicy<B = ()> =
    RollingExponentialBackoffRetryPolicy<SecretsHttpClassifier<B>, StandardHttpRetryLifecycle>;

/// Whether secret resolution might replace a rejected API key.
///
/// The retry classifier reads this on every `403 Forbidden` response, and a [`SecretsGateRefresher`]
/// writes it when typed configuration changes. Cloning shares the gate, so every endpoint's classifier
/// follows the same setting.
#[derive(Clone)]
pub(crate) struct SecretsGate {
    in_use: Arc<AtomicBool>,
}

impl SecretsGate {
    /// Creates a gate reporting `secrets`, which nothing updates.
    pub(crate) fn new_fixed(secrets: &Secrets) -> Self {
        Self {
            in_use: Arc::new(AtomicBool::new(secrets.in_use())),
        }
    }

    fn store(&self, secrets: &Secrets) {
        self.in_use.store(secrets.in_use(), Ordering::Relaxed);
    }

    fn is_open(&self) -> bool {
        self.in_use.load(Ordering::Relaxed)
    }
}

/// Updates a [`SecretsGate`] as typed configuration changes.
pub(crate) struct SecretsGateRefresher {
    secrets: Live<Secrets>,

    /// The gate the classifiers read. Clone it before spawning.
    pub(crate) gate: SecretsGate,
}

impl SecretsGateRefresher {
    /// Creates a refresher whose gate reports what `secrets` projects now.
    ///
    /// Seeding here rather than in the task closes the same gap `ApiKeyRefresher` closes: a configuration
    /// store between view creation and the first `changed` would otherwise leave the gate reporting the
    /// older setting.
    pub(crate) fn new(secrets: Live<Secrets>) -> Self {
        let gate = SecretsGate::new_fixed(&secrets);

        Self { secrets, gate }
    }

    /// Spawns the task that follows the view.
    pub(crate) fn spawn(mut self) {
        spawn_traced_named("dd-secrets-gate-refresher", async move {
            loop {
                let secrets = self.secrets.changed().await;
                self.gate.store(&secrets);
            }
        });
    }
}

pub(crate) struct SecretsHttpClassifier<B = ()> {
    standard: StandardHttpClassifier<B>,
    secrets: SecretsGate,
}

impl<B> Clone for SecretsHttpClassifier<B> {
    fn clone(&self) -> Self {
        Self {
            standard: self.standard.clone(),
            secrets: self.secrets.clone(),
        }
    }
}

impl<B, Error> RetryClassifier<Response<B>, Error> for SecretsHttpClassifier<B> {
    fn should_retry(&mut self, response: &Result<Response<B>, Error>) -> bool {
        if let Ok(response) = response {
            if response.status() == StatusCode::FORBIDDEN && self.secrets.is_open() {
                return true;
            }
        }

        self.standard.should_retry(response)
    }
}

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
    pub(super) fn from_configuration(forwarder: &shared::Forwarder, run_path: Option<&Path>) -> Self {
        Self {
            backoff_factor: forwarder.backoff_factor,
            backoff_base: forwarder.backoff_base,
            backoff_max: forwarder.backoff_max,
            recovery_error_decrease_factor: forwarder.recovery_interval,
            recovery_reset: forwarder.recovery_reset,
            queue_max_size_bytes: forwarder.effective_retry_queue_max_size_bytes(),
            storage_max_size_bytes: forwarder.storage_max_size_in_bytes,
            flush_to_disk_mem_ratio: forwarder.flush_to_disk_mem_ratio,
            storage_path: resolve_storage_path(&forwarder.storage_path, run_path),
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

    /// Creates a new HTTP retry policy based on the forwarder configuration.
    ///
    /// A `403 Forbidden` response is retriable only when `secrets` reports that secret resolution might replace the
    /// rejected API key. The gate is updated as configuration changes, so turning secrets management on or off while the
    /// process runs changes the gate without rebuilding the service. A gate reporting nothing configured leaves a 403
    /// non-retriable, which is the default behavior.
    pub(crate) fn to_default_http_retry_policy<B: 'static>(&self, secrets: SecretsGate) -> SecretsHttpRetryPolicy<B> {
        let retry_backoff = ExponentialBackoff::with_jitter(
            Duration::from_secs_f64(self.backoff_base),
            Duration::from_secs_f64(self.backoff_max),
            self.backoff_factor,
        );
        let classifier = SecretsHttpClassifier {
            standard: StandardHttpClassifier::new(),
            secrets,
        };

        let recovery_error_decrease_factor = (!self.recovery_reset).then_some(self.recovery_error_decrease_factor);
        RollingExponentialBackoffRetryPolicy::new(classifier, retry_backoff)
            .with_retry_lifecycle(StandardHttpRetryLifecycle)
            .with_recovery_error_decrease_factor(recovery_error_decrease_factor)
    }
}

/// Resolves the directory where retry payloads are persisted to disk.
///
/// A configured `forwarder_storage_path` is used as-is. Otherwise the path is derived from
/// `run_path`. If neither is configured, no storage path is available.
fn resolve_storage_path(configured: &Path, run_path: Option<&Path>) -> PathBuf {
    if configured.parent().is_some() {
        return configured.to_path_buf();
    }

    match run_path {
        Some(run_path) => run_path.join(RETRY_TXN_DIR),
        None => {
            debug!("`forwarder_storage_path` and `run_path` were empty. Cannot calculate default storage path for forwarder.");
            PathBuf::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::{ConfigValue, SalukiConfiguration};
    use http::{Request, Response};
    use tower::retry::Policy;

    use super::*;
    use crate::common::datadog::test_util::LiveConfiguration;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;
    type TestRequest = Request<()>;
    type TestResponse = Result<Response<()>, BoxError>;

    const RUN_PATH: &str = "/my/little/run_path";

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

    fn would_retry(policy: &mut SecretsHttpRetryPolicy, mut response: TestResponse) -> bool {
        let mut request = test_request();
        Policy::<TestRequest, Response<()>, BoxError>::retry(policy, &mut request, &mut response).is_some()
    }

    fn test_retry_config() -> RetryConfiguration {
        // Use small backoffs so that any returned `Sleep` futures are cheap; we never await them, but build them.
        let forwarder = shared::Forwarder {
            backoff_base: 0.001,
            backoff_max: 0.01,
            backoff_factor: 2.0,
            ..Default::default()
        };

        RetryConfiguration::from_configuration(&forwarder, None)
    }

    #[test]
    fn storage_path_is_derived_from_run_path_when_not_configured() {
        let retry_config =
            RetryConfiguration::from_configuration(&shared::Forwarder::default(), Some(Path::new(RUN_PATH)));

        assert_eq!(Path::new(RUN_PATH).join(RETRY_TXN_DIR), retry_config.storage_path());
    }

    #[test]
    fn a_configured_storage_path_wins_over_run_path() {
        const FORWARDER_STORAGE_PATH: &str = "/custom/path/to/storage";

        let forwarder = shared::Forwarder {
            storage_path: PathBuf::from(FORWARDER_STORAGE_PATH),
            ..Default::default()
        };
        let retry_config = RetryConfiguration::from_configuration(&forwarder, Some(Path::new(RUN_PATH)));

        assert_eq!(PathBuf::from(FORWARDER_STORAGE_PATH), retry_config.storage_path());
    }

    #[test]
    fn there_is_no_storage_path_without_a_run_path() {
        // With neither setting, no valid path can be constructed, so disk persistence has nowhere to go.
        let retry_config = RetryConfiguration::from_configuration(&shared::Forwarder::default(), None);

        assert_eq!(PathBuf::new(), retry_config.storage_path());
    }

    #[test]
    fn queue_max_size_bytes_carries_the_resolved_size() {
        // Which of the two retry-queue settings applies is resolved by the configuration layer; the
        // forwarder stores only the outcome.
        let forwarder = shared::Forwarder {
            retry_queue_payloads_max_size: ConfigValue::defaulted(15 * 1024 * 1024),
            retry_queue_max_size: ConfigValue::explicit(1024),
            ..Default::default()
        };
        let retry_config = RetryConfiguration::from_configuration(&forwarder, None);

        assert_eq!(1024, retry_config.queue_max_size_bytes());
    }

    #[test]
    fn capacity_time_interval_secs_is_clamped_to_the_bucket_size() {
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
            let retry_config = RetryConfiguration::from_configuration(&forwarder, None);

            assert_eq!(expected, retry_config.capacity_time_interval_secs(), "{configured}");
        }
    }

    #[test]
    fn policy_without_secrets_management_configured_does_not_retry_403() {
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(&Secrets::default()));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn policy_with_secrets_management_retries_403() {
        for secrets in [
            Secrets {
                backend_command: Some("/bin/true".to_string()),
                refresh_on_api_key_failure_interval: 0,
            },
            Secrets {
                backend_command: None,
                refresh_on_api_key_failure_interval: 5,
            },
        ] {
            let retry_config = test_retry_config();
            let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(&secrets));

            assert!(
                would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)),
                "{secrets:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_secrets_gate_does_not_affect_other_status_codes() {
        let secrets = Secrets {
            backend_command: Some("/bin/true".to_string()),
            refresh_on_api_key_failure_interval: 0,
        };
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(&secrets));

        assert!(!would_retry(&mut policy, ok_response(StatusCode::OK)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::BAD_REQUEST)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::UNAUTHORIZED)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::PAYLOAD_TOO_LARGE)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::TOO_MANY_REQUESTS)));
    }

    #[tokio::test]
    async fn the_403_gate_follows_a_secrets_configuration_change() {
        let live_config = LiveConfiguration::new(SalukiConfiguration::default());
        let refresher = SecretsGateRefresher::new(live_config.live(|config| &config.shared.secrets));
        let gate = refresher.gate.clone();
        refresher.spawn();

        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(gate.clone());

        // Before secrets management is configured, a 403 must not be retried.
        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        // The same policy instance reads the gate on every response, so turning secrets management on
        // changes the gate without rebuilding the service.
        live_config.store(configuration_with(Secrets {
            backend_command: Some("/bin/true".to_string()),
            refresh_on_api_key_failure_interval: 0,
        }));
        await_gate(&gate, true).await;
        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        live_config.store(SalukiConfiguration::default());
        await_gate(&gate, false).await;
        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    /// Waits for the refresher to bring `gate` to `expected`, failing if it never does.
    async fn await_gate(gate: &SecretsGate, expected: bool) {
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            while gate.is_open() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await;

        assert!(observed.is_ok(), "the gate should report {expected}");
    }

    /// Returns a configuration carrying `secrets` and nothing else.
    fn configuration_with(secrets: Secrets) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.secrets = secrets;

        config
    }
}
