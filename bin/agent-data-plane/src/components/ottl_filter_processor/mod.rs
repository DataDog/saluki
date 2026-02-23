//! OTTL filter processor component.
//!
//! Drops spans (and optionally span events) when OTTL conditions match, following the
//! [OpenTelemetry filterprocessor] spec.
//!
//! [OpenTelemetry filterprocessor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/filterprocessor

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use ottl::{CallbackMap, EnumMap, OttlParser, Value};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::{Span, Trace},
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error};

mod config;
mod span_context;

use config::{ErrorMode, OttlFilterConfig};
use span_context::{SpanFilterContext, SpanFilterFamily};

/// Configuration for the OTTL filter processor, loaded from the data plane config.
#[derive(Clone, Debug)]
pub struct OttlFilterConfiguration {
    /// Parsed filter config (error_mode, traces.span, etc.).
    config: OttlFilterConfig,
}

impl OttlFilterConfiguration {
    /// Creates configuration from the given generic configuration.
    ///
    /// Reads the OTTL filter config from the `ottl_config` key at the top level of the
    /// data-plane configuration. The YAML structure is: `error_mode`, `traces.span` (list
    /// of OTTL condition strings). If the key is absent, a default (no-op) config is used.
    ///
    /// # Errors
    ///
    /// Returns an error if a value at `ottl_config` exists but fails to deserialize.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let filter_config = config.try_get_typed::<OttlFilterConfig>("ottl_config")?;
        Ok(Self {
            config: filter_config.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for OttlFilterConfiguration {
    /// Builds the OTTL filter transform from the current configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if any OTTL span condition string fails to parse.
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let path_resolvers = span_context::span_filter_path_resolvers();
        let editors = CallbackMap::new();
        let converters = CallbackMap::new();
        let enums = EnumMap::new();

        let mut span_parsers = Vec::new();
        for condition in &self.config.traces.span {
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
    error_mode: ErrorMode,
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

        let mut ctx = SpanFilterContext::new(span, trace.resource_tags());

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
                    ErrorMode::Ignore => {
                        error!(error = %e, "OTTL filter condition error; ignoring");
                    }
                    ErrorMode::Silent => {}
                    ErrorMode::Propagate => {
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

    use saluki_common::collections::FastHashMap;
    use saluki_config::ConfigurationLoader;
    use saluki_context::tags::TagSet;
    use saluki_core::{
        components::{transforms::*, ComponentContext},
        data_model::event::{trace::Span, trace::Trace, Event},
        topology::{ComponentId, EventsBuffer},
    };
    use stringtheory::MetaString;

    use super::*;

    fn make_span(trace_id: u64, span_id: u64, meta: HashMap<String, String>) -> Span {
        let mut meta_map = FastHashMap::default();
        for (k, v) in meta {
            meta_map.insert(MetaString::from(k), MetaString::from(v));
        }
        Span::new("svc", "op", "res", "web", trace_id, span_id, 0, 0, 1000, 0).with_meta(meta_map)
    }

    fn make_trace(spans: Vec<Span>, resource_tags: Option<Vec<&'static str>>) -> Trace {
        let mut tag_set = TagSet::default();
        if let Some(tags) = resource_tags {
            for t in tags {
                tag_set.insert_tag(t);
            }
        }
        Trace::new(spans, tag_set)
    }

    fn span_count_in_buffer(buffer: &EventsBuffer) -> usize {
        buffer
            .into_iter()
            .filter_map(|e| match e {
                Event::Trace(t) => Some(t.spans().len()),
                _ => None,
            })
            .sum()
    }

    /// When `ottl_config` is absent, config defaults to empty conditions and no spans are dropped.
    #[tokio::test]
    async fn from_configuration_absent_key_returns_default() {
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).expect("should succeed");
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
        let mut transform = ottl_config.build(ctx).await.expect("build should succeed");
        let span = make_span(1, 1, HashMap::from([("a".into(), "b".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1, "default config must not drop spans");
    }

    /// When `ottl_config` is present but invalid (e.g. unknown fields), `from_configuration` returns an error.
    #[tokio::test]
    async fn from_configuration_invalid_yaml_returns_error() {
        let invalid = serde_json::json!({
            "ottl_config": {
                "unknown_field": 1
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(invalid), None, false).await;
        let result = OttlFilterConfiguration::from_configuration(&config);
        assert!(result.is_err(), "invalid ottl_config must yield error");
    }

    /// When a span condition string is invalid OTTL syntax, `build` returns an error.
    #[tokio::test]
    async fn build_invalid_condition_returns_error() {
        let cfg_json = serde_json::json!({
            "ottl_config": {
                "traces": { "span": ["syntax error !!"] }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).expect("config is valid");
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
        let result = ottl_config.build(ctx).await;
        assert!(result.is_err(), "invalid OTTL condition must make build fail");
    }

    /// Multiple valid conditions in `traces.span` are all parsed; filter uses OR semantics (any match drops the span).
    #[tokio::test]
    async fn build_multiple_conditions_all_parsed() {
        let cfg_json = serde_json::json!({
            "ottl_config": {
                "traces": {
                    "span": [
                        "attributes[\"a\"] == \"x\"",
                        "resource.attributes[\"host.name\"] == \"localhost\""
                    ]
                }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).expect("config is valid");
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
        let (config, _) = ConfigurationLoader::for_tests(None, None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
            "ottl_config": { "traces": { "span": ["attributes[\"env\"] == \"drop\""] } }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
            "ottl_config": { "traces": { "span": ["attributes[\"env\"] == \"drop\""] } }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
            "ottl_config": { "traces": { "span": ["resource.attributes[\"host.name\"] == \"localhost\""] } }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
            "ottl_config": {
                "traces": {
                    "span": [
                        "attributes[\"first\"] == \"no\"",
                        "attributes[\"second\"] == \"yes\""
                    ]
                }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
            "ottl_config": {
                "error_mode": "ignore",
                "traces": { "span": ["attributes[\"x\"]"] }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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

    /// With `error_mode: ignore`, when condition evaluation errors (e.g. type mismatch), the span is kept.
    #[tokio::test]
    async fn error_mode_ignore_eval_error_keeps_span() {
        let cfg_json = serde_json::json!({
            "ottl_config": {
                "error_mode": "ignore",
                "traces": { "span": ["attributes[\"x\"] > 1"] }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("x".into(), "string".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1);
    }

    /// With `error_mode: silent`, when condition evaluation errors, the span is kept (no log).
    #[tokio::test]
    async fn error_mode_silent_eval_error_keeps_span() {
        let cfg_json = serde_json::json!({
            "ottl_config": {
                "error_mode": "silent",
                "traces": { "span": ["attributes[\"x\"] > 1"] }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("x".into(), "string".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 1);
    }

    /// With `error_mode: propagate`, when condition evaluation errors, the span is dropped.
    #[tokio::test]
    async fn error_mode_propagate_eval_error_drops_span() {
        let cfg_json = serde_json::json!({
            "ottl_config": {
                "error_mode": "propagate",
                "traces": { "span": ["attributes[\"x\"] > 1"] }
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
        let mut transform = ottl_config.build(ctx).await.unwrap();
        let span = make_span(1, 1, HashMap::from([("x".into(), "string".into())]));
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(span_count_in_buffer(&buffer), 0);
    }

    /// `transform_buffer` removes only spans that match the condition; remaining spans are unchanged and in order.
    #[tokio::test]
    async fn transform_buffer_trace_spans_filtered() {
        let cfg_json = serde_json::json!({
            "ottl_config": { "traces": { "span": ["attributes[\"drop\"] == \"yes\""] } }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
                s.meta()
                    .iter()
                    .find(|(k, _)| k.as_ref() == "label")
                    .map(|(_, v)| v.as_ref().to_string())
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
            "ottl_config": { "traces": { "span": ["attributes[\"env\"] == \"drop\""] } }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlFilterConfiguration::from_configuration(&config).unwrap();
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_filter").unwrap());
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
