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
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{
        trace::{AttributeValue, Span},
        Event,
    },
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;

pub use self::obfuscator::{tags, ObfuscationConfig, Obfuscator};

const TEXT_NON_PARSABLE_SQL: &str = "Non-parsable SQL query";

/// Trace obfuscation configuration.
pub struct TraceObfuscationConfiguration {
    /// Obfuscator configuration.
    pub config: ObfuscationConfig,
}

impl TraceObfuscationConfiguration {
    /// Creates a new `TraceObfuscationConfiguration` from its native configuration.
    pub fn from_native(config: &saluki_component_config::traces::TraceObfuscationConfig) -> Self {
        Self {
            config: ObfuscationConfig::from_native(&config.config),
        }
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
