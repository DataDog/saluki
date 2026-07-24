use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Condvar, Mutex as StdMutex,
};
use std::time::Duration;

use http::{
    header::{AUTHORIZATION, CONTENT_TYPE},
    HeaderValue, Request, StatusCode,
};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Bytes;
use saluki_api::{response::Response, APIHandler};
use saluki_components::transforms::{
    aggregate_context_snapshot_channel_for_test, AggregateContextSnapshotEntry, AggregateContextSnapshotHandle,
    AggregateContextSnapshotPendingResponse, AggregateMetricType,
};
use saluki_context::Context;
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tower::ServiceExt as _;

use super::{
    ContextDumpPublisher, ContextSnapshotCoordinator, DogStatsDContextDumpAPIHandler, FileSystemContextDumpPublisher,
    SnapshotError,
};
use crate::dogstatsd_contexts::{
    artifact::{for_each_record, CONTEXT_DUMP_FILENAME},
    publish_context_dump,
};

const AUTH_TOKEN: &str = "expected-agent-token";
const ROUTE: &str = super::CONTEXT_DUMP_ROUTE;

#[test]
fn constructor_rejects_empty_and_non_visible_http_tokens_without_exposing_them() {
    let invalid_tokens: &[&[u8]] = &[
        b"",
        b"contains space",
        b"line\nbreak",
        b"tab\tvalue",
        b"nul\0value",
        b"\x7f",
        b"\xff",
    ];

    for token in invalid_tokens {
        let error = match DogStatsDContextDumpAPIHandler::new(*token, Vec::new(), PathBuf::from("run")) {
            Ok(_) => panic!("invalid authentication token should be rejected"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        if !token.is_empty() {
            assert!(!message.as_bytes().windows(token.len()).any(|window| window == *token));
        }
    }
}

#[tokio::test]
async fn rejects_missing_duplicate_malformed_and_wrong_authorization_with_an_exact_safe_body() {
    let publisher = Arc::new(NoOpPublisher);
    let handler = test_handler(Vec::new(), PathBuf::from("unused"), publisher);
    let mut duplicate = post_request(None);
    duplicate
        .headers_mut()
        .append(AUTHORIZATION, HeaderValue::from_static("Bearer expected-agent-token"));
    duplicate
        .headers_mut()
        .append(AUTHORIZATION, HeaderValue::from_static("Bearer expected-agent-token"));
    let mut non_utf8 = post_request(None);
    non_utf8.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_bytes(b"Bearer \xff").expect("HTTP headers allow opaque non-ASCII bytes"),
    );
    let cases = [
        post_request(None),
        duplicate,
        non_utf8,
        post_request(Some("Basic expected-agent-token")),
        post_request(Some("Bearer")),
        post_request(Some("Bearersecret")),
        post_request(Some("Bearer\texpected-agent-token")),
        post_request(Some("Bearer expected-agent-token trailing")),
        post_request(Some("Bearer supplied-secret")),
        post_request(Some("Bearer unexpect-agent-token")),
    ];

    for request in cases {
        let response = send(&handler, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = response_body(response).await;
        assert_eq!(body, "Authentication required.");
        assert!(!body.contains(AUTH_TOKEN));
        assert!(!body.contains("supplied-secret"));
    }
}

#[tokio::test]
async fn accepts_bearer_scheme_case_insensitively() {
    for scheme in ["bearer", "BEARER", "BeArEr"] {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        let handler = test_handler(vec![handle], PathBuf::from("unused"), Arc::new(NoOpPublisher));
        let owner = tokio::spawn(async move { responder.respond(Vec::new()).await });

        let response = send(&handler, post_request(Some(&format!("{scheme} {AUTH_TOKEN}")))).await;

        assert_eq!(response.status(), StatusCode::OK);
        owner.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn accepts_only_the_exact_bearer_credential_bytes() {
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let handler = test_handler(vec![handle], PathBuf::from("unused"), Arc::new(NoOpPublisher));
    let owner = tokio::spawn(async move { responder.respond(Vec::new()).await });

    let response = send(&handler, post_request(Some("Bearer expected-agent-token"))).await;

    assert_eq!(response.status(), StatusCode::OK);
    owner.await.unwrap().unwrap();
}

#[tokio::test]
async fn coordinator_reuses_the_one_owner_snapshot_allocation() {
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let owner_snapshot = vec![snapshot_entry("owner.one.first"), snapshot_entry("owner.one.second")];
    let expected = owner_snapshot.clone();
    let original_pointer = owner_snapshot.as_ptr() as usize;
    let original_capacity = owner_snapshot.capacity();
    let owner = tokio::spawn(async move { responder.respond(owner_snapshot).await });
    let coordinator = ContextSnapshotCoordinator::new(vec![handle], Duration::from_secs(1));

    let actual = coordinator.snapshot().await.expect("snapshot should succeed");

    assert_eq!(actual, expected);
    assert_eq!(actual.as_ptr() as usize, original_pointer);
    assert_eq!(actual.capacity(), original_capacity);
    owner.await.unwrap().unwrap();
}

#[tokio::test]
async fn coordinator_requests_all_owners_concurrently_and_flattens_each_snapshot_once_in_owner_order() {
    let (first_handle, mut first_responder) = aggregate_context_snapshot_channel_for_test();
    let (second_handle, mut second_responder) = aggregate_context_snapshot_channel_for_test();
    let first_snapshot = vec![snapshot_entry("owner.one.first"), snapshot_entry("owner.one.second")];
    let second_snapshot = vec![snapshot_entry("owner.two.only")];
    let expected = [first_snapshot.clone(), second_snapshot.clone()].concat();
    let (second_responded_tx, second_responded_rx) = tokio::sync::oneshot::channel();
    let first_owner = tokio::spawn(async move {
        second_responded_rx
            .await
            .expect("second owner should receive its request");
        first_responder.respond(first_snapshot).await
    });
    let second_owner = tokio::spawn(async move {
        let result = second_responder.respond(second_snapshot).await;
        let _ = second_responded_tx.send(());
        result
    });
    let coordinator = ContextSnapshotCoordinator::new(vec![first_handle, second_handle], Duration::from_millis(250));

    let actual = coordinator.snapshot().await.expect("both snapshots should succeed");

    assert_eq!(actual, expected);
    first_owner.await.unwrap().unwrap();
    second_owner.await.unwrap().unwrap();
}

#[tokio::test]
async fn coordinator_reports_no_handles_as_typed_unavailable() {
    let coordinator = ContextSnapshotCoordinator::new(Vec::new(), Duration::from_secs(1));

    let error = coordinator.snapshot().await.expect_err("empty owner set should fail");

    assert_eq!(error, SnapshotError::SnapshotUnavailable);
}

#[tokio::test]
async fn coordinator_reports_a_closed_owner_as_typed_unavailable() {
    let (handle, responder) = aggregate_context_snapshot_channel_for_test();
    drop(responder);
    let coordinator = ContextSnapshotCoordinator::new(vec![handle], Duration::from_secs(1));

    let error = coordinator.snapshot().await.expect_err("closed owner should fail");

    assert_eq!(error, SnapshotError::SnapshotUnavailable);
}

#[tokio::test]
async fn coordinator_reports_elapsed_deadline_as_typed_timeout() {
    let (handle, _responder) = aggregate_context_snapshot_channel_for_test();
    let coordinator = ContextSnapshotCoordinator::new(vec![handle], Duration::from_millis(10));

    let error = coordinator
        .snapshot()
        .await
        .expect_err("unresponsive owner should time out");

    assert_eq!(error, SnapshotError::SnapshotTimedOut);
}

#[tokio::test]
async fn canceled_coordinator_request_does_not_make_a_late_owner_response_panic_or_fail() {
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let coordinator = ContextSnapshotCoordinator::new(vec![handle], Duration::from_secs(1));
    let request = tokio::spawn(async move { coordinator.snapshot().await });
    let pending_response: AggregateContextSnapshotPendingResponse = responder
        .receive()
        .await
        .expect("the owner should accept the snapshot request");

    request.abort();
    let _ = request.await;

    pending_response.respond(vec![snapshot_entry("late.owner.response")]);
}

#[tokio::test]
async fn generated_router_rejects_get_with_method_not_allowed() {
    let handler = test_handler(Vec::new(), PathBuf::from("unused"), Arc::new(NoOpPublisher));
    let request = Request::builder()
        .method("GET")
        .uri(ROUTE)
        .body(Empty::<Bytes>::new())
        .unwrap();

    let response = send(&handler, request).await;

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn unauthorized_post_does_not_request_a_snapshot_or_create_an_artifact() {
    let run_directory = tempfile::tempdir().unwrap();
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let handler = DogStatsDContextDumpAPIHandler::new(AUTH_TOKEN, vec![handle], run_directory.path().to_owned())
        .expect("handler should be valid");

    let response = send(&handler, post_request(None)).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!run_directory.path().join(CONTEXT_DUMP_FILENAME).exists());
    drop(handler);
    assert!(responder.respond(Vec::new()).await.is_err());
}

#[tokio::test]
async fn authorized_router_post_returns_only_the_json_path_to_a_complete_decodable_artifact() {
    let run_directory = tempfile::tempdir().unwrap();
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let snapshot = vec![snapshot_entry("published.first"), snapshot_entry("published.second")];
    let handler = DogStatsDContextDumpAPIHandler::new(AUTH_TOKEN.as_bytes(), vec![handle], run_directory.path())
        .expect("handler should be valid");
    let owner = tokio::spawn(async move { responder.respond(snapshot).await });

    let response = send(&handler, post_request(Some("Bearer expected-agent-token"))).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "application/json");
    assert_eq!(
        response_body(response).await,
        serde_json::to_string(target.to_str().unwrap()).unwrap()
    );
    owner.await.unwrap().unwrap();
    let mut names = Vec::new();
    for_each_record(&target, |record| names.push(record.name)).expect("published artifact should decode");
    assert_eq!(names, ["published.first", "published.second"]);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[tokio::test]
async fn no_handles_and_closed_owner_return_service_unavailable() {
    let no_handles = test_handler(Vec::new(), PathBuf::from("unused"), Arc::new(NoOpPublisher));
    let response = send(&no_handles, authorized_post()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response_body(response).await,
        "DogStatsD context snapshot is unavailable; retry after the aggregate owner is running."
    );

    let (handle, responder) = aggregate_context_snapshot_channel_for_test();
    drop(responder);
    let closed_owner = test_handler(vec![handle], PathBuf::from("unused"), Arc::new(NoOpPublisher));
    let response = send(&closed_owner, authorized_post()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response_body(response).await,
        "DogStatsD context snapshot is unavailable; retry after the aggregate owner is running."
    );
}

#[tokio::test]
async fn owner_stopped_after_accepting_request_returns_service_unavailable() {
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let handler = test_handler(vec![handle], PathBuf::from("unused"), Arc::new(NoOpPublisher));
    let owner = tokio::spawn(async move { responder.stop_after_receiving().await });

    let response = send(&handler, authorized_post()).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response_body(response).await,
        "DogStatsD context snapshot is unavailable; retry after the aggregate owner is running."
    );
    owner.await.unwrap().unwrap();
}

#[tokio::test]
async fn snapshot_deadline_returns_gateway_timeout() {
    let (handle, _responder) = aggregate_context_snapshot_channel_for_test();
    let handler = DogStatsDContextDumpAPIHandler::new_for_test(
        AUTH_TOKEN,
        vec![handle],
        PathBuf::from("unused"),
        Duration::from_millis(10),
        Arc::new(NoOpPublisher),
    )
    .unwrap();

    let response = send(&handler, authorized_post()).await;

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        response_body(response).await,
        "Timed out waiting for the DogStatsD context snapshot; retry the request."
    );
}

#[tokio::test]
async fn empty_and_non_directory_run_paths_reach_publication_and_return_safe_internal_errors() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let non_directory = temporary_directory.path().join("not-a-directory");
    fs::write(&non_directory, b"fixture").unwrap();

    for run_path in [PathBuf::new(), non_directory] {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        let handler = DogStatsDContextDumpAPIHandler::new(AUTH_TOKEN, vec![handle], run_path).unwrap();
        let owner = tokio::spawn(async move { responder.respond(Vec::new()).await });

        let response = send(&handler, authorized_post()).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response_body(response).await;
        assert_eq!(
            body,
            "Failed to publish DogStatsD context dump; check the configured run path and permissions."
        );
        assert!(!body.contains(AUTH_TOKEN));
        owner.await.unwrap().unwrap();
    }
    assert_eq!(
        directory_entries(temporary_directory.path()),
        vec!["not-a-directory".to_owned()]
    );
}

#[tokio::test]
async fn publication_failure_and_blocking_task_panic_return_safe_internal_errors() {
    for publisher in [
        Arc::new(FailingPublisher) as Arc<dyn ContextDumpPublisher>,
        Arc::new(PanickingPublisher) as Arc<dyn ContextDumpPublisher>,
    ] {
        let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
        let handler = test_handler(vec![handle], PathBuf::from("unused"), publisher);
        let owner = tokio::spawn(async move { responder.respond(Vec::new()).await });

        let response = send(&handler, authorized_post()).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response_body(response).await;
        assert!(
            body == "Failed to publish DogStatsD context dump; check the configured run path and permissions."
                || body == "DogStatsD context dump publication did not complete; retry the request."
        );
        assert!(!body.contains("injected"));
        assert!(!body.contains(AUTH_TOKEN));
        owner.await.unwrap().unwrap();
    }
}

#[test]
fn blocking_publisher_release_guard_releases_and_notifies_on_drop() {
    let release = Arc::new((StdMutex::new(false), Condvar::new()));
    let (waiting_tx, waiting_rx) = mpsc::sync_channel(1);

    std::thread::scope(|scope| {
        let waiter_release = release.clone();
        let waiter = scope.spawn(move || {
            let (released, wake) = &*waiter_release;
            let released = released.lock().unwrap();
            waiting_tx.send(()).unwrap();
            let (released, timeout) = wake
                .wait_timeout_while(released, Duration::from_secs(1), |released| !*released)
                .unwrap();
            assert!(!timeout.timed_out(), "release guard should notify the waiter");
            assert!(*released);
        });
        waiting_rx.recv().unwrap();

        let release_guard = BlockingPublisherReleaseGuard::new(release);
        drop(release_guard);
        waiter.join().unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorized_requests_serialize_fixed_path_publisher_executions() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let release = Arc::new((StdMutex::new(false), Condvar::new()));
    let publisher = Arc::new(BlockingFirstPublisher {
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        first_started: started_tx,
        release: release.clone(),
    });
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let handler = test_handler(vec![handle], PathBuf::from("unused"), publisher.clone());
    let owner = tokio::spawn(async move {
        responder.respond(Vec::new()).await.unwrap();
        responder.respond(Vec::new()).await.unwrap();
    });

    let first_handler = handler.clone();
    let first = tokio::spawn(async move { send(&first_handler, authorized_post()).await });
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .unwrap()
        .expect("first publisher should start");
    let release_guard = BlockingPublisherReleaseGuard::new(release);
    let second_handler = handler.clone();
    let second = tokio::spawn(async move { send(&second_handler, authorized_post()).await });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(publisher.calls.load(Ordering::SeqCst), 1);
    drop(release_guard);

    assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    assert_eq!(second.await.unwrap().status(), StatusCode::OK);
    owner.await.unwrap();
    assert_eq!(publisher.calls.load(Ordering::SeqCst), 2);
    assert_eq!(publisher.max_active.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceling_after_spawn_blocking_starts_still_completes_atomic_publication() {
    let run_directory = tempfile::tempdir().unwrap();
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    fs::write(&target, b"original canonical artifact").unwrap();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let release = Arc::new((StdMutex::new(false), Condvar::new()));
    let completed = Arc::new(AtomicBool::new(false));
    let publisher = Arc::new(BlockingRealPublisher {
        started: started_tx,
        release: release.clone(),
        completed: completed.clone(),
    });
    let (handle, mut responder) = aggregate_context_snapshot_channel_for_test();
    let handler = test_handler(vec![handle], run_directory.path().to_owned(), publisher);
    let owner = tokio::spawn(async move { responder.respond(vec![snapshot_entry("after.cancel")]).await });
    let request_handler = handler.clone();
    let request = tokio::spawn(async move { send(&request_handler, authorized_post()).await });
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(1)))
        .await
        .unwrap()
        .expect("publisher should start");
    let release_guard = BlockingPublisherReleaseGuard::new(release);

    request.abort();
    let _ = request.await;
    assert_eq!(fs::read(&target).unwrap(), b"original canonical artifact");
    drop(release_guard);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !completed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("detached publication should complete");

    owner.await.unwrap().unwrap();
    let mut names = Vec::new();
    for_each_record(&target, |record| names.push(record.name)).expect("canonical artifact should be complete");
    assert_eq!(names, ["after.cancel"]);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

fn test_handler(
    handles: Vec<AggregateContextSnapshotHandle>, run_path: PathBuf, publisher: Arc<dyn ContextDumpPublisher>,
) -> DogStatsDContextDumpAPIHandler {
    DogStatsDContextDumpAPIHandler::new_for_test(AUTH_TOKEN, handles, run_path, Duration::from_millis(100), publisher)
        .expect("test handler should be valid")
}

fn post_request(authorization: Option<&str>) -> Request<Empty<Bytes>> {
    let mut builder = Request::builder().method("POST").uri(ROUTE);
    if let Some(authorization) = authorization {
        builder = builder.header(AUTHORIZATION, authorization);
    }
    builder.body(Empty::new()).unwrap()
}

fn authorized_post() -> Request<Empty<Bytes>> {
    post_request(Some("Bearer expected-agent-token"))
}

async fn send(handler: &DogStatsDContextDumpAPIHandler, request: Request<Empty<Bytes>>) -> Response {
    handler
        .generate_routes()
        .with_state(handler.generate_initial_state())
        .oneshot(request)
        .await
        .unwrap()
}

async fn response_body(response: Response) -> String {
    String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn snapshot_entry(name: &'static str) -> AggregateContextSnapshotEntry {
    AggregateContextSnapshotEntry::for_test(
        Context::from_static_name(name),
        AggregateMetricType::Gauge,
        MetaString::empty(),
    )
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort_unstable();
    entries
}

struct NoOpPublisher;

impl ContextDumpPublisher for NoOpPublisher {
    fn publish(
        &self, run_path: PathBuf, _snapshot: Vec<AggregateContextSnapshotEntry>,
    ) -> Result<PathBuf, GenericError> {
        Ok(run_path.join(CONTEXT_DUMP_FILENAME))
    }
}

struct FailingPublisher;

impl ContextDumpPublisher for FailingPublisher {
    fn publish(
        &self, _run_path: PathBuf, _snapshot: Vec<AggregateContextSnapshotEntry>,
    ) -> Result<PathBuf, GenericError> {
        Err(generic_error!("injected publisher failure with {AUTH_TOKEN}"))
    }
}

struct PanickingPublisher;

impl ContextDumpPublisher for PanickingPublisher {
    fn publish(
        &self, _run_path: PathBuf, _snapshot: Vec<AggregateContextSnapshotEntry>,
    ) -> Result<PathBuf, GenericError> {
        panic!("injected publisher panic")
    }
}

struct BlockingPublisherReleaseGuard {
    release: Arc<(StdMutex<bool>, Condvar)>,
}

impl BlockingPublisherReleaseGuard {
    fn new(release: Arc<(StdMutex<bool>, Condvar)>) -> Self {
        Self { release }
    }
}

impl Drop for BlockingPublisherReleaseGuard {
    fn drop(&mut self) {
        let (released, wake) = &*self.release;
        let mut released = released.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *released = true;
        wake.notify_all();
    }
}

struct BlockingFirstPublisher {
    active: AtomicUsize,
    max_active: AtomicUsize,
    calls: AtomicUsize,
    first_started: mpsc::SyncSender<()>,
    release: Arc<(StdMutex<bool>, Condvar)>,
}

impl ContextDumpPublisher for BlockingFirstPublisher {
    fn publish(
        &self, run_path: PathBuf, _snapshot: Vec<AggregateContextSnapshotEntry>,
    ) -> Result<PathBuf, GenericError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.first_started.send(()).unwrap();
            let (released, wake) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(run_path.join(CONTEXT_DUMP_FILENAME))
    }
}

struct BlockingRealPublisher {
    started: mpsc::SyncSender<()>,
    release: Arc<(StdMutex<bool>, Condvar)>,
    completed: Arc<AtomicBool>,
}

impl ContextDumpPublisher for BlockingRealPublisher {
    fn publish(
        &self, run_path: PathBuf, snapshot: Vec<AggregateContextSnapshotEntry>,
    ) -> Result<PathBuf, GenericError> {
        self.started.send(()).unwrap();
        let (released, wake) = &*self.release;
        let mut released = released.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        drop(released);
        let result = publish_context_dump(&run_path, &snapshot);
        self.completed.store(true, Ordering::SeqCst);
        result
    }
}

#[test]
fn production_publisher_type_uses_the_real_artifact_publisher() {
    let run_directory = tempfile::tempdir().unwrap();
    let path = FileSystemContextDumpPublisher
        .publish(run_directory.path().to_owned(), vec![snapshot_entry("real.publisher")])
        .unwrap();

    let mut count = 0;
    for_each_record(&path, |_| count += 1).unwrap();
    assert_eq!(count, 1);
}
