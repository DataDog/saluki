//! Exercises the worker against a fake Agent: polling, integrity checks, caching, status reporting, and scheduling.
//!
//! Every test runs under Tokio's paused clock, so waits between polls take no real time and are measured exactly. The
//! metrics and log tests additionally run under a thread-local recorder or subscriber, which is what lets them assert
//! the client's own counters and log lines.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fmt, mem};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use datadog_protos::remote_config::{
    ClientGetConfigsRequest, ClientGetConfigsResponse, ConfigState, ConfigStatus, File, TargetFileMeta,
};
use metrics::{Key, Label, SharedString, Unit};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use metrics_util::{CompositeKey, MetricKind};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::Supervisable as _;
use saluki_error::generic_error;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};
use tracing::field::{Field, Visit};
use tracing::subscriber::NoSubscriber;
use tracing::{Dispatch, Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

use super::{test_settings, TEST_CLIENT_NAME, TEST_CLIENT_VERSION};
use crate::protocol::sha256;
use crate::source::{FetchError, RcAgent};
use crate::{
    AgentIdentity, ClientKind, ConfigId, ProductDecoder, RcClientConfiguration, RemoteConfigurationClient,
    RemoteConfigurationWorker, Subscription,
};

/// The Agent side of a fake connection: the test receives each poll and chooses its answer.
struct Agent {
    requests: mpsc::UnboundedReceiver<ClientGetConfigsRequest>,
    responses: mpsc::UnboundedSender<Result<ClientGetConfigsResponse, FetchError>>,
}

struct FakeAgent {
    requests: mpsc::UnboundedSender<ClientGetConfigsRequest>,
    responses: mpsc::UnboundedReceiver<Result<ClientGetConfigsResponse, FetchError>>,
}

#[async_trait::async_trait]
impl RcAgent for FakeAgent {
    async fn get_configs(&mut self, request: ClientGetConfigsRequest) -> Result<ClientGetConfigsResponse, FetchError> {
        let _ = self.requests.send(request);
        match self.responses.recv().await {
            Some(response) => response,
            None => std::future::pending().await,
        }
    }
}

impl Agent {
    /// Waits for the worker's next poll.
    async fn poll(&mut self) -> ClientGetConfigsRequest {
        timeout(Duration::from_secs(3600), self.requests.recv())
            .await
            .expect("the worker should poll")
            .expect("the worker should be running")
    }

    fn respond(&self, response: Result<ClientGetConfigsResponse, FetchError>) {
        self.responses.send(response).unwrap();
    }

    /// Waits for the next poll and answers it.
    async fn exchange(&mut self, response: ClientGetConfigsResponse) -> ClientGetConfigsRequest {
        let request = self.poll().await;
        self.respond(Ok(response));
        request
    }

    /// Asserts that the worker does not poll within `window`.
    async fn assert_quiet(&mut self, window: Duration) {
        assert!(timeout(window, self.requests.recv()).await.is_err(), "unexpected poll");
    }
}

fn client_with(config: RcClientConfiguration) -> (RemoteConfigurationClient, RemoteConfigurationWorker, Agent) {
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let (response_tx, response_rx) = mpsc::unbounded_channel();
    let (client, worker) = RemoteConfigurationClient::with_agent(
        Box::new(FakeAgent {
            requests: request_tx,
            responses: response_rx,
        }),
        config,
    );
    let agent = Agent {
        requests: request_rx,
        responses: response_tx,
    };
    (client, worker, agent)
}

fn client() -> (RemoteConfigurationClient, RemoteConfigurationWorker, Agent) {
    client_with(test_settings())
}

/// Builds an Agent response. Files are assigned under `employee/<PRODUCT>/<id>/<name>` paths.
struct Response {
    version: u64,
    targets: serde_json::Map<String, serde_json::Value>,
    client_configs: Vec<String>,
    files: Vec<File>,
    roots: Vec<Vec<u8>>,
    expired: bool,
}

impl Response {
    fn new(version: u64) -> Self {
        Self {
            version,
            targets: serde_json::Map::new(),
            client_configs: Vec::new(),
            files: Vec::new(),
            roots: Vec::new(),
            expired: false,
        }
    }

    /// Assigns a file and sends its contents.
    fn send(self, path: &str, version: u64, payload: &[u8]) -> Self {
        self.tampered(path, version, payload, payload)
    }

    /// Assigns a file without sending it, as the Agent does for a file the client advertises as cached.
    fn cached(mut self, path: &str, version: u64, payload: &[u8]) -> Self {
        let meta = serde_json::json!({
            "length": payload.len(),
            "hashes": {"sha256": faster_hex::hex_string(&sha256(payload))},
            "custom": {"v": version},
        });
        self.targets.insert(path.to_owned(), meta);
        self.client_configs.push(path.to_owned());
        self
    }

    /// Assigns a file described by `payload` but sends `sent` instead.
    fn tampered(self, path: &str, version: u64, payload: &[u8], sent: &[u8]) -> Self {
        let mut this = self.cached(path, version, payload);
        this.files.push(File {
            path: path.to_owned(),
            raw: sent.to_vec(),
        });
        this
    }

    fn root(mut self, version: u64) -> Self {
        let root = serde_json::json!({"signed": {"_type": "root", "version": version}, "signatures": []});
        self.roots.push(root.to_string().into_bytes());
        self
    }

    fn expired(mut self) -> Self {
        self.expired = true;
        self
    }

    fn build(self) -> ClientGetConfigsResponse {
        let targets = serde_json::json!({
            "signed": {
                "_type": "targets",
                "version": self.version,
                "custom": {"opaque_backend_state": STANDARD.encode(format!("state-{}", self.version))},
                "targets": self.targets,
            },
            "signatures": [],
        });
        ClientGetConfigsResponse {
            roots: self.roots,
            targets: targets.to_string().into_bytes(),
            target_files: self.files,
            client_configs: self.client_configs,
            config_status: if self.expired {
                ConfigStatus::Expired as i32
            } else {
                ConfigStatus::Ok as i32
            },
        }
    }
}

/// Names the product a test decoder subscribes to.
trait Product: Send + 'static {
    const NAME: &'static str;
}

