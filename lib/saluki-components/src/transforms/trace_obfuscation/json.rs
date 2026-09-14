//! JSON obfuscation for MongoDB, Elasticsearch, and OpenSearch queries.
//!
//! Obfuscation is a single pass that copies the characters it keeps, so keys come out in the order
//! they were sent and malformed input can still be obfuscated up to the point where it breaks.
//! Whitespace between tokens is dropped, which is what the Datadog Agent does.
//!
//! Ported from the Datadog Agent's `pkg/obfuscate/json.go`.

use saluki_common::collections::FastHashSet;
use stringtheory::MetaString;
use tracing::debug;

use super::json_scanner::{Op, Scanner};
use super::obfuscator::SqlObfuscationConfig;
use super::sql::obfuscate_sql_string;

/// Replaces a value whose SQL obfuscation failed.
///
/// Kept identical to the Datadog Agent's message: the value reaches the backend as part of the
/// resource string that stats are aggregated on, so a different message would split the aggregation.
const SQL_OBFUSCATION_FAILURE: &str =
    "Datadog-agent failed to obfuscate SQL string. Enable agent debug logs for more info.";

/// The value written in place of anything that is obfuscated.
const OBFUSCATED_VALUE: &str = "\"?\"";

/// Appended to output that was cut short by a syntax error.
const TRUNCATION_MARKER: &str = "...";

/// Pre-initialized JSON obfuscator with computed sets.
pub struct JsonObfuscator {
    keep_keys: FastHashSet<MetaString>,
    sql_keys: FastHashSet<MetaString>,
    sql_config: SqlObfuscationConfig,
}

impl JsonObfuscator {
    /// Creates a new JSON obfuscator with pre-computed sets.
    pub fn new(keep_values: &[String], obfuscate_sql_values: &[String], sql_config: &SqlObfuscationConfig) -> Self {
        Self {
            keep_keys: keep_values.iter().map(|s| MetaString::from(s.as_str())).collect(),
            sql_keys: obfuscate_sql_values
                .iter()
                .map(|s| MetaString::from(s.as_str()))
                .collect(),
            sql_config: sql_config.clone(),
        }
    }

    /// Obfuscates a JSON string by replacing every value with `?`.
    ///
    /// Keys are always kept. A value whose key is in `keep_values` is kept as sent, along with
    /// everything nested under it, and a string value whose key is in `obfuscate_sql_values` is
    /// replaced by its SQL obfuscation.
    ///
    /// Malformed input is obfuscated as far as it parses and the result ends in `...`. Returning
    /// the part we understood is safer than returning the input, which would leak the values we
    /// could not reach.
    pub fn obfuscate(&self, json_str: &str) -> String {
        if json_str.is_empty() {
            return String::new();
        }

        let (obfuscated, err) = ObfuscationPass::new(self, json_str.len()).run(json_str);
        if let Some(err) = err {
            debug!(
                error = err,
                "Failed to scan JSON string; obfuscated output was truncated."
            );
        }

        obfuscated
    }
}

/// The composite value the pass is inside of.
enum Closure {
    Object,
    Array,
}

/// One pass of a [`JsonObfuscator`] over an input string.
struct ObfuscationPass<'a> {
    obfuscator: &'a JsonObfuscator,

    /// The obfuscated output built so far.
    out: String,

    /// The current key, or the value of a key awaiting SQL obfuscation.
    buf: String,

    closures: Vec<Closure>,

    /// The depth at which `keeping` stops.
    keep_depth: usize,

    /// True while scanning a key rather than a value.
    key: bool,

    /// True once the current value has been replaced, so a value spanning several characters is
    /// replaced once rather than per character.
    wiped: bool,

    /// True while inside a value that is kept as sent.
    keeping: bool,

    /// True while collecting a value for SQL obfuscation.
    transforming: bool,
}

impl<'a> ObfuscationPass<'a> {
    fn new(obfuscator: &'a JsonObfuscator, input_len: usize) -> Self {
        Self {
            obfuscator,
            out: String::with_capacity(input_len),
            buf: String::new(),
            closures: Vec::new(),
            keep_depth: 0,
            key: false,
            wiped: false,
            keeping: false,
            transforming: false,
        }
    }

