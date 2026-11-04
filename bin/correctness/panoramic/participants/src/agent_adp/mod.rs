//! The `agent-adp` participant type: the Core Agent and ADP running together.
//!
//! Uses the default ten-second stop grace; s6 stops both services in about 2.6 s.

use std::collections::BTreeMap;

use panoramic_core::artifact::{ArtifactKind, ArtifactRole};
use panoramic_core::endpoint::{ConsumedEndpoint, Endpoint};
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::participant::{
    CaseValues, EffectiveConfig, FactDeclaration, Operation, ParticipantType, ProcessSpec, RenderedConfig,
};
use panoramic_core::recipe::Recipe;

mod linux;

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
            _ => Err(format!(
                "participant type '{}' has no recipe for {} on {}",
                Self::NAME,
                realization.kind.name(),
                realization.os.name()
            )),
        }
    }

    fn validate_config(&self, realization: Realization, config: &CaseValues) -> Result<(), String> {
        if matches!(
            (realization.os, realization.kind),
            (Os::Linux, RealizationKind::LinuxContainer)
        ) {
            let mut env = linux::default_env();
            env.extend(config.env.clone());
            linux::endpoints(&env)?;
        }
        Ok(())
    }

    fn default_env(&self, realization: Realization, _: &CaseValues) -> BTreeMap<String, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::default_env(),
            _ => BTreeMap::new(),
        }
    }

    fn endpoints(
        &self, realization: Realization, config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => linux::endpoints(&config.env),
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
            (Os::Linux, RealizationKind::LinuxContainer) => &[FactDeclaration {
                name: "default_log_file",
                value: "/var/log/datadog/agent-data-plane.log",
            }],
            (Os::Windows, RealizationKind::WindowsContainer) => &[FactDeclaration {
                name: "default_log_file",
                value: r"C:\ProgramData\Datadog\logs\agent-data-plane.log",
            }],
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
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn config_dump_is_offered_only_for_linux_containers() {
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
                    vec!["config_dump"]
                } else {
                    vec![]
                };
                assert_eq!(names, expected, "{os:?} {kind:?}");
            }
        }
    }

    #[test]
    fn agent_adp_keeps_macos_and_windows_declarations_unchanged() {
        let config = EffectiveConfig::default();
        for &(os, kind) in &AgentAdp.supports()[1..] {
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
    fn agent_adp_provides_default_log_file_for_its_linux_and_windows_containers_only() {
        for (os, kind, expected) in [
            (
                Os::Linux,
                RealizationKind::LinuxContainer,
                vec![("default_log_file", "/var/log/datadog/agent-data-plane.log")],
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
