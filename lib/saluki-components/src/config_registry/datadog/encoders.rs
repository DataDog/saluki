//! Annotations for shared Datadog encoder configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SupportLevel, ValueType};

crate::declare_annotations! {
    /// `serializer_compressor_kind` — compression algorithm for encoder request payloads.
    SERIALIZER_COMPRESSOR_KIND = SalukiAnnotation {
        schema: &schema::SERIALIZER_COMPRESSOR_KIND,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_EVENTS_CONFIGURATION],
        value_type_override: None,
    };

    /// `serializer_zstd_compressor_level` — zstd compression level for encoder request payloads.
    /// Schema says Float but the Rust field is i32, so we override the test value type.
    SERIALIZER_ZSTD_COMPRESSOR_LEVEL = SalukiAnnotation {
        schema: &schema::SERIALIZER_ZSTD_COMPRESSOR_LEVEL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DATADOG_EVENTS_CONFIGURATION],
        value_type_override: Some(ValueType::Integer),
    };
}
