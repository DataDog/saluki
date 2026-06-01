//! Annotations for multi-region failover configuration keys.

use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SupportLevel, ValueType};

crate::declare_annotations! {
    /// `multi_region_failover.api_key` - API key for the MRF failover region.
    MULTI_REGION_FAILOVER_API_KEY = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_API_KEY,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_MULTI_REGION_FAILOVER_API_KEY"]),
        used_by: &[structs::MRF_CONFIGURATION],
        value_type_override: Some(ValueType::String),
        test_json: None,
    };

    /// `multi_region_failover.dd_url` - intake URL for the MRF failover region.
    MULTI_REGION_FAILOVER_DD_URL = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_DD_URL,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_MULTI_REGION_FAILOVER_DD_URL"]),
        used_by: &[structs::MRF_CONFIGURATION],
        value_type_override: Some(ValueType::String),
        test_json: None,
    };

    /// `multi_region_failover.enabled` - multi-region failover mode.
    MULTI_REGION_FAILOVER_ENABLED = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_MULTI_REGION_FAILOVER_ENABLED"]),
        used_by: &[structs::MRF_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `multi_region_failover.failover_metrics` - metrics forwarding to the failover region.
    MULTI_REGION_FAILOVER_FAILOVER_METRICS = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_FAILOVER_METRICS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_MULTI_REGION_FAILOVER_FAILOVER_METRICS"]),
        used_by: &[structs::MRF_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `multi_region_failover.metric_allowlist` - metric name allowlist for MRF forwarding.
    MULTI_REGION_FAILOVER_METRIC_ALLOWLIST = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_METRIC_ALLOWLIST,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_MULTI_REGION_FAILOVER_METRIC_ALLOWLIST"]),
        used_by: &[structs::MRF_CONFIGURATION],
        value_type_override: Some(ValueType::StringList),
        test_json: None,
    };

    /// `multi_region_failover.site` - Datadog site for the MRF failover region.
    MULTI_REGION_FAILOVER_SITE = SalukiAnnotation {
        schema: &schema::MULTI_REGION_FAILOVER_SITE,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_MULTI_REGION_FAILOVER_SITE"]),
        used_by: &[structs::MRF_CONFIGURATION],
        value_type_override: Some(ValueType::String),
        test_json: None,
    };
}
