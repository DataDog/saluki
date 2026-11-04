//! Step execution fixtures and readers for native lifecycle tests.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use saluki_error::GenericError;
use serde_json::{json, Value};
use tokio::sync::watch;

use super::*;
use crate::{
    artifact::ArtifactRole,
    case::{CaseLoader, Registries, TypedCase, TypedCaseLoader},
    endpoint::{Endpoint, Port, ResolvedAddress},
    layout::{Os, Realization, RealizationKind},
    observe::{
        exit_record::RecordRefresh, output::OutputLine, Capture, ParticipantExit, ProcessObservation, ProcessOutput,
    },
    participant::{EffectiveConfig, FactDeclaration, OpContext, Operation, OutputStream, ParticipantType},
    plan::Planner,
    probe::{AttemptLimits, ProbeAnswer},
};

struct Fixture(&'static [&'static str]);

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
    fn operations(&self, _: Realization) -> &'static [&'static dyn Operation] {
        &[&ConfigDump]
    }
    fn processes(&self) -> &'static [&'static str] {
        self.0
    }
    fn endpoints(&self, _: Realization, _: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        Ok(BTreeMap::from([
            ("tcp".into(), Endpoint::tcp(Port::Fixed(1001))),
            ("udp".into(), Endpoint::udp(Port::Fixed(1002))),
        ]))
    }
    fn facts(&self, _: Realization) -> &'static [FactDeclaration] {
        &[FactDeclaration {
            name: "file",
            value: "/logs/output.log",
        }]
    }
}

pub(crate) struct ConfigDump;

