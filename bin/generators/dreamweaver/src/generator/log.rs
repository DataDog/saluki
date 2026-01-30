//! Log generator.

use otlp_protos::opentelemetry::proto::{
    common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use rand::Rng;

use crate::config::LogTemplate;
use crate::model::trace_context::now_ns;
use crate::model::TraceContext;

/// Generator for OTLP logs.
pub struct LogGenerator {
    service_name: String,
    templates: Vec<LogTemplate>,
}

impl LogGenerator {
    /// Creates a new log generator for the given service.
    pub fn new(service_name: String, templates: Vec<LogTemplate>) -> Self {
        Self {
            service_name,
            templates,
        }
    }

    /// Generates logs for this service, optionally correlated with a trace.
    pub fn generate_logs(&self, rng: &mut impl Rng, ctx: Option<&TraceContext>) -> ResourceLogs {
        let now = now_ns();
        let records: Vec<LogRecord> = self
            .templates
            .iter()
            .map(|template| self.generate_log_record(rng, template, ctx, now))
            .collect();

        ResourceLogs {
            resource: Some(self.create_resource()),
            scope_logs: vec![ScopeLogs {
                scope: Some(self.create_scope()),
                log_records: records,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }
    }

    fn generate_log_record(
        &self, rng: &mut impl Rng, template: &LogTemplate, ctx: Option<&TraceContext>, now: u64,
    ) -> LogRecord {
        let pattern = &template.patterns[rng.random_range(0..template.patterns.len())];
        let body = self.interpolate_pattern(rng, pattern);

        LogRecord {
            time_unix_nano: now,
            observed_time_unix_nano: now,
            severity_number: template.severity.to_otlp_number(),
            severity_text: template.severity.to_text().to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(body)),
            }),
            attributes: self.generate_log_attributes(rng),
            dropped_attributes_count: 0,
            flags: 0,
            trace_id: ctx.map(|c| c.trace_id_bytes()).unwrap_or_default(),
            span_id: ctx.map(|c| c.span_id_bytes()).unwrap_or_default(),
            event_name: String::new(),
        }
    }

    fn interpolate_pattern(&self, rng: &mut impl Rng, pattern: &str) -> String {
        let mut result = pattern.to_string();

        // Replace common placeholders with realistic values
        if result.contains("{ip}") {
            let ip = format!(
                "{}.{}.{}.{}",
                rng.random_range(1u8..255),
                rng.random_range(0u8..255),
                rng.random_range(0u8..255),
                rng.random_range(1u8..255)
            );
            result = result.replace("{ip}", &ip);
        }

        if result.contains("{duration}") {
            let duration = rng.random_range(1..500);
            result = result.replace("{duration}", &duration.to_string());
        }

        if result.contains("{error}") {
            let errors = [
                "Connection timeout",
                "Service unavailable",
                "Internal server error",
                "Database connection failed",
                "Rate limit exceeded",
            ];
            let error = errors[rng.random_range(0..errors.len())];
            result = result.replace("{error}", error);
        }

        if result.contains("{user_id}") {
            let user_id = rng.random_range(1000..9999);
            result = result.replace("{user_id}", &user_id.to_string());
        }

        if result.contains("{request_id}") {
            let request_id: u64 = rng.random();
            result = result.replace("{request_id}", &format!("{:016x}", request_id));
        }

        result
    }

    fn generate_log_attributes(&self, rng: &mut impl Rng) -> Vec<KeyValue> {
        vec![
            KeyValue {
                key: "thread.id".to_string(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::IntValue(rng.random_range(1..100))),
                }),
            },
            KeyValue {
                key: "code.function".to_string(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::StringValue("handle_request".to_string())),
                }),
            },
        ]
    }

    fn create_resource(&self) -> Resource {
        Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_string(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::StringValue(self.service_name.clone())),
                }),
            }],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        }
    }

    fn create_scope(&self) -> InstrumentationScope {
        InstrumentationScope {
            name: "dreamweaver".to_string(),
            version: "1.0.0".to_string(),
            attributes: vec![],
            dropped_attributes_count: 0,
        }
    }
}
