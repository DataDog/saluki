//! Chooses how each planned case runs.

use panoramic_core::case::CaseSchema;

use super::{integration_bridge::BRIDGED_TYPES, typed_case::LoadedPlan};

/// The execution path selected for a planned case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Route {
    /// The runner for the case's top-level type.
    Typed,
    /// The integration runner translating participants.
    Bridge,
    /// The panoramic-core lifecycle.
    Native,
}

/// Fixes each case's execution route before running.
pub(crate) fn route(plan: &LoadedPlan) -> Route {
    match plan.case().schema() {
        CaseSchema::Typed(_) => Route::Typed,
        CaseSchema::Participants { file, .. } => {
            if file
                .participants
                .values()
                .all(|entry| BRIDGED_TYPES.contains(&(entry.participant_type.as_str(), plan.subject_os())))
            {
                Route::Bridge
            } else {
                Route::Native
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use panoramic_core::{
        artifact::{ArtifactManifest, ArtifactRole},
        case::{CaseLoader, Registries},
        layout::{Os, Realization, RealizationKind},
        participant::ParticipantType,
        plan::Planner,
    };
    use panoramic_participants::AgentAdp;

    use super::*;
    use crate::{
        config::{
            build_runners,
            runner_artifacts::RunnerArtifacts,
            tests::{plan, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
            typed_case::TypedConfigLoader,
            Execution,
        },
        REGISTRIES,
    };

    pub(crate) struct Plain;

    impl ParticipantType for Plain {
        fn name(&self) -> &'static str {
            "plain"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::MacOs, RealizationKind::HostProcessGroup),
            ]
        }

        fn artifacts(&self, _realization: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    pub(crate) fn plan_with_plain(yaml: &str, name: &str, os: Os) -> LoadedPlan {
        let registries = Registries {
            participant_types: &[&AgentAdp, &Plain],
            ..REGISTRIES
        };
        let source = format!("/cases/{name}/config.yaml");
        let case = CaseLoader::new(registries, TypedConfigLoader, "integration")
            .load_content(yaml, name, Path::new(&source))
            .unwrap()
            .remove(0);
        Planner::new(registries, ArtifactManifest::default())
            .unwrap()
            .compile(case, os)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[test]
    fn typed_cases_route_to_their_typed_runner() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let plan = plan(
                "type: integration\nname: case\ntimeout: 1s\nruntimes: [linux, mac, windows]\nprocedure: []\n",
                os,
            );
            assert_eq!(route(&plan), Route::Typed);
        }
        for runtime in ["docker", "kubernetes_in_docker"] {
            let plan = plan(
                &format!("type: correctness\n{}", CORRECTNESS_FIELDS.replace("docker", runtime)),
                Os::Linux,
            );
            assert_eq!(route(&plan), Route::Typed);
        }
    }

    #[test]
    fn linux_participant_cases_run_natively_while_macos_and_windows_use_the_bridge() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for extra in ["", "  other: {type: agent-adp}\n"] {
                let yaml = PARTICIPANT_FIELDS
                    .replace("[linux]", "[linux, macos, windows]")
                    .replace("participants:\n", &format!("participants:\n{extra}"));
                assert_eq!(
                    route(&plan(&yaml, os)),
                    if os == Os::Linux { Route::Native } else { Route::Bridge }
                );
            }
        }
    }

    #[test]
    fn a_participant_case_with_a_type_lacking_a_bridge_route_routes_natively() {
        let single = plan_with_plain(&PARTICIPANT_FIELDS.replace("agent-adp", "plain"), "case", Os::Linux);
        assert_eq!(route(&single), Route::Native);
        let mixed = PARTICIPANT_FIELDS
            .replace("  adp:", "  other: {type: plain, os: linux}\n  adp:")
            .replace("[linux]", "[linux, macos, windows]");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(route(&plan_with_plain(&mixed, "case", os)), Route::Native);
        }
    }

    #[test]
    fn participant_cases_without_a_bridge_route_build_native_executions() {
        let single = PARTICIPANT_FIELDS.replace("agent-adp", "plain");
        let mixed = PARTICIPANT_FIELDS.replace("  adp:", "  other: {type: plain}\n  adp:");
        let plans = [
            plan_with_plain(&single, "single", Os::Linux),
            plan_with_plain(&mixed, "mixed", Os::Linux),
            plan(PARTICIPANT_FIELDS, Os::Linux),
        ];
        let built = build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&[])).unwrap();
        assert_eq!(built.len(), 3);
        for (built, name) in built.iter().zip(["single", "mixed", "case"]) {
            assert_eq!(built.plan.case().name(), name);
            assert!(matches!(built.execution, Execution::Native));
            assert!(built.runner().is_none());
        }
    }
}
