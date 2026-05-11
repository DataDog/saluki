//! Annotations for keys consumed via `get_typed` / `try_get_typed` rather than struct
//! deserialization.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `syslog_rfc` — use RFC 5424 syslog format when syslog logging is enabled.
    SYSLOG_RFC = SalukiAnnotation {
        schema: &schema::SYSLOG_RFC,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::GET_TYPED],
        value_type_override: None,
        test_json: None,
    };

    /// `syslog_uri` — destination URI for syslog output.
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
