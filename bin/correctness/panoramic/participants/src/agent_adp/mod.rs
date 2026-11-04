//! The `agent-adp` participant type: the Core Agent and ADP running together.
//!
//! Runs under s6 in Linux containers or as a native macOS host process group.
//! Uses the default ten-second stop grace.

use std::collections::BTreeMap;

use panoramic_core::artifact::{ArtifactKind, ArtifactRole};
use panoramic_core::backend::LateFact;
use panoramic_core::endpoint::{ConsumedEndpoint, Endpoint};
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::participant::{
    CaseValues, EffectiveConfig, FactDeclaration, Operation, ParticipantType, ProcessSpec, RenderedConfig,
};
use panoramic_core::recipe::Recipe;

mod late;
mod linux;
mod listeners;
mod macos;
mod operations;

/// The Core Agent and ADP running together.
pub struct AgentAdp;

impl AgentAdp {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "agent-adp";

    /// Image providing the Core Agent and ADP in Linux containers.
    pub const LINUX_IMAGE: ArtifactRole = ArtifactRole {
        name: "agent-adp-linux-image",
        kind: ArtifactKind::Image { os: Os::Linux },
        local_default: "saluki-images/datadog-agent:testing-devel",
    };

    /// Image providing the Core Agent and ADP in Windows containers.
    pub const WINDOWS_IMAGE: ArtifactRole = ArtifactRole {
        name: "agent-adp-windows-image",
        kind: ArtifactKind::Image { os: Os::Windows },
        local_default: "saluki-images/agent-data-plane:testing-windows",
    };

    /// ADP executable for macOS host processes.
    pub const ADP_HOST_BINARY: ArtifactRole = ArtifactRole {
        name: "adp-host-binary",
        kind: ArtifactKind::Binary,
        local_default: "target/release/agent-data-plane",
    };

    /// Core Agent install and main binary for macOS host processes.
    /// Defaults to the sandbox install `make provision-macos-test-env` writes.
    pub const AGENT_HOST_INSTALL: ArtifactRole = ArtifactRole {
        name: "agent-host-install",
        kind: ArtifactKind::Install {
            main_binary: "bin/agent/agent",
        },
        local_default: "/tmp/saluki-dda/datadog-agent",
    };
}

