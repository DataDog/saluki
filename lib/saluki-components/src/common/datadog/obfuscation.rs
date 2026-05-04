//! Obfuscation configuration types.

use facet::Facet;
use saluki_config::deserialize_space_separated_or_seq;
use serde::Deserialize;

/// Configuration for the obfuscator.
///
/// Sub-struct fields use `#[serde(flatten)]` so that serde reads directly from the flat
/// `apm_obfuscation_*` keys produced by `DD_APM_OBFUSCATION_*` env vars. `KEY_ALIASES` in
/// `crate::config` bridges the nested YAML paths (e.g. `apm_config.obfuscation.credit_cards.enabled`)
/// to these same flat keys so both sources are handled identically.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct ObfuscationConfig {
    /// Credit card obfuscation settings.
    #[serde(flatten)]
    pub(crate) credit_cards: CreditCardObfuscationConfig,

    /// HTTP URL obfuscation settings.
    #[serde(flatten)]
    pub(crate) http: HttpObfuscationConfig,

    /// Memcached obfuscation settings.
    #[serde(flatten)]
    pub(crate) memcached: MemcachedObfuscationConfig,

    /// Redis obfuscation settings.
    #[serde(flatten)]
    pub(crate) redis: RedisObfuscationConfig,

    /// Valkey obfuscation settings.
    #[serde(flatten)]
    pub(crate) valkey: ValkeyObfuscationConfig,

    /// SQL obfuscation settings.
    #[serde(flatten)]
    pub(crate) sql: SqlObfuscationConfig,

    /// MongoDB obfuscation settings.
    #[serde(flatten)]
    pub(crate) mongo: MongoObfuscationConfig,

    /// Elasticsearch obfuscation settings.
    #[serde(flatten)]
    pub(crate) es: EsObfuscationConfig,

    /// OpenSearch obfuscation settings.
    #[serde(flatten)]
    pub(crate) open_search: OpenSearchObfuscationConfig,
}

/// HTTP URL obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct HttpObfuscationConfig {
    /// Whether to remove query strings from HTTP URLs.
    #[serde(default, rename = "apm_obfuscation_http_remove_query_string")]
    pub(crate) remove_query_string: bool,

    /// Whether to obfuscate path segments containing digits.
    #[serde(default, rename = "apm_obfuscation_http_remove_paths_with_digits")]
    pub(crate) remove_path_digits: bool,
}

/// Memcached obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct MemcachedObfuscationConfig {
    /// Whether memcached obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_memcached_enabled")]
    pub(crate) enabled: bool,

    /// Whether to keep the command (if false, entire tag is removed).
    #[serde(default, rename = "apm_obfuscation_memcached_keep_command")]
    pub(crate) keep_command: bool,
}

/// Credit card obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct CreditCardObfuscationConfig {
    /// Whether credit card obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_credit_cards_enabled")]
    pub(crate) enabled: bool,

    /// Whether to use Luhn checksum validation (reduces false positives, increases CPU cost).
    #[serde(default, rename = "apm_obfuscation_credit_cards_luhn")]
    pub(crate) luhn: bool,

    /// Tag keys that are known to not contain credit cards and can be kept.
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_credit_cards_keep_values"
    )]
    pub(crate) keep_values: Vec<String>,
}

/// Redis obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct RedisObfuscationConfig {
    /// Whether Redis obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_redis_enabled")]
    pub(crate) enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    #[serde(default, rename = "apm_obfuscation_redis_remove_all_args")]
    pub(crate) remove_all_args: bool,
}

/// Valkey obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct ValkeyObfuscationConfig {
    /// Whether Valkey obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_valkey_enabled")]
    pub(crate) enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    #[serde(default, rename = "apm_obfuscation_valkey_remove_all_args")]
    pub(crate) remove_all_args: bool,
}

/// SQL obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct SqlObfuscationConfig {
    /// DBMS type (for example, "postgresql", "mysql", "mssql", "sqlite").
    #[serde(default, rename = "apm_obfuscation_sql_dbms")]
    pub(crate) dbms: String,

    /// Whether to extract table names.
    #[serde(default, rename = "apm_obfuscation_sql_table_names")]
    pub(crate) table_names: bool,

    /// Whether to replace digits in table names and identifiers.
    #[serde(default, rename = "apm_obfuscation_sql_replace_digits")]
    pub(crate) replace_digits: bool,

    /// Whether to keep SQL aliases (AS keyword) or truncate them.
    #[serde(default, rename = "apm_obfuscation_sql_keep_sql_alias")]
    pub(crate) keep_sql_alias: bool,

    /// Whether to treat "$func$" dollar-quoted strings specially (PostgreSQL).
    #[serde(default, rename = "apm_obfuscation_sql_dollar_quoted_func")]
    pub(crate) dollar_quoted_func: bool,
}

impl SqlObfuscationConfig {
    /// Returns a clone with the specified DBMS.
    pub fn with_dbms(&self, dbms: String) -> Self {
        let mut clone = self.clone();
        clone.dbms = dbms;
        clone
    }

    /// Returns a clone with dollar_quoted_func disabled.
    /// Used for recursive obfuscation to avoid infinite loops.
    pub fn with_dollar_quoted_func_disabled(&self) -> Self {
        let mut clone = self.clone();
        clone.dollar_quoted_func = false;
        clone
    }
}

/// Elasticsearch obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct EsObfuscationConfig {
    /// Whether Elasticsearch obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_elasticsearch_enabled")]
    pub(crate) enabled: bool,

    /// Keys whose values should not be obfuscated.
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_elasticsearch_keep_values"
    )]
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with "?".
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_elasticsearch_obfuscate_sql_values"
    )]
    pub(crate) obfuscate_sql_values: Vec<String>,
}

/// MongoDB obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct MongoObfuscationConfig {
    /// Whether MongoDB obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_mongodb_enabled")]
    pub(crate) enabled: bool,

    /// Keys whose values should not be obfuscated.
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_mongodb_keep_values"
    )]
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with "?".
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_mongodb_obfuscate_sql_values"
    )]
    pub(crate) obfuscate_sql_values: Vec<String>,
}

/// OpenSearch obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct OpenSearchObfuscationConfig {
    /// Whether OpenSearch obfuscation is enabled.
    #[serde(default, rename = "apm_obfuscation_opensearch_enabled")]
    pub(crate) enabled: bool,

    /// Keys whose values should not be obfuscated.
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_opensearch_keep_values"
    )]
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with "?".
    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_opensearch_obfuscate_sql_values"
    )]
    pub(crate) obfuscate_sql_values: Vec<String>,
}
