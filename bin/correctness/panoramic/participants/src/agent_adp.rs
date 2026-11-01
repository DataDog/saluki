//! The `agent-adp` participant type: the Core Agent and ADP running together.

use panoramic_core::layout::{Os, Realization, RealizationKind};
use panoramic_core::participant::{FactDeclaration, ParticipantType};

/// The Core Agent and ADP running together.
pub struct AgentAdp;

impl AgentAdp {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "agent-adp";
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