struct Alpha;

impl Product for Alpha {
    const NAME: &'static str = "ALPHA";
}

struct Beta;

impl Product for Beta {
    const NAME: &'static str = "BETA";
}

/// Publishes each configuration's ID and payload, in the order it received them.
///
/// A `bad` payload fails to decode, an `unbuildable` one fails the build, and a `panic` one panics.
struct Recorder<N = Alpha> {
    decoded: Vec<(String, String)>,
    product: PhantomData<fn() -> N>,
}

impl<N> Default for Recorder<N> {
    fn default() -> Self {
        Self {
            decoded: Vec::new(),
            product: PhantomData,
        }
    }
}

impl<N: Product> ProductDecoder for Recorder<N> {
    const PRODUCT: &'static str = N::NAME;

    type Snapshot = Vec<(String, String)>;
    type Error = String;

    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        match payload {
            b"bad" => return Err("Bad payload.".to_owned()),
            b"panic" => panic!("decoder panicked"),
            _ => {}
        }
        self.decoded
            .push((id.to_string(), String::from_utf8_lossy(payload).into_owned()));
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        if self.decoded.iter().any(|(_, payload)| payload == "unbuildable") {
            return Err("Cannot build.".to_owned());
        }
        Ok(self.decoded)
    }
}

/// Like [`Recorder`], but rejects an empty assignment.
#[derive(Default)]
struct Required(Recorder<Beta>);

impl ProductDecoder for Required {
    const PRODUCT: &'static str = Beta::NAME;

    type Snapshot = Vec<(String, String)>;
    type Error = String;

    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        self.0.decode(id, payload)
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        if self.0.decoded.is_empty() {
            return Err("Nothing assigned.".to_owned());
        }
        self.0.build()
    }
}

/// Panics in `build` when assigned nothing.
#[derive(Default)]
struct PanicsWhenEmpty(Recorder<Beta>);

impl ProductDecoder for PanicsWhenEmpty {
    const PRODUCT: &'static str = Beta::NAME;

    type Snapshot = Vec<(String, String)>;
    type Error = String;

    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        self.0.decode(id, payload)
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        assert!(!self.0.decoded.is_empty(), "build panicked");
        self.0.build()
    }
}

async fn next(subscription: &mut Subscription<Vec<(String, String)>>) -> Result<Vec<(String, String)>, String> {
    timeout(Duration::from_secs(1), subscription.changed())
        .await
        .expect("should publish")
        .map(|snapshot| (*snapshot).clone())
        .map_err(|error| (*error).clone())
}

async fn assert_unpublished(subscription: &mut Subscription<Vec<(String, String)>>) {
    assert!(
        timeout(Duration::from_millis(10), subscription.changed())
            .await
            .is_err(),
        "unexpected publication"
    );
}

fn snapshot(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(id, payload)| (id.to_string(), payload.to_string()))
        .collect()
}

fn state(request: &ClientGetConfigsRequest) -> &datadog_protos::remote_config::ClientState {
    request.client.as_ref().unwrap().state.as_ref().unwrap()
}

fn products(request: &ClientGetConfigsRequest) -> Vec<&str> {
    request
        .client
        .as_ref()
        .unwrap()
        .products
        .iter()
        .map(String::as_str)
        .collect()
}

/// Each reported configuration as `(product, id, version, apply_state, apply_error)`.
fn rows(request: &ClientGetConfigsRequest) -> Vec<(&str, &str, u64, u64, &str)> {
    state(request)
        .config_states
        .iter()
        .map(
            |ConfigState {
                 id,
                 version,
                 product,
                 apply_state,
                 apply_error,
             }| {
                (
                    product.as_str(),
                    id.as_str(),
                    *version,
                    *apply_state,
                    apply_error.as_str(),
                )
            },
        )
        .collect()
}

fn cached(request: &ClientGetConfigsRequest) -> BTreeMap<&str, &TargetFileMeta> {
    request
        .cached_target_files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect()
}

const ACKNOWLEDGED: u64 = 2;
const ERROR: u64 = 3;
const UNACKNOWLEDGED: u64 = 1;

