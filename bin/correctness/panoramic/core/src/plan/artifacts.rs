//! Collects layout artifact roles and checks agreement across a run.

use std::collections::{btree_map::Entry, BTreeMap};

use crate::{
    artifact::ArtifactRole,
    backend,
    case::CaseFile,
    layout::{describe_participant, Layout},
    participant::{find_type, ParticipantType},
};

/// Artifact roles declared in one run, keyed by name, with the first declaration of each.
pub(super) struct DeclaredRoles {
    roles: BTreeMap<&'static str, (ArtifactRole, String)>,
}

impl DeclaredRoles {
    pub(super) fn new() -> Self {
        Self { roles: BTreeMap::new() }
    }

    /// Collects roles in participant name order: each participant's type roles, then backend roles.
    /// Returns each name once, sorted by name.
    ///
    /// # Errors
    ///
    /// Returns an error when a role's kind or local default differs from an earlier declaration in this run,
    /// naming both declarations.
    pub(super) fn collect(
        &mut self, case: &str, file: &CaseFile, layout: &Layout, types: &[&dyn ParticipantType],
    ) -> Result<Vec<ArtifactRole>, String> {
        let mut roles = BTreeMap::new();
        for (name, realization) in layout.iter() {
            let entry = &file.participants[name];
            let participant_type =
                find_type(types, &entry.participant_type).expect("laid-out participants have registered types");
            for (declarations, declarer) in [
                (
                    participant_type.artifacts(realization),
                    describe_participant(name, &entry.participant_type, realization),
                ),
                (
                    backend::artifacts(realization.kind),
                    format!("the {} backend", realization.kind.name()),
                ),
            ] {
                for &role in declarations {
                    match self.roles.entry(role.name) {
                        Entry::Occupied(earlier) => {
                            let (previous, previous_declarer) = earlier.get();
                            if role != *previous {
                                return Err(format!(
                                    "artifact role '{}' is declared as {} '{}' by {declarer}, \
                                     but as {} '{}' by {previous_declarer}",
                                    role.name,
                                    role.kind.name(),
                                    role.local_default,
                                    previous.kind.name(),
                                    previous.local_default,
                                ));
                            }
                        }
                        Entry::Vacant(vacant) => {
                            vacant.insert((role, format!("{declarer} in case '{case}'")));
                        }
                    }
                    roles.insert(role.name, role);
                }
            }
        }
        Ok(roles.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        artifact::{ArtifactKind, ArtifactRole},
        case::{
            fixtures::{minimal_procedure, PARTICIPANT_FIELDS},
            Registries,
        },
        layout::{Os, Realization, RealizationKind},
        participant::ParticipantType,
        plan::{
            fixtures::{compile_with, planner, CONFIG_LOADER, CONFIG_REGISTRIES},
            Planner,
        },
    };

    const TOOL: ArtifactRole = ArtifactRole {
        name: "tool",
        kind: ArtifactKind::Binary,
        local_default: "bin/tool",
    };
    const RESOURCES: ArtifactRole = ArtifactRole {
        name: "resources",
        kind: ArtifactKind::Install,
        local_default: "share/tool",
    };
    const WINDOWS_TOOL: ArtifactRole = ArtifactRole {
        name: "windows-tool",
        kind: ArtifactKind::Binary,
        local_default: "bin/tool.exe",
    };
    const FIXUP: ArtifactRole = ArtifactRole {
        name: "fixup-image",
        kind: ArtifactKind::Image,
        local_default: airlock::driver::DEFAULT_ALPINE_IMAGE,
    };

    struct Type {
        name: &'static str,
        roles: &'static [ArtifactRole],
        windows_roles: &'static [ArtifactRole],
    }

    impl ParticipantType for Type {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::MacOs, RealizationKind::HostProcessGroup),
                (Os::Windows, RealizationKind::WindowsContainer),
            ]
        }

        fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
            if realization.os == Os::Windows {
                self.windows_roles
            } else {
                self.roles
            }
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    const SERVER: Type = Type {
        name: "server",
        roles: &[TOOL, RESOURCES],
        windows_roles: &[WINDOWS_TOOL],
    };

    const TWO_TYPES: &str = concat!(
        "timeout: 1s\nsubject_os: [macos]\nparticipants:\n",
        "  z: {type: configured, subject: true}\n  a: {type: server}\n",
        minimal_procedure!(),
        "\n"
    );

    fn role_planner(types: &'static [&dyn ParticipantType]) -> Planner<'static> {
        Planner::with_run(
            Registries {
                participant_types: types,
                ..CONFIG_REGISTRIES
            },
            "testrun0",
        )
    }

    fn planning_error(types: &'static [&dyn ParticipantType], yaml: &str, os: Os) -> String {
        compile_with(&mut role_planner(types), yaml, os)
            .err()
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_plan_lists_each_role_of_its_participant_types_and_backends_once_sorted_by_name() {
        let yaml = TWO_TYPES
            .replace("type: configured", "type: server")
            .replace("a: {type: server}", "a: {type: server, os: linux}");
        let plan = compile_with(&mut role_planner(&[&SERVER]), &yaml, Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(plan.artifact_roles(), Some([FIXUP, RESOURCES, TOOL].as_slice()));
    }

    #[test]
    fn a_typed_case_plan_has_no_artifact_roles() {
        let plan = compile_with(&mut planner(), "type: fixture", Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(plan.artifact_roles(), None);
    }

    #[test]
    fn a_participant_case_with_no_declared_roles_has_an_empty_role_list() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]");
        let plan = compile_with(&mut planner(), &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.artifact_roles(), Some([].as_slice()));
    }

    #[test]
    fn a_role_declared_with_another_kind_fails_planning_naming_both_declarations() {
        let message = planning_error(
            &[
                &SERVER,
                &Type {
                    name: "configured",
                    roles: &[ArtifactRole {
                        kind: ArtifactKind::Image,
                        ..TOOL
                    }],
                    windows_roles: &[],
                },
            ],
            TWO_TYPES,
            Os::MacOs,
        );
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', \
             artifact role 'tool' is declared as image 'bin/tool' by \
             participant 'z' of type 'configured' as host-process-group on macos, but as binary 'bin/tool' by \
             participant 'a' of type 'server' as host-process-group on macos in case 'alpha'"
        );
    }

    #[test]
    fn a_role_declared_with_another_local_default_fails_planning_naming_both_declarations() {
        let message = planning_error(
            &[
                &SERVER,
                &Type {
                    name: "configured",
                    roles: &[ArtifactRole {
                        local_default: "share/other",
                        ..RESOURCES
                    }],
                    windows_roles: &[],
                },
            ],
            TWO_TYPES,
            Os::MacOs,
        );
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'macos', \
             artifact role 'resources' is declared as install 'share/other' by \
             participant 'z' of type 'configured' as host-process-group on macos, but as install 'share/tool' by \
             participant 'a' of type 'server' as host-process-group on macos in case 'alpha'"
        );
    }

    #[test]
    fn a_role_conflicting_with_one_declared_by_an_earlier_case_fails_planning_naming_that_case() {
        let mut planner = role_planner(&[
            &SERVER,
            &Type {
                name: "configured",
                roles: &[ArtifactRole {
                    local_default: "bin/other",
                    ..TOOL
                }],
                windows_roles: &[],
            },
        ]);
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]");
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.artifact_roles(), Some([RESOURCES, TOOL].as_slice()));
        let case = CONFIG_LOADER
            .load_content(
                &yaml.replace("type: server", "type: configured"),
                "beta",
                Path::new("cases/beta/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let message = planner.compile(case, Os::MacOs).err().unwrap().to_string();
        assert_eq!(
            message,
            "case 'beta' in cases/beta/config.yaml: on subject OS 'macos', \
             artifact role 'tool' is declared as binary 'bin/other' by \
             participant 'subject' of type 'configured' as host-process-group on macos, but as binary 'bin/tool' by \
             participant 'subject' of type 'server' as host-process-group on macos in case 'alpha'"
        );
    }

    #[test]
    fn a_type_role_named_like_a_backend_role_must_match_it() {
        let message = planning_error(
            &[&Type {
                name: "server",
                roles: &[ArtifactRole {
                    local_default: "other:latest",
                    ..FIXUP
                }],
                windows_roles: &[],
            }],
            PARTICIPANT_FIELDS,
            Os::Linux,
        );
        assert_eq!(
            message,
            format!(
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', \
                 artifact role 'fixup-image' is declared as image '{}' by the linux-container backend, \
                 but as image 'other:latest' by participant 'subject' of type 'server' as linux-container on linux \
                 in case 'alpha'",
                airlock::driver::DEFAULT_ALPINE_IMAGE,
            )
        );
        let mut planner = role_planner(&[&Type {
            name: "server",
            roles: &[FIXUP],
            windows_roles: &[],
        }]);
        let plan = compile_with(&mut planner, PARTICIPANT_FIELDS, Os::Linux)
            .unwrap()
            .unwrap();
        assert_eq!(plan.artifact_roles(), Some([FIXUP].as_slice()));
    }

    #[test]
    fn roles_are_checked_on_every_declared_subject_os_even_when_the_selected_os_is_excluded() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let message = planning_error(
                &[&Type {
                    name: "server",
                    roles: &[TOOL],
                    windows_roles: &[ArtifactRole {
                        local_default: "bin/tool.exe",
                        ..TOOL
                    }],
                }],
                &yaml,
                os,
            );
            assert_eq!(
                message,
                "case 'alpha' in cases/alpha/config.yaml: on subject OS 'windows', \
                 artifact role 'tool' is declared as binary 'bin/tool.exe' by \
                 participant 'subject' of type 'server' as windows-container on windows, but as binary 'bin/tool' by \
                 participant 'subject' of type 'server' as linux-container on linux in case 'alpha'"
            );
        }
    }

    #[test]
    fn a_plan_lists_only_the_selected_subject_os_roles() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        let mut planner = role_planner(&[&SERVER]);
        let linux = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(linux.artifact_roles(), Some([FIXUP, RESOURCES, TOOL].as_slice()));
        let windows = compile_with(&mut planner, &yaml, Os::Windows).unwrap().unwrap();
        assert_eq!(windows.artifact_roles(), Some([WINDOWS_TOOL].as_slice()));
    }
}
