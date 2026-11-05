//! Native participant reporting through the runner, including a dropped case body.

use std::collections::BTreeMap;
use std::collections::HashMap;

use async_trait::async_trait;
use panoramic_core::{
    artifact::{verified_artifacts, ArtifactKind, ArtifactManifest, ArtifactRole},
    backend::{self, RunBackends, FIXUP_IMAGE},
    case::{CaseLoader, Registries},
    layout::{Os, Realization, RealizationKind},
    participant::{
        CollectDestination, CollectFile, ExitChannel, OutputChannel, OutputStream, ProcessSpec, RenderedConfig,
    },
    plan::Planner,
    recipe::{ContainerRecipe, Recipe},
    record::{AssertionStatus, StepReport},
    result::{ReclaimResult, TestOutcome},
    step::{Need, Step, StepContext, StepFactory, StepFields},
};

use super::*;

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
    fn recipe(&self, _: Realization, _: &RenderedConfig) -> Result<Recipe, String> {
        Ok(Recipe::Container(
            ContainerRecipe::new(IMAGE)
                .with_entrypoint(["sh", "-c"])
                .with_command(["echo ready; exec sleep 60"])
                .with_file("/capture", 0o644, b"payload")
                .with_file("/capture.json", 0o644, br#"{"complete":true,"records":1}"#),
        ))
    }
    fn collect(&self, _: Realization) -> &'static [CollectFile] {
        &[
            CollectFile {
                path: "/capture",
                name: "input.jsonl.zst",
                kind: "input",
                destination: CollectDestination::Traffic,
            },
            CollectFile {
                path: "/capture.json",
                name: "input-manifest.json",
                kind: "input_manifest",
                destination: CollectDestination::Traffic,
            },
            CollectFile {
                path: "/capture",
                name: "retained.bin",
                kind: "sample",
                destination: CollectDestination::Participant,
            },
        ]
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

fn plan() -> CasePlan {
    let registries = Registries {
        participant_types: &[&Fixture],
        steps: &[&Pass],
    };
    let case = CaseLoader::new(registries)
        .load_content("cases", "timeout: 2m\nsubject_os: [linux]\nparticipants:\n  subject: {type: fixture, subject: true}\nprocedure: [{step: pass}]",
            "report", Path::new("cases/report/config.yaml"))
        .unwrap();
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
    inputs: NativeInputs,
    cancel_run: CancellationToken,
}

#[async_trait]
impl Test for StallsAfterStartup {
    async fn run(&self, plan: &CasePlan, tctx: TestContext) -> TestResult {
        let control = CaseControl {
            interrupt: self.cancel_run.clone(),
            observations: tctx.observations.clone(),
            record: tctx.record.clone(),
            deadline: tctx.deadline,
            cancel: tctx.test_cancel_token(),
            phases: tctx.phases.clone(),
            ledger: tctx.ledger.clone(),
            tasks: tctx.tasks.clone(),
            log_dir: tctx.log_dir().to_owned(),
        };
        let result = lifecycle::run(plan.participants(), &[&Fixture], &self.inputs, &control).await;
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
    let mut runner = Runner::new(logs.path());
    let cancel = CancellationToken::new();
    let built = if stall {
        BuiltCase::new(
            plan,
            Box::new(StallsAfterStartup {
                inputs,
                cancel_run: cancel.clone(),
            }),
        )
    } else {
        BuiltCase::new(
            plan,
            Box::new(NativeTest {
                inputs: Arc::new(inputs),
                types: &[&Fixture],
            }),
        )
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
    assert_eq!(report["schema_version"], 4);
    let collected = report["collected"].as_array().unwrap();
    assert_eq!(collected.len(), 1, "{collected:?}");
    let retained = result.log_dir.as_ref().unwrap().join("subject/collected/retained.bin");
    assert_eq!(collected[0]["path"], retained.to_str().unwrap());
    assert_eq!(std::fs::read(retained).unwrap(), b"payload");
    let traffic = result.log_dir.as_ref().unwrap().join("traffic");
    assert!(traffic.join("manifest.json").is_file());
    assert!(!traffic.join("input.jsonl.zst").exists());
    assert!(!traffic.join("input-manifest.json").exists());
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

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_case_deadline_keeps_traffic_collections_before_reclaim() {
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
    let (_logs, report) = super::test_support::run_native_case(
        &[&Fixture],
        crate::REGISTRIES.steps,
        serde_json::json!({
            "timeout": "10s", "subject_os": ["linux"],
            "participants": {"subject": {"type": "fixture", "subject": true}},
            "procedure": [{"step": "process_stable_for", "on": "subject", "process": "main", "duration": "30s"}],
        }),
        BTreeMap::from([(IMAGE.name.into(), image)]),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "timed_out", "{report:#}");
    assert_eq!(report["timeout"]["active_phase"], "steps");
    assert_eq!(report["issues"], serde_json::json!([]));
    let collected = report["collected"].as_array().unwrap();
    assert_eq!(collected.len(), 2, "{collected:?}");
    for file in collected {
        assert_eq!(std::fs::read(file["path"].as_str().unwrap()).unwrap(), b"payload");
    }
    assert!(collected.iter().any(|file| file["kind"] == "input"));
    let traffic = Path::new(report["log_dir"].as_str().unwrap()).join("traffic");
    assert!(!traffic.join("input-manifest.json").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(traffic.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["captures_retained"], true);
    assert_eq!(manifest["input"]["file"]["present"], true);
}
