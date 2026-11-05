//! Step execution fixtures and readers for native lifecycle tests.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

use serde_json::{json, Value};
use tokio::sync::watch;

use super::*;
use crate::{
    artifact::ArtifactRole,
    case::{CaseLoader, Registries},
    endpoint::{Endpoint, Port, ResolvedAddress},
    layout::{Os, Realization, RealizationKind},
    observe::{
        exit_record::RecordRefresh, output::OutputLine, Capture, ParticipantExit, ProcessObservation, ProcessOutput,
    },
    participant::{EffectiveConfig, FactDeclaration, OpContext, Operation, ParticipantType},
    plan::Planner,
    probe::{AttemptLimits, ProbeAnswer},
};

#[derive(Clone, Copy)]
struct Fixture {
    name: &'static str,
    processes: &'static [&'static str],
    tls: bool,
    operations: &'static [&'static dyn Operation],
}

const DEFAULT_OPERATIONS: &[&dyn Operation] = &[&ConfigDump, &Dump];

impl ParticipantType for Fixture {
    fn name(&self) -> &'static str {
        self.name
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[]
    }
    fn endpoints(&self, _: Realization, _: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        let http = Endpoint::http(Port::Fixed(1003));
        Ok(BTreeMap::from([
            ("http".into(), if self.tls { http.with_tls() } else { http }),
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
    fn operations(&self, _: Realization) -> &'static [&'static dyn Operation] {
        self.operations
    }
    fn processes(&self) -> &'static [&'static str] {
        self.processes
    }
}

/// Executes a configuration dump command and decodes its JSON output.
pub struct ConfigDump;

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

/// Executes a record dump command and decodes its JSON output.
pub struct Dump;

#[async_trait]
impl Operation for Dump {
    fn name(&self) -> &'static str {
        "dump"
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| HandleError::Harness {
                message: "dump requires a kind".into(),
            })?;
        let output = cx.exec(vec!["dump".into(), kind.into()]).await?;
        serde_json::from_slice(&output.stdout).map_err(|_| HandleError::Product {
            message: "dump output is not JSON".into(),
        })
    }
}

