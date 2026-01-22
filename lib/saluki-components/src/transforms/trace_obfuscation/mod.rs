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
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::{trace::Span, Event, EventType},
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use tokio::select;
use tracing::error;

pub use self::obfuscator::{tags, ObfuscationConfig, Obfuscator};
use crate::common::datadog::apm::ApmConfig;

/// Trace obfuscation configuration.
#[derive(serde::Deserialize)]
pub struct TraceObfuscationConfiguration {
    /// Obfuscator configuration.
    #[serde(default)]
    pub config: ObfuscationConfig,
}

impl TraceObfuscationConfiguration {
    /// Creates a new `TraceObfuscationConfiguration` from the given generic configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
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
impl TransformBuilder for TraceObfuscationConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Trace
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Trace)];
        OUTPUTS
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
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
        let span_type = span.span_type().to_string();

        if self.obfuscator.config.credit_cards().enabled() {
            self.obfuscate_credit_cards_in_span(span);
        }

        match span_type.as_str() {
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
        let keys_to_update: Vec<_> = span.meta().keys().map(|k| k.to_string()).collect();

        for key in keys_to_update {
            if let Some(value) = span.meta().get(key.as_str()) {
                let value_str = value.to_string();
                let obfuscated = self.obfuscator.obfuscate_credit_card_number(&key, &value_str);

                if obfuscated != value_str {
                    span.meta_mut().insert(key.into(), obfuscated.into());
                }
            }
        }
    }

    fn obfuscate_http_span(&mut self, span: &mut Span) {
        let url_value = match span.meta_mut().get(tags::HTTP_URL) {
            Some(v) => v.to_string(),
            None => return,
        };

        if url_value.is_empty() {
            return;
        }

        let obfuscated = self.obfuscator.obfuscate_url(&url_value);
        span.meta_mut().insert(tags::HTTP_URL.into(), obfuscated.into());
    }

    fn obfuscate_sql_span(&mut self, span: &mut Span) {
        let sql_query = span
            .meta()
            .get(tags::DB_STATEMENT)
            .map(|v| v.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| span.resource().to_string());

        if sql_query.is_empty() {
            return;
        }

        let dbms = span.meta().get(tags::DBMS).map(|v| v.to_string()).unwrap_or_default();

        let config = if !dbms.is_empty() {
            self.obfuscator.config.sql().with_dbms(dbms.clone())
        } else {
            self.obfuscator.config.sql().clone()
        };

        match sql::obfuscate_sql_string(&sql_query, &config) {
            Ok(obfuscated) => {
                span.set_resource(obfuscated.query.clone());
                span.meta_mut()
                    .insert(tags::SQL_QUERY.into(), obfuscated.query.clone().into());

                if span.meta().contains_key(tags::DB_STATEMENT) {
                    span.meta_mut()
                        .insert(tags::DB_STATEMENT.into(), obfuscated.query.into());
                }

                if !obfuscated.table_names.is_empty() {
                    span.meta_mut()
                        .insert("sql.tables".into(), obfuscated.table_names.into());
                }
            }
            Err(_) => {
                const TEXT_NON_PARSABLE: &str = "Non-parsable SQL query";
                span.set_resource(TEXT_NON_PARSABLE.to_string());
                span.meta_mut().insert(tags::SQL_QUERY.into(), TEXT_NON_PARSABLE.into());
            }
        }
    }

    fn obfuscate_redis_span(&mut self, span: &mut Span) {
        let resource = span.resource().to_string();
        if resource.is_empty() {
            return;
        }

        let quantized = self.obfuscator.quantize_redis_string(&resource);
        span.set_resource(quantized);

        if span.span_type() == "redis" && self.obfuscator.config.redis().enabled() {
            if let Some(cmd_value) = span.meta_mut().get(tags::REDIS_RAW_COMMAND) {
                let cmd_str = cmd_value.to_string();
                let obfuscated = self.obfuscator.obfuscate_redis_string(&cmd_str);
                span.meta_mut()
                    .insert(tags::REDIS_RAW_COMMAND.into(), obfuscated.into());
            }
        }

        if span.span_type() == "valkey" && self.obfuscator.config.valkey().enabled() {
            if let Some(cmd_value) = span.meta_mut().get(tags::VALKEY_RAW_COMMAND) {
                let cmd_str = cmd_value.to_string();
                let obfuscated = self.obfuscator.obfuscate_valkey_string(&cmd_str);
                span.meta_mut()
                    .insert(tags::VALKEY_RAW_COMMAND.into(), obfuscated.into());
            }
        }
    }

    fn obfuscate_memcached_span(&mut self, span: &mut Span) {
        if !self.obfuscator.config.memcached().enabled() {
            return;
        }

        let cmd_value = match span.meta_mut().get(tags::MEMCACHED_COMMAND) {
            Some(v) => v.to_string(),
            None => return,
        };

        if cmd_value.is_empty() {
            return;
        }

        let obfuscated = self.obfuscator.obfuscate_memcached_command(&cmd_value);

        if obfuscated.is_empty() {
            span.meta_mut().remove(tags::MEMCACHED_COMMAND);
        } else {
            span.meta_mut()
                .insert(tags::MEMCACHED_COMMAND.into(), obfuscated.into());
        }
    }

    fn obfuscate_mongodb_span(&mut self, span: &mut Span) {
        let query_value = match span.meta().get(tags::MONGODB_QUERY) {
            Some(v) => v.to_string(),
            None => return,
        };

        let obfuscated = self.obfuscator.obfuscate_mongodb_string(&query_value);
        span.meta_mut().insert(tags::MONGODB_QUERY.into(), obfuscated.into());
    }

    fn obfuscate_elasticsearch_span(&mut self, span: &mut Span) {
        if let Some(body_value) = span.meta().get(tags::ELASTIC_BODY) {
            let body_str = body_value.to_string();
            let obfuscated = self.obfuscator.obfuscate_elasticsearch_string(&body_str);
            span.meta_mut().insert(tags::ELASTIC_BODY.into(), obfuscated.into());
        }

        if let Some(body_value) = span.meta().get(tags::OPENSEARCH_BODY) {
            let body_str = body_value.to_string();
            let obfuscated = self.obfuscator.obfuscate_opensearch_string(&body_str);
            span.meta_mut().insert(tags::OPENSEARCH_BODY.into(), obfuscated.into());
        }
    }
}

#[async_trait]
impl Transform for TraceObfuscation {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut events) => {
                        for event in &mut events {
                            if let Event::Trace(ref mut trace) = event {
                                for span in trace.spans_mut() {
                                    self.obfuscate_span(span);
                                }
                            }
                        }

                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    },
                    None => break,
                }
            }
        }

        Ok(())
    }
}
