use std::{collections::HashSet, future, sync::LazyLock, time::Duration};

use bytes::Bytes;
use http::{Request, StatusCode, Uri};
use http_body_util::Empty;
use regex::Regex;
use saluki_core::diagnostic::{DiagnosticDetails, DiagnosticEvent, DiagnosticsEmitter};
use saluki_core::runtime::{self, ShutdownStrategy};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::client::http::HttpClient;
use tokio::{
    select,
    sync::{mpsc, watch},
    time::{self, MissedTickBehavior},
};
use tracing::{debug, warn};
use url::Url;

use super::{
    api_key::ApiKeyChanges,
    delivery::{RecipientOrigin, RecipientProtocol, RecipientSpec},
    endpoints::ResolvedEndpoint,
};

const VALIDATE_PATH: &str = "/api/v1/validate";
// TODO: Move the shared Datadog fake API key constant to `datadog-agent-commons`.
const FAKE_API_KEY: &str = "00000000000000000000000000000000";

static DATADOG_API_DOMAIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([a-z]{2,}\d{1,2}\.)?(datadoghq\.[a-z]+|ddog-gov\.com)\.?$").unwrap());

/// Readiness decision produced by API key validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValidationReadiness {
    /// At least one key is valid, or validation could not prove every key invalid.
    Ready,
    /// Validation proved that every known key is invalid.
    NotReady,
}

/// The endpoints whose API keys validation checks: a forwarder's startup endpoint set, or the native Datadog intake
/// recipients of a delivery host's table, which follow the table ([`RecipientValidationTargets`]).
pub(crate) struct ValidationTargets {
    endpoints: watch::Receiver<Vec<ResolvedEndpoint>>,

    /// Whether a set without endpoints is reported with the native warning too: a forwarder's is, while a delivery
    /// host without native intake recipients, such as an idle failover host, is ready without one.
    warn_when_empty: bool,
}

impl ValidationTargets {
    /// Returns a forwarder's startup endpoint set, which does not change.
    ///
    /// A set with no usable key is reported ready, with a warning.
    pub(crate) fn startup(endpoints: Vec<ResolvedEndpoint>) -> Self {
        // Nothing changes the set, so the sender goes at once, and the receiver keeps the set.
        let (_, endpoints) = watch::channel(endpoints);

        Self {
            endpoints,
            warn_when_empty: true,
        }
    }

    /// Returns the endpoints to validate now, which [`changed`][Self::changed] then waits for a change of: a set
    /// published before validation started is validated once.
    fn endpoints(&mut self) -> Vec<ResolvedEndpoint> {
        self.endpoints.borrow_and_update().clone()
    }

    /// Returns whether validating `endpoints` warns when none of them has a usable key: always for a forwarder's set,
    /// and for a delivery host's once it holds a native intake recipient, whose key may be blank, as a forwarder's
    /// primary key may.
    fn warns_without_usable_keys(&self, endpoints: &[ResolvedEndpoint]) -> bool {
        self.warn_when_empty || !endpoints.is_empty()
    }

    /// Waits until the endpoints to validate change, or forever when they cannot.
    async fn changed(&mut self) {
        if self.endpoints.changed().await.is_err() {
            future::pending().await
        }
    }
}

/// Publishes a delivery host's validation targets: the native Datadog intake recipients of its table, as it stands.
///
/// A table with no such recipient, as an idle failover host's, is ready, and nothing is logged. A table whose such
/// recipients have no usable key, such as a primary endpoint whose key is blank, is ready with a forwarder's warning.
pub(crate) struct RecipientValidationTargets {
    targets: watch::Sender<Vec<ResolvedEndpoint>>,
}

impl RecipientValidationTargets {
    /// Creates a publisher with no targets, and the validation targets it feeds.
    pub(crate) fn new() -> (Self, ValidationTargets) {
        let (targets, endpoints) = watch::channel(Vec::new());
        let validation_targets = ValidationTargets {
            endpoints,
            warn_when_empty: false,
        };

        (Self { targets }, validation_targets)
    }

    /// Returns whether validation checks a recipient's credential: only a native recipient of the Datadog intake's.
    ///
    /// A declared recipient's endpoint would otherwise receive its credential in a validation request, and its
    /// credential never affects readiness; the Cluster Agent has nothing to validate a key against.
    pub(crate) fn includes(spec: &RecipientSpec) -> bool {
        matches!(spec.origin, RecipientOrigin::Native(_))
            && matches!(spec.protocol, RecipientProtocol::DatadogIntake { .. })
    }

