//! OTTL Transform processor component.
//!
//! Executes OTTL transformation statements against trace spans, following the
//! [OpenTelemetry Transform processor] spec. Currently supports the `set` editor function
//! for span-level attributes (`attributes["key"]`), with read access to resource attributes
//! (`resource.attributes["key"]`) for use in conditions.
//!
//! [OpenTelemetry Transform processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor

use async_trait::async_trait;
use ottl::{CallbackMap, EnumMap, OttlParser};
use saluki_common::collections::FastHashMap;
use saluki_config::GenericConfiguration;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::{AttributeValue, Span},
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tracing::{debug, error};

mod config;
use self::config::{ErrorMode, OttlTransformConfig};

mod span_context;
use self::span_context::{SpanTransformContext, SpanTransformFamily};

/// Configuration for the OTTL Transform processor.
#[derive(Clone, Debug)]
pub struct OttlTransformConfiguration {
    config: OttlTransformConfig,
}

impl OttlTransformConfiguration {
    /// Creates an `OttlTransformConfiguration` from the given configuration.
    ///
    /// Reads the OTTL Transform config from the `ottl_transform_config` key at the top level of the data-plane
    /// configuration.
    ///
    /// # Errors
    ///
    /// If a value at `ottl_transform_config` exists but fails to deserialize, an error is returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let transform_config = config.try_get_typed::<OttlTransformConfig>("ottl_transform_config")?;
        Ok(Self {
            config: transform_config.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for OttlTransformConfiguration {
    /// Builds the OTTL `Transform` from the current configuration.
    ///
    /// Registers the `set` editor function, parses each trace statement, and returns the transform. Statements may be
    /// editor calls like `set(attributes["key"], "value")` with optional `where` clauses.
    ///
    /// # Errors
    ///
    /// If the OTTL statement fails to parse, an error is returned.
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let path_resolvers = span_context::span_transform_path_resolvers();

        let editors = ottl::editors::standard();
        let converters = CallbackMap::new();
        let enums = EnumMap::new();

        let mut span_parsers = Vec::new();
        for statement in &self.config.trace_statements {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            let parser = ottl::Parser::new(&editors, &converters, &enums, &path_resolvers, statement);

            debug!("Registered OTTL transform statement: \"{}\"", statement);

            parser
                .is_error()
                .map_err(|e| generic_error!("OTTL transform statement parse error: {}: {}", statement, e))?;
            span_parsers.push(parser);
        }

        Ok(Box::new(OttlTransform {
            error_mode: self.config.error_mode,
            span_parsers,
        }))
    }
}

impl MemoryBounds for OttlTransformConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<OttlTransform>("component struct");
    }
}

/// Synchronous transform that applies OTTL statements to each span in a trace.
pub struct OttlTransform {
    error_mode: ErrorMode,
    span_parsers: Vec<ottl::Parser<SpanTransformFamily>>,
}

impl OttlTransform {
    /// Applies all configured OTTL statements to a single span.
    ///
    /// Each statement is executed in order. For editor statements (for example, `set`), the `where`
    /// clause is evaluated first; if it matches (or is absent), the editor function runs.
    /// Errors are handled according to `error_mode`.
    fn transform_span(&self, span: &mut Span, resource_attrs: &FastHashMap<MetaString, AttributeValue>) {
        let mut ctx = SpanTransformContext::new(span, resource_attrs);

        for parser in &self.span_parsers {
            match parser.execute(&mut ctx) {
                Ok(_) => {}
                Err(e) => match self.error_mode {
                    ErrorMode::Ignore => {
                        error!(error = %e, "OTTL transform statement error; ignoring");
                    }
                    ErrorMode::Silent => {}
                    ErrorMode::Propagate => {
                        // The OTel spec drops the entire payload on propagate errors,
                        // but the SynchronousTransform API does not support error propagation.
                        // We log and stop processing further statements for this span.
                        error!(error = %e, "OTTL transform statement error; stopping span processing (error_mode=propagate)");
                        return;
                    }
                },
            }
        }
    }
}

impl SynchronousTransform for OttlTransform {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        if self.span_parsers.is_empty() {
            return;
        }

