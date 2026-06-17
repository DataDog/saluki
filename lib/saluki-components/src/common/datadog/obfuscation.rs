//! Obfuscation configuration types.
//!
//! These runtime types mirror their leaf counterparts in
//! `saluki_component_config::traces` and are built from them via `from_native`. They carry no
//! source-language serde attributes; source-to-native mapping is owned by the configuration system.

use saluki_component_config::traces as leaf;

/// Configuration for the obfuscator.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ObfuscationConfig {
    /// Credit card obfuscation settings.
    pub(crate) credit_cards: CreditCardObfuscationConfig,

    /// HTTP URL obfuscation settings.
    pub(crate) http: HttpObfuscationConfig,

    /// Memcached obfuscation settings.
    pub(crate) memcached: MemcachedObfuscationConfig,

    /// Redis obfuscation settings.
    pub(crate) redis: RedisObfuscationConfig,

    /// Valkey obfuscation settings.
    pub(crate) valkey: ValkeyObfuscationConfig,

    /// SQL obfuscation settings.
    pub(crate) sql: SqlObfuscationConfig,

    /// MongoDB obfuscation settings.
    pub(crate) mongo: MongoObfuscationConfig,

    /// Elasticsearch obfuscation settings.
    pub(crate) es: EsObfuscationConfig,

    /// OpenSearch obfuscation settings.
    pub(crate) open_search: OpenSearchObfuscationConfig,
}

impl ObfuscationConfig {
    /// Builds the runtime obfuscation configuration from its leaf mirror.
    pub fn from_native(cfg: &leaf::ObfuscationConfig) -> Self {
        Self {
            credit_cards: CreditCardObfuscationConfig::from_native(&cfg.credit_cards),
            http: HttpObfuscationConfig::from_native(&cfg.http),
            memcached: MemcachedObfuscationConfig::from_native(&cfg.memcached),
            redis: RedisObfuscationConfig::from_native(&cfg.redis),
            valkey: ValkeyObfuscationConfig::from_native(&cfg.valkey),
            sql: SqlObfuscationConfig::from_native(&cfg.sql),
            mongo: MongoObfuscationConfig::from_native(&cfg.mongo),
            es: EsObfuscationConfig::from_native(&cfg.es),
            open_search: OpenSearchObfuscationConfig::from_native(&cfg.open_search),
        }
    }
}

/// HTTP URL obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct HttpObfuscationConfig {
    /// Whether to remove query strings from HTTP URLs.
    pub(crate) remove_query_string: bool,

    /// Whether to obfuscate path segments containing digits.
    pub(crate) remove_path_digits: bool,
}

impl HttpObfuscationConfig {
    fn from_native(cfg: &leaf::HttpObfuscationConfig) -> Self {
        Self {
            remove_query_string: cfg.remove_query_string,
            remove_path_digits: cfg.remove_path_digits,
        }
    }
}

/// Memcached obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct MemcachedObfuscationConfig {
    /// Whether memcached obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to keep the command (if false, entire tag is removed).
    pub(crate) keep_command: bool,
}

impl MemcachedObfuscationConfig {
    fn from_native(cfg: &leaf::MemcachedObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            keep_command: cfg.keep_command,
        }
    }
}

/// Credit card obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct CreditCardObfuscationConfig {
    /// Whether credit card obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to use Luhn checksum validation (reduces false positives, increases CPU cost).
    pub(crate) luhn: bool,

    /// Tag keys that are known to not contain credit cards and can be kept.
    pub(crate) keep_values: Vec<String>,
}

impl CreditCardObfuscationConfig {
    fn from_native(cfg: &leaf::CreditCardObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            luhn: cfg.luhn,
            keep_values: cfg.keep_values.clone(),
        }
    }
}

/// Redis obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct RedisObfuscationConfig {
    /// Whether Redis obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    pub(crate) remove_all_args: bool,
}

impl RedisObfuscationConfig {
    fn from_native(cfg: &leaf::RedisObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            remove_all_args: cfg.remove_all_args,
        }
    }
}

/// Valkey obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ValkeyObfuscationConfig {
    /// Whether Valkey obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Whether to remove all arguments (nuclear option).
    pub(crate) remove_all_args: bool,
}

impl ValkeyObfuscationConfig {
    fn from_native(cfg: &leaf::ValkeyObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            remove_all_args: cfg.remove_all_args,
        }
    }
}

/// SQL obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct SqlObfuscationConfig {
    /// DBMS type (for example, "postgresql", "mysql", "mssql", "sqlite").
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

impl SqlObfuscationConfig {
    fn from_native(cfg: &leaf::SqlObfuscationConfig) -> Self {
        Self {
            dbms: cfg.dbms.clone(),
            table_names: cfg.table_names,
            replace_digits: cfg.replace_digits,
            keep_sql_alias: cfg.keep_sql_alias,
            dollar_quoted_func: cfg.dollar_quoted_func,
        }
    }
}

impl SqlObfuscationConfig {
    /// Returns a clone with the specified DBMS.
    pub fn with_dbms(&self, dbms: String) -> Self {
        let mut clone = self.clone();
        clone.dbms = dbms;
        clone
    }

    /// Returns a clone with `dollar_quoted_func` disabled.
    /// Used for recursive obfuscation to avoid infinite loops.
    pub fn with_dollar_quoted_func_disabled(&self) -> Self {
        let mut clone = self.clone();
        clone.dollar_quoted_func = false;
        clone
    }
}

/// Elasticsearch obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct EsObfuscationConfig {
    /// Whether Elasticsearch obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Keys whose values shouldn't be obfuscated.
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with `?`.
    pub(crate) obfuscate_sql_values: Vec<String>,
}

impl EsObfuscationConfig {
    fn from_native(cfg: &leaf::EsObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            keep_values: cfg.keep_values.clone(),
            obfuscate_sql_values: cfg.obfuscate_sql_values.clone(),
        }
    }
}

/// MongoDB obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct MongoObfuscationConfig {
    /// Whether MongoDB obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Keys whose values shouldn't be obfuscated.
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with `?`.
    pub(crate) obfuscate_sql_values: Vec<String>,
}

impl MongoObfuscationConfig {
    fn from_native(cfg: &leaf::MongoObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            keep_values: cfg.keep_values.clone(),
            obfuscate_sql_values: cfg.obfuscate_sql_values.clone(),
        }
    }
}

/// OpenSearch obfuscation configuration.
#[derive(Clone, Debug, Default)]
#[cfg_attr(test, derive(PartialEq))]
pub struct OpenSearchObfuscationConfig {
    /// Whether OpenSearch obfuscation is enabled.
    pub(crate) enabled: bool,

    /// Keys whose values shouldn't be obfuscated.
    pub(crate) keep_values: Vec<String>,

    /// Keys whose string values should be SQL-obfuscated instead of replaced with `?`.
    pub(crate) obfuscate_sql_values: Vec<String>,
}

impl OpenSearchObfuscationConfig {
    fn from_native(cfg: &leaf::OpenSearchObfuscationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            keep_values: cfg.keep_values.clone(),
            obfuscate_sql_values: cfg.obfuscate_sql_values.clone(),
        }
    }
}
