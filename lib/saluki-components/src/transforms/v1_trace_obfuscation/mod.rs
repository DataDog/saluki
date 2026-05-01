//! V1 trace obfuscation transform.

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{
        trace::v1::{V1AnyValue, V1KeyValue, V1Span},
        Event,
    },
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;

use crate::common::datadog::apm::ApmConfig;
use crate::transforms::trace_obfuscation::{tags, ObfuscationConfig, Obfuscator};

const TEXT_NON_PARSABLE_SQL: &str = "Non-parsable SQL query";

/// V1 trace obfuscation configuration.
///
/// V1 counterpart to [`TraceObfuscationConfiguration`][super::trace_obfuscation::TraceObfuscationConfiguration],
/// operating on [`Event::V1Trace`] events whose span fields are [`MetaString`] and attributes are
/// stored as [`Vec<V1KeyValue>`] rather than the OTLP `Span` hashmaps.
pub struct V1TraceObfuscationConfiguration {
    config: ObfuscationConfig,
}

impl V1TraceObfuscationConfiguration {
    /// Creates a new `V1TraceObfuscationConfiguration` from the APM configuration section.
    pub fn from_apm_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        Ok(Self {
            config: apm_config.obfuscation().clone(),
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for V1TraceObfuscationConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(V1TraceObfuscation {
            obfuscator: Obfuscator::new(self.config.clone()),
        }))
    }
}

impl MemoryBounds for V1TraceObfuscationConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<V1TraceObfuscation>("component struct");
    }
}

/// The V1 obfuscation transform.
pub struct V1TraceObfuscation {
    obfuscator: Obfuscator,
}

impl V1TraceObfuscation {
    fn obfuscate_span(&mut self, span: &mut V1Span) {
        if self.obfuscator.config.credit_cards().enabled() {
            self.obfuscate_credit_cards_in_span(span);
        }

        match span.span_type.as_ref() {
            "http" | "web" => self.obfuscate_http_span(span),
            "sql" | "cassandra" => self.obfuscate_sql_span(span),
            "redis" | "valkey" => self.obfuscate_redis_span(span),
            "memcached" => self.obfuscate_memcached_span(span),
            "mongodb" => self.obfuscate_mongodb_span(span),
            "elasticsearch" | "opensearch" => self.obfuscate_elasticsearch_span(span),
            _ => {}
        }
    }

    fn obfuscate_credit_cards_in_span(&mut self, span: &mut V1Span) {
        for kv in &mut span.attributes {
            if let V1AnyValue::String(ref mut value) = kv.value {
                if let Some(replacement) = self
                    .obfuscator
                    .obfuscate_credit_card_number(kv.key.as_ref(), value.as_ref())
                {
                    *value = replacement;
                }
            }
        }
    }

