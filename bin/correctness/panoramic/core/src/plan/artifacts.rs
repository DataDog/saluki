//! Validates the registered role catalog and resolves each layout's artifact bindings.

use std::{
    collections::{btree_map::Entry, BTreeMap},
    path::{Component, Path},
};

use saluki_error::{generic_error, GenericError};

use super::CasePlan;
use crate::{
    artifact::{ArtifactBinding, ArtifactKind, ArtifactManifest, ArtifactRole, BindingSource},
    backend,
    case::{CaseFile, Registries},
    layout::{Layout, Realization},
    participant::{find_type, ParticipantType},
};

/// Checks bindings against the artifact roles declared by all registered participant types and backends.
///
/// # Errors
///
/// Returns an error for invalid install main-binary paths or conflicting role declarations,
/// then for unknown manifest roles, naming the manifest's origin.
pub fn check_artifact_manifest(registries: Registries<'_>, manifest: &ArtifactManifest) -> Result<(), GenericError> {
    let catalog = role_catalog(registries.participant_types).map_err(|error| generic_error!(error))?;
    check_manifest(&catalog, manifest).map_err(|error| generic_error!(error))
}

fn role_catalog(types: &[&dyn ParticipantType]) -> Result<BTreeMap<&'static str, ArtifactRole>, String> {
    let mut roles: BTreeMap<_, (ArtifactRole, String)> = BTreeMap::new();
    let mut declare = |role: ArtifactRole, declarer: String| {
        if let ArtifactKind::Install { main_binary } = role.kind {
            if main_binary.is_empty()
                || !Path::new(main_binary).components().all(|part| matches!(part, Component::Normal(_)))
                // Path components normalize interior dots; check the original spelling too.
                || main_binary.split(std::path::is_separator).any(|part| part == ".")
            {
                return Err(format!(
                    "artifact role '{}' declared by {declarer} has invalid main binary '{main_binary}': \
                     expected a nonempty relative path containing only normal names",
                    role.name
                ));
            }
        }
        match roles.entry(role.name) {
            Entry::Occupied(earlier) => {
                let (previous, previous_declarer) = earlier.get();
                if role != *previous {
                    return Err(format!(
                        "artifact role '{}' is declared as {} by {declarer}, \
                         but as {} by {previous_declarer}",
                        role.name,
                        role.describe(),
                        previous.describe(),
                    ));
                }
            }
            Entry::Vacant(vacant) => {
                vacant.insert((role, declarer));
            }
        }
        Ok(())
    };
    for participant_type in types {
        for &(os, kind) in participant_type.supports() {
            for &role in participant_type.artifacts(Realization { os, kind }) {
                declare(
                    role,
                    format!(
                        "participant type '{}' as {} on {}",
                        participant_type.name(),
                        kind.name(),
                        os.name()
                    ),
                )?;
            }
        }
    }
    for backend in backend::BACKENDS {
        for &role in backend.artifacts() {
            declare(role, format!("the {} backend", backend.kind().name()))?;
        }
    }
    Ok(roles.into_iter().map(|(name, (role, _))| (name, role)).collect())
}

fn check_manifest(catalog: &BTreeMap<&str, ArtifactRole>, manifest: &ArtifactManifest) -> Result<(), String> {
    let unknown: Vec<_> = manifest
        .roles
        .keys()
        .filter(|name| !catalog.contains_key(name.as_str()))
        .map(|name| format!("'{name}'"))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{}: no participant type or backend declares role {}; declared roles: {}",
            manifest.origin,
            unknown.join(", "),
            catalog.keys().copied().collect::<Vec<_>>().join(", "),
        ));
    }
    Ok(())
}

pub(super) fn bind(
    file: &CaseFile, layout: &Layout, types: &[&dyn ParticipantType], bindings: &BTreeMap<String, String>,
) -> Vec<ArtifactBinding> {
    let mut roles = BTreeMap::new();
    for (name, realization) in layout.iter() {
        let entry = &file.participants[name];
        let participant_type =
            find_type(types, &entry.participant_type).expect("laid-out participants have registered types");
        for &role in participant_type
            .artifacts(realization)
            .iter()
            .chain(backend::artifacts(realization.kind))
        {
            roles.entry(role.name).or_insert(role);
        }
    }
    roles
        .into_values()
        .map(|role| {
            let (reference, source) = match bindings.get(role.name) {
                Some(reference) => (reference.clone(), BindingSource::Manifest),
                None => (role.local_default.to_string(), BindingSource::LocalDefault),
            };
            ArtifactBinding {
                role,
                reference,
                source,
            }
        })
        .collect()
}

