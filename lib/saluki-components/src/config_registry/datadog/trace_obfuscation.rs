//! Annotations for trace obfuscation transform configuration keys.
//!
//! The TraceObfuscationConfiguration deserializes from `config.*` paths (ADP-specific),
//! not from the Agent schema's `apm_config.obfuscation.*` paths.
use crate::config_registry::{structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// All paths are ADP-specific; the Agent schema uses `apm_config.obfuscation.*` equivalents.
static CC_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.credit_cards.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static CC_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "config.credit_cards.keep_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static CC_LUHN: SchemaEntry = SchemaEntry {
    yaml_path: "config.credit_cards.luhn",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static ES_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.es.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static ES_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "config.es.keep_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static ES_OBFUSCATE_SQL: SchemaEntry = SchemaEntry {
    yaml_path: "config.es.obfuscate_sql_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static HTTP_REMOVE_PATH_DIGITS: SchemaEntry = SchemaEntry {
    yaml_path: "config.http.remove_path_digits",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static HTTP_REMOVE_QUERY_STRING: SchemaEntry = SchemaEntry {
    yaml_path: "config.http.remove_query_string",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static MCD_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.memcached.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static MCD_KEEP_COMMAND: SchemaEntry = SchemaEntry {
    yaml_path: "config.memcached.keep_command",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static MONGO_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.mongo.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static MONGO_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "config.mongo.keep_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static MONGO_OBFUSCATE_SQL: SchemaEntry = SchemaEntry {
    yaml_path: "config.mongo.obfuscate_sql_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static OS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.open_search.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static OS_KEEP_VALUES: SchemaEntry = SchemaEntry {
    yaml_path: "config.open_search.keep_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static OS_OBFUSCATE_SQL: SchemaEntry = SchemaEntry {
    yaml_path: "config.open_search.obfuscate_sql_values",
    env_vars: &[],
    value_type: ValueType::StringList,
};
static REDIS_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.redis.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static REDIS_REMOVE_ALL: SchemaEntry = SchemaEntry {
    yaml_path: "config.redis.remove_all_args",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static SQL_DBMS: SchemaEntry = SchemaEntry {
    yaml_path: "config.sql.dbms",
    env_vars: &[],
    value_type: ValueType::String,
};
static SQL_DOLLAR_QUOTED: SchemaEntry = SchemaEntry {
    yaml_path: "config.sql.dollar_quoted_func",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static SQL_KEEP_ALIAS: SchemaEntry = SchemaEntry {
    yaml_path: "config.sql.keep_sql_alias",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static SQL_REPLACE_DIGITS: SchemaEntry = SchemaEntry {
    yaml_path: "config.sql.replace_digits",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static SQL_TABLE_NAMES: SchemaEntry = SchemaEntry {
    yaml_path: "config.sql.table_names",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static VALKEY_ENABLED: SchemaEntry = SchemaEntry {
    yaml_path: "config.valkey.enabled",
    env_vars: &[],
    value_type: ValueType::Bool,
};
static VALKEY_REMOVE_ALL: SchemaEntry = SchemaEntry {
    yaml_path: "config.valkey.remove_all_args",
    env_vars: &[],
    value_type: ValueType::Bool,
};

crate::declare_annotations! {
    /// `config.credit_cards.enabled`
    CONFIG_CREDIT_CARDS_ENABLED = SalukiAnnotation { schema: &CC_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.credit_cards.keep_values`
    CONFIG_CREDIT_CARDS_KEEP_VALUES = SalukiAnnotation { schema: &CC_KEEP_VALUES, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.credit_cards.luhn`
    CONFIG_CREDIT_CARDS_LUHN = SalukiAnnotation { schema: &CC_LUHN, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.es.enabled`
    CONFIG_ES_ENABLED = SalukiAnnotation { schema: &ES_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.es.keep_values`
    CONFIG_ES_KEEP_VALUES = SalukiAnnotation { schema: &ES_KEEP_VALUES, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.es.obfuscate_sql_values`
    CONFIG_ES_OBFUSCATE_SQL_VALUES = SalukiAnnotation { schema: &ES_OBFUSCATE_SQL, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.http.remove_path_digits`
    CONFIG_HTTP_REMOVE_PATH_DIGITS = SalukiAnnotation { schema: &HTTP_REMOVE_PATH_DIGITS, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.http.remove_query_string`
    CONFIG_HTTP_REMOVE_QUERY_STRING = SalukiAnnotation { schema: &HTTP_REMOVE_QUERY_STRING, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.memcached.enabled`
    CONFIG_MEMCACHED_ENABLED = SalukiAnnotation { schema: &MCD_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.memcached.keep_command`
    CONFIG_MEMCACHED_KEEP_COMMAND = SalukiAnnotation { schema: &MCD_KEEP_COMMAND, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.mongo.enabled`
    CONFIG_MONGO_ENABLED = SalukiAnnotation { schema: &MONGO_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.mongo.keep_values`
    CONFIG_MONGO_KEEP_VALUES = SalukiAnnotation { schema: &MONGO_KEEP_VALUES, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.mongo.obfuscate_sql_values`
    CONFIG_MONGO_OBFUSCATE_SQL_VALUES = SalukiAnnotation { schema: &MONGO_OBFUSCATE_SQL, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.open_search.enabled`
    CONFIG_OPEN_SEARCH_ENABLED = SalukiAnnotation { schema: &OS_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.open_search.keep_values`
    CONFIG_OPEN_SEARCH_KEEP_VALUES = SalukiAnnotation { schema: &OS_KEEP_VALUES, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.open_search.obfuscate_sql_values`
    CONFIG_OPEN_SEARCH_OBFUSCATE_SQL_VALUES = SalukiAnnotation { schema: &OS_OBFUSCATE_SQL, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.redis.enabled`
    CONFIG_REDIS_ENABLED = SalukiAnnotation { schema: &REDIS_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.redis.remove_all_args`
    CONFIG_REDIS_REMOVE_ALL_ARGS = SalukiAnnotation { schema: &REDIS_REMOVE_ALL, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.sql.dbms`
    CONFIG_SQL_DBMS = SalukiAnnotation { schema: &SQL_DBMS, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.sql.dollar_quoted_func`
    CONFIG_SQL_DOLLAR_QUOTED_FUNC = SalukiAnnotation { schema: &SQL_DOLLAR_QUOTED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.sql.keep_sql_alias`
    CONFIG_SQL_KEEP_SQL_ALIAS = SalukiAnnotation { schema: &SQL_KEEP_ALIAS, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.sql.replace_digits`
    CONFIG_SQL_REPLACE_DIGITS = SalukiAnnotation { schema: &SQL_REPLACE_DIGITS, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.sql.table_names`
    CONFIG_SQL_TABLE_NAMES = SalukiAnnotation { schema: &SQL_TABLE_NAMES, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.valkey.enabled`
    CONFIG_VALKEY_ENABLED = SalukiAnnotation { schema: &VALKEY_ENABLED, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
    /// `config.valkey.remove_all_args`
    CONFIG_VALKEY_REMOVE_ALL_ARGS = SalukiAnnotation { schema: &VALKEY_REMOVE_ALL, support_level: SupportLevel::Full, additional_yaml_paths: &[], env_var_override: None, used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION], value_type_override: None, test_json: None };
}
