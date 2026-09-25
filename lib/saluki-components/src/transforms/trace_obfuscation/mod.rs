//! Trace obfuscation transform.

mod credit_cards;
mod http;
mod json;
mod memcached;
mod obfuscator;
mod redis;
mod sql;
mod sql_filters;
mod sql_tokenizer;

use std::fmt::Write as _;

use agent_data_plane_config::domains;
use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::{
        trace::{AttributeValue, Span},
        Event,
    },
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;

use self::credit_cards::CREDIT_CARD_REPLACEMENT;
pub use self::obfuscator::{tags, ObfuscationConfig, Obfuscator};

const TEXT_NON_PARSABLE_SQL: &str = "Non-parsable SQL query";

/// Trace obfuscation configuration.
pub struct TraceObfuscationConfiguration {
    /// Obfuscator configuration.
    pub config: ObfuscationConfig,
}

impl TraceObfuscationConfiguration {
    /// Creates a new `TraceObfuscationConfiguration` from the resolved trace configuration.
    pub fn from_configuration(config: &domains::traces::Obfuscation) -> Self {
        Self { config: config.into() }
    }
}

#[async_trait]
impl SynchronousTransformBuilder for TraceObfuscationConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(TraceObfuscation {
            obfuscator: Obfuscator::new(self.config.clone()),
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
    obfuscator: Obfuscator,
}

impl TraceObfuscation {
    fn obfuscate_span(&mut self, span: &mut Span) {
        if self.obfuscator.config.credit_cards.enabled {
            self.obfuscate_credit_cards_in_span(span);
            self.obfuscate_credit_cards_in_span_events(span);
        }

        match span.span_type() {
            "http" | "web" => self.obfuscate_http_span(span),
            "sql" | "cassandra" => self.obfuscate_sql_span(span),
            "redis" | "valkey" => self.obfuscate_redis_span(span),
            "memcached" => self.obfuscate_memcached_span(span),
            "mongodb" => self.obfuscate_mongodb_span(span),
            "elasticsearch" | "opensearch" => self.obfuscate_elasticsearch_span(span),
            _ => {}
        }
    }

    fn obfuscate_credit_cards_in_span(&mut self, span: &mut Span) {
        for (key, value) in span.attributes.iter_mut() {
            if let AttributeValue::String(str_val) = value {
                if let Some(replacement) = self
                    .obfuscator
                    .obfuscate_credit_card_number(key.as_ref(), str_val.as_ref())
                {
                    *str_val = replacement;
                }
            }
        }
    }

    /// Scrubs card numbers from the attributes of every event attached to `span`.
    ///
    /// Event attributes are typed, so a card number can arrive as a string, as a number, or as an
    /// element of an array. Numbers are compared as their string form.
    fn obfuscate_credit_cards_in_span_events(&self, span: &mut Span) {
        // Formatting buffer for numeric attributes, reused across all events of the span. An empty
        // `String` does not allocate, so a span whose events carry no numeric attribute pays nothing.
        let mut formatted = String::new();

        for event in span.span_events_mut() {
            for (key, value) in event.attributes_mut().iter_mut() {
                if !self.obfuscator.should_obfuscate_credit_card_key(key.as_ref()) {
                    continue;
                }

                match value {
                    AttributeValue::Array(elements) => {
                        for element in elements.iter_mut() {
                            self.obfuscate_credit_card_attribute(element, &mut formatted);
                        }
                    }
                    value => self.obfuscate_credit_card_attribute(value, &mut formatted),
                }
            }
        }
    }

    /// Replaces `value` with `?` when it holds a card number, using `formatted` as scratch space.
    ///
    /// Only string, integer and floating-point attributes can hold a card number. Booleans cannot,
    /// and the remaining variants have no counterpart in the trace payloads this mirrors, so they
    /// are left alone. Nested arrays are left alone for the same reason.
    fn obfuscate_credit_card_attribute(&self, value: &mut AttributeValue, formatted: &mut String) {
        let is_card = match value {
            AttributeValue::String(str_val) => self.obfuscator.is_credit_card_number(str_val.as_ref()),
            AttributeValue::Int(int_val) => {
                formatted.clear();
                let _ = write!(formatted, "{}", int_val);
                self.obfuscator.is_credit_card_number(formatted)
            }
            AttributeValue::Float(float_val) => {
                formatted.clear();
                let _ = write!(formatted, "{}", float_val);
                self.obfuscator.is_credit_card_number(formatted)
            }
            _ => false,
        };

        if is_card {
            *value = AttributeValue::String(CREDIT_CARD_REPLACEMENT.into());
        }
    }

    fn obfuscate_http_span(&mut self, span: &mut Span) {
        // Every URL goes through the algorithm. Screening with a cheap byte scan first would save the work on URLs
        // with nothing to redact, but no scan we have says whether the algorithm changes a URL, and the one upstream
        // offers misses the URLs it redacts wholesale. See `http::obfuscate_url`.
        let obfuscated = match span.attributes.get(tags::HTTP_URL).and_then(AttributeValue::as_string) {
            Some(url) if !url.is_empty() => self.obfuscator.obfuscate_url(url),
            _ => return,
        };

        if let Some(obfuscated) = obfuscated {
            span.attributes
                .insert(tags::HTTP_URL.into(), AttributeValue::String(obfuscated));
        }
    }

    fn obfuscate_sql_span(&mut self, span: &mut Span) {
        let sql_query_owned: Option<String> = span
            .attributes
            .get(tags::DB_STATEMENT)
            .and_then(AttributeValue::as_string)
            .filter(|s| !s.is_empty())
            .map(|s| s.as_ref().to_owned());
        let sql_query: &str = match &sql_query_owned {
            Some(s) => s.as_str(),
            None => span.resource(),
        };

        if sql_query.is_empty() {
            return;
        }

        let dbms_owned: Option<String> = span
            .attributes
            .get(tags::DBMS)
            .and_then(AttributeValue::as_string)
            .filter(|s| !s.is_empty())
            .map(|s| s.as_ref().to_owned());

        let config = match &dbms_owned {
            Some(d) => self.obfuscator.config.sql.with_dbms(d.clone()),
            None => self.obfuscator.config.sql.clone(),
        };

        match sql::obfuscate_sql_string(sql_query, &config) {
            Ok(obfuscated) => {
                let query: MetaString = obfuscated.query.into();

                span.set_resource(query.clone());
                span.attributes
                    .insert(tags::SQL_QUERY.into(), AttributeValue::String(query.clone()));

                if span.attributes.contains_key(tags::DB_STATEMENT) {
                    span.attributes
                        .insert(tags::DB_STATEMENT.into(), AttributeValue::String(query));
                }

                if !obfuscated.table_names.is_empty() {
                    span.attributes.insert(
                        "sql.tables".into(),
                        AttributeValue::String(obfuscated.table_names.into()),
                    );
                }
            }
            Err(_) => {
                let non_parsable: MetaString = TEXT_NON_PARSABLE_SQL.into();
                span.set_resource(non_parsable.clone());
                span.attributes
                    .insert(tags::SQL_QUERY.into(), AttributeValue::String(non_parsable));
            }
        }
    }

    fn obfuscate_redis_span(&mut self, span: &mut Span) {
        let resource = span.resource();
        if resource.is_empty() {
            return;
        }

        if let Some(quantized) = self.obfuscator.quantize_redis_string(resource) {
            span.set_resource(quantized.to_string());
        }

        if span.span_type() == "redis" && self.obfuscator.config.redis.enabled {
            if let Some(cmd_value) = span
                .attributes
                .get(tags::REDIS_RAW_COMMAND)
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref().to_owned())
            {
                if let Some(obfuscated) = self.obfuscator.obfuscate_redis_string(&cmd_value) {
                    span.attributes
                        .insert(tags::REDIS_RAW_COMMAND.into(), AttributeValue::String(obfuscated));
                }
            }
        }

        if span.span_type() == "valkey" && self.obfuscator.config.valkey.enabled {
            if let Some(cmd_value) = span
                .attributes
                .get(tags::VALKEY_RAW_COMMAND)
                .and_then(AttributeValue::as_string)
                .map(|s| s.as_ref().to_owned())
            {
                if let Some(obfuscated) = self.obfuscator.obfuscate_valkey_string(&cmd_value) {
                    span.attributes
                        .insert(tags::VALKEY_RAW_COMMAND.into(), AttributeValue::String(obfuscated));
                }
            }
        }
    }

    fn obfuscate_memcached_span(&mut self, span: &mut Span) {
        if !self.obfuscator.config.memcached.enabled {
            return;
        }

        let cmd_value = match span
            .attributes
            .get(tags::MEMCACHED_COMMAND)
            .and_then(AttributeValue::as_string)
        {
            Some(v) if !v.is_empty() => v.as_ref().to_owned(),
            _ => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_memcached_command(&cmd_value) {
            if obfuscated.is_empty() {
                span.attributes.remove(tags::MEMCACHED_COMMAND);
            } else {
                span.attributes
                    .insert(tags::MEMCACHED_COMMAND.into(), AttributeValue::String(obfuscated));
            }
        }
    }

    fn obfuscate_mongodb_span(&mut self, span: &mut Span) {
        let query_value = match span
            .attributes
            .get(tags::MONGODB_QUERY)
            .and_then(AttributeValue::as_string)
        {
            Some(v) => v.as_ref().to_owned(),
            None => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_mongodb_string(&query_value) {
            span.attributes
                .insert(tags::MONGODB_QUERY.into(), AttributeValue::String(obfuscated));
        }
    }

    fn obfuscate_elasticsearch_span(&mut self, span: &mut Span) {
        if let Some(body_value) = span
            .attributes
            .get(tags::ELASTIC_BODY)
            .and_then(AttributeValue::as_string)
            .map(|s| s.as_ref().to_owned())
        {
            if let Some(obfuscated) = self.obfuscator.obfuscate_elasticsearch_string(&body_value) {
                span.attributes
                    .insert(tags::ELASTIC_BODY.into(), AttributeValue::String(obfuscated));
            }
        }

        if let Some(body_value) = span
            .attributes
            .get(tags::OPENSEARCH_BODY)
            .and_then(AttributeValue::as_string)
            .map(|s| s.as_ref().to_owned())
        {
            if let Some(obfuscated) = self.obfuscator.obfuscate_opensearch_string(&body_value) {
                span.attributes
                    .insert(tags::OPENSEARCH_BODY.into(), AttributeValue::String(obfuscated));
            }
        }
    }
}

impl SynchronousTransform for TraceObfuscation {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        for event in buffer {
            if let Event::Trace(ref mut trace) = event {
                for span in trace.spans_mut() {
                    self.obfuscate_span(span);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::SpanEvent;

    use super::*;
    use crate::common::datadog::obfuscation::CreditCardObfuscationConfig;

    const CARD: &str = "4111111111111111";

    fn obfuscation(keep_values: Vec<String>) -> TraceObfuscation {
        let config = ObfuscationConfig {
            credit_cards: CreditCardObfuscationConfig {
                enabled: true,
                luhn: false,
                keep_values,
            },
            ..Default::default()
        };

        TraceObfuscation {
            obfuscator: Obfuscator::new(config),
        }
    }

    fn http_span(url: &str) -> Span {
        let mut span = Span::new("svc", "http.request", "GET /x", "http", 1, 0, 0, 0, 0);
        span.attributes
            .insert(tags::HTTP_URL.into(), AttributeValue::String(url.into()));
        span
    }

    fn transform(remove_query_string: bool, remove_paths_with_digits: bool) -> TraceObfuscation {
        let mut config = ObfuscationConfig::default();
        config.http.remove_query_string = remove_query_string;
        config.http.remove_paths_with_digits = remove_paths_with_digits;

        TraceObfuscation {
            obfuscator: Obfuscator::new(config),
        }
    }

    fn obfuscated_url(url: &str, remove_query_string: bool, remove_paths_with_digits: bool) -> String {
        let mut span = http_span(url);
        transform(remove_query_string, remove_paths_with_digits).obfuscate_span(&mut span);

        span.attributes
            .get(tags::HTTP_URL)
            .and_then(AttributeValue::as_string)
            .map(|s| s.as_ref().to_owned())
            .expect("http.url was removed from the span")
    }

    // The reference implementation redacts a URL it cannot parse wholesale as soon as either option is on, so the span
    // path has to reach the algorithm for these URLs rather than screening them out first.
    #[test]
    fn unparseable_url_is_redacted_on_the_span() {
        for url in ["https://example.com:port/x", "http://foo:bar.com/x", ":"] {
            for (remove_query_string, remove_paths_with_digits) in [(true, false), (false, true), (true, true)] {
                assert_eq!(
                    obfuscated_url(url, remove_query_string, remove_paths_with_digits),
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

    fn span_with_event_attribute(key: &str, value: AttributeValue) -> Span {
        let mut attributes = FastHashMap::default();
        attributes.insert(MetaString::from(key), value);

        let event = SpanEvent::new(1, "exception").with_attributes(attributes);

        Span::new("svc", "op", "res", "custom", 1, 0, 1, 1, 0).with_span_events(vec![event])
    }

    fn event_attribute(span: &Span, key: &str) -> AttributeValue {
        span.span_events()[0]
            .attributes()
            .get(key)
            .expect("attribute should be present")
            .clone()
    }

    #[test]
    fn event_string_attribute_is_scrubbed() {
        let mut obfuscation = obfuscation(Vec::new());
        let mut span = span_with_event_attribute("payment.card", AttributeValue::String(CARD.into()));

        obfuscation.obfuscate_span(&mut span);

        assert_eq!(
            event_attribute(&span, "payment.card"),
            AttributeValue::String("?".into())
        );
    }

    #[test]
    fn event_numeric_attributes_are_compared_as_strings() {
        let mut obfuscation = obfuscation(Vec::new());

        let mut int_span = span_with_event_attribute("payment.card", AttributeValue::Int(4111111111111111));
        obfuscation.obfuscate_span(&mut int_span);
        assert_eq!(
            event_attribute(&int_span, "payment.card"),
            AttributeValue::String("?".into())
        );

        let mut float_span = span_with_event_attribute("payment.card", AttributeValue::Float(4111111111111111.0));
        obfuscation.obfuscate_span(&mut float_span);
        assert_eq!(
            event_attribute(&float_span, "payment.card"),
            AttributeValue::String("?".into())
        );

        // An integer that is too short to be a card number stays an integer.
        let mut short_span = span_with_event_attribute("payment.card", AttributeValue::Int(41111));
        obfuscation.obfuscate_span(&mut short_span);
        assert_eq!(event_attribute(&short_span, "payment.card"), AttributeValue::Int(41111));
    }

    #[test]
    fn event_array_attribute_is_scrubbed_element_by_element() {
        let mut obfuscation = obfuscation(Vec::new());
        let mut span = span_with_event_attribute(
            "payment.cards",
            AttributeValue::Array(vec![
                AttributeValue::String(CARD.into()),
                AttributeValue::String("not-a-card".into()),
                AttributeValue::Int(4111111111111111),
                AttributeValue::Bool(true),
            ]),
        );

        obfuscation.obfuscate_span(&mut span);

        assert_eq!(
            event_attribute(&span, "payment.cards"),
            AttributeValue::Array(vec![
                AttributeValue::String("?".into()),
                AttributeValue::String("not-a-card".into()),
                AttributeValue::String("?".into()),
                AttributeValue::Bool(true),
            ])
        );
    }

    #[test]
    fn event_attribute_keys_honor_the_allowlist() {
        let mut obfuscation = obfuscation(vec!["keep.me".to_string()]);

        for key in ["databricks_job_id", "_internal", "keep.me", "http.status_code"] {
            let mut span = span_with_event_attribute(key, AttributeValue::String(CARD.into()));
            obfuscation.obfuscate_span(&mut span);

            assert_eq!(
                event_attribute(&span, key),
                AttributeValue::String(CARD.into()),
                "attribute should be left alone: {}",
                key
            );
        }
    }

    #[test]
    fn event_attribute_with_more_than_sixteen_digits_is_left_alone() {
        let mut obfuscation = obfuscation(Vec::new());
        let job_run_id = "41111111111111111";
        let mut span = span_with_event_attribute("job.run", AttributeValue::String(job_run_id.into()));

        obfuscation.obfuscate_span(&mut span);

        assert_eq!(
            event_attribute(&span, "job.run"),
            AttributeValue::String(job_run_id.into())
        );
    }

    #[test]
    fn span_attributes_are_still_scrubbed() {
        let mut obfuscation = obfuscation(Vec::new());
        let mut span = Span::new("svc", "op", "res", "custom", 1, 0, 1, 1, 0);
        span.attributes
            .insert("payment.card".into(), AttributeValue::String(CARD.into()));
        span.attributes
            .insert("databricks_job_id".into(), AttributeValue::String(CARD.into()));

        obfuscation.obfuscate_span(&mut span);

        assert_eq!(
            span.attributes.get("payment.card"),
            Some(&AttributeValue::String("?".into()))
        );
        assert_eq!(
            span.attributes.get("databricks_job_id"),
            Some(&AttributeValue::String(CARD.into()))
        );
    }

    #[test]
    fn events_are_left_alone_when_credit_card_obfuscation_is_disabled() {
        let mut obfuscation = TraceObfuscation {
            obfuscator: Obfuscator::new(ObfuscationConfig::default()),
        };
        let mut span = span_with_event_attribute("payment.card", AttributeValue::String(CARD.into()));

        obfuscation.obfuscate_span(&mut span);

        assert_eq!(
            event_attribute(&span, "payment.card"),
            AttributeValue::String(CARD.into())
        );
    }
}
