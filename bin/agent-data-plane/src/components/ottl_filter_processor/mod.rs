//! OTTL filter processor component.
//!
//! Drops spans when OTTL conditions match, following the [OpenTelemetry filterprocessor] spec.
//! Only span-level conditions (`traces.span`) are implemented; span-event filtering is not.
//!
//! [OpenTelemetry filterprocessor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/filterprocessor

use agent_data_plane_config::domains::traces::{OttlErrorMode, OttlFilter as TypedOttlFilter};
use async_trait::async_trait;
use ottl::{CallbackMap, EnumMap, OttlParser, Value};
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::trace::{Span, Trace},
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error};

mod span_context;
use span_context::{SpanFilterContext, SpanFilterFamily};

/// Configuration for the OTTL filter processor, loaded from the data plane config.
#[derive(Clone, Debug)]
pub struct OttlFilterConfiguration {
    config: TypedOttlFilter,
}

impl OttlFilterConfiguration {
    /// Creates configuration from the resolved typed traces configuration.
    pub fn from_configuration(config: &TypedOttlFilter) -> Self {
        Self { config: config.clone() }
    }
}

#[async_trait]
impl SynchronousTransformBuilder for OttlFilterConfiguration {
    /// Builds the OTTL filter transform from the current configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if any OTTL span condition string fails to parse.
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let path_resolvers = span_context::span_filter_path_resolvers();
        let editors = CallbackMap::new();
        let converters = CallbackMap::new();
        let enums = EnumMap::new();

        let mut span_parsers = Vec::new();
        for condition in &self.config.span_conditions {
            let condition = condition.trim();
            if condition.is_empty() {
                continue;
            }
            let parser = ottl::Parser::new(&editors, &converters, &enums, &path_resolvers, condition);

            debug!("Add new parser with condition: \"{}\"", condition);

            parser
                .is_error()
                .map_err(|e| generic_error!("OTTL filter span condition parse error: {}: {}", condition, e))?;
            span_parsers.push(parser);
        }

        Ok(Box::new(OttlFilter {
            error_mode: self.config.error_mode,
            span_parsers,
        }))
    }
}

impl MemoryBounds for OttlFilterConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<OttlFilter>("component struct");
    }
}

/// Synchronous transform that drops spans matching OTTL conditions.
pub struct OttlFilter {
    error_mode: OttlErrorMode,
    span_parsers: Vec<ottl::Parser<SpanFilterFamily>>,
}

impl OttlFilter {
    /// Returns true if the span should be dropped (any condition matched).
    ///
    /// Uses `self.current_trace` (set in `transform_buffer`) to access resource tags.
    fn should_drop_span(&self, trace: &Trace, span: &Span) -> bool {
        if self.span_parsers.is_empty() {
            return false;
        }

        let mut ctx = SpanFilterContext::new(span, &trace.attributes);

        for parser in &self.span_parsers {
            match parser.execute(&mut ctx) {
                Ok(Value::Bool(true)) => {
                    //debug!(span_name = %span.name(), "OTTL filter condition matched; dropping span");
                    return true;
                }
                Ok(Value::Bool(false)) => {
                    //debug!(span_name = %span.name(), "OTTL filter condition NOT matched; keeping span");
                    continue;
                }
                Ok(_) => continue,
                Err(e) => match self.error_mode {
                    OttlErrorMode::Ignore => {
                        error!(error = %e, "OTTL filter condition error; ignoring");
                    }
                    OttlErrorMode::Silent => {}
                    OttlErrorMode::Propagate => {
                        //propagate: The processor returns the error up the pipeline. This will result in the payload
                        // being dropped from the collector.
                        // AZH: The current API of SynchronousTransform::transform_buffer does not propagate errors;
                        //  it only logs them.
                        error!(error = %e, "OTTL filter condition error; dropping span (error_mode=propagate)");
                        return true;
                    }
                },
            }
        }
        false
    }
}

