//! Failure reports and resource removal through the native public runner path.

use std::{collections::BTreeMap, io::Read as _};

use async_trait::async_trait;
use panoramic_core::{
    artifact::{ArtifactKind, ArtifactRole},
    backend::LateFact,
    layout::{Os, Realization, RealizationKind},
    participant::{ExitChannel, OutputChannel, OutputStream, ProcessSpec, RenderedConfig},
    probe::POLL_INTERVAL,
    recipe::{
        test_support::{shell, with_output_line_bytes},
        Recipe,
    },
    record::{AssertionStatus, StepReport},
    step::{decode_fields, ExitObservation, Need, OutputEnd, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::*;

const BOUND: Duration = Duration::from_secs(120);
const OUTPUT_LINE_BYTES: usize = 2 * 1024 * 1024;
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
        Ok(with_output_line_bytes(shell(IMAGE, config), OUTPUT_LINE_BYTES))
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
        let handle = cx.handle("first").expect("wait needs its participant");
        let _ = self
            .entered
            .send(handle.late(LateFact::ContainerId).unwrap().to_owned());
        if let Some(removed) = &self.removed {
            // Finish fault injection before the case can enter reclaim and race the external removal.
            removed.cancelled().await;
        }
        let started = tokio::time::Instant::now();
        match &self.condition {
            WaitFor::Exit { process } => {
                let (status, message) = match handle.wait_for_exit(process).await {
                    ExitObservation::Exited(status) if status.code == Some(0) => {
                        (AssertionStatus::Passed, "process exited".into())
                    }
                    ExitObservation::Exited(status) => (AssertionStatus::Failed, format!("process exited: {status:?}")),
                    ExitObservation::Undetermined(reason) => (AssertionStatus::Errored, reason),
                    ExitObservation::Running => unreachable!("wait_for_exit waits for a terminal observation"),
                };
                StepReport::new(started, status, message)
            }
            WaitFor::Output { text } => {
                let mut output = handle.output(None);
                loop {
                    let batch = output.read();
                    if batch.lines.iter().any(|line| line == text) {
                        return StepReport::new(started, AssertionStatus::Passed, format!("{text} observed"));
                    }
                    if let OutputEnd::Failed(reason) = batch.end {
                        return StepReport::new(started, AssertionStatus::Errored, reason);
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
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
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let removed = matches!(fault, Fault::Remove).then(CancellationToken::new);
    let factory = WaitFactory {
        entered: entered_tx,
        removed: removed.clone(),
    };
    let case = json!({
        "timeout": if matches!(fault, Fault::Deadline) { "10s" } else { "90s" },
        "subject_os": ["linux"], "participants": participants, "procedure": procedure,
    });
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
    let bindings = BTreeMap::from([(IMAGE.name.into(), image)]);
    let cancel = CancellationToken::new();
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
    let steps = [&factory as &dyn StepFactory];
    let (result, injection) = tokio::join!(
        super::test_support::run_native_case(&[&Fixture], &steps, case, bindings, cancel.clone()),
        async {
            let result = inject.await;
            if result.is_err() {
                cancel.cancel();
            }
            result
        }
    );
    injection.unwrap();
    result
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
    participants["first"]["config"] = json!({"env": {"SCRIPT": concat!(
        "mkdir -p $PANORAMIC_LOG_DIR; ",
        "awk 'BEGIN { for (i=0;i<8191;i++) s=s \"x\"; for (i=0;i<288;i++) print s }' ",
        "> $PANORAMIC_LOG_DIR/worker; printf 'after-bound\\n' >> $PANORAMIC_LOG_DIR/worker",
    )}});
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
            && message.contains(&format!("{OUTPUT_LINE_BYTES}-byte line-text bound"))
            && message.contains(path),
        "{message}"
    );
    let mut file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut expected = [b'x'; 8192];
    expected[8191] = b'\n';
    let mut bytes = [0; 8192];
    for _ in 0..288 {
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

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn native_log_and_file_steps_observe_a_real_participant() {
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
    let case = json!({
        "timeout": "90s", "subject_os": ["linux"],
        "participants": {"first": {"type": "fixture", "subject": true, "config": {"env": {
            "SCRIPT": "printf 'ready\\n'; printf 'worker ready\\n' > \"$PANORAMIC_LOG_DIR/worker\"; sleep 60",
        }}}},
        "procedure": [
            {"step": "log_contains", "on": "first", "process": "main", "pattern": "ready", "timeout": "20s"},
            {"step": "log_contains", "on": "first", "process": "worker", "pattern": "worker ready", "timeout": "20s"},
            {"step": "file_contains", "on": "first", "path": "/airlock/first/log/worker", "pattern": "worker ready", "timeout": "20s"},
            {"step": "log_not_contains", "on": "first", "pattern": "panic", "during": "1s"},
        ],
    });
    let (_logs, report) = super::test_support::run_native_case(
        &[&Fixture],
        panoramic_steps::REGISTERED_STEPS,
        case,
        BTreeMap::from([(IMAGE.name.into(), image)]),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    let assertions = report["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), 4);
    assert!(
        assertions.iter().all(|assertion| assertion["status"] == "passed"),
        "{report:#}"
    );
}
