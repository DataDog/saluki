//! Step execution fixtures and readers for native lifecycle tests.

use std::{path::Path, sync::Mutex, time::SystemTime};

use airlock::{exec::ExecOutput, files::FileStat};
use saluki_error::GenericError;
use serde_json::{json, Value};
use tokio::sync::watch;

use super::*;
use crate::{
    artifact::ArtifactRole,
    case::{CaseLoader, Registries, TypedCase, TypedCaseLoader},
    endpoint::{ResolvedAddress, RunnerHostRoute},
    handle::ExecCommand,
    layout::{Os, Realization, RealizationKind},
    observe::{output::OutputLine, Capture, ParticipantExit, ProcessOutput},
    participant::{FactDeclaration, OutputStream, ParticipantType},
    plan::Planner,
    probe::{AttemptLimits, ProbeAnswer},
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
        &[]
    }
    fn processes(&self) -> &'static [&'static str] {
        &["main", "worker"]
    }
    fn facts(&self, _: Realization) -> &'static [FactDeclaration] {
        &[FactDeclaration {
            name: "file",
            value: "/logs/output.log",
        }]
    }
}

impl TypedCaseLoader for Fixture {
    type Config = ();
    fn load(&self, _: &Value, _: &str, _: &str, _: &Path) -> Result<Vec<TypedCase<()>>, GenericError> {
        panic!("step tests declare participants");
    }
}

/// Builds and runs a step with a `subject` participant in its planned state.
///
/// # Panics
///
/// Panics if the fields cannot load or compile with the fixture participant.
pub async fn run_step(factory: &dyn StepFactory, fields: &StepFields) -> StepReport {
    run_step_with(factory, fields, None).await
}

/// Runs a step with a scripted started participant, or no handle when `fake` is `None`.
///
/// # Panics
///
/// Panics if the fields cannot load or compile with the fixture participant.
pub async fn run_step_with(
    factory: &dyn StepFactory, fields: &StepFields, fake: Option<&FakeParticipant>,
) -> StepReport {
    let registries = Registries {
        participant_types: &[&Fixture],
        steps: &[factory],
        typed_case_roles: &[],
    };
    let mut fields = fields.clone();
    fields.insert("step".into(), factory.name().into());
    let content = json!({
        "timeout": "30s", "subject_os": ["linux"],
        "participants": {"subject": {"type": "fixture", "subject": true}},
        "procedure": [fields],
    })
    .to_string();
    let case = CaseLoader::new(registries, Fixture, "fixture")
        .load_content(&content, "case", Path::new("cases/case/config.yaml"))
        .unwrap()
        .remove(0);
    let plan = Planner::new(registries, Default::default())
        .unwrap()
        .compile(case, Os::Linux)
        .unwrap()
        .into_plan()
        .unwrap();
    let plan = plan.participants().unwrap();
    let crate::case::ProcedureNode::Step(step) = &plan.procedure()[0] else {
        panic!("step tests have one sequential step");
    };
    let participants = fake
        .map(|fake| {
            BTreeMap::from([(
                "subject",
                StepParticipant {
                    primitives: fake,
                    observation: &fake.observation,
                    late: BTreeMap::new(),
                },
            )])
        })
        .unwrap_or_default();
    step.run(&StepContext::new(plan, participants)).await
}

type FileAnswer = Result<Option<Vec<u8>>, HandleError>;

/// A participant whose output, capture state and files can change while a step runs.
pub struct FakeParticipant {
    observation: ParticipantObservation,
    exit: watch::Sender<ParticipantExit>,
    capture: watch::Sender<Capture>,
    process_captures: BTreeMap<&'static str, watch::Sender<Capture>>,
    files: Mutex<BTreeMap<String, FileAnswer>>,
}

impl Default for FakeParticipant {
    fn default() -> Self {
        let (exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (capture, capture_rx) = watch::channel(Capture::Following);
        let mut observation = ParticipantObservation::new(exit_rx, capture_rx, Default::default(), Default::default());
        let process_captures = Fixture
            .processes()
            .iter()
            .map(|&name| {
                let (tx, rx) = watch::channel(Capture::Following);
                observation.outputs.insert(
                    name,
                    ProcessOutput {
                        capture: rx,
                        paths: Vec::new(),
                    },
                );
                (name, tx)
            })
            .collect();
        Self {
            observation,
            exit,
            capture,
            process_captures,
            files: Mutex::new(BTreeMap::new()),
        }
    }
}

impl FakeParticipant {
    /// Appends a line tagged with its process, or participant-wide output for `None`.
    pub fn push_line(&self, process: Option<&'static str>, text: &str) {
        self.observation.lines.push(OutputLine {
            participant: "subject".into(),
            process,
            stream: OutputStream::Merged,
            received: SystemTime::now(),
            text: text.into(),
        });
    }

