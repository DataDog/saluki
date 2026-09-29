//! Internal tests of the remote configuration client.
//!
//! A product's configurations are either several instances of one schema under arbitrary IDs, or a known set of IDs
//! each with a schema of its own. These tests exercise both shapes through the client's evaluation and subscription
//! paths. They also cover apply error messages, the mapping of RPC statuses to fetch errors, settings validation, and
//! the client's subscription registry, all without the polling loop, which [`worker`] exercises instead.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier, OnceLock};
use std::time::Duration;

use datadog_protos::remote_config::{ClientGetConfigsRequest, ClientGetConfigsResponse};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use snafu::Snafu;

use crate::decoder::{evaluate, Outcome, Verdict};
use crate::source::{FetchError, RcAgent};
use crate::{
    AgentIdentity, ApplyError, ClientKind, ConfigId, Error, ProductDecoder, RcClientConfiguration,
    RemoteConfigurationClient, RemoteConfigurationWorker, Subscription, TestPublisher,
};

const TEST_CLIENT_NAME: &str = "test-client";
const TEST_CLIENT_VERSION: &str = "1.2.3";

/// Settings with the standard schedule, for an agent named [`TEST_CLIENT_NAME`] at [`TEST_CLIENT_VERSION`].
fn test_settings() -> RcClientConfiguration {
    RcClientConfiguration::new(ClientKind::Agent(AgentIdentity::new(
        TEST_CLIENT_NAME,
        TEST_CLIENT_VERSION,
    )))
}

#[derive(Debug, Deserialize)]
struct TestAttributes {
    rename: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct TestMetrics {
    drop: Vec<String>,
}

/// The assembled snapshot: attribute mappings are required, metric mappings are not.
struct TestProductPayload {
    attributes: TestAttributes,
    metrics: Option<TestMetrics>,
}

#[derive(Debug, Snafu)]
enum TestProductError {
    #[snafu(display("Configuration {id} is not one this product knows."))]
    UnknownConfiguration { id: String },

    #[snafu(display("Configuration {id} is not valid JSON: {source}"))]
    MalformedConfiguration { id: String, source: serde_json::Error },

    #[snafu(display("No attribute mappings were assigned."))]
    MissingAttributes,
}

impl ApplyError for TestProductError {
    fn apply_error(&self) -> String {
        self.to_string()
    }
}

/// Decodes a product whose configuration IDs are known in advance and each carry a different schema.
#[derive(Default)]
struct TestProductDecoder {
    attributes: Option<TestAttributes>,
    metrics: Option<TestMetrics>,
}

impl ProductDecoder for TestProductDecoder {
    const PRODUCT: &'static str = "TEST_PRODUCT";

    type Snapshot = TestProductPayload;

    type Error = TestProductError;

    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        match &**id {
            "attributes.v1" => self.attributes = Some(from_json(id, payload)?),
            "metrics.v1" => self.metrics = Some(from_json(id, payload)?),
            _ => return Err(TestProductError::UnknownConfiguration { id: id.to_string() }),
        }

        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        let attributes = self.attributes.ok_or(TestProductError::MissingAttributes)?;

        Ok(TestProductPayload {
            attributes,
            metrics: self.metrics,
        })
    }
}

fn from_json<T: DeserializeOwned>(id: &ConfigId, payload: &[u8]) -> Result<T, TestProductError> {
    serde_json::from_slice(payload).map_err(|source| TestProductError::MalformedConfiguration {
        id: id.to_string(),
        source,
    })
}

#[derive(Debug, Deserialize)]
struct TestInstance {
    rename: BTreeMap<String, String>,
}

/// Decodes a product assigned any number of configurations of one schema, keeping the last valid one.
#[derive(Default)]
struct TestLastValidDecoder {
    chosen: Option<TestInstance>,
}

impl ProductDecoder for TestLastValidDecoder {
    const PRODUCT: &'static str = "TEST_LAST_VALID";

    type Snapshot = TestInstance;

