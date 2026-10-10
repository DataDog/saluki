//! The stub worker against an in-process fake stub server, one test per rule of the stub protocol's poll.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use agent_data_plane_config::defaults::DEFAULT_ARTIFACT_CHANNEL_POLL_INTERVAL;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode as AxumStatus},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use saluki_metric_plan::channel::{
    artifact_channel, ApplyState, ArtifactConfigId, ArtifactFeed, ArtifactProduct, InstallerEnd,
};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle, time::Instant};

use super::*;

/// The longest a test waits for the worker or the fake stub.
const PATIENCE: Duration = Duration::from_secs(10);

/// The poll interval the worker runs with in these tests.
const INTERVAL: Duration = Duration::from_millis(20);

fn client() -> ClientInfo {
    ClientInfo {
        id: "a1".to_string(),
        version: "1.8.0".to_string(),
    }
}

fn config(payload: &str) -> Value {
    json!({
        "id": "demo", "version": 7, "sha256": ArtifactConfig::transport_hash(payload), "payload": payload
    })
}

fn answer(configs: Vec<Value>) -> Value {
    json!({"protocol_version": 1, "products": {"policy": configs}})
}

fn snapshot(answer: Value) -> ArtifactSnapshot {
    serde_json::from_value::<PollResponse>(answer)
        .expect("the answer is readable")
        .into_policy_snapshot()
}

/// How the fake stub answers a poll.
#[derive(Clone, Debug)]
enum Answer {
    /// `200 OK` with this JSON body.
    Json(Value),

    /// This status, with this body.
    Status(u16, String),

    /// `200 OK` with exactly these bytes as the body.
    Raw(Bytes),

    /// No answer: the request waits until the connection closes.
    Hang,

    /// `200 OK` and its headers, and then a body that never ends.
    Stall,
}

