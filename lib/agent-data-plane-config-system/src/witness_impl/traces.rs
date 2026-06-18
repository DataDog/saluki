use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "apm_config.obfuscation.credit_cards.enabled" => {
            translator.native.components.traces.obfuscation.credit_cards.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.credit_cards.keep_values" => {
            translator.native.components.traces.obfuscation.credit_cards.keep_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.credit_cards.luhn" => {
            translator.native.components.traces.obfuscation.credit_cards.luhn = bool_value(value);

            None
        }
        "apm_config.obfuscation.elasticsearch.enabled" => {
            translator.native.components.traces.obfuscation.elasticsearch.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.elasticsearch.keep_values" => {
            translator
                .native
                .components
                .traces
                .obfuscation
                .elasticsearch
                .keep_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.elasticsearch.obfuscate_sql_values" => {
            translator
                .native
                .components
                .traces
                .obfuscation
                .elasticsearch
                .obfuscate_sql_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.http.remove_paths_with_digits" => {
            translator
                .native
                .components
                .traces
                .obfuscation
                .http
                .remove_paths_with_digits = bool_value(value);

            None
        }
        "apm_config.obfuscation.http.remove_query_string" => {
            translator.native.components.traces.obfuscation.http.remove_query_string = bool_value(value);

            None
        }
        "apm_config.obfuscation.memcached.enabled" => {
            translator.native.components.traces.obfuscation.memcached.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.memcached.keep_command" => {
            translator.native.components.traces.obfuscation.memcached.keep_command = bool_value(value);

            None
        }
        "apm_config.obfuscation.mongodb.enabled" => {
            translator.native.components.traces.obfuscation.mongodb.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.mongodb.keep_values" => {
            translator.native.components.traces.obfuscation.mongodb.keep_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.mongodb.obfuscate_sql_values" => {
            translator
                .native
                .components
                .traces
                .obfuscation
                .mongodb
                .obfuscate_sql_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.opensearch.enabled" => {
            translator.native.components.traces.obfuscation.opensearch.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.opensearch.keep_values" => {
            translator.native.components.traces.obfuscation.opensearch.keep_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.opensearch.obfuscate_sql_values" => {
            translator
                .native
                .components
                .traces
                .obfuscation
                .opensearch
                .obfuscate_sql_values = string_vec_value(value);

            None
        }
        "apm_config.obfuscation.redis.enabled" => {
            translator.native.components.traces.obfuscation.redis.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.redis.remove_all_args" => {
            translator.native.components.traces.obfuscation.redis.remove_all_args = bool_value(value);

            None
        }
        "apm_config.obfuscation.valkey.enabled" => {
            translator.native.components.traces.obfuscation.valkey.enabled = bool_value(value);

            None
        }
        "apm_config.obfuscation.valkey.remove_all_args" => {
            translator.native.components.traces.obfuscation.valkey.remove_all_args = bool_value(value);

            None
        }
        _ => Some(value),
    }
}
