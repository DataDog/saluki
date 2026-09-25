//! JSON obfuscation for MongoDB, Elasticsearch, and OpenSearch queries.
//!
//! The scan lives in `libdd_trace_obfuscation::json`: a single pass that copies the characters it keeps, so keys keep
//! their order and their text, and malformed input is still obfuscated up to the point where it breaks and ends in
//! `...`. Whitespace between tokens is dropped, as the reference implementation does.
//!
//! SQL obfuscation of a value is passed to the scan as a callback, so the SQL configuration this transform resolved
//! from its own configuration can reach it without becoming part of the scan's configuration, which is plain data. The
//! callback also carries the failure policy: a value whose SQL obfuscation fails carries the reference
//! implementation's failure message rather than `"?"`.

use std::{borrow::Cow, convert::Infallible};

use libdd_trace_obfuscation::json::{self, JsonObfuscationScratch, ScratchCapacity};
use libdd_trace_obfuscation::obfuscation_config::JsonObfuscatorConfig;
use tracing::debug;

use super::obfuscator::SqlObfuscationConfig;
use super::sql::obfuscate_sql_string;

/// Replaces a value whose SQL obfuscation failed.
///
/// Kept identical to the reference implementation's message: the value reaches the backend as part of the resource
/// string that stats are aggregated on, so a different message would split the aggregation.
const SQL_OBFUSCATION_FAILURE: &str =
    "Datadog-agent failed to obfuscate SQL string. Enable agent debug logs for more info.";

/// How much output buffer the obfuscator keeps between calls, in bytes.
///
/// The buffer is kept so the next query reuses it, but it holds whatever capacity the last query grew it to, so one
/// enormous query would otherwise stay held for the obfuscator's lifetime. The limit sits well above an ordinary
/// query: a query within it keeps its reuse, and a query past it has the excess released before the obfuscator is
/// used again.
const OUTPUT_RETENTION_LIMIT_BYTES: usize = 64 * 1024;

/// How much scratch memory the obfuscator keeps between calls, with the same policy as
/// [`OUTPUT_RETENTION_LIMIT_BYTES`].
const SCRATCH_RETENTION_LIMIT: ScratchCapacity = ScratchCapacity::new(OUTPUT_RETENTION_LIMIT_BYTES, 256);

/// A JSON obfuscator for MongoDB, Elasticsearch, and OpenSearch queries, wrapping the shared crate's scan.
///
/// The output and scratch buffers are held here and reused by every call, so an ordinary query allocates only the
/// returned copy of its result. A pass that grows them past the retention limits has the excess released before the
/// obfuscator is used again, so an oversized query does not stay held.
pub struct JsonObfuscator {
    obfuscator: json::JsonObfuscator,
    sql_config: SqlObfuscationConfig,
    /// The obfuscated output, reused across calls.
    out: String,
    /// Working memory for the scan, reused across calls.
    scratch: JsonObfuscationScratch,
}

impl JsonObfuscator {
    /// Creates a new JSON obfuscator with pre-computed key sets.
    ///
    /// A value whose key is in `keep_values` is kept as sent, along with everything nested under it. A string value
    /// whose key is in `obfuscate_sql_values` is replaced by its SQL obfuscation, using `sql_config`.
    pub fn new(keep_values: &[String], obfuscate_sql_values: &[String], sql_config: &SqlObfuscationConfig) -> Self {
        let mut config = JsonObfuscatorConfig::enabled();
        config.keep_keys.extend(keep_values.iter().cloned());
        config.transform_keys.extend(obfuscate_sql_values.iter().cloned());

        Self {
            obfuscator: json::JsonObfuscator::new(config),
            sql_config: sql_config.clone(),
            out: String::new(),
            scratch: JsonObfuscationScratch::new(),
        }
    }

    /// Obfuscates a JSON string by replacing every value with `"?"`.
    ///
    /// Keys are always kept, in the order and with the text they were sent with. Malformed input is obfuscated as far
    /// as it scans and the result ends in `...`: returning the part that was understood is safer than returning the
    /// input, which would leak the values that could not be reached.
    pub fn obfuscate(&mut self, json_str: &str) -> String {
        if json_str.is_empty() {
            return String::new();
        }

        let report = self
            .obfuscator
            .obfuscate_into(json_str, &mut self.out, &mut self.scratch, |value| {
                transform_sql_value(value, &self.sql_config)
            });
        if let Some(scan_error) = report.scan_error {
            debug!(
                error = %scan_error,
                "Failed to scan JSON string; obfuscated output was truncated."
            );
        }

        let output = self.out.clone();
        trim_retained_buffers(&mut self.out, &mut self.scratch);
        output
    }
}

/// Releases buffer capacity past the retention limits.
///
/// `output` is cleared first because a shrink cannot go below the live bytes it still holds, and the scratch because
/// a pass leaves what it last wrote in its buffers. A query within the limits keeps everything it grew, so the
/// ordinary case never pays for this.
fn trim_retained_buffers(output: &mut String, scratch: &mut JsonObfuscationScratch) {
    if output.capacity() > OUTPUT_RETENTION_LIMIT_BYTES {
        output.clear();
        output.shrink_to(OUTPUT_RETENTION_LIMIT_BYTES);
    }

    let retained = scratch.retained_capacity();
    let exceeds = retained.unescape_bytes > SCRATCH_RETENTION_LIMIT.unescape_bytes
        || retained.nesting_slots > SCRATCH_RETENTION_LIMIT.nesting_slots;
    if exceeds {
        scratch.clear();
        scratch.trim_to(SCRATCH_RETENTION_LIMIT);
    }
}

