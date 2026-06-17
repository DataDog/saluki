use std::{collections::HashSet, future, sync::LazyLock, time::Duration};

use bytes::Bytes;
use http::{Request, StatusCode, Uri};
use http_body_util::Empty;
use regex::Regex;
use saluki_common::task::spawn_traced_named;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::client::http::HttpClient;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use tracing::{debug, warn};
use url::Url;

use super::endpoints::{LiveForwarderConfig, RoutableEndpoint};

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

/// API key validation for the startup endpoint set.
///
/// # Missing
///
/// The previous implementation also subscribed to the raw configuration stream and re-validated
/// whenever `api_key`, `multi_region_failover.api_key`, or `additional_endpoints` changed. That
/// raw-map validation path is a configuration-system concern and has been removed in the
/// typed-config cutover; validation now runs only on the periodic interval. Each periodic pass still
/// refreshes endpoint API keys from the live forwarder configuration slice carried by the endpoints
/// themselves (see [`ResolvedEndpoint::api_key`][super::endpoints::ResolvedEndpoint::api_key]), so a
/// rotated key is picked up on the next tick.
pub(crate) struct ApiKeyValidator {
    endpoints: Vec<RoutableEndpoint>,
    client: HttpClient,
    interval: Duration,
}

impl ApiKeyValidator {
    /// Creates API key validation for the given startup endpoint set.
    ///
    /// The `_live_config` handle is retained in the signature for call-site symmetry with the
    /// forwarder, but periodic validation reads the latest keys through the endpoints' own handles,
    /// so it is not stored.
    pub(crate) fn new(
        endpoints: Vec<RoutableEndpoint>, client: HttpClient, _live_config: Option<LiveForwarderConfig>,
        interval: Duration,
    ) -> Self {
        Self {
            endpoints,
            client,
            interval,
        }
    }

    /// Spawns the API key validation task and returns a readiness handle.
    pub(crate) fn spawn(self) -> ApiKeyValidationHandle {
        let (readiness_tx, readiness_rx) = mpsc::channel(1);
        let task = spawn_validation_task(self.endpoints, self.client, self.interval, readiness_tx);

        ApiKeyValidationHandle {
            task,
            readiness_rx: Some(readiness_rx),
        }
    }
}

/// Handle for API key validation readiness updates.
pub(crate) struct ApiKeyValidationHandle {
    task: JoinHandle<()>,
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

