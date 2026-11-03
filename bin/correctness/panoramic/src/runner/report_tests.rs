//! Native participant reporting through the runner, including a dropped case body.

use std::collections::BTreeMap;

use async_trait::async_trait;
use panoramic_core::{
    artifact::{verified_artifacts, ArtifactKind, ArtifactManifest, ArtifactRole},
    backend::{self, RunBackends, FIXUP_IMAGE},
    case::{CaseLoader, Registries},
    layout::{Os, Realization, RealizationKind},
    participant::{ExitChannel, OutputChannel, OutputStream, ProcessSpec, RenderedConfig},
    plan::Planner,
    recipe::{test_support::container, Recipe},
    record::{AssertionStatus, StepReport},
    result::{ReclaimResult, TestOutcome},
    step::{Need, Step, StepContext, StepFactory, StepFields},
};

use super::{test_support::test_runner_settings, *};
use crate::config::typed_case::TypedConfigLoader;

const IMAGE: ArtifactRole = ArtifactRole {
    name: "report-test-image",
    kind: ArtifactKind::Image { os: Os::Linux },
    local_default: "",
};

struct Fixture;

impl ParticipantType for Fixture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[IMAGE]
    }
    fn stop_grace(&self, _: Realization) -> Duration {
        Duration::from_secs(1)
    }
    fn processes(&self) -> &'static [&'static str] {
        &["main", "worker"]
    }
    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        &[
            ProcessSpec {
                name: "main",
                exit: ExitChannel::Container,
                output: OutputChannel::Container,
            },
            ProcessSpec {
                name: "worker",
                exit: ExitChannel::Record { file: "worker" },
                output: OutputChannel::File {
                    file: "worker",
                    stream: OutputStream::Merged,
                },
            },
        ]
    }
    fn recipe(&self, _: Realization, _: &RenderedConfig) -> Result<Recipe, String> {
        Ok(container(IMAGE, &["sh", "-c"], &["echo ready; exec sleep 60"]))
    }
}

#[derive(Debug)]
struct Pass;

#[async_trait]
impl Step for Pass {
    fn name(&self) -> &'static str {
        "pass"
    }
    fn needs(&self) -> Vec<Need> {
        vec![]
    }
    fn budget(&self) -> Duration {
        Duration::from_secs(30)
    }
    async fn run(&self, _: &StepContext<'_>) -> StepReport {
        StepReport::new(tokio::time::Instant::now(), AssertionStatus::Passed, "passed")
    }
}

impl StepFactory for Pass {
    fn name(&self) -> &'static str {
        "pass"
    }
    fn build(&self, _: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(Self))
    }
}

fn plan() -> LoadedPlan {
    let registries = Registries {
        participant_types: &[&Fixture],
        steps: &[&Pass],
        typed_case_roles: &[],
    };
    let case = CaseLoader::new(registries, TypedConfigLoader, "native")
        .load_content("timeout: 2m\nsubject_os: [linux]\nparticipants:\n  subject: {type: fixture, subject: true}\nprocedure: [{step: pass}]",
            "report", Path::new("cases/report/config.yaml"))
        .unwrap().remove(0);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let courier = backend::artifacts(RealizationKind::LinuxContainer)
        .iter()
        .find(|role| matches!(role.kind, ArtifactKind::ContainerBinary { .. }))
        .unwrap();
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
    let manifest = ArtifactManifest::new(
        "test",
        BTreeMap::from([
            (IMAGE.name.into(), image.clone()),
            (FIXUP_IMAGE.name.into(), image),
            (
                courier.name.into(),
                root.join(courier.local_default).to_str().unwrap().into(),
            ),
        ]),
    )
    .unwrap();
    Planner::new(registries, manifest)
        .unwrap()
        .compile(case, Os::Linux)
        .unwrap()
        .into_plan()
        .unwrap()
}

struct StallsAfterStartup {
    plan: LoadedPlan,
    inputs: NativeInputs,
    cancel_run: CancellationToken,
}

#[async_trait]
impl Test for StallsAfterStartup {
    fn images(&self) -> BTreeMap<&str, String> {
        BTreeMap::new()
    }
    async fn run(&self, tctx: TestContext) -> TestResult {
        let control = CaseControl {
            observations: tctx.observations.clone(),
            record: tctx.record.clone(),
            deadline: tctx.deadline,
            cancel: tctx.test_cancel_token(),
            phases: tctx.phases.clone(),
            ledger: tctx.ledger.clone(),
            tasks: tctx.tasks.clone(),
            log_dir: tctx.log_dir().to_owned(),
        };
        let result = lifecycle::run(self.plan.participants().unwrap(), &[&Fixture], &self.inputs, &control).await;
        if result.outcome != TestOutcome::Passed {
            return result;
        }
        self.cancel_run.cancel();
        std::future::pending().await
    }
}