    type Error = String;

    fn decode(&mut self, _id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        self.chosen = Some(serde_json::from_slice(payload).map_err(|_| "Configuration is not valid JSON.".to_owned())?);

        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        self.chosen.ok_or_else(|| "No configuration was assigned.".to_owned())
    }
}

#[test]
fn string_apply_error_preserves_message() {
    for message in ["Required configuration is missing.", "", "  details\nwith whitespace  "] {
        assert_eq!(message.to_owned().apply_error(), message);
    }
}

#[tokio::test]
async fn decodes_configurations_of_differing_shapes() {
    let (publisher, mut subscription) = TestPublisher::<TestProductPayload, TestProductError>::new();
    assert!(subscription.current().is_none());

    publisher.assign::<TestProductDecoder>([
        ("metrics.v1", br#"{"drop":["runtime.jvm.gc.count"]}"#.as_slice()),
        (
            "attributes.v1",
            br#"{"rename":{"http.host":"server.address"}}"#.as_slice(),
        ),
    ]);
    let current = subscription.current().expect("should publish");
    let snapshot = subscription.changed().await.expect("should build");
    assert!(std::sync::Arc::ptr_eq(&current, &snapshot));

    assert_eq!(
        Some(&"server.address".to_string()),
        snapshot.attributes.rename.get("http.host")
    );
    assert_eq!(
        vec!["runtime.jvm.gc.count".to_string()],
        snapshot.metrics.as_ref().expect("should decode metrics").drop
    );
    assert!(std::sync::Arc::ptr_eq(&snapshot, &subscription.current().unwrap()));
}

#[tokio::test]
async fn rejects_one_configuration_and_keeps_the_rest() {
    let (publisher, mut subscription) = TestPublisher::<TestProductPayload, TestProductError>::new();
    publisher.assign::<TestProductDecoder>([
        (
            "attributes.v1",
            br#"{"rename":{"http.host":"server.address"}}"#.as_slice(),
        ),
        ("metrics.v1", b"{".as_slice()),
    ]);
    let snapshot = subscription
        .changed()
        .await
        .expect("should build without the malformed configuration");

    assert!(snapshot.attributes.rename.contains_key("http.host"));
    assert!(snapshot.metrics.is_none());

    let evaluated = evaluate::<TestProductDecoder>(vec![
        (ConfigId::new("metrics.v1"), b"{"),
        (ConfigId::new("attributes.v1"), br#"{"rename":{}}"#),
    ]);
    assert!(matches!(evaluated.outcome, Outcome::Accepted(_)));
    assert_eq!(evaluated.verdicts[0].0.to_string(), "attributes.v1");
    assert!(matches!(evaluated.verdicts[0].1, Verdict::Acknowledged));
    assert_eq!(evaluated.verdicts[1].0.to_string(), "metrics.v1");
    assert!(matches!(
        &evaluated.verdicts[1].1,
        Verdict::DecodeRejected(reason) if reason.contains("metrics.v1")
    ));
}

/// Awaits a rejection, failing rather than hanging if none is published.
async fn expect_rejection<T, E>(changed: impl std::future::Future<Output = Result<T, E>>) -> E {
    tokio::time::timeout(std::time::Duration::from_secs(5), changed)
        .await
        .expect("a rejection should be published")
        .err()
        .expect("the publication should be a rejection")
}

#[tokio::test]
async fn rejects_a_snapshot_missing_a_required_configuration() {
    let (publisher, mut subscription) = TestPublisher::<TestProductPayload, TestProductError>::new();
    publisher.assign::<TestProductDecoder>([("metrics.v1", br#"{"drop":[]}"#)]);

    assert!(matches!(
        &*expect_rejection(subscription.changed()).await,
        TestProductError::MissingAttributes
    ));
    assert!(subscription.current().is_none());

    let evaluated = evaluate::<TestProductDecoder>(vec![
        (ConfigId::new("metrics.v1"), br#"{"drop":[]}"#),
        (ConfigId::new("unknown.v1"), b"{}"),
    ]);
    assert!(matches!(
        evaluated.outcome,
        Outcome::Rejected {
            error: TestProductError::MissingAttributes,
            ..
        }
    ));
    assert!(matches!(&evaluated.verdicts[0].1,
        Verdict::BuildRejected(reason) if reason == "No attribute mappings were assigned."
    ));
    assert!(matches!(&evaluated.verdicts[1].1,
        Verdict::DecodeRejected(reason) if reason == "Configuration unknown.v1 is not one this product knows."
    ));
}

#[tokio::test]
async fn rejects_an_empty_assignment_when_configuration_is_required() {
    let (publisher, mut subscription) = TestPublisher::<TestProductPayload, TestProductError>::new();
    publisher.assign::<TestProductDecoder>([("attributes.v1", br#"{"rename":{}}"#)]);
    let accepted = subscription.changed().await.unwrap();

    publisher.assign::<TestProductDecoder>(Vec::<(&str, &[u8])>::new());
    assert!(matches!(
        &*expect_rejection(subscription.changed()).await,
        TestProductError::MissingAttributes
    ));
    assert!(std::sync::Arc::ptr_eq(&accepted, &subscription.current().unwrap()));

    let evaluated = evaluate::<TestProductDecoder>(vec![]);
    assert!(matches!(
        evaluated.outcome,
        Outcome::Rejected {
            error: TestProductError::MissingAttributes,
            ..
        }
    ));
    assert!(evaluated.verdicts.is_empty());
}

#[tokio::test]
async fn reduces_configurations_of_one_shape_in_ascending_order() {
    let (publisher, mut subscription) = TestPublisher::<TestInstance>::new();
    publisher.assign::<TestLastValidDecoder>([
        ("instance.v3", br#"{"rename":{"host":"host.name"}}"#.as_slice()),
        ("instance.v1", br#"{"rename":{}}"#.as_slice()),
        ("instance.v2", b"{".as_slice()),
    ]);
    let snapshot = subscription
        .changed()
        .await
        .expect("should build from the valid configurations");

    assert_eq!(Some(&"host.name".to_string()), snapshot.rename.get("host"));
}

#[derive(Default)]
struct TestPanickingDecoder {
    panic_in_build: bool,
}

impl ProductDecoder for TestPanickingDecoder {
    // The same product as `TestLastValidDecoder`, so the two contend for one subscription.
    const PRODUCT: &'static str = "TEST_LAST_VALID";

    type Snapshot = ();
    type Error = String;

    fn decode(&mut self, _id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        match payload {
            b"decode" => panic!("decoder panicked"),
            b"build" => self.panic_in_build = true,
            _ => {}
        }
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        assert!(!self.panic_in_build, "build panicked");
        Ok(())
    }
}

#[tokio::test]
async fn panics_reject_every_configuration_without_publishing() {
    let (publisher, mut subscription) = TestPublisher::<()>::new();
    publisher.assign::<TestPanickingDecoder>(Vec::<(&str, &[u8])>::new());
    let accepted = subscription.changed().await.unwrap();

    for panic_at in ["decode", "build"] {
        let evaluated = evaluate::<TestPanickingDecoder>(vec![
            (ConfigId::new("first"), b"ok"),
            (ConfigId::new("second"), panic_at.as_bytes()),
        ]);
        assert!(matches!(evaluated.outcome, Outcome::Panicked));
        assert_eq!(evaluated.verdicts.len(), 2);
        assert!(evaluated
            .verdicts
            .iter()
            .all(|(_, verdict)| matches!(verdict, Verdict::Panicked)));

        publisher.assign::<TestPanickingDecoder>([("first", "ok"), ("second", panic_at)]);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), subscription.changed())
                .await
                .is_err()
        );
        assert!(std::sync::Arc::ptr_eq(&accepted, &subscription.current().unwrap()));
    }
}

#[tokio::test]
async fn clones_observe_latest_state_and_closed_subscriptions_wait() {
    let (publisher, mut subscription) = TestPublisher::<u32>::new();
    let mut clone = subscription.clone();
    publisher.accept(1);
    publisher.accept(2);
    assert_eq!(*subscription.changed().await.unwrap(), 2);
    assert_eq!(*clone.changed().await.unwrap(), 2);

    publisher.reject("invalid".to_owned());
    assert_eq!(*expect_rejection(subscription.changed()).await, "invalid");
    assert_eq!(*expect_rejection(clone.changed()).await, "invalid");
    assert_eq!(*subscription.current().unwrap(), 2);
    assert_eq!(*clone.current().unwrap(), 2);

    drop(publisher);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), subscription.changed())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_pending_publication_is_observed_after_the_publisher_stops() {
    let (publisher, mut subscription) = TestPublisher::<u32>::new();
    publisher.accept(3);
    drop(publisher);

    assert_eq!(*subscription.changed().await.unwrap(), 3);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), subscription.changed())
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn an_inert_subscription_has_no_snapshot_and_never_changes() {
    let mut inert = Subscription::<u32>::inert();
    let mut clone = inert.clone();

    assert!(inert.current().is_none());
    assert!(clone.current().is_none());
    // No publisher backs an inert subscription, so its channel starts closed rather than merely empty.
    assert!(inert.receiver.has_changed().is_err());
    assert!(clone.receiver.has_changed().is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3600), inert.changed())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3600), clone.changed())
            .await
            .is_err()
    );
}

/// A snapshot type without `Debug`, so the tests below show that no `Debug` bound is needed on it.
struct TestOpaque;

/// A consumer that holds a subscription and derives `Debug`, which compiles only if `Subscription` implements `Debug`
/// without requiring it of the snapshot.
#[derive(Debug)]
struct TestConsumer {
    subscription: Subscription<TestOpaque>,
}

#[test]
fn a_consumer_holding_a_subscription_can_derive_debug() {
    let (publisher, subscription) = TestPublisher::<TestOpaque>::new();
    let consumer = TestConsumer { subscription };
    assert_eq!(
        format!("{consumer:?}"),
        "TestConsumer { subscription: Subscription { accepted: false, .. } }"
    );

    publisher.accept(TestOpaque);
    assert!(consumer.subscription.current().is_some());
    assert_eq!(
        format!("{consumer:?}"),
        "TestConsumer { subscription: Subscription { accepted: true, .. } }"
    );

    let inert = TestConsumer {
        subscription: Subscription::inert(),
    };
    assert_eq!(
        format!("{inert:?}"),
        "TestConsumer { subscription: Subscription { accepted: false, .. } }"
    );
}

#[test]
fn a_test_publisher_debug_shows_whether_it_is_subscribed() {
    let (publisher, subscription) = TestPublisher::<TestOpaque>::new();
    assert_eq!(format!("{publisher:?}"), "TestPublisher { subscribed: true, .. }");

    drop(subscription);
    assert_eq!(format!("{publisher:?}"), "TestPublisher { subscribed: false, .. }");
}

#[test]
#[should_panic(expected = "duplicate configuration ID")]
fn test_publisher_rejects_duplicate_ids() {
    let (publisher, _) = TestPublisher::<TestInstance>::new();
    publisher.assign::<TestLastValidDecoder>([("same", b"{}"), ("same", b"{}")]);
}

#[test]
fn fetch_error_retains_unimplemented_status() {
    let error = FetchError::from(tonic::Status::unimplemented("Remote Configuration is disabled"));
    let FetchError::Unimplemented(cause) = error else {
        panic!("expected an unimplemented RPC");
    };

    let status = cause
        .downcast_ref::<tonic::Status>()
        .expect("original status is retained");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert_eq!(status.message(), "Remote Configuration is disabled");
}

#[test]
fn fetch_error_retains_other_rpc_statuses() {
    for code in [
        tonic::Code::Unavailable,
        tonic::Code::Unauthenticated,
        tonic::Code::InvalidArgument,
    ] {
        let error = FetchError::from(tonic::Status::new(code, "poll failed"));
        let FetchError::Rpc(cause) = error else {
            panic!("expected an RPC failure");
        };

        let status = cause
            .downcast_ref::<tonic::Status>()
            .expect("original status is retained");
        assert_eq!(status.code(), code);
        assert_eq!(status.message(), "poll failed");
    }
}

#[test]
fn settings_default_to_the_upstream_poll_schedule() {
    let config = test_settings();

    assert_eq!(std::time::Duration::from_secs(5), config.poll_interval);
    assert_eq!(std::time::Duration::from_secs(90), config.max_backoff);
    assert_eq!(std::time::Duration::from_secs(30), config.request_timeout);
    config.validate().unwrap();
}

fn settings(poll_interval: u64, max_backoff: u64, request_timeout: u64) -> crate::RcClientConfiguration {
    crate::RcClientConfiguration {
        poll_interval: std::time::Duration::from_millis(poll_interval),
        max_backoff: std::time::Duration::from_millis(max_backoff),
        request_timeout: std::time::Duration::from_millis(request_timeout),
        ..test_settings()
    }
}

#[test]
fn settings_reject_an_empty_agent_name_or_version() {
    let config = RcClientConfiguration::new(ClientKind::Agent(AgentIdentity::new("", TEST_CLIENT_VERSION)));
    let error = config.validate().unwrap_err();
    assert!(matches!(error, crate::Error::InvalidSettings { .. }));
    assert_eq!(error.to_string(), "The agent name must not be empty.");

    let config = RcClientConfiguration::new(ClientKind::Agent(AgentIdentity::new(TEST_CLIENT_NAME, "")));
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "The agent version must not be empty."
    );
}

#[test]
fn settings_reject_invalid_poll_intervals() {
    for poll_interval in [0, 999] {
        let error = settings(poll_interval, 90_000, 30_000).validate().unwrap_err();
        assert!(matches!(error, crate::Error::InvalidSettings { .. }));
        assert!(error.to_string().contains("poll_interval must be at least one second"));
    }
}

#[test]
fn settings_reject_max_backoff_below_poll_interval() {
    let error = settings(5_000, 4_000, 30_000).validate().unwrap_err();
    assert!(matches!(error, crate::Error::InvalidSettings { .. }));
    assert!(error.to_string().contains("max_backoff must be at least poll_interval"));

    settings(1_000, 1_000, 30_000).validate().unwrap();
}

#[test]
fn settings_reject_invalid_request_timeouts() {
    for request_timeout in [0, 999] {
        let error = settings(5_000, 90_000, request_timeout).validate().unwrap_err();
        assert!(matches!(error, crate::Error::InvalidSettings { .. }));
        assert!(error
            .to_string()
            .contains("request_timeout must be at least one second"));
    }

    settings(5_000, 90_000, 1_000).validate().unwrap();
}

/// Stands in for the Agent in tests that never start the polling loop.
struct TestNoAgent;

#[async_trait::async_trait]
impl RcAgent for TestNoAgent {
    async fn get_configs(&mut self, _request: ClientGetConfigsRequest) -> Result<ClientGetConfigsResponse, FetchError> {
        unreachable!("these tests never start the polling loop")
    }
}

fn client() -> (RemoteConfigurationClient, RemoteConfigurationWorker) {
    RemoteConfigurationClient::with_agent(Box::new(TestNoAgent), test_settings())
}

fn instance_payload() -> Vec<(ConfigId, &'static [u8])> {
    vec![(ConfigId::new("instance.v1"), br#"{"rename":{"host":"host.name"}}"#)]
}

fn assert_already_subscribed<T>(result: crate::Result<T>, expected: &str) {
    match result {
        Err(Error::AlreadySubscribed { product }) => assert_eq!(product, expected),
        Err(error) => panic!("expected AlreadySubscribed, got {error}"),
        Ok(_) => panic!("expected AlreadySubscribed, got a subscription"),
    }
}

#[test]
fn a_subscription_is_to_the_product_its_decoder_names() {
    let (client, worker) = client();

    let _product = client.subscribe::<TestProductDecoder>().unwrap();
    assert_already_subscribed(client.subscribe::<TestProductDecoder>(), "TEST_PRODUCT");
    assert!(worker.shared.assign("TEST_PRODUCT", Vec::new()).is_some());

    // A decoder naming another product subscribes alongside it.
    let _last_valid = client.subscribe::<TestLastValidDecoder>().unwrap();
}

#[test]
fn a_second_live_subscription_is_rejected_whatever_its_decoder() {
    let (client, _worker) = client();
    let subscription = client.subscribe::<TestLastValidDecoder>().unwrap();

    assert_already_subscribed(client.subscribe::<TestLastValidDecoder>(), "TEST_LAST_VALID");
    assert_already_subscribed(client.subscribe::<TestPanickingDecoder>(), "TEST_LAST_VALID");
    assert_already_subscribed(client.clone().subscribe::<TestLastValidDecoder>(), "TEST_LAST_VALID");

    // A surviving clone keeps the product subscribed.
    let clone = subscription.clone();
    drop(subscription);
    assert_already_subscribed(client.subscribe::<TestLastValidDecoder>(), "TEST_LAST_VALID");
    drop(clone);
    client.subscribe::<TestLastValidDecoder>().unwrap();
}

#[tokio::test]
async fn resubscribing_starts_without_an_accepted_snapshot() {
    let (client, worker) = client();
    let mut first = client.subscribe::<TestLastValidDecoder>().unwrap();
    worker.shared.assign("TEST_LAST_VALID", instance_payload()).unwrap();
    assert!(first.changed().await.is_ok());
    drop(first);

    // The replacement may use a different decoder, and inherits nothing from the subscription it replaces.
    let mut second = client.subscribe::<TestPanickingDecoder>().unwrap();
    assert!(second.current().is_none());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), second.changed())
            .await
            .is_err()
    );

    let (_, evaluation) = worker
        .shared
        .assign("TEST_LAST_VALID", vec![(ConfigId::new("first"), b"ok")])
        .unwrap();
    let verdicts = evaluation.verdicts;
    assert_eq!(verdicts.len(), 1);
    assert!(matches!(verdicts[0].1, Verdict::Acknowledged));
    assert!(second.changed().await.is_ok());
}

