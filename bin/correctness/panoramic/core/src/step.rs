//! Step declarations and field decoding.

use std::{collections::BTreeMap, fmt, time::Duration};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use crate::handle::{ExecCommand, ExecOutput, FileKind, FileStatus, HandleError};
pub use crate::observe::{ExitObservation, ExitStatus};
use crate::{
    backend::LateFact,
    case::{template::Reference, StepTemplate},
    endpoint::{EndpointOperation, RunnerHostRoute},
    handle::{probe_from_runner_host, send_from_runner_host, Primitives},
    observe::{Capture, ParticipantExit, ParticipantObservation},
    participant::OpContext,
    plan::ParticipantPlan,
    probe::{AttemptLimits, ProbeAnswer},
    record::StepReport,
};

const EXIT_REFRESH_LIMIT: Duration = Duration::from_secs(5);

/// A step's fields as written in the case file, keyed by field name.
pub type StepFields = BTreeMap<String, Value>;

/// A dependency of a built step.
///
/// Loading checks participant and process names. Planning checks operations, facts, late facts and endpoint reachability
/// on each subject OS.
#[derive(Debug, PartialEq)]
pub enum Need {
    /// The participant the step runs on.
    Handle(String),
    /// One named process of a participant: (participant, process).
    Process(String, String),
    /// A named operation: (participant, operation).
    Operation(String, String),
    /// An early fact of a participant: (participant, fact).
    Fact(String, String),
    /// A late fact of a participant: (participant, fact). Known only once the participant exists or runs.
    Late(String, LateFact),
    /// An endpoint operation from the runner host: (participant, endpoint, operation).
    Endpoint(String, String, EndpointOperation),
}

/// A step built from its case-file fields.
#[async_trait]
pub trait Step: fmt::Debug + Send + Sync {
    /// Registry name, as written in the case procedure.
    fn name(&self) -> &'static str;

    /// Participants, processes, operations, facts, late facts and endpoints the step uses.
    fn needs(&self) -> Vec<Need>;

    /// The step's own time budget.
    fn budget(&self) -> Duration;

    /// Runs against the started participants and their observations.
    /// Release owned resources on drop: the driver may drop this future on cancellation or a budget overrun.
    async fn run(&self, cx: &StepContext<'_>) -> StepReport;
}

/// Started participants and the planned values available while a procedure runs.
pub struct StepContext<'a> {
    plan: ParticipantPlan<'a>,
    participants: BTreeMap<&'a str, StepParticipant<'a>>,
}

pub(crate) struct StepParticipant<'a> {
    pub primitives: &'a dyn Primitives,
    pub observation: &'a ParticipantObservation,
    pub late: BTreeMap<LateFact, &'a str>,
}

impl<'a> StepContext<'a> {
    pub(crate) fn new(plan: ParticipantPlan<'a>, participants: BTreeMap<&'a str, StepParticipant<'a>>) -> Self {
        Self { plan, participants }
    }

    /// Returns a borrowed handle, or `None` if the participant did not start.
    pub fn handle(&self, name: &str) -> Option<Handle<'_>> {
        self.participants
            .get_key_value(name)
            .map(|(&name, participant)| Handle {
                name,
                plan: self.plan,
                late: &participant.late,
                primitives: participant.primitives,
                observation: participant.observation,
            })
    }

    pub(crate) fn late_fact(&self, name: &str, fact: LateFact) -> Option<&str> {
        self.participants.get(name)?.late.get(&fact).copied()
    }

    /// Resolves a step template using planned facts and the started participants' late facts.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing fact or a substitution that introduces a template reference.
    pub fn resolve(&self, template: &StepTemplate) -> Result<String, String> {
        template.0.substitute(|written, reference| {
            let value = match reference {
                Reference::Fact { participant, fact } => self
                    .plan
                    .fact(participant.name(""), fact)
                    .ok_or_else(|| format!("participant '{participant}' has no fact '{fact}'"))?,
                Reference::Late { participant, fact } => self
                    .late_fact(participant.name(""), *fact)
                    .ok_or_else(|| format!("participant '{participant}' has no late fact '{}'", fact.name()))?,
                _ => return Err(format!("cannot resolve step reference '{written}'")),
            };
            Ok(Some(value.to_owned()))
        })
    }
}

/// A started participant's observations and operations, borrowed for a step.
pub struct Handle<'a> {
    name: &'a str,
    plan: ParticipantPlan<'a>,
    late: &'a BTreeMap<LateFact, &'a str>,
    primitives: &'a dyn Primitives,
    observation: &'a ParticipantObservation,
}