/// An operation that executes its name and JSON arguments, returning stdout as JSON.
pub struct ExecOperation(pub &'static str);

#[async_trait]
impl Operation for ExecOperation {
    fn name(&self) -> &'static str {
        self.0
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let output = cx.exec(vec![self.0.into(), args.to_string()]).await?;
        if output.exit_code != 0 {
            return Err(HandleError::Product {
                message: format!("{} exited with {}", self.0, output.exit_code),
            });
        }
        serde_json::from_slice(&output.stdout).map_err(|_| HandleError::Product {
            message: format!("{} returned invalid JSON", self.0),
        })
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

/// Runs a step without a started participant, offering the complete operation slice in place of the defaults.
///
/// # Panics
///
/// Panics if the fields cannot load or compile with the fixture participant.
pub async fn run_step_offering(
    factory: &dyn StepFactory, fields: &StepFields, operations: &'static [&'static dyn Operation],
) -> StepReport {
    run_step_using(factory, fields, None, None, false, operations).await
}

/// Runs a step with a scripted started participant, or no handle when `fake` is `None`.
///
/// # Panics
///
/// Panics if the fields cannot load or compile with the fixture participant.
pub async fn run_step_with(
    factory: &dyn StepFactory, fields: &StepFields, fake: Option<&FakeParticipant>,
) -> StepReport {
    run_step_with_port(factory, fields, fake, None, false).await
}

/// Runs a step with a scripted participant and its HTTP request route reaching a loopback port.
///
/// # Panics
///
/// Panics if the fields cannot load or compile with the fixture participant and the single available port.
pub async fn run_step_reaching(
    factory: &dyn StepFactory, fields: &StepFields, fake: &FakeParticipant, port: u16,
) -> StepReport {
    run_step_with_port(factory, fields, Some(fake), Some(port), false).await
}

/// Runs a step with a scripted participant and its HTTPS route reaching a loopback port.
///
/// # Panics
///
/// Panics if the fields cannot load or compile with the fixture participant and the single available port.
pub async fn run_step_reaching_tls(
    factory: &dyn StepFactory, fields: &StepFields, fake: &FakeParticipant, port: u16,
) -> StepReport {
    run_step_with_port(factory, fields, Some(fake), Some(port), true).await
}

async fn run_step_with_port(
    factory: &dyn StepFactory, fields: &StepFields, fake: Option<&FakeParticipant>, port: Option<u16>, tls: bool,
) -> StepReport {
    run_step_using(
        factory,
        fields,
        fake,
        port,
        tls,
        fake.map_or(DEFAULT_OPERATIONS, |fake| fake.operations),
    )
    .await
}

async fn run_step_using(
    factory: &dyn StepFactory, fields: &StepFields, fake: Option<&FakeParticipant>, port: Option<u16>, tls: bool,
    operations: &'static [&'static dyn Operation],
) -> StepReport {
    let fixture = Fixture {
        name: "fixture",
        processes: fake.map_or(&["main", "worker"][..], |fake| fake.process_names),
        tls,
        operations,
    };
    let registries = Registries {
        participant_types: &[&fixture],
        steps: &[factory],
    };
    let mut fields = fields.clone();
    fields.insert("step".into(), factory.name().into());
    let content = json!({
        "timeout": "30s", "subject_os": ["linux"],
        "participants": {"subject": {"type": "fixture", "subject": true}},
        "procedure": [fields],
    })
    .to_string();
    let case = CaseLoader::new(registries)
        .load_content("cases", &content, "case", Path::new("cases/case/config.yaml"))
        .unwrap();
    let mut planner = Planner::new(registries, Default::default()).unwrap();
    if let Some(port) = port {
        planner = planner.with_port_range(port..=port);
    }
    let plan = planner.compile(case, Os::Linux).unwrap().into_plan().unwrap();
    let plan = plan.participants();
    let fakes = fake.map(|fake| ("subject", fake)).into_iter().collect::<Vec<_>>();
    run_planned_step(plan, &fakes, Path::new(".")).await
}

