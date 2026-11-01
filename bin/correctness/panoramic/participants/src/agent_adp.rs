//! The `agent-adp` participant type: the Core Agent and ADP running together.

use panoramic_core::artifact::{ArtifactKind, ArtifactRole};
use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::participant::{FactDeclaration, ParticipantType};

/// The Core Agent and ADP running together.
pub struct AgentAdp;

impl AgentAdp {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "agent-adp";

    /// Image providing the Core Agent and ADP in Linux containers.
    pub const LINUX_IMAGE: ArtifactRole = ArtifactRole {
        name: "agent-adp-linux-image",
        kind: ArtifactKind::Image,
        local_default: "saluki-images/datadog-agent:testing-devel",
    };

    /// Image providing the Core Agent and ADP in Windows containers.
    pub const WINDOWS_IMAGE: ArtifactRole = ArtifactRole {
        name: "agent-adp-windows-image",
        kind: ArtifactKind::Image,
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
}

#[cfg(test)]
mod tests {
    use panoramic_core::participant::EffectiveConfig;

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
                    kind: ArtifactKind::Image,
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
                    kind: ArtifactKind::Image,
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
    fn agent_adp_serves_and_consumes_no_endpoints_on_any_supported_realization() {
        let config = EffectiveConfig::default();
        for &(os, kind) in AgentAdp.supports() {
            let realization = Realization { os, kind };
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
