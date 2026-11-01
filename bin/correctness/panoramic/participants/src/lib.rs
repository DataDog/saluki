//! Participant type declarations: what each type accepts in a case file and what it provides to steps.
//!
//! Types register as [`ParticipantType`] values. Knowledge of the products under test lives in this crate.

#![deny(warnings)]
#![deny(missing_docs)]

use panoramic_core::participant::ParticipantType;

mod agent_adp;
pub use self::agent_adp::AgentAdp;

/// Participant types available to case files.
pub const REGISTERED_TYPES: &[&dyn ParticipantType] = &[&AgentAdp];

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use panoramic_core::{
        layout::{Os, Realization, RealizationKind},
        participant::SHARED_DIR_FACT,
    };

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
    fn registered_roles_with_one_name_agree_on_kind_and_local_default() {
        let mut roles = BTreeMap::new();
        for participant_type in REGISTERED_TYPES {
            for &(os, kind) in participant_type.supports() {
                for role in participant_type.artifacts(Realization { os, kind }) {
                    let declarer = participant_type.name();
                    if let Some((previous, previous_type, previous_os, previous_kind)) =
                        roles.insert(role.name, (role, declarer, os, kind))
                    {
                        assert_eq!(
                            role, previous,
                            "{declarer} on {os:?} as {kind:?} disagrees with \
                             {previous_type} on {previous_os:?} as {previous_kind:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn registered_facts_have_unique_sorted_names_and_literal_values_for_every_supported_realization() {
        for participant_type in REGISTERED_TYPES {
            for &(os, kind) in participant_type.supports() {
                let facts = participant_type.facts(Realization { os, kind });
                let names: Vec<_> = facts.iter().map(|fact| fact.name).collect();
                assert!(
                    names.windows(2).all(|pair| pair[0] < pair[1]),
                    "{} on {os:?} as {kind:?}: {names:?}",
                    participant_type.name()
                );
                for fact in facts {
                    assert_ne!(
                        fact.name,
                        SHARED_DIR_FACT,
                        "{} on {os:?} as {kind:?}",
                        participant_type.name()
                    );
                    assert!(
                        !fact.value.contains("{{"),
                        "{} on {os:?} as {kind:?}: {}: {}",
                        participant_type.name(),
                        fact.name,
                        fact.value
                    );
                }
            }
        }
    }

    #[test]
    fn registered_types_provide_no_facts_for_realizations_they_do_not_support() {
        for participant_type in REGISTERED_TYPES {
            // Keep both variant lists in sync with Os and RealizationKind.
            for os in [Os::Linux, Os::MacOs, Os::Windows] {
                for kind in [
                    RealizationKind::LinuxContainer,
                    RealizationKind::HostProcessGroup,
                    RealizationKind::WindowsContainer,
                    RealizationKind::KubernetesPod,
                ] {
                    if !participant_type.supports().contains(&(os, kind)) {
                        assert!(
                            participant_type.facts(Realization { os, kind }).is_empty(),
                            "{} on {os:?} as {kind:?}",
                            participant_type.name()
                        );
                    }
                }
            }
        }
    }
}