impl Handle<'_> {
    /// The participant's case name.
    pub fn name(&self) -> &str {
        self.name
    }

    /// A fact learned when creating or starting the participant.
    pub fn late(&self, fact: LateFact) -> Option<&str> {
        self.late.get(&fact).copied()
    }

    /// Declared process names in sorted order.
    pub fn processes(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.observation.processes.keys().copied()
    }

    /// Reads output from its beginning, optionally restricted to a named process.
    pub fn output<'a>(&'a self, process: Option<&'a str>) -> OutputCursor<'a> {
        OutputCursor {
            observation: self.observation,
            process,
            cursor: 0,
        }
    }

    /// Reads a regular file, returning `None` if it is missing.
    ///
    /// # Errors
    ///
    /// Distinguishes a failed read mechanism from an unsuccessful product answer.
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
        self.primitives.read_file(path).await
    }

    /// Runs a command with the given limit; every exit code is returned as data.
    ///
    /// # Errors
    ///
    /// Returns a harness error if execution fails or exceeds the limit.
    pub async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError> {
        self.primitives.exec(command, limit).await
    }

    /// Runs a named operation offered by this participant's planned realization.
    ///
    /// # Errors
    ///
    /// Returns a harness error for an unknown operation or failed mechanism, or a product error for a bad answer.
    pub async fn op(&self, name: &str, args: Value, limit: Duration) -> Result<Value, HandleError> {
        let operation = self
            .plan
            .operations(self.name)
            .iter()
            .find(|operation| operation.name() == name)
            .ok_or_else(|| HandleError::Harness {
                message: format!("participant '{}' has no operation '{name}'", self.name),
            })?;
        operation
            .run(
                &OpContext {
                    primitives: self.primitives,
                    limit,
                },
                args,
            )
            .await
    }

    /// The process's current exit observation, or an error for an undeclared process.
    pub fn exit(&self, process: &str) -> ExitObservation {
        match self.observation.process(process) {
            Some(observation) => observation.current(),
            None => ExitObservation::Undetermined(format!("participant '{}' has no process '{process}'", self.name)),
        }
    }

    /// Waits for the first exit or an observation error.
    pub async fn wait_for_exit(&self, process: &str) -> ExitObservation {
        match self.observation.process(process) {
            Some(observation) => observation.settled().await,
            None => self.exit(process),
        }
    }

    /// Re-reads the process's exit channel, bounded to five seconds.
    pub async fn refresh_exit(&self, process: &str) -> ExitObservation {
        match self.observation.process(process) {
            Some(observation) => tokio::time::timeout(EXIT_REFRESH_LIMIT, observation.refresh())
                .await
                .unwrap_or_else(|_| {
                    ExitObservation::Undetermined(format!(
                        "exit record of process '{process}' was not re-read within {EXIT_REFRESH_LIMIT:?}"
                    ))
                }),
            None => self.exit(process),
        }
    }

    /// Probes an endpoint through its planned runner-host route.
    ///
    /// # Errors
    ///
    /// Returns a harness error for a missing endpoint need or failed probe mechanism.
    pub async fn probe(&self, endpoint: &str, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        let route = self.route(endpoint, EndpointOperation::Probe)?;
        probe_from_runner_host(route, self.primitives, limits).await
    }

    /// Sends bytes through an endpoint's planned runner-host route.
    ///
    /// # Errors
    ///
    /// Distinguishes a missing endpoint need or failed mechanism from an unsuccessful product answer.
    pub async fn send(&self, endpoint: &str, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError> {
        let route = self.route(endpoint, EndpointOperation::Send)?;
        send_from_runner_host(route, self.primitives, payload, limits).await
    }

    fn route(&self, endpoint: &str, operation: EndpointOperation) -> Result<&RunnerHostRoute, HandleError> {
        self.plan
            .runner_host_route(self.name, endpoint, operation)
            .ok_or_else(|| HandleError::Harness {
                message: format!(
                    "no {operation:?} route for '{}.{endpoint}'; the step must declare an endpoint need",
                    self.name
                ),
            })
    }
}

/// An independent position in a participant's captured output.
pub struct OutputCursor<'a> {
    observation: &'a ParticipantObservation,
    process: Option<&'a str>,
    cursor: usize,
}

/// Lines read since the previous batch and the capture state observed before reading them.
pub struct OutputBatch {
    /// Captured lines, with the optional process filter applied.
    pub lines: Vec<String>,
    /// Whether output can still arrive or capture failed.
    pub end: OutputEnd,
}

