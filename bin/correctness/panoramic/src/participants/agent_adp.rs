//! The `agent-adp` participant type: the Core Agent and ADP running together.

use crate::config::{case::Os, FactDeclaration, ParticipantDeclaration};

/// Configuration, connection, process and fact names accepted by `agent-adp`.
///
/// `default_log_file` follows ADP's `PlatformSettings::get_default_log_file_path()` in
/// Linux and Windows containers.
pub(crate) const AGENT_ADP: ParticipantDeclaration = ParticipantDeclaration {
    name: "agent-adp",
    config_files: &[],
    endpoints: &[],
    consumes: &[],
    params: &[],
    facts: &[FactDeclaration {
        name: "default_log_file",
        values: &[
            (Os::Linux, "/var/log/datadog/agent-data-plane.log"),
            (Os::Windows, r"C:\ProgramData\Datadog\logs\agent-data-plane.log"),
        ],
    }],
    processes: &["adp", "agent"],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_adp_declares_its_default_log_file_on_linux_and_windows_only() {
        assert_eq!(
            AGENT_ADP.facts.iter().map(|fact| fact.name).collect::<Vec<_>>(),
            ["default_log_file"]
        );
        assert_eq!(
            AGENT_ADP.fact_value("default_log_file", Os::Linux),
            Some("/var/log/datadog/agent-data-plane.log")
        );
        assert_eq!(
            AGENT_ADP.fact_value("default_log_file", Os::Windows),
            Some(r"C:\ProgramData\Datadog\logs\agent-data-plane.log")
        );
        assert_eq!(AGENT_ADP.fact_value("default_log_file", Os::MacOs), None);
        assert_eq!(AGENT_ADP.fact_value("unknown", Os::Linux), None);
    }
}
