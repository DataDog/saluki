//! Obfuscation configuration types.

use facet::Facet;
use saluki_config::deserialize_space_separated_or_seq;
use serde::Deserialize;

/// Configuration for the obfuscator.
///
/// Fields use flat `apm_obfuscation_*` serde renames. `KEY_ALIASES` in `crate::config` bridges
/// the nested YAML paths (e.g. `apm_config.obfuscation.credit_cards.enabled`) to these flat keys
/// so that both YAML config files and `DD_APM_OBFUSCATION_*` env vars are read correctly.
#[derive(Clone, Debug, Default, Deserialize, Facet)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct ObfuscationConfig {
    #[serde(default, rename = "apm_obfuscation_credit_cards_enabled")]
    pub(crate) credit_cards_enabled: bool,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_credit_cards_keep_values"
    )]
    pub(crate) credit_cards_keep_values: Vec<String>,

    #[serde(default, rename = "apm_obfuscation_credit_cards_luhn")]
    pub(crate) credit_cards_luhn: bool,

    #[serde(default, rename = "apm_obfuscation_elasticsearch_enabled")]
    pub(crate) es_enabled: bool,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_elasticsearch_keep_values"
    )]
    pub(crate) es_keep_values: Vec<String>,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_elasticsearch_obfuscate_sql_values"
    )]
    pub(crate) es_obfuscate_sql_values: Vec<String>,

    #[serde(default, rename = "apm_obfuscation_http_remove_paths_with_digits")]
    pub(crate) http_remove_path_digits: bool,

    #[serde(default, rename = "apm_obfuscation_http_remove_query_string")]
    pub(crate) http_remove_query_string: bool,

    #[serde(default, rename = "apm_obfuscation_memcached_enabled")]
    pub(crate) memcached_enabled: bool,

    #[serde(default, rename = "apm_obfuscation_memcached_keep_command")]
    pub(crate) memcached_keep_command: bool,

    #[serde(default, rename = "apm_obfuscation_mongodb_enabled")]
    pub(crate) mongo_enabled: bool,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_mongodb_keep_values"
    )]
    pub(crate) mongo_keep_values: Vec<String>,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_mongodb_obfuscate_sql_values"
    )]
    pub(crate) mongo_obfuscate_sql_values: Vec<String>,

    #[serde(default, rename = "apm_obfuscation_opensearch_enabled")]
    pub(crate) open_search_enabled: bool,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_opensearch_keep_values"
    )]
    pub(crate) open_search_keep_values: Vec<String>,

    #[serde(
        default,
        deserialize_with = "deserialize_space_separated_or_seq",
        rename = "apm_obfuscation_opensearch_obfuscate_sql_values"
    )]
    pub(crate) open_search_obfuscate_sql_values: Vec<String>,

    #[serde(default, rename = "apm_obfuscation_redis_enabled")]
    pub(crate) redis_enabled: bool,

    #[serde(default, rename = "apm_obfuscation_redis_remove_all_args")]
    pub(crate) redis_remove_all_args: bool,

    #[serde(default, rename = "apm_obfuscation_valkey_enabled")]
    pub(crate) valkey_enabled: bool,

    #[serde(default, rename = "apm_obfuscation_valkey_remove_all_args")]
    pub(crate) valkey_remove_all_args: bool,

    #[serde(default, rename = "apm_obfuscation_sql_dbms")]
    pub(crate) sql_dbms: String,

    #[serde(default, rename = "apm_obfuscation_sql_table_names")]
    pub(crate) sql_table_names: bool,

    #[serde(default, rename = "apm_obfuscation_sql_replace_digits")]
    pub(crate) sql_replace_digits: bool,

    #[serde(default, rename = "apm_obfuscation_sql_keep_sql_alias")]
    pub(crate) sql_keep_sql_alias: bool,

    #[serde(default, rename = "apm_obfuscation_sql_dollar_quoted_func")]
    pub(crate) sql_dollar_quoted_func: bool,
}

impl ObfuscationConfig {
    pub fn http(&self) -> HttpObfuscationConfig {
        HttpObfuscationConfig {
            remove_query_string: self.http_remove_query_string,
            remove_path_digits: self.http_remove_path_digits,
        }
    }