#[tokio::test(start_paused = true)]
async fn the_first_poll_identifies_the_client_with_empty_state() {
    let (client, worker, mut agent) = client();
    let _alpha = client.subscribe::<Recorder>().unwrap();
    let _beta = client.subscribe::<Recorder<Beta>>().unwrap();
    tokio::spawn(worker.run());

    let request = agent.poll().await;
    let wire = request.client.as_ref().unwrap();
    assert_eq!(wire.id, client.shared.client_id);
    assert!(wire.is_agent && !wire.is_tracer && !wire.is_updater);
    assert!(wire.client_tracer.is_none());
    let agent_info = wire.client_agent.as_ref().unwrap();
    assert_eq!(agent_info.name, TEST_CLIENT_NAME);
    assert_eq!(agent_info.version, TEST_CLIENT_VERSION);
    assert!(agent_info.cluster_name.is_empty() && agent_info.cluster_id.is_empty());
    assert!(wire.capabilities.is_empty());
    assert_eq!(products(&request), ["ALPHA", "BETA"]);

    let state = state(&request);
    assert_eq!((state.root_version, state.targets_version), (1, 0));
    assert!(state.config_states.is_empty() && !state.has_error && state.backend_client_state.is_empty());
    assert!(request.cached_target_files.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_cluster_level_agent_reports_its_cluster() {
    let identity = AgentIdentity {
        cluster_name: Some("prod-east".to_owned()),
        cluster_id: Some("5a1c3f2e".to_owned()),
        ..AgentIdentity::new(TEST_CLIENT_NAME, TEST_CLIENT_VERSION)
    };
    let (client, worker, mut agent) = client_with(RcClientConfiguration::new(ClientKind::Agent(identity)));
    let _alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    let request = agent.poll().await;
    let agent_info = request.client.as_ref().unwrap().client_agent.as_ref().unwrap();
    assert_eq!(
        (agent_info.cluster_name.as_str(), agent_info.cluster_id.as_str()),
        ("prod-east", "5a1c3f2e")
    );
}

#[tokio::test(start_paused = true)]
async fn delivers_an_assignment_and_reports_each_configuration() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/b/config", 4, b"bad")
                .send("datadog/2/ALPHA/a/config", 3, b"one")
                .root(2)
                .root(3)
                .build(),
        )
        .await;
    assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("a", "one")]));

    let request = agent.poll().await;
    let state = state(&request);
    assert_eq!((state.root_version, state.targets_version), (3, 10));
    assert_eq!(state.backend_client_state, b"state-10");
    assert_eq!(
        rows(&request),
        [
            ("ALPHA", "a", 3, ACKNOWLEDGED, ""),
            ("ALPHA", "b", 4, ERROR, "Bad payload."),
        ]
    );
    let cached = cached(&request);
    assert_eq!(cached.len(), 2);
    let a = cached["datadog/2/ALPHA/a/config"];
    assert_eq!(a.length, 3);
    assert_eq!(a.hashes[0].algorithm, "sha256");
    assert_eq!(a.hashes[0].hash, faster_hex::hex_string(&sha256(b"one")));
}

#[tokio::test(start_paused = true)]
async fn unchanged_contents_are_not_decoded_again() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .send("employee/ALPHA/b/config", 1, b"bad")
                .build(),
        )
        .await;
    next(&mut alpha).await.unwrap();

    // A version bump with unchanged contents keeps each status and reports the new version.
    agent
        .exchange(
            Response::new(11)
                .cached("employee/ALPHA/a/config", 2, b"one")
                .cached("employee/ALPHA/b/config", 5, b"bad")
                .build(),
        )
        .await;
    assert_unpublished(&mut alpha).await;
    let request = agent.poll().await;
    assert_eq!(state(&request).targets_version, 11);
    assert_eq!(
        rows(&request),
        [
            ("ALPHA", "a", 2, ACKNOWLEDGED, ""),
            ("ALPHA", "b", 5, ERROR, "Bad payload."),
        ]
    );

    // The Agent sends an empty response when nothing changed.
    agent.respond(Ok(ClientGetConfigsResponse::default()));
    assert_unpublished(&mut alpha).await;
    assert_eq!(state(&agent.poll().await).targets_version, 11);
}