impl SynchronousTransform for OttlFilter {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        for event in event_buffer {
            if let Some(trace) = event.try_as_trace_mut() {
                trace.remove_spans(|trace, span| self.should_drop_span(trace, span));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_core::{
        components::{transforms::*, BuildContext},
        data_model::event::{trace::AttributeValue, Event},
        topology::EventsBuffer,
    };

    use super::*;
    use crate::components::test_support::{make_span, make_trace};

    fn span_count_in_buffer(buffer: &EventsBuffer) -> usize {
        buffer
            .into_iter()
            .filter_map(|e| match e {
                Event::Trace(t) => Some(t.spans().len()),
                _ => None,
            })
            .sum()
    }

    fn test_component_context() -> BuildContext {
        BuildContext::test_transform("ottl_filter")
    }

    fn test_config(value: serde_json::Value) -> OttlFilterConfiguration {
        let root = value.get("ottl_filter_config").cloned().unwrap_or_default();
        let error_mode = match root.get("error_mode").and_then(serde_json::Value::as_str) {
            Some("ignore") => OttlErrorMode::Ignore,
            Some("silent") => OttlErrorMode::Silent,
            _ => OttlErrorMode::Propagate,
        };
        let span_conditions = root
            .pointer("/traces/span")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        let typed = TypedOttlFilter {
            error_mode,
            span_conditions,
        };
        OttlFilterConfiguration::from_configuration(&typed)
    }

    /// When `ottl_filter_config` is absent, config defaults to empty conditions and no spans are dropped.
    #[tokio::test]
    async fn from_configuration_absent_key_returns_default() {
        let ottl_config = test_config(serde_json::json!({}));
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.expect("build should succeed");
        let span = make_span(1, 1, HashMap::from([("a".into(), "b".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1, "default config must not drop spans");
    }

    /// When a span condition string is invalid OTTL syntax, `build` returns an error.
    #[tokio::test]
    async fn build_invalid_condition_returns_error() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": {
                "traces": { "span": ["syntax error !!"] }
            }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let result = ottl_config.build(ctx).await;
        assert!(result.is_err(), "invalid OTTL condition must make build fail");
    }

    /// Multiple valid conditions in `traces.span` are all parsed; filter uses OR semantics (any match drops the span).
    #[tokio::test]
    async fn build_multiple_conditions_all_parsed() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": {
                "traces": {
                    "span": [
                        "attributes[\"a\"] == \"x\"",
                        "resource.attributes[\"host.name\"] == \"localhost\""
                    ]
                }
            }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.expect("build must succeed");
        let span_match_first = make_span(1, 1, HashMap::from([("a".into(), "x".into())]));
        let trace1 = make_trace(vec![span_match_first], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace1)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            span_count_in_buffer(&buffer),
            0,
            "first condition matches -> span dropped"
        );
        let span_match_second = make_span(2, 1, HashMap::new());
        let trace2 = make_trace(vec![span_match_second], Some(vec!["host.name:localhost"]));
        assert!(buffer.try_push(Event::Trace(trace2)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            span_count_in_buffer(&buffer),
            0,
            "second condition matches -> span dropped"
        );
    }

    /// With no conditions configured, no span is dropped.
    #[tokio::test]
    async fn should_drop_span_empty_parsers_returns_false() {
        let ottl_config = test_config(serde_json::json!({}));
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("drop".into(), "me".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1);
    }

