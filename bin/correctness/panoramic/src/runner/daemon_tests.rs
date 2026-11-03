//! Failure reports and resource removal through the native public runner path.

use std::{collections::BTreeMap, io::Read as _};

use async_trait::async_trait;
use clap::Parser as _;
use panoramic_core::{
    artifact::{ArtifactKind, ArtifactManifest, ArtifactRole},
    backend::{self, LateFact, RunBackends, FIXUP_IMAGE},
    case::{CaseLoader, Registries},
    layout::{Os, Realization, RealizationKind},
    participant::{ExitChannel, OutputChannel, OutputStream, ProcessSpec, RenderedConfig},
    plan::Planner,
    recipe::{test_support::shell, Recipe},
    record::StepReport,
    step::{decode_fields, test_support, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{report_tests::assert_scope_removed, test_support::test_runner_settings, *};
use crate::{
    cli::{Cli, Command},
    config::typed_case::TypedConfigLoader,
    console::Console,
    run_command,
};

const BOUND: Duration = Duration::from_secs(120);
const IMAGE: ArtifactRole = ArtifactRole {
    name: "runner-test-image",
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
    fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        Ok(shell(IMAGE, config))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum WaitFor {
    Exit { process: String },
    Output { text: String },
}

#[derive(Debug)]
struct Wait {
    condition: WaitFor,
    entered: mpsc::UnboundedSender<String>,
    removed: Option<CancellationToken>,
}

#[async_trait]
impl Step for Wait {
    fn name(&self) -> &'static str {
        "wait"
    }
    fn needs(&self) -> Vec<Need> {
        let mut needs = vec![Need::Late("first".into(), LateFact::ContainerId)];
        if let WaitFor::Exit { process } = &self.condition {
            needs.push(Need::Process("first".into(), process.clone()));
        }
        needs
    }
    fn budget(&self) -> Duration {
        BOUND
    }
    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let _ = self.entered.send(test_support::container_id(cx, "first").unwrap());
        if let Some(removed) = &self.removed {
            // Finish fault injection before the case can enter reclaim and race the external removal.
            removed.cancelled().await;
        }
        match &self.condition {
            WaitFor::Exit { process } => test_support::wait_for_exit(cx, "first", process).await,
            WaitFor::Output { text } => test_support::wait_for_output(cx, "first", text).await,
        }
    }
}

struct WaitFactory {
    entered: mpsc::UnboundedSender<String>,
    removed: Option<CancellationToken>,
}

impl StepFactory for WaitFactory {
    fn name(&self) -> &'static str {
        "wait"
    }
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(Wait {
            condition: decode_fields(fields)?,
            entered: self.entered.clone(),
            removed: self.removed.clone(),
        }))
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Remove,
    Cancel,
    Deadline,
}

fn participants(second_env: Value) -> Value {
    json!({
        "first": {"type": "fixture", "subject": true},
        "second": {"type": "fixture", "config": {"env": second_env}},
    })
}

fn waiting() -> Value {
    json!([{ "step": "wait", "action": "exit", "process": "worker" },
           { "step": "wait", "action": "output", "text": "never" }])
}

