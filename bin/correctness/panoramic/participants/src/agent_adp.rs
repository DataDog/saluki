//! The `agent-adp` participant type: the Core Agent and ADP running together.

use panoramic_core::layout::{Os, RealizationKind};
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
    fn facts(&self) -> &'static [FactDeclaration] {
        &[FactDeclaration {
            name: "default_log_file",
            values: &[
                (Os::Linux, "/var/log/datadog/agent-data-plane.log"),
                (Os::Windows, r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
            ],
        }]
    }

    fn processes(&self) -> &'static [&'static str] {
        &["adp", "agent"]
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
    fn agent_adp_declares_its_default_log_file_on_linux_and_windows_only() {
        assert_eq!(
            AgentAdp.facts().iter().map(|fact| fact.name).collect::<Vec<_>>(),
            ["default_log_file"]
        );
        assert_eq!(
            AgentAdp.fact_value("default_log_file", Os::Linux),
            Some("/var/log/datadog/agent-data-plane.log")
        );
        assert_eq!(
            AgentAdp.fact_value("default_log_file", Os::Windows),
            Some(r"C:\ProgramData\Datadog\logs\agent-data-plane.log")
        );
        assert_eq!(AgentAdp.fact_value("default_log_file", Os::MacOs), None);
        assert_eq!(AgentAdp.fact_value("unknown", Os::Linux), None);
    }
}
