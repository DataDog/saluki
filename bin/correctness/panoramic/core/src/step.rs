//! Step declarations and field decoding.

use std::{collections::BTreeMap, fmt, time::Duration};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    backend::LateFact,
    case::{template::Reference, StepTemplate},
    endpoint::{EndpointOperation, RunnerHostRoute},
    handle::Primitives,
    observe::ParticipantObservation,
    plan::ParticipantPlan,
    record::StepReport,
};

/// A step's fields as written in the case file, keyed by field name.
pub type StepFields = BTreeMap<String, Value>;

/// A dependency of a built step.
///
/// Loading checks participant and process names. Planning checks facts, late facts and endpoint reachability
/// on each subject OS.
#[derive(Debug, PartialEq)]
pub enum Need {
    /// The participant the step runs on.
    Handle(String),
    /// One named process of a participant: (participant, process).
    Process(String, String),
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

    /// Participants, processes, facts, late facts and endpoints the step uses.
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

    #[cfg_attr(
        not(any(test, feature = "test-util")),
        expect(dead_code, reason = "Participant I/O for native steps.")
    )]
    pub(crate) fn primitives(&self, name: &str) -> Option<&dyn Primitives> {
        self.participants.get(name).map(|participant| participant.primitives)
    }

    #[cfg_attr(
        not(any(test, feature = "test-util")),
        expect(dead_code, reason = "Exit and output observations for native steps.")
    )]
    pub(crate) fn observation(&self, name: &str) -> Option<&ParticipantObservation> {
        self.participants.get(name).map(|participant| participant.observation)
    }

    pub(crate) fn late_fact(&self, name: &str, fact: LateFact) -> Option<&str> {
        self.participants.get(name)?.late.get(&fact).copied()
    }

    #[cfg_attr(
        not(any(test, feature = "test-util")),
        expect(dead_code, reason = "Runner-host routes for native steps.")
    )]
    pub(crate) fn runner_host_route(
        &self, server: &str, endpoint: &str, operation: EndpointOperation,
    ) -> Option<&RunnerHostRoute> {
        self.plan.runner_host_route(server, endpoint, operation)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Execution-time template resolution for native steps.")
    )]
    pub(crate) fn resolve(&self, template: &StepTemplate) -> Result<String, String> {
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

/// Builds a named step from its case-file fields at load time.
///
/// The step crate registers factories for log and file assertions. The loader builds steps and checks
/// participant and process names; planning checks facts, late facts and endpoints through [`Step::needs`].
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
    use airlock::{exec::ExecOutput, files::FileStat};
    use tokio::sync::watch;

    use super::*;
    use crate::{
        case::fixtures::{self, FixtureConfig},
        case::ProcedureNode,
        endpoint::ResolvedAddress,
        handle::{ExecCommand, HandleError},
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

    struct Files;

    #[async_trait]
    impl Primitives for Files {
        async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError> {
            Ok((path == "/logs/output.log/created-id/192.0.2.7").then(Vec::new))
        }
        async fn exec(&self, _: ExecCommand, _: Duration) -> Result<ExecOutput, HandleError> {
            panic!("the step reads a file");
        }
        async fn stat(&self, _: &str) -> Result<Option<FileStat>, HandleError> {
            panic!("the step reads a file");
        }
        async fn probe(&self, _: &ResolvedAddress, _: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
            panic!("the step reads a file");
        }
        async fn send(&self, _: &ResolvedAddress, _: &[u8], _: &AttemptLimits) -> Result<(), HandleError> {
            panic!("the step reads a file");
        }
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
                        primitives: &Files,
                        observation: &observation,
                        late,
                    },
                )]),
            );
            let report = step.run(&cx).await;
            assert_eq!(report.status, status);
            assert_eq!(report.message, message);
            assert!(cx.primitives("missing").is_none());
            assert!(cx.observation("missing").is_none());
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
                    primitives: &Files,
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