    /// Ends all captures and records the participant's exit.
    pub fn complete(&self) {
        self.exit.send_replace(ParticipantExit::Exited { code: 0, error: None });
        self.capture.send_replace(Capture::Complete);
        for capture in self.process_captures.values() {
            capture.send_replace(Capture::Complete);
        }
    }

    /// Fails the participant-wide capture.
    pub fn fail_capture(&self, reason: &str) {
        self.capture.send_replace(Capture::Failed { reason: reason.into() });
    }

    /// Sets the result of subsequent reads of a path.
    pub fn set_file(&self, path: &str, result: Result<Option<Vec<u8>>, HandleError>) {
        self.files.lock().unwrap().insert(path.into(), result);
    }
}

#[async_trait]
impl Primitives for FakeParticipant {
    async fn exec(&self, _: ExecCommand, _: Duration) -> Result<ExecOutput, HandleError> {
        panic!("file and output steps do not execute commands");
    }
    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        self.files.lock().unwrap().get(path).cloned().unwrap_or(Ok(None))
    }
    async fn stat(&self, _: &str) -> Result<Option<FileStat>, HandleError> {
        panic!("file and output steps do not stat files");
    }
    async fn probe(&self, _: &ResolvedAddress, _: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        panic!("file and output steps do not probe endpoints");
    }
    async fn send(&self, _: &ResolvedAddress, _: &[u8], _: &AttemptLimits) -> Result<(), HandleError> {
        panic!("file and output steps do not send payloads");
    }
}

/// Reads a started participant's container ID for daemon-side fault injection.
pub fn container_id(cx: &StepContext<'_>, on: &str) -> Option<String> {
    cx.late_fact(on, LateFact::ContainerId).map(str::to_owned)
}

/// Waits for a named process, reporting a step error if its exit is unknown.
///
/// # Panics
///
/// Panics if the step did not declare an existing participant and process.
pub async fn wait_for_exit(cx: &StepContext<'_>, on: &str, process: &str) -> StepReport {
    use crate::{observe::ExitObservation, record::AssertionStatus};
    let started = tokio::time::Instant::now();
    let observation = cx
        .handle(on)
        .unwrap()
        .observation
        .process(process)
        .unwrap()
        .settled()
        .await;
    let (status, message) = match observation {
        ExitObservation::Exited(status) if status.code == Some(0) => (AssertionStatus::Passed, "process exited".into()),
        ExitObservation::Exited(status) => (AssertionStatus::Failed, format!("process exited: {status:?}")),
        ExitObservation::Undetermined(reason) => (AssertionStatus::Errored, reason),
        ExitObservation::Running => unreachable!("settled waits for a terminal observation"),
    };
    StepReport::new(started, status, message)
}