    /// When the condition evaluates to true for a span, that span is dropped.
    #[tokio::test]
    async fn should_drop_span_condition_true_drops() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": { "traces": { "span": ["attributes[\"env\"] == \"drop\""] } }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("env".into(), "drop".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 0);
    }

    /// When the condition evaluates to false for a span, that span is kept.
    #[tokio::test]
    async fn should_drop_span_condition_false_keeps() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": { "traces": { "span": ["attributes[\"env\"] == \"drop\""] } }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("env".into(), "keep".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1);
    }

    /// Conditions on `resource.attributes` match trace resource tags; matching span is dropped, non-matching kept.
    #[tokio::test]
    async fn should_drop_span_resource_attributes() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": { "traces": { "span": ["resource.attributes[\"host.name\"] == \"localhost\""] } }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], Some(vec!["host.name:localhost"]));
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 0);
        let span2 = make_span(2, 1, HashMap::new());
        let trace2 = make_trace(vec![span2], Some(vec!["host.name:other"]));
        assert!(buffer.try_push(Event::Trace(trace2)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1);
    }

    /// Multiple conditions are combined with OR: span is dropped if any condition is true; kept only if all are false.
    #[tokio::test]
    async fn should_drop_span_or_semantics() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": {
                "traces": {
                    "span": [
                        "attributes[\"first\"] == \"no\"",
                        "attributes[\"second\"] == \"yes\""
                    ]
                }
            }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span_first_false_second_true = make_span(
            1,
            1,
            HashMap::from([("first".into(), "no".into()), ("second".into(), "yes".into())]),
        );
        let trace = make_trace(vec![span_first_false_second_true], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 0);
        let span_both_false = make_span(
            2,
            1,
            HashMap::from([("first".into(), "x".into()), ("second".into(), "y".into())]),
        );
        let trace2 = make_trace(vec![span_both_false], None);
        assert!(buffer.try_push(Event::Trace(trace2)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1);
    }

    /// Path used as condition returns non-bool → OTTL errors; with `error_mode: ignore` the span is kept.
    #[tokio::test]
    async fn should_drop_span_non_bool_result_keeps() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": {
                "error_mode": "ignore",
                "traces": { "span": ["attributes[\"x\"]"] }
            }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("x".into(), "value".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            span_count_in_buffer(&buffer),
            1,
            "path returning non-bool errors; ignore keeps span"
        );
    }

    /// When a condition errors, `error_mode` decides the span's fate: ignore/silent keep it, propagate drops
    /// it. One case per `ErrorMode` arm.
    #[tokio::test]
    async fn error_mode_controls_span_fate_when_a_condition_errors() {
        struct Case {
            error_mode: &'static str,
            expected_span_count: usize,
        }

        let cases = [
            Case {
                error_mode: "ignore",
                expected_span_count: 1,
            },
            Case {
                error_mode: "silent",
                expected_span_count: 1,
            },
            Case {
                error_mode: "propagate",
                expected_span_count: 0,
            },
        ];

        for case in cases {
            let cfg_json = serde_json::json!({
                "ottl_filter_config": {
                    "error_mode": case.error_mode,
                    // `x` is a string, so `attributes["x"] > 1` errors during evaluation.
                    "traces": { "span": ["attributes[\"x\"] > 1"] }
                }
            });
            let ottl_config = test_config(cfg_json);
            let mut transform = ottl_config.build(test_component_context()).await.unwrap();
            let span = make_span(1, 1, HashMap::from([("x".into(), "string".into())]));
            let trace = make_trace(vec![span], None);
            let mut buffer = EventsBuffer::default();
            assert!(buffer.try_push(Event::Trace(trace)).is_none());
            transform.transform_buffer(&mut buffer);
            assert_eq!(
                span_count_in_buffer(&buffer),
                case.expected_span_count,
                "error_mode={}",
                case.error_mode
            );
        }
    }

    /// With `error_mode` omitted, the documented default (`Propagate`) drops the span when a condition errors.
    #[tokio::test]
    async fn omitted_error_mode_defaults_to_propagate() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": { "traces": { "span": ["attributes[\"x\"] > 1"] } }
        });
        let ottl_config = test_config(cfg_json);
        assert_eq!(
            ottl_config.config.error_mode,
            OttlErrorMode::Propagate,
            "omitted error_mode must default to Propagate"
        );

        let mut transform = ottl_config
            .build(test_component_context())
            .await
            .expect("build should succeed");
        // `x` is a string, so `attributes["x"] > 1` errors during evaluation; propagate drops the span.
        let span = make_span(1, 1, HashMap::from([("x".into(), "string".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(
            span_count_in_buffer(&buffer),
            0,
            "default (propagate) must drop the span when a condition errors"
        );
    }

    /// `transform_buffer` removes only spans that match the condition; remaining spans are unchanged and in order.
    #[tokio::test]
    async fn transform_buffer_trace_spans_filtered() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": { "traces": { "span": ["attributes[\"drop\"] == \"yes\""] } }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let keep1 = make_span(
            1,
            1,
            HashMap::from([("drop".into(), "no".into()), ("label".into(), "keep1".into())]),
        );
        let drop1 = make_span(
            1,
            2,
            HashMap::from([("drop".into(), "yes".into()), ("label".into(), "drop1".into())]),
        );
        let keep2 = make_span(1, 3, HashMap::from([("label".into(), "keep2".into())]));
        let trace = make_trace(vec![keep1, drop1, keep2], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 2);
        let remaining_labels: Vec<String> = buffer
            .into_iter()
            .filter_map(|e| match e {
                Event::Trace(t) => Some(t.spans().to_vec()),
                _ => None,
            })
            .flatten()
            .filter_map(|s| {
                s.attributes
                    .iter()
                    .find(|(k, _)| k.as_ref() == "label")
                    .and_then(|(_, v)| match v {
                        AttributeValue::String(sv) => Some(sv.as_ref().to_string()),
                        _ => None,
                    })
            })
            .collect();
        assert_eq!(
            remaining_labels,
            ["keep1", "keep2"],
            "only spans that did not match the condition must remain"
        );
    }

    /// When all spans in a trace match the condition, the trace ends up with zero spans.
    #[tokio::test]
    async fn transform_buffer_all_spans_dropped_trace_empty() {
        let cfg_json = serde_json::json!({
            "ottl_filter_config": { "traces": { "span": ["attributes[\"env\"] == \"drop\""] } }
        });
        let ottl_config = test_config(cfg_json);
        let ctx = test_component_context();
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let s1 = make_span(1, 1, HashMap::from([("env".into(), "drop".into())]));
        let s2 = make_span(1, 2, HashMap::from([("env".into(), "drop".into())]));
        let trace = make_trace(vec![s1, s2], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 0);
    }
}
