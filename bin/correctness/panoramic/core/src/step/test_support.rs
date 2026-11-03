//! Executes steps against a planned participant with no live resources.

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