/// Waits for an exact output line, reporting a step error if capture is lost or overflows.
///
/// # Panics
///
/// Panics if the step did not declare an existing participant.
pub async fn wait_for_output(cx: &StepContext<'_>, on: &str, text: &str) -> StepReport {
    use crate::{probe::POLL_INTERVAL, record::AssertionStatus};
    let started = tokio::time::Instant::now();
    let handle = cx.handle(on).unwrap();
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

/// Returns a direct runner-host route's port, or `None` for a missing or courier route.
pub fn runner_host_port(
    cx: &StepContext<'_>, server: &str, endpoint: &str, operation: EndpointOperation,
) -> Option<u16> {
    match cx.runner_host_route(server, endpoint, operation)? {
        RunnerHostRoute::Direct(address) => address.port,
        RunnerHostRoute::Courier(_) => None,
    }
}

/// Sends bytes through the endpoint's planned runner-host route, with a ten-second attempt limit.
///
/// # Errors
///
/// Returns an error for a missing route or participant, or a failed send.
pub async fn send(cx: &StepContext<'_>, server: &str, endpoint: &str, payload: &[u8]) -> Result<(), String> {
    let route = cx
        .runner_host_route(server, endpoint, EndpointOperation::Send)
        .ok_or_else(|| format!("no send route for '{server}.{endpoint}'"))?;
    let handle = cx
        .handle(server)
        .ok_or_else(|| format!("participant '{server}' has not started"))?;
    crate::handle::send_from_runner_host(
        route,
        handle.primitives,
        payload,
        &crate::probe::AttemptLimits::new(Duration::from_secs(10)),
    )
    .await
    .map_err(|error| error.to_string())
}

/// Runs a command through a started participant's backend with a ten-second limit.
///
/// # Errors
///
/// Returns an error if the participant is missing or execution fails.
pub async fn exec(cx: &StepContext<'_>, on: &str, argv: &[&str]) -> Result<airlock::exec::ExecOutput, String> {
    let handle = cx
        .handle(on)
        .ok_or_else(|| format!("participant '{on}' has not started"))?;
    handle
        .primitives
        .exec(
            crate::handle::ExecCommand {
                argv: argv.iter().map(|arg| (*arg).into()).collect(),
                env: Vec::new(),
                stdin: None,
            },
            Duration::from_secs(10),
        )
        .await
        .map_err(|error| error.to_string())
}

/// Returns the named process's current exit report, or `None` if it is not observed.
pub fn process_exit(cx: &StepContext<'_>, on: &str, process: &str) -> Option<crate::result::ProcessExitReport> {
    Some(cx.handle(on)?.observation.process(process)?.snapshot())
}

/// Reads the named process's retained output lines.
///
/// # Errors
///
/// Returns an error for missing observations, a failed capture or an overflowed line buffer.
pub fn process_output(cx: &StepContext<'_>, on: &str, process: &str) -> Result<Vec<String>, String> {
    let handle = cx
        .handle(on)
        .ok_or_else(|| format!("participant '{on}' has not started"))?;
    let batch = handle.output(Some(process)).read();
    match batch.end {
        OutputEnd::Failed(reason) => Err(reason),
        OutputEnd::Open | OutputEnd::Complete => Ok(batch.lines),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_cursors_filter_processes_and_advance_independently() {
        let fake = FakeParticipant::default();
        let handle = Handle {
            primitives: &fake,
            observation: &fake.observation,
        };
        let mut all = handle.output(None);
        let mut worker = handle.output(Some("worker"));
        fake.push_line(None, "shared");
        fake.push_line(Some("main"), "main line");
        fake.push_line(Some("worker"), "worker line");
        assert_eq!(all.read().lines, ["shared", "main line", "worker line"]);
        assert_eq!(worker.read().lines, ["worker line"]);
        assert!(all.read().lines.is_empty());
        fake.push_line(Some("worker"), "next");
        assert_eq!(worker.read().lines, ["next"]);
        assert_eq!(all.read().lines, ["next"]);
    }

    #[test]
    fn output_completes_only_after_exit_and_every_capture_finishes() {
        for exit_first in [false, true] {
            let fake = FakeParticipant::default();
            let handle = Handle {
                primitives: &fake,
                observation: &fake.observation,
            };
            let mut output = handle.output(Some("main"));
            fake.capture.send_replace(Capture::Complete);
            fake.process_captures["main"].send_replace(Capture::Complete);
            if exit_first {
                fake.exit.send_replace(ParticipantExit::Exited { code: 0, error: None });
            }
            assert_eq!(output.read().end, OutputEnd::Open);
            fake.push_line(Some("main"), "last");
            fake.process_captures["worker"].send_replace(Capture::Complete);
            let batch = output.read();
            assert_eq!(batch.lines, ["last"]);
            assert_eq!(
                batch.end,
                if exit_first {
                    OutputEnd::Complete
                } else {
                    OutputEnd::Open
                }
            );
            fake.exit.send_replace(ParticipantExit::Exited { code: 0, error: None });
            assert_eq!(output.read().end, OutputEnd::Complete);
        }
    }

    #[test]
    fn filtered_output_reports_a_failure_in_another_processs_capture() {
        let fake = FakeParticipant::default();
        let handle = Handle {
            primitives: &fake,
            observation: &fake.observation,
        };
        let mut output = handle.output(Some("worker"));
        fake.process_captures["main"].send_replace(Capture::Failed {
            reason: "main capture lost".into(),
        });
        fake.push_line(Some("worker"), "worker line");
        let batch = output.read();
        assert_eq!(batch.lines, ["worker line"]);
        assert_eq!(batch.end, OutputEnd::Failed("main capture lost".into()));
    }

    #[test]
    fn output_returns_retained_lines_before_capture_failure_or_overflow() {
        let mut fake = FakeParticipant::default();
        fake.observation.lines = crate::observe::output::OutputLines::new(5);
        let handle = Handle {
            primitives: &fake,
            observation: &fake.observation,
        };
        let mut output = handle.output(None);
        fake.push_line(None, "first");
        fake.fail_capture("capture lost");
        let batch = output.read();
        assert_eq!(batch.lines, ["first"]);
        assert_eq!(batch.end, OutputEnd::Failed("capture lost".into()));
        fake.push_line(None, "overflow");
        let OutputEnd::Failed(reason) = output.read().end else {
            panic!("overflow must fail capture")
        };
        assert!(reason.contains("output capture failed"), "{reason}");
    }
}