async fn run_case(participants: Value, procedure: Value, fault: Fault) -> (tempfile::TempDir, Value) {
    let logs = tempfile::tempdir().unwrap();
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let removed = matches!(fault, Fault::Remove).then(CancellationToken::new);
    let factory = WaitFactory {
        entered: entered_tx,
        removed: removed.clone(),
    };
    let registries = Registries {
        participant_types: &[&Fixture],
        steps: &[&factory],
        typed_case_roles: &[],
    };
    let case = CaseLoader::new(registries, TypedConfigLoader, "native")
        .load_content(
            &json!({
                "timeout": if matches!(fault, Fault::Deadline) { "10s" } else { "90s" },
                "subject_os": ["linux"], "participants": participants, "procedure": procedure,
            })
            .to_string(),
            "failure",
            &logs.path().join("cases/failure/config.yaml"),
        )
        .unwrap()
        .remove(0);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let courier = backend::artifacts(RealizationKind::LinuxContainer)
        .iter()
        .find(|role| matches!(role.kind, ArtifactKind::ContainerBinary { .. }))
        .unwrap();
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
    let manifest = ArtifactManifest::new(
        "runner tests",
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
    let path = logs.path().join("artifacts.toml");
    std::fs::write(&path, manifest.render()).unwrap();
    let plan = Planner::new(registries, ArtifactManifest::load(&path).unwrap())
        .unwrap()
        .compile(case, Os::Linux)
        .unwrap()
        .into_plan()
        .unwrap();
    let scope = panoramic_core::plan::test_support::scope_id(&plan).unwrap();
    eprintln!("Runner failure test scope: {scope}");
    let built = BuiltCase {
        plan,
        execution: Execution::Native,
    };
    let Command::Run(cmd) = Cli::try_parse_from([
        "panoramic",
        "run",
        "--subject-os",
        "linux",
        "--no-tui",
        "--artifacts",
        path.to_str().unwrap(),
    ])
    .unwrap()
    .command
    else {
        panic!("expected run")
    };
    let (verified, native) = run_command::prepare_artifacts(
        Arc::new(RunBackends::default()),
        &cmd,
        &Console { use_tui: false },
        Os::Linux,
        std::slice::from_ref(&built),
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    let verified = serde_json::to_value(verified.unwrap()).unwrap();
    assert!(verified
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| artifact["role"] == IMAGE.name
            && artifact["identity"]
                .as_str()
                .is_some_and(|identity| !identity.is_empty())));
    let mut runner =
        Runner::new(logs.path(), test_runner_settings(logs.path())).with_native(native.unwrap(), &[&Fixture]);
    runner.register(built).unwrap();
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let inject = async {
        if matches!(fault, Fault::None) {
            return Ok(());
        }
        let id = tokio::time::timeout(Duration::from_secs(30), entered_rx.recv())
            .await
            .map_err(|_| "the procedure did not start".to_string())?
            .ok_or("the procedure ended before starting")?;
        match fault {
            Fault::Remove => {
                let docker = airlock::docker::connect().unwrap();
                // Nested Docker daemons can briefly leave overlay mounts busy during forced removal.
                for attempt in 0..5 {
                    match docker
                        .remove_container(
                            &id,
                            Some(
                                bollard::query_parameters::RemoveContainerOptionsBuilder::default()
                                    .force(true)
                                    .build(),
                            ),
                        )
                        .await
                    {
                        Ok(()) | Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
                            break
                        }
                        Err(bollard::errors::Error::DockerResponseServerError { status_code: 500, .. })
                            if attempt < 4 =>
                        {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
                removed.as_ref().unwrap().cancel();
            }
            Fault::Cancel => cancel.cancel(),
            Fault::Deadline => {}
            Fault::None => unreachable!(),
        }
        Ok::<(), String>(())
    };
    let (_, injection) = tokio::time::timeout(BOUND, async {
        tokio::join!(runner.run_tests(RunArgs::new(cancel.clone(), tx)), async {
            let result = inject.await;
            if result.is_err() {
                cancel.cancel();
            }
            result
        })
    })
    .await
    .expect("the runner must return after its cleanup budgets");
    let mut completed = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let TestEvent::TestCompleted { result, .. } = event {
            completed.push(result);
        }
    }
    assert_scope_removed(&scope).await;
    injection.unwrap();
    assert_eq!(completed.len(), 1);
    let result = &completed[0];
    let report: Value =
        serde_json::from_slice(&std::fs::read(result.log_dir.as_ref().unwrap().join("result.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(
        report["participants"],
        serde_json::to_value(&result.participants).unwrap()
    );
    assert_eq!(report["issues"], json!([]), "{report:#}");
    let reclaim = report["reclaim"].as_array().unwrap();
    assert_eq!(reclaim.len(), 3, "{report:#}");
    for (entry, name) in reclaim.iter().zip(["second", "first"]) {
        assert_eq!(
            *entry,
            json!({"resource": format!("participant:{name}"), "participant": name, "result": "reclaimed"})
        );
    }
    assert_eq!(
        reclaim[2],
        json!({"resource": format!("isolation_group:{scope}"), "result": "reclaimed"})
    );
    (logs, report)
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_second_participant_creation_failure_reclaims_the_first() {
    let (_logs, report) = run_case(
        participants(json!({"HOST_MOUNT": "relative/path"})),
        waiting(),
        Fault::None,
    )
    .await;
    assert_eq!(report["outcome"], "errored", "{report:#}");
    assert_eq!(report["error"]["kind"], "setup");
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Failed to create participant 'second'"),
        "{report:#}"
    );
    assert_eq!(
        report["phase_timings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|phase| phase["phase"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["prepare", "create:first", "create:second", "reclaim"]
    );
    for participant in report["participants"].as_array().unwrap() {
        assert_eq!(participant["processes"], json!([]));
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_start_failure_is_a_setup_error_and_reclaims_every_participant() {
    let (_logs, report) = run_case(
        participants(json!({"ENTRYPOINT": "/no-such-executable"})),
        waiting(),
        Fault::None,
    )
    .await;
    assert_eq!(report["outcome"], "errored", "{report:#}");
    assert_eq!(report["error"]["kind"], "setup");
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Failed to start participant 'second'"),
        "{report:#}"
    );
    assert_eq!(report["participants"][0]["name"], "first");
    assert_eq!(report["participants"][0]["processes"].as_array().unwrap().len(), 2);
    assert_eq!(report["participants"][1]["processes"], json!([]));
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn removing_a_participant_during_the_procedure_reports_an_unknown_exit_and_errored_step() {
    let (_logs, report) = run_case(participants(json!({})), waiting(), Fault::Remove).await;
    assert_eq!(report["outcome"], "errored", "{report:#}");
    let process = &report["participants"][0]["processes"][1];
    assert_eq!(process["name"], "worker");
    assert_eq!(process["exit"]["state"], "undetermined");
    assert_eq!(process["exit"]["source"], "record");
    assert!(process["exit"].get("code").is_none());
    assert!(process["exit"].get("signal").is_none());
    assert_eq!(report["assertions"][0]["status"], "errored");
    assert_eq!(report["assertions"][0]["message"], process["exit"]["reason"]);
    assert_eq!(report["assertions"][1]["status"], "not_run");
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn large_output_stays_byte_identical_on_disk_and_overflows_the_step_reader() {
    let mut participants = participants(json!({}));
    participants["first"]["config"] = json!({"env": {"SCRIPT":
        "mkdir -p /airlock/first/log; awk 'BEGIN { for (i=0;i<8191;i++) s=s \"x\"; for (i=0;i<8448;i++) print s }' > /airlock/first/log/worker; printf 'after-bound\\n' >> /airlock/first/log/worker"
    }});
    let (_logs, report) = run_case(
        participants,
        json!([
            {"step": "wait", "action": "exit", "process": "main"},
            {"step": "wait", "action": "output", "text": "after-bound"},
        ]),
        Fault::None,
    )
    .await;
    assert_eq!(report["outcome"], "errored", "{report:#}");
    assert_eq!(report["assertions"][0]["status"], "passed");
    assert_eq!(report["assertions"][1]["status"], "errored");
    let path = report["participants"][0]["processes"][1]["output"]["merged"]
        .as_str()
        .unwrap();
    let message = report["assertions"][1]["message"].as_str().unwrap();
    assert!(
        message.contains("output capture failed")
            && message.contains("67108864-byte line-text bound")
            && message.contains(path),
        "{message}"
    );
    let mut file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut expected = [b'x'; 8192];
    expected[8191] = b'\n';
    let mut bytes = [0; 8192];
    for _ in 0..8448 {
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
    }
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, b"after-bound\n");
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn an_interrupted_run_reclaims_every_participant_and_reports_its_running_step() {
    let (_logs, report) = run_case(participants(json!({})), waiting(), Fault::Cancel).await;
    assert_eq!(report["outcome"], "errored", "{report:#}");
    assert_eq!(report["cause"]["by"], "signal");
    assert_interrupted_step(&report);
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_case_deadline_reclaims_every_participant() {
    let (_logs, report) = run_case(participants(json!({})), waiting(), Fault::Deadline).await;
    assert_eq!(report["outcome"], "timed_out", "{report:#}");
    assert_eq!(report["cause"]["by"], "case_deadline");
    assert_eq!(report["timeout"]["active_phase"], "steps");
    assert_interrupted_step(&report);
}

fn assert_interrupted_step(report: &Value) {
    assert_eq!(report["cause"]["active"].as_array().unwrap().len(), 1);
    assert_eq!(report["cause"]["active"][0]["step"], 0);
    assert_eq!(report["cause"]["active"][0]["kind"], "wait");
    assert_eq!(report["assertions"][0]["status"], "interrupted");
    assert_eq!(report["assertions"][1]["status"], "not_run");
    let participants = report["participants"].as_array().unwrap();
    assert_eq!(participants.len(), 2);
    for participant in participants {
        let processes = participant["processes"].as_array().unwrap();
        assert_eq!(processes.len(), 2);
        for process in processes {
            assert_eq!(process["exit"]["state"], "running", "{report:#}");
        }
    }
}