#[tokio::test(start_paused = true)]
async fn a_changed_assignment_is_decoded_from_cached_and_sent_files() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .send("employee/ALPHA/b/config", 1, b"two")
                .build(),
        )
        .await;
    next(&mut alpha).await.unwrap();

    // `b` is dropped, `a` is unchanged, and `c` is new.
    agent
        .exchange(
            Response::new(11)
                .cached("employee/ALPHA/a/config", 1, b"one")
                .send("employee/ALPHA/c/config", 1, b"three")
                .build(),
        )
        .await;
    assert_eq!(
        next(&mut alpha).await.unwrap(),
        snapshot(&[("a", "one"), ("c", "three")])
    );

    let request = agent.poll().await;
    assert_eq!(
        rows(&request),
        [("ALPHA", "a", 1, ACKNOWLEDGED, ""), ("ALPHA", "c", 1, ACKNOWLEDGED, ""),]
    );
    assert_eq!(
        cached(&request).into_keys().collect::<Vec<_>>(),
        ["employee/ALPHA/a/config", "employee/ALPHA/c/config"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_build_failure_rejects_the_configurations_that_decoded() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .send("employee/ALPHA/b/config", 1, b"bad")
                .send("employee/ALPHA/c/config", 1, b"unbuildable")
                .build(),
        )
        .await;
    assert_eq!(next(&mut alpha).await.unwrap_err(), "Cannot build.");
    assert!(alpha.current().is_none());

    assert_eq!(
        rows(&agent.poll().await),
        [
            ("ALPHA", "a", 1, ERROR, "Cannot build."),
            ("ALPHA", "b", 1, ERROR, "Bad payload."),
            ("ALPHA", "c", 1, ERROR, "Cannot build."),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_decoder_panic_rejects_everything_and_polling_continues() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .send("employee/ALPHA/b/config", 1, b"panic")
                .build(),
        )
        .await;
    assert_unpublished(&mut alpha).await;

    assert_eq!(
        rows(&agent.poll().await),
        [
            ("ALPHA", "a", 1, ERROR, "Product decoder panicked."),
            ("ALPHA", "b", 1, ERROR, "Product decoder panicked."),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_hash_mismatch_aborts_the_whole_poll() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    let mut beta = client.subscribe::<Recorder<Beta>>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build())
        .await;
    next(&mut alpha).await.unwrap();
    next(&mut beta).await.unwrap();

    agent
        .exchange(
            Response::new(11)
                .tampered("employee/ALPHA/a/config", 2, b"two", b"tw0")
                .send("employee/BETA/b/config", 1, b"fine")
                .build(),
        )
        .await;
    assert_unpublished(&mut alpha).await;
    assert_unpublished(&mut beta).await;

    // Nothing was committed: the cursor, cache, and statuses are those from before the bad response.
    let request = agent.poll().await;
    let reported = state(&request);
    assert_eq!(reported.targets_version, 10);
    assert!(reported.has_error);
    assert!(reported.error.contains("employee/ALPHA/a/config"), "{}", reported.error);
    assert_eq!(rows(&request), [("ALPHA", "a", 1, ACKNOWLEDGED, "")]);
    assert_eq!(
        cached(&request).into_keys().collect::<Vec<_>>(),
        ["employee/ALPHA/a/config"]
    );

    // The next good response recovers and clears the reported error.
    agent.respond(Ok(Response::new(11)
        .send("employee/ALPHA/a/config", 2, b"two")
        .send("employee/BETA/b/config", 1, b"fine")
        .build()));
    assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("a", "two")]));
    assert_eq!(next(&mut beta).await.unwrap(), snapshot(&[("b", "fine")]));
    assert!(!state(&agent.poll().await).has_error);
}

#[tokio::test(start_paused = true)]
async fn malformed_responses_are_discarded() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    let unsent = Response::new(10).cached("employee/ALPHA/a/config", 1, b"one").build();
    let bad_path = Response::new(10).send("employee/ALPHA/config", 1, b"one").build();
    let mut bad_targets = Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build();
    bad_targets.targets = b"{".to_vec();
    let mut unlisted = Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build();
    unlisted.targets = Response::new(10).build().targets;

    agent.poll().await;
    for (response, expected) in [
        (unsent, "neither sent nor cached"),
        (bad_path, "malformed"),
        (bad_targets, "Targets metadata is malformed"),
        (unlisted, "missing from the targets metadata"),
    ] {
        agent.respond(Ok(response));
        let request = agent.poll().await;
        let reported = state(&request);
        assert_eq!(reported.targets_version, 0);
        assert!(reported.error.contains(expected), "{}", reported.error);
        assert_unpublished(&mut alpha).await;
    }
    // The last poll is still outstanding from the loop above.
    agent.respond(Ok(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build()));
    next(&mut alpha).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn colliding_configuration_ids_are_rejected_as_a_set() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/x/config", 1, b"first")
                .send("employee/ALPHA/x/backup", 4, b"second")
                .send("employee/ALPHA/y/config", 1, b"other")
                .build(),
        )
        .await;
    assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("y", "other")]));

    let request = agent.poll().await;
    assert_eq!(
        rows(&request),
        [
            ("ALPHA", "x", 4, ERROR, crate::repository::COLLISION),
            ("ALPHA", "y", 1, ACKNOWLEDGED, ""),
        ]
    );
    assert_eq!(cached(&request).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn expiry_withdraws_every_configuration() {
    let (client, worker, mut agent) = client();
    let mut optional = client.subscribe::<Recorder>().unwrap();
    let mut required = client.subscribe::<Required>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .send("employee/BETA/b/config", 1, b"two")
                .build(),
        )
        .await;
    next(&mut optional).await.unwrap();
    next(&mut required).await.unwrap();

    agent.exchange(Response::new(11).expired().build()).await;
    assert_eq!(next(&mut optional).await.unwrap(), snapshot(&[]));
    assert_eq!(next(&mut required).await.unwrap_err(), "Nothing assigned.");
    assert_eq!(*required.current().unwrap(), snapshot(&[("b", "two")]));

    let request = agent.poll().await;
    assert_eq!(state(&request).targets_version, 11);
    assert!(rows(&request).is_empty());
    assert!(request.cached_target_files.is_empty());
}

#[tokio::test(start_paused = true)]
async fn empty_files_are_delivered_but_not_advertised_as_cached() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"").build())
        .await;
    assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("a", "")]));

    let request = agent.poll().await;
    assert!(request.cached_target_files.is_empty());
    assert_eq!(rows(&request), [("ALPHA", "a", 1, ACKNOWLEDGED, "")]);
}

