//! JSON obfuscation for MongoDB, Elasticsearch, and OpenSearch queries.

use saluki_common::collections::FastHashSet;
use serde_json::{Map, Value};
use stringtheory::MetaString;

use super::obfuscator::{JsonObfuscationConfig, SqlObfuscationConfig};
use super::sql::obfuscate_sql_string;

/// Pre-initialized JSON obfuscator with computed sets.
pub struct JsonObfuscator {
    keep_keys: FastHashSet<MetaString>,
    sql_keys: FastHashSet<MetaString>,
    sql_config: SqlObfuscationConfig,
}

impl JsonObfuscator {
    /// Creates a new JSON obfuscator with pre-computed sets.
    pub fn new(config: &JsonObfuscationConfig, sql_config: &SqlObfuscationConfig) -> Self {
        Self {
            keep_keys: config
                .keep_values()
                .iter()
                .map(|s| MetaString::from(s.as_str()))
                .collect(),
            sql_keys: config
                .obfuscate_sql_values()
                .iter()
                .map(|s| MetaString::from(s.as_str()))
                .collect(),
            sql_config: sql_config.clone(),
        }
    }

    /// Obfuscates a JSON string by replacing all values with "?" except for keys in `keep_values`.
    pub fn obfuscate(&self, json_str: &str) -> String {
        if json_str.is_empty() {
            return String::new();
        }

        let value: Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(_) => return json_str.to_string(),
        };