/// The state of a participant's output capture.
#[derive(Debug, PartialEq, Eq)]
pub enum OutputEnd {
    /// More output may arrive.
    Open,
    /// The participant exited and every selected output capture finished.
    Complete,
    /// A selected process's capture failed, or the shared retained line buffer overflowed.
    /// A cursor over all output reports failures from any capture.
    Failed(String),
}

impl OutputCursor<'_> {
    /// Reads available lines before reporting completion or capture failure.
    pub fn read(&mut self) -> OutputBatch {
        // Snapshot captures first so a final batch cannot be skipped on completion.
        let captures = match self.process {
            Some(process) => vec![self
                .observation
                .outputs
                .get(process)
                .map(|output| output.current())
                .unwrap_or_else(|| Capture::Failed {
                    reason: format!("no output capture for process '{process}'"),
                })],
            None => std::iter::once(self.observation.capture())
                .chain(self.observation.outputs.values().map(|output| output.current()))
                .collect::<Vec<_>>(),
        };
        let end = if let Some(Capture::Failed { reason }) = captures
            .iter()
            .find(|capture| matches!(capture, Capture::Failed { .. }))
        {
            OutputEnd::Failed(reason.clone())
        } else if captures.iter().all(|capture| *capture == Capture::Complete)
            && matches!(self.observation.exit(), ParticipantExit::Exited { .. })
        {
            OutputEnd::Complete
        } else {
            OutputEnd::Open
        };
        match self.observation.lines_since(self.cursor) {
            Ok((lines, next)) => {
                self.cursor = next;
                OutputBatch {
                    lines: lines
                        .into_iter()
                        .filter(|line| self.process.is_none() || line.process == self.process)
                        .map(|line| line.text)
                        .collect(),
                    end,
                }
            }
            Err(reason) => OutputBatch {
                lines: Vec::new(),
                end: OutputEnd::Failed(reason),
            },
        }
    }
}

/// Builds a named step from its case-file fields at load time.
///
/// The loader builds steps and checks participant and process names; planning checks operations, facts,
/// late facts and endpoints through [`Step::needs`].
pub trait StepFactory: Send + Sync {
    /// Registry name, as written in a node's `step`.
    fn name(&self) -> &'static str;

    /// The only fields in which the loader accepts `{{ … }}` template references; defaults to none.
    ///
    /// The built step must report each field's references through [`StepTemplate::needs`].
    fn template_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Decodes and validates fields without performing I/O or running the step.
    ///
    /// Report dependencies through [`Step::needs`] for load-time and plan-time checks.
    ///
    /// # Errors
    ///
    /// Returns the first problem with the step's fields.
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String>;
}

/// Decodes step fields into `T`; errors name the failing field.
pub fn decode_fields<T>(fields: &StepFields) -> Result<T, String>
where
    T: DeserializeOwned,
{
    serde_path_to_error::deserialize(Value::Object(fields.clone().into_iter().collect())).map_err(|e| e.to_string())
}