    /// Publishes the recipients of the host's table, each with its endpoint and credential cell, of which only those
    /// [`includes`][Self::includes] says are validated.
    ///
    /// Validation runs again when the targets, or their keys, differ from those published before.
    pub(crate) fn publish<'a>(&self, recipients: impl IntoIterator<Item = (&'a RecipientSpec, &'a ResolvedEndpoint)>) {
        let targets = recipients
            .into_iter()
            .filter(|(spec, _)| Self::includes(spec))
            .map(|(_, endpoint)| endpoint.clone())
            .collect::<Vec<_>>();
        self.targets.send_if_modified(|current| {
            let identify = |endpoint: &ResolvedEndpoint| (endpoint.endpoint().clone(), endpoint.api_key());
            let changed = !current.iter().map(identify).eq(targets.iter().map(identify));
            *current = targets;
            changed
        });
    }
}

/// API key validation for a forwarder's endpoints, or a delivery host's recipients.
pub(crate) struct ApiKeyValidator {
    targets: ValidationTargets,
    client: HttpClient,
    api_key_changes: Option<ApiKeyChanges>,
    interval: Duration,
    emitter: DiagnosticsEmitter,
}

impl ApiKeyValidator {
    /// Creates API key validation for the given targets.
    ///
    /// Validation re-runs whenever the targets change, whenever `api_key_changes` reports a new key, and on `interval`
    /// regardless. A forwarder whose keys configuration cannot change supplies no handle and is validated on the
    /// interval alone.
    ///
    /// `emitter` is used to surface a diagnostic event whenever validation determines that every configured API key is
    /// invalid.
    pub(crate) fn new(
        targets: ValidationTargets, client: HttpClient, api_key_changes: Option<ApiKeyChanges>, interval: Duration,
        emitter: DiagnosticsEmitter,
    ) -> Self {
        Self {
            targets,
            client,
            api_key_changes,
            interval,
            emitter,
        }
    }

    /// Spawns the API key validation child and returns a readiness handle.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a supervision tree.
    pub(crate) fn spawn(self) -> ApiKeyValidationHandle {
        let (readiness_tx, readiness_rx) = mpsc::channel(1);
        spawn_validation_task(
            self.targets,
            self.client,
            self.api_key_changes,
            self.interval,
            readiness_tx,
            self.emitter,
        );

        ApiKeyValidationHandle {
            readiness_rx: Some(readiness_rx),
        }
    }
}

/// Handle for API key validation readiness updates.
///
/// Dropping the handle closes the readiness channel, which is the validation child's terminal condition. It is
/// spawned as a [`Brutal`][ShutdownStrategy::Brutal] child regardless, because it only notices the closure on its
/// next validation pass -- up to a whole interval later -- and a probe has nothing in flight worth preserving.
pub(crate) struct ApiKeyValidationHandle {
    readiness_rx: Option<mpsc::Receiver<ValidationReadiness>>,
}