    /// Obfuscates `input`, returning the output and the syntax error that cut it short, if any.
    fn run(mut self, input: &str) -> (String, Option<String>) {
        let mut scanner = Scanner::new();

        for c in input.chars() {
            let op = scanner.step(c);

            // The depth before this character is applied, which is the depth the value or key that
            // just ended belongs to.
            let depth = self.closures.len();

            match op {
                Op::BeginObject => {
                    self.closures.push(Closure::Object);
                    self.set_key();
                    self.transforming = false;
                }
                Op::BeginArray => {
                    self.closures.push(Closure::Array);
                    self.set_key();
                    self.transforming = false;
                }
                Op::EndObject | Op::EndArray => {
                    // The outermost closure is left in place, which is what decides whether a value
                    // following a complete document is read as a key or as a value.
                    if self.closures.len() > 1 {
                        self.closures.pop();
                    }
                    self.set_key();
                    self.finish_value(depth);
                }
                Op::ObjectValue | Op::ArrayValue => {
                    self.set_key();
                    self.finish_value(depth);
                }
                Op::BeginLiteral | Op::Continue => {
                    if self.transforming {
                        self.buf.push(c);
                        continue;
                    } else if self.key {
                        self.buf.push(c);
                    } else if !self.keeping {
                        if !self.wiped {
                            self.out.push_str(OBFUSCATED_VALUE);
                            self.wiped = true;
                        }
                        continue;
                    }
                }
                Op::ObjectKey => {
                    let key = self.buf.trim_matches('"');
                    if !self.keeping && self.obfuscator.keep_keys.contains(key) {
                        self.keeping = true;
                        self.keep_depth = depth + 1;
                    } else if !self.transforming && self.obfuscator.sql_keys.contains(key) {
                        // Only a string value is obfuscated as SQL. Anything else ends the attempt
                        // and is obfuscated as usual.
                        self.transforming = true;
                    }
                    self.buf.clear();
                    self.key = false;
                }
                Op::SkipSpace => continue,
                Op::Error => {
                    self.out.push_str(TRUNCATION_MARKER);
                    return (self.out, scanner.err);
                }
                // Whitespace after a document ended, which is kept.
                Op::End => {}
            }

            self.out.push(c);
        }

        if scanner.eof() == Op::Error {
            self.out.push_str(TRUNCATION_MARKER);
        }

        (self.out, scanner.err)
    }

    /// A key follows at the top level and inside an object, but not inside an array.
    fn set_key(&mut self) {
        self.key = matches!(self.closures.last(), None | Some(Closure::Object));
        self.wiped = false;
    }