impl ParticipantType for AgentAdp {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ]
    }

    fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[Self::LINUX_IMAGE],
            (Os::MacOs, RealizationKind::HostProcessGroup) => &[Self::ADP_HOST_BINARY, Self::AGENT_HOST_INSTALL],
            (Os::Windows, RealizationKind::WindowsContainer) => &[Self::WINDOWS_IMAGE],
            _ => &[],
        }
    }

    fn recipe(&self, realization: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::recipe(config),
            (Os::MacOs, RealizationKind::HostProcessGroup) => macos::recipe(config),
            _ => Err(format!(
                "participant type '{}' has no recipe for {} on {}",
                Self::NAME,
                realization.kind.name(),
                realization.os.name()
            )),
        }
    }

    fn config_files(&self) -> &'static [&'static str] {
        &["cont-init.d/", "files/"]
    }

    fn params(&self) -> &'static [&'static str] {
        &["extra_hosts"]
    }

    fn validate_config(&self, realization: Realization, config: &CaseValues) -> Result<(), String> {
        if (realization.os, realization.kind) != (Os::Linux, RealizationKind::LinuxContainer) {
            if config
                .params
                .get("extra_hosts")
                .is_some_and(|hosts| hosts.as_object().is_none_or(|hosts| !hosts.is_empty()))
            {
                return Err("agent-adp extra_hosts is supported in Linux containers only".into());
            }
            if !config.files.is_empty() {
                return Err("agent-adp case files are supported in Linux containers only".into());
            }
        }
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => {
                late::validate(&config.env, &config.params)?;
                let mut env = linux::default_env();
                env.extend(config.env.clone());
                listeners::endpoints(realization, &env)?;
            }
            (Os::MacOs, RealizationKind::HostProcessGroup) => macos::validate_config(realization, config)?,
            _ => {}
        }
        Ok(())
    }

    fn default_env(&self, realization: Realization, _: &CaseValues) -> BTreeMap<String, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::default_env(),
            (Os::MacOs, RealizationKind::HostProcessGroup) => macos::default_env(),
            _ => BTreeMap::new(),
        }
    }

    fn endpoints(
        &self, realization: Realization, config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) | (Os::MacOs, RealizationKind::HostProcessGroup) => {
                listeners::endpoints(realization, &config.env)
            }
            _ => Ok(BTreeMap::new()),
        }
    }

    fn consumes(&self, realization: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::consumes(),
            _ => BTreeMap::new(),
        }
    }

    /// `default_log_file` follows ADP's `PlatformSettings::get_default_log_file_path()` in
    /// Linux and Windows containers.
    fn facts(&self, realization: Realization) -> &'static [FactDeclaration] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[
                FactDeclaration {
                    name: "default_log_file",
                    value: "/var/log/datadog/agent-data-plane.log",
                },
                FactDeclaration {
                    name: "files_dir",
                    value: crate::CASE_FILES_DIR,
                },
            ],
            (Os::Windows, RealizationKind::WindowsContainer) => &[FactDeclaration {
                name: "default_log_file",
                value: r"C:\ProgramData\Datadog\logs\agent-data-plane.log",
            }],
            _ => &[],
        }
    }

    fn substituted_late_facts(&self, realization: Realization) -> &'static [LateFact] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[LateFact::Ip],
            _ => &[],
        }
    }

    fn processes(&self) -> &'static [&'static str] {
        &["adp", "agent"]
    }

    fn operations(&self, realization: Realization) -> &'static [&'static dyn Operation] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::OPERATIONS,
            _ => &[],
        }
    }

    fn process_specs(&self, realization: Realization) -> &'static [ProcessSpec] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &linux::PROCESS_SPECS,
            (Os::MacOs, RealizationKind::HostProcessGroup) => &macos::PROCESS_SPECS,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_adp_substitutes_its_own_ip_only_in_linux_containers() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::WindowsContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::KubernetesPod,
            ] {
                let expected: &[LateFact] = if (os, kind) == (Os::Linux, RealizationKind::LinuxContainer) {
                    &[LateFact::Ip]
                } else {
                    &[]
                };
                assert_eq!(AgentAdp.substituted_late_facts(Realization { os, kind }), expected);
            }
        }
    }

    #[test]
    fn extra_hosts_are_rejected_outside_linux_containers() {
        assert_eq!(AgentAdp.params(), &["extra_hosts"]);
        for &(os, kind) in &AgentAdp.supports()[1..] {
            for (hosts, accepted) in [
                (serde_json::json!({}), true),
                (serde_json::json!({"foo.local": "self"}), false),
                (serde_json::json!("self"), false),
            ] {
                let config = CaseValues {
                    params: BTreeMap::from([("extra_hosts".into(), hosts)]),
                    ..Default::default()
                };
                let result = AgentAdp.validate_config(Realization { os, kind }, &config);
                if accepted {
                    result.unwrap();
                } else {
                    assert_eq!(
                        result.unwrap_err(),
                        "agent-adp extra_hosts is supported in Linux containers only"
                    );
                }
            }
        }
    }

    #[test]
    fn agent_adp_supports_linux_containers_macos_host_process_groups_and_windows_containers() {
        assert_eq!(
            AgentAdp.supports(),
            &[
                (Os::Linux, RealizationKind::LinuxContainer),
                (Os::MacOs, RealizationKind::HostProcessGroup),
                (Os::Windows, RealizationKind::WindowsContainer)
            ]
        );
    }

    #[test]
    fn agent_adp_declares_an_image_per_container_and_a_host_binary_and_install_on_macos() {
        let table = [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                vec![ArtifactRole {
                    name: "agent-adp-linux-image",
                    kind: ArtifactKind::Image { os: Os::Linux },
                    local_default: "saluki-images/datadog-agent:testing-devel",
                }],
            ),
            (
                Os::MacOs,
                RealizationKind::HostProcessGroup,
                vec![
                    ArtifactRole {
                        name: "adp-host-binary",
                        kind: ArtifactKind::Binary,
                        local_default: "target/release/agent-data-plane",
                    },
                    ArtifactRole {
                        name: "agent-host-install",
                        kind: ArtifactKind::Install {
                            main_binary: "bin/agent/agent",
                        },
                        local_default: "/tmp/saluki-dda/datadog-agent",
                    },
                ],
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                vec![ArtifactRole {
                    name: "agent-adp-windows-image",
                    kind: ArtifactKind::Image { os: Os::Windows },
                    local_default: "saluki-images/agent-data-plane:testing-windows",
                }],
            ),
        ];
        for (os, kind, expected) in &table {
            assert_eq!(
                AgentAdp.artifacts(Realization { os: *os, kind: *kind }),
                expected.as_slice(),
                "{os:?} {kind:?}"
            );
        }
        assert_eq!(
            table.iter().map(|(os, kind, _)| (*os, *kind)).collect::<Vec<_>>(),
            AgentAdp.supports()
        );
    }

    #[test]
    fn agent_adp_offers_its_operations_only_in_linux_containers() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::WindowsContainer,
                RealizationKind::KubernetesPod,
            ] {
                let names = AgentAdp
                    .operations(Realization { os, kind })
                    .iter()
                    .map(|operation| operation.name())
                    .collect::<Vec<_>>();
                let expected = if (os, kind) == (Os::Linux, RealizationKind::LinuxContainer) {
                    vec![
                        "cli_adp",
                        "cli_agent",
                        "config_dump",
                        "config_set",
                        "dogstatsd_capture",
                        "dogstatsd_replay",
                        "dogstatsd_stats",
                    ]
                } else {
                    vec![]
                };
                assert_eq!(names, expected, "{os:?} {kind:?}");
            }
        }
    }

    #[test]
    fn agent_adp_keeps_windows_declarations_unchanged() {
        let config = EffectiveConfig::default();
        for &(os, kind) in &AgentAdp.supports()[2..] {
            let realization = Realization { os, kind };
            assert!(AgentAdp.default_env(realization, &CaseValues::default()).is_empty());
            assert!(AgentAdp.process_specs(realization).is_empty());
            assert!(AgentAdp
                .recipe(
                    realization,
                    &panoramic_core::recipe::test_support::rendered(BTreeMap::new(), BTreeMap::new())
                )
                .is_err());
            assert!(AgentAdp.endpoints(realization, &config).unwrap().is_empty());
            assert!(AgentAdp.consumes(realization, &config).is_empty());
        }
    }

    #[test]
    fn case_files_are_rejected_outside_linux_containers() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::WindowsContainer,
                RealizationKind::KubernetesPod,
            ] {
                let realization = Realization { os, kind };
                AgentAdp.validate_config(realization, &CaseValues::default()).unwrap();
                for name in ["files/", "cont-init.d/"] {
                    let config = CaseValues {
                        files: BTreeMap::from([(
                            name.into(),
                            panoramic_core::case::FileSource {
                                from: "case-files".into(),
                                template: false,
                            },
                        )]),
                        ..Default::default()
                    };
                    let result = AgentAdp.validate_config(realization, &config);
                    if (os, kind) == (Os::Linux, RealizationKind::LinuxContainer) {
                        result.unwrap();
                    } else {
                        assert_eq!(
                            result.unwrap_err(),
                            "agent-adp case files are supported in Linux containers only",
                            "{os:?} {kind:?}: {name}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn agent_adp_provides_realization_specific_log_and_case_file_facts() {
        for (os, kind, expected) in [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                vec![
                    ("default_log_file", "/var/log/datadog/agent-data-plane.log"),
                    ("files_dir", "/var/lib/panoramic/files"),
                ],
            ),
            (
                Os::Windows,
                RealizationKind::WindowsContainer,
                vec![("default_log_file", r"C:\ProgramData\Datadog\logs\agent-data-plane.log")],
            ),
            (Os::MacOs, RealizationKind::HostProcessGroup, vec![]),
            (Os::Linux, RealizationKind::KubernetesPod, vec![]),
        ] {
            assert_eq!(
                AgentAdp
                    .facts(Realization { os, kind })
                    .iter()
                    .map(|fact| (fact.name, fact.value))
                    .collect::<Vec<_>>(),
                expected,
                "{os:?} {kind:?}"
            );
        }
    }
}