async fn run_report_case(stall: bool) {
    let plan = plan();
    let backends = Arc::new(RunBackends::default());
    let verified = backends.verify(plan.artifacts(), Path::new(".")).await;
    let verified = verified_artifacts(&verified).expect("daemon, image and built Linux courier must be available");
    let inputs = NativeInputs::new(backends, &verified, Path::new(".")).unwrap();
    let logs = tempfile::tempdir().unwrap();
    let mut runner = Runner::new(logs.path(), test_runner_settings(logs.path()));
    let cancel = CancellationToken::new();
    let built = if stall {
        BuiltCase {
            plan: self::plan(),
            execution: Execution::Runner(Box::new(StallsAfterStartup {
                plan,
                inputs,
                cancel_run: cancel.clone(),
            })),
        }
    } else {
        runner = runner.with_native(inputs, &[&Fixture]);
        BuiltCase {
            plan,
            execution: Execution::Native,
        }
    };
    runner.register(built).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(90), runner.run_tests(RunArgs::new(cancel, tx)))
        .await
        .unwrap();
    let mut result = None;
    while let Ok(event) = rx.try_recv() {
        if let TestEvent::TestCompleted { result: completed, .. } = event {
            result = Some(completed);
        }
    }
    let result = result.expect("the case must report completion");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(result.log_dir.as_ref().unwrap().join("result.json")).unwrap()).unwrap();
    assert_eq!(result.outcome, TestOutcome::Passed, "{report:#}");
    assert_eq!(report["schema_version"], 3);
    assert_eq!(
        report["participants"],
        serde_json::to_value(&result.participants).unwrap()
    );
    assert_eq!(report["participants"].as_array().unwrap().len(), 1);
    let participant = &report["participants"][0];
    assert_eq!(participant["name"], "subject");
    assert_eq!(participant["type"], "fixture");
    assert_eq!(participant["os"], "linux");
    assert_eq!(participant["realization"], "linux-container");
    let processes = participant["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    for (process, name, source) in [
        (&processes[0], "main", "container"),
        (&processes[1], "worker", "record"),
    ] {
        assert_eq!(process["name"], name);
        assert_eq!(process["exit"]["source"], source);
        assert_eq!(process["exit"]["state"], "running");
        assert!(process["exit"].get("code").is_none());
    }
    let output = result.log_dir.as_ref().unwrap().join("subject");
    assert_eq!(
        processes[0]["output"],
        serde_json::json!({"stdout": output.join("container.stdout.log"), "stderr": output.join("container.stderr.log")})
    );
    assert_eq!(
        processes[1]["output"],
        serde_json::json!({"merged": output.join("worker.log")})
    );
    assert_eq!(result.reclaim.len(), 2);
    assert_eq!(
        report["reclaim"][0],
        serde_json::json!({"resource": "participant:subject", "participant": "subject", "result": "reclaimed"})
    );
    assert!(report["reclaim"][1].get("participant").is_none());
    assert!(result
        .reclaim
        .iter()
        .all(|entry| entry.result == ReclaimResult::Reclaimed));
    assert!(result.phase_timings.iter().any(|phase| phase.phase == "observe"));
    assert_eq!(
        result
            .issues
            .iter()
            .any(|issue| issue.kind == IssueKind::CleanupGraceExpired),
        stall
    );
    let id = result.reclaim[1].resource.strip_prefix("isolation_group:").unwrap();
    eprintln!("Native report test scope: {id}");
    assert_scope_removed(id).await;
}

pub(super) async fn assert_scope_removed(id: &str) {
    use bollard::query_parameters::{
        ListContainersOptionsBuilder, ListNetworksOptionsBuilder, ListVolumesOptionsBuilder,
    };
    let docker = airlock::docker::connect().unwrap();
    let filters = HashMap::from([("label", vec![format!("airlock-isolation-group={id}")])]);
    assert!(docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build()
        ))
        .await
        .unwrap()
        .is_empty());
    assert!(docker
        .list_networks(Some(ListNetworksOptionsBuilder::default().filters(&filters).build()))
        .await
        .unwrap()
        .is_empty());
    assert!(docker
        .list_volumes(Some(ListVolumesOptionsBuilder::default().filters(&filters).build()))
        .await
        .unwrap()
        .volumes
        .unwrap_or_default()
        .is_empty());
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn the_runner_reports_native_processes_and_per_participant_reclaim() {
    run_report_case(false).await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn the_runner_snapshots_participants_after_dropping_a_case_body_that_outlived_its_grace() {
    run_report_case(true).await;
}
