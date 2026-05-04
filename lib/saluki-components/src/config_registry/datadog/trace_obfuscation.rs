//! Annotations for trace obfuscation transform configuration keys.
use crate::config_registry::{generated::schema, structs, SalukiAnnotation, SchemaEntry, SupportLevel, ValueType};

// Custom statics for SQL obfuscation fields: no corresponding entries exist in the
// vendored Agent schema, so these are defined manually with the correct paths.

static SQL_DBMS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql.dbms",
    env_vars: &[],
    value_type: ValueType::String,
    default: None,
};

static SQL_DOLLAR_QUOTED: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql.dollar_quoted_func",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

static SQL_KEEP_ALIAS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql.keep_sql_alias",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

static SQL_REPLACE_DIGITS: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql.replace_digits",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

static SQL_TABLE_NAMES: SchemaEntry = SchemaEntry {
    yaml_path: "apm_config.obfuscation.sql.table_names",
    env_vars: &[],
    value_type: ValueType::Bool,
    default: None,
};

crate::declare_annotations! {
    /// `apm_config.obfuscation.credit_cards.enabled`
    CONFIG_CREDIT_CARDS_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_CREDIT_CARDS_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.credit_cards.keep_values`
    CONFIG_CREDIT_CARDS_KEEP_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_CREDIT_CARDS_KEEP_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.credit_cards.luhn`
    CONFIG_CREDIT_CARDS_LUHN = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_CREDIT_CARDS_LUHN,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.elasticsearch.enabled`
    CONFIG_ES_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_ELASTICSEARCH_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.elasticsearch.keep_values`
    CONFIG_ES_KEEP_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_ELASTICSEARCH_KEEP_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.elasticsearch.obfuscate_sql_values`
    CONFIG_ES_OBFUSCATE_SQL_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_ELASTICSEARCH_OBFUSCATE_SQL_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.http.remove_paths_with_digits`
    CONFIG_HTTP_REMOVE_PATH_DIGITS = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_HTTP_REMOVE_PATHS_WITH_DIGITS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.http.remove_query_string`
    CONFIG_HTTP_REMOVE_QUERY_STRING = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_HTTP_REMOVE_QUERY_STRING,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.memcached.enabled`
    CONFIG_MEMCACHED_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_MEMCACHED_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.memcached.keep_command`
    CONFIG_MEMCACHED_KEEP_COMMAND = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_MEMCACHED_KEEP_COMMAND,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.mongodb.enabled`
    CONFIG_MONGO_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_MONGODB_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.mongodb.keep_values`
    CONFIG_MONGO_KEEP_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_MONGODB_KEEP_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.mongodb.obfuscate_sql_values`
    CONFIG_MONGO_OBFUSCATE_SQL_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_MONGODB_OBFUSCATE_SQL_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.opensearch.enabled`
    CONFIG_OPEN_SEARCH_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_OPENSEARCH_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.opensearch.keep_values`
    CONFIG_OPEN_SEARCH_KEEP_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_OPENSEARCH_KEEP_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.opensearch.obfuscate_sql_values`
    CONFIG_OPEN_SEARCH_OBFUSCATE_SQL_VALUES = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_OPENSEARCH_OBFUSCATE_SQL_VALUES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.redis.enabled`
    CONFIG_REDIS_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_REDIS_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.redis.remove_all_args`
    CONFIG_REDIS_REMOVE_ALL_ARGS = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_REDIS_REMOVE_ALL_ARGS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.sql.dbms`
    CONFIG_SQL_DBMS = SalukiAnnotation {
        schema: &SQL_DBMS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.sql.dollar_quoted_func`
    CONFIG_SQL_DOLLAR_QUOTED_FUNC = SalukiAnnotation {
        schema: &SQL_DOLLAR_QUOTED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.sql.keep_sql_alias`
    CONFIG_SQL_KEEP_SQL_ALIAS = SalukiAnnotation {
        schema: &SQL_KEEP_ALIAS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.sql.replace_digits`
    CONFIG_SQL_REPLACE_DIGITS = SalukiAnnotation {
        schema: &SQL_REPLACE_DIGITS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.sql.table_names`
    CONFIG_SQL_TABLE_NAMES = SalukiAnnotation {
        schema: &SQL_TABLE_NAMES,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };

    /// `apm_config.obfuscation.valkey.enabled`
    CONFIG_VALKEY_ENABLED = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_VALKEY_ENABLED,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: Some("true"),
    };

    /// `apm_config.obfuscation.valkey.remove_all_args`
    CONFIG_VALKEY_REMOVE_ALL_ARGS = SalukiAnnotation {
        schema: &schema::APM_CONFIG_OBFUSCATION_VALKEY_REMOVE_ALL_ARGS,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TRACE_OBFUSCATION_CONFIGURATION],
        value_type_override: None,
        test_json: None,
    };
}
