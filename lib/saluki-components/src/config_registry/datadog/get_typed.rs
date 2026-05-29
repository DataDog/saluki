//! Annotations for keys consumed via `get_typed` / `try_get_typed` rather than struct
//! deserialization.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `cmd_port`—port for the Datadog Agent IPC/CMD API server.
    CMD_PORT = SalukiAnnotation {
        schema: &schema::CMD_PORT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::REMOTE_AGENT_CLIENT_CONFIGURATION],
        value_type_override: None,
        test_json: Some("5101"),
    };

    /// `log_format_rfc3339`—use RFC 3339 timestamp format in log output.
    LOG_FORMAT_RFC3339 = SalukiAnnotation {
        schema: &schema::LOG_FORMAT_RFC3339,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::GET_TYPED],
        value_type_override: None,
        test_json: None,
    };

    /// `syslog_rfc`—use RFC 5424 syslog format when syslog logging is enabled.
    SYSLOG_RFC = SalukiAnnotation {
        schema: &schema::SYSLOG_RFC,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::GET_TYPED],
        value_type_override: None,
        test_json: None,
    };

    /// `syslog_uri`—destination URI for syslog output.
    SYSLOG_URI = SalukiAnnotation {
        schema: &schema::SYSLOG_URI,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::GET_TYPED],
        value_type_override: None,
        test_json: None,
    };
}
