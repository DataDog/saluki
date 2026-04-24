use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `proxy.http` → `ProxyConfiguration::http_server`
    PROXY_HTTP = SalukiAnnotation {
        schema: &schema::PROXY_HTTP,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_PROXY_HTTP", "HTTP_PROXY"]),
        used_by: &[structs::FORWARDER_CONFIGURATION, structs::PROXY_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `proxy.https` → `ProxyConfiguration::https_server`
    PROXY_HTTPS = SalukiAnnotation {
        schema: &schema::PROXY_HTTPS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_PROXY_HTTPS", "HTTPS_PROXY"]),
        used_by: &[structs::FORWARDER_CONFIGURATION, structs::PROXY_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `proxy.no_proxy` → `ProxyConfiguration::no_proxy`
    PROXY_NO_PROXY = SalukiAnnotation {
        schema: &schema::PROXY_NO_PROXY,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_PROXY_NO_PROXY"]),
        used_by: &[structs::FORWARDER_CONFIGURATION, structs::PROXY_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `no_proxy_nonexact_match` → `ProxyConfiguration::no_proxy_nonexact_match`
    NO_PROXY_NONEXACT_MATCH = SalukiAnnotation {
        schema: &schema::NO_PROXY_NONEXACT_MATCH,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_NO_PROXY_NONEXACT_MATCH"]),
        used_by: &[structs::FORWARDER_CONFIGURATION, structs::PROXY_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `use_proxy_for_cloud_metadata` → `ProxyConfiguration::use_proxy_for_cloud_metadata`
    USE_PROXY_FOR_CLOUD_METADATA = SalukiAnnotation {
        schema: &schema::USE_PROXY_FOR_CLOUD_METADATA,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: Some(&["DD_USE_PROXY_FOR_CLOUD_METADATA"]),
        used_by: &[structs::FORWARDER_CONFIGURATION, structs::PROXY_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
