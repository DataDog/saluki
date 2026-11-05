//! Participant type declarations: what each type accepts in a case file and what it provides to steps.
//!
//! Types register as [`ParticipantType`] values. Knowledge of the products under test lives in this crate.

#![deny(warnings)]
#![deny(missing_docs)]

use panoramic_core::participant::ParticipantType;

mod agent;
mod agent_adp;
mod datadog_intake;
mod fixture;
mod millstone;
mod tools;
pub use self::{
    agent::Agent, agent_adp::AgentAdp, datadog_intake::DatadogIntake, fixture::Fixture, millstone::Millstone,
    tools::TOOLS_LINUX_IMAGE,
};

/// Directory for case-supplied files in Linux containers.
pub(crate) const CASE_FILES_DIR: &str = "/var/lib/panoramic/files";
/// Directory for case-supplied files in Windows containers.
pub(crate) const WINDOWS_CASE_FILES_DIR: &str = r"C:\panoramic\files";

/// Participant types available to case files.
pub const REGISTERED_TYPES: &[&dyn ParticipantType] = &[&Agent, &AgentAdp, &DatadogIntake, &Fixture, &Millstone];

#[cfg(test)]
mod tests {
    use panoramic_core::{
        artifact::ArtifactManifest,
        case::Registries,
        layout::{Os, Realization, RealizationKind},
        participant::SHARED_DIR_FACT,
        plan::Planner,
    };

    use super::*;

    #[test]
    fn registered_types_have_their_case_names_in_sorted_order() {
        assert_eq!(
            REGISTERED_TYPES
                .iter()
                .map(|participant_type| participant_type.name())
                .collect::<Vec<_>>(),
            [
                Agent::NAME,
                AgentAdp::NAME,
                DatadogIntake::NAME,
                Fixture::NAME,
                Millstone::NAME
            ]
        );
        assert_eq!(Agent::NAME, "agent");
        assert_eq!(AgentAdp::NAME, "agent-adp");
        assert_eq!(DatadogIntake::NAME, "datadog-intake");
        assert_eq!(Fixture::NAME, "fixture");
        assert_eq!(Millstone::NAME, "millstone");
    }

    #[test]
    fn registered_types_backends_and_tools_declare_a_consistent_role_catalog() {
        Planner::new(
            Registries {
                participant_types: REGISTERED_TYPES,
                typed_case_roles: &[TOOLS_LINUX_IMAGE],
                ..Default::default()
            },
            ArtifactManifest::default(),
        )
        .unwrap();
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
