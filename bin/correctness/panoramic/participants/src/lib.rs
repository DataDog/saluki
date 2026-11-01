//! Participant type declarations: what each type accepts in a case file and what it provides to steps.
//!
//! Knowledge of the products under test lives in this crate.

#![deny(warnings)]
#![deny(missing_docs)]

use panoramic_core::case::ParticipantDeclaration;

mod agent_adp;
pub use self::agent_adp::AGENT_ADP;

/// Participant types available to case files.
pub const REGISTERED_TYPES: &[&ParticipantDeclaration] = &[&AGENT_ADP];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_facts_have_unique_sorted_names_and_literal_values() {
        for declaration in REGISTERED_TYPES {
            let names: Vec<_> = declaration.facts.iter().map(|fact| fact.name).collect();
            assert!(
                names.windows(2).all(|pair| pair[0] < pair[1]),
                "{}: {names:?}",
                declaration.name
            );
            for fact in declaration.facts {
                for (_, value) in fact.values {
                    assert!(!value.contains("{{"), "{}: {}: {value}", declaration.name, fact.name);
                }
            }
        }
    }
}