#[tokio::test]
async fn the_worker_delivers_into_subscriptions_made_by_any_client_handle() {
    let (client, worker) = client();
    assert!(Arc::ptr_eq(&client.shared, &worker.shared));

    let mut last_valid = client.clone().subscribe::<TestLastValidDecoder>().unwrap();
    let (_, evaluation) = worker
        .shared
        .assign(
            "TEST_LAST_VALID",
            vec![
                (ConfigId::new("instance.v2"), b"{"),
                (ConfigId::new("instance.v1"), br#"{"rename":{"host":"host.name"}}"#),
            ],
        )
        .unwrap();
    let verdicts = evaluation.verdicts;

    let snapshot = last_valid.changed().await.unwrap();
    assert_eq!(Some(&"host.name".to_string()), snapshot.rename.get("host"));
    assert_eq!(verdicts[0], (ConfigId::new("instance.v1"), Verdict::Acknowledged));
    assert_eq!(
        verdicts[1],
        (
            ConfigId::new("instance.v2"),
            Verdict::DecodeRejected("Configuration is not valid JSON.".to_owned())
        )
    );

    assert!(worker.shared.assign("TEST_UNSUBSCRIBED", instance_payload()).is_none());
}

#[test]
fn each_client_has_its_own_random_id() {
    let (first, _first_worker) = client();
    let (second, _second_worker) = client();

    let id = uuid::Uuid::parse_str(&first.shared.client_id).unwrap();
    assert_eq!(id.get_version(), Some(uuid::Version::Random));
    assert_ne!(first.shared.client_id, second.shared.client_id);
    assert_eq!(first.shared.client_id, first.clone().shared.client_id);
}

#[test]
fn the_client_and_worker_debug_show_the_client_id() {
    let (client, worker) = client();
    let id = client.shared.client_id.clone();

    assert_eq!(
        format!("{client:?}"),
        format!("RemoteConfigurationClient {{ client_id: {id:?}, .. }}")
    );
    let worker_debug = format!("{worker:?}");
    assert!(
        worker_debug.starts_with(&format!(
            "RemoteConfigurationWorker {{ client_id: {id:?}, config: RcClientConfiguration {{"
        )),
        "{worker_debug}"
    );
    assert!(worker_debug.ends_with(", .. }"), "{worker_debug}");
}

#[tokio::test]
async fn subscribing_wakes_the_worker_once() {
    let (client, worker) = client();
    let _last_valid = client.subscribe::<TestLastValidDecoder>().unwrap();
    let _product = client.subscribe::<TestProductDecoder>().unwrap();

    tokio::time::timeout(std::time::Duration::from_millis(10), worker.shared.wake.notified())
        .await
        .expect("subscribing should wake the worker");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), worker.shared.wake.notified())
            .await
            .is_err()
    );

    // A rejected subscribe does not wake the worker.
    assert_already_subscribed(client.subscribe::<TestLastValidDecoder>(), "TEST_LAST_VALID");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), worker.shared.wake.notified())
            .await
            .is_err()
    );
}

