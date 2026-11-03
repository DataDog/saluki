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
        },
        REGISTRIES,
    };

    struct Plain;

    impl ParticipantType for Plain {
        fn name(&self) -> &'static str {
            "plain"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[(Os::Linux, RealizationKind::LinuxContainer)]
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
    fn a_participant_case_routes_to_the_bridge_when_every_participant_type_has_a_bridge_route_on_the_subject_os() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for extra in ["", "  other: {type: agent-adp}\n"] {
                let yaml = PARTICIPANT_FIELDS
                    .replace("[linux]", "[linux, macos, windows]")
                    .replace("participants:\n", &format!("participants:\n{extra}"));
                assert_eq!(route(&plan(&yaml, os)), Route::Bridge);
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
    fn a_native_route_fails_runner_construction_naming_the_case_and_its_types() {
        let yaml = PARTICIPANT_FIELDS.replace("  adp:", "  other: {type: plain}\n  again: {type: plain}\n  adp:");
        let plans = ["first", "second"].map(|name| plan_with_plain(&yaml, name, Os::Linux));
        let error = build_runners(&plans, Os::Linux, &RunnerArtifacts::Overrides(&[]))
            .err()
            .expect("native cases must fail runner construction");
        assert_eq!(error.to_string(), concat!(
            "Failed to build test runners:\n",
            "  - case 'first' in /cases/first/config.yaml: on subject OS 'linux', participant type(s) 'plain' have no integration bridge route, and panoramic cannot run them natively\n",
            "  - case 'second' in /cases/second/config.yaml: on subject OS 'linux', participant type(s) 'plain' have no integration bridge route, and panoramic cannot run them natively",
        ));
    }
}
