//! Step execution fixtures and readers for native lifecycle tests.

use std::path::Path;

use saluki_error::GenericError;
use serde_json::{json, Value};

use super::*;
use crate::{
    artifact::ArtifactRole,
    case::{CaseLoader, Registries, TypedCase, TypedCaseLoader},
    layout::{Os, Realization, RealizationKind},
    participant::ParticipantType,
    plan::Planner,
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
        &[]
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
    step.run(&StepContext::new(plan, BTreeMap::new())).await
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
    let observation = cx.observation(on).unwrap().process(process).unwrap().settled().await;
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
    use crate::{observe::Capture, probe::POLL_INTERVAL, record::AssertionStatus};
    let started = tokio::time::Instant::now();
    let observation = cx.observation(on).unwrap();
    let mut cursor = 0;
    loop {
        let captures = std::iter::once(observation.capture())
            .chain(observation.outputs.values().map(|output| output.current()))
            .collect::<Vec<_>>();
        let (lines, next) = match observation.lines_since(cursor) {
            Ok(lines) => lines,
            Err(message) => return StepReport::new(started, AssertionStatus::Errored, message),
        };
        cursor = next;
        if lines.iter().any(|line| line.text == text) {
            return StepReport::new(started, AssertionStatus::Passed, format!("{text} observed"));
        }
        if let Some(Capture::Failed { reason }) = captures
            .iter()
            .find(|capture| matches!(capture, Capture::Failed { .. }))
        {
            return StepReport::new(started, AssertionStatus::Errored, reason.clone());
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
    let primitives = cx
        .primitives(server)
        .ok_or_else(|| format!("participant '{server}' has not started"))?;
    crate::handle::send_from_runner_host(
        route,
        primitives,
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
    let primitives = cx
        .primitives(on)
        .ok_or_else(|| format!("participant '{on}' has not started"))?;
    primitives
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
    Some(cx.observation(on)?.process(process)?.snapshot())
}

/// Reads the named process's retained output lines.
///
/// # Errors
///
/// Returns an error for missing observations, a failed capture or an overflowed line buffer.
pub fn process_output(cx: &StepContext<'_>, on: &str, process: &str) -> Result<Vec<String>, String> {
    let observation = cx
        .observation(on)
        .ok_or_else(|| format!("participant '{on}' has not started"))?;
    let output = observation
        .outputs
        .get(process)
        .ok_or_else(|| format!("process '{on}.{process}' has no output"))?;
    if let crate::observe::Capture::Failed { reason } = output.current() {
        return Err(reason);
    }
    let (lines, _) = observation.lines_since(0)?;
    Ok(lines
        .into_iter()
        .filter(|line| line.process == Some(process))
        .map(|line| line.text)
        .collect())
}