/// A snapshot whose `Drop` panics, standing in for a subscriber type with a faulty destructor.
struct TestPanicOnDrop;

impl Drop for TestPanicOnDrop {
    fn drop(&mut self) {
        panic!("a subscriber's snapshot panicked on drop");
    }
}

#[derive(Default)]
struct TestPanicOnDropDecoder;

impl ProductDecoder for TestPanicOnDropDecoder {
    const PRODUCT: &'static str = "TEST_PANIC_ON_DROP";

    type Snapshot = TestPanicOnDrop;
    type Error = String;

    fn decode(&mut self, _id: &ConfigId, _payload: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        Ok(TestPanicOnDrop)
    }
}

/// Subscribes to [`TestPanicOnDropDecoder`], publishes a snapshot, and drops the subscription, leaving the registry
/// holding the snapshot's last reference.
fn abandon_a_panicking_snapshot(client: &RemoteConfigurationClient, worker: &RemoteConfigurationWorker) {
    let subscription = client.subscribe::<TestPanicOnDropDecoder>().unwrap();
    worker
        .shared
        .assign(TestPanicOnDropDecoder::PRODUCT, Vec::new())
        .unwrap();
    drop(subscription);
}

#[test]
fn a_panic_dropping_a_pruned_snapshot_does_not_poison_the_registry() {
    let (client, worker) = client();
    abandon_a_panicking_snapshot(&client, &worker);

    assert!(std::panic::catch_unwind(AssertUnwindSafe(|| worker.shared.live_products())).is_err());
    assert!(!worker.shared.is_poisoned());
    let _last_valid = client.subscribe::<TestLastValidDecoder>().unwrap();
    assert!(worker
        .shared
        .live_products()
        .contains_key(TestLastValidDecoder::PRODUCT));
}

