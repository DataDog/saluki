//! Obfuscation configuration types.

use serde::Deserialize;

/// Configuration for the obfuscator.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ObfuscationConfig {
    /// HTTP URL obfuscation settings.
    http: HttpObfuscationConfig,

    /// Memcached obfuscation settings.
    memcached: MemcachedObfuscationConfig,

    /// Credit card obfuscation settings.
    credit_cards: CreditCardObfuscationConfig,

    /// Redis obfuscation settings.
    redis: RedisObfuscationConfig,

    /// Valkey obfuscation settings.
    valkey: ValkeyObfuscationConfig,

    /// SQL obfuscation settings.
    sql: SqlObfuscationConfig,

    /// MongoDB obfuscation settings.
    #[serde(alias = "mongodb")]
    mongo: JsonObfuscationConfig,

    /// Elasticsearch obfuscation settings.
    #[serde(alias = "elasticsearch")]
    es: JsonObfuscationConfig,

    /// OpenSearch obfuscation settings.
    #[serde(alias = "opensearch")]
    open_search: JsonObfuscationConfig,
}

/// HTTP URL obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct HttpObfuscationConfig {
    /// Whether to remove query strings from HTTP URLs.
    pub(crate) remove_query_string: bool,

    /// Whether to obfuscate path segments containing digits.
    #[serde(alias = "remove_paths_with_digits")]
    pub(crate) remove_path_digits: bool,
}

/// Memcached obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MemcachedObfuscationConfig {
    /// Whether memcached obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to keep the command (if false, entire tag is removed).
    pub(crate) keep_command: bool,
}

/// Credit card obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreditCardObfuscationConfig {
    /// Whether credit card obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to use Luhn checksum validation (reduces false positives, increases CPU cost).
    pub(crate) luhn: bool,

    /// Tag keys that are known to not contain credit cards and can be kept.
    #[serde(default)]
    pub(crate) keep_values: Vec<String>,
}

/// Redis obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RedisObfuscationConfig {
    /// Whether Redis obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    pub(crate) remove_all_args: bool,
}

/// Valkey obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ValkeyObfuscationConfig {
    /// Whether Valkey obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    pub(crate) remove_all_args: bool,
}

/// SQL obfuscation configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SqlObfuscationConfig {
    /// DBMS type (e.g., "postgresql", "mysql", "mssql", "sqlite").
    #[serde(default)]
    pub(crate) dbms: String,

    /// Whether to extract table names.
    pub(crate) table_names: bool,

    /// Whether to replace digits in table names and identifiers.
    pub(crate) replace_digits: bool,

    /// Whether to keep SQL aliases (AS keyword) or truncate them.
    pub(crate) keep_sql_alias: bool,

    /// Whether to treat "$func$" dollar-quoted strings specially (PostgreSQL).
    pub(crate) dollar_quoted_func: bool,
}

/// JSON obfuscation configuration for MongoDB, Elasticsearch, and OpenSearch.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct JsonObfuscationConfig {
    /// Whether JSON obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Keys whose values should not be obfuscated.
    #[serde(default)]
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with "?".
    #[serde(default)]
    pub(crate) obfuscate_sql_values: Vec<String>,
}

impl ObfuscationConfig {
    pub fn http(&self) -> &HttpObfuscationConfig {
        &self.http
    }

    pub fn set_http(&mut self, http: HttpObfuscationConfig) {
        self.http = http;
    }

    pub fn memcached(&self) -> &MemcachedObfuscationConfig {
        &self.memcached
    }

    pub fn credit_cards(&self) -> &CreditCardObfuscationConfig {
        &self.credit_cards
    }

    pub fn redis(&self) -> &RedisObfuscationConfig {
        &self.redis
    }

    pub fn valkey(&self) -> &ValkeyObfuscationConfig {
        &self.valkey
    }

    pub fn sql(&self) -> &SqlObfuscationConfig {
        &self.sql
    }

    pub fn mongo(&self) -> &JsonObfuscationConfig {
        &self.mongo
    }

    pub fn es(&self) -> &JsonObfuscationConfig {
        &self.es
    }

    pub fn open_search(&self) -> &JsonObfuscationConfig {
        &self.open_search
    }
}

impl HttpObfuscationConfig {
    pub fn remove_query_string(&self) -> bool {
        self.remove_query_string
    }

    pub fn remove_path_digits(&self) -> bool {
        self.remove_path_digits
    }
}

impl MemcachedObfuscationConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn keep_command(&self) -> bool {
        self.keep_command
    }
}

impl CreditCardObfuscationConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn luhn(&self) -> bool {
        self.luhn
    }

    pub fn keep_values(&self) -> &[String] {
        &self.keep_values
    }
}

impl RedisObfuscationConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn remove_all_args(&self) -> bool {
        self.remove_all_args
    }
}

impl ValkeyObfuscationConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn remove_all_args(&self) -> bool {
        self.remove_all_args
    }
}

impl SqlObfuscationConfig {
    pub fn dbms(&self) -> &str {
        &self.dbms
    }

    pub fn table_names(&self) -> bool {
        self.table_names
    }

    pub fn replace_digits(&self) -> bool {
        self.replace_digits
    }

    pub fn keep_sql_alias(&self) -> bool {
        self.keep_sql_alias
    }

    pub fn dollar_quoted_func(&self) -> bool {
        self.dollar_quoted_func
    }

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

impl JsonObfuscationConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn keep_values(&self) -> &[String] {
        &self.keep_values
    }

    pub fn obfuscate_sql_values(&self) -> &[String] {
        &self.obfuscate_sql_values
    }
}