#[async_trait]
impl Operation for ConfigDump {
    fn name(&self) -> &'static str {
        "config_dump"
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let view = args
            .get("view")
            .and_then(Value::as_str)
            .ok_or_else(|| HandleError::Harness {
                message: "config_dump requires a view".into(),
            })?;
        let output = cx.exec(vec!["dump".into(), view.into()]).await?;
        serde_json::from_slice(&output.stdout).map_err(|_| HandleError::Product {
            message: "configuration output is not JSON".into(),
        })
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
    let fixture = Fixture(fake.map_or(&["main", "worker"][..], |fake| fake.process_names));
    let registries = Registries {
        participant_types: &[&fixture],
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
    let case = CaseLoader::new(registries, Fixture(fixture.0), "fixture")
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
    let mut refreshers = tokio::task::JoinSet::new();
    if let Some(fake) = fake {
        for process in fake.processes.values() {
            let mut requests = process.requests.clone();
            let served = process.served.clone();
            let status = process.status.clone();
            let reveal = process.reveal.clone();
            let delay = process.refresh_delay;
            refreshers.spawn(async move {
                while requests.changed().await.is_ok() {
                    let ticket = *requests.borrow_and_update();
                    match delay {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => std::future::pending().await,
                    }
                    if let Some(observation) = reveal.lock().unwrap().take() {
                        status.send_replace(observation);
                    }
                    served.send_replace(ticket);
                }
            });
        }
    }
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

struct FakeProcess {
    refresh_delay: Option<Duration>,
    status: watch::Sender<ExitObservation>,
    requests: watch::Receiver<u64>,
    served: watch::Sender<u64>,
    reveal: Arc<Mutex<Option<ExitObservation>>>,
}

/// A participant with scripted output, exits, file reads, command output and probe answers.
pub struct FakeParticipant {
    observation: ParticipantObservation,
    exit: watch::Sender<ParticipantExit>,
    capture: watch::Sender<Capture>,
    process_captures: BTreeMap<&'static str, watch::Sender<Capture>>,
    files: Mutex<BTreeMap<String, FileAnswer>>,
    process_names: &'static [&'static str],
    processes: BTreeMap<&'static str, FakeProcess>,
    probes: Mutex<VecDeque<Result<ProbeAnswer, HandleError>>>,
    execs: Mutex<VecDeque<Result<ExecOutput, HandleError>>>,
}

impl Default for FakeParticipant {
    fn default() -> Self {
        Self::new(&["main", "worker"])
    }
}

impl FakeParticipant {
    /// Creates a participant declaring the given process names.
    pub fn new(process_names: &'static [&'static str]) -> Self {
        let (exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (capture, capture_rx) = watch::channel(Capture::Following);
        let mut observation = ParticipantObservation::new(exit_rx, capture_rx, Default::default(), Default::default());
        let process_captures = process_names
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
        let processes = process_names
            .iter()
            .map(|&name| {
                let (status, status_rx) = watch::channel(ExitObservation::Running);
                let (requests, requests_rx) = watch::channel(0);
                let (served, served_rx) = watch::channel(0);
                observation.processes.insert(
                    name,
                    ProcessObservation::Record {
                        status: status_rx,
                        refresh: Some(Arc::new(RecordRefresh {
                            requests,
                            served: served_rx,
                        })),
                    },
                );
                (
                    name,
                    FakeProcess {
                        refresh_delay: Some(Duration::ZERO),
                        status,
                        requests: requests_rx,
                        served,
                        reveal: Arc::new(Mutex::new(None)),
                    },
                )
            })
            .collect();
        Self {
            observation,
            exit,
            capture,
            process_captures,
            files: Mutex::new(BTreeMap::new()),
            process_names,
            processes,
            probes: Mutex::new(VecDeque::new()),
            execs: Mutex::new(VecDeque::new()),
        }
    }

    /// Sets the named process's exit observation.
    ///
    /// # Panics
    ///
    /// Panics for a process the fixture does not declare.
    pub fn set_exit(&self, process: &str, observation: ExitObservation) {
        self.processes[process].status.send_replace(observation);
    }

    /// Reveals an exit only when the running step requests a re-read.
    ///
    /// # Panics
    ///
    /// Panics for a process the fixture does not declare.
    pub fn reveal_on_refresh(&self, process: &str, observation: ExitObservation) {
        *self.processes[process].reveal.lock().unwrap() = Some(observation);
    }

    /// Delays a process's re-read response; `None` leaves requests pending.
    ///
    /// # Panics
    ///
    /// Panics for a process the fixture does not declare.
    pub fn delay_refresh(&mut self, process: &str, delay: Option<Duration>) {
        self.processes.get_mut(process).unwrap().refresh_delay = delay;
    }

    /// Queues a probe result; the last queued answer repeats.
    pub fn push_probe(&self, answer: Result<ProbeAnswer, HandleError>) {
        self.probes.lock().unwrap().push_back(answer);
    }

    /// Queues command output; the last queued answer repeats.
    pub fn push_exec(&self, answer: Result<ExecOutput, HandleError>) {
        self.execs.lock().unwrap().push_back(answer);
    }

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
        let mut execs = self.execs.lock().unwrap();
        if execs.len() > 1 {
            execs.pop_front().unwrap()
        } else {
            execs.front().expect("the step must script its command output").clone()
        }
    }
    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        self.files.lock().unwrap().get(path).cloned().unwrap_or(Ok(None))
    }
    async fn stat(&self, _: &str) -> Result<Option<FileStatus>, HandleError> {
        panic!("file and output steps do not stat files");
    }
    async fn probe(&self, _: &ResolvedAddress, _: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        let mut probes = self.probes.lock().unwrap();
        if probes.len() > 1 {
            probes.pop_front().unwrap()
        } else {
            probes.front().expect("the step must script its probe answers").clone()
        }
    }
    async fn send(&self, _: &ResolvedAddress, _: &[u8], _: &AttemptLimits) -> Result<(), HandleError> {
        panic!("file and output steps do not send payloads");
    }
}