    fn obfuscate_http_span(&mut self, span: &mut V1Span) {
        let url = get_string_attr(&span.attributes, tags::HTTP_URL)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());
        let url = match url {
            Some(u) => u,
            None => return,
        };
        if let Some(obfuscated) = self.obfuscator.obfuscate_url(&url) {
            set_string_attr(&mut span.attributes, tags::HTTP_URL.into(), obfuscated);
        }
    }

    fn obfuscate_sql_span(&mut self, span: &mut V1Span) {
        let db_stmt = get_string_attr(&span.attributes, tags::DB_STATEMENT)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());
        let resource_str = span.resource.as_ref().to_owned();
        let sql_query = db_stmt.as_deref().unwrap_or(&resource_str);

        if sql_query.is_empty() {
            return;
        }

        let dbms = get_string_attr(&span.attributes, tags::DBMS)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());

        match self.obfuscator.obfuscate_sql_string(sql_query, dbms.as_deref()) {
            Ok((obfuscated_query, table_names)) => {
                let query: MetaString = obfuscated_query.into();
                span.resource = query.clone();
                set_string_attr(&mut span.attributes, tags::SQL_QUERY.into(), query.clone());
                if db_stmt.is_some() {
                    set_string_attr(&mut span.attributes, tags::DB_STATEMENT.into(), query);
                }
                if !table_names.is_empty() {
                    set_string_attr(&mut span.attributes, "sql.tables".into(), table_names.into());
                }
            }
            Err(()) => {
                let non_parsable: MetaString = TEXT_NON_PARSABLE_SQL.into();
                span.resource = non_parsable.clone();
                set_string_attr(&mut span.attributes, tags::SQL_QUERY.into(), non_parsable);
            }
        }
    }

    fn obfuscate_redis_span(&mut self, span: &mut V1Span) {
        if span.resource.is_empty() {
            return;
        }
        let resource = span.resource.as_ref().to_owned();
        if let Some(quantized) = self.obfuscator.quantize_redis_string(&resource) {
            span.resource = MetaString::from(quantized.as_ref().to_owned());
        }

        if span.span_type.as_ref() == "redis" && self.obfuscator.config.redis().enabled() {
            let cmd = get_string_attr(&span.attributes, tags::REDIS_RAW_COMMAND).map(|s| s.to_owned());
            if let Some(cmd_value) = cmd {
                if let Some(obfuscated) = self.obfuscator.obfuscate_redis_string(&cmd_value) {
                    set_string_attr(&mut span.attributes, tags::REDIS_RAW_COMMAND.into(), obfuscated);
                }
            }
        }

        if span.span_type.as_ref() == "valkey" && self.obfuscator.config.valkey().enabled() {
            let cmd = get_string_attr(&span.attributes, tags::VALKEY_RAW_COMMAND).map(|s| s.to_owned());
            if let Some(cmd_value) = cmd {
                if let Some(obfuscated) = self.obfuscator.obfuscate_valkey_string(&cmd_value) {
                    set_string_attr(&mut span.attributes, tags::VALKEY_RAW_COMMAND.into(), obfuscated);
                }
            }
        }
    }

    fn obfuscate_memcached_span(&mut self, span: &mut V1Span) {
        if !self.obfuscator.config.memcached().enabled() {
            return;
        }

        let cmd = get_string_attr(&span.attributes, tags::MEMCACHED_COMMAND)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());
        let cmd_value = match cmd {
            Some(v) => v,
            None => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_memcached_command(&cmd_value) {
            if obfuscated.is_empty() {
                remove_attr(&mut span.attributes, tags::MEMCACHED_COMMAND);
            } else {
                set_string_attr(&mut span.attributes, tags::MEMCACHED_COMMAND.into(), obfuscated);
            }
        }
    }

    fn obfuscate_mongodb_span(&mut self, span: &mut V1Span) {
        let query = get_string_attr(&span.attributes, tags::MONGODB_QUERY).map(|s| s.to_owned());
        let query_value = match query {
            Some(v) => v,
            None => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_mongodb_string(&query_value) {
            set_string_attr(&mut span.attributes, tags::MONGODB_QUERY.into(), obfuscated);
        }
    }

    fn obfuscate_elasticsearch_span(&mut self, span: &mut V1Span) {
        let elastic_body = get_string_attr(&span.attributes, tags::ELASTIC_BODY).map(|s| s.to_owned());
        if let Some(body_value) = elastic_body {
            if let Some(obfuscated) = self.obfuscator.obfuscate_elasticsearch_string(&body_value) {
                set_string_attr(&mut span.attributes, tags::ELASTIC_BODY.into(), obfuscated);
            }
        }

        let opensearch_body = get_string_attr(&span.attributes, tags::OPENSEARCH_BODY).map(|s| s.to_owned());
        if let Some(body_value) = opensearch_body {
            if let Some(obfuscated) = self.obfuscator.obfuscate_opensearch_string(&body_value) {
                set_string_attr(&mut span.attributes, tags::OPENSEARCH_BODY.into(), obfuscated);
            }
        }
    }
}

