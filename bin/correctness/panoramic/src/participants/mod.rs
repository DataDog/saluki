//! Participant types: what each piece of software runs and how, as data the core plans.

use crate::core::registry::Registry;

mod agent_adp;
mod datadog_agent;
mod datadog_intake;

/// Registers every participant type.
pub(crate) fn register(registry: &mut Registry) {
    registry.add_participant_type(Box::new(agent_adp::AgentAdp));
    registry.add_participant_type(Box::new(datadog_intake::DatadogIntake));
}