#[tokio::test(start_paused = true)]
async fn a_late_subscription_polls_at_once_and_asks_for_the_full_assignment() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build())
        .await;
    next(&mut alpha).await.unwrap();

    let subscribed_at = Instant::now();
    let mut beta = client.subscribe::<Recorder<Beta>>().unwrap();
    let request = agent.poll().await;
    assert_eq!(Instant::now(), subscribed_at);
    assert_eq!(products(&request), ["ALPHA", "BETA"]);
    assert_eq!(state(&request).targets_version, 0);
    assert_eq!(cached(&request).len(), 1);

    // The Agent sends only what is not cached.
    agent.respond(Ok(Response::new(10)
        .cached("employee/ALPHA/a/config", 1, b"one")
        .send("employee/BETA/b/config", 1, b"two")
        .build()));
    assert_eq!(next(&mut beta).await.unwrap(), snapshot(&[("b", "two")]));
    assert_unpublished(&mut alpha).await;

    assert_eq!(state(&agent.poll().await).targets_version, 10);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_last_clone_stops_requesting_and_reporting_the_product() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    let beta = client.subscribe::<Recorder<Beta>>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .send("employee/BETA/b/config", 1, b"two")
                .build(),
        )
        .await;
    next(&mut alpha).await.unwrap();

    // Dropping does not wake the worker; the next scheduled poll notices.
    let dropped_at = Instant::now();
    drop(beta);
    let request = agent.poll().await;
    assert_eq!(Instant::now() - dropped_at, Duration::from_secs(5));
    assert_eq!(products(&request), ["ALPHA"]);
    assert_eq!(state(&request).targets_version, 10);
    assert_eq!(rows(&request), [("ALPHA", "a", 1, ACKNOWLEDGED, "")]);
    assert_eq!(
        cached(&request).into_keys().collect::<Vec<_>>(),
        ["employee/ALPHA/a/config"]
    );
}

#[tokio::test(start_paused = true)]
async fn resubscribing_before_the_worker_notices_delivers_to_the_new_subscription() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent
        .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build())
        .await;
    next(&mut alpha).await.unwrap();
    drop(alpha);

    let mut alpha = client.subscribe::<Recorder>().unwrap();
    assert!(alpha.current().is_none());
    let request = agent.poll().await;
    assert_eq!(state(&request).targets_version, 0);
    assert_eq!(rows(&request), [("ALPHA", "a", 1, UNACKNOWLEDGED, "")]);

    // The Agent sees the file cached and does not resend it; the new subscription is built from the cache.
    agent.respond(Ok(Response::new(10)
        .cached("employee/ALPHA/a/config", 1, b"one")
        .build()));
    assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("a", "one")]));
}

#[tokio::test(start_paused = true)]
async fn a_restart_keeps_subscriptions_and_discards_protocol_state() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();

    let (coordinator, shutdown) = ShutdownHandle::paired();
    let first_run = tokio::spawn(worker.initialize(shutdown).await.unwrap());
    let first = agent
        .exchange(
            Response::new(10)
                .send("employee/ALPHA/a/config", 1, b"one")
                .root(2)
                .build(),
        )
        .await;
    next(&mut alpha).await.unwrap();
    let accepted = alpha.current().unwrap();
    agent.poll().await;
    coordinator.shutdown();
    first_run.await.unwrap().unwrap();

    let (_coordinator, shutdown) = ShutdownHandle::paired();
    tokio::spawn(worker.initialize(shutdown).await.unwrap());
    let request = agent.poll().await;
    let wire = request.client.as_ref().unwrap();
    assert_eq!(wire.id, first.client.as_ref().unwrap().id);
    assert_eq!((state(&request).root_version, state(&request).targets_version), (1, 0));
    assert!(rows(&request).is_empty() && request.cached_target_files.is_empty());

    // Until the restarted worker decodes again, the snapshot from before the restart is still current.
    assert!(Arc::ptr_eq(&accepted, &alpha.current().unwrap()));

    // The restarted worker decodes again, publishing a snapshot equal to the one already held.
    agent.respond(Ok(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build()));
    assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("a", "one")]));
}

#[tokio::test(start_paused = true)]
async fn retries_every_second_until_the_first_success() {
    let (client, worker, mut agent) = client();
    let _alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    let started = Instant::now();
    agent.poll().await;
    assert_eq!(Instant::now(), started);
    for attempt in 1..=3 {
        agent.respond(Err(FetchError::Rpc(generic_error!("connection refused"))));
        let request = agent.poll().await;
        assert_eq!(Instant::now() - started, Duration::from_secs(attempt));
        assert!(state(&request).error.contains("connection refused"));
    }

    agent.respond(Ok(Response::new(10).build()));
    let before = Instant::now();
    let request = agent.poll().await;
    assert_eq!(Instant::now() - before, Duration::from_secs(5));
    assert!(!state(&request).has_error);
}