    pub fn memcached(&self) -> MemcachedObfuscationConfig {
        MemcachedObfuscationConfig {
            enabled: self.memcached_enabled,
            keep_command: self.memcached_keep_command,
        }
    }

    pub fn credit_cards(&self) -> CreditCardObfuscationConfig {
        CreditCardObfuscationConfig {
            enabled: self.credit_cards_enabled,
            luhn: self.credit_cards_luhn,
            keep_values: self.credit_cards_keep_values.clone(),
        }
    }

    pub fn redis(&self) -> RedisObfuscationConfig {
        RedisObfuscationConfig {
            enabled: self.redis_enabled,
            remove_all_args: self.redis_remove_all_args,
        }
    }

    pub fn valkey(&self) -> ValkeyObfuscationConfig {
        ValkeyObfuscationConfig {
            enabled: self.valkey_enabled,
            remove_all_args: self.valkey_remove_all_args,
        }
    }

    pub fn sql(&self) -> SqlObfuscationConfig {
        SqlObfuscationConfig {
            dbms: self.sql_dbms.clone(),
            table_names: self.sql_table_names,
            replace_digits: self.sql_replace_digits,
            keep_sql_alias: self.sql_keep_sql_alias,
            dollar_quoted_func: self.sql_dollar_quoted_func,
        }
    }

    pub fn mongo(&self) -> JsonObfuscationConfig {
        JsonObfuscationConfig {
            enabled: self.mongo_enabled,
            keep_values: self.mongo_keep_values.clone(),
            obfuscate_sql_values: self.mongo_obfuscate_sql_values.clone(),
        }
    }

    pub fn es(&self) -> JsonObfuscationConfig {
        JsonObfuscationConfig {
            enabled: self.es_enabled,
            keep_values: self.es_keep_values.clone(),
            obfuscate_sql_values: self.es_obfuscate_sql_values.clone(),
        }
    }

    pub fn open_search(&self) -> JsonObfuscationConfig {
        JsonObfuscationConfig {
            enabled: self.open_search_enabled,
            keep_values: self.open_search_keep_values.clone(),
            obfuscate_sql_values: self.open_search_obfuscate_sql_values.clone(),
        }
    }
}

/// HTTP URL obfuscation configuration.
#[derive(Clone, Debug, Default)]
pub struct HttpObfuscationConfig {
    pub(crate) remove_query_string: bool,
    pub(crate) remove_path_digits: bool,
}

/// Memcached obfuscation configuration.
#[derive(Clone, Debug, Default)]
pub struct MemcachedObfuscationConfig {
    pub(crate) enabled: bool,
    pub(crate) keep_command: bool,
}

/// Credit card obfuscation configuration.
#[derive(Clone, Debug, Default)]
pub struct CreditCardObfuscationConfig {
    pub(crate) enabled: bool,
    pub(crate) luhn: bool,
    pub(crate) keep_values: Vec<String>,
}

/// Redis obfuscation configuration.
#[derive(Clone, Debug, Default)]
pub struct RedisObfuscationConfig {
    pub(crate) enabled: bool,
    pub(crate) remove_all_args: bool,
}

/// Valkey obfuscation configuration.
#[derive(Clone, Debug, Default)]
pub struct ValkeyObfuscationConfig {
    pub(crate) enabled: bool,
    pub(crate) remove_all_args: bool,
}

/// SQL obfuscation configuration.
#[derive(Clone, Debug, Default)]
pub struct SqlObfuscationConfig {
    pub(crate) dbms: String,
    pub(crate) table_names: bool,
    pub(crate) replace_digits: bool,
    pub(crate) keep_sql_alias: bool,
    pub(crate) dollar_quoted_func: bool,
}

/// JSON obfuscation configuration for MongoDB, Elasticsearch, and OpenSearch.
#[derive(Clone, Debug, Default)]
pub struct JsonObfuscationConfig {
    pub(crate) enabled: bool,
    pub(crate) keep_values: Vec<String>,
    pub(crate) obfuscate_sql_values: Vec<String>,
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

    pub fn with_dbms(&self, dbms: String) -> Self {
        let mut clone = self.clone();
        clone.dbms = dbms;
        clone
    }

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
