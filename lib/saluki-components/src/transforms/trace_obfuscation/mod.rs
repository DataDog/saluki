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

use async_trait::async_trait;
use facet::Facet;
use saluki_config::GenericConfiguration;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{
        trace::{AttributeValue, Span},
        Event,
    },
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use serde::Deserialize;
use stringtheory::MetaString;

pub use self::obfuscator::{tags, ObfuscationConfig, Obfuscator};
use crate::common::datadog::apm::ApmConfig;

const TEXT_NON_PARSABLE_SQL: &str = "Non-parsable SQL query";

/// Trace obfuscation configuration.
#[derive(Deserialize, Facet)]
#[cfg_attr(test, derive(Debug, PartialEq, serde::Serialize))]
pub struct TraceObfuscationConfiguration {
    /// Obfuscator configuration.
    #[serde(default)]
    pub config: ObfuscationConfig,
}

impl TraceObfuscationConfiguration {
    /// Creates a new `TraceObfuscationConfiguration` from the given generic configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Self::from_apm_configuration(config)
    }

    /// Creates a new `TraceObfuscationConfiguration` from the APM configuration section.
    pub fn from_apm_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let apm_config = ApmConfig::from_configuration(config)?;
        Ok(Self {
            config: apm_config.obfuscation().clone(),
        })
    }

    /// Creates a new `TraceObfuscationConfiguration` with default settings.
    pub fn new() -> Self {
        Self {
            config: ObfuscationConfig::default(),
        }
    }
}

impl Default for TraceObfuscationConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SynchronousTransformBuilder for TraceObfuscationConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
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

    fn obfuscate_http_span(&mut self, span: &mut Span) {
        let url_value = match span.attributes.get(tags::HTTP_URL).and_then(AttributeValue::as_string) {
            Some(v) if !v.is_empty() => v.as_ref().to_owned(),
            _ => return,
        };

        if let Some(obfuscated) = self.obfuscator.obfuscate_url(&url_value) {
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
mod config_smoke {
    use datadog_agent_config_testing::config_registry::structs;
    use datadog_agent_config_testing::run_config_smoke_tests;
    use serde_json::json;

    use super::TraceObfuscationConfiguration;
    use crate::config::{DatadogRemapper, KEY_ALIASES};

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(
            structs::TRACE_OBFUSCATION_CONFIGURATION,
            &[],
            json!({}),
            |cfg| {
                TraceObfuscationConfiguration::from_apm_configuration(&cfg)
                    .expect("TraceObfuscationConfiguration should deserialize")
            },
            KEY_ALIASES,
            DatadogRemapper::new,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use saluki_common::collections::FastHashMap;

    use super::*;

    fn span_with(span_type: &str, resource: &str, attrs: &[(&str, &str)]) -> Span {
        let mut attributes = FastHashMap::default();
        for (key, value) in attrs {
            attributes.insert(MetaString::from(*key), AttributeValue::String(MetaString::from(*value)));
        }
        Span::default()
            .with_span_type(span_type)
            .with_resource(resource)
            .with_attributes(attributes)
    }

    fn string_attr(span: &Span, key: &str) -> Option<String> {
        span.attributes
            .get(key)
            .and_then(AttributeValue::as_string)
            .map(|value| value.as_ref().to_string())
    }

    fn obfuscate(config: ObfuscationConfig, mut span: Span) -> Span {
        let mut transform = TraceObfuscation {
            obfuscator: Obfuscator::new(config),
        };
        transform.obfuscate_span(&mut span);
        span
    }

    #[test]
    fn redis_span_type_quantizes_resource_and_obfuscates_command() {
        let mut config = ObfuscationConfig::default();
        config.redis.enabled = true;
        let span = obfuscate(
            config,
            span_with("redis", "GET mykey", &[(tags::REDIS_RAW_COMMAND, "AUTH secret")]),
        );

        assert_eq!(span.resource(), "GET");
        assert_eq!(string_attr(&span, tags::REDIS_RAW_COMMAND).as_deref(), Some("AUTH ?"));
    }

    #[test]
    fn sql_span_type_obfuscates_resource_and_sets_sql_query() {
        let span = obfuscate(ObfuscationConfig::default(), span_with("sql", "SELECT 1", &[]));

        assert_eq!(span.resource(), "SELECT ?");
        assert_eq!(string_attr(&span, tags::SQL_QUERY).as_deref(), Some("SELECT ?"));
    }

    #[test]
    fn http_span_type_removes_url_userinfo() {
        let span = obfuscate(
            ObfuscationConfig::default(),
            span_with("web", "GET /", &[(tags::HTTP_URL, "http://user:pass@host/path")]),
        );

        let url = string_attr(&span, tags::HTTP_URL).expect("http.url should still be present");
        assert!(!url.contains("user:pass"), "userinfo must be stripped, got {url}");
        assert!(url.contains("host/path"), "host and path must be preserved, got {url}");
    }

    #[test]
    fn unknown_span_type_is_left_untouched() {
        // A span type that matches no obfuscation handler passes through with resource and attributes unchanged.
        let span = obfuscate(
            ObfuscationConfig::default(),
            span_with("custom", "SELECT 1", &[(tags::HTTP_URL, "http://user:pass@host/path")]),
        );

        assert_eq!(span.resource(), "SELECT 1");
        assert_eq!(
            string_attr(&span, tags::HTTP_URL).as_deref(),
            Some("http://user:pass@host/path"),
        );
    }

    #[test]
    fn credit_cards_are_obfuscated_regardless_of_span_type() {
        // Credit-card obfuscation runs before the span-type dispatch, so it applies even to an unrecognized span type.
        let mut config = ObfuscationConfig::default();
        config.credit_cards.enabled = true;
        let span = obfuscate(
            config,
            span_with("custom", "noop", &[("card_number", "4532123456789010")]),
        );

        assert_eq!(string_attr(&span, "card_number").as_deref(), Some("?"));
    }
}