#[tokio::test(start_paused = true)]
async fn backs_off_after_failures_and_resets_on_success() {
    let config = RcClientConfiguration {
        max_backoff: Duration::from_secs(30),
        ..test_settings()
    };
    let (client, worker, mut agent) = client_with(config);
    let _alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent.exchange(Response::new(10).build()).await;
    agent.poll().await;
    // Jitter makes each wait fall between half and all of the doubled wait, capped at the maximum.
    for (lower, upper) in [(5, 10), (10, 20), (15, 30), (30, 30), (30, 30)] {
        agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
        let before = Instant::now();
        agent.poll().await;
        let waited = Instant::now() - before;
        assert!(
            (Duration::from_secs(lower)..=Duration::from_secs(upper)).contains(&waited),
            "waited {waited:?}"
        );
    }

    agent.respond(Ok(ClientGetConfigsResponse::default()));
    let before = Instant::now();
    agent.poll().await;
    assert_eq!(Instant::now() - before, Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn remote_configuration_disabled_on_the_agent_is_checked_at_the_maximum_backoff() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    agent.poll().await;
    for _ in 0..3 {
        agent.respond(Err(FetchError::Unimplemented(generic_error!("unimplemented"))));
        let before = Instant::now();
        let request = agent.poll().await;
        assert_eq!(Instant::now() - before, Duration::from_secs(90));
        assert!(!state(&request).has_error);
    }

    // Enabling it on the Agent resumes normal polling without intervention.
    agent.respond(Ok(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build()));
    next(&mut alpha).await.unwrap();
    let before = Instant::now();
    agent.poll().await;
    assert_eq!(Instant::now() - before, Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn a_poll_the_agent_never_answers_times_out_and_fails() {
    let (client, worker, mut agent) = client();
    let mut alpha = client.subscribe::<Recorder>().unwrap();
    tokio::spawn(worker.run());

    // Before the first success, a timed-out poll is retried after a second.
    let started = Instant::now();
    agent.poll().await;
    let request = agent.poll().await;
    assert_eq!(Instant::now() - started, Duration::from_secs(31));
    assert!(
        state(&request).error.contains("did not answer within 30s"),
        "{}",
        state(&request).error
    );

    // After a success, it backs off like any other failure.
    agent.respond(Ok(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build()));
    next(&mut alpha).await.unwrap();
    agent.poll().await;
    let unanswered = Instant::now();
    let request = agent.poll().await;
    let waited = Instant::now() - unanswered;
    assert!(
        (Duration::from_secs(35)..=Duration::from_secs(40)).contains(&waited),
        "waited {waited:?}"
    );
    assert!(state(&request).has_error);
    assert_eq!(rows(&request), [("ALPHA", "a", 1, ACKNOWLEDGED, "")]);
}

#[tokio::test(start_paused = true)]
async fn subscribing_before_the_first_poll_does_not_cause_an_extra_poll() {
    let (client, worker, mut agent) = client();
    let _alpha = client.subscribe::<Recorder>().unwrap();
    let _beta = client.subscribe::<Recorder<Beta>>().unwrap();
    tokio::spawn(worker.run());

    agent.exchange(Response::new(10).build()).await;
    agent.assert_quiet(Duration::from_millis(4900)).await;
    agent.poll().await;
}

// The client's own metrics, which are the only visibility into failures that subscribers cannot see.

type MetricsSnapshot = HashMap<CompositeKey, (Option<Unit>, Option<SharedString>, DebugValue)>;

/// Runs an async test body under a fresh recorder and Tokio's paused clock, and returns the metrics it emitted.
///
/// The recorder is thread-local, so the body runs on one thread for its whole duration, across every await, and each
/// test sees only the metrics it drove the worker to emit. The runtime is built inside the recorder's scope rather
/// than by `#[tokio::test]`, which would run the body outside it.
fn recorded<F, Fut>(body: F) -> MetricsSnapshot
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    metrics::with_local_recorder(&recorder, || {
        runtime.block_on(async {
            tokio::time::pause();
            body().await;
        });
    });
    snapshotter.snapshot().into_hashmap()
}

/// Returns the value of a tagged counter, or panics if the worker never incremented it.
#[track_caller]
fn counter(snapshot: &MetricsSnapshot, metric: &'static str, tags: &[(&'static str, &'static str)]) -> u64 {
    let labels: Vec<_> = tags.iter().map(|(name, value)| Label::new(*name, *value)).collect();
    let key = CompositeKey::new(MetricKind::Counter, Key::from_parts(metric, labels));
    match snapshot.get(&key) {
        Some((_, _, DebugValue::Counter(value))) => *value,
        _ => panic!("no {metric} counter with tags {tags:?}"),
    }
}

/// Returns the value of a gauge, or panics if the worker never set it.
#[track_caller]
fn gauge(snapshot: &MetricsSnapshot, metric: &'static str) -> f64 {
    let key = CompositeKey::new(MetricKind::Gauge, Key::from_name(metric));
    match snapshot.get(&key) {
        Some((_, _, DebugValue::Gauge(value))) => value.into_inner(),
        _ => panic!("no {metric} gauge"),
    }
}

#[test]
fn counts_each_poll_outcome() {
    let snapshot = recorded(|| async {
        let (client, worker, mut agent) = client();
        let mut alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        agent
            .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"one").build())
            .await;
        next(&mut alpha).await.unwrap();
        agent.exchange(Response::new(11).expired().build()).await;
        agent.poll().await;
        agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
        agent.poll().await;
        agent.respond(Err(FetchError::Unimplemented(generic_error!("unimplemented"))));
        agent
            .exchange(
                Response::new(12)
                    .tampered("employee/ALPHA/a/config", 2, b"two", b"tw0")
                    .build(),
            )
            .await;
        // The unanswered poll confirms the previous response was applied, so the last outcome is counted.
        agent.poll().await;
    });

    for (outcome, count) in [
        ("ok", 1),
        ("expired", 1),
        ("rpc_error", 1),
        ("unimplemented", 1),
        ("invalid_response", 1),
    ] {
        assert_eq!(
            counter(&snapshot, "remote_config_polls_total", &[("outcome", outcome)]),
            count,
            "polls with outcome {outcome}"
        );
    }
}