impl SynchronousTransform for V1TraceObfuscation {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        for event in buffer {
            if let Event::V1Trace(ref mut trace) = event {
                for span in &mut trace.chunk.spans {
                    self.obfuscate_span(span);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::trace::v1::{V1AnyValue, V1KeyValue, V1Span};

    use crate::common::datadog::obfuscation::{
        CreditCardObfuscationConfig, ObfuscationConfig, RedisObfuscationConfig,
    };

    use super::*;

    fn make_span(span_type: &str, resource: &str, attrs: Vec<V1KeyValue>) -> V1Span {
        V1Span {
            service: MetaString::from("svc"),
            name: MetaString::from("op"),
            resource: MetaString::from(resource),
            span_id: 1,
            parent_id: 0,
            start: 0,
            duration: 0,
            error: false,
            attributes: attrs,
            span_type: MetaString::from(span_type),
            links: vec![],
            events: vec![],
            env: MetaString::default(),
            version: MetaString::default(),
            component: MetaString::default(),
            kind: 0,
        }
    }

    fn str_attr(key: &str, val: &str) -> V1KeyValue {
        V1KeyValue {
            key: MetaString::from(key),
            value: V1AnyValue::String(MetaString::from(val)),
        }
    }

    fn read_str(attrs: &[V1KeyValue], key: &str) -> Option<String> {
        attrs
            .iter()
            .find(|kv| kv.key.as_ref() == key)
            .and_then(|kv| match &kv.value {
                V1AnyValue::String(s) => Some(s.as_ref().to_owned()),
                _ => None,
            })
    }

    fn make_transform(config: ObfuscationConfig) -> V1TraceObfuscation {
        V1TraceObfuscation {
            obfuscator: Obfuscator::new(config),
        }
    }

    // ── SQL ──────────────────────────────────────────────────────────────────

    #[test]
    fn sql_span_resource_is_obfuscated_and_sql_query_attr_is_set() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("sql", "SELECT * FROM users WHERE id=42", vec![]);
        t.obfuscate_span(&mut span);

        assert!(
            span.resource.as_ref().contains('?'),
            "resource should contain '?': {}",
            span.resource.as_ref()
        );
        let query_attr = read_str(&span.attributes, "sql.query").expect("sql.query attr should be set");
        assert_eq!(query_attr, span.resource.as_ref(), "sql.query should equal obfuscated resource");
    }

    #[test]
    fn sql_span_db_statement_attr_is_preferred_and_updated() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span(
            "sql",
            "resource",
            vec![str_attr("db.statement", "SELECT name FROM accounts WHERE balance=1000")],
        );
        t.obfuscate_span(&mut span);

        let stmt = read_str(&span.attributes, "db.statement").expect("db.statement should still be present");
        assert!(stmt.contains('?'), "db.statement should be obfuscated: {}", stmt);
        assert!(!stmt.contains("1000"), "literal should be replaced in db.statement");
    }

    #[test]
    fn cassandra_span_resource_is_obfuscated() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("cassandra", "SELECT * FROM ks.table WHERE pk=1", vec![]);
        t.obfuscate_span(&mut span);
        assert!(span.resource.as_ref().contains('?'));
    }

    // ── HTTP ─────────────────────────────────────────────────────────────────

    #[test]
    fn http_span_userinfo_is_stripped_from_url() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span(
            "http",
            "",
            vec![str_attr("http.url", "http://user:pass@example.com/path")],
        );
        t.obfuscate_span(&mut span);

