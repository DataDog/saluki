//! Participant type declarations: what each type accepts in a case file and what it provides to steps.
//!
//! Types register as [`ParticipantType`] values. Knowledge of the products under test lives in this crate.

#![deny(warnings)]
#![deny(missing_docs)]

use panoramic_core::case::ParticipantType;

mod agent_adp;
pub use self::agent_adp::AgentAdp;

/// Participant types available to case files.
pub const REGISTERED_TYPES: &[&dyn ParticipantType] = &[&AgentAdp];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_types_are_agent_adp_under_its_name() {
        assert_eq!(
            REGISTERED_TYPES
                .iter()
                .map(|participant_type| participant_type.name())
                .collect::<Vec<_>>(),
            [AgentAdp::NAME]
        );
        assert_eq!(AgentAdp::NAME, "agent-adp");
    }

    #[test]
    fn registered_facts_have_unique_sorted_names_and_literal_values() {
        for participant_type in REGISTERED_TYPES {
            let names: Vec<_> = participant_type.facts().iter().map(|fact| fact.name).collect();
            assert!(
                names.windows(2).all(|pair| pair[0] < pair[1]),
                "{}: {names:?}",
                participant_type.name()
            );
            for fact in participant_type.facts() {
                for (_, value) in fact.values {
                    assert!(
                        !value.contains("{{"),
                        "{}: {}: {value}",
                        participant_type.name(),
                        fact.name
                    );
                }
            }
        }
    }
}
