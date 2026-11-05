//! Validates the registered role catalog and resolves each case's artifact bindings.

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
    layout::{Layout, Os, Realization, RealizationKind},
    participant::{find_type, ParticipantType},
};

/// Checks bindings against the artifact roles declared by all registered participant types, backends and typed schemas.
///
/// # Errors
///
/// Returns an error for macOS images or container binaries, invalid install main-binary paths
/// or conflicting role declarations, then for unknown manifest roles, naming the manifest's origin.
pub fn check_artifact_manifest(registries: Registries<'_>, manifest: &ArtifactManifest) -> Result<(), GenericError> {
    let catalog = role_catalog(registries).map_err(|error| generic_error!(error))?;
    check_manifest(&catalog, manifest).map_err(|error| generic_error!(error))
}

pub(super) fn role_catalog(registries: Registries<'_>) -> Result<BTreeMap<&'static str, ArtifactRole>, String> {
    let mut roles: BTreeMap<_, (ArtifactRole, String)> = BTreeMap::new();
    let mut declare = |role: ArtifactRole, declarer: String| {
        if role.kind == (ArtifactKind::Image { os: Os::MacOs }) {
            return Err(format!(
                "artifact role '{}' declared by {declarer} is a macos image; images run on linux or windows",
                role.name
            ));
        }
        if let ArtifactKind::ContainerBinary { os } = role.kind {
            if os == Os::MacOs {
                return Err(format!(
                    "artifact role '{}' declared by {declarer} is a {} container binary; \
                     container binaries run on linux or windows",
                    role.name,
                    os.name()
                ));
            }
        }
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
    for participant_type in registries.participant_types {
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
    for &role in registries.typed_case_roles {
        declare(role, "typed case schemas".to_string())?;
    }
    Ok(roles.into_iter().map(|(name, (role, _))| (name, role)).collect())
}

pub(super) fn check_manifest(
    catalog: &BTreeMap<&str, ArtifactRole>, manifest: &ArtifactManifest,
) -> Result<(), String> {
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
) -> (Vec<ArtifactBinding>, BTreeMap<RealizationKind, Vec<ArtifactRole>>) {
    let mut image_roles: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    let roles = layout.iter().flat_map(|(name, realization)| {
        let entry = &file.participants[name];
        let participant_type =
            find_type(types, &entry.participant_type).expect("laid-out participants have registered types");
        participant_type
            .artifacts(realization)
            .iter()
            .chain(backend::artifacts(realization.kind))
            .copied()
            .map(move |role| (realization.kind, role))
    });
    let bindings = bind_roles(
        roles.map(|(kind, role)| {
            if matches!(role.kind, ArtifactKind::Image { .. }) {
                image_roles.entry(kind).or_default().insert(role.name, role);
            }
            role
        }),
        bindings,
    );
    let image_roles = image_roles
        .into_iter()
        .map(|(kind, roles)| (kind, roles.into_values().collect()))
        .collect();
    (bindings, image_roles)
}

pub(super) fn bind_roles(
    roles: impl IntoIterator<Item = ArtifactRole>, bindings: &BTreeMap<String, String>,
) -> Vec<ArtifactBinding> {
    roles
        .into_iter()
        .map(|role| (role.name, role))
        .collect::<BTreeMap<_, _>>()
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

/// Collects one binding per role, sorted by name.
/// Plans must come from one planner, which binds each role identically.
pub fn needed_artifacts<'p, C: 'p>(plans: impl IntoIterator<Item = &'p CasePlan<C>>) -> Vec<ArtifactBinding> {
    let mut bindings = BTreeMap::new();
    for plan in plans {
        for binding in plan.artifacts() {
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
        backend::{COURIER_LINUX_BINARY, COURIER_WINDOWS_BINARY},
        case::{
            fixtures::{minimal_procedure, FixtureConfig, FixtureTypesWithRoles, PARTICIPANT_FIELDS},
            CaseConfig, CaseLoader, Registries,
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
        kind: ArtifactKind::Image { os: Os::Linux },
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
                (Os::Linux, RealizationKind::KubernetesPod),
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

    fn typed_case(roles: BTreeMap<Os, Vec<ArtifactRole>>) -> CaseConfig<FixtureConfig> {
        CaseLoader::new(CONFIG_REGISTRIES, FixtureTypesWithRoles(roles))
            .load_content("cases", "type: fixture", "typed", Path::new("typed/config.yaml"))
            .unwrap()
            .remove(0)
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
            local_bindings(&[COURIER_LINUX_BINARY, FIXUP, RESOURCES, TOOL]).as_slice()
        );
    }

    #[test]
    fn image_roles_are_bound_once_per_realization_without_binary_roles() {
        const POD_IMAGE: ArtifactRole = ArtifactRole {
            name: "pod-image",
            local_default: "pod:local",
            ..FIXUP
        };
        let mut planner = role_planner(&[
            &Type {
                name: "server",
                roles: &[POD_IMAGE, TOOL],
                windows_roles: &[],
            },
            &Type {
                name: "configured",
                roles: &[TOOL],
                windows_roles: &[],
            },
        ]);
        let yaml = concat!(
            "timeout: 1s\nsubject_os: [linux]\nparticipants:\n",
            "  pod: {type: server, subject: true, realization: {kind: kubernetes-pod}}\n",
            "  other: {type: server}\n",
            "  container: {type: configured, realization: {kind: linux-container}}\n",
            minimal_procedure!(),
            "\n"
        );
        let plan = compile_with(&mut planner, yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(plan.image_roles_for(RealizationKind::KubernetesPod), [POD_IMAGE]);
        assert_eq!(plan.image_roles_for(RealizationKind::LinuxContainer), [FIXUP]);
        assert!(plan.image_roles_for(RealizationKind::HostProcessGroup).is_empty());
        let typed = planner
            .compile(
                typed_case(BTreeMap::from([(Os::MacOs, vec![TOOL]), (Os::Windows, vec![])])),
                Os::MacOs,
            )
            .unwrap()
            .into_plan()
            .unwrap();
        assert!(typed.image_roles_for(RealizationKind::KubernetesPod).is_empty());
    }

    #[test]
    fn needed_artifacts_deduplicates_and_sorts_roles_across_participant_and_typed_plans() {
        let mut planner = role_planner(&[&SERVER]);
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        let windows = compile_with(&mut planner, &yaml, Os::Windows).unwrap().unwrap();
        let linux = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        let another = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        let typed = planner
            .compile(
                typed_case(BTreeMap::from([(Os::MacOs, vec![TOOL]), (Os::Windows, vec![])])),
                Os::MacOs,
            )
            .unwrap()
            .into_plan()
            .unwrap();
        assert_eq!(
            needed_artifacts([&windows, &typed, &linux, &another]),
            local_bindings(&[
                COURIER_LINUX_BINARY,
                COURIER_WINDOWS_BINARY,
                FIXUP,
                RESOURCES,
                TOOL,
                WINDOWS_TOOL
            ])
        );
        assert_eq!(needed_artifacts([&typed]), local_bindings(&[TOOL]));
    }

    #[test]
    fn a_typed_case_plan_binds_selected_os_artifact_roles_once_in_name_order() {
        let manifest =
            ArtifactManifest::new("--set", BTreeMap::from([("tool".into(), "bin/override".into())])).unwrap();
        let mut planner = Planner::new(
            Registries {
                participant_types: &[&SERVER],
                ..CONFIG_REGISTRIES
            },
            manifest,
        )
        .unwrap();
        for (os, expected) in [
            (
                Os::MacOs,
                vec![
                    local_bindings(&[RESOURCES]).remove(0),
                    ArtifactBinding {
                        role: TOOL,
                        reference: "bin/override".into(),
                        source: BindingSource::Manifest,
                    },
                ],
            ),
            (Os::Windows, local_bindings(&[WINDOWS_TOOL])),
        ] {
            let case = typed_case(BTreeMap::from([
                (Os::MacOs, vec![TOOL, RESOURCES, TOOL]),
                (Os::Windows, vec![WINDOWS_TOOL]),
            ]));
            let plan = planner.compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.artifacts(), expected);
        }
    }

    #[test]
    fn a_typed_case_artifact_role_must_match_a_catalog_declaration() {
        for (role, expected) in [
            (
                ArtifactRole {
                    name: "unknown",
                    ..TOOL
                },
                "artifact role 'unknown' is not declared by any participant type, backend or typed case schema",
            ),
            (
                ArtifactRole {
                    local_default: "bin/different",
                    ..TOOL
                },
                "artifact role 'tool' is binary 'bin/different' but declared as binary 'bin/tool'",
            ),
            (
                ArtifactRole {
                    kind: ArtifactKind::Image { os: Os::Linux },
                    ..TOOL
                },
                "artifact role 'tool' is linux image 'bin/tool' but declared as binary 'bin/tool'",
            ),
        ] {
            let case = typed_case(BTreeMap::from([(Os::MacOs, vec![role]), (Os::Windows, vec![])]));
            let error = role_planner(&[&SERVER]).compile(case, Os::MacOs).err().unwrap();
            assert_eq!(
                error.to_string(),
                format!("case 'typed' in typed/config.yaml: on subject OS 'macos', {expected}")
            );
        }
    }

    #[test]
    fn a_role_only_typed_schemas_declare_is_accepted_by_manifest_checking_and_planning() {
        let registries = Registries {
            typed_case_roles: &[TOOL],
            ..Default::default()
        };
        let manifest =
            ArtifactManifest::new("--set", BTreeMap::from([("tool".into(), "bin/override".into())])).unwrap();
        check_artifact_manifest(registries, &manifest).unwrap();
        let mut planner = Planner::new(registries, manifest).unwrap();
        let case = typed_case(BTreeMap::from([(Os::MacOs, vec![TOOL]), (Os::Windows, vec![])]));
        let plan = planner.compile(case, Os::MacOs).unwrap().into_plan().unwrap();
        assert_eq!(
            plan.artifacts(),
            [ArtifactBinding {
                role: TOOL,
                reference: "bin/override".into(),
                source: BindingSource::Manifest,
            }]
        );
    }

    #[test]
    fn a_typed_schema_artifact_role_cannot_disagree_with_a_participant_type() {
        let roles = [ArtifactRole {
            kind: ArtifactKind::Image { os: Os::Linux },
            ..TOOL
        }];
        let registries = Registries {
            participant_types: &[&SERVER],
            typed_case_roles: &roles,
            ..Default::default()
        };
        let manifest = ArtifactManifest::default();
        let checked = check_artifact_manifest(registries, &manifest).unwrap_err();
        let planned = Planner::new(registries, manifest).err().unwrap();
        let expected = "artifact role 'tool' is declared as linux image 'bin/tool' by typed case schemas, \
                        but as binary 'bin/tool' by participant type 'server' as linux-container on linux";
        assert_eq!(checked.to_string(), expected);
        assert_eq!(planned.to_string(), expected);
    }

    #[test]
    fn a_participant_case_with_no_declared_roles_has_an_empty_artifact_list() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]");
        let plan = compile_with(&mut planner(), &yaml, Os::MacOs).unwrap().unwrap();
        assert_eq!(plan.artifacts(), [].as_slice());
    }

    #[test]
    fn a_planner_rejects_registered_types_declaring_one_role_with_different_kinds_without_compiling_cases() {
        let message = catalog_error(&[
            &SERVER,
            &Type {
                name: "configured",
                roles: &[ArtifactRole {
                    kind: ArtifactKind::Image { os: Os::Linux },
                    ..TOOL
                }],
                windows_roles: &[],
            },
        ]);
        assert_eq!(
            message,
            "artifact role 'tool' is declared as linux image 'bin/tool' by participant type 'configured' \
             as linux-container on linux, but as binary 'bin/tool' by participant type 'server' \
             as linux-container on linux"
        );
    }

    #[test]
    fn a_planner_rejects_an_image_role_for_macos_before_compiling_cases() {
        let message = catalog_error(&[&Type {
            name: "server",
            roles: &[ArtifactRole {
                kind: ArtifactKind::Image { os: Os::MacOs },
                ..TOOL
            }],
            windows_roles: &[],
        }]);
        assert_eq!(
            message,
            "artifact role 'tool' declared by participant type 'server' as linux-container on linux \
             is a macos image; images run on linux or windows"
        );
    }

    #[test]
    fn the_role_catalog_accepts_windows_container_binaries_and_rejects_macos_ones() {
        for os in [Os::Linux, Os::Windows, Os::MacOs] {
            let roles = [ArtifactRole {
                kind: ArtifactKind::ContainerBinary { os },
                ..TOOL
            }];
            let registries = Registries {
                typed_case_roles: &roles,
                ..Default::default()
            };
            let result = super::role_catalog(registries);
            if os == Os::MacOs {
                assert_eq!(result.unwrap_err(), "artifact role 'tool' declared by typed case schemas is a macos container binary; container binaries run on linux or windows");
            } else {
                assert_eq!(result.unwrap()["tool"], roles[0]);
            }
        }
    }

    #[test]
    fn a_planner_rejects_image_roles_with_different_oses_naming_both() {
        let message = catalog_error(&[&Type {
            name: "server",
            roles: &[ArtifactRole {
                kind: ArtifactKind::Image { os: Os::Linux },
                ..TOOL
            }],
            windows_roles: &[ArtifactRole {
                kind: ArtifactKind::Image { os: Os::Windows },
                ..TOOL
            }],
        }]);
        assert_eq!(
            message,
            "artifact role 'tool' is declared as windows image 'bin/tool' by participant type 'server' \
             as windows-container on windows, but as linux image 'bin/tool' by participant type 'server' \
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
                "artifact role 'fixup-image' is declared as linux image '{}' by the linux-container backend, \
             but as linux image 'other:latest' by participant type 'server' as linux-container on linux",
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
        assert_eq!(
            plan.artifacts(),
            local_bindings(&[COURIER_LINUX_BINARY, FIXUP]).as_slice()
        );
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
             declared roles: courier-linux-binary, courier-windows-binary, fixup-image, resources, tool, windows-tool"
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
             declared roles: courier-linux-binary, courier-windows-binary, fixup-image, resources, tool, windows-tool"
        );
    }

    #[test]
    fn checking_a_manifest_and_constructing_a_planner_fail_identically() {
        let manifest = ArtifactManifest::new("--set", BTreeMap::from([("unknown".into(), "x".into())])).unwrap();
        let disagreeing = Type {
            name: "server",
            roles: &[TOOL],
            windows_roles: &[ArtifactRole {
                kind: ArtifactKind::Image { os: Os::Linux },
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
                kind: ArtifactKind::Image { os: Os::Linux },
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
        Planner::new(Registries::default(), manifest).unwrap();
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
            plan.artifacts(),
            [
                ArtifactBinding {
                    role: COURIER_LINUX_BINARY,
                    reference: COURIER_LINUX_BINARY.local_default.into(),
                    source: BindingSource::LocalDefault
                },
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

    #[tokio::test]
    async fn a_binary_without_a_local_default_verifies_only_with_a_manifest_binding() {
        const REQUIRED_TOOL: ArtifactRole = ArtifactRole {
            local_default: "",
            ..TOOL
        };
        let dir = tempfile::tempdir().unwrap();
        crate::artifact::test_util::write_executable(&dir.path().join("bin/tool"));
        for (roles, reference, source, identity) in [
            (
                BTreeMap::new(),
                "",
                BindingSource::LocalDefault,
                Err("role 'tool' requires a binding in an artifact manifest"),
            ),
            (
                BTreeMap::from([("tool".into(), "bin/tool".into())]),
                "bin/tool",
                BindingSource::Manifest,
                Ok("sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d"),
            ),
        ] {
            let mut planner = Planner::new(
                Registries {
                    typed_case_roles: &[REQUIRED_TOOL],
                    ..Default::default()
                },
                ArtifactManifest::new("test", roles).unwrap(),
            )
            .unwrap();
            let case = typed_case(BTreeMap::from([
                (Os::MacOs, vec![REQUIRED_TOOL]),
                (Os::Windows, vec![]),
            ]));
            let plan = planner.compile(case, Os::MacOs).unwrap().into_plan().unwrap();
            assert_eq!(
                plan.artifacts(),
                &[ArtifactBinding {
                    role: REQUIRED_TOOL,
                    reference: reference.into(),
                    source,
                }]
            );
            let results = crate::backend::RunBackends::default()
                .verify(plan.artifacts(), dir.path())
                .await;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].identity.as_deref().map_err(String::as_str), identity);
        }
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
        assert_eq!(plan.artifacts(), local_bindings(&[RESOURCES, TOOL]).as_slice());
    }

    #[test]
    fn a_plan_lists_only_the_selected_subject_os_roles() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        let mut planner = role_planner(&[&SERVER]);
        let linux = compile_with(&mut planner, &yaml, Os::Linux).unwrap().unwrap();
        assert_eq!(
            linux.artifacts(),
            local_bindings(&[COURIER_LINUX_BINARY, FIXUP, RESOURCES, TOOL]).as_slice()
        );
        let windows = compile_with(&mut planner, &yaml, Os::Windows).unwrap().unwrap();
        assert_eq!(
            windows.artifacts(),
            local_bindings(&[COURIER_WINDOWS_BINARY, WINDOWS_TOOL]).as_slice()
        );
    }
}
