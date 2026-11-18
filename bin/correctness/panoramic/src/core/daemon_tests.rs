//! Tests of the container backend against a real Docker daemon. They are ignored by default; run them by hand with
//! `make test-container-daemon`, which builds the courier they copy into each container.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::Duration;

use airlock::image::{ensure_image, DAEMON_TEST_IMAGE};
use bollard::errors::Error;
use bollard::query_parameters::ListContainersOptionsBuilder;
use bollard::Docker;
use futures::FutureExt as _;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::case::{self, CASE_FILE};
use super::container::{CaseContainers, ContainerBackend, LINUX};
use super::ledger::ResourceLedger;
use super::participant::Os;
use super::plan::{plan, CasePlan};
use super::reference::Values;
use super::registry::Registry;
use super::result::{ReclaimEntry, ReclaimResult};

const BUDGET: Duration = Duration::from_secs(60);

fn deadline() -> Instant {
    Instant::now() + BUDGET
}

/// A planned case with its scope prepared.
struct Case {
    plan: CasePlan,
    containers: CaseContainers,
    cancel: CancellationToken,
}

/// Plans `config`, a case's `config.yaml` in which `{image}` stands for the test image, for Linux. Then prepares its
/// scope, runs `body`, reclaims the scope and checks that nothing of it is left, even when `body` panics.
///
/// Returns the reclaim entries.
async fn in_case<F, Fut>(config: &str, body: F) -> Vec<ReclaimEntry>
where
    F: FnOnce(Case) -> Fut,
    Fut: Future<Output = ()>,
{
    let _ = crate::default_crypto_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(CASE_FILE), config.replace("{image}", DAEMON_TEST_IMAGE)).unwrap();
    let registry = Registry::default();
    let plan = case::load(dir.path(), &registry)
        .and_then(|case| plan(&case, Os::Linux, &registry))
        .unwrap_or_else(|errors| panic!("the case should plan: {errors:?}"));

    let docker = airlock::docker::connect().expect("a Docker daemon should be reachable");
    ensure_image(&docker, DAEMON_TEST_IMAGE, deadline())
        .await
        .expect("the test image should be available");
    let courier = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join(LINUX.courier_binary());
    let courier = std::fs::read(&courier)
        .unwrap_or_else(|e| panic!("run `make build-courier` first: {}: {e}", courier.display()));
    let backend = ContainerBackend::new(docker.clone(), &LINUX, courier.into());

    let ledger = ResourceLedger::default();
    let outcome = AssertUnwindSafe(async {
        let cancel = CancellationToken::new();
        let containers = backend
            .prepare(&ledger, &cancel, deadline())
            .await
            .expect("the scope should be prepared");
        body(Case {
            plan,
            containers,
            cancel,
        })
        .await;
    })
    .catch_unwind()
    .await;
    let (entries, _) = ledger.reclaim(deadline()).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    let scope = entries
        .iter()
        .find_map(|entry| entry.resource.strip_prefix("case_scope:"))
        .expect("the ledger should hold the case scope");
    assert_nothing_left(&docker, scope).await;
    entries
}

fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::DockerResponseServerError { status_code: 404, .. })
}

/// Checks that no container, network or volume named after the scope `name` is left.
async fn assert_nothing_left(docker: &Docker, name: &str) {
    let filters: HashMap<&str, Vec<&str>> = HashMap::from([("name", vec![name])]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let containers = docker.list_containers(Some(options)).await.unwrap();
    assert!(containers.is_empty(), "{containers:?}");
    let network = docker.inspect_network(name, None).await;
    assert!(network.as_ref().is_err_and(is_not_found), "{network:?}");
    let volume = docker.inspect_volume(name).await;
    assert!(volume.as_ref().is_err_and(is_not_found), "{volume:?}");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_failed_start_leaves_no_container_network_or_volume() {
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: "{image}"
    command: [/no/such/program]
procedure: []
"#;
    let entries = in_case(config, |case| async move {
        let values = Values::of_plan(&case.plan.participants);
        let mut app = case
            .containers
            .create("app", &case.plan.participants["app"], &values, &case.cancel, deadline())
            .await
            .unwrap();
        app.start(&case.cancel, deadline())
            .await
            .expect_err("a missing program should fail the start");
    })
    .await;
    let results: Vec<ReclaimResult> = entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Reclaimed]);
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_cancelled_case_leaves_no_container_network_or_volume() {
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: "{image}"
    command: [sleep, "600"]
  late:
    image: "{image}"
    command: [sleep, "600"]
procedure: []
"#;
    let entries = in_case(config, |case| async move {
        let values = Values::of_plan(&case.plan.participants);
        let create = |name: &'static str| {
            case.containers
                .create(name, &case.plan.participants[name], &values, &case.cancel, deadline())
        };
        let mut app = create("app").await.unwrap();
        app.start(&case.cancel, deadline()).await.unwrap();

        case.cancel.cancel();
        let error = create("late")
            .await
            .expect_err("a cancelled case should create nothing more");
        assert!(format!("{error:#}").contains("Cancelled"), "{error:#}");
    })
    .await;
    // The ledger marks the scope unsettled whenever cancellation stops a create, even one that never reached Docker,
    // as here, so the scope is unsettled though nothing is left.
    let results: Vec<ReclaimResult> = entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Unsettled]);
}
