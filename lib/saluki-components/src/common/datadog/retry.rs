use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use http::{Response, StatusCode};
use saluki_common::sync::live::Live;
use saluki_core::runtime::{self, ShutdownStrategy};
use saluki_io::net::util::retry::{
    ExponentialBackoff, RetryCauseTelemetry, RetryClassifier, RollingExponentialBackoffRetryPolicy,
    StandardHttpClassifier, StandardHttpRetryLifecycle,
};
use tracing::debug;

const RETRY_TXN_DIR: &str = "transactions_to_retry";
const RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS: u64 = 10;

pub(crate) type SecretsHttpRetryPolicy<B = ()> =
    RollingExponentialBackoffRetryPolicy<SecretsHttpClassifier<B>, StandardHttpRetryLifecycle>;

/// Whether secret resolution might replace a rejected API key.
///
/// The retry classifier reads this on every `403 Forbidden` response, and a [`SecretsGateRefresher`]
/// writes it when configuration changes. Cloning shares the gate, so every endpoint's classifier
/// follows the same setting.
#[derive(Clone)]
pub(crate) struct SecretsGate {
    in_use: Arc<AtomicBool>,
}

impl SecretsGate {
    /// Creates a gate reporting `secrets_in_use`, which nothing updates.
    pub(crate) fn new_fixed(secrets_in_use: bool) -> Self {
        Self {
            in_use: Arc::new(AtomicBool::new(secrets_in_use)),
        }
    }

    fn store(&self, secrets_in_use: bool) {
        self.in_use.store(secrets_in_use, Ordering::Relaxed);
    }

    fn is_open(&self) -> bool {
        self.in_use.load(Ordering::Relaxed)
    }
}

/// Updates a [`SecretsGate`] when configuration changes.
pub(crate) struct SecretsGateRefresher {
    secrets_in_use: Live<bool>,

    /// The gate the classifiers read. Clone it before spawning.
    pub(crate) gate: SecretsGate,
}

impl SecretsGateRefresher {
    /// Creates a refresher whose gate reports what `secrets_in_use` holds now.
    ///
    /// Seeding here rather than in the task closes the same gap `ApiKeyRefresher` closes: a configuration
    /// store between view creation and the first `changed` would otherwise leave the gate reporting the
    /// older setting.
    pub(crate) fn new(secrets_in_use: Live<bool>) -> Self {
        let gate = SecretsGate::new_fixed(*secrets_in_use);

        Self { secrets_in_use, gate }
    }

    /// Spawns the supervised child that follows the view.
    ///
    /// The child follows its view forever and has no terminal condition, so it is spawned as a
    /// [`Brutal`][ShutdownStrategy::Brutal] child: waiting for it at shutdown would hold the drain open until the
    /// supervisor's budget elapsed, and there is nothing in flight to preserve.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a supervision tree.
    pub(crate) fn spawn(self) {
        let Self { secrets_in_use, gate } = self;

        runtime::worker("secrets_gate_refresher", follow_secrets(secrets_in_use, gate))
            .with_shutdown_strategy(ShutdownStrategy::Brutal)
            .spawn();
    }
}