        for event in event_buffer {
            if let Some(trace) = event.try_as_trace_mut() {
                let resource_attrs = std::sync::Arc::clone(&trace.attributes);
                for span in trace.spans_mut() {
                    self.transform_span(span, &resource_attrs);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_config::ConfigurationLoader;
    use saluki_core::{
        components::{transforms::*, ComponentContext},
        data_model::event::{
            service_check::{CheckStatus, ServiceCheck},
            trace::AttributeValue,
            Event,
        },
        topology::EventsBuffer,
    };
    use stringtheory::MetaString;

    use super::*;
    use crate::components::test_support::{make_span, make_trace};

    // ---- Helpers ----

    fn get_span_attr(buffer: &EventsBuffer, span_index: usize, key: &str) -> Option<String> {
        buffer
            .into_iter()
            .filter_map(|e| match e {
                Event::Trace(t) => Some(t.spans()),
                _ => None,
            })
            .flat_map(|spans| spans.iter())
            .nth(span_index)
            .and_then(|span| {
                let k = MetaString::from(key);
                span.attributes.get(&k).and_then(|v| match v {
                    AttributeValue::String(s) => Some(s.as_ref().to_string()),
                    _ => None,
                })
            })
    }

    fn test_component_context() -> ComponentContext {
        ComponentContext::test_transform("ottl_transform")
    }

    async fn build_transform(cfg_json: Option<serde_json::Value>) -> Box<dyn SynchronousTransform + Send> {
        let (config, _) = ConfigurationLoader::for_tests(cfg_json, None, false).await;
        let ottl_config = OttlTransformConfiguration::from_configuration(&config).expect("config should parse");
        let ctx = test_component_context();
        ottl_config.build(ctx).await.expect("build should succeed")
    }

    // ---- Group 1: Configuration and build ----

    #[tokio::test]
    async fn from_configuration_absent_key_returns_default() {
        let mut transform = build_transform(None).await;
        let span = make_span(1, 1, HashMap::from([("a".into(), "b".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "a").as_deref(),
            Some("b"),
            "default config must not modify spans"
        );
    }

    #[tokio::test]
    async fn from_configuration_invalid_yaml_returns_error() {
        let invalid = serde_json::json!({
            "ottl_transform_config": {
                "unknown_field": 1
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(invalid), None, false).await;
        let result = OttlTransformConfiguration::from_configuration(&config);
        assert!(result.is_err(), "unknown fields must cause deserialization error");
    }

    #[tokio::test]
    async fn build_invalid_statement_returns_error() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["syntax error !!"]
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlTransformConfiguration::from_configuration(&config).expect("config is valid");
        let ctx = test_component_context();
        let result = ottl_config.build(ctx).await;
        assert!(result.is_err(), "invalid OTTL syntax must make build fail");
    }

    #[tokio::test]
    async fn build_empty_and_whitespace_statements_skipped() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["", "   ", "\t"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("a".into(), "b".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "a").as_deref(),
            Some("b"),
            "empty/whitespace statements should be skipped; span unchanged"
        );
    }

    // ---- Group 2: Core set functionality ----

    #[tokio::test]
    async fn set_stores_each_literal_value_type_as_a_string() {
        // `set` creates a new span attribute and stores every scalar literal type as a string. One case per
        // supported OTTL literal type; the string case also covers new-attribute creation on an empty span.
        struct Case {
            name: &'static str,
            // The literal exactly as written in the OTTL statement (the string case keeps its quotes).
            literal: &'static str,
            expected: &'static str,
        }

        let cases = [
            Case {
                name: "string",
                literal: "\"newval\"",
                expected: "newval",
            },
            Case {
                name: "int",
                literal: "42",
                expected: "42",
            },
            Case {
                name: "float",
                literal: "6.14",
                expected: "6.14",
            },
            Case {
                name: "bool",
                literal: "true",
                expected: "true",
            },
        ];

        for case in cases {
            let statement = format!("set(attributes[\"k\"], {})", case.literal);
            let cfg_json = serde_json::json!({
                "ottl_transform_config": { "trace_statements": [statement] }
            });
            let mut transform = build_transform(Some(cfg_json)).await;
            let span = make_span(1, 1, HashMap::new());
            let trace = make_trace(vec![span], None);
            let mut buffer = EventsBuffer::default();
            assert!(buffer.try_push(Event::Trace(trace)).is_none());
            transform.transform_buffer(&mut buffer);
            assert_eq!(
                get_span_attr(&buffer, 0, "k").as_deref(),
                Some(case.expected),
                "{}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn set_overwrite_existing_attribute() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"key\"], \"new\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("key".into(), "old".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "key").as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn set_nil_removes_attribute() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"gone\"], attributes[\"nonexistent\"])"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("gone".into(), "here".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "gone"),
            None,
            "setting to a non-existent attribute (Nil) should remove the key"
        );
    }

    #[tokio::test]
    async fn set_from_another_attribute() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"dst\"], attributes[\"src\"])"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("src".into(), "hello".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "dst").as_deref(), Some("hello"));
    }

    // ---- Group 3: Where-clause conditions ----

    #[tokio::test]
    async fn set_with_where_clause_matching() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"x\"], \"v\") where attributes[\"env\"] == \"prod\""]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("env".into(), "prod".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "x").as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn set_with_where_clause_not_matching() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"x\"], \"v\") where attributes[\"env\"] == \"prod\""]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("env".into(), "staging".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "x"),
            None,
            "where clause did not match; attribute should not be set"
        );
    }

