//! Tests of the pod backend and the lifecycle against a real kind cluster. They are ignored by default; run them by
//! hand with `make test-kind`, which builds the courier they place on the node. They use the single-node cluster named
//! by `KIND_CLUSTER_NAME`, or `saluki-correctness`, which must exist.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use airlock::container::Container;
use futures::FutureExt as _;
use k8s_openapi::api::authorization::v1::{ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec};
use k8s_openapi::api::core::v1::{Namespace, Pod};
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding};
use kube::api::{Api, PostParams};
use kube::Client;
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
use super::result::{ErrorKind, PhaseTracker, ReclaimEntry, ReclaimResult, TestOutcome};
use super::tasks::{CaseTasks, TASK_STOP_TIME};

const BUDGET: Duration = Duration::from_secs(180);
/// How long the checks before reclaim may take, so that a stalled request cannot keep the case from its cleanup.
const CHECK_TIME: Duration = Duration::from_secs(30);

/// What a case run left: its result, the ledger's reclaim entries, and the cluster.
struct Ran {
    result: super::result::TestResult,
    entries: Vec<ReclaimEntry>,
    client: Client,
    node: Container,
}

impl Ran {
    /// The name of the resource that the ledger reports with the label `prefix`.
    fn resource(&self, prefix: &str) -> &str {
        self.entries
            .iter()
            .find_map(|entry| entry.resource.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("the ledger should hold a {prefix}: {:?}", self.entries))
    }
}

/// Runs the case `config` on the kind cluster, calls `before_reclaim` with the cluster's client and the labels of the
/// resources the case made, and then reclaims them. A check that panics or runs out of time fails the test after
/// reclaim.
async fn run_case<F, Fut>(config: &str, before_reclaim: F) -> Ran
where
    F: FnOnce(Client, Vec<String>) -> Fut,
    Fut: Future<Output = ()>,
{
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
    let checked = tokio::time::timeout(
        CHECK_TIME,
        AssertUnwindSafe(before_reclaim(client.clone(), ledger.labels())).catch_unwind(),
    )
    .await;
    let (entries, _) = ledger.reclaim(Instant::now() + RECLAIM_TIME).await;
    let result = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    checked
        .unwrap_or_else(|_| panic!("the checks before reclaim took more than {CHECK_TIME:?}"))
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    Ran {
        result,
        entries,
        client,
        node,
    }
}

/// Asserts that a pod that cannot pull its image failed the case with a setup error.
fn assert_cannot_pull(ran: &Ran) {
    assert_eq!(ran.result.outcome, TestOutcome::Errored, "{:?}", ran.result);
    let error = ran
        .result
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
}

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
    let ran = run_case(config, |_, _| async {}).await;
    assert_cannot_pull(&ran);

    let results: Vec<ReclaimResult> = ran.entries.iter().map(|entry| entry.result).collect();
    assert_eq!(
        results,
        [ReclaimResult::Reclaimed, ReclaimResult::Reclaimed],
        "{:?}",
        ran.entries
    );
    let (namespace, directory) = (ran.resource("namespace:"), ran.resource("node_directory:"));
    let namespaces: Api<Namespace> = Api::all(ran.client.clone());
    let found = namespaces.get(namespace).await;
    assert!(
        matches!(&found, Err(kube::Error::Api(status)) if status.code == 404),
        "{found:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let probe = ran
        .node
        .exec(
            vec!["test".to_string(), "-e".to_string(), directory.to_string()],
            None,
            deadline,
        )
        .await
        .unwrap();
    assert_ne!(probe.exit_code, 0, "the node directory {directory} should be gone");
}

#[tokio::test]
#[ignore = "needs a kind cluster"]
async fn a_pod_with_the_kubelet_option_may_read_the_kubelet_api_until_reclaim() {
    let config = r#"
timeout: 3m
subject_os: [linux]
participants:
  app:
    subject: true
    image: panoramic.invalid/no-such-image:0
    command: [sleep, "600"]
    realization: {enclosure: kubernetes-pod, options: {kubelet: true}}
procedure: []
"#;
    let ran = run_case(config, |client, labels| async move {
        let label = |prefix: &str| {
            labels
                .iter()
                .find_map(|label| label.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("the ledger should hold a {prefix}: {labels:?}"))
                .to_string()
        };
        let namespace = label("namespace:");
        let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
        let pod = pods.get("app").await.unwrap();
        assert_eq!(
            pod.spec.and_then(|spec| spec.service_account_name).as_deref(),
            Some("kubelet")
        );
        let user = format!("system:serviceaccount:{}:kubelet", namespace);
        for (subresource, allowed) in [("proxy", true), ("stats", true), ("log", false)] {
            assert_eq!(
                may_get_node(&client, &user, subresource).await,
                allowed,
                "{user} on nodes/{subresource}"
            );
        }
        let roles: Api<ClusterRole> = Api::all(client);
        let role = roles.get(&label("cluster_role:")).await.unwrap();
        assert_eq!(
            role.metadata.owner_references.unwrap_or_default()[0].name,
            namespace,
            "the namespace owns the ClusterRole"
        );
    })
    .await;
    assert_cannot_pull(&ran);

    assert!(
        ran.entries.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
        "{:?}",
        ran.entries
    );
    let name = ran.resource("cluster_role:");
    let roles: Api<ClusterRole> = Api::all(ran.client.clone());
    let bindings: Api<ClusterRoleBinding> = Api::all(ran.client.clone());
    assert!(
        roles.get_opt(name).await.unwrap().is_none(),
        "the ClusterRole should be gone"
    );
    assert!(
        bindings.get_opt(name).await.unwrap().is_none(),
        "the ClusterRoleBinding should be gone"
    );
}

/// Whether `user` may get the `subresource` of nodes, as the API server authorizes the kubelet's requests. A review
/// that the API server could not evaluate fails the test, so that it does not read as a denial.
async fn may_get_node(client: &Client, user: &str, subresource: &str) -> bool {
    let review = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(user.to_string()),
            resource_attributes: Some(ResourceAttributes {
                verb: Some("get".to_string()),
                resource: Some("nodes".to_string()),
                subresource: Some(subresource.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let reviews: Api<SubjectAccessReview> = Api::all(client.clone());
    let review = reviews.create(&PostParams::default(), &review).await.unwrap();
    let status = review.status.expect("the API server should answer the review");
    assert!(
        status.evaluation_error.as_deref().unwrap_or_default().is_empty(),
        "the review of {user} on nodes/{subresource} could not be evaluated: {status:?}"
    );
    status.allowed
}
