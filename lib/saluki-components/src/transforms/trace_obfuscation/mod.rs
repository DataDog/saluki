//! Trace obfuscation transform.
//!
//! Obfuscates the span data that can carry sensitive values: SQL queries, Redis, Valkey and Memcached commands, HTTP
//! URLs, JSON query bodies, and credit card numbers. The obfuscation itself comes from a shared crate that works on
//! Saluki's trace types through their trait implementations, so this transform only applies it to each trace.
//!
//! Tag replacement isn't applied here. It runs in the `trace_tag_replacer` transform, so that other steps can run
//! between the two.

use async_trait::async_trait;
use libdd_trace_obfuscation::{
    obfuscate::{obfuscate_chunk_attributes, obfuscate_span},
    obfuscation_config::ObfuscationConfig,
};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{transforms::*, BuildContext},
    data_model::event::trace::{AttributeValue, AttributeValueExt, Trace},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;

/// Trace obfuscation configuration.
pub struct TraceObfuscationConfiguration {
    config: ObfuscationConfig,
    sql_dialect: MetaString,
}

impl TraceObfuscationConfiguration {
    /// Creates a new `TraceObfuscationConfiguration` from the obfuscation settings.
    ///
    /// This transform never applies tag replacement rules: `TraceTagReplacerConfiguration` owns them, so any rules in
    /// `config` are ignored. `sql_dialect` supplies the dialect when a span has no per-span hint.
    pub fn new(mut config: ObfuscationConfig, sql_dialect: String) -> Self {
        config.tag_replace_rules.clear();
        Self {
            config,
            sql_dialect: sql_dialect.into(),
        }
    }
}

#[async_trait]
impl SynchronousTransformBuilder for TraceObfuscationConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(TraceObfuscation {
            config: self.config.clone(),
            sql_dialect: self.sql_dialect.clone(),
        }))
    }
}

impl MemoryBounds for TraceObfuscationConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<TraceObfuscation>("component struct");
    }
}

/// The obfuscation transform that processes traces.
pub struct TraceObfuscation {
    config: ObfuscationConfig,
    sql_dialect: MetaString,
}

impl TraceObfuscation {
    fn obfuscate_trace(&self, trace: &mut Trace) {
        for span in trace.spans_mut() {
            // The shared SQL obfuscator reads `db.type`. Supply the OTLP hint or configured fallback only for this call.
            let dialect = if matches!(span.span_type(), "sql" | "cassandra") && !span.attributes.contains_key("db.type")
            {
                span.attributes
                    .get("db.system")
                    .and_then(AttributeValue::as_string)
                    .filter(|hint| !hint.is_empty())
                    .cloned()
                    .or_else(|| (!self.sql_dialect.is_empty()).then(|| self.sql_dialect.clone()))
            } else {
                None
            };
            let temporary_hint = dialect.is_some();
            if let Some(dialect) = dialect {
                span.attributes
                    .insert("db.type".into(), AttributeValue::String(dialect));
            }
            obfuscate_span(span, &self.config);
            if temporary_hint {
                span.attributes.remove("db.type");
            }
        }

        // Attributes shared by every span of the trace are scrubbed too, since they are merged into each span when the
        // trace is encoded.
        obfuscate_chunk_attributes(trace, &self.config);
    }
}

impl SynchronousTransform for TraceObfuscation {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        for trace in buffer.events_mut::<Trace>() {
            self.obfuscate_trace(trace);
        }
    }
}

#[cfg(test)]
mod tests {
    use libdd_trace_obfuscation::{
        json::JsonObfuscator,
        obfuscation_config::{HttpConfig, JsonObfuscatorConfig},
        replacer::parse_rules_from_string,
    };
    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::{AttributeValue, AttributeValueExt, Span, SpanEvent, TraceIngestInfo};

    use super::*;

    fn transform(config: ObfuscationConfig) -> TraceObfuscation {
        TraceObfuscationConfiguration::new(config, String::new()).into_transform()
    }

    impl TraceObfuscationConfiguration {
        fn into_transform(self) -> TraceObfuscation {
            TraceObfuscation {
                config: self.config,
                sql_dialect: self.sql_dialect,
            }
        }
    }