#[cfg(any(test, feature = "test-util"))]
pub mod test_support;

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    use super::*;
    use crate::{
        case::fixtures::{self, FixtureConfig},
        case::ProcedureNode,
        endpoint::ResolvedAddress,
        layout::Os,
        observe::{Capture, ParticipantExit},
        plan::{CasePlan, Planner},
        probe::{AttemptLimits, ProbeAnswer},
        record::AssertionStatus,
    };

    const FACT_CASE: &str = "timeout: 30s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: [{step: probe, on: subject, path: '{{ subject.facts.file }}'}]";

    fn compile(yaml: &str) -> CasePlan<FixtureConfig> {
        let case = fixtures::LOADER
            .load_content(yaml, "case", std::path::Path::new("case/config.yaml"))
            .unwrap()
            .remove(0);
        Planner::new(fixtures::REGISTRIES, Default::default())
            .unwrap()
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    struct Backend;

    #[async_trait]
    impl Primitives for Backend {
        async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
            Ok((path == "/logs/output.log/created-id/192.0.2.7").then(Vec::new))
        }
        async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError> {
            if command.argv == ["dump", "runtime"] {
                assert!(command.env.is_empty());
                assert!(command.stdin.is_none());
                assert_eq!(limit, Duration::from_secs(2));
                return Ok(ExecOutput {
                    stdout: br#"{"enabled":true}"#.to_vec(),
                    stderr: Vec::new(),
                    exit_code: 0,
                });
            }
            assert_eq!(command.argv, ["tool", "argument"]);
            assert_eq!(command.env, [("KEY".into(), "value".into())]);
            assert_eq!(command.stdin, Some(vec![0, 255]));
            assert_eq!(limit, Duration::from_secs(2));
            Ok(ExecOutput {
                stdout: vec![0, 255],
                stderr: vec![254],
                exit_code: 17,
            })
        }
        async fn stat(&self, _: &str) -> Result<Option<FileStatus>, HandleError> {
            panic!("the step reads a file");
        }
        async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
            assert_eq!(address.kind, crate::endpoint::EndpointKind::Udp);
            assert_eq!(address.url, "udp://subject:9000");
            assert_eq!(limits, &AttemptLimits::new(Duration::from_secs(3)));
            Ok(ProbeAnswer::Addressable)
        }
        async fn send(
            &self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits,
        ) -> Result<(), HandleError> {
            assert_eq!(address.kind, crate::endpoint::EndpointKind::Udp);
            assert_eq!(address.url, "udp://subject:9000");
            assert_eq!(payload, b"payload");
            assert_eq!(limits, &AttemptLimits::new(Duration::from_secs(3)));
            Ok(())
        }
    }

    #[tokio::test]
    async fn handles_use_planned_endpoint_routes_and_reject_undeclared_needs() {
        let plan = compile("timeout: 30s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure:\n  - {step: reach, endpoint: subject.metrics}\n  - {step: reach, endpoint: subject.metrics, operation: send}");
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (_capture, capture_rx) = watch::channel(Capture::Following);
        let observation = ParticipantObservation::new(exit_rx, capture_rx, Default::default(), Default::default());
        let cx = StepContext::new(
            plan.participants().unwrap(),
            BTreeMap::from([(
                "subject",
                StepParticipant {
                    primitives: &Backend,
                    observation: &observation,
                    late: BTreeMap::from([(LateFact::ContainerId, "id")]),
                },
            )]),
        );
        let handle = cx.handle("subject").unwrap();
        assert_eq!(handle.name(), "subject");
        assert_eq!(handle.late(LateFact::ContainerId), Some("id"));
        assert_eq!(handle.late(LateFact::Ip), None);
        let limits = AttemptLimits::new(Duration::from_secs(3));
        assert_eq!(handle.probe("metrics", &limits).await, Ok(ProbeAnswer::Addressable));
        assert_eq!(handle.send("metrics", b"payload", &limits).await, Ok(()));
        for error in [
            handle.probe("api", &limits).await.unwrap_err(),
            handle.send("api", b"payload", &limits).await.unwrap_err(),
        ] {
            let HandleError::Harness { message } = error else {
                panic!("a missing route is a harness error");
            };
            assert!(message.contains("subject.api"), "{message}");
            assert!(message.contains("the step must declare an endpoint need"), "{message}");
        }
        let output = handle
            .exec(
                ExecCommand {
                    argv: vec!["tool".into(), "argument".into()],
                    env: vec![("KEY".into(), "value".into())],
                    stdin: Some(vec![0, 255]),
                },
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(
            output,
            ExecOutput {
                stdout: vec![0, 255],
                stderr: vec![254],
                exit_code: 17
            }
        );
    }

    #[tokio::test]
    async fn a_handle_runs_a_planned_operation_with_its_arguments_and_limit() {
        use crate::case::{
            fixtures::{CallFactory, FixtureTypes, OperationServer},
            CaseLoader, Registries,
        };
        let registries = Registries {
            participant_types: &[&OperationServer],
            steps: &[&CallFactory],
            typed_case_roles: &[],
        };
        let case = CaseLoader::new(registries, FixtureTypes, "fixture").load_content(
            "timeout: 30s\nsubject_os: [linux]\nparticipants:\n  subject: {type: server, subject: true}\nprocedure: [{step: call, on: subject, operation: config_dump}]", "case", std::path::Path::new("case/config.yaml"),
        ).unwrap().remove(0);
        let plan = Planner::new(registries, Default::default())
            .unwrap()
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (_capture, capture_rx) = watch::channel(Capture::Following);
        let observation = ParticipantObservation::new(exit_rx, capture_rx, Default::default(), Default::default());
        let cx = StepContext::new(
            plan.participants().unwrap(),
            BTreeMap::from([(
                "subject",
                StepParticipant {
                    primitives: &Backend,
                    observation: &observation,
                    late: BTreeMap::new(),
                },
            )]),
        );
        let handle = cx.handle("subject").unwrap();
        assert_eq!(
            handle
                .op(
                    "config_dump",
                    serde_json::json!({"view": "runtime"}),
                    Duration::from_secs(2)
                )
                .await,
            Ok(serde_json::json!({"enabled": true}))
        );
        assert_eq!(
            handle.op("missing", Value::Null, Duration::from_secs(2)).await,
            Err(HandleError::Harness {
                message: "participant 'subject' has no operation 'missing'".into()
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn handles_report_unknown_processes_and_bound_exit_refreshes() {
        use crate::observe::{exit_record::RecordRefresh, ProcessObservation};
        let plan = compile(FACT_CASE);
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (_capture, capture_rx) = watch::channel(Capture::Following);
        let mut observation =
            ParticipantObservation::new(exit_rx.clone(), capture_rx, Default::default(), Default::default());
        let (_status, status) = watch::channel(ExitObservation::Running);
        let (requests, _requests_rx) = watch::channel(0);
        let (_served, served) = watch::channel(0);
        observation.processes.insert(
            "worker",
            ProcessObservation::Record {
                status,
                refresh: Some(std::sync::Arc::new(RecordRefresh { requests, served })),
            },
        );
        observation
            .processes
            .insert("server", ProcessObservation::Container(exit_rx));
        let cx = StepContext::new(
            plan.participants().unwrap(),
            BTreeMap::from([(
                "subject",
                StepParticipant {
                    primitives: &Backend,
                    observation: &observation,
                    late: BTreeMap::new(),
                },
            )]),
        );
        let handle = cx.handle("subject").unwrap();
        assert_eq!(handle.processes().collect::<Vec<_>>(), ["server", "worker"]);
        let expected = ExitObservation::Undetermined("participant 'subject' has no process 'missing'".into());
        assert_eq!(handle.exit("missing"), expected);
        assert_eq!(handle.wait_for_exit("missing").await, expected);
        assert_eq!(handle.refresh_exit("missing").await, expected);
        assert_eq!(handle.refresh_exit("server").await, ExitObservation::Running);
        let started = tokio::time::Instant::now();
        assert_eq!(
            handle.refresh_exit("worker").await,
            ExitObservation::Undetermined("exit record of process 'worker' was not re-read within 5s".into())
        );
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn steps_resolve_planned_and_started_facts_at_run_time_and_error_when_a_late_fact_is_missing() {
        let yaml = FACT_CASE.replace(
            "{{ subject.facts.file }}",
            "{{ subject.facts.file }}/{{ subject.late.container_id }}/{{ subject.late.ip }}",
        );
        let plan = compile(&yaml);
        let plan = plan.participants().unwrap();
        let ProcedureNode::Step(step) = &plan.procedure()[0] else {
            panic!("one probe step");
        };
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (_capture, capture_rx) = watch::channel(Capture::Following);
        let observation = ParticipantObservation::new(exit_rx, capture_rx, Default::default(), Default::default());
        for (ip, status, message) in [
            (Some("192.0.2.7"), AssertionStatus::Passed, "probe passed"),
            (
                None,
                AssertionStatus::Errored,
                "participant 'subject' has no late fact 'ip'",
            ),
        ] {
            let mut late = BTreeMap::from([(LateFact::ContainerId, "created-id")]);
            if let Some(ip) = ip {
                late.insert(LateFact::Ip, ip);
            }
            let cx = StepContext::new(
                plan,
                BTreeMap::from([(
                    "subject",
                    StepParticipant {
                        primitives: &Backend,
                        observation: &observation,
                        late,
                    },
                )]),
            );
            let report = step.run(&cx).await;
            assert_eq!(report.status, status);
            assert_eq!(report.message, message);
            assert!(cx.handle("missing").is_none());
            assert_eq!(cx.late_fact("missing", LateFact::Ip), None);
        }
    }

    #[test]
    fn step_resolution_rejects_values_that_introduce_template_references() {
        let plan = compile(FACT_CASE);
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (_capture, capture_rx) = watch::channel(Capture::Following);
        let observation = ParticipantObservation::new(exit_rx, capture_rx, Default::default(), Default::default());
        let cx = StepContext::new(
            plan.participants().unwrap(),
            BTreeMap::from([(
                "subject",
                StepParticipant {
                    primitives: &Backend,
                    observation: &observation,
                    late: BTreeMap::from([(LateFact::Ip, "{{unresolved}}")]),
                },
            )]),
        );
        assert_eq!(
            cx.resolve(&"{{subject.late.ip}}".parse().unwrap()),
            Err("substitution would form a template delimiter ('{{')".into())
        );
    }
}
