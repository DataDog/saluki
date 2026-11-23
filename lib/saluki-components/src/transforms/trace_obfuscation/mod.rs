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
    data_model::event::trace::Trace,
    topology::EventsBuffer,
};
use saluki_error::GenericError;

/// Trace obfuscation configuration.
pub struct TraceObfuscationConfiguration {
    config: ObfuscationConfig,
}

impl TraceObfuscationConfiguration {
    /// Creates a new `TraceObfuscationConfiguration` from the obfuscation settings.
    ///
    /// This transform never applies tag replacement rules: `TraceTagReplacerConfiguration` owns them, so any rules in
    /// `config` are ignored.
    pub fn new(mut config: ObfuscationConfig) -> Self {
        config.tag_replace_rules.clear();
        Self { config }
    }
}

#[async_trait]
impl SynchronousTransformBuilder for TraceObfuscationConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(TraceObfuscation {
            config: self.config.clone(),
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
}

impl TraceObfuscation {
    fn obfuscate_trace(&self, trace: &mut Trace) {
        for span in trace.spans_mut() {
            obfuscate_span(span, &self.config);
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
        TraceObfuscationConfiguration::new(config).into_transform()
    }

    impl TraceObfuscationConfiguration {
        fn into_transform(self) -> TraceObfuscation {
            TraceObfuscation { config: self.config }
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
    fn unparseable_sql_is_replaced_instead_of_forwarded() {
        let span = Span::new("svc", "db.query", "#", "sql", 1, 0, 0, 0, 0);
        let span = obfuscate(ObfuscationConfig::default(), span);

        assert_eq!(span.resource(), "Non-parsable SQL query");
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