/// Returns a direct request route's port for tests that issue their own HTTP requests.
/// Returns `None` for a missing or courier route.
pub fn runner_host_port(
    cx: &StepContext<'_>, server: &str, endpoint: &str, operation: EndpointOperation,
) -> Option<u16> {
    match cx.plan.runner_host_route(server, endpoint, operation)? {
        RunnerHostRoute::Direct(address) => address.port,
        RunnerHostRoute::Courier(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_cursors_filter_processes_and_advance_independently() {
        let fake = FakeParticipant::default();
        let mut all = OutputCursor {
            observation: &fake.observation,
            process: None,
            cursor: 0,
        };
        let mut worker = OutputCursor {
            observation: &fake.observation,
            process: Some("worker"),
            cursor: 0,
        };
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
            let mut output = OutputCursor {
                observation: &fake.observation,
                process: None,
                cursor: 0,
            };
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
    fn filtered_output_ignores_other_captures_and_reports_its_own_failure() {
        let fake = FakeParticipant::default();
        let mut output = OutputCursor {
            observation: &fake.observation,
            process: Some("worker"),
            cursor: 0,
        };
        fake.process_captures["main"].send_replace(Capture::Failed {
            reason: "main capture lost".into(),
        });
        fake.push_line(Some("worker"), "worker line");
        let batch = output.read();
        assert_eq!(batch.lines, ["worker line"]);
        assert_eq!(batch.end, OutputEnd::Open);
        fake.fail_capture("participant capture lost");
        assert_eq!(output.read().end, OutputEnd::Open);
        fake.process_captures["worker"].send_replace(Capture::Failed {
            reason: "worker capture lost".into(),
        });
        assert_eq!(output.read().end, OutputEnd::Failed("worker capture lost".into()));
    }

    #[test]
    fn a_process_owning_the_container_output_reports_its_capture_failure() {
        let mut fake = FakeParticipant::default();
        fake.observation.outputs.get_mut("main").unwrap().capture = fake.capture.subscribe();
        let mut output = OutputCursor {
            observation: &fake.observation,
            process: Some("main"),
            cursor: 0,
        };
        fake.fail_capture("log stream lost");
        assert_eq!(output.read().end, OutputEnd::Failed("log stream lost".into()));
    }

    #[test]
    fn an_all_process_cursor_reports_any_process_capture_failure() {
        let fake = FakeParticipant::default();
        let mut output = OutputCursor {
            observation: &fake.observation,
            process: None,
            cursor: 0,
        };
        fake.process_captures["worker"].send_replace(Capture::Failed {
            reason: "worker capture lost".into(),
        });
        assert_eq!(output.read().end, OutputEnd::Failed("worker capture lost".into()));
    }

    #[test]
    fn filtered_output_completes_after_its_capture_and_the_participant_exit() {
        for exit_first in [false, true] {
            let fake = FakeParticipant::default();
            let mut output = OutputCursor {
                observation: &fake.observation,
                process: Some("main"),
                cursor: 0,
            };
            if exit_first {
                fake.exit.send_replace(ParticipantExit::Exited { code: 0, error: None });
            } else {
                fake.process_captures["main"].send_replace(Capture::Complete);
            }
            assert_eq!(output.read().end, OutputEnd::Open);
            fake.exit.send_replace(ParticipantExit::Exited { code: 0, error: None });
            fake.process_captures["main"].send_replace(Capture::Complete);
            assert_eq!(output.read().end, OutputEnd::Complete);
        }
    }

    #[test]
    fn line_buffer_overflow_fails_a_process_filtered_cursor() {
        let mut fake = FakeParticipant::default();
        fake.observation.lines = crate::observe::output::OutputLines::new(1);
        let mut output = OutputCursor {
            observation: &fake.observation,
            process: Some("worker"),
            cursor: 0,
        };
        fake.push_line(Some("main"), "overflow");
        assert!(
            matches!(output.read().end, OutputEnd::Failed(reason) if reason.contains("in-memory output exceeded the 1-byte line-text bound"))
        );
    }

    #[test]
    fn output_returns_retained_lines_before_capture_failure_or_overflow() {
        let mut fake = FakeParticipant::default();
        fake.observation.lines = crate::observe::output::OutputLines::new(5);
        let mut output = OutputCursor {
            observation: &fake.observation,
            process: None,
            cursor: 0,
        };
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
