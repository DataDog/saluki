//! The participant types and steps a case may name, filled in by the binary.

use std::collections::BTreeMap;

use super::participant::ParticipantType;
use super::step::StepFactory;

/// Participant types and steps by the name a case writes.
#[derive(Default)]
pub(crate) struct Registry {
    participant_types: BTreeMap<&'static str, Box<dyn ParticipantType>>,
    steps: BTreeMap<&'static str, Box<dyn StepFactory>>,
}

// TODO: remove the dead_code allowance once a participant type and a step are registered.
#[allow(dead_code)]
impl Registry {
    /// Registers a participant type.
    ///
    /// # Panics
    ///
    /// Panics if a participant type with the same name is already registered.
    pub(crate) fn add_participant_type(&mut self, participant_type: Box<dyn ParticipantType>) {
        let name = participant_type.name();
        let previous = self.participant_types.insert(name, participant_type);
        assert!(previous.is_none(), "participant type '{}' is registered twice", name);
    }

    /// Registers a step.
    ///
    /// # Panics
    ///
    /// Panics if a step with the same name is already registered.
    pub(crate) fn add_step(&mut self, step: Box<dyn StepFactory>) {
        let name = step.name();
        let previous = self.steps.insert(name, step);
        assert!(previous.is_none(), "step '{}' is registered twice", name);
    }

    pub(crate) fn participant_type(&self, name: &str) -> Option<&dyn ParticipantType> {
        self.participant_types.get(name).map(Box::as_ref)
    }

    pub(crate) fn step(&self, name: &str) -> Option<&dyn StepFactory> {
        self.steps.get(name).map(Box::as_ref)
    }
}
