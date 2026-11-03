//! Adapts loaded steps and started participants to the procedure driver.

use std::{collections::BTreeMap, convert::Infallible, time::Duration};

use async_trait::async_trait;

use super::{CaseControl, Started};
use crate::{
    backend::LateFact,
    case::ProcedureNode,
    plan::ParticipantPlan,
    procedure::{Member, Procedure},
    record::StepReport,
    step::{Step, StepContext, StepParticipant},
};

pub(super) async fn run(plan: ParticipantPlan<'_>, running: &BTreeMap<String, Started>, control: &CaseControl) {
    let participants = running
        .iter()
        .map(|(name, started)| {
            let mut late = BTreeMap::from([(LateFact::ContainerId, started.container_id.as_str())]);
            if let Some(ip) = &started.ip {
                late.insert(LateFact::Ip, ip.as_str());
            }
            (
                name.as_str(),
                StepParticipant {
                    primitives: started.participant.as_ref(),
                    observation: &started.observation,
                    late,
                },
            )
        })
        .collect();
    let cx = StepContext::new(plan, participants);
    let member = |step| -> Result<Box<dyn Member + '_>, Infallible> { Ok(Box::new(StepMember { step, cx: &cx })) };
    let nodes = plan.procedure().iter().map(|node| match node {
        ProcedureNode::Step(step) => ProcedureNode::Step(member(step.as_ref())),
        ProcedureNode::Parallel(steps) => {
            ProcedureNode::Parallel(steps.iter().map(|step| member(step.as_ref())).collect())
        }
    });
    let procedure = Procedure::build(nodes).expect("loaded steps adapt infallibly to procedure members");
    procedure.run(control.deadline, &control.cancel, &control.record).await;
}

struct StepMember<'a> {
    step: &'a dyn Step,
    cx: &'a StepContext<'a>,
}

#[async_trait]
impl Member for StepMember<'_> {
    fn name(&self) -> &'static str {
        self.step.name()
    }
    fn description(&self) -> String {
        format!("{:?}", self.step)
    }
    fn budget(&self) -> Duration {
        self.step.budget()
    }
    async fn run(&self) -> StepReport {
        self.step.run(self.cx).await
    }
}