#[test]
fn counts_rejections_by_stage_and_published_snapshots() {
    let snapshot = recorded(|| async {
        let (client, worker, mut agent) = client();
        let mut alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        // A decode rejection, and a build failure that rejects the two configurations that decoded.
        agent
            .exchange(
                Response::new(10)
                    .send("employee/ALPHA/a/config", 1, b"bad")
                    .send("employee/ALPHA/b/config", 1, b"unbuildable")
                    .send("employee/ALPHA/c/config", 1, b"one")
                    .build(),
            )
            .await;
        assert_eq!(next(&mut alpha).await.unwrap_err(), "Cannot build.");

        // A panic rejects the whole assignment, including the configuration that had already decoded.
        agent
            .exchange(
                Response::new(11)
                    .send("employee/ALPHA/a/config", 1, b"one")
                    .send("employee/ALPHA/b/config", 1, b"panic")
                    .build(),
            )
            .await;
        assert_unpublished(&mut alpha).await;

        // A collision rejects both colliding configurations as one, and the snapshot still publishes from the rest.
        agent
            .exchange(
                Response::new(12)
                    .send("employee/ALPHA/x/config", 1, b"first")
                    .send("employee/ALPHA/x/backup", 4, b"second")
                    .send("employee/ALPHA/y/config", 1, b"other")
                    .build(),
            )
            .await;
        assert_eq!(next(&mut alpha).await.unwrap(), snapshot(&[("y", "other")]));

        // A version bump that changes no contents decodes nothing, so nothing is counted again.
        agent
            .exchange(
                Response::new(13)
                    .cached("employee/ALPHA/x/config", 2, b"first")
                    .cached("employee/ALPHA/x/backup", 5, b"second")
                    .cached("employee/ALPHA/y/config", 2, b"other")
                    .build(),
            )
            .await;
        assert_unpublished(&mut alpha).await;
        agent.poll().await;
    });

    let rejections = |stage: &'static str| {
        counter(
            &snapshot,
            "remote_config_configurations_rejected_total",
            &[("product", "ALPHA"), ("stage", stage)],
        )
    };
    assert_eq!(rejections("decode"), 1);
    assert_eq!(rejections("build"), 2);
    assert_eq!(rejections("panic"), 2);
    assert_eq!(rejections("collision"), 1);
    assert_eq!(
        counter(
            &snapshot,
            "remote_config_snapshots_published_total",
            &[("product", "ALPHA")]
        ),
        1
    );
}

#[test]
fn counts_a_panic_over_an_empty_assignment() {
    let snapshot = recorded(|| async {
        let (client, worker, mut agent) = client();
        let mut beta = client.subscribe::<PanicsWhenEmpty>().unwrap();
        tokio::spawn(worker.run());

        agent.exchange(Response::new(10).build()).await;
        assert_unpublished(&mut beta).await;
        agent.poll().await;
    });

    assert_eq!(
        counter(
            &snapshot,
            "remote_config_configurations_rejected_total",
            &[("product", "BETA"), ("stage", "panic")],
        ),
        1
    );
}

#[test]
fn tracks_time_since_the_last_successful_poll() {
    let snapshot = recorded(|| async {
        let (client, worker, mut agent) = client();
        let _alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        // Two failures count from when the worker started, because there is no success to count from yet.
        agent.poll().await;
        agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
        agent.poll().await;
        agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
        agent.poll().await;

        // A success resets the count, and the failure five seconds later measures from it.
        agent.respond(Ok(ClientGetConfigsResponse::default()));
        agent.poll().await;
        agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
        agent.poll().await;
    });

    assert_eq!(
        gauge(&snapshot, "remote_config_seconds_since_last_successful_poll"),
        5.0
    );
}

// The client's own logs, which carry the detail its metrics cannot.

/// Records the level and message of every event this crate logs.
#[derive(Clone, Default)]
struct LogRecorder(Arc<Mutex<Vec<(Level, String)>>>);

impl<S: Subscriber> Layer<S> for LogRecorder {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !event.metadata().target().starts_with(env!("CARGO_CRATE_NAME")) {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push((*event.metadata().level(), message.0));
    }
}

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Runs an async test body under a fresh subscriber and Tokio's paused clock, and returns what the crate logged.
///
/// Like [`recorded`], the subscriber is thread-local, so the body runs on one thread for its whole duration.
fn logged<F, Fut>(body: F) -> Vec<(Level, String)>
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    let recorder = LogRecorder::default();
    let subscriber = tracing_subscriber::registry().with(recorder.clone());
    // Tracing caches whether each callsite is enabled for the whole process. While only one dispatcher exists, it
    // decides from whichever thread first reaches the callsite, so a concurrent test on another thread would disable
    // this crate's callsites for this subscriber. A second dispatcher makes it consult every dispatcher instead.
    let _peer = Dispatch::new(NoSubscriber::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            tokio::time::pause();
            body().await;
        });
    });
    let logs = mem::take(&mut *recorder.0.lock().unwrap());
    logs
}