/// Runs a step with named scripted participants and a case artifact directory.
/// The participant named `subject`, or the first participant, is the subject.
///
/// # Panics
///
/// Panics for an empty participant list or a case that cannot load or compile.
pub async fn run_step_with_participants(
    factory: &dyn StepFactory, fields: &StepFields, participants: &[(&'static str, &FakeParticipant)], log_dir: &Path,
) -> StepReport {
    let fixtures = participants
        .iter()
        .map(|(name, fake)| Fixture {
            name,
            processes: fake.process_names,
            tls: false,
            operations: fake.operations,
        })
        .collect::<Vec<_>>();
    let types = fixtures
        .iter()
        .map(|fixture| fixture as &dyn ParticipantType)
        .collect::<Vec<_>>();
    let registries = Registries {
        participant_types: &types,
        steps: &[factory],
    };
    let subject = participants
        .iter()
        .find(|(name, _)| *name == "subject")
        .unwrap_or(&participants[0])
        .0;
    let declarations = participants
        .iter()
        .map(|(name, _)| (*name, json!({"type": name, "subject": *name == subject})))
        .collect::<BTreeMap<_, _>>();
    let mut fields = fields.clone();
    fields.insert("step".into(), factory.name().into());
    let content = json!({
        "timeout": "30s", "subject_os": ["linux"], "participants": declarations, "procedure": [fields],
    })
    .to_string();
    let case = CaseLoader::new(registries)
        .load_content("cases", &content, "case", Path::new("cases/case/config.yaml"))
        .unwrap();
    let plan = Planner::new(registries, Default::default())
        .unwrap()
        .compile(case, Os::Linux)
        .unwrap()
        .into_plan()
        .unwrap();
    run_planned_step(plan.participants(), participants, log_dir).await
}

async fn run_planned_step(
    plan: crate::plan::ParticipantPlan<'_>, fakes: &[(&str, &FakeParticipant)], log_dir: &Path,
) -> StepReport {
    let crate::case::ProcedureNode::Step(step) = &plan.procedure()[0] else {
        panic!("step tests have one sequential step");
    };
    let mut refreshers = tokio::task::JoinSet::new();
    for (_, fake) in fakes {
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
    let participants = fakes
        .iter()
        .map(|(name, fake)| {
            (
                *name,
                StepParticipant {
                    primitives: *fake,
                    observation: &fake.observation,
                    late: fake.late.clone(),
                },
            )
        })
        .collect();
    step.run(&StepContext::new(plan, participants, log_dir)).await
}

type FileAnswer = Result<Option<Vec<u8>>, HandleError>;

struct FakeProcess {
    refresh_delay: Option<Duration>,
    status: watch::Sender<ExitObservation>,
    requests: watch::Receiver<u64>,
    served: watch::Sender<u64>,
    reveal: Arc<Mutex<Option<ExitObservation>>>,
}

/// A participant with scripted output, exits, file reads, command output, probe answers and sends.
pub struct FakeParticipant {
    operations: &'static [&'static dyn Operation],
    late: BTreeMap<LateFact, &'static str>,
    observation: ParticipantObservation,
    exit: watch::Sender<ParticipantExit>,
    capture: watch::Sender<Capture>,
    process_captures: BTreeMap<&'static str, watch::Sender<Capture>>,
    files: Mutex<BTreeMap<String, FileAnswer>>,
    process_names: &'static [&'static str],
    processes: BTreeMap<&'static str, FakeProcess>,
    probes: Mutex<VecDeque<Result<ProbeAnswer, HandleError>>>,
    execs: Mutex<VecDeque<Result<ExecOutput, HandleError>>>,
    exec_delay: Duration,
    executed: Mutex<Vec<(ExecCommand, Duration)>>,
    sends: Mutex<VecDeque<Result<String, HandleError>>>,
    sent: Mutex<Vec<Vec<u8>>>,
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
            operations: DEFAULT_OPERATIONS,
            late: BTreeMap::new(),
            observation,
            exit,
            capture,
            process_captures,
            files: Mutex::new(BTreeMap::new()),
            process_names,
            processes,
            probes: Mutex::new(VecDeque::new()),
            execs: Mutex::new(VecDeque::new()),
            exec_delay: Duration::ZERO,
            executed: Mutex::new(Vec::new()),
            sends: Mutex::new(VecDeque::new()),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Replaces the default operations with the complete supplied slice.
    pub fn with_operations(mut self, operations: &'static [&'static dyn Operation]) -> Self {
        self.operations = operations;
        self
    }

    /// Supplies a late fact to steps running with this participant.
    pub fn set_late(&mut self, fact: LateFact, value: &'static str) {
        self.late.insert(fact, value);
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

    /// Delays each command before returning its scripted answer.
    pub fn delay_exec(&mut self, delay: Duration) {
        self.exec_delay = delay;
    }

    /// Checks every command and its execution limit in invocation order.
    ///
    /// # Panics
    ///
    /// Panics if the recorded commands or limits differ from `commands`.
    #[track_caller]
    pub fn assert_executed(&self, commands: &[(ExecCommand, Duration)]) {
        let executed = self.executed.lock().unwrap();
        assert_eq!(executed.len(), commands.len());
        for ((actual, actual_limit), (expected, expected_limit)) in executed.iter().zip(commands) {
            assert_eq!(actual.argv, expected.argv);
            assert_eq!(actual.env, expected.env);
            assert_eq!(actual.stdin, expected.stdin);
            assert_eq!(actual_limit, expected_limit);
        }
    }

    /// Queues a send result; the last queued answer repeats.
    pub fn push_send(&self, answer: Result<String, HandleError>) {
        self.sends.lock().unwrap().push_back(answer);
    }

    /// Checks the payload of every send attempt, in invocation order, including failed attempts.
    ///
    /// # Panics
    ///
    /// Panics if the recorded payloads differ from `payloads`.
    #[track_caller]
    pub fn assert_sent(&self, payloads: &[&[u8]]) {
        let sent = self.sent.lock().unwrap();
        assert_eq!(sent.iter().map(Vec::as_slice).collect::<Vec<_>>(), payloads);
    }

    /// Appends a line tagged with its process, or participant-wide output for `None`.
    pub fn push_line(&self, process: Option<&'static str>, text: &str) {
        self.observation.lines.push(OutputLine {
            process,
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
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError> {
        self.executed.lock().unwrap().push((command, limit));
        if !self.exec_delay.is_zero() {
            tokio::time::sleep(self.exec_delay).await;
        }
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
    async fn send(&self, _: &ResolvedAddress, payload: &[u8], _: &AttemptLimits) -> Result<String, HandleError> {
        self.sent.lock().unwrap().push(payload.to_vec());
        let mut sends = self.sends.lock().unwrap();
        if sends.len() > 1 {
            sends.pop_front().unwrap()
        } else {
            sends.front().expect("the step must script its send answers").clone()
        }
    }
    async fn http(
        &self, _: &ResolvedAddress, _: &str, _: bool, _: &AttemptLimits,
    ) -> Result<HttpResponse, HandleError> {
        panic!("this participant expects HTTP requests to use a direct route");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::AssertionStatus;

    #[tokio::test]
    async fn a_fake_participants_late_facts_reach_steps() {
        let factory = crate::case::fixtures::STEPS
            .iter()
            .find(|factory| factory.name() == "probe")
            .unwrap();
        let fields = serde_json::from_value(json!({"on": "subject", "path": "{{subject.late.ip}}"})).unwrap();
        let mut fake = FakeParticipant::default();
        fake.set_late(LateFact::Ip, "192.0.2.7");
        fake.set_file("192.0.2.7", Ok(Some(b"ready".to_vec())));
        let report = run_step_with(*factory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed, "{report:?}");
    }

    struct CallFactory;

    impl StepFactory for CallFactory {
        fn name(&self) -> &'static str {
            "call"
        }
        fn build(&self, _: &StepFields) -> Result<Box<dyn Step>, String> {
            Ok(Box::new(Call))
        }
    }

    #[derive(Debug)]
    struct Call;

    #[async_trait]
    impl Step for Call {
        fn name(&self) -> &'static str {
            "call"
        }
        fn needs(&self) -> Vec<Need> {
            ["config_dump", "dump", "extra"]
                .into_iter()
                .map(|operation| Need::Operation("subject".into(), operation.into()))
                .collect()
        }
        fn budget(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn run(&self, cx: &StepContext<'_>) -> StepReport {
            let started = tokio::time::Instant::now();
            let Some(handle) = cx.handle("subject") else {
                return StepReport::new(started, AssertionStatus::Errored, "not started");
            };
            for (name, args) in [
                ("config_dump", json!({"view": "source"})),
                ("dump", json!({"kind": "records"})),
                ("extra", json!({"value": 1})),
            ] {
                assert_eq!(
                    handle.op(name, args, self.budget()).await.unwrap(),
                    json!({"answer": 42})
                );
            }
            StepReport::new(started, AssertionStatus::Passed, "operations returned their answers")
        }
    }

    #[tokio::test]
    async fn a_fake_participant_offers_the_supplied_complete_operation_slice() {
        let fake = FakeParticipant::default().with_operations(&[&ConfigDump, &Dump, &ExecOperation("extra")]);
        fake.push_exec(Ok(ExecOutput {
            stdout: br#"{"answer":42}"#.to_vec(),
            stderr: vec![],
            exit_code: 0,
        }));
        let report = run_step_with(&CallFactory, &StepFields::new(), Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Passed);
        let command = |argv: &[&str]| {
            (
                ExecCommand {
                    argv: argv.iter().map(|arg| (*arg).into()).collect(),
                    env: vec![],
                    stdin: None,
                },
                Duration::from_secs(1),
            )
        };
        fake.assert_executed(&[
            command(&["dump", "source"]),
            command(&["dump", "records"]),
            command(&["extra", r#"{"value":1}"#]),
        ]);
        let report = run_step_offering(
            &CallFactory,
            &StepFields::new(),
            &[&ConfigDump, &Dump, &ExecOperation("extra")],
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Errored);
        assert_eq!(report.message, "not started");
    }

    #[tokio::test]
    async fn an_exec_operation_returns_command_json_and_classifies_bad_output_and_exits() {
        use crate::participant::test_support::run_operation;
        for (code, text, expected) in [
            (0, r#"{"ready":true}"#, Ok(json!({"ready": true}))),
            (
                0,
                "not JSON",
                Err(HandleError::Product {
                    message: "extra returned invalid JSON".into(),
                }),
            ),
            (
                7,
                r#"{"ready":true}"#,
                Err(HandleError::Product {
                    message: "extra exited with 7".into(),
                }),
            ),
        ] {
            let result = run_operation(&ExecOperation("extra"), json!({"key": "value"}), |command| {
                assert_eq!(command.argv, ["extra", r#"{"key":"value"}"#]);
                Ok(ExecOutput {
                    stdout: text.as_bytes().to_vec(),
                    stderr: vec![],
                    exit_code: code,
                })
            })
            .await;
            assert_eq!(result, expected);
        }
        let error = HandleError::Harness {
            message: "unavailable".into(),
        };
        assert_eq!(
            run_operation(&ExecOperation("extra"), json!({}), |_| Err(error.clone()))
                .await
                .unwrap_err(),
            error
        );
    }

    struct RequestFactory(EndpointOperation);

    impl StepFactory for RequestFactory {
        fn name(&self) -> &'static str {
            "request"
        }
        fn build(&self, _: &StepFields) -> Result<Box<dyn Step>, String> {
            Ok(Box::new(RequestStep(self.0)))
        }
    }

    #[derive(Debug)]
    struct RequestStep(EndpointOperation);

    #[async_trait]
    impl Step for RequestStep {
        fn name(&self) -> &'static str {
            "request"
        }
        fn needs(&self) -> Vec<Need> {
            vec![Need::Endpoint("subject".into(), "http".into(), self.0)]
        }
        fn budget(&self) -> Duration {
            Duration::from_secs(2)
        }
        async fn run(&self, cx: &StepContext<'_>) -> StepReport {
            let started = tokio::time::Instant::now();
            let handle = cx.handle("subject").unwrap();
            let (status, message) = match handle
                .http_get("http", "/records?key=value", &AttemptLimits::new(self.budget()))
                .await
            {
                Ok(response) => (
                    AssertionStatus::Passed,
                    json!({"status": response.status, "body": response.body}).to_string(),
                ),
                Err(HandleError::Harness { message }) => (AssertionStatus::Errored, message),
                Err(HandleError::Product { message }) => (AssertionStatus::Failed, message),
            };
            StepReport::new(started, status, message)
        }
    }

    #[tokio::test]
    async fn a_steps_http_get_uses_its_planned_request_route() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake = FakeParticipant::default();
        let fields = StepFields::new();
        let responder = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            assert_eq!(
                request,
                format!("GET /records?key=value HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n").as_bytes()
            );
            stream
                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 3\r\n\r\n\x00\xff\n")
                .await
                .unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let (_, report) = tokio::join!(
                responder,
                run_step_reaching(&RequestFactory(EndpointOperation::Request), &fields, &fake, port)
            );
            assert_eq!(report.status, AssertionStatus::Passed);
            assert_eq!(report.message, json!({"status": 202, "body": [0, 255, 10]}).to_string());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_steps_http_get_without_a_request_need_is_a_harness_error() {
        let fake = FakeParticipant::default();
        for operation in [EndpointOperation::Probe, EndpointOperation::Send] {
            let report = run_step_with(&RequestFactory(operation), &StepFields::new(), Some(&fake)).await;
            assert_eq!(report.status, AssertionStatus::Errored);
            assert_eq!(
                report.message,
                "no Request route for 'subject.http'; the step must declare an endpoint need"
            );
        }
    }

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
