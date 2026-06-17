use figment::{
    providers::Serialized,
    value::{Dict, Map},
    Error, Metadata, Profile, Provider,
};

/// Key aliases for Datadog source normalization.
pub const KEY_ALIASES: &[(&str, &str)] = &[
    ("proxy.http", "proxy_http"),
    ("proxy.https", "proxy_https"),
    ("proxy.no_proxy", "proxy_no_proxy"),
    ("apm_config.enable_rare_sampler", "apm_enable_rare_sampler"),
    (
        "apm_config.error_tracking_standalone.enabled",
        "apm_error_tracking_standalone_enabled",
    ),
    (
        "apm_config.obfuscation.credit_cards.enabled",
        "apm_obfuscation_credit_cards_enabled",
    ),
    (
        "apm_config.obfuscation.credit_cards.keep_values",
        "apm_obfuscation_credit_cards_keep_values",
    ),
    (
        "apm_config.obfuscation.credit_cards.luhn",
        "apm_obfuscation_credit_cards_luhn",
    ),
    (
        "apm_config.obfuscation.elasticsearch.enabled",
        "apm_obfuscation_elasticsearch_enabled",
    ),
    (
        "apm_config.obfuscation.elasticsearch.keep_values",
        "apm_obfuscation_elasticsearch_keep_values",
    ),
    (
        "apm_config.obfuscation.elasticsearch.obfuscate_sql_values",
        "apm_obfuscation_elasticsearch_obfuscate_sql_values",
    ),
    (
        "apm_config.obfuscation.http.remove_paths_with_digits",
        "apm_obfuscation_http_remove_paths_with_digits",
    ),
    (
        "apm_config.obfuscation.http.remove_query_string",
        "apm_obfuscation_http_remove_query_string",
    ),
    (
        "apm_config.obfuscation.memcached.enabled",
        "apm_obfuscation_memcached_enabled",
    ),
    (
        "apm_config.obfuscation.memcached.keep_command",
        "apm_obfuscation_memcached_keep_command",
    ),
    (
        "apm_config.obfuscation.mongodb.enabled",
        "apm_obfuscation_mongodb_enabled",
    ),
    (
        "apm_config.obfuscation.mongodb.keep_values",
        "apm_obfuscation_mongodb_keep_values",
    ),
    (
        "apm_config.obfuscation.mongodb.obfuscate_sql_values",
        "apm_obfuscation_mongodb_obfuscate_sql_values",
    ),
    (
        "apm_config.obfuscation.opensearch.enabled",
        "apm_obfuscation_opensearch_enabled",
    ),
    (
        "apm_config.obfuscation.opensearch.keep_values",
        "apm_obfuscation_opensearch_keep_values",
    ),
    (
        "apm_config.obfuscation.opensearch.obfuscate_sql_values",
        "apm_obfuscation_opensearch_obfuscate_sql_values",
    ),
    ("apm_config.obfuscation.redis.enabled", "apm_obfuscation_redis_enabled"),
    (
        "apm_config.obfuscation.redis.remove_all_args",
        "apm_obfuscation_redis_remove_all_args",
    ),
    (
        "apm_config.obfuscation.valkey.enabled",
        "apm_obfuscation_valkey_enabled",
    ),
    (
        "apm_config.obfuscation.valkey.remove_all_args",
        "apm_obfuscation_valkey_remove_all_args",
    ),
    ("apm_config.obfuscation.sql.dbms", "apm_obfuscation_sql_dbms"),
    (
        "apm_config.obfuscation.sql.dollar_quoted_func",
        "apm_obfuscation_sql_dollar_quoted_func",
    ),
    (
        "apm_config.obfuscation.sql.keep_sql_alias",
        "apm_obfuscation_sql_keep_sql_alias",
    ),
    (
        "apm_config.obfuscation.sql.replace_digits",
        "apm_obfuscation_sql_replace_digits",
    ),
    (
        "apm_config.obfuscation.sql.table_names",
        "apm_obfuscation_sql_table_names",
    ),
    (
        "otlp_config.traces.probabilistic_sampler.sampling_percentage",
        "otlp_config_traces_probabilistic_sampler_sampling_percentage",
    ),
    (
        "observability_pipelines_worker.metrics.enabled",
        "observability_pipelines_worker_metrics_enabled",
    ),
    (
        "observability_pipelines_worker.metrics.url",
        "observability_pipelines_worker_metrics_url",
    ),
    ("vector.metrics.enabled", "vector_metrics_enabled"),
    ("vector.metrics.url", "vector_metrics_url"),
    ("agent_ipc.grpc_max_message_size", "agent_ipc_grpc_max_message_size"),
    ("use_v2_api.series", "use_v2_api_series"),
];

const ENV_REMAPPINGS: &[(&str, &str)] = &[("http_proxy", "proxy_http"), ("https_proxy", "proxy_https")];

/// Figment provider that remaps Datadog-recognized environment variable names to canonical config keys.
pub struct DatadogRemapper {
    values: serde_json::Map<String, serde_json::Value>,
}

impl DatadogRemapper {
    /// Constructs a remapper by snapshotting process environment variables.
    pub fn new() -> Self {
        let mut values = serde_json::Map::new();

        for (env_key, env_value) in std::env::vars() {
            let lower = env_key.to_lowercase();
            for &(from, to) in ENV_REMAPPINGS {
                if lower == from && !values.contains_key(to) {
                    values.insert(to.to_string(), serde_json::Value::String(env_value.clone()));
                }
            }
        }

        Self { values }
    }
}

impl Default for DatadogRemapper {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for DatadogRemapper {
    fn metadata(&self) -> Metadata {
        Metadata::named("Datadog config remapper")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        if self.values.is_empty() {
            return Ok(Map::new());
        }
        Serialized::defaults(serde_json::Value::Object(self.values.clone())).data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_var_remapped_case_insensitively() {
        let _guard = ENV_MUTEX.lock().unwrap();

        std::env::set_var("HTTP_PROXY", "http://proxy.example.com");
        let remapper = DatadogRemapper::new();
        std::env::remove_var("HTTP_PROXY");

        assert_eq!(
            remapper.values.get("proxy_http").and_then(|v| v.as_str()),
            Some("http://proxy.example.com"),
        );
    }

    #[test]
    fn env_var_not_remapped_when_absent() {
        let _guard = ENV_MUTEX.lock().unwrap();

        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("http_proxy");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("https_proxy");

        let remapper = DatadogRemapper::new();

        assert!(remapper.values.get("proxy_http").is_none());
        assert!(remapper.values.get("proxy_https").is_none());
    }
}