    /// Stops the validation task.
    pub(crate) fn abort(&self) {
        self.task.abort();
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
    endpoints: Vec<RoutableEndpoint>, client: HttpClient, interval: Duration,
    readiness_tx: mpsc::Sender<ValidationReadiness>,
) -> JoinHandle<()> {
    spawn_traced_named(
        "dd-api-key-validation",
        run_validation_loop(endpoints, client, interval, readiness_tx),
    )
}

async fn run_validation_loop(
    mut endpoints: Vec<RoutableEndpoint>, mut client: HttpClient, interval: Duration,
    readiness_tx: mpsc::Sender<ValidationReadiness>,
) {
    if !validate_and_send_readiness(&mut endpoints, &mut client, &readiness_tx).await {
        return;
    }

    let mut interval = time::interval(interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The startup validation above is the immediate tick.
    interval.tick().await;

    loop {
        interval.tick().await;
        if !validate_and_send_readiness(&mut endpoints, &mut client, &readiness_tx).await {
            return;
        }
    }
}

async fn validate_and_send_readiness(
    endpoints: &mut [RoutableEndpoint], client: &mut HttpClient, readiness_tx: &mpsc::Sender<ValidationReadiness>,
) -> bool {
    let targets = collect_validation_targets(endpoints);
    let readiness = validate_targets(client, &targets).await;

    if readiness_tx.send(readiness).await.is_err() {
        debug!("API key validation readiness receiver dropped; stopping validation task.");
        return false;
    }

    true
}

async fn validate_targets(client: &mut HttpClient, targets: &[ValidationTarget]) -> ValidationReadiness {
    if targets.is_empty() {
        warn!("No Datadog API keys are available for validation; marking forwarder ready.");
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

fn collect_validation_targets(endpoints: &mut [RoutableEndpoint]) -> Vec<ValidationTarget> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    for routable in endpoints {
        // `api_key()` lazily refreshes the cached key from live configuration before validation.
        let endpoint = routable.endpoint_mut();
        let api_key = endpoint.api_key().trim().to_string();
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use axum::{routing::get, Router};
    use saluki_component_config::forwarder as leaf;
    use saluki_component_config::ScopedConfig;
    use saluki_tls::initialize_default_crypto_provider;
    use tokio::net::TcpListener;
    use tokio::sync::watch;

    use super::*;
    use crate::common::datadog::{config::ForwarderConfiguration, endpoints::ResolvedEndpoint};

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
        let leaf_cfg = leaf::DatadogForwarderConfig {
            endpoint: leaf::EndpointConfiguration {
                api_key: "primary-key".to_string(),
                dd_url: Some("http://primary.example.com".to_string()),
                additional_endpoints: leaf::AdditionalEndpoints(
                    [(
                        "http://additional.example.com".to_string(),
                        vec![
                            "additional-key".to_string(),
                            "additional-key".to_string(),
                            String::new(),
                        ],
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            },
            opw_metrics: leaf::OpwMetricsConfiguration {
                observability_pipelines_worker_enabled: true,
                observability_pipelines_worker_url: "http://opw.example.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let forwarder_config = ForwarderConfiguration::from_native(&leaf_cfg);
        let mut endpoints = forwarder_config
            .build_routable_endpoints(None)
            .expect("endpoints should resolve");

        let targets = collect_validation_targets(&mut endpoints);
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
    async fn validation_targets_refresh_existing_additional_endpoint_key() {
        fn forwarder_config_with(additional: &[(&str, &[&str])]) -> leaf::DatadogForwarderConfig {
            leaf::DatadogForwarderConfig {
                endpoint: leaf::EndpointConfiguration {
                    api_key: "primary-key".to_string(),
                    dd_url: Some("http://primary.example.com".to_string()),
                    additional_endpoints: leaf::AdditionalEndpoints(
                        additional
                            .iter()
                            .map(|(url, keys)| {
                                (url.to_string(), keys.iter().map(|k| k.to_string()).collect::<Vec<_>>())
                            })
                            .collect(),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            }
        }

        let initial = forwarder_config_with(&[("http://additional.example.com", &["old-additional-key"])]);
        let (tx, rx) = watch::channel(initial.clone());
        let live = ScopedConfig::live(initial.clone(), rx);

        let forwarder_config = ForwarderConfiguration::from_native(&initial);
        let mut endpoints = forwarder_config
            .build_routable_endpoints(Some(live))
            .expect("endpoints should resolve");

        // Publish a config that rotates the additional key and introduces a new domain. The new
        // domain is ignored because endpoints are resolved once at startup; only existing endpoints
        // refresh their keys.
        tx.send(forwarder_config_with(&[
            ("http://additional.example.com", &["new-additional-key"]),
            ("http://new.example.com", &["ignored-new-domain-key"]),
        ]))
        .expect("receiver alive");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let targets = loop {
            let targets = collect_validation_targets(&mut endpoints);
            if targets.iter().any(|target| target.api_key == "new-additional-key") {
                break targets;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for key refresh"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        assert!(targets.iter().any(|target| target.api_key == "new-additional-key"));
        assert!(!targets
            .iter()
            .any(|target| target.endpoint.as_str() == "http://new.example.com/"));
    }

    #[tokio::test]
    async fn readiness_is_not_ready_only_when_all_targets_are_confirmed_invalid() {
        let invalid_url = start_validation_server(StatusCode::FORBIDDEN).await;
        let error_url = start_validation_server(StatusCode::INTERNAL_SERVER_ERROR).await;
        let mut client = test_client(Duration::from_secs(1));

        assert_eq!(
            validate_targets(&mut client, &[target_for(&invalid_url)]).await,
            ValidationReadiness::NotReady
        );
        assert_eq!(
            validate_targets(&mut client, &[target_for(&error_url)]).await,
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
            validate_targets(&mut client, &[target_for(&valid_url), target_for(&later_url)]).await,
            ValidationReadiness::Ready
        );
        assert_eq!(later_requests.load(Ordering::SeqCst), 0);
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