    fn attributes(pairs: &[(&str, &str)]) -> FastHashMap<stringtheory::MetaString, AttributeValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), AttributeValue::String((*v).into())))
            .collect()
    }

    /// Runs `span` through the transform as the only span of a trace.
    fn obfuscate(config: ObfuscationConfig, span: Span) -> Span {
        let mut trace = Trace::new(vec![span], TraceIngestInfo::Datadog);
        transform(config).obfuscate_trace(&mut trace);
        trace
            .spans()
            .first()
            .cloned()
            .expect("the span was removed from the trace")
    }

    fn attribute<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
        span.attributes
            .get(key)
            .and_then(AttributeValue::as_string)
            .map(|s| &**s)
    }

    #[test]
    fn sql_resource_is_obfuscated() {
        let span = Span::new(
            "svc",
            "db.query",
            "SELECT * FROM users WHERE id = 42",
            "sql",
            1,
            0,
            0,
            0,
            0,
        );
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(span.resource(), "SELECT * FROM users WHERE id = ?");
    }

    #[test]
    fn sql_dialect_prefers_native_then_otlp_then_configured_hints() {
        for (pairs, fallback, expected) in [
            (vec![], "postgresql", "SELECT doc #>> ? FROM t"),
            (vec![("db.system", "postgresql")], "mysql", "SELECT doc #>> ? FROM t"),
            (
                vec![("db.type", "postgresql"), ("db.system", "mysql")],
                "mysql",
                "SELECT doc #>> ? FROM t",
            ),
            (vec![("db.type", "mysql")], "postgresql", "SELECT doc"),
        ] {
            let span = Span::new("svc", "query", "SELECT doc #>> '{a,b}' FROM t", "sql", 1, 0, 0, 0, 0)
                .with_attributes(attributes(&pairs));
            let original = span.attributes.clone();
            let mut trace = Trace::new(vec![span], TraceIngestInfo::Datadog);
            TraceObfuscationConfiguration::new(ObfuscationConfig::default(), fallback.into())
                .into_transform()
                .obfuscate_trace(&mut trace);
            assert_eq!(trace.spans()[0].resource(), expected, "{pairs:?}, fallback {fallback}");
            for key in ["db.type", "db.system"] {
                assert_eq!(
                    trace.spans()[0].attributes.get(key),
                    original.get(key),
                    "dialect tag {key}"
                );
            }
        }
    }

    #[test]
    fn unparseable_sql_is_replaced_instead_of_forwarded() {
        let span = Span::new("svc", "db.query", "#", "sql", 1, 0, 0, 0, 0);
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(span.resource(), "Non-parsable SQL query");
    }

    #[test]
    fn sql_resource_with_leading_hash_comment_is_obfuscated() {
        // A statement can start with a `#` line comment, and a bare `#` must not make the obfuscator panic. The Agent
        // skips such a comment to the line break when the DBMS is not PostgreSQL: its tokenizer calls `scanCommentType1`
        // in the default branch of `'#'`, and that function reads to the line break. A real run of the Agent gives
        // `SELECT ?` for these statements, as this test does.
        // https://github.com/DataDog/datadog-agent/blob/92824ec3d3f4a4c5b0926452999a39df5b4eb850/pkg/obfuscate/sql_tokenizer.go#L390-L416
        // https://github.com/DataDog/datadog-agent/blob/92824ec3d3f4a4c5b0926452999a39df5b4eb850/pkg/obfuscate/sql_tokenizer.go#L842-L850
        for statement in ["#c\nSELECT 1", " #c\nSELECT 1"] {
            let span = Span::new("svc", "db.query", statement, "sql", 1, 0, 0, 0, 0)
                .with_attributes(attributes(&[("db.system", "mysql")]));
            let span = obfuscate(ObfuscationConfig::default(), span);

            assert_eq!(span.resource(), "SELECT ?", "{statement:?}");
        }

        // The Agent tokenizes `#` as an operator character when the DBMS is PostgreSQL, in the same `'#'` case as above. A
        // real run of the Agent gives `SELECT ?` for all three statements, and ADP must give the same resource, not
        // `Non-parsable SQL query`.
        let span = Span::new("svc", "db.query", "#c\nSELECT 2", "sql", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("db.system", "postgresql")]));
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(span.resource(), "SELECT ?");
    }

    #[test]
    fn redis_resource_keeps_the_first_three_commands() {
        // At most three commands are kept, and a cut is marked with ` ...`.
        let span = Span::new("svc", "redis.command", "SET k v\nGET k\nDEL k", "redis", 1, 0, 0, 0, 0);
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(span.resource(), "SET GET DEL ...");
    }

    #[test]
    fn redis_raw_command_arguments_are_obfuscated() {
        let span = Span::new("svc", "redis.command", "SET", "redis", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("redis.raw_command", "SET mykey secret-value")]));
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(attribute(&span, "redis.raw_command"), Some("SET mykey ?"));
    }

    #[test]
    fn memcached_command_keeps_only_the_command_when_configured() {
        let mut config = ObfuscationConfig::default();
        config.memcached.keep_command = true;
        let span = Span::new("svc", "memcached.cmd", "set", "memcached", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("memcached.command", "set mykey 0 60 5\r\nvalue")]));
        let span = obfuscate(config, span);

        assert_eq!(attribute(&span, "memcached.command"), Some("set mykey 0 60 5"));
    }

    fn http_span(url: &str) -> Span {
        Span::new("svc", "http.request", "GET /x", "http", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("http.url", url)]))
    }

    fn obfuscated_url(url: &str, remove_query_string: bool, remove_path_digits: bool) -> String {
        let config = ObfuscationConfig {
            http: HttpConfig {
                remove_query_string,
                remove_path_digits,
            },
            ..Default::default()
        };
        let span = obfuscate(config, http_span(url));
        attribute(&span, "http.url")
            .expect("http.url was removed from the span")
            .to_owned()
    }

    // A removed query string leaves a trailing `?`.
    #[test]
    fn http_url_query_string_and_path_digits_are_removed_when_configured() {
        assert_eq!(
            obfuscated_url("https://example.com/users/123/profile?token=abc", true, true),
            "https://example.com/users/?/profile?"
        );
    }

    // A URL that can't be parsed is redacted wholesale as soon as either option is on.
    #[test]
    fn unparseable_url_is_redacted_when_either_option_is_on() {
        for url in ["https://example.com:port/x", "http://foo:bar.com/x", ":"] {
            for (remove_query_string, remove_path_digits) in [(true, false), (false, true), (true, true)] {
                assert_eq!(
                    obfuscated_url(url, remove_query_string, remove_path_digits),
                    "?",
                    "expected wholesale redaction for {url:?}"
                );
            }
        }
    }

    // With both options off the algorithm only strips userinfo, and an unparseable URL keeps everything else.
    #[test]
    fn unparseable_url_is_kept_when_both_options_are_off() {
        assert_eq!(
            obfuscated_url("https://example.com:port/x", false, false),
            "https://example.com:port/x"
        );
    }

    #[test]
    fn credit_card_numbers_are_scrubbed_from_span_and_span_event_attributes() {
        let event = SpanEvent::new(1, "exception").with_attributes(attributes(&[("card", "4111 1111 1111 1111")]));
        let span = Span::new("svc", "op", "res", "custom", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[
                ("card", "4111 1111 1111 1111"),
                ("note", "hello"),
                // Known not to hold card numbers, although the value looks like one.
                ("databricks_job_id", "4111111111111111"),
            ]))
            .with_span_events(vec![event]);
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(attribute(&span, "card"), Some("?"));
        assert_eq!(attribute(&span, "note"), Some("hello"));
        assert_eq!(attribute(&span, "databricks_job_id"), Some("4111111111111111"));
        let event_card = span.span_events()[0]
            .attributes()
            .get("card")
            .and_then(AttributeValue::as_string);
        assert_eq!(event_card.map(|s| &**s), Some("?"));
    }

    #[test]
    fn credit_card_numbers_are_scrubbed_from_trace_attributes() {
        let mut trace = Trace::new(vec![Span::default()], TraceIngestInfo::Datadog);
        std::sync::Arc::make_mut(&mut trace.attributes)
            .insert("card".into(), AttributeValue::String("4111111111111111".into()));
        transform(ObfuscationConfig::default()).obfuscate_trace(&mut trace);

        let card = trace.attributes.get("card").and_then(AttributeValue::as_string);
        assert_eq!(card.map(|s| &**s), Some("?"));
    }

    #[test]
    fn credit_card_keep_values_are_not_scrubbed() {
        let mut config = ObfuscationConfig::default();
        config.credit_cards.keep_values.insert("order_ref".to_owned());
        let span = Span::new("svc", "op", "res", "custom", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("order_ref", "4111111111111111")]));
        let span = obfuscate(config, span);

        assert_eq!(attribute(&span, "order_ref"), Some("4111111111111111"));
    }

    #[test]
    fn elasticsearch_body_values_are_obfuscated() {
        let span = Span::new("svc", "es.query", "search", "elasticsearch", 1, 0, 0, 0, 0).with_attributes(attributes(
            &[("elasticsearch.body", r#"{"query":{"match":{"user":"alice"}}}"#)],
        ));
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(
            attribute(&span, "elasticsearch.body"),
            Some(r#"{"query":{"match":{"user":"?"}}}"#)
        );
    }

    // Replacement rules are applied by `trace_tag_replacer`, so this transform must leave them alone.
    #[test]
    fn tag_replacement_rules_are_not_applied_by_this_transform() {
        let rules = parse_rules_from_string(r#"[{"name":"secret","pattern":".*","repl":"?"}]"#).unwrap();
        let config = ObfuscationConfig {
            tag_replace_rules: rules,
            ..Default::default()
        };
        let span = Span::new("svc", "op", "res", "custom", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("secret", "hunter2")]));
        let span = obfuscate(config, span);

        assert_eq!(attribute(&span, "secret"), Some("hunter2"));
    }

    // The following tests record expected behavior that isn't implemented yet. They assert that behavior, so they fail
    // today and are ignored. Remove the `ignore` once they pass.

    // The raw query attributes are obfuscated along with the resource.
    #[test]
    #[ignore = "not implemented yet: db.statement and db.query.text are left unobfuscated (DataDog/libdatadog#2923)"]
    fn sql_query_attributes_are_obfuscated_with_the_resource() {
        let query = "SELECT * FROM users WHERE id = 42";
        let span = Span::new("svc", "db.query", query, "sql", 1, 0, 0, 0, 0)
            .with_attributes(attributes(&[("db.statement", query), ("db.query.text", query)]));
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(
            attribute(&span, "db.statement"),
            Some("SELECT * FROM users WHERE id = ?")
        );
        assert_eq!(
            attribute(&span, "db.query.text"),
            Some("SELECT * FROM users WHERE id = ?")
        );
        assert_eq!(attribute(&span, "sql.query"), None, "sql.query is not added");
    }

    // The tables a query uses are reported in `sql.tables`.
    #[test]
    #[ignore = "not implemented yet: table names aren't extracted (DataDog/libdatadog#2922)"]
    fn sql_table_names_are_reported() {
        let span = Span::new(
            "svc",
            "db.query",
            "SELECT * FROM users JOIN orders ON users.id = 1",
            "sql",
            1,
            0,
            0,
            0,
            0,
        );
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(attribute(&span, "sql.tables"), Some("users,orders"));
    }

    // Values under `obfuscate_sql_values` keys are obfuscated as SQL. The span-level obfuscation replaces the whole value
    // with `?` instead.
    #[test]
    #[ignore = "not implemented yet: JSON values under transform keys aren't obfuscated as SQL"]
    fn json_values_under_transform_keys_are_obfuscated_as_sql() {
        let config = ObfuscationConfig {
            elasticsearch: JsonObfuscator::new(JsonObfuscatorConfig {
                enabled: true,
                keep_keys: Default::default(),
                transform_keys: ["query".to_owned()].into_iter().collect(),
            }),
            ..Default::default()
        };
        let span = Span::new("svc", "es.query", "search", "elasticsearch", 1, 0, 0, 0, 0).with_attributes(attributes(
            &[("elasticsearch.body", r#"{"query":"SELECT * FROM t WHERE id = 7"}"#)],
        ));
        let span = obfuscate(config, span);

        assert_eq!(
            attribute(&span, "elasticsearch.body"),
            Some(r#"{"query":"SELECT * FROM t WHERE id = ?"}"#)
        );
    }
}
