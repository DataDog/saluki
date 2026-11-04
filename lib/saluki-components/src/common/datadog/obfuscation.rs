//! Obfuscation configuration types.

/// Configuration for the obfuscator.
///
/// This mirrors the Datadog Agent's `apm_config.obfuscation` section: each field is one of its
/// subsections, named exactly as the Agent names it.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct ObfuscationConfig {
    /// Credit card obfuscation settings.
    pub credit_cards: CreditCardObfuscationConfig,

    /// HTTP URL obfuscation settings.
    pub http: HttpObfuscationConfig,

    /// Memcached obfuscation settings.
    pub memcached: MemcachedObfuscationConfig,

    /// Redis obfuscation settings.
    pub redis: RedisObfuscationConfig,

    /// Valkey obfuscation settings.
    pub valkey: ValkeyObfuscationConfig,

    /// SQL obfuscation settings.
    pub sql: SqlObfuscationConfig,

    /// MongoDB obfuscation settings.
    pub mongodb: MongoObfuscationConfig,

    /// Elasticsearch obfuscation settings.
    pub elasticsearch: EsObfuscationConfig,

    /// OpenSearch obfuscation settings.
    pub opensearch: OpenSearchObfuscationConfig,
}

/// HTTP URL obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct HttpObfuscationConfig {
    /// Whether to remove query strings from HTTP URLs.
    pub remove_query_string: bool,

    /// Whether to obfuscate path segments containing digits.
    pub remove_paths_with_digits: bool,
}

/// Memcached obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct MemcachedObfuscationConfig {
    /// Whether memcached obfuscation is enabled.
    pub enabled: bool,

    /// Whether to keep the command (if false, entire tag is removed).
    pub keep_command: bool,
}

/// Credit card obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct CreditCardObfuscationConfig {
    /// Whether credit card obfuscation is enabled.
    pub enabled: bool,

    /// Whether to use Luhn checksum validation (reduces false positives, increases CPU cost).
    pub luhn: bool,

    /// Tag keys that are known to not contain credit cards and can be kept.
    pub keep_values: Vec<String>,
}

/// Redis obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct RedisObfuscationConfig {
    /// Whether Redis obfuscation is enabled.
    pub enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    pub remove_all_args: bool,
}

/// Valkey obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct ValkeyObfuscationConfig {
    /// Whether Valkey obfuscation is enabled.
    pub enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    pub remove_all_args: bool,
}

/// SQL obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct SqlObfuscationConfig {
    /// DBMS type (for example, `postgresql`, `mysql`, `mssql`, `sqlite`).
    pub dbms: String,

    /// Whether to extract table names.
    pub table_names: bool,

    /// Whether to replace digits in table names and identifiers.
    pub replace_digits: bool,

    /// Whether to keep SQL aliases (AS keyword) or truncate them.
    pub keep_sql_alias: bool,

    /// Whether to treat "$func$" dollar-quoted strings specially (PostgreSQL).
    pub dollar_quoted_func: bool,
}

impl SqlObfuscationConfig {
    /// Returns a clone with the specified DBMS.
    pub(crate) fn with_dbms(&self, dbms: String) -> Self {
        let mut clone = self.clone();
        clone.dbms = dbms;
        clone
    }

    /// Returns a clone with `dollar_quoted_func` disabled.
    /// Used for recursive obfuscation to avoid infinite loops.
    pub(crate) fn with_dollar_quoted_func_disabled(&self) -> Self {
        let mut clone = self.clone();
        clone.dollar_quoted_func = false;
        clone
    }
}

/// Elasticsearch obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct EsObfuscationConfig {
    /// Whether Elasticsearch obfuscation is enabled.
    pub enabled: bool,

    /// Keys whose values shouldn't be obfuscated.
    pub keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with `?`.
    pub obfuscate_sql_values: Vec<String>,
}

/// MongoDB obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct MongoObfuscationConfig {
    /// Whether MongoDB obfuscation is enabled.
    pub enabled: bool,

    /// Keys whose values shouldn't be obfuscated.
    pub keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with `?`.
    pub obfuscate_sql_values: Vec<String>,
}

/// OpenSearch obfuscation configuration.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default, PartialEq))]
pub struct OpenSearchObfuscationConfig {
    /// Whether OpenSearch obfuscation is enabled.
    pub enabled: bool,

    /// Keys whose values shouldn't be obfuscated.
    pub keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with `?`.
    pub obfuscate_sql_values: Vec<String>,
}