/// Collects one participant-case binding per role, sorted by name.
/// Plans must come from one planner, which binds each role identically.
pub fn needed_artifacts<'p, C: 'p>(plans: impl IntoIterator<Item = &'p CasePlan<C>>) -> Vec<ArtifactBinding> {
    let mut bindings = BTreeMap::new();
    for plan in plans {
        for binding in plan.artifacts().unwrap_or_default() {
            bindings.entry(binding.role.name).or_insert_with(|| binding.clone());
        }
    }
    bindings.into_values().collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use crate::{
        artifact::{ArtifactBinding, ArtifactKind, ArtifactManifest, ArtifactRole, BindingSource},
        case::{
            fixtures::{minimal_procedure, PARTICIPANT_FIELDS},
            Registries,
        },
        layout::{Os, Realization, RealizationKind},
        participant::ParticipantType,
        plan::{
            check_artifact_manifest,
            fixtures::{compile_with, planner, CONFIG_REGISTRIES},
            needed_artifacts, Planner,
        },
    };

    const TOOL: ArtifactRole = ArtifactRole {
        name: "tool",
        kind: ArtifactKind::Binary,
        local_default: "bin/tool",
    };
    const RESOURCES: ArtifactRole = ArtifactRole {
        name: "resources",
        kind: ArtifactKind::Install {
            main_binary: "bin/tool",
        },
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
            assert!(self.supports().contains(&(realization.os, realization.kind)));
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

    fn catalog_error(types: &[&dyn ParticipantType]) -> String {
        Planner::new(
            Registries {
                participant_types: types,
                ..CONFIG_REGISTRIES
            },
            ArtifactManifest::default(),
        )
        .err()
        .unwrap()
        .to_string()
    }

    fn local_bindings(roles: &[ArtifactRole]) -> Vec<ArtifactBinding> {
        roles
            .iter()
            .map(|&role| ArtifactBinding {
                role,
                reference: role.local_default.to_string(),
                source: BindingSource::LocalDefault,
            })
            .collect()
    }

    #[test]
    fn a_plan_lists_each_role_of_its_participant_types_and_backends_once_sorted_by_name() {
        let yaml = TWO_TYPES
            .replace("type: configured", "type: server")
            .replace("a: {type: server}", "a: {type: server, os: linux}");
        let plan = compile_with(&mut role_planner(&[&SERVER]), &yaml, Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.artifacts(),
            Some(local_bindings(&[FIXUP, RESOURCES, TOOL]).as_slice())
        );
    }

    #[test]
    fn needed_artifacts_deduplicates_and_sorts_roles_across_plans_and_skips_typed_cases() {
        let mut planner = role_planner(&[&SERVER]);
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        let windows = compile_with(&mut planner, &yaml, Os::Windows).unwrap().unwrap();
        let linux = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        let another = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        let typed = compile_with(&mut planner, r#"type: fixture"#, Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(
            needed_artifacts([&windows, &typed, &linux, &another]),
            local_bindings(&[FIXUP, RESOURCES, TOOL, WINDOWS_TOOL])
        );
        assert_eq!(needed_artifacts([&typed]), vec![]);
    }

    #[test]
    fn a_typed_case_plan_has_no_artifacts() {
        let plan = compile_with(&mut planner(), "type: fixture", Os::MacOs)
            .unwrap()
            .unwrap();
        assert_eq!(plan.artifacts(), None);
    }

    #[test]
    fn a_participant_case_with_no_declared_roles_has_an_empty_artifact_list() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]");
        let plan = compile_with(&mut planner(), &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.artifacts(), Some([].as_slice()));
    }

    #[test]
    fn a_planner_rejects_registered_types_declaring_one_role_with_different_kinds_without_compiling_cases() {
        let message = catalog_error(&[
            &SERVER,
            &Type {
                name: "configured",
                roles: &[ArtifactRole {
                    kind: ArtifactKind::Image,
                    ..TOOL
                }],
                windows_roles: &[],
            },
        ]);
        assert_eq!(
            message,
            "artifact role 'tool' is declared as image 'bin/tool' by participant type 'configured' \
             as linux-container on linux, but as binary 'bin/tool' by participant type 'server' \
             as linux-container on linux"
        );
    }

    #[test]
    fn a_planner_rejects_registered_types_declaring_one_role_with_different_local_defaults() {
        let message = catalog_error(&[
            &SERVER,
            &Type {
                name: "configured",
                roles: &[ArtifactRole {
                    local_default: "share/other",
                    ..RESOURCES
                }],
                windows_roles: &[],
            },
        ]);
        assert_eq!(
            message,
            "artifact role 'resources' is declared as install 'share/other' with main binary 'bin/tool' \
             by participant type 'configured' as linux-container on linux, but as install 'share/tool' \
             with main binary 'bin/tool' by participant type 'server' as linux-container on linux"
        );
    }

    #[test]
    fn a_planner_rejects_install_roles_with_different_main_binaries() {
        let message = catalog_error(&[
            &SERVER,
            &Type {
                name: "configured",
                roles: &[ArtifactRole {
                    kind: ArtifactKind::Install {
                        main_binary: "bin/other",
                    },
                    ..RESOURCES
                }],
                windows_roles: &[],
            },
        ]);
        assert_eq!(
            message,
            "artifact role 'resources' is declared as install 'share/tool' with main binary 'bin/other' \
             by participant type 'configured' as linux-container on linux, but as install 'share/tool' \
             with main binary 'bin/tool' by participant type 'server' as linux-container on linux"
        );
    }

    #[test]
    fn a_planner_rejects_main_binaries_outside_the_install_or_with_non_normal_components() {
        const INVALID_ROLES: &[&[ArtifactRole]] = &[
            &[ArtifactRole {
                kind: ArtifactKind::Install { main_binary: "" },
                ..RESOURCES
            }],
            &[ArtifactRole {
                kind: ArtifactKind::Install {
                    main_binary: "/bin/tool",
                },
                ..RESOURCES
            }],
            &[ArtifactRole {
                kind: ArtifactKind::Install { main_binary: "../tool" },
                ..RESOURCES
            }],
            &[ArtifactRole {
                kind: ArtifactKind::Install { main_binary: "./tool" },
                ..RESOURCES
            }],
            &[ArtifactRole {
                kind: ArtifactKind::Install {
                    main_binary: "bin/../tool",
                },
                ..RESOURCES
            }],
            &[ArtifactRole {
                kind: ArtifactKind::Install {
                    main_binary: "bin/./tool",
                },
                ..RESOURCES
            }],
        ];
        for &roles in INVALID_ROLES {
            let message = catalog_error(&[&Type {
                name: "server",
                roles,
                windows_roles: &[],
            }]);
            let ArtifactKind::Install { main_binary } = roles[0].kind else {
                unreachable!()
            };
            assert_eq!(
                message,
                format!(
                    "artifact role 'resources' declared by participant type 'server' as linux-container on linux \
                     has invalid main binary '{main_binary}': expected a nonempty relative path \
                     containing only normal names"
                )
            );
        }
    }

    #[test]
    fn a_planner_rejects_a_type_declaring_one_role_differently_on_two_realizations() {
        let message = catalog_error(&[&Type {
            name: "server",
            roles: &[TOOL],
            windows_roles: &[ArtifactRole {
                local_default: "bin/tool.exe",
                ..TOOL
            }],
        }]);
        assert_eq!(
            message,
            "artifact role 'tool' is declared as binary 'bin/tool.exe' by participant type 'server' \
             as windows-container on windows, but as binary 'bin/tool' by participant type 'server' \
             as linux-container on linux"
        );
    }

    #[test]
    fn a_planner_rejects_a_type_role_that_disagrees_with_a_backend_role() {
        let message = catalog_error(&[&Type {
            name: "server",
            roles: &[ArtifactRole {
                local_default: "other:latest",
                ..FIXUP
            }],
            windows_roles: &[],
        }]);
        assert_eq!(
            message,
            format!(
                "artifact role 'fixup-image' is declared as image '{}' by the linux-container backend, \
             but as image 'other:latest' by participant type 'server' as linux-container on linux",
                airlock::driver::DEFAULT_ALPINE_IMAGE,
            )
        );
        let plan = compile_with(
            &mut role_planner(&[&Type {
                name: "server",
                roles: &[FIXUP],
                windows_roles: &[],
            }]),
            PARTICIPANT_FIELDS,
            Os::Linux,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.artifacts(), Some(local_bindings(&[FIXUP]).as_slice()));
    }

    #[test]
    fn a_planner_reports_every_unknown_manifest_role_and_the_declared_roles_in_name_order() {
        let manifest = ArtifactManifest::parse(
            "schema = 1\n[roles]\nz = 'z'\na = 'a'\ntool = 'bin/other'",
            Path::new("artifacts.toml"),
        )
        .unwrap();
        let message = Planner::new(
            Registries {
                participant_types: &[&SERVER],
                ..CONFIG_REGISTRIES
            },
            manifest,
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(
            message,
            "artifact manifest artifacts.toml: no participant type or backend declares role 'a', 'z'; \
             declared roles: fixup-image, resources, tool, windows-tool"
        );
    }

    #[test]
    fn checking_a_manifest_reports_every_unknown_role_naming_its_origin() {
        let manifest = ArtifactManifest::new(
            "--set",
            BTreeMap::from([
                ("z".into(), "z".into()),
                ("a".into(), "a".into()),
                ("tool".into(), "bin/other".into()),
            ]),
        )
        .unwrap();
        let error = check_artifact_manifest(
            Registries {
                participant_types: &[&SERVER],
                ..CONFIG_REGISTRIES
            },
            &manifest,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "--set: no participant type or backend declares role 'a', 'z'; \
             declared roles: fixup-image, resources, tool, windows-tool"
        );
    }

    #[test]
    fn checking_a_manifest_and_constructing_a_planner_fail_identically() {
        let manifest = ArtifactManifest::new("--set", BTreeMap::from([("unknown".into(), "x".into())])).unwrap();
        let disagreeing = Type {
            name: "server",
            roles: &[TOOL],
            windows_roles: &[ArtifactRole {
                kind: ArtifactKind::Image,
                ..TOOL
            }],
        };
        for participant_type in [&SERVER, &disagreeing] {
            let registries = Registries {
                participant_types: &[participant_type],
                ..CONFIG_REGISTRIES
            };
            let checked = check_artifact_manifest(registries, &manifest).unwrap_err().to_string();
            let planned = Planner::new(registries, manifest.clone()).err().unwrap().to_string();
            assert_eq!(checked, planned);
        }
    }

    #[test]
    fn catalog_disagreement_precedes_unknown_manifest_roles() {
        let manifest =
            ArtifactManifest::parse("schema = 1\n[roles]\nunknown = 'x'", Path::new("artifacts.toml")).unwrap();
        let types: &[&dyn ParticipantType] = &[&Type {
            name: "server",
            roles: &[TOOL],
            windows_roles: &[ArtifactRole {
                kind: ArtifactKind::Image,
                ..TOOL
            }],
        }];
        let message = Planner::new(
            Registries {
                participant_types: types,
                ..CONFIG_REGISTRIES
            },
            manifest,
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(message, catalog_error(types));
    }

    #[test]
    fn a_planner_accepts_a_manifest_role_only_a_backend_declares() {
        let manifest = ArtifactManifest::parse(
            "schema = 1\n[roles]\nfixup-image = 'registry.example/tools:3'",
            Path::new("artifacts.toml"),
        )
        .unwrap();
        Planner::new(
            Registries {
                participant_types: &[],
                steps: &[],
            },
            manifest,
        )
        .unwrap();
    }

    #[test]
    fn a_plan_binds_manifest_roles_and_gives_omitted_roles_their_local_default() {
        let manifest = ArtifactManifest::parse(
            "schema = 1\n[roles]\ntool = '../bin/other'\nfixup-image = 'registry.example/tools:3'",
            Path::new("artifacts.toml"),
        )
        .unwrap();
        let mut planner = Planner::new(
            Registries {
                participant_types: &[&SERVER],
                ..CONFIG_REGISTRIES
            },
            manifest,
        )
        .unwrap();
        let plan = compile_with(&mut planner, PARTICIPANT_FIELDS, Os::Linux)
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.artifacts().unwrap(),
            [
                ArtifactBinding {
                    role: FIXUP,
                    reference: "registry.example/tools:3".into(),
                    source: BindingSource::Manifest
                },
                ArtifactBinding {
                    role: RESOURCES,
                    reference: "share/tool".into(),
                    source: BindingSource::LocalDefault
                },
                ArtifactBinding {
                    role: TOOL,
                    reference: "../bin/other".into(),
                    source: BindingSource::Manifest
                },
            ]
        );
    }

    #[test]
    fn a_manifest_role_the_case_does_not_need_is_ignored() {
        let manifest = ArtifactManifest::parse(
            "schema = 1\n[roles]\nwindows-tool = 'bin/other.exe'\nfixup-image = 'registry.example/tools:3'",
            Path::new("artifacts.toml"),
        )
        .unwrap();
        let mut planner = Planner::new(
            Registries {
                participant_types: &[&SERVER],
                ..CONFIG_REGISTRIES
            },
            manifest,
        )
        .unwrap();
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]");
        let plan = compile_with(&mut planner, &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.artifacts(), Some(local_bindings(&[RESOURCES, TOOL]).as_slice()));
    }

    #[test]
    fn a_plan_lists_only_the_selected_subject_os_roles() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        let mut planner = role_planner(&[&SERVER]);
        let linux = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            linux.artifacts(),
            Some(local_bindings(&[FIXUP, RESOURCES, TOOL]).as_slice())
        );
        let windows = compile_with(&mut planner, &yaml, Os::Windows).unwrap().unwrap();
        assert_eq!(windows.artifacts(), Some(local_bindings(&[WINDOWS_TOOL]).as_slice()));
    }
}