/// Returns the messages logged at `level`, in order.
fn at(logs: &[(Level, String)], level: Level) -> Vec<&str> {
    logs.iter()
        .filter(|(logged, _)| *logged == level)
        .map(|(_, message)| message.as_str())
        .collect()
}

const RPC_FAILED: &str = "Failed to poll the Agent for Remote Configuration.";

#[test]
fn an_rpc_error_after_an_invalid_response_still_warns() {
    let logs = logged(|| async {
        let (client, worker, mut agent) = client();
        let _alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        agent.exchange(Response::new(10).build()).await;
        let mut invalid = Response::new(11).build();
        invalid.targets = b"{".to_vec();
        agent.exchange(invalid).await;
        agent.poll().await;
        agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
        agent.poll().await;
    });

    assert_eq!(
        at(&logs, Level::ERROR),
        ["Discarded an invalid Remote Configuration response."]
    );
    assert_eq!(at(&logs, Level::WARN), [RPC_FAILED]);
    assert!(!at(&logs, Level::DEBUG).contains(&RPC_FAILED));
}

#[test]
fn logs_a_rejection_once_per_change_of_inputs() {
    let logs = logged(|| async {
        let (client, worker, mut agent) = client();
        let _alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        agent
            .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"bad").build())
            .await;
        // Neither a version bump with unchanged contents nor an empty response decodes, so neither logs.
        agent
            .exchange(Response::new(11).cached("employee/ALPHA/a/config", 2, b"bad").build())
            .await;
        agent.exchange(ClientGetConfigsResponse::default()).await;
        agent
            .exchange(
                Response::new(12)
                    .send("employee/ALPHA/a/config", 3, b"unbuildable")
                    .build(),
            )
            .await;
        agent
            .exchange(
                Response::new(13)
                    .cached("employee/ALPHA/a/config", 4, b"unbuildable")
                    .build(),
            )
            .await;
        agent.poll().await;
    });

    assert_eq!(
        at(&logs, Level::WARN),
        ["Rejected a configuration.", "Rejected a configuration snapshot."]
    );
}

#[test]
fn logs_rpc_errors_at_warn_then_debug_and_their_recovery_at_info() {
    let logs = logged(|| async {
        let (client, worker, mut agent) = client();
        let _alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        agent.exchange(Response::new(10).build()).await;
        for _ in 0..2 {
            for _ in 0..3 {
                agent.poll().await;
                agent.respond(Err(FetchError::Rpc(generic_error!("unavailable"))));
            }
            agent.exchange(ClientGetConfigsResponse::default()).await;
        }
        agent.poll().await;
    });

    assert_eq!(at(&logs, Level::WARN), [RPC_FAILED, RPC_FAILED]);
    let repeats = at(&logs, Level::DEBUG)
        .into_iter()
        .filter(|message| *message == RPC_FAILED)
        .count();
    assert_eq!(repeats, 4);
    assert_eq!(
        at(&logs, Level::INFO),
        [
            "Polling the Agent for Remote Configuration recovered.",
            "Polling the Agent for Remote Configuration recovered."
        ]
    );
}

#[test]
fn logs_unimplemented_and_expiry_when_entered_and_cleared() {
    let logs = logged(|| async {
        let (client, worker, mut agent) = client();
        let _alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        for _ in 0..3 {
            agent.poll().await;
            agent.respond(Err(FetchError::Unimplemented(generic_error!("unimplemented"))));
        }
        agent.exchange(Response::new(10).build()).await;
        agent.exchange(Response::new(11).expired().build()).await;
        agent.exchange(Response::new(12).expired().build()).await;
        agent.exchange(Response::new(13).build()).await;
        agent.poll().await;
    });

    assert_eq!(
        at(&logs, Level::INFO),
        [
            "Remote Configuration is not enabled on the Agent; checking again periodically.",
            "Remote Configuration is enabled on the Agent; polling resumed.",
            "The Agent's Remote Configuration is no longer expired.",
        ]
    );
    assert_eq!(
        at(&logs, Level::WARN),
        ["The Agent reports its Remote Configuration as expired; configurations are withdrawn until it recovers."]
    );
}

#[test]
fn logs_integrity_failures_and_panics_as_errors_and_collisions_as_warnings() {
    let logs = logged(|| async {
        let (client, worker, mut agent) = client();
        let mut alpha = client.subscribe::<Recorder>().unwrap();
        tokio::spawn(worker.run());

        agent
            .exchange(
                Response::new(10)
                    .tampered("employee/ALPHA/a/config", 1, b"one", b"on3")
                    .build(),
            )
            .await;
        agent
            .exchange(Response::new(10).send("employee/ALPHA/a/config", 1, b"panic").build())
            .await;
        assert_unpublished(&mut alpha).await;
        agent
            .exchange(
                Response::new(11)
                    .send("employee/ALPHA/x/config", 1, b"first")
                    .send("employee/ALPHA/x/backup", 1, b"second")
                    .build(),
            )
            .await;
        agent.poll().await;
    });

    assert_eq!(
        at(&logs, Level::ERROR),
        [
            "Discarded an invalid Remote Configuration response.",
            "Product decoder panicked; rejected its configuration snapshot.",
        ]
    );
    assert_eq!(
        at(&logs, Level::WARN),
        ["Rejected configurations that share a configuration ID."]
    );
}
