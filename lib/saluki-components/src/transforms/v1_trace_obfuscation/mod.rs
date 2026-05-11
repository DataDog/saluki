//! V1 trace obfuscation transform.

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{trace::Span, Event},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;
use tracing::debug;

use crate::common::datadog::apm::ApmConfig;
use crate::transforms::trace_obfuscation::{tags, ObfuscationConfig, Obfuscator};

const TEXT_NON_PARSABLE_SQL: &str = "Non-parsable SQL query";

/// V1 trace obfuscation configuration.
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
    fn obfuscate_span(&mut self, span: &mut Span) {
        if self.obfuscator.config.credit_cards().enabled() {
            self.obfuscate_credit_cards_in_span(span);
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
        let keys: Vec<MetaString> = span.meta().keys().cloned().collect();
        for key in keys {
            let current = span.meta().get(&key).cloned();
            if let Some(value) = current {
                if let Some(replacement) = self.obfuscator.obfuscate_credit_card_number(key.as_ref(), value.as_ref()) {
                    span.meta_mut().insert(key, replacement);
                }
            }
        }
    }

    fn obfuscate_http_span(&mut self, span: &mut Span) {
        let url = span.meta().get(tags::HTTP_URL).filter(|s| !s.is_empty()).map(|s| s.as_ref().to_owned());
        let url = match url {
            Some(u) => u,
            None => return,
        };
        if let Some(obfuscated) = self.obfuscator.obfuscate_url(&url) {
            span.meta_mut().insert(MetaString::from(tags::HTTP_URL), obfuscated);
        }
    }

    fn obfuscate_sql_span(&mut self, span: &mut Span) {
        let db_stmt = span.meta().get(tags::DB_STATEMENT).filter(|s| !s.is_empty()).map(|s| s.as_ref().to_owned());
        let resource_str = span.resource().to_owned();
        let sql_query = db_stmt.as_deref().unwrap_or(&resource_str);

        if sql_query.is_empty() {
            return;
        }

        let dbms = span.meta().get(tags::DBMS).filter(|s| !s.is_empty()).map(|s| s.as_ref().to_owned());

        match self.obfuscator.obfuscate_sql_string(sql_query, dbms.as_deref()) {
            Ok((obfuscated_query, table_names)) => {
                let query: MetaString = obfuscated_query.into();
                span.set_resource(query.clone());
                span.meta_mut().insert(MetaString::from(tags::SQL_QUERY), query.clone());
                if db_stmt.is_some() {
                    span.meta_mut().insert(MetaString::from(tags::DB_STATEMENT), query);
                }
                if !table_names.is_empty() {
                    span.meta_mut().insert(MetaString::from("sql.tables"), table_names.into());
                }
            }
            Err(()) => {
                let non_parsable: MetaString = TEXT_NON_PARSABLE_SQL.into();
                span.set_resource(non_parsable.clone());
                span.meta_mut().insert(MetaString::from(tags::SQL_QUERY), non_parsable);
            }
        }
    }

    fn obfuscate_redis_span(&mut self, span: &mut Span) {
        if span.resource().is_empty() {
            return;
        }
        let resource = span.resource().to_owned();
        if let Some(quantized) = self.obfuscator.quantize_redis_string(&resource) {
            span.set_resource(MetaString::from(quantized.as_ref().to_owned()));
        }

        if span.span_type() == "redis" && self.obfuscator.config.redis().enabled() {
            let cmd = span.meta().get(tags::REDIS_RAW_COMMAND).map(|s| s.as_ref().to_owned());
            if let Some(cmd_value) = cmd {
                if let Some(obfuscated) = self.obfuscator.obfuscate_redis_string(&cmd_value) {
                    span.meta_mut().insert(MetaString::from(tags::REDIS_RAW_COMMAND), obfuscated);
                }
            }
        }

        if span.span_type() == "valkey" && self.obfuscator.config.valkey().enabled() {
            let cmd = span.meta().get(tags::VALKEY_RAW_COMMAND).map(|s| s.as_ref().to_owned());
            if let Some(cmd_value) = cmd {
                if let Some(obfuscated) = self.obfuscator.obfuscate_valkey_string(&cmd_value) {
                    span.meta_mut().insert(MetaString::from(tags::VALKEY_RAW_COMMAND), obfuscated);
                }
            }
        }
    }

    fn obfuscate_memcached_span(&mut self, span: &mut Span) {
        if !self.obfuscator.config.memcached().enabled() {
            return;
        }

        let cmd = span.meta().get(tags::MEMCACHED_COMMAND).filter(|s| !s.is_empty()).map(|s| s.as_ref().to_owned());
        let cmd_value = match cmd {
            Some(v) => v,
            None => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_memcached_command(&cmd_value) {
            if obfuscated.is_empty() {
                span.meta_mut().remove(tags::MEMCACHED_COMMAND);
            } else {
                span.meta_mut().insert(MetaString::from(tags::MEMCACHED_COMMAND), obfuscated);
            }
        }
    }

    fn obfuscate_mongodb_span(&mut self, span: &mut Span) {
        let query = span.meta().get(tags::MONGODB_QUERY).map(|s| s.as_ref().to_owned());
        let query_value = match query {
            Some(v) => v,
            None => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_mongodb_string(&query_value) {
            span.meta_mut().insert(MetaString::from(tags::MONGODB_QUERY), obfuscated);
        }
    }

    fn obfuscate_elasticsearch_span(&mut self, span: &mut Span) {
        let elastic_body = span.meta().get(tags::ELASTIC_BODY).map(|s| s.as_ref().to_owned());
        if let Some(body_value) = elastic_body {
            if let Some(obfuscated) = self.obfuscator.obfuscate_elasticsearch_string(&body_value) {
                span.meta_mut().insert(MetaString::from(tags::ELASTIC_BODY), obfuscated);
            }
        }

        let opensearch_body = span.meta().get(tags::OPENSEARCH_BODY).map(|s| s.as_ref().to_owned());
        if let Some(body_value) = opensearch_body {
            if let Some(obfuscated) = self.obfuscator.obfuscate_opensearch_string(&body_value) {
                span.meta_mut().insert(MetaString::from(tags::OPENSEARCH_BODY), obfuscated);
            }
        }
    }
}

impl SynchronousTransform for V1TraceObfuscation {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        let mut count = 0u32;
        for event in buffer {
            if let Event::Trace(ref mut trace) = event {
                count += 1;
                for span in trace.spans_mut() {
                    self.obfuscate_span(span);
                }
            }
        }
        if count > 0 {
            debug!(traces = count, "V1 trace obfuscation processed buffer.");
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::Span;
    use stringtheory::MetaString;

    use crate::common::datadog::obfuscation::{
        CreditCardObfuscationConfig, ObfuscationConfig, RedisObfuscationConfig,
    };

    use super::*;

    fn make_span(span_type: &str, resource: &str, meta: FastHashMap<MetaString, MetaString>) -> Span {
        Span::new("svc", "op", resource, span_type, 0, 1, 0, 0, 0, 0).with_meta(Some(meta))
    }

    fn str_meta(pairs: &[(&str, &str)]) -> FastHashMap<MetaString, MetaString> {
        pairs
            .iter()
            .map(|(k, v)| (MetaString::from(*k), MetaString::from(*v)))
            .collect()
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
        let mut span = make_span("sql", "SELECT * FROM users WHERE id=42", FastHashMap::default());
        t.obfuscate_span(&mut span);

        assert!(
            span.resource().contains('?'),
            "resource should contain '?': {}",
            span.resource()
        );
        let query_attr = span.meta().get("sql.query").expect("sql.query attr should be set");
        assert_eq!(query_attr.as_ref(), span.resource(), "sql.query should equal obfuscated resource");
    }

    #[test]
    fn sql_span_db_statement_attr_is_preferred_and_updated() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span(
            "sql",
            "resource",
            str_meta(&[("db.statement", "SELECT name FROM accounts WHERE balance=1000")]),
        );
        t.obfuscate_span(&mut span);

        let stmt = span.meta().get("db.statement").expect("db.statement should still be present");
        assert!(stmt.as_ref().contains('?'), "db.statement should be obfuscated: {}", stmt.as_ref());
        assert!(!stmt.as_ref().contains("1000"), "literal should be replaced in db.statement");
    }

    #[test]
    fn cassandra_span_resource_is_obfuscated() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("cassandra", "SELECT * FROM ks.table WHERE pk=1", FastHashMap::default());
        t.obfuscate_span(&mut span);
        assert!(span.resource().contains('?'));
    }

    // ── HTTP ─────────────────────────────────────────────────────────────────

    #[test]
    fn http_span_userinfo_is_stripped_from_url() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("http", "", str_meta(&[("http.url", "http://user:pass@example.com/path")]));
        t.obfuscate_span(&mut span);

        let url = span.meta().get("http.url").expect("http.url should be present");
        assert!(!url.as_ref().contains("user:pass"), "userinfo should be stripped: {}", url.as_ref());
        assert!(url.as_ref().contains("example.com"), "host should remain: {}", url.as_ref());
    }

    #[test]
    fn web_span_is_treated_same_as_http() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span(
            "web",
            "",
            str_meta(&[("http.url", "http://admin:secret@internal.svc/api")]),
        );
        t.obfuscate_span(&mut span);

        let url = span.meta().get("http.url").expect("http.url should be present");
        assert!(!url.as_ref().contains("admin:secret"), "userinfo should be stripped: {}", url.as_ref());
    }

    // ── Redis ─────────────────────────────────────────────────────────────────

    #[test]
    fn redis_span_resource_is_quantized_regardless_of_enabled_flag() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("redis", "SET mykey myvalue", FastHashMap::default());
        t.obfuscate_span(&mut span);

        assert_eq!(span.resource(), "SET", "resource should be quantized to command name only");
    }

    #[test]
    fn redis_raw_command_attr_is_obfuscated_when_redis_enabled() {
        let mut config = ObfuscationConfig::default();
        config.set_redis(RedisObfuscationConfig { enabled: true, remove_all_args: false });
        let mut t = make_transform(config);
        let mut span = make_span(
            "redis",
            "SET key value",
            str_meta(&[("redis.raw_command", "SET mykey supersecret")]),
        );
        t.obfuscate_span(&mut span);

        let raw = span.meta().get("redis.raw_command").expect("raw_command should be present");
        assert_eq!(raw.as_ref(), "SET mykey ?", "raw_command should be obfuscated");
    }

    #[test]
    fn redis_raw_command_attr_is_not_touched_when_redis_disabled() {
        let mut t = make_transform(ObfuscationConfig::default());
        let original = "SET mykey supersecret";
        let mut span = make_span("redis", "SET key value", str_meta(&[("redis.raw_command", original)]));
        t.obfuscate_span(&mut span);

        let raw = span.meta().get("redis.raw_command").expect("raw_command should be present");
        assert_eq!(raw.as_ref(), original, "raw_command should not be modified when redis disabled");
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
        let mut span = make_span("web", "", str_meta(&[("payment.card", "4532123456789010")]));
        t.obfuscate_span(&mut span);

        let val = span.meta().get("payment.card").expect("attribute should exist");
        assert_eq!(val.as_ref(), "?", "credit card number should be obfuscated to '?'");
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
        let mut span = make_span("web", "", str_meta(&[("http.status_code", "4532123456789010")]));
        t.obfuscate_span(&mut span);

        let val = span.meta().get("http.status_code").expect("attribute should exist");
        assert_eq!(val.as_ref(), "4532123456789010", "allowlisted key should not be obfuscated");
    }

    // ── Routing: unknown span type leaves span unchanged ─────────────────────

    #[test]
    fn unknown_span_type_is_not_modified() {
        let mut t = make_transform(ObfuscationConfig::default());
        let mut span = make_span("rpc", "some-resource", str_meta(&[("rpc.method", "GetUser")]));
        let original_resource = span.resource().to_owned();
        t.obfuscate_span(&mut span);

        assert_eq!(span.resource(), original_resource.as_str(), "resource should be unchanged for unknown span type");
        assert_eq!(
            span.meta().get("rpc.method").map(|s| s.as_ref()),
            Some("GetUser"),
            "attributes should be unchanged for unknown span type"
        );
    }
}