/// Stores what `secrets_in_use` holds into `gate` for as long as the view keeps reporting changes.
async fn follow_secrets(mut secrets_in_use: Live<bool>, gate: SecretsGate) {
    loop {
        let in_use = secrets_in_use.changed().await;
        gate.store(in_use);
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
    fn should_retry(&self, response: &Result<Response<B>, Error>) -> bool {
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
    pub backoff_factor: f64,

    /// The base growth rate of the backoff duration when retrying requests, in seconds.
    pub backoff_base: f64,

    /// The upper bound of the backoff duration when retrying requests, in seconds.
    pub backoff_max: f64,

    /// The amount to decrease the error count by when a request is successful.
    ///
    /// This essentially controls how quickly we forget about the number of previous errors when calculating the next
    /// backoff duration for a request that must be retried.
    pub recovery_error_decrease_factor: u32,

    /// Whether or not a successful request should completely reset the error count.
    pub recovery_reset: bool,

    /// The maximum in-memory size of the retry queue, in bytes.
    pub queue_max_size_bytes: u64,

    /// The maximum size of the retry queue on disk, in bytes.
    ///
    /// A value of `0` disables disk persistence.
    pub storage_max_size_bytes: u64,

    /// The ratio of in-memory retry queue bytes to flush to disk when the queue is full.
    ///
    /// When disk persistence is enabled and the in-memory retry queue does not have enough room for a new transaction,
    /// this controls how much in-memory data ADP moves to disk. For example, `0.5` moves at least half of the configured
    /// in-memory retry queue size to disk during each overflow. If set to `0`, ADP moves only enough old transactions to
    /// disk to make room for the new transaction.
    pub flush_to_disk_mem_ratio: f64,

    /// The configured path to the directory where the retry queue will be stored on disk.
    ///
    /// If this has no parent, such as when it is empty, the path is derived from `run_path` instead.
    pub storage_path: PathBuf,

    /// The base directory for runtime-state files, used to derive the storage path when none is configured.
    ///
    /// If neither is set, there is no storage path, and disk persistence has nowhere to write.
    pub run_path: Option<PathBuf>,

    /// The maximum disk usage ratio for storing transactions on disk.
    ///
    /// `0.8` means the Agent can store transactions on disk until `forwarder_storage_max_size_in_bytes`
    /// is reached or when the disk mount for `forwarder_storage_path` exceeds 80% of the disk capacity,
    /// whichever is lower.
    pub storage_max_disk_ratio: f64,

    /// Maximum age in days for retry files on disk before they are deleted at startup.
    ///
    /// When disk persistence is enabled, ADP removes any `retry-*.json` files in the
    /// per-queue subdirectory of the storage path that are older than this many days
    /// each time it starts. This prevents unbounded disk growth from stale retry data left
    /// behind after long outages.
    pub outdated_file_in_days: u32,

    /// The time window used to estimate retry queue capacity, in seconds.
    ///
    /// ADP records incoming transaction payload bytes over this window and uses that rate to estimate how many seconds
    /// of data the retry queue can buffer. Values below 10 seconds are clamped to 10 seconds, matching the fixed retry
    /// queue capacity bucket size.
    pub capacity_time_interval_secs: u64,
}

impl RetryConfiguration {
    /// Returns the path to the directory where the retry queue will be stored on disk.
    ///
    /// A configured storage path is used as-is. Otherwise the path is derived from `run_path`, matching the Datadog
    /// Agent's own layout. If neither is configured, the path is empty.
    pub(crate) fn resolved_storage_path(&self) -> PathBuf {
        resolve_storage_path(&self.storage_path, self.run_path.as_deref())
    }

    /// Returns the time window used to estimate retry queue capacity, in seconds, clamped to the bucket size.
    pub(crate) fn effective_capacity_time_interval_secs(&self) -> u64 {
        self.capacity_time_interval_secs
            .max(RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS)
    }

    /// Creates a new HTTP retry policy based on the forwarder configuration.
    ///
    /// A `403 Forbidden` response is retriable only when `secrets` reports that secret resolution might replace the
    /// rejected API key. The gate is updated as configuration changes, so turning secrets management on or off while the
    /// process runs changes the gate without rebuilding the service. A gate reporting nothing configured leaves a 403
    /// non-retriable, which is the default behavior.
    ///
    /// Retries are counted by cause through `retry_causes`, which is scoped to the domain the policy sends to.
    pub(crate) fn to_default_http_retry_policy<B: 'static>(
        &self, secrets: SecretsGate, retry_causes: RetryCauseTelemetry,
    ) -> SecretsHttpRetryPolicy<B> {
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
            .with_retry_lifecycle(StandardHttpRetryLifecycle::new().with_telemetry(retry_causes))
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
    use http::{Request, Response};
    use metrics::{Key, Label};
    use saluki_core::runtime::test_util::TestSupervisor;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};
    use tower::retry::Policy;

    use super::*;
    use crate::common::datadog::test_util::{retry_configuration, LiveConfiguration, LiveRoot};

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
        RetryConfiguration {
            backoff_base: 0.001,
            backoff_max: 0.01,
            backoff_factor: 2.0,
            ..retry_configuration()
        }
    }

    fn test_retry_causes() -> RetryCauseTelemetry {
        RetryCauseTelemetry::from_builder(&MetricsBuilder::default(), "http://localhost")
    }

    #[test]
    fn storage_path_is_derived_from_run_path_when_not_configured() {
        let retry_config = RetryConfiguration {
            storage_path: PathBuf::new(),
            run_path: Some(PathBuf::from(RUN_PATH)),
            ..retry_configuration()
        };

        assert_eq!(
            Path::new(RUN_PATH).join(RETRY_TXN_DIR),
            retry_config.resolved_storage_path()
        );
    }

    #[test]
    fn a_configured_storage_path_wins_over_run_path() {
        const FORWARDER_STORAGE_PATH: &str = "/custom/path/to/storage";

        let retry_config = RetryConfiguration {
            storage_path: PathBuf::from(FORWARDER_STORAGE_PATH),
            run_path: Some(PathBuf::from(RUN_PATH)),
            ..retry_configuration()
        };

        assert_eq!(
            PathBuf::from(FORWARDER_STORAGE_PATH),
            retry_config.resolved_storage_path()
        );
    }

    #[test]
    fn there_is_no_storage_path_without_a_run_path() {
        // With neither setting, no valid path can be constructed, so disk persistence has nowhere to go.
        let retry_config = RetryConfiguration {
            storage_path: PathBuf::new(),
            run_path: None,
            ..retry_configuration()
        };

        assert_eq!(PathBuf::new(), retry_config.resolved_storage_path());
    }

    #[test]
    fn capacity_time_interval_secs_is_clamped_to_the_bucket_size() {
        let cases = [
            (900, 900),
            (60, 60),
            (1, RETRY_QUEUE_CAPACITY_MIN_HISTORY_DURATION_SECS),
        ];

        for (configured, expected) in cases {
            let retry_config = RetryConfiguration {
                capacity_time_interval_secs: configured,
                ..retry_configuration()
            };

            assert_eq!(
                expected,
                retry_config.effective_capacity_time_interval_secs(),
                "{configured}"
            );
        }
    }

    #[test]
    fn policy_without_secrets_management_configured_does_not_retry_403() {
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(false), test_retry_causes());

        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn retry_policy_counts_the_cause_when_it_retries() {
        // Exercise telemetry through the policy used by the forwarder, rather than calling the lifecycle hook directly.
        let recorder = TestRecorder::default();
        let builder = MetricsBuilder::default();
        let retry_causes = RetryCauseTelemetry::from_builder(&builder, "https://example.com");
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(false), retry_causes);

        let retried = metrics::with_local_recorder(&recorder, || {
            would_retry(&mut policy, ok_response(StatusCode::SERVICE_UNAVAILABLE))
        });

        assert!(retried);
        let key = Key::from_parts(
            "network_http_requests_retry_causes_total",
            vec![
                Label::new("domain", "https://example.com"),
                Label::new("cause", "http_status"),
            ],
        );
        assert_eq!(recorder.counter(key), Some(1));
    }

    #[tokio::test]
    async fn policy_with_secrets_management_retries_403() {
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(true), test_retry_causes());

        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));
    }

    #[tokio::test]
    async fn the_secrets_gate_does_not_affect_other_status_codes() {
        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(SecretsGate::new_fixed(true), test_retry_causes());

        assert!(!would_retry(&mut policy, ok_response(StatusCode::OK)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::BAD_REQUEST)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::UNAUTHORIZED)));
        assert!(!would_retry(&mut policy, ok_response(StatusCode::PAYLOAD_TOO_LARGE)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(would_retry(&mut policy, ok_response(StatusCode::TOO_MANY_REQUESTS)));
    }

    #[tokio::test]
    async fn the_403_gate_follows_a_secrets_configuration_change() {
        let mut live_config = LiveConfiguration::new(LiveRoot::default());
        let refresher = SecretsGateRefresher::new(live_config.source.live(|root| &root.secrets_in_use));
        let gate = refresher.gate.clone();

        // The refresher spawns on the ambient supervisor, and the supervisor is what keeps it running for the rest of
        // the test.
        let supervisor = TestSupervisor::start("test_forwarder").await;
        supervisor.scope(async move { refresher.spawn() }).await;

        let retry_config = test_retry_config();
        let mut policy = retry_config.to_default_http_retry_policy(gate.clone(), test_retry_causes());

        // Before secrets management is configured, a 403 must not be retried.
        assert!(!would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        // The same policy instance reads the gate on every response, so turning secrets management on
        // changes the gate without rebuilding the service.
        live_config.publisher.publish(LiveRoot {
            secrets_in_use: true,
            ..LiveRoot::default()
        });
        await_gate(&gate, true).await;
        assert!(would_retry(&mut policy, ok_response(StatusCode::FORBIDDEN)));

        live_config.publisher.publish(LiveRoot::default());
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
}