    /// Handles the end of a value: writes its SQL obfuscation if one was collected, or leaves a
    /// kept subtree once the pass climbs back out of it.
    fn finish_value(&mut self, depth: usize) {
        if self.transforming {
            // The collected characters are a JSON literal. A string is unescaped before
            // obfuscation; anything else is passed on as written.
            let query = serde_json::from_str::<String>(&self.buf).unwrap_or_else(|_| self.buf.clone());
            let obfuscated = match obfuscate_sql_string(&query, &self.obfuscator.sql_config) {
                Ok(sql) => sql.query,
                Err(err) => {
                    // The query is logged because the message written in its place tells the user
                    // to look for it in the debug logs.
                    debug!(error = err, query = query, "Failed to obfuscate SQL string.");
                    SQL_OBFUSCATION_FAILURE.to_owned()
                }
            };

            self.out.push('"');
            self.out.push_str(&obfuscated);
            self.out.push('"');
            self.transforming = false;
            self.buf.clear();
        } else if self.keeping && depth < self.keep_depth {
            self.keeping = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn default_sql_config() -> SqlObfuscationConfig {
        SqlObfuscationConfig::default()
    }

    /// Helper for tests - creates obfuscator and obfuscates in one call.
    fn obfuscate_json_string(
        json_str: &str, keep_values: &[String], obfuscate_sql_values: &[String], sql_config: &SqlObfuscationConfig,
    ) -> String {
        JsonObfuscator::new(keep_values, obfuscate_sql_values, sql_config).obfuscate(json_str)
    }

    fn keys(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    /// Every expected value below is what the Datadog Agent's obfuscator returns for the same
    /// input, compared character for character.
    fn assert_obfuscated(input: &str, keep_values: &[&str], obfuscate_sql_values: &[&str], expected: &str) {
        let result = obfuscate_json_string(
            input,
            &keys(keep_values),
            &keys(obfuscate_sql_values),
            &default_sql_config(),
        );
        assert_eq!(result, expected, "\ninput:\n{}", input);
    }

    const ES_BODY_MULTI_MATCH: &str = r#"{ "query": { "multi_match" : { "query" : "guide", "fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"] } } }"#;

    const ES_BODY_FIELDS: &str = r#"{"fields" : ["_all", { "key": "value", "other": ["1", "2", {"k": "v"}] }, "2"]}"#;

    const ES_BODY_SEARCH: &str = r#"{
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

    const MYSQL_PLAN: &str = r#"{
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

    #[test]
    fn simple_object() {
        assert_obfuscated(r#"{"user": "john", "id": 123}"#, &[], &[], r#"{"user":"?","id":"?"}"#);
    }

    #[test]
    fn nested_object() {
        assert_obfuscated(
            r#"{"user": {"name": "john", "age": 30}, "active": true}"#,
            &[],
            &[],
            r#"{"user":{"name":"?","age":"?"},"active":"?"}"#,
        );
    }

    #[test]
    fn array() {
        assert_obfuscated(
            r#"{"items": [1, 2, 3], "names": ["alice", "bob"]}"#,
            &[],
            &[],
            r#"{"items":["?","?","?"],"names":["?","?"]}"#,
        );
    }

    #[test]
    fn keep_values() {
        assert_obfuscated(
            r#"{"user": "john", "status": "active", "version": "1.0"}"#,
            &["status", "version"],
            &[],
            r#"{"user":"?","status":"active","version":"1.0"}"#,
        );
    }

    #[test]
    fn mongodb_query() {
        assert_obfuscated(
            r#"{"find": "users", "filter": {"age": {"$gt": 25}}, "limit": 10}"#,
            &[],
            &[],
            r#"{"find":"?","filter":{"age":{"$gt":"?"}},"limit":"?"}"#,
        );
    }

    #[test]
    fn elasticsearch_query() {
        assert_obfuscated(
            r#"{"query": {"match": {"title": "search term"}}, "size": 20}"#,
            &[],
            &[],
            r#"{"query":{"match":{"title":"?"}},"size":"?"}"#,
        );
    }

    #[test]
    fn empty_string() {
        assert_obfuscated("", &[], &[], "");
    }

    #[test]
    fn key_order_is_kept() {
        // Alphabetizing the keys would change the resource string that stats aggregate on.
        assert_obfuscated(
            r#"{"z": 1, "a": 2, "m": {"y": 3, "b": 4}}"#,
            &[],
            &[],
            r#"{"z":"?","a":"?","m":{"y":"?","b":"?"}}"#,
        );
    }

    #[test]
    fn whitespace_between_tokens_is_dropped() {
        assert_obfuscated(
            "{\n  \"a\": [ 1, 2 ],\n  \"b\": {\"z\": true}\n}",
            &[],
            &[],
            r#"{"a":["?","?"],"b":{"z":"?"}}"#,
        );
    }

    #[test]
    fn whitespace_after_a_document_is_kept() {
        assert_obfuscated(r#"  {"a":"b"}  "#, &[], &[], r#"{"a":"?"}  "#);
    }

    #[test]
    fn several_documents_are_each_obfuscated() {
        assert_obfuscated(
            r#"{"index":{"_index":"traces"}} {"value":1}"#,
            &[],
            &[],
            r#"{"index":{"_index":"?"}} {"value":"?"}"#,
        );
    }

    #[test]
    fn malformed_json_is_obfuscated_up_to_the_error() {
        // Returning the input untouched would leak the values before the error.
        assert_obfuscated(r#"{"invalid": json}"#, &[], &[], r#"{"invalid":..."#);
        assert_obfuscated(r#"{"a": [1, 2}"#, &[], &[], r#"{"a":["?","?"..."#);
        assert_obfuscated("nope", &[], &[], r#""?"..."#);
        assert_obfuscated(r#"{"a": tru}"#, &[], &[], r#"{"a":"?"..."#);
    }

    #[test]
    fn truncated_json_is_obfuscated_up_to_the_end() {
        assert_obfuscated(r#"{"a": "b", "c": "#, &[], &[], r#"{"a":"?","c":..."#);
        assert_obfuscated(r#"{"a": "b""#, &[], &[], r#"{"a":"?"..."#);
        assert_obfuscated(
            r#"{"a": "b", "keepme": "c""#,
            &["keepme"],
            &[],
            r#"{"a":"?","keepme":"c"..."#,
        );
    }

    #[test]
    fn keys_are_kept_as_sent() {
        assert_obfuscated(
            r#"{"que\"ry": 1, "sp ace": 2}"#,
            &[],
            &[],
            r#"{"que\"ry":"?","sp ace":"?"}"#,
        );
    }

    #[test]
    fn escaped_and_multibyte_values_are_obfuscated() {
        assert_obfuscated(
            r#"{"a": "he said \"hi\"", "b": "héllo ☃"}"#,
            &[],
            &[],
            r#"{"a":"?","b":"?"}"#,
        );
    }

    #[test]
    fn keep_values_keeps_the_whole_subtree() {
        assert_obfuscated(
            r#"{"keepme": {"x": 1, "y": [2, {"z": 3}]}, "other": 4}"#,
            &["keepme"],
            &[],
            r#"{"keepme":{"x":1,"y":[2,{"z":3}]},"other":"?"}"#,
        );
        assert_obfuscated(
            r#"{"keepme": [1, {"a": "b"}], "c": "d"}"#,
            &["keepme"],
            &[],
            r#"{"keepme":[1,{"a":"b"}],"c":"?"}"#,
        );
    }

    #[test]
    fn keep_values_stops_at_the_end_of_the_subtree() {
        assert_obfuscated(
            r#"{"a": {"keepme": {"b": 1}}, "c": 2}"#,
            &["keepme"],
            &[],
            r#"{"a":{"keepme":{"b":1}},"c":"?"}"#,
        );
    }

    #[test]
    fn es_body_multi_match() {
        assert_obfuscated(
            ES_BODY_MULTI_MATCH,
            &[],
            &[],
            r#"{"query":{"multi_match":{"query":"?","fields":["?",{"key":"?","other":["?","?",{"k":"?"}]},"?"]}}}"#,
        );
    }

    #[test]
    fn es_body_multi_match_keep_other() {
        assert_obfuscated(
            ES_BODY_MULTI_MATCH,
            &["other"],
            &[],
            r#"{"query":{"multi_match":{"query":"?","fields":["?",{"key":"?","other":["1","2",{"k":"v"}]},"?"]}}}"#,
        );
    }

    #[test]
    fn es_body_highlight() {
        assert_obfuscated(
            "{\n  \"highlight\": {\n    \"pre_tags\": [ \"<em>\" ],\n    \"post_tags\": [ \"</em>\" ],\n    \"index\": 1\n  }\n}",
            &[],
            &[],
            r#"{"highlight":{"pre_tags":["?"],"post_tags":["?"],"index":"?"}}"#,
        );
    }

    #[test]
    fn es_body_fields_keep_fields() {
        assert_obfuscated(
            ES_BODY_FIELDS,
            &["fields"],
            &[],
            r#"{"fields":["_all",{"key":"value","other":["1","2",{"k":"v"}]},"2"]}"#,
        );
    }

    #[test]
    fn es_body_fields_keep_k() {
        assert_obfuscated(
            ES_BODY_FIELDS,
            &["k"],
            &[],
            r#"{"fields":["?",{"key":"?","other":["?","?",{"k":"v"}]},"?"]}"#,
        );
    }

    #[test]
    fn es_body_fields_keep_nested_key() {
        assert_obfuscated(
            r#"{"fields" : [{"A": 1, "B": {"C": 3}}, "2"]}"#,
            &["C"],
            &[],
            r#"{"fields":[{"A":"?","B":{"C":3}},"?"]}"#,
        );
    }

    #[test]
    fn es_body_search() {
        assert_obfuscated(
            ES_BODY_SEARCH,
            &[],
            &[],
            r#"{"query":{"match":{"title":"?"}},"size":"?","from":"?","_source":["?","?","?"],"highlight":{"fields":{"title":{}}}}"#,
        );
    }

    #[test]
    fn es_body_search_keep_source() {
        assert_obfuscated(
            ES_BODY_SEARCH,
            &["_source"],
            &[],
            r#"{"query":{"match":{"title":"?"}},"size":"?","from":"?","_source":["title","summary","publish_date"],"highlight":{"fields":{"title":{}}}}"#,
        );
    }

    #[test]
    fn es_body_search_keep_query() {
        assert_obfuscated(
            ES_BODY_SEARCH,
            &["query"],
            &[],
            r#"{"query":{"match":{"title":"in action"}},"size":"?","from":"?","_source":["?","?","?"],"highlight":{"fields":{"title":{}}}}"#,
        );
    }

    #[test]
    fn es_body_search_keep_match() {
        assert_obfuscated(
            ES_BODY_SEARCH,
            &["match"],
            &[],
            r#"{"query":{"match":{"title":"in action"}},"size":"?","from":"?","_source":["?","?","?"],"highlight":{"fields":{"title":{}}}}"#,
        );
    }

    #[test]
    fn mongo_keep_company_wallet() {
        assert_obfuscated(
            r#"{"email":"dev@datadoghq.com","company_wallet_configuration_id":1}"#,
            &["company_wallet_configuration_id"],
            &[],
            r#"{"email":"?","company_wallet_configuration_id":1}"#,
        );
    }

    #[test]
    fn sql_value_is_obfuscated() {
        assert_obfuscated(
            r#"{"query": "select * from table where id = 2", "hello": "world", "hi": "there"}"#,
            &["hello"],
            &["query"],
            r#"{"query":"select * from table where id = ?","hello":"world","hi":"?"}"#,
        );
    }

    #[test]
    fn sql_key_with_object_value_falls_back_to_obfuscation() {
        assert_obfuscated(
            r#"{"object": {"not a": "query"}}"#,
            &[],
            &["object"],
            r#"{"object":{"not a":"?"}}"#,
        );
    }

    #[test]
    fn sql_key_with_array_value_falls_back_to_obfuscation() {
        assert_obfuscated(
            r#"{"object": ["not", "a", "query"]}"#,
            &[],
            &["object"],
            r#"{"object":["?","?","?"]}"#,
        );
    }

    #[test]
    fn failed_sql_obfuscation_reports_the_failure() {
        // An unterminated string literal cannot be tokenized.
        assert_obfuscated(
            r#"{"query": "select * from t where x = '"}"#,
            &[],
            &["query"],
            &format!(r#"{{"query":"{}"}}"#, SQL_OBFUSCATION_FAILURE),
        );

        // An unterminated comment cannot be tokenized either.
        assert_obfuscated(
            r#"{"query": "/* comment"}"#,
            &[],
            &["query"],
            &format!(r#"{{"query":"{}"}}"#, SQL_OBFUSCATION_FAILURE),
        );
    }

    #[test]
    fn sql_key_with_escaped_string_value_is_unescaped_first() {
        assert_obfuscated(
            r#"{"query": "select \"a\" from t where b = 'lit'"}"#,
            &[],
            &["query"],
            r#"{"query":"select a from t where b = ?"}"#,
        );
    }

    #[test]
    fn mysql_plan() {
        assert_obfuscated(
            MYSQL_PLAN,
            &[
                "select_id",
                "using_filesort",
                "table_name",
                "access_type",
                "possible_keys",
                "key",
                "key_length",
                "used_key_parts",
                "used_columns",
                "ref",
                "update",
            ],
            &["attached_condition"],
            r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"?"},"ordering_operation":{"using_filesort":true,"cost_info":{"sort_cost":"?"},"table":{"table_name":"sbtest1","access_type":"range","possible_keys":["PRIMARY"],"key":"PRIMARY","used_key_parts":["id"],"key_length":"4","rows_examined_per_scan":"?","rows_produced_per_join":"?","filtered":"?","cost_info":{"read_cost":"?","eval_cost":"?","prefix_cost":"?","data_read_per_join":"?"},"used_columns":["id","c"],"attached_condition":"( sbtest . sbtest1 . id between ? and ? )"}}}}"#,
        );
    }

    #[test]
    fn a_value_after_a_document_follows_the_first_document() {
        // Whether a second top-level value is read as a key or as a value depends on the closure
        // left over from the first document, so a string after an object is kept while the same
        // string after an array is obfuscated. Both match the Datadog Agent.
        assert_obfuscated(r#"{"a":1} "secret""#, &[], &[], r#"{"a":"?"} "secret""#);
        assert_obfuscated(r#"[1,2] "secret""#, &[], &[], r#"["?","?"] "?""#);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]
        #[test]
        fn property_test_arbitrary_input_never_panics(input in ".*") {
            // The scanner walks whatever a tracer sent, so it must not panic on any input. Not
            // exhaustive, but it catches simple robustness regressions on every test run.
            let _ = obfuscate_json_string(&input, &keys(&["a"]), &keys(&["query"]), &default_sql_config());
        }
    }
}