    #[tokio::test]
    async fn set_with_where_clause_on_resource_attributes() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": [
                    "set(attributes[\"x\"], \"v\") where resource.attributes[\"host.name\"] == \"localhost\""
                ]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], Some(vec!["host.name:localhost"]));
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "x").as_deref(), Some("v"));
    }

    // ---- Group 4: Multiple statements and ordering ----

    #[tokio::test]
    async fn multiple_statements_execute_in_order() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": [
                    "set(attributes[\"a\"], \"1\")",
                    "set(attributes[\"b\"], \"2\")"
                ]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "a").as_deref(), Some("1"));
        assert_eq!(get_span_attr(&buffer, 0, "b").as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn later_statement_sees_earlier_mutation() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": [
                    "set(attributes[\"a\"], \"1\")",
                    "set(attributes[\"b\"], attributes[\"a\"])"
                ]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "b").as_deref(),
            Some("1"),
            "second statement should read value set by the first"
        );
    }

    #[tokio::test]
    async fn where_clause_sees_earlier_mutation() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": [
                    "set(attributes[\"env\"], \"prod\")",
                    "set(attributes[\"x\"], \"1\") where attributes[\"env\"] == \"prod\""
                ]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "x").as_deref(),
            Some("1"),
            "where clause should see attribute set by the earlier statement"
        );
    }

    // ---- Group 5: Error modes ----

    #[tokio::test]
    async fn error_mode_controls_processing_after_a_statement_error() {
        // After a statement errors, `error_mode` decides whether later statements still run: ignore/silent
        // continue, propagate stops processing the span. One case per `ErrorMode` arm. The first statement
        // (setting a read-only resource attribute) always errors.
        struct Case {
            error_mode: &'static str,
            later_statement_runs: bool,
        }

        let cases = [
            Case {
                error_mode: "ignore",
                later_statement_runs: true,
            },
            Case {
                error_mode: "silent",
                later_statement_runs: true,
            },
            Case {
                error_mode: "propagate",
                later_statement_runs: false,
            },
        ];

        for case in cases {
            let cfg_json = serde_json::json!({
                "ottl_transform_config": {
                    "error_mode": case.error_mode,
                    "trace_statements": [
                        "set(resource.attributes[\"x\"], \"fail\")",
                        "set(attributes[\"after\"], \"yes\")"
                    ]
                }
            });
            let mut transform = build_transform(Some(cfg_json)).await;
            let span = make_span(1, 1, HashMap::new());
            let trace = make_trace(vec![span], None);
            let mut buffer = EventsBuffer::default();
            assert!(buffer.try_push(Event::Trace(trace)).is_none());
            transform.transform_buffer(&mut buffer);

            let after = get_span_attr(&buffer, 0, "after");
            if case.later_statement_runs {
                assert_eq!(
                    after.as_deref(),
                    Some("yes"),
                    "error_mode={}: later statement should still execute",
                    case.error_mode
                );
            } else {
                assert_eq!(
                    after, None,
                    "error_mode={}: later statement should be skipped",
                    case.error_mode
                );
            }
        }
    }

    #[tokio::test]
    async fn omitted_error_mode_defaults_to_propagate() {
        // Per the transform processor spec, an omitted `error_mode` defaults to `Propagate`. Behaviorally,
        // that means a statement error stops the rest of the span's statements (unlike ignore/silent, which
        // continue). The config key is omitted here to exercise that default path.
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": [
                    "set(resource.attributes[\"x\"], \"fail\")",
                    "set(attributes[\"after\"], \"yes\")"
                ]
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlTransformConfiguration::from_configuration(&config).expect("config should parse");
        assert_eq!(
            ottl_config.config.error_mode,
            ErrorMode::Propagate,
            "omitted error_mode must default to Propagate"
        );

        let mut transform = ottl_config
            .build(test_component_context())
            .await
            .expect("build should succeed");
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "after"),
            None,
            "default (propagate) must stop processing after the first statement error"
        );
    }

    #[tokio::test]
    async fn invalid_error_mode_value_is_rejected() {
        // `error_mode` accepts only ignore/silent/propagate; any other value must fail deserialization.
        let cfg_json = serde_json::json!({
            "ottl_transform_config": { "error_mode": "explode" }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let result = OttlTransformConfiguration::from_configuration(&config);
        assert!(result.is_err(), "an unrecognized error_mode value must be rejected");
    }

    // ---- Group 6: Multi-span and multi-trace (integration) ----

    #[tokio::test]
    async fn transform_buffer_applies_to_all_spans() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"added\"], \"yes\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let spans = vec![
            make_span(1, 1, HashMap::new()),
            make_span(1, 2, HashMap::new()),
            make_span(1, 3, HashMap::new()),
        ];
        let trace = make_trace(spans, None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        for i in 0..3 {
            assert_eq!(
                get_span_attr(&buffer, i, "added").as_deref(),
                Some("yes"),
                "span {} should have the attribute set",
                i
            );
        }
    }

    #[tokio::test]
    async fn transform_buffer_each_span_independent() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"x\"], \"v\") where attributes[\"env\"] == \"prod\""]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let spans = vec![
            make_span(1, 1, HashMap::from([("env".into(), "prod".into())])),
            make_span(1, 2, HashMap::from([("env".into(), "staging".into())])),
            make_span(1, 3, HashMap::new()),
        ];
        let trace = make_trace(spans, None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "x").as_deref(),
            Some("v"),
            "span with env=prod should be modified"
        );
        assert_eq!(
            get_span_attr(&buffer, 1, "x"),
            None,
            "span with env=staging should be untouched"
        );
        assert_eq!(
            get_span_attr(&buffer, 2, "x"),
            None,
            "span without env should be untouched"
        );
    }

    #[tokio::test]
    async fn transform_buffer_multiple_traces() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"tagged\"], \"yes\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let trace1 = make_trace(vec![make_span(1, 1, HashMap::new())], None);
        let trace2 = make_trace(vec![make_span(2, 1, HashMap::new())], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace1)).is_none());
        assert!(buffer.try_push(Event::Trace(trace2)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "tagged").as_deref(),
            Some("yes"),
            "first trace's span should be modified"
        );
        assert_eq!(
            get_span_attr(&buffer, 1, "tagged").as_deref(),
            Some("yes"),
            "second trace's span should be modified"
        );
    }

    #[tokio::test]
    async fn transform_buffer_empty_statements_noop() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": []
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::from([("a".into(), "b".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "a").as_deref(),
            Some("b"),
            "no statements configured; span should pass through unchanged"
        );
    }

    // ---- Group 7: Resource attributes read-only constraint ----

    #[tokio::test]
    async fn set_resource_attributes_returns_error() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "error_mode": "ignore",
                "trace_statements": ["set(resource.attributes[\"key\"], \"value\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], Some(vec!["key:original"]));
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);

        let trace_out = buffer
            .into_iter()
            .find_map(|e| match e {
                Event::Trace(t) => Some(t),
                _ => None,
            })
            .expect("trace should still be in buffer");
        let tag_val = trace_out
            .attributes
            .get(&MetaString::from("key"))
            .and_then(|v| match v {
                AttributeValue::String(s) => Some(s.as_ref().to_string()),
                _ => None,
            });
        assert_eq!(
            tag_val.as_deref(),
            Some("original"),
            "resource tag should remain unchanged because set is not supported"
        );
    }

    // ---- Group 8: Edge cases ----

    #[tokio::test]
    async fn set_empty_string_value() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"key\"], \"\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "key").as_deref(),
            Some(""),
            "empty string value should be stored, not treated as Nil"
        );
    }

    #[tokio::test]
    async fn set_attribute_with_dots_in_key() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"service.name\"], \"foo\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "service.name").as_deref(), Some("foo"));
    }

    #[tokio::test]
    async fn set_attribute_with_special_characters() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"key with spaces\"], \"value with ñ and 日本語\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            get_span_attr(&buffer, 0, "key with spaces").as_deref(),
            Some("value with ñ and 日本語")
        );
    }

    #[tokio::test]
    async fn transform_buffer_non_trace_events_ignored() {
        let cfg_json = serde_json::json!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"x\"], \"y\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;

        let sc = ServiceCheck::new("test.check", CheckStatus::Ok);
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);

        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::ServiceCheck(sc)).is_none());
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);

        let mut saw_service_check = false;
        let mut trace_span_x = None;
        for event in &buffer {
            match event {
                Event::ServiceCheck(_) => saw_service_check = true,
                Event::Trace(t) => {
                    trace_span_x = t.spans().first().and_then(|s| {
                        let k = MetaString::from("x");
                        s.attributes.get(&k).and_then(|v| match v {
                            AttributeValue::String(sv) => Some(sv.as_ref().to_string()),
                            _ => None,
                        })
                    });
                }
                _ => {}
            }
        }
        assert!(saw_service_check, "non-trace events should pass through untouched");
        assert_eq!(
            trace_span_x.as_deref(),
            Some("y"),
            "trace span should still be transformed"
        );
    }
}
