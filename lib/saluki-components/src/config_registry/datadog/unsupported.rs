//! Annotations for configuration keys that Saluki does not support.
use crate::config_registry::{generated::schema, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `dogstatsd_so_rcvbuf` - socket receive buffer size.
    DOGSTATSD_SO_RCVBUF = SalukiAnnotation {
        schema: &schema::DOGSTATSD_SO_RCVBUF,
        support_level: SupportLevel::Incompatible,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[],
        value_type_override: None,
        test_json: None,
    };
}