        let obfuscated = self.obfuscate_value(&value, None);
        serde_json::to_string(&obfuscated).unwrap_or_else(|_| json_str.to_string())
    }

    fn obfuscate_value(&self, value: &Value, current_key: Option<&str>) -> Value {
        if let Some(key) = current_key {
            if self.keep_keys.contains(key) {
                return value.clone();
            }

            if self.sql_keys.contains(key) {
                if let Value::String(s) = value {
                    if let Ok(obfuscated) = obfuscate_sql_string(s, &self.sql_config) {
                        return Value::String(obfuscated.query);
                    }
                }
            }
        }

        match value {
            Value::Object(map) => {
                let mut new_map = Map::new();
                for (key, val) in map {
                    let obfuscated_val = self.obfuscate_value(val, Some(key));
                    new_map.insert(key.clone(), obfuscated_val);
                }
                Value::Object(new_map)
            }
            Value::Array(arr) => {
                let obfuscated_arr: Vec<Value> = arr.iter().map(|v| self.obfuscate_value(v, None)).collect();
                Value::Array(obfuscated_arr)
            }
            Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => Value::String("?".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> JsonObfuscationConfig {
        JsonObfuscationConfig {
            enabled: true,
            keep_values: Vec::new(),
            obfuscate_sql_values: Vec::new(),
        }
    }

    fn default_sql_config() -> SqlObfuscationConfig {
        SqlObfuscationConfig::default()
    }

    /// Helper for tests - creates obfuscator and obfuscates in one call.
    fn obfuscate_json_string(
        json_str: &str, config: &JsonObfuscationConfig, sql_config: &SqlObfuscationConfig,
    ) -> String {
        JsonObfuscator::new(config, sql_config).obfuscate(json_str)
    }

    #[test]
    fn test_obfuscate_simple_object() {
        let config = default_config();
        let sql_config = default_sql_config();
        let json = r#"{"user": "john", "id": 123}"#;
        let result = obfuscate_json_string(json, &config, &sql_config);

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["user"], "?");
        assert_eq!(parsed["id"], "?");
    }

    #[test]
    fn test_obfuscate_nested_object() {
        let config = default_config();
        let json = r#"{"user": {"name": "john", "age": 30}, "active": true}"#;
        let result = obfuscate_json_string(json, &config, &default_sql_config());

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["user"]["name"], "?");
        assert_eq!(parsed["user"]["age"], "?");
        assert_eq!(parsed["active"], "?");
    }

    #[test]
    fn test_obfuscate_array() {
        let config = default_config();
        let json = r#"{"items": [1, 2, 3], "names": ["alice", "bob"]}"#;
        let result = obfuscate_json_string(json, &config, &default_sql_config());

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["items"][0], "?");
        assert_eq!(parsed["items"][1], "?");
        assert_eq!(parsed["items"][2], "?");
        assert_eq!(parsed["names"][0], "?");
        assert_eq!(parsed["names"][1], "?");
    }

    #[test]
    fn test_keep_values() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["status".to_string(), "version".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let json = r#"{"user": "john", "status": "active", "version": "1.0"}"#;
        let result = obfuscate_json_string(json, &config, &default_sql_config());

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["user"], "?");
        assert_eq!(parsed["status"], "active");
        assert_eq!(parsed["version"], "1.0");
    }

    #[test]
    fn test_mongodb_query() {
        let config = default_config();
        let json = r#"{"find": "users", "filter": {"age": {"$gt": 25}}, "limit": 10}"#;
        let result = obfuscate_json_string(json, &config, &default_sql_config());

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["find"], "?");
        assert_eq!(parsed["filter"]["age"]["$gt"], "?");
        assert_eq!(parsed["limit"], "?");
    }

    #[test]
    fn test_elasticsearch_query() {
        let config = default_config();
        let json = r#"{"query": {"match": {"title": "search term"}}, "size": 20}"#;
        let result = obfuscate_json_string(json, &config, &default_sql_config());

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["query"]["match"]["title"], "?");
        assert_eq!(parsed["size"], "?");
    }

    #[test]
    fn test_invalid_json() {
        let config = default_config();
        let json = r#"{"invalid": json}"#;
        let result = obfuscate_json_string(json, &config, &default_sql_config());

        // Should return original string on parse error
        assert_eq!(result, json);
    }

    #[test]
    fn test_empty_string() {
        let config = default_config();
        let result = obfuscate_json_string("", &config, &default_sql_config());
        assert_eq!(result, "");
    }

    fn assert_json_eq(actual: &str, expected: &str) {
        let actual_val: Value = serde_json::from_str(actual).unwrap();
        let expected_val: Value = serde_json::from_str(expected).unwrap();
        assert_eq!(
            actual_val, expected_val,
            "\nActual:\n{}\nExpected:\n{}",
            actual, expected
        );
    }

    #[test]
    fn test_agent_elasticsearch_body_1() {
        let config = default_config();
        let input = r#"{ "query": { "multi_match" : { "query" : "guide", "fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"] } } }"#;
        let expected = r#"{ "query": { "multi_match": { "query": "?", "fields" : ["?", { "key": "?", "other": ["?", "?", {"k": "?"}] }, "?"] } } }"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_2() {
        let config = default_config();
        let input = r#"{
  "highlight": {
    "pre_tags": [ "<em>" ],
    "post_tags": [ "</em>" ],
    "index": 1
  }
}"#;
        let expected = r#"{
  "highlight": {
    "pre_tags": [ "?" ],
    "post_tags": [ "?" ],
    "index": "?"
  }
}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_3_keep_other() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["other".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{ "query": { "multi_match" : { "query" : "guide", "fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"] } } }"#;
        let expected = r#"{ "query": { "multi_match": { "query": "?", "fields" : ["?", { "key": "?", "other": ["1", "2", {"k": "v"}] }, "?"] } } }"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_4_keep_fields() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["fields".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{"fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"]}"#;
        let expected = r#"{"fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"]}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_5_keep_k() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["k".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{"fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"]}"#;
        let expected = r#"{"fields" : ["?", { "key": "?", "other": ["?", "?", {"k": "v"}] }, "?"]}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_6_keep_c() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["C".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{"fields" : [{"A": 1, "B": {"C": 3}}, "2"]}"#;
        let expected = r#"{"fields" : [{"A": "?", "B": {"C": 3}}, "?"]}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_7() {
        let config = default_config();
        let input = r#"{
    "query": {
       "match" : {
          "title" : "in action"
       }
    },
    "size": 2,
    "from": 0,
    "_source": [ "title", "summary", "publish_date" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let expected = r#"{
    "query": {
       "match" : {
          "title" : "?"
       }
    },
    "size": "?",
    "from": "?",
    "_source": [ "?", "?", "?" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_8_keep_source() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["_source".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{
    "query": {
       "match" : {
          "title" : "in action"
       }
    },
    "size": 2,
    "from": 0,
    "_source": [ "title", "summary", "publish_date" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let expected = r#"{
    "query": {
       "match" : {
          "title" : "?"
       }
    },
    "size": "?",
    "from": "?",
    "_source": [ "title", "summary", "publish_date" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_9_keep_query() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["query".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{
    "query": {
       "match" : {
          "title" : "in action"
       }
    },
    "size": 2,
    "from": 0,
    "_source": [ "title", "summary", "publish_date" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let expected = r#"{
    "query": {
       "match" : {
          "title" : "in action"
       }
    },
    "size": "?",
    "from": "?",
    "_source": [ "?", "?", "?" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_elasticsearch_body_10_keep_match() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["match".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{
    "query": {
       "match" : {
          "title" : "in action"
       }
    },
    "size": 2,
    "from": 0,
    "_source": [ "title", "summary", "publish_date" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let expected = r#"{
    "query": {
       "match" : {
          "title" : "in action"
       }
    },
    "size": "?",
    "from": "?",
    "_source": [ "?", "?", "?" ],
    "highlight": {
       "fields" : {
          "title" : {}
       }
    }
}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_mongo_keep_company_wallet() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["company_wallet_configuration_id".to_string()],
            obfuscate_sql_values: Vec::new(),
        };
        let input = r#"{"email":"dev@datadoghq.com","company_wallet_configuration_id":1}"#;
        let expected = r#"{"email":"?","company_wallet_configuration_id":1}"#;
        let result = obfuscate_json_string(input, &config, &default_sql_config());
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_sql_json_basic() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec!["hello".to_string()],
            obfuscate_sql_values: vec!["query".to_string()],
        };
        let sql_config = default_sql_config();
        let input = r#"{"query": "select * from table where id = 2", "hello": "world", "hi": "there"}"#;
        let expected = r#"{"query": "select * from table where id = ?", "hello": "world", "hi": "?"}"#;
        let result = obfuscate_json_string(input, &config, &sql_config);
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_sql_json_tried_sql_obfuscate_an_object() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: Vec::new(),
            obfuscate_sql_values: vec!["object".to_string()],
        };
        let sql_config = default_sql_config();
        let input = r#"{"object": {"not a": "query"}}"#;
        let expected = r#"{"object": {"not a": "?"}}"#;
        let result = obfuscate_json_string(input, &config, &sql_config);
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_sql_json_tried_sql_obfuscate_an_array() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: Vec::new(),
            obfuscate_sql_values: vec!["object".to_string()],
        };
        let sql_config = default_sql_config();
        let input = r#"{"object": ["not", "a", "query"]}"#;
        let expected = r#"{"object": ["?", "?", "?"]}"#;
        let result = obfuscate_json_string(input, &config, &sql_config);
        assert_json_eq(&result, expected);
    }

    #[test]
    fn test_agent_sql_plan_mysql() {
        let config = JsonObfuscationConfig {
            enabled: true,
            keep_values: vec![
                "select_id".to_string(),
                "using_filesort".to_string(),
                "table_name".to_string(),
                "access_type".to_string(),
                "possible_keys".to_string(),
                "key".to_string(),
                "key_length".to_string(),
                "used_key_parts".to_string(),
                "used_columns".to_string(),
                "ref".to_string(),
                "update".to_string(),
            ],
            obfuscate_sql_values: vec!["attached_condition".to_string()],
        };
        let sql_config = default_sql_config();
        let input = r#"{
  "query_block": {
	"select_id": 1,
	"cost_info": {
	  "query_cost": "120.31"
	},
	"ordering_operation": {
	  "using_filesort": true,
	  "cost_info": {
		"sort_cost": "100.00"
	  },
	  "table": {
		"table_name": "sbtest1",
		"access_type": "range",
		"possible_keys": [
		  "PRIMARY"
		],
		"key": "PRIMARY",
		"used_key_parts": [
		  "id"
		],
		"key_length": "4",
		"rows_examined_per_scan": 100,
		"rows_produced_per_join": 100,
		"filtered": "100.00",
		"cost_info": {
		  "read_cost": "10.31",
		  "eval_cost": "10.00",
		  "prefix_cost": "20.31",
		  "data_read_per_join": "71K"
		},
		"used_columns": [
		  "id",
		  "c"
		],
		"attached_condition": "(`sbtest`.`sbtest1`.`id` between 5016 and 5115)"
	  }
	}
  }
}"#;
        let expected = r#"{
  "query_block": {
	"select_id": 1,
	"cost_info": {
	  "query_cost": "?"
	},
	"ordering_operation": {
	  "using_filesort": true,
	  "cost_info": {
		"sort_cost": "?"
	  },
	  "table": {
		"table_name": "sbtest1",
		"access_type": "range",
		"possible_keys": [
		  "PRIMARY"
		],
		"key": "PRIMARY",
		"used_key_parts": [
		  "id"
		],
		"key_length": "4",
		"rows_examined_per_scan": "?",
		"rows_produced_per_join": "?",
		"filtered": "?",
		"cost_info": {
		  "read_cost": "?",
		  "eval_cost": "?",
		  "prefix_cost": "?",
		  "data_read_per_join": "?"
		},
		"used_columns": [
		  "id",
		  "c"
		],
		"attached_condition": "( sbtest . sbtest1 . id between ? and ? )"
	  }
	}
  }
}"#;
        let result = obfuscate_json_string(input, &config, &sql_config);
        assert_json_eq(&result, expected);
    }
}
