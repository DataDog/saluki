//! Annotations for DogStatsD prefix filter transform configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// ADP uses "blocklist" spelling; schema has legacy "blacklist".
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
