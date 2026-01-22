//! Core obfuscator implementation.

use super::credit_cards::CreditCardObfuscator;
use super::http::obfuscate_url;
use super::json::JsonObfuscator;
use super::memcached::obfuscate_memcached_command;
use super::redis::{obfuscate_redis_string, obfuscate_valkey_string, quantize_redis_string, remove_all_redis_args};

pub use crate::common::datadog::obfuscation::{
    CreditCardObfuscationConfig, HttpObfuscationConfig, JsonObfuscationConfig, MemcachedObfuscationConfig,
    ObfuscationConfig, RedisObfuscationConfig, SqlObfuscationConfig, ValkeyObfuscationConfig,
};

/// Tag name constants for span metadata.
pub mod tags {
    pub const HTTP_URL: &str = "http.url";
    pub const SQL_QUERY: &str = "sql.query";
    pub const REDIS_RAW_COMMAND: &str = "redis.raw_command";
    pub const VALKEY_RAW_COMMAND: &str = "valkey.raw_command";
    pub const MEMCACHED_COMMAND: &str = "memcached.command";
    pub const MONGODB_QUERY: &str = "mongodb.query";
    pub const ELASTIC_BODY: &str = "elasticsearch.body";
    pub const OPENSEARCH_BODY: &str = "opensearch.body";
    pub const DBMS: &str = "db.system";
    pub const DB_STATEMENT: &str = "db.statement";
}

/// The main obfuscator that handles all obfuscation types.
pub struct Obfuscator {
    pub config: ObfuscationConfig,
    cc_obfuscator: Option<CreditCardObfuscator>,
    es_obfuscator: Option<JsonObfuscator>,
    open_search_obfuscator: Option<JsonObfuscator>,
    mongo_obfuscator: Option<JsonObfuscator>,
}

impl Obfuscator {
    /// Creates a new obfuscator with the given configuration.
    pub fn new(config: ObfuscationConfig) -> Self {
        let cc_obfuscator = if config.credit_cards().enabled() {
            Some(CreditCardObfuscator::new(config.credit_cards()))
        } else {
            None
        };

        let es_obfuscator = if config.es().enabled() {
            Some(JsonObfuscator::new(config.es(), config.sql()))
        } else {
            None
        };

        let open_search_obfuscator = if config.open_search().enabled() {
            Some(JsonObfuscator::new(config.open_search(), config.sql()))
        } else {
            None
        };

        let mongo_obfuscator = if config.mongo().enabled() {
            Some(JsonObfuscator::new(config.mongo(), config.sql()))
        } else {
            None
        };

        Self {
            config,
            cc_obfuscator,
            es_obfuscator,
            open_search_obfuscator,
            mongo_obfuscator,
        }
    }

    /// Obfuscates a URL string.
    pub fn obfuscate_url(&self, url: &str) -> String {
        obfuscate_url(url, self.config.http())
    }

    /// Obfuscates a Memcached command.
    pub fn obfuscate_memcached_command(&self, cmd: &str) -> String {
        obfuscate_memcached_command(cmd, self.config.memcached())
    }

    /// Obfuscates potential credit card numbers in a tag value.
    pub fn obfuscate_credit_card_number(&self, key: &str, val: &str) -> String {
        match &self.cc_obfuscator {
            Some(cc) => cc.obfuscate_credit_card_number(key, val),
            None => val.to_string(),
        }
    }

    /// Quantizes a Redis command string (extracts command names only).
    pub fn quantize_redis_string(&self, query: &str) -> String {
        quantize_redis_string(query)
    }

    /// Obfuscates a Redis command string using command-specific rules.
    pub fn obfuscate_redis_string(&self, rediscmd: &str) -> String {
        if self.config.redis().remove_all_args() {
            remove_all_redis_args(rediscmd)
        } else {
            obfuscate_redis_string(rediscmd, self.config.redis())
        }
    }

    /// Obfuscates a Valkey command string using command-specific rules.
    pub fn obfuscate_valkey_string(&self, valkeycmd: &str) -> String {
        if self.config.valkey().remove_all_args() {
            remove_all_redis_args(valkeycmd)
        } else {
            obfuscate_valkey_string(valkeycmd, self.config.valkey())
        }
    }

    /// Obfuscates a MongoDB JSON query string.
    pub fn obfuscate_mongodb_string(&self, query: &str) -> String {
        match &self.mongo_obfuscator {
            Some(obf) => obf.obfuscate(query),
            None => query.to_string(),
        }
    }

    /// Obfuscates an Elasticsearch JSON query string.
    pub fn obfuscate_elasticsearch_string(&self, query: &str) -> String {
        match &self.es_obfuscator {
            Some(obf) => obf.obfuscate(query),
            None => query.to_string(),
        }
    }

    /// Obfuscates an OpenSearch JSON query string.
    pub fn obfuscate_opensearch_string(&self, query: &str) -> String {
        match &self.open_search_obfuscator {
            Some(obf) => obf.obfuscate(query),
            None => query.to_string(),
        }
    }
}
