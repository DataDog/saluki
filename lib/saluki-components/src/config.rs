//! Datadog-specific configuration providers and remappers.
use figment::{
    providers::Serialized,
    value::{Dict, Map},
    Error, Metadata, Profile, Provider,
};

/// Key aliases to pass to [`ConfigurationLoader::with_key_aliases`][saluki_config::ConfigurationLoader::with_key_aliases].
///
/// Each entry maps a nested dot-separated path to a flat key name. When the nested path is found in a loaded
/// config file, its value is also emitted under the flat key — but only if the flat key is not already
/// explicitly set. This ensures both YAML nested format and flat env var format produce the same Figment key,
/// so source precedence (env vars > file) works correctly.
pub const KEY_ALIASES: &[(&str, &str)] = &[
    // The Datadog Agent config file uses `proxy: http:` and `proxy: https:` (nested), while env
    // vars produce `proxy_http` and `proxy_https` (flat). Figment treats these as different keys,
    // so without this alias env var precedence over YAML is silently broken for proxy config.
    ("proxy.http", "proxy_http"),
    ("proxy.https", "proxy_https"),
    ("proxy.no_proxy", "proxy_no_proxy"),
    ("apm_config.enable_rare_sampler", "apm_enable_rare_sampler"),
    (
        "apm_config.error_tracking_standalone.enabled",
        "apm_error_tracking_standalone_enabled",
    ),
    // Obfuscation keys live at `apm_config.obfuscation.*` in YAML but the Agent's env vars use
    // `DD_APM_OBFUSCATION_*` (no `_CONFIG_` segment), producing flat keys. These aliases emit the
    // flat key when the nested YAML path is present so that both sources land on the same Figment
    // key and env var precedence over file config works correctly.
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
];

/// Remappings from environment variable names to canonical config keys.
///
/// Matching is case-insensitive.
const ENV_REMAPPINGS: &[(&str, &str)] = &[("http_proxy", "proxy_http"), ("https_proxy", "proxy_https")];

/// A Figment provider that remaps canonical environment variable names to our desired config keys.
///
/// Reads environment variables case-insensitively and maps them to config keys (e.g. `HTTP_PROXY` →
/// `proxy_http`). Values are snapshotted at construction time.
///
/// Add this provider to a [`ConfigurationLoader`][saluki_config::ConfigurationLoader] *after* file-based
/// providers and *before* vendor-prefixed env providers (e.g. `DD_`) to achieve the correct precedence:
/// file < remapped env vars < `DD_`-prefixed.
///
/// For YAML key aliasing (e.g. `proxy.http` → `proxy_http`), pass [`KEY_ALIASES`] to
/// [`ConfigurationLoader::with_key_aliases`][saluki_config::ConfigurationLoader::with_key_aliases] instead —
/// that is handled at file-load time.
pub struct DatadogRemapper {
    values: serde_json::Map<String, serde_json::Value>,
}

impl DatadogRemapper {
    /// Constructs a `DatadogRemapper` by eagerly snapshotting env var remappings.
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