/// A snapshot and rejection whose `Drop` reads the subscription it was published to, as a subscriber's `Drop` might.
struct TestReadsOnDrop {
    subscription: Arc<OnceLock<Subscription<TestReadsOnDrop, TestReadsOnDrop>>>,
    reads: Arc<AtomicUsize>,
}

impl Drop for TestReadsOnDrop {
    fn drop(&mut self) {
        if let Some(subscription) = self.subscription.get() {
            let _ = subscription.current();
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[test]
fn a_replaced_snapshot_or_rejection_can_read_the_subscription_when_dropped() {
    let (done, finished) = mpsc::channel();
    // A drop under the channel's lock deadlocks this thread, so the test waits for it with a timeout.
    std::thread::spawn(move || {
        let (publisher, subscription) = TestPublisher::<TestReadsOnDrop, TestReadsOnDrop>::new();
        let shared = Arc::new(OnceLock::new());
        let reads = Arc::new(AtomicUsize::new(0));
        let value = || TestReadsOnDrop {
            subscription: Arc::clone(&shared),
            reads: Arc::clone(&reads),
        };
        let _ = shared.set(subscription);

        publisher.accept(value());
        publisher.reject(value());
        // Replaces the rejection.
        publisher.reject(value());
        // Replaces the accepted snapshot and the second rejection.
        publisher.accept(value());
        done.send(reads.load(Ordering::SeqCst)).unwrap();
    });

    let reads = finished
        .recv_timeout(Duration::from_secs(10))
        .expect("dropping a replaced value should not wait for the channel's lock");
    assert_eq!(reads, 3);
}

#[test]
fn a_panic_dropping_a_replaced_snapshot_does_not_poison_the_registry() {
    let (client, worker) = client();
    abandon_a_panicking_snapshot(&client, &worker);

    assert!(std::panic::catch_unwind(AssertUnwindSafe(|| client.subscribe::<TestPanicOnDropDecoder>())).is_err());
    assert!(!worker.shared.is_poisoned());
    client.subscribe::<TestLastValidDecoder>().unwrap();
}

#[test]
fn the_registry_recovers_from_a_poisoned_lock() {
    let (client, worker) = client();
    let shared = Arc::clone(&worker.shared);
    std::thread::spawn(move || shared.panic_while_locked())
        .join()
        .unwrap_err();
    assert!(worker.shared.is_poisoned());

    let _last_valid = client.subscribe::<TestLastValidDecoder>().unwrap();
    assert!(worker
        .shared
        .live_products()
        .contains_key(TestLastValidDecoder::PRODUCT));
    assert!(worker
        .shared
        .assign(TestLastValidDecoder::PRODUCT, instance_payload())
        .is_some());
}

static DECODING: Barrier = Barrier::new(2);
static RELEASED: Barrier = Barrier::new(2);

/// Blocks in `build` until the test releases it, so a test can act on the registry while a decoder runs.
#[derive(Default)]
struct TestGatedDecoder;

impl ProductDecoder for TestGatedDecoder {
    const PRODUCT: &'static str = "TEST_GATED";

    type Snapshot = ();
    type Error = String;

    fn decode(&mut self, _id: &ConfigId, _payload: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        DECODING.wait();
        RELEASED.wait();
        Ok(())
    }
}

#[test]
fn a_running_decoder_does_not_block_subscribing() {
    let (client, worker) = client();
    let first = client.subscribe::<TestGatedDecoder>().unwrap();
    let shared = Arc::clone(&worker.shared);
    let decoding = std::thread::spawn(move || shared.assign(TestGatedDecoder::PRODUCT, Vec::new()));
    DECODING.wait();

    // A subscribe that waited for the decoder would never return, since the decoder waits for this test.
    drop(first);
    let (subscribed_tx, subscribed_rx) = std::sync::mpsc::channel();
    let subscriber = client.clone();
    std::thread::spawn(move || {
        let _ = subscribed_tx.send(subscriber.subscribe::<TestGatedDecoder>());
    });
    let subscribed = subscribed_rx.recv_timeout(std::time::Duration::from_secs(5));
    RELEASED.wait();
    let replacement = subscribed
        .expect("subscribing should not wait for the decoder")
        .unwrap();

    // The replacement receives nothing from the decoder run it replaced, and the worker can tell it was replaced.
    let (generation, _) = decoding.join().unwrap().unwrap();
    assert!(replacement.current().is_none());
    assert!(!replacement.receiver.has_changed().unwrap());
    assert_ne!(worker.shared.live_products()[TestGatedDecoder::PRODUCT], generation);
}

mod worker;
