//! Trace span generator.

use otlp_protos::opentelemetry::proto::{
    common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue},
    resource::v1::Resource,
    trace::v1::{status::StatusCode, ResourceSpans, ScopeSpans, Span, Status},
};
use rand::Rng;

use crate::config::TraceTemplate;
use crate::model::TraceContext;

/// Generator for OTLP trace spans.
pub struct TraceGenerator {
    service_name: String,
    template: TraceTemplate,
}

impl TraceGenerator {
    /// Creates a new trace generator for the given service.
    pub fn new(service_name: String, template: TraceTemplate) -> Self {
        Self { service_name, template }
    }

    /// Generates a span for this service.
    pub fn generate_span(&self, rng: &mut impl Rng, ctx: &TraceContext, duration_ns: u64) -> ResourceSpans {
        let operation = self.random_operation(rng);
        let is_error = rng.random::<f64>() < self.template.error_rate;

        let span = Span {
            trace_id: ctx.trace_id_bytes(),
            span_id: ctx.span_id_bytes(),
            parent_span_id: ctx.parent_span_id_bytes(),
            name: operation.to_string(),
            kind: self.template.span_kind.to_otlp(),
            start_time_unix_nano: ctx.start_time_ns,
            end_time_unix_nano: ctx.start_time_ns + duration_ns,
            status: Some(Status {
                code: if is_error {
                    StatusCode::Error as i32
                } else {
                    StatusCode::Ok as i32
                },
                message: if is_error {
                    "Simulated error".to_string()
                } else {
                    String::new()
                },
            }),
            attributes: self.generate_span_attributes(rng, operation, is_error),
            ..Default::default()
        };

        ResourceSpans {
            resource: Some(self.create_resource()),
            scope_spans: vec![ScopeSpans {
                scope: Some(self.create_scope()),
                spans: vec![span],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }
    }

    /// Returns the base duration in nanoseconds for spans from this template.
    pub fn base_duration_ns(&self) -> u64 {
        self.template.base_duration_ms * 1_000_000
    }

    fn random_operation(&self, rng: &mut impl Rng) -> &str {
        let idx = rng.random_range(0..self.template.operations.len());
        &self.template.operations[idx]
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

    fn generate_span_attributes(&self, rng: &mut impl Rng, operation: &str, is_error: bool) -> Vec<KeyValue> {
        let mut attrs = vec![KeyValue {
            key: "operation".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(operation.to_string())),
            }),
        }];

        // Add some random attributes based on span kind
        match self.template.span_kind {
            crate::config::SpanKind::Server | crate::config::SpanKind::Client => {
                attrs.push(KeyValue {
                    key: "http.status_code".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::IntValue(if is_error { 500 } else { 200 })),
                    }),
                });
                attrs.push(KeyValue {
                    key: "http.method".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue(
                            if operation.starts_with("GET") {
                                "GET"
                            } else if operation.starts_with("POST") {
                                "POST"
                            } else if operation.starts_with("PUT") {
                                "PUT"
                            } else if operation.starts_with("DELETE") {
                                "DELETE"
                            } else {
                                "GET"
                            }
                            .to_string(),
                        )),
                    }),
                });
            }
            crate::config::SpanKind::Producer | crate::config::SpanKind::Consumer => {
                attrs.push(KeyValue {
                    key: "messaging.system".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("kafka".to_string())),
                    }),
                });
                attrs.push(KeyValue {
                    key: "messaging.message.payload_size_bytes".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::IntValue(rng.random_range(100..10000))),
                    }),
                });
            }
            crate::config::SpanKind::Internal => {
                // Internal spans get fewer attributes
            }
        }

        attrs
    }
}