        let url = read_str(&span.attributes, "http.url").expect("http.url should be present");
        assert!(!url.contains("user:pass"), "userinfo should be stripped: {}", url);
        assert!(url.contains("example.com"), "host should remain: {}", url);
    }

    #[test]
    fn web_span_is_treated_same_as_http() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span(
            "web",
            "",
            vec![str_attr("http.url", "http://admin:secret@internal.svc/api")],
        );
        t.obfuscate_span(&mut span);

        let url = read_str(&span.attributes, "http.url").expect("http.url should be present");
        assert!(!url.contains("admin:secret"), "userinfo should be stripped: {}", url);
    }

    // ── Redis ─────────────────────────────────────────────────────────────────

    #[test]
    fn redis_span_resource_is_quantized_regardless_of_enabled_flag() {
        // Quantization always happens, even when redis obfuscation is disabled.
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("redis", "SET mykey myvalue", vec![]);
        t.obfuscate_span(&mut span);

        assert_eq!(span.resource.as_ref(), "SET", "resource should be quantized to command name only");
    }

    #[test]
    fn redis_raw_command_attr_is_obfuscated_when_redis_enabled() {
        let mut config = ObfuscationConfig::default();
        config.set_redis(RedisObfuscationConfig { enabled: true, remove_all_args: false });
        let mut t = make_transform(config);
        let mut span = make_span(
            "redis",
            "SET key value",
            vec![str_attr("redis.raw_command", "SET mykey supersecret")],
        );
        t.obfuscate_span(&mut span);

        let raw = read_str(&span.attributes, "redis.raw_command").expect("raw_command should be present");
        assert_eq!(raw, "SET mykey ?", "raw_command should be obfuscated");
    }

    #[test]
    fn redis_raw_command_attr_is_not_touched_when_redis_disabled() {
        // Default config has redis.enabled = false.
        let mut t = make_transform(ObfuscationConfig::default());
        let original = "SET mykey supersecret";
        let mut span = make_span("redis", "SET key value", vec![str_attr("redis.raw_command", original)]);
        t.obfuscate_span(&mut span);

        let raw = read_str(&span.attributes, "redis.raw_command").expect("raw_command should be present");
        assert_eq!(raw, original, "raw_command should not be modified when redis disabled");
    }

    // ── Credit cards ──────────────────────────────────────────────────────────

    #[test]
    fn credit_card_number_in_string_attribute_is_obfuscated() {
        let mut config = ObfuscationConfig::default();
        config.set_credit_cards(CreditCardObfuscationConfig {
            enabled: true,
            luhn: false,
            keep_values: vec![],
        });
        let mut t = make_transform(config);
        // Visa number that passes IIN + length checks without Luhn
        let mut span = make_span("web", "", vec![str_attr("payment.card", "4532123456789010")]);
        t.obfuscate_span(&mut span);

        let val = read_str(&span.attributes, "payment.card").expect("attribute should exist");
        assert_eq!(val, "?", "credit card number should be obfuscated to '?'");
    }

    #[test]
    fn allowlisted_key_is_not_obfuscated_for_credit_cards() {
        let mut config = ObfuscationConfig::default();
        config.set_credit_cards(CreditCardObfuscationConfig {
            enabled: true,
            luhn: false,
            keep_values: vec![],
        });
        let mut t = make_transform(config);
        let mut span = make_span("web", "", vec![str_attr("http.status_code", "4532123456789010")]);
        t.obfuscate_span(&mut span);

        let val = read_str(&span.attributes, "http.status_code").expect("attribute should exist");
        assert_eq!(val, "4532123456789010", "allowlisted key should not be obfuscated");
    }

    // ── Routing: unknown span type leaves span unchanged ─────────────────────

    #[test]
    fn unknown_span_type_is_not_modified() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("rpc", "some-resource", vec![str_attr("rpc.method", "GetUser")]);
        let original_resource = span.resource.clone();
        t.obfuscate_span(&mut span);

        assert_eq!(span.resource, original_resource, "resource should be unchanged for unknown span type");
        assert_eq!(
            read_str(&span.attributes, "rpc.method").as_deref(),
            Some("GetUser"),
            "attributes should be unchanged for unknown span type"
        );
    }
}

fn get_string_attr<'a>(attrs: &'a [V1KeyValue], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|kv| kv.key.as_ref() == key)
        .and_then(|kv| match &kv.value {
            V1AnyValue::String(s) => Some(s.as_ref()),
            _ => None,
        })
}

fn set_string_attr(attrs: &mut Vec<V1KeyValue>, key: MetaString, value: MetaString) {
    if let Some(kv) = attrs.iter_mut().find(|kv| kv.key == key) {
        kv.value = V1AnyValue::String(value);
    } else {
        attrs.push(V1KeyValue {
            key,
            value: V1AnyValue::String(value),
        });
    }
}

fn remove_attr(attrs: &mut Vec<V1KeyValue>, key: &str) {
    attrs.retain(|kv| kv.key.as_ref() != key);
}