/// A poll the fake stub received.
#[derive(Clone, Debug)]
struct Poll {
    at: Instant,
    content_type: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct StubState {
    answer: Arc<Mutex<Answer>>,
    polls: mpsc::UnboundedSender<Poll>,
}

async fn handle_poll(State(state): State<StubState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let poll = Poll {
        at: Instant::now(),
        content_type: headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    let _ = state.polls.send(poll);
    let answer = state.answer.lock().unwrap().clone();
    match answer {
        Answer::Json(body) => (AxumStatus::OK, body.to_string()).into_response(),
        Answer::Status(status, body) => (AxumStatus::from_u16(status).unwrap(), body).into_response(),
        Answer::Raw(body) => (AxumStatus::OK, body).into_response(),
        Answer::Hang => std::future::pending().await,
        Answer::Stall => {
            let body = axum::body::Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
            (AxumStatus::OK, body).into_response()
        }
    }
}

/// An in-process stub server, which records every poll and answers as told.
struct FakeStub {
    address: SocketAddr,
    answer: Arc<Mutex<Answer>>,
    polls: mpsc::UnboundedReceiver<Poll>,
    server: JoinHandle<()>,
}

impl FakeStub {
    /// Starts a stub on a free loopback port, answering `answer`.
    async fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::serve(listener, answer)
    }

    /// Starts a stub on the given listener, answering `answer`.
    fn serve(listener: TcpListener, answer: Answer) -> Self {
        Self::serve_under(listener, "", answer)
    }

    /// Starts a stub on the given listener, answering `answer` to polls under the path `prefix`.
    fn serve_under(listener: TcpListener, prefix: &str, answer: Answer) -> Self {
        let address = listener.local_addr().unwrap();
        let answer = Arc::new(Mutex::new(answer));
        let (polls_tx, polls) = mpsc::unbounded_channel();
        let state = StubState {
            answer: Arc::clone(&answer),
            polls: polls_tx,
        };
        let router = Router::new()
            .route(&format!("{}{}", prefix, POLL_PATH), post(handle_poll))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            address,
            answer,
            polls,
            server,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    /// Waits for the next poll.
    async fn next_poll(&mut self) -> Poll {
        tokio::time::timeout(PATIENCE, self.polls.recv())
            .await
            .expect("the worker polls")
            .expect("the stub runs")
    }

    /// Forgets the polls recorded so far.
    fn drain(&mut self) {
        while self.polls.try_recv().is_ok() {}
    }

    /// Waits until `count` more polls have arrived, and returns the last.
    async fn polls(&mut self, count: usize) -> Poll {
        let mut last = self.next_poll().await;
        for _ in 1..count {
            last = self.next_poll().await;
        }
        last
    }
}

impl Drop for FakeStub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A running stub worker, and the installer's end of its channel.
struct RunningWorker {
    installer: InstallerEnd,
    coordinator: Option<saluki_common::sync::shutdown::ShutdownCoordinator>,
    task: JoinHandle<()>,
}

impl RunningWorker {
    /// Starts a worker polling `url` every [`INTERVAL`].
    async fn start(url: &str) -> Self {
        let config = StubChannelConfig::new(url, INTERVAL).expect("the URL is a loopback address");
        Self::with_config(config).await
    }

    /// Starts a worker polling `url` every [`INTERVAL`], abandoning polls that take longer than `poll_timeout`.
    async fn with_poll_timeout(url: &str, poll_timeout: Duration) -> Self {
        let config = StubChannelConfig::new(url, INTERVAL)
            .expect("the URL is a loopback address")
            .with_poll_timeout(poll_timeout);
        Self::with_config(config).await
    }

    async fn with_config(config: StubChannelConfig) -> Self {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let (transport, installer) = artifact_channel();
        let worker = StubChannelWorker::new(config, client(), transport);
        let (coordinator, shutdown) = ShutdownHandle::paired();
        let run = match worker.initialize(shutdown).await {
            Ok(run) => run,
            Err(error) => panic!("the worker initializes: {}", error),
        };
        let task = tokio::spawn(async move {
            run.await.expect("the worker stops cleanly");
        });
        Self {
            installer,
            coordinator: Some(coordinator),
            task,
        }
    }

    fn feed(&mut self) -> &mut ArtifactFeed {
        &mut self.installer.feed
    }

    /// Waits for the next snapshot the worker publishes.
    async fn next_snapshot(&mut self) -> Arc<ArtifactSnapshot> {
        tokio::time::timeout(PATIENCE, self.feed().changed())
            .await
            .expect("the worker publishes a snapshot")
            .expect("the worker runs")
    }

    /// Stops the worker, and waits until it has stopped.
    async fn stop(mut self) {
        if let Some(coordinator) = self.coordinator.take() {
            coordinator.shutdown();
        }
        tokio::time::timeout(PATIENCE, &mut self.task)
            .await
            .expect("the worker stops at shutdown")
            .expect("the worker does not panic");
    }
}

#[test]
fn stub_urls_must_name_a_loopback_address_and_the_poll_path_is_appended_to_theirs() {
    for (url, polled) in [
        ("http://127.0.0.1:17100", "http://127.0.0.1:17100/v1/artifacts/poll"),
        ("http://[::1]:17100", "http://[::1]:17100/v1/artifacts/poll"),
        ("http://localhost:17100", "http://localhost:17100/v1/artifacts/poll"),
        ("http://127.0.0.2", "http://127.0.0.2/v1/artifacts/poll"),
        ("http://127.0.0.1:17100/", "http://127.0.0.1:17100/v1/artifacts/poll"),
        (
            "http://127.0.0.1:17100/stub",
            "http://127.0.0.1:17100/stub/v1/artifacts/poll",
        ),
        (
            "http://127.0.0.1:17100/stub/",
            "http://127.0.0.1:17100/stub/v1/artifacts/poll",
        ),
        (
            "http://127.0.0.1:17100/a/b",
            "http://127.0.0.1:17100/a/b/v1/artifacts/poll",
        ),
    ] {
        let config = StubChannelConfig::new(url, DEFAULT_ARTIFACT_CHANNEL_POLL_INTERVAL).unwrap();
        assert_eq!(
            config.stub_url().to_string().trim_end_matches('/'),
            url.trim_end_matches('/')
        );
        assert_eq!(config.poll_url().to_string(), polled, "{}", url);
        assert_eq!(config.poll_interval(), DEFAULT_ARTIFACT_CHANNEL_POLL_INTERVAL);
    }
    let refused = [
        ("http://10.0.0.1:17100", "must name a loopback address"),
        ("http://stub.example.com", "must name a loopback address"),
        ("https://127.0.0.1:17100", "must be an `http://` URL"),
        ("127.0.0.1:17100", "must be an `http://` URL"),
        ("http://[::1", "is not a valid URL"),
        ("http://127.0.0.1:17100?token=1", "must not have a query"),
        ("http://127.0.0.1:17100/stub?token=1", "must not have a query"),
    ];
    for (url, expected) in refused {
        let error = StubChannelConfig::new(url, DEFAULT_ARTIFACT_CHANNEL_POLL_INTERVAL)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{}: {}", url, error);
        assert!(error.contains("artifact_channel.stub_url"), "{}", error);
    }
    let error = StubChannelConfig::new("http://127.0.0.1:1", Duration::ZERO)
        .unwrap_err()
        .to_string();
    assert!(error.contains("artifact_channel.poll_interval"), "{}", error);
}

#[test]
fn the_channel_is_configured_only_when_a_stub_url_is_set() {
    // Unset: no channel.
    assert_eq!(
        StubChannelConfig::from_configuration(&ArtifactChannel::default()).unwrap(),
        None
    );

    let settings = ArtifactChannel {
        stub_url: Some("http://127.0.0.1:17100".to_string()),
        poll_interval: Duration::from_millis(250),
    };
    let config = StubChannelConfig::from_configuration(&settings)
        .unwrap()
        .expect("a stub URL configures the channel");
    assert_eq!(
        config.poll_url().to_string(),
        "http://127.0.0.1:17100/v1/artifacts/poll"
    );
    assert_eq!(config.poll_interval(), Duration::from_millis(250));

    // A URL that is not a loopback address fails, naming the key and how to fix it.
    let remote = ArtifactChannel {
        stub_url: Some("http://192.0.2.1:17100".to_string()),
        ..ArtifactChannel::default()
    };
    let error = StubChannelConfig::from_configuration(&remote).unwrap_err().to_string();
    assert!(
        error.contains("`experimental.programmable_metrics.artifact_channel.stub_url` must name a loopback address"),
        "{}",
        error
    );
}

#[test]
fn polls_are_written_as_the_stub_protocol_says() {
    let states = ApplyStates::default();
    let poll = serde_json::to_value(PollRequest::new(&client(), &states)).unwrap();
    assert_eq!(
        poll,
        json!({
            "protocol_version": 1,
            "client": {"id": "a1", "version": "1.8.0"},
            "products": ["policy"],
            "config_states": [],
            "status": {
                "default_program_hash": null, "revision_high_water": null, "active_generation": null,
                "active_plan_hash": null, "active_artifact": null, "stale_base": false, "stale_review": false,
                "degraded": null, "live_generations": 0
            }
        })
    );
}

#[test]
fn a_process_identity_is_random_and_names_the_version() {
    let (first, second) = (ClientInfo::for_process(), ClientInfo::for_process());
    assert_ne!(first.id, second.id);
    assert_eq!(first.version, saluki_metadata::get_app_details().version().raw());
}

#[test]
fn answers_are_read_as_complete_policy_snapshots() {
    let read = snapshot(json!({
        "protocol_version": 1,
        "products": {
            "policy": [{"id": "demo", "version": 7, "sha256": "00", "payload": "{}", "future": true}],
            // Another product's entries can have any shape: they are not read.
            "resources": [{"name": "endpoint", "spec": {"url": "http://example.com"}}],
            "other": "not a list"
        },
        "future_member": 1
    }));
    assert_eq!(read.product, ArtifactProduct::Policy);
    assert_eq!(read.configs.len(), 1);
    assert_eq!(read.configs[0].id, ArtifactConfigId::new("demo"));
    assert_eq!(read.configs[0].version, 7);
    assert_eq!(read.configs[0].payload, "{}");

    // An empty list, or a missing product, revokes.
    assert!(snapshot(answer(Vec::new())).revokes());
    assert!(snapshot(json!({"protocol_version": 1, "products": {}})).revokes());
    assert!(snapshot(json!({"protocol_version": 1, "products": {"resources": []}})).revokes());

    // An answer without products, such as an error object, is unreadable, which is a transport failure: it never
    // revokes.
    for unreadable in [
        json!({"protocol_version": 1}),
        json!({"protocol_version": 1, "error": "the store is busy"}),
        json!({"protocol_version": 1, "products": null}),
    ] {
        let read = serde_json::from_value::<PollResponse>(unreadable.clone());
        assert!(read.is_err(), "{} was read as {:?}", unreadable, read);
    }

    // A policy member that is not a list of configs makes the answer unreadable too.
    for products in [
        json!({"policy": null}),
        json!({"policy": {"id": "demo"}}),
        json!({"policy": [{"id": "demo", "version": 7, "payload": "{}"}]}),
        json!({"policy": [{"id": "demo", "version": "7", "sha256": "00", "payload": "{}"}]}),
    ] {
        let read = serde_json::from_value::<PollResponse>(json!({"protocol_version": 1, "products": products}));
        let error = read.expect_err("the answer is unreadable").to_string();
        assert!(error.starts_with("product `policy`:"), "{}", error);
    }
}

#[test]
fn the_worker_is_supervised_under_its_own_name() {
    let (transport, _installer) = artifact_channel();
    let config = StubChannelConfig::new("http://127.0.0.1:17100", DEFAULT_ARTIFACT_CHANNEL_POLL_INTERVAL).unwrap();
    let worker = StubChannelWorker::new(config, client(), transport);
    assert_eq!(worker.name(), "programmable-metrics-artifact-channel-stub");
}

#[tokio::test]
async fn the_worker_polls_with_the_latest_acknowledgments_and_publishes_each_new_snapshot() {
    let first = answer(vec![config(r#"{"revision": 1}"#)]);
    let mut stub = FakeStub::start(Answer::Json(first.clone())).await;
    let mut worker = RunningWorker::start(&stub.url()).await;

    // The first poll is sent at once, as JSON, with what the installer has reported so far: nothing.
    let poll = stub.next_poll().await;
    assert_eq!(poll.content_type.as_deref(), Some("application/json"));
    let states = ApplyStates::default();
    assert_eq!(
        poll.body,
        serde_json::to_value(PollRequest::new(&client(), &states)).unwrap()
    );
    assert_eq!(*worker.next_snapshot().await, snapshot(first.clone()));

    // The installer reports an acknowledgment: the next polls carry it.
    let mut reported = ApplyStates::default();
    reported.status.revision_high_water = Some(1);
    reported.config_states.push(ConfigAcknowledgment {
        product: ArtifactProduct::Policy,
        id: ArtifactConfigId::new("demo"),
        version: 7,
        revision: Some(1),
        artifact_hash: None,
        state: ApplyState::Pending,
        generation: None,
        cut_at: None,
        error: None,
    });
    assert!(worker.installer.reporter.report(reported.clone()));
    let expected = serde_json::to_value(PollRequest::new(&client(), &reported)).unwrap();
    let poll = tokio::time::timeout(PATIENCE, async {
        loop {
            let poll = stub.next_poll().await;
            if poll.body == expected {
                return poll;
            }
        }
    })
    .await
    .expect("a poll carries the reported acknowledgment");
    assert_eq!(poll.body["config_states"][0]["state"], "pending");

    // The same answer, poll after poll, is not delivered again.
    stub.polls(3).await;
    assert!(
        tokio::time::timeout(INTERVAL * 5, worker.feed().changed())
            .await
            .is_err(),
        "an unchanged snapshot was delivered again"
    );

    // A new answer is.
    let second = answer(vec![config(r#"{"revision": 2}"#)]);
    stub.answer(Answer::Json(second.clone()));
    assert_eq!(*worker.next_snapshot().await, snapshot(second));
    worker.stop().await;
}

#[tokio::test]
async fn an_empty_or_missing_policy_product_revokes() {
    let installed = answer(vec![config(r#"{"revision": 1}"#)]);
    let mut stub = FakeStub::start(Answer::Json(installed.clone())).await;
    let mut worker = RunningWorker::start(&stub.url()).await;
    assert!(!worker.next_snapshot().await.revokes());

    stub.answer(Answer::Json(answer(Vec::new())));
    assert!(worker.next_snapshot().await.revokes());

    stub.answer(Answer::Json(installed));
    assert!(!worker.next_snapshot().await.revokes());
    stub.answer(Answer::Json(
        json!({"protocol_version": 1, "products": {"resources": []}}),
    ));
    assert!(worker.next_snapshot().await.revokes());
    stub.polls(1).await;
    worker.stop().await;
}

#[tokio::test]
async fn transport_failures_keep_the_current_snapshot_and_the_next_poll_retries() {
    let installed = answer(vec![config(r#"{"revision": 1}"#)]);
    let mut stub = FakeStub::start(Answer::Json(installed.clone())).await;
    let mut worker = RunningWorker::start(&stub.url()).await;
    let current = worker.next_snapshot().await;

    let failures = [
        ("a server error", Answer::Status(500, "boom".to_string())),
        ("not found", Answer::Status(404, String::new())),
        (
            "a refusal",
            Answer::Status(400, r#"{"error": "unsupported"}"#.to_string()),
        ),
        ("a body that is not JSON", Answer::Status(200, "<html>".to_string())),
        (
            "an answer without a protocol version",
            Answer::Json(json!({"products": {}})),
        ),
        (
            "an answer without products",
            Answer::Json(json!({"protocol_version": 1, "error": "the store is busy"})),
        ),
        (
            "another protocol version",
            Answer::Json(json!({"protocol_version": 2, "products": {}})),
        ),
        (
            "a policy config that cannot be read",
            Answer::Json(json!({"protocol_version": 1, "products": {"policy": [{"id": "demo"}]}})),
        ),
        (
            "a policy member that is not a list",
            Answer::Json(json!({"protocol_version": 1, "products": {"policy": null}})),
        ),
    ];
    for (what, failure) in failures {
        stub.answer(failure);
        // Polls continue, and none of them changes the snapshot: in particular, none revokes it.
        stub.polls(3).await;
        assert_eq!(worker.feed().latest(), Some(Arc::clone(&current)), "after {}", what);
        assert!(
            tokio::time::timeout(INTERVAL, worker.feed().changed()).await.is_err(),
            "{} delivered a snapshot",
            what
        );
    }

    // Once the stub answers again, its snapshot is delivered.
    stub.answer(Answer::Json(answer(Vec::new())));
    assert!(worker.next_snapshot().await.revokes());
    worker.stop().await;
}

#[tokio::test]
async fn an_unreachable_stub_is_retried_until_it_answers() {
    // Nothing listens on the stub's port yet.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut worker = RunningWorker::start(&format!("http://{}", address)).await;
    tokio::time::sleep(INTERVAL * 5).await;
    assert_eq!(worker.feed().latest(), None);

    // The stub starts on that port: the next poll reaches it.
    let installed = answer(vec![config(r#"{"revision": 1}"#)]);
    let listener = TcpListener::bind(address).await.unwrap();
    let mut stub = FakeStub::serve(listener, Answer::Json(installed.clone()));
    stub.next_poll().await;
    assert_eq!(*worker.next_snapshot().await, snapshot(installed));
    worker.stop().await;
}

#[tokio::test]
async fn polls_are_spaced_by_the_poll_interval() {
    let mut stub = FakeStub::start(Answer::Json(answer(Vec::new()))).await;
    let started = Instant::now();
    let worker = RunningWorker::start(&stub.url()).await;
    // Poll `k` is due no sooner than `k` intervals after the first, which is sent once the worker runs. When each one
    // arrives depends on how long it takes to send, so only that lower bound holds whatever the load.
    for k in 0..6u32 {
        let at = stub.next_poll().await.at;
        assert!(
            at.duration_since(started) >= INTERVAL * k,
            "poll {} arrived {:?} after the worker started",
            k,
            at.duration_since(started)
        );
    }
    worker.stop().await;
}

#[tokio::test]
async fn the_worker_stops_at_shutdown_even_while_a_poll_waits_for_the_stub() {
    let mut stub = FakeStub::start(Answer::Hang).await;
    let worker = RunningWorker::start(&stub.url()).await;
    stub.next_poll().await;
    // The stub never answers: shutdown still stops the worker at once, not after the poll times out.
    let started = Instant::now();
    worker.stop().await;
    assert!(
        started.elapsed() < POLL_TIMEOUT,
        "the worker waited {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_poll_that_gets_no_complete_answer_within_the_poll_timeout_is_abandoned_and_the_next_poll_retries() {
    const TIMEOUT: Duration = Duration::from_millis(400);
    let installed = answer(vec![config(r#"{"revision": 1}"#)]);
    let mut stub = FakeStub::start(Answer::Json(installed)).await;
    let mut worker = RunningWorker::with_poll_timeout(&stub.url(), TIMEOUT).await;
    let current = worker.next_snapshot().await;

    for (what, stuck) in [
        ("a stub that never answers", Answer::Hang),
        ("a stub that sends its headers and then stalls its body", Answer::Stall),
    ] {
        stub.answer(stuck);
        // Every poll recorded from now on gets the stuck answer, which the stub reads after recording the poll.
        stub.drain();
        let stuck_at = stub.next_poll().await.at;
        // The next poll is sent once the stuck one is abandoned, the poll timeout after it was sent. When it was
        // recorded depends on how long it took to arrive, so the bound leaves room for that.
        let next = stub.next_poll().await;
        assert!(
            next.at.duration_since(stuck_at) >= TIMEOUT / 2,
            "{}: the next poll was sent {:?} after the stuck one",
            what,
            next.at.duration_since(stuck_at)
        );
        // A stuck poll is a transport failure: the current snapshot stays.
        assert_eq!(worker.feed().latest(), Some(Arc::clone(&current)), "{}", what);
    }

    // Once the stub answers again, its snapshot is delivered.
    stub.answer(Answer::Json(answer(Vec::new())));
    assert!(worker.next_snapshot().await.revokes());
    worker.stop().await;
}

#[tokio::test]
async fn an_answer_larger_than_the_answer_limit_is_a_transport_failure() {
    let installed = answer(vec![config(r#"{"revision": 1}"#)]);
    let mut stub = FakeStub::start(Answer::Json(installed)).await;
    let mut worker = RunningWorker::start(&stub.url()).await;
    let current = worker.next_snapshot().await;

    // A revocation, padded with whitespace, which JSON allows, to `size` bytes.
    let padded = |size: usize| {
        let mut body = answer(Vec::new()).to_string().into_bytes();
        body.resize(size, b' ');
        Bytes::from(body)
    };

    // One byte over the limit: unreadable, so it never revokes.
    stub.answer(Answer::Raw(padded(MAX_ANSWER_SIZE + 1)));
    stub.drain();
    stub.polls(3).await;
    assert_eq!(worker.feed().latest(), Some(Arc::clone(&current)));
    assert!(
        tokio::time::timeout(INTERVAL, worker.feed().changed()).await.is_err(),
        "an oversized answer delivered a snapshot"
    );

    // Exactly at the limit: read, so it revokes.
    stub.answer(Answer::Raw(padded(MAX_ANSWER_SIZE)));
    assert!(worker.next_snapshot().await.revokes());
    worker.stop().await;
}

#[tokio::test]
async fn a_stub_served_under_a_path_is_polled_under_that_path() {
    let installed = answer(vec![config(r#"{"revision": 1}"#)]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut stub = FakeStub::serve_under(listener, "/stub", Answer::Json(installed.clone()));
    let mut worker = RunningWorker::start(&format!("{}/stub/", stub.url())).await;
    stub.next_poll().await;
    assert_eq!(*worker.next_snapshot().await, snapshot(installed));
    worker.stop().await;
}
