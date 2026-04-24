//! Annotations for DogStatsD mapper transform configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// ADP-specific keys not present in the vendored Agent schema.
static DOGSTATSD_MAPPER_STRING_INTERNER_SIZE_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "dogstatsd_mapper_string_interner_size",
    env_vars: &[],
    value_type: ValueType::Integer,
    default: None,
};

crate::declare_annotations! {
    /// `dogstatsd_mapper_profiles` — JSON-encoded list of DogStatsD metric mapping profiles.
    /// Uses a custom test value since the generic String test value is not valid mapper JSON.
    DOGSTATSD_MAPPER_PROFILES = SalukiAnnotation {
        schema: &schema::DOGSTATSD_MAPPER_PROFILES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_MAPPER_CONFIGURATION],
        value_type_override: None,
        test_json: Some(r#"[{"name":"test","prefix":"test.","mappings":[]}]"#),
    };

    /// `dogstatsd_mapper_string_interner_size` — interner byte budget for the mapper transform.
    /// ADP-specific key, not in the Agent schema.
    DOGSTATSD_MAPPER_STRING_INTERNER_SIZE = SalukiAnnotation {
        schema: &DOGSTATSD_MAPPER_STRING_INTERNER_SIZE_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_MAPPER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
