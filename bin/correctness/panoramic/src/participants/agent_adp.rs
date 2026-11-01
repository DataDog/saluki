//! The `agent-adp` participant type: the Core Agent and ADP running together.

use panoramic_core::case::{FactDeclaration, Os, ParticipantDeclaration};

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
    use panoramic_core::case::{CaseSchema, Need, ProcedureNode};

    use super::*;
    use crate::config::tests::{write_case, PARTICIPANT_FIELDS};

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

    #[test]
    fn default_log_file_references_load_on_linux_and_windows_but_not_macos() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace("procedure: []", "procedure: [{step: file_contains, on: adp, path: '{{ adp.facts.default_log_file }}', pattern: ready, timeout: 1s}]");
        for oses in ["linux", "windows", "linux, windows"] {
            let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", &format!("[{oses}]")));
            let cases = crate::CASE_LOADER.load_file(&path).unwrap();
            let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
                panic!("expected participant data")
            };
            let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                panic!("expected one step")
            };
            assert_eq!(
                step.needs(),
                [
                    Need::Handle("adp".into()),
                    Need::Fact("adp".into(), "default_log_file".into())
                ]
            );
        }
        let path = write_case(dir.path(), "alpha", &yaml.replace("[linux]", "[linux, macos, windows]"));
        let message = crate::CASE_LOADER
            .load_file(&path)
            .err()
            .expect("macOS has no declared default log file")
            .to_string();
        assert_eq!(message, format!(
            "case 'alpha' in {}: step 1 'file_contains' references fact 'default_log_file' of participant 'adp', which type 'agent-adp' does not provide on macos; it provides it on linux, windows",
            path.canonicalize().unwrap().display()
        ));
    }

    #[test]
    fn agent_adp_log_steps_may_name_the_adp_and_agent_processes_only() {
        let dir = tempfile::tempdir().unwrap();
        for (step, duration) in [("log_contains", "timeout"), ("log_not_contains", "during")] {
            for process in ["adp", "agent", "core"] {
                let yaml = PARTICIPANT_FIELDS.replace(
                    "procedure: []",
                    &format!(
                        "procedure: [{{step: {step}, on: adp, process: {process}, pattern: ready, {duration}: 1s}}]"
                    ),
                );
                let path = write_case(dir.path(), "alpha", &yaml);
                if process == "core" {
                    let error = crate::CASE_LOADER
                        .load_file(&path)
                        .err()
                        .expect("undeclared process must fail");
                    assert_eq!(
                        error.to_string(),
                        format!(
                            "case 'alpha' in {}: step 1 '{step}' names process 'core' of participant 'adp', which type 'agent-adp' does not run; it runs adp, agent",
                            path.canonicalize().unwrap().display()
                        )
                    );
                } else {
                    let cases = crate::CASE_LOADER.load_file(&path).unwrap();
                    assert_eq!(cases.len(), 1);
                    let CaseSchema::Participants { procedure, .. } = cases[0].schema() else {
                        panic!("expected participant data");
                    };
                    let [ProcedureNode::Step(step)] = procedure.as_slice() else {
                        panic!("expected one built step");
                    };
                    assert_eq!(step.needs(), vec![Need::Process("adp".into(), process.into())]);
                }
            }
        }
    }

    #[test]
    fn agent_adp_participants_accept_environment_variables_and_os_overlays_only() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = PARTICIPANT_FIELDS.replace(
            "subject: true",
            "subject: true, config: {env: {ANY_KEY: arbitrary}, os: {linux: {env: {ANY_KEY: linux}}}}",
        );
        let path = write_case(dir.path(), "alpha", &yaml);
        let cases = crate::CASE_LOADER.load_file(&path).unwrap();
        assert_eq!(cases.len(), 1);
        let CaseSchema::Participants { file, .. } = cases[0].schema() else {
            panic!("expected participant data");
        };
        let config = &file.participants["adp"].config;
        assert_eq!(config.env["ANY_KEY"], "arbitrary");
        assert_eq!(config.os[&Os::Linux].env["ANY_KEY"], "linux");

        for (fields, expected) in [
            ("config: {files: {settings.yaml: missing.yaml}}", "it declares none"),
            ("config: {endpoints: {api: {port: 80}}}", "it serves none"),
            ("config: {params: {mode: fast}}", "it accepts none"),
            ("connect: {upstream: other.api}", "it consumes none"),
            (
                "config: {os: {linux: {files: {settings.yaml: missing.yaml}}}}",
                "it declares none",
            ),
            ("config: {os: {linux: {params: {mode: fast}}}}", "it accepts none"),
        ] {
            let yaml = PARTICIPANT_FIELDS.replace("subject: true", &format!("subject: true, {fields}"));
            let path = write_case(dir.path(), "alpha", &yaml);
            let error = crate::CASE_LOADER
                .load_file(&path)
                .err()
                .expect("undeclared names must fail");
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("case 'alpha' in {}: ", path.canonicalize().unwrap().display())),
                "{message}"
            );
            assert!(message.contains("which type 'agent-adp' does not"), "{message}");
            assert!(message.ends_with(expected), "{message}");
        }
    }
}
