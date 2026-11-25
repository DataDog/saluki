//! Tests of the pod backend and the lifecycle against a real kind cluster. They are ignored by default; run them by
//! hand with `make test-kind`, which builds the courier they place on the node. They use the single-node cluster named
//! by `KIND_CLUSTER_NAME`, or `saluki-correctness`, which must exist.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::Api;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::backend::Backends;
use super::case::{self, CASE_FILE};
use super::container::LINUX;
use super::deadline::{Budget, Deadline};
use super::ledger::{ResourceLedger, RECLAIM_TIME};
use super::lifecycle::{self, CaseRun};
use super::participant::{Enclosure, Os};
use super::plan::plan;
use super::pod::PodBackend;
use super::record::CaseRecord;
use super::registry::Registry;
use super::result::{ErrorKind, PhaseTracker, ReclaimResult, TestOutcome};
use super::tasks::{CaseTasks, TASK_STOP_TIME};

const BUDGET: Duration = Duration::from_secs(180);

#[tokio::test]
#[ignore = "needs a kind cluster"]
async fn a_pod_that_cannot_pull_its_image_leaves_no_namespace_or_node_directory() {
    let config = r#"
timeout: 3m
subject_os: [linux]
participants:
  app:
    subject: true
    image: panoramic.invalid/no-such-image:0
    command: [sleep, "600"]
    realization: {enclosure: kubernetes-pod}
procedure: []
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(CASE_FILE), config).unwrap();
    let registry = Registry::default();
    let plan = case::load(dir.path(), &registry)
        .and_then(|case| plan(&case, Os::Linux, &registry, &BTreeMap::new()))
        .unwrap_or_else(|errors| panic!("the case should plan: {errors:?}"));

    let _ = crate::default_crypto_provider().install_default();
    let docker = airlock::docker::connect().expect("a Docker daemon should be reachable");
    let courier = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join(LINUX.courier_binary());
    let courier = std::fs::read(&courier)
        .unwrap_or_else(|e| panic!("run `make build-courier` first: {}: {e}", courier.display()));
    let cluster = std::env::var("KIND_CLUSTER_NAME").unwrap_or_else(|_| "saluki-correctness".to_string());
    let backend = PodBackend::connect(&cluster, docker, courier.into())
        .await
        .expect("the kind cluster should be reachable");
    let (client, node) = (backend.client.clone(), backend.node.clone());
    let mut backends = Backends::default();
    backends.insert(Enclosure::KubernetesPod, Arc::new(backend));

    let cancel = CancellationToken::new();
    let logs = tempfile::tempdir().unwrap();
    let (ledger, record, phases) = (
        ResourceLedger::default(),
        CaseRecord::default(),
        PhaseTracker::default(),
    );
    let tasks = CaseTasks::new(&cancel);
    let run = CaseRun {
        backends: &backends,
        ledger: &ledger,
        tasks: &tasks,
        cancel: &cancel,
        deadline: Deadline::after(Budget::Case, BUDGET),
        log_dir: logs.path(),
        phases: &phases,
        record: &record,
    };
    let outcome = AssertUnwindSafe(lifecycle::run(&plan, &run)).catch_unwind().await;
    tasks.stop_all(Instant::now() + TASK_STOP_TIME).await;
    let (entries, _) = ledger.reclaim(Instant::now() + RECLAIM_TIME).await;
    let result = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    assert_eq!(result.outcome, TestOutcome::Errored, "{result:?}");
    let error = result
        .error
        .as_ref()
        .expect("a pod that cannot start is a harness error");
    assert_eq!(error.kind, ErrorKind::Setup);
    assert!(
        error.message.contains("cannot start its container")
            && (error.message.contains("ErrImagePull") || error.message.contains("ImagePullBackOff")),
        "{}",
        error.message
    );

    let results: Vec<ReclaimResult> = entries.iter().map(|entry| entry.result).collect();
    assert_eq!(
        results,
        [ReclaimResult::Reclaimed, ReclaimResult::Reclaimed],
        "{entries:?}"
    );
    let resource = |prefix: &str| {
        entries
            .iter()
            .find_map(|entry| entry.resource.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("the ledger should hold a {prefix}: {entries:?}"))
    };
    let (namespace, directory) = (resource("namespace:"), resource("node_directory:"));
    let namespaces: Api<Namespace> = Api::all(client);
    let found = namespaces.get(namespace).await;
    assert!(
        matches!(&found, Err(kube::Error::Api(status)) if status.code == 404),
        "{found:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let probe = node
        .exec(
            vec!["test".to_string(), "-e".to_string(), directory.to_string()],
            None,
            deadline,
        )
        .await
        .unwrap();
    assert_ne!(probe.exit_code, 0, "the node directory {directory} should be gone");
}