impl ApiKeyValidationHandle {
    /// Waits until API key validation produces a readiness update.
    pub(crate) async fn wait_for_change(&mut self) -> ValidationReadiness {
        let Some(rx) = &mut self.readiness_rx else {
            return future::pending().await;
        };

        match rx.recv().await {
            Some(readiness) => readiness,
            None => {
                self.readiness_rx = None;
                debug!("Datadog API key validation task stopped.");
                future::pending().await
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ValidationTargetKey {
    validation_base_url: String,
    api_key: String,
}

#[derive(Clone, Debug)]
struct ValidationTarget {
    endpoint: url::Url,
    validation_base_url: url::Url,
    api_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyValidationResult {
    Valid,
    Invalid,
    Error,
}

fn spawn_validation_task(
    targets: ValidationTargets, client: HttpClient, api_key_changes: Option<ApiKeyChanges>, interval: Duration,
    readiness_tx: mpsc::Sender<ValidationReadiness>, emitter: DiagnosticsEmitter,
) {
    runtime::worker(
        "api_key_validation",
        run_validation_loop(targets, client, api_key_changes, interval, readiness_tx, emitter),
    )
    .with_shutdown_strategy(ShutdownStrategy::Brutal)
    .spawn();
}

async fn run_validation_loop(
    mut targets: ValidationTargets, mut client: HttpClient, mut api_key_changes: Option<ApiKeyChanges>,
    interval: Duration, readiness_tx: mpsc::Sender<ValidationReadiness>, emitter: DiagnosticsEmitter,
) {
    if !validate_and_send_readiness(&mut targets, &mut client, &readiness_tx, &emitter).await {
        return;
    }

    let mut interval = time::interval(interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The startup validation above is the immediate tick.
    interval.tick().await;

    loop {
        select! {
            _ = interval.tick() => {},
            _ = wait_for_api_key_change(&mut api_key_changes) => {},
            _ = targets.changed() => {},
        }
        if !validate_and_send_readiness(&mut targets, &mut client, &readiness_tx, &emitter).await {
            return;
        }
    }
}

/// Waits for an endpoint to hold a new API key, or forever when no key can change.
async fn wait_for_api_key_change(api_key_changes: &mut Option<ApiKeyChanges>) {
    match api_key_changes {
        Some(api_key_changes) => api_key_changes.changed().await,
        None => future::pending().await,
    }
}

async fn validate_and_send_readiness(
    targets: &mut ValidationTargets, client: &mut HttpClient, readiness_tx: &mpsc::Sender<ValidationReadiness>,
    emitter: &DiagnosticsEmitter,
) -> bool {
    let endpoints = targets.endpoints();
    let warn_when_empty = targets.warns_without_usable_keys(&endpoints);
    let targets = collect_validation_targets(&endpoints);
    let readiness = validate_targets(client, &targets, warn_when_empty).await;

    if readiness == ValidationReadiness::NotReady {
        emitter.emit(DiagnosticEvent::new(
            "All configured Datadog API key(s) were rejected as invalid.",
            DiagnosticDetails::InvalidApiKey,
        ));
    }

    if readiness_tx.send(readiness).await.is_err() {
        debug!("API key validation readiness receiver dropped; stopping validation task.");
        return false;
    }

    true
}

async fn validate_targets(
    client: &mut HttpClient, targets: &[ValidationTarget], warn_when_empty: bool,
) -> ValidationReadiness {
    if targets.is_empty() {
        if warn_when_empty {
            warn!("No Datadog API keys are available for validation; marking forwarder ready.");
        }
        return ValidationReadiness::Ready;
    }

    let mut saw_error = false;

    for target in targets {
        match validate_target(client, target).await {
            KeyValidationResult::Valid => return ValidationReadiness::Ready,
            KeyValidationResult::Invalid => {}
            KeyValidationResult::Error => saw_error = true,
        }
    }

    if saw_error {
        ValidationReadiness::Ready
    } else {
        ValidationReadiness::NotReady
    }
}

fn collect_validation_targets(endpoints: &[ResolvedEndpoint]) -> Vec<ValidationTarget> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    for endpoint in endpoints {
        // `api_key()` returns the key the request path is using, so validation follows a rotation
        // without rebuilding the endpoint set.
        let api_key = endpoint.api_key().to_string();
        if api_key.is_empty() {
            continue;
        }

        let validation_base_url = validation_base_url(endpoint.endpoint());
        let key = ValidationTargetKey {
            validation_base_url: validation_base_url.to_string(),
            api_key: api_key.clone(),
        };

        if seen.insert(key) {
            targets.push(ValidationTarget {
                endpoint: endpoint.endpoint().clone(),
                validation_base_url,
                api_key,
            });
        }
    }

    targets
}

async fn validate_target(client: &mut HttpClient, target: &ValidationTarget) -> KeyValidationResult {
    if target.api_key == FAKE_API_KEY {
        debug!(endpoint = %target.endpoint, "Treating fake Datadog API key as valid.");
        return KeyValidationResult::Valid;
    }

    let request = match build_validation_request(target) {
        Ok(request) => request,
        Err(e) => {
            debug!(endpoint = %target.endpoint, error = %e, "Could not build Datadog API key validation request.");
            return KeyValidationResult::Error;
        }
    };

    match client.send(request).await {
        Ok(response) => match response.status() {
            StatusCode::OK => {
                debug!(endpoint = %target.endpoint, validation_endpoint = %target.validation_base_url, "Datadog API key is valid.");
                KeyValidationResult::Valid
            }
            StatusCode::FORBIDDEN => {
                warn!(endpoint = %target.endpoint, validation_endpoint = %target.validation_base_url, "Datadog API key is invalid.");
                KeyValidationResult::Invalid
            }
            status => {
                debug!(
                    endpoint = %target.endpoint,
                    validation_endpoint = %target.validation_base_url,
                    %status,
                    "Datadog API key validation returned an unexpected status."
                );
                KeyValidationResult::Error
            }
        },
        Err(e) => {
            debug!(
                endpoint = %target.endpoint,
                validation_endpoint = %target.validation_base_url,
                error = %e,
                "Datadog API key validation request failed."
            );
            KeyValidationResult::Error
        }
    }
}

fn build_validation_request(target: &ValidationTarget) -> Result<Request<Empty<Bytes>>, GenericError> {
    let mut url = target.validation_base_url.clone();
    url.set_path(VALIDATE_PATH);
    url.set_query(None);
    url.query_pairs_mut().append_pair("api_key", &target.api_key);

    let uri = url
        .as_str()
        .parse::<Uri>()
        .map_err(|e| generic_error!("Failed to parse validation URL as URI: {}", e))?;

    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Empty::<Bytes>::new())
        .map_err(|e| generic_error!("Failed to build validation request: {}", e))
}

fn validation_base_url(endpoint: &Url) -> Url {
    let Some(host) = endpoint.host_str() else {
        return endpoint.clone();
    };

    if let Some(matched) = DATADOG_API_DOMAIN_RE.find(host) {
        let site = matched.as_str().trim_end_matches('.');
        let mut api_endpoint = endpoint.clone();
        let _ = api_endpoint.set_scheme("https");
        let _ = api_endpoint.set_host(Some(&format!("api.{site}")));
        api_endpoint.set_path("");
        api_endpoint.set_query(None);
        return api_endpoint;
    }

    endpoint.clone()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use agent_data_plane_config::{
        shared::{AltMetricsIntake, SharedConfiguration},
        ConfigValue, SalukiConfiguration,
    };
    use axum::{extract::RawQuery, routing::get, Router};
    use saluki_core::components::test_util::TestComponentSupervisor;
    use saluki_tls::initialize_default_crypto_provider;
    use tokio::net::TcpListener;

    use super::*;
    use crate::common::datadog::{
        api_key::ApiKeyRefresher,
        config::ForwarderConfiguration,
        delivery::{IntakeAuthorities, PayloadClasses, RecipientPolicy, V3Acceptance},
        endpoints::{MetricsRecipient, MetricsRecipientRoute, RoutableEndpoint},
        test_util::{shared_configuration, LiveConfiguration},
    };

    /// Returns shared configuration whose primary endpoint is `dd_url`, with `primary-key` as its API key.
    fn shared_configuration_for(dd_url: &str) -> SharedConfiguration {
        let mut shared = shared_configuration();
        shared.endpoints.api_key = "primary-key".to_string();
        shared.endpoints.dd_url = ConfigValue::explicit(dd_url.to_string());
        shared
    }

    /// Returns the resolved endpoints of a forwarder's startup endpoint set.
    fn resolved(endpoints: &[RoutableEndpoint]) -> Vec<ResolvedEndpoint> {
        endpoints.iter().map(|routable| routable.endpoint().clone()).collect()
    }

    /// Returns a forwarder's startup endpoint set as validation targets.
    fn startup_targets(endpoints: &[RoutableEndpoint]) -> ValidationTargets {
        ValidationTargets::startup(resolved(endpoints))
    }

    /// Returns the specification of a recipient of the given origin and protocol.
    fn recipient_spec(origin: RecipientOrigin, protocol: RecipientProtocol) -> RecipientSpec {
        RecipientSpec {
            id: "dd_out/recipient".into(),
            endpoint: "http://127.0.0.1:1".to_string(),
            credential: "credentials/recipient".into(),
            classes: PayloadClasses::All,
            protocol,
            policy: RecipientPolicy::default(),
            origin,
        }
    }

    fn native_origin() -> RecipientOrigin {
        RecipientOrigin::Native(MetricsRecipient {
            route: MetricsRecipientRoute::Primary,
            configured_endpoint: "http://127.0.0.1:1".to_string(),
            api_key_index: None,
            classes: PayloadClasses::All,
            protocol: datadog_intake(),
        })
    }

    fn datadog_intake() -> RecipientProtocol {
        RecipientProtocol::DatadogIntake {
            authorities: IntakeAuthorities::default(),
            allow_arbitrary_tags: false,
            v3: V3Acceptance::default(),
        }
    }

    #[test]
    fn only_native_datadog_intake_recipients_are_validated() {
        let declared = RecipientOrigin::Declared {
            name: "team_d".into(),
            credential: "team_d".into(),
        };
        let cases = [
            (native_origin(), datadog_intake(), true),
            (declared.clone(), datadog_intake(), false),
            (native_origin(), RecipientProtocol::ClusterAgent, false),
            (declared, RecipientProtocol::ClusterAgent, false),
        ];

        let (publisher, mut targets) = RecipientValidationTargets::new();
        for (origin, protocol, validated) in cases {
            let spec = recipient_spec(origin, protocol);
            assert_eq!(RecipientValidationTargets::includes(&spec), validated, "{spec:?}");

            let endpoint = ResolvedEndpoint::from_raw_endpoint("http://127.0.0.1:1", "key").expect("endpoint resolves");
            publisher.publish([(&spec, &endpoint)]);
            assert_eq!(targets.endpoints().len(), usize::from(validated), "{spec:?}");
        }
    }

    #[test]
    fn a_host_warns_that_no_key_is_usable_only_once_it_holds_a_native_intake_recipient() {
        // An idle host has nothing to validate, and nothing to warn about.
        let (publisher, mut targets) = RecipientValidationTargets::new();
        let endpoints = targets.endpoints();
        assert!(!targets.warns_without_usable_keys(&endpoints));

        // A primary endpoint whose key is blank, as when the key is not streamed yet: nothing can be validated, and the
        // host warns as a forwarder with a blank primary key does.
        let spec = recipient_spec(native_origin(), datadog_intake());
        let blank = ResolvedEndpoint::from_raw_endpoint("http://127.0.0.1:1", " ").expect("endpoint resolves");
        publisher.publish([(&spec, &blank)]);
        let endpoints = targets.endpoints();
        assert!(collect_validation_targets(&endpoints).is_empty());
        assert!(targets.warns_without_usable_keys(&endpoints));

        // A forwarder's startup set warns whenever no key is usable.
        assert!(ValidationTargets::startup(Vec::new()).warns_without_usable_keys(&[]));
    }

    #[tokio::test]
    async fn validation_follows_a_host_table_and_a_host_without_native_recipients_is_ready_without_requests() {
        let _ = initialize_default_crypto_provider();
        let requests = Arc::new(AtomicUsize::new(0));
        let url = start_counting_validation_server(StatusCode::FORBIDDEN, Arc::clone(&requests)).await;
        let emitter = DiagnosticsEmitter::from_dataspace(
            saluki_core::support::SubsystemIdentifier::from_segments(["test-forwarder"]),
            saluki_core::runtime::state::DataspaceRegistry::new(),
        );
        let (publisher, targets) = RecipientValidationTargets::new();
        let (readiness_tx, mut readiness_rx) = mpsc::channel(1);
        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        supervisor
            .scope(async move {
                // An interval no test can wait out, so only a change of the table can drive the next validation.
                spawn_validation_task(
                    targets,
                    test_client(Duration::from_secs(1)),
                    None,
                    Duration::from_secs(3600),
                    readiness_tx,
                    emitter,
                );
            })
            .await;

        // An idle host is ready, without validating anything.
        assert_eq!(await_readiness(&mut readiness_rx).await, ValidationReadiness::Ready);
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        // A native recipient whose key the intake rejects makes the host not ready, as a restart with it would.
        let native = recipient_spec(native_origin(), datadog_intake());
        let endpoint = ResolvedEndpoint::from_raw_endpoint(&url, "rejected-key").expect("endpoint resolves");
        publisher.publish([(&native, &endpoint)]);
        assert_eq!(await_readiness(&mut readiness_rx).await, ValidationReadiness::NotReady);
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        // Publishing the same table again validates nothing more.
        publisher.publish([(&native, &endpoint)]);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), readiness_rx.recv())
                .await
                .is_err(),
            "an unchanged table is not validated again"
        );

        // A declared recipient with the same rejected key never affects readiness, and its endpoint never receives
        // its credential.
        let declared = recipient_spec(
            RecipientOrigin::Declared {
                name: "team_d".into(),
                credential: "team_d".into(),
            },
            datadog_intake(),
        );
        publisher.publish([(&declared, &endpoint)]);
        assert_eq!(await_readiness(&mut readiness_rx).await, ValidationReadiness::Ready);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    fn validation_url_for(raw_endpoint: &str) -> String {
        let endpoint = ResolvedEndpoint::from_raw_endpoint(raw_endpoint, "api-key").expect("endpoint should resolve");
        validation_base_url(endpoint.endpoint()).to_string()
    }

    #[test]
    fn validation_url_derivation() {
        let cases = [
            (
                "datadog default site",
                "https://app.datadoghq.com",
                "https://api.datadoghq.com/",
            ),
            (
                "datadog regional site",
                "https://app.us5.datadoghq.com",
                "https://api.us5.datadoghq.com/",
            ),
            (
                "datadog api validation always uses https",
                "http://app.datadoghq.com",
                "https://api.datadoghq.com/",
            ),
            ("custom endpoint", "http://127.0.0.1:12345", "http://127.0.0.1:12345/"),
        ];

        for (case_name, raw_endpoint, expected_url) in cases {
            assert_eq!(validation_url_for(raw_endpoint), expected_url, "{case_name}");
        }
    }

    #[tokio::test]
    async fn a_rotated_key_is_revalidated_before_the_next_interval() {
        use saluki_core::runtime::state::DataspaceRegistry;
        use saluki_core::support::SubsystemIdentifier;

        // Validation waits for the refresher to install a key rather than for the configuration update that
        // drove it, so it cannot publish readiness for a key the request path has stopped using.
        let _ = initialize_default_crypto_provider();

        let url = start_key_aware_validation_server("rotated-key").await;
        let endpoints = ForwarderConfiguration::from_configuration(&shared_configuration_for(&url))
            .build_routable_endpoints()
            .expect("endpoints should resolve");

        let mut live_config = SalukiConfiguration::default();
        live_config.shared.endpoints.api_key = "primary-key".to_string();
        let live = LiveConfiguration::new(live_config.clone());
        let refresher =
            ApiKeyRefresher::new(&endpoints, &live.api_keys()).expect("the endpoint should follow the live views");
        let api_key_changes = refresher.changes();

        let emitter = DiagnosticsEmitter::from_dataspace(
            SubsystemIdentifier::from_segments(["test-forwarder"]),
            DataspaceRegistry::new(),
        );
        let (readiness_tx, mut readiness_rx) = mpsc::channel(1);

        // Both the refresher and validation spawn on the ambient supervisor, and it is what keeps them running for the
        // rest of the test.
        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        supervisor
            .scope(async move {
                refresher.spawn();

                // An interval no test can wait out, so only a key change can drive the second validation.
                spawn_validation_task(
                    startup_targets(&endpoints),
                    test_client(Duration::from_secs(1)),
                    Some(api_key_changes),
                    Duration::from_secs(3600),
                    readiness_tx,
                    emitter,
                );
            })
            .await;

        assert_eq!(await_readiness(&mut readiness_rx).await, ValidationReadiness::NotReady);

        live_config.shared.endpoints.api_key = "rotated-key".to_string();
        live.store(live_config);

        assert_eq!(await_readiness(&mut readiness_rx).await, ValidationReadiness::Ready);
    }

    #[tokio::test]
    async fn fake_api_key_is_valid_without_network() {
        let _ = initialize_default_crypto_provider();
        let mut client = test_client(Duration::from_secs(1));
        let target = ValidationTarget {
            endpoint: Url::parse("http://127.0.0.1:1/").unwrap(),
            validation_base_url: Url::parse("http://127.0.0.1:1/").unwrap(),
            api_key: FAKE_API_KEY.to_string(),
        };

        assert_eq!(validate_target(&mut client, &target).await, KeyValidationResult::Valid);
    }

    #[tokio::test]
    async fn validation_classifies_response_statuses() {
        let _ = initialize_default_crypto_provider();
        let cases = [
            (StatusCode::OK, KeyValidationResult::Valid),
            (StatusCode::FORBIDDEN, KeyValidationResult::Invalid),
            (StatusCode::INTERNAL_SERVER_ERROR, KeyValidationResult::Error),
        ];

        for (status, expected_result) in cases {
            let url = start_validation_server(status).await;
            let mut client = test_client(Duration::from_secs(1));

            assert_eq!(
                validate_target(&mut client, &target_for(&url)).await,
                expected_result,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn validation_treats_transport_failure_as_error() {
        let _ = initialize_default_crypto_provider();
        let mut client = test_client(Duration::from_millis(50));

        assert_eq!(
            validate_target(&mut client, &target_for("http://127.0.0.1:1/")).await,
            KeyValidationResult::Error
        );
    }

    #[tokio::test]
    async fn validation_targets_include_primary_additional_and_opw() {
        let mut shared = shared_configuration_for("http://primary.example.com");
        shared.endpoints.additional_endpoints = HashMap::from([(
            "http://additional.example.com".to_string(),
            vec![
                "additional-key".to_string(),
                "additional-key".to_string(),
                String::new(),
            ],
        )]);
        shared.endpoints.opw_intake = AltMetricsIntake {
            enabled: true,
            url: "http://opw.example.com".to_string(),
            use_v3_series: false,
        };
        let forwarder_config = ForwarderConfiguration::from_configuration(&shared);
        let endpoints = forwarder_config
            .build_routable_endpoints()
            .expect("endpoints should resolve");

        let targets = collect_validation_targets(&resolved(&endpoints));
        let mut target_pairs = targets
            .into_iter()
            .map(|target| (target.endpoint.to_string(), target.api_key))
            .collect::<Vec<_>>();
        target_pairs.sort();

        assert_eq!(
            target_pairs,
            vec![
                (
                    "http://additional.example.com/".to_string(),
                    "additional-key".to_string()
                ),
                ("http://opw.example.com/".to_string(), "primary-key".to_string()),
                ("http://primary.example.com/".to_string(), "primary-key".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn validation_follows_rotated_keys_but_does_not_add_new_endpoints() {
        // Validation reads the same cells the request path does, so a rotated key reaches validation
        // without rebuilding the endpoint set.
        let url = "http://additional.example.com";
        let mut shared = shared_configuration_for("http://primary.example.com");
        shared.endpoints.additional_endpoints =
            HashMap::from([(url.to_string(), vec!["old-additional-key".to_string()])]);

        let mut live_config = SalukiConfiguration::default();
        live_config.shared.endpoints.api_key = "primary-key".to_string();
        live_config.shared.endpoints.additional_endpoints = shared.endpoints.additional_endpoints.clone();
        let live = LiveConfiguration::new(live_config.clone());

        let endpoints = ForwarderConfiguration::from_configuration(&shared)
            .build_routable_endpoints()
            .expect("endpoints should resolve");
        let refresher =
            ApiKeyRefresher::new(&endpoints, &live.api_keys()).expect("the endpoints should follow the live views");
        let supervisor = TestComponentSupervisor::start("test_forwarder").await;
        supervisor.scope(async move { refresher.spawn() }).await;

        // Rotate the key at the configured position, and add a URL no endpoint was built for.
        live_config.shared.endpoints.additional_endpoints = HashMap::from([
            (url.to_string(), vec!["new-additional-key".to_string()]),
            (
                "http://new.example.com".to_string(),
                vec!["ignored-new-domain-key".to_string()],
            ),
        ]);
        live.store(live_config);

        let targets = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let targets = collect_validation_targets(&resolved(&endpoints));
                if targets.iter().any(|target| target.api_key == "new-additional-key") {
                    return targets;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the rotated key should reach validation");

        assert!(
            !targets
                .iter()
                .any(|target| target.endpoint.as_str() == "http://new.example.com/"),
            "a URL no endpoint was built for is not validated"
        );
    }

    #[tokio::test]
    async fn readiness_is_not_ready_only_when_all_targets_are_confirmed_invalid() {
        let invalid_url = start_validation_server(StatusCode::FORBIDDEN).await;
        let error_url = start_validation_server(StatusCode::INTERNAL_SERVER_ERROR).await;
        let mut client = test_client(Duration::from_secs(1));

        assert_eq!(
            validate_targets(&mut client, &[target_for(&invalid_url)], true).await,
            ValidationReadiness::NotReady
        );
        assert_eq!(
            validate_targets(&mut client, &[target_for(&error_url)], true).await,
            ValidationReadiness::Ready
        );
    }

    #[tokio::test]
    async fn readiness_short_circuits_after_valid_target() {
        let valid_url = start_validation_server(StatusCode::OK).await;
        let later_requests = Arc::new(AtomicUsize::new(0));
        let later_url = start_counting_validation_server(StatusCode::FORBIDDEN, Arc::clone(&later_requests)).await;
        let mut client = test_client(Duration::from_secs(1));

        assert_eq!(
            validate_targets(&mut client, &[target_for(&valid_url), target_for(&later_url)], true).await,
            ValidationReadiness::Ready
        );
        assert_eq!(later_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn not_ready_emits_invalid_api_key_diagnostic() {
        use saluki_core::runtime::state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter};
        use saluki_core::support::SubsystemIdentifier;

        let _ = initialize_default_crypto_provider();

        // A validation server that rejects the key with a 403, so validation concludes it is invalid.
        let invalid_url = start_validation_server(StatusCode::FORBIDDEN).await;
        let forwarder_config = ForwarderConfiguration::from_configuration(&shared_configuration_for(&invalid_url));
        let endpoints = forwarder_config
            .build_routable_endpoints()
            .expect("endpoints should resolve");

        // Subscribe to diagnostic events on a dataspace, then build an emitter that publishes to that same dataspace.
        let dataspace = DataspaceRegistry::new();
        let mut events = dataspace.subscribe::<DiagnosticEvent>(IdentifierFilter::all());
        let emitter =
            DiagnosticsEmitter::from_dataspace(SubsystemIdentifier::from_segments(["test-forwarder"]), dataspace);

        let mut client = test_client(Duration::from_secs(1));
        let (readiness_tx, mut readiness_rx) = mpsc::channel(1);

        assert!(
            validate_and_send_readiness(&mut startup_targets(&endpoints), &mut client, &readiness_tx, &emitter).await
        );
        assert_eq!(readiness_rx.recv().await, Some(ValidationReadiness::NotReady));

        // The rejected key must have produced an `InvalidApiKey` diagnostic event.
        match events.recv().await {
            Some(DataspaceUpdate::Message(_, event)) => {
                assert_eq!(event.details(), &DiagnosticDetails::InvalidApiKey);
            }
            other => panic!("expected an InvalidApiKey diagnostic event, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ready_does_not_emit_diagnostic() {
        use saluki_core::runtime::state::{DataspaceRegistry, IdentifierFilter};
        use saluki_core::support::SubsystemIdentifier;

        let _ = initialize_default_crypto_provider();

        // A validation server that accepts the key, so validation concludes it is valid.
        let valid_url = start_validation_server(StatusCode::OK).await;
        let forwarder_config = ForwarderConfiguration::from_configuration(&shared_configuration_for(&valid_url));
        let endpoints = forwarder_config
            .build_routable_endpoints()
            .expect("endpoints should resolve");

        let dataspace = DataspaceRegistry::new();
        let mut events = dataspace.subscribe::<DiagnosticEvent>(IdentifierFilter::all());
        let emitter =
            DiagnosticsEmitter::from_dataspace(SubsystemIdentifier::from_segments(["test-forwarder"]), dataspace);

        let mut client = test_client(Duration::from_secs(1));
        let (readiness_tx, mut readiness_rx) = mpsc::channel(1);

        assert!(
            validate_and_send_readiness(&mut startup_targets(&endpoints), &mut client, &readiness_tx, &emitter).await
        );
        assert_eq!(readiness_rx.recv().await, Some(ValidationReadiness::Ready));

        // A valid key must not produce any diagnostic event.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "no diagnostic event should be emitted when the API key is valid"
        );
    }

    #[tokio::test]
    async fn readiness_is_ready_when_there_are_no_targets_to_validate() {
        // With no usable API keys, `validate_targets` short-circuits to `Ready` (and issues no requests)
        // so that a deployment without validatable keys does not wedge forwarder startup.
        let mut client = test_client(Duration::from_secs(1));

        assert_eq!(
            validate_targets(&mut client, &[], true).await,
            ValidationReadiness::Ready
        );
    }

    /// Returns the next readiness update, failing if none arrives.
    async fn await_readiness(rx: &mut mpsc::Receiver<ValidationReadiness>) -> ValidationReadiness {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("validation should publish readiness")
            .expect("the validation task should still be running")
    }

    fn target_for(base_url: &str) -> ValidationTarget {
        ValidationTarget {
            endpoint: Url::parse(base_url).unwrap(),
            validation_base_url: Url::parse(base_url).unwrap(),
            api_key: "api-key".to_string(),
        }
    }

    fn test_client(timeout: Duration) -> HttpClient {
        let _ = initialize_default_crypto_provider();

        HttpClient::builder()
            .with_request_timeout(timeout)
            .with_tls_config(|builder| builder.danger_accept_invalid_certs())
            .build()
            .expect("client should build")
    }

    async fn start_validation_server(status: StatusCode) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(VALIDATE_PATH, get(move || async move { status }));

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        format!("http://127.0.0.1:{port}/")
    }

    /// Starts a validation server that accepts only `valid_api_key`.
    async fn start_key_aware_validation_server(valid_api_key: &str) -> String {
        // Validation sends the key as the request's only query parameter.
        let valid_query = format!("api_key={valid_api_key}");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            VALIDATE_PATH,
            get(move |RawQuery(query): RawQuery| {
                let valid_query = valid_query.clone();
                async move {
                    if query.as_deref() == Some(valid_query.as_str()) {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    }
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        format!("http://127.0.0.1:{port}/")
    }

    async fn start_counting_validation_server(status: StatusCode, requests: Arc<AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            VALIDATE_PATH,
            get(move || {
                let requests = Arc::clone(&requests);
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    status
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        format!("http://127.0.0.1:{port}/")
    }
}
