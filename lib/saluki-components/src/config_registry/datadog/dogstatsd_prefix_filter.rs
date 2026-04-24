//! Annotations for DogStatsD prefix filter transform configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// The Agent schema uses `statsd_metric_namespace_blacklist` (generated/schema.rs:
// STATSD_METRIC_NAMESPACE_BLACKLIST, yaml_path "statsd_metric_namespace_blacklist").
//
// ADP renamed the field to `statsd_metric_namespace_blocklist` for inclusive language, but the
// struct has NO serde alias for the old spelling. This means Agent config files that set
// `statsd_metric_namespace_blacklist` are silently ignored by ADP — the customized list is
// dropped and ADP falls back to the hardcoded default. The default lists happen to be identical
// today, so this only affects users who have customized the key in their Agent config.
//
// Fix: add `#[serde(alias = "statsd_metric_namespace_blacklist")]` to the struct field in
// `transforms/dogstatsd_prefix_filter/mod.rs` and switch this static to reference the generated
// STATSD_METRIC_NAMESPACE_BLACKLIST schema entry with the old key as additional_yaml_paths.
// TODO: https://github.com/DataDog/saluki/issues — add serde alias for blacklist → blocklist
static STATSD_METRIC_NAMESPACE_BLOCKLIST_SCHEMA: SchemaEntry = SchemaEntry {
    yaml_path: "statsd_metric_namespace_blocklist",
    env_vars: &[],
    value_type: ValueType::StringList,
    default: None,
};

crate::declare_annotations! {
    /// `metric_filterlist` — explicit list of metric names to allow through the filter.
    METRIC_FILTERLIST = SalukiAnnotation {
        schema: &schema::METRIC_FILTERLIST,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `metric_filterlist_match_prefix` — whether filterlist entries match as prefixes.
    METRIC_FILTERLIST_MATCH_PREFIX = SalukiAnnotation {
        schema: &schema::METRIC_FILTERLIST_MATCH_PREFIX,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `statsd_metric_blocklist` — metric names to block.
    STATSD_METRIC_BLOCKLIST = SalukiAnnotation {
        schema: &schema::STATSD_METRIC_BLOCKLIST,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `statsd_metric_blocklist_match_prefix` — whether blocklist entries match as prefixes.
    STATSD_METRIC_BLOCKLIST_MATCH_PREFIX = SalukiAnnotation {
        schema: &schema::STATSD_METRIC_BLOCKLIST_MATCH_PREFIX,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `statsd_metric_namespace` — prefix to prepend to every metric name.
    STATSD_METRIC_NAMESPACE = SalukiAnnotation {
        schema: &schema::STATSD_METRIC_NAMESPACE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `statsd_metric_namespace_blocklist` — namespace prefixes to block from forwarding.
    /// ADP uses "blocklist" spelling; the Agent schema uses "blacklist".
    STATSD_METRIC_NAMESPACE_BLOCKLIST = SalukiAnnotation {
        schema: &STATSD_METRIC_NAMESPACE_BLOCKLIST_SCHEMA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::DOGSTATSD_PREFIX_FILTER_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