/// SQL-obfuscates a value selected by a transform key.
///
/// A value whose obfuscation fails is still replaced by something other than itself, but carries the failure message
/// rather than `"?"`. The message tells the user to look for the query in the debug logs, which is why the query is
/// logged here.
fn transform_sql_value<'a>(value: &'a str, sql_config: &SqlObfuscationConfig) -> Result<Cow<'a, str>, Infallible> {
    match obfuscate_sql_string(value, sql_config) {
        // An empty result is a failure too, as in the reference implementation, which checks the obfuscated
        // output rather than the input: a query of only whitespace fails the same way.
        Ok(obfuscated) if !obfuscated.query.is_empty() => Ok(Cow::Owned(obfuscated.query)),
        Ok(_) => {
            debug!(query = value, "SQL obfuscation result was empty.");
            Ok(Cow::Borrowed(SQL_OBFUSCATION_FAILURE))
        }
        Err(error) => {
            debug!(%error, query = value, "Failed to obfuscate SQL string.");
            Ok(Cow::Borrowed(SQL_OBFUSCATION_FAILURE))
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

    /// Every expected value below is what the reference implementation's obfuscator returns for the same input,
    /// compared character for character. Tests with a comment saying otherwise are the exceptions.
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

    // TODO: unskip this test with DataDog/libdatadog#2577
    #[test]
    #[ignore = "the shared scanner copies this value instead of obfuscating it: DataDog/libdatadog#2577"]
    fn a_top_level_value_after_an_object_document_is_obfuscated() {
        assert_obfuscated(r#"{"a":1} "secret""#, &[], &[], r#"{"a":"?"} "?""#);
    }

    // TODO: unskip this test with DataDog/libdatadog#2577
    #[test]
    #[ignore = "the shared scanner copies this value instead of obfuscating it: DataDog/libdatadog#2577"]
    fn a_top_level_value_after_an_array_document_is_obfuscated() {
        assert_obfuscated(r#"[1,2] "secret""#, &[], &[], r#"["?","?"] "?""#);
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
    fn sql_value_that_obfuscates_to_empty_reports_the_failure() {
        // The reference implementation checks the obfuscated output, not the input, so an empty query and one of
        // only whitespace fail the same way.
        for input in [r#"{"query": ""}"#, r#"{"query": "\t"}"#] {
            assert_obfuscated(
                input,
                &[],
                &["query"],
                &format!(r#"{{"query":"{}"}}"#, SQL_OBFUSCATION_FAILURE),
            );
        }
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
    fn sql_value_with_json_only_escapes_is_still_obfuscated() {
        // Deliberately not what the reference implementation returns. It unescapes a SQL value with Go's
        // `strconv.Unquote`, which is Go string syntax rather than JSON: it rejects `\/` and surrogate pairs,
        // falls back to the raw literal with its quotes, and the SQL tokenizer then reads the whole value as one
        // quoted identifier, so the query passes through unobfuscated. The shared scanner unescapes with serde
        // instead, so a JSON-valid value always reaches the SQL tokenizer. That matters because `\/` is ordinary
        // production traffic: PHP's `json_encode` escapes every `/` as `\/` by default. Leaking every literal in
        // the value is worse than the resource string differing, so the shared behavior is kept.
        assert_obfuscated(
            r#"{"query": "select * from t where path = 'a\/b'"}"#,
            &[],
            &["query"],
            r#"{"query":"select * from t where path = ?"}"#,
        );

        // A surrogate pair is the other JSON escape `strconv.Unquote` rejects, which the reference implementation
        // then leaks as `ud83dude00`.
        assert_obfuscated(
            r#"{"query": "select 1 where x = '\ud83d\ude00'"}"#,
            &[],
            &["query"],
            r#"{"query":"select ? where x = ?"}"#,
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
    fn an_oversized_query_does_not_leave_its_buffers_behind() {
        // The output and scratch buffers keep the capacity they grow to, so an enormous query would otherwise stay
        // held for the obfuscator's lifetime. Whatever a pass grows past the retention limits is released before the
        // obfuscator is used again, and the final call checks it is still usable afterwards.
        let mut obfuscator = JsonObfuscator::new(&[], &keys(&["query"]), &default_sql_config());

        // The output buffer is reserved to the input length up front, so it grows past the limit even though the
        // obfuscated result is a single identifier.
        let oversized = format!(r#"{{"query": "{}"}}"#, "a".repeat(2 * OUTPUT_RETENTION_LIMIT_BYTES));
        let _ = obfuscator.obfuscate(&oversized);
        assert!(obfuscator.out.capacity() <= OUTPUT_RETENTION_LIMIT_BYTES);

        // A transform value with an escape sequence is unescaped into the scratch buffer, so a value past the
        // unescape limit grows that side of the scratch.
        let escaped = format!(
            r#"{{"query": "{}"}}"#,
            r"a\b ".repeat(OUTPUT_RETENTION_LIMIT_BYTES / 4 + 8)
        );
        let _ = obfuscator.obfuscate(&escaped);
        let retained = obfuscator.scratch.retained_capacity();
        assert!(retained.unescape_bytes > 0);
        assert!(retained.unescape_bytes <= SCRATCH_RETENTION_LIMIT.unescape_bytes);

        let nesting = SCRATCH_RETENTION_LIMIT.nesting_slots + 1;
        let deep = format!("{}1{}", "[".repeat(nesting), "]".repeat(nesting));
        let _ = obfuscator.obfuscate(&deep);
        let retained = obfuscator.scratch.retained_capacity();
        assert!(retained.nesting_slots <= SCRATCH_RETENTION_LIMIT.nesting_slots);
        assert!(retained.unescape_bytes <= SCRATCH_RETENTION_LIMIT.unescape_bytes);

        assert_eq!(obfuscator.obfuscate(r#"{"a":1}"#), r#"{"a":"?"}"#);
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
