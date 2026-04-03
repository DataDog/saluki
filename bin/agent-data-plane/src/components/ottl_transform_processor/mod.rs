//! OTTL Transform processor component.
//!
//! Executes OTTL transformation statements against trace spans, following the
//! [OpenTelemetry Transform processor] spec. Currently supports the `set` editor function
//! for span-level attributes (`attributes["key"]`), with read access to resource attributes
//! (`resource.attributes["key"]`) for use in conditions.
//!
//! [OpenTelemetry Transform processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use ottl::{CallbackMap, EnumMap, OttlParser};
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::Span,
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, error};

mod config;
mod span_context;

use config::{ErrorMode, OttlTransformConfig};
use span_context::{SpanTransformContext, SpanTransformFamily};

/// Configuration for the OTTL Transform processor, loaded from the data plane config.
#[derive(Clone, Debug)]
pub struct OttlTransformConfiguration {
    config: OttlTransformConfig,
}

impl OttlTransformConfiguration {
    /// Creates configuration from the given generic configuration.
    ///
    /// Reads the OTTL Transform config from the `ottl_transform_config` key at the top level of the
    /// data-plane configuration.
    ///
    /// Returns an error if a value at `ottl_transform_config` exists but fails to deserialize.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let transform_config = config.try_get_typed::<OttlTransformConfig>("ottl_transform_config")?;
        Ok(Self {
            config: transform_config.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for OttlTransformConfiguration {
    /// Builds the OTTL Transform transform from the current configuration.
    ///
    /// Registers the `set` editor function, parses each trace statement, and returns
    /// the transform. Statements may be editor calls like `set(attributes["key"], "value")`
    /// with optional `where` clauses.
    ///
    /// # Errors
    ///
    /// Returns an error if any OTTL statement fails to parse.
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
    /// Each statement is executed in order. For editor statements (e.g. `set`), the `where`
    /// clause is evaluated first; if it matches (or is absent), the editor function runs.
    /// Errors are handled according to `error_mode`.
    fn transform_span(&self, span: &mut Span, resource_tags: &TagSet) {
        let mut ctx = SpanTransformContext::new(span, resource_tags);

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
                let resource_tags = trace.resource_tags().clone();
                for span in trace.spans_mut() {
                    self.transform_span(span, &resource_tags);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_common::collections::FastHashMap;
    use saluki_config::{value::value, ConfigurationLoader};
    use saluki_context::tags::TagSet;
    use saluki_core::{
        components::{transforms::*, ComponentContext},
        data_model::event::{
            service_check::{CheckStatus, ServiceCheck},
            trace::{Span, Trace},
            Event,
        },
        topology::{ComponentId, EventsBuffer},
    };
    use stringtheory::MetaString;

    use super::*;

    // ---- Helpers ----

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

    fn get_span_attr(buffer: &EventsBuffer, span_index: usize, key: &str) -> Option<String> {
        buffer
            .into_iter()
            .filter_map(|e| match e {
                Event::Trace(t) => Some(t.spans()),
                _ => None,
            })
            .flat_map(|spans| spans.iter())
            .nth(span_index)
            .and_then(|span| span.meta().get(key).map(|v| v.as_ref().to_string()))
    }

    async fn build_transform(cfg_json: Option<saluki_config::value::Value>) -> Box<dyn SynchronousTransform + Send> {
        let (config, _) = ConfigurationLoader::for_tests(cfg_json, None, false).await;
        let ottl_config = OttlTransformConfiguration::from_configuration(&config).expect("config should parse");
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_transform").unwrap());
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
        let invalid = value!({
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
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["syntax error !!"]
            }
        });
        let (config, _) = ConfigurationLoader::for_tests(Some(cfg_json), None, false).await;
        let ottl_config = OttlTransformConfiguration::from_configuration(&config).expect("config is valid");
        let ctx = ComponentContext::transform(ComponentId::try_from("ottl_transform").unwrap());
        let result = ottl_config.build(ctx).await;
        assert!(result.is_err(), "invalid OTTL syntax must make build fail");
    }

    #[tokio::test]
    async fn build_empty_and_whitespace_statements_skipped() {
        let cfg_json = value!({
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
    async fn set_new_attribute() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"newkey\"], \"newval\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "newkey").as_deref(), Some("newval"));
    }

    #[tokio::test]
    async fn set_overwrite_existing_attribute() {
        let cfg_json = value!({
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
        let cfg_json = value!({
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
    async fn set_int_value_converts_to_string() {
        //"The answer to the Ultimate Question of Life, the Universe, and Everything"
        //The Hitchhiker's Guide to the Galaxy ;)
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"num\"], 42)"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "num").as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn set_float_value_converts_to_string() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"not_pi\"], 6.14)"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "not_pi").as_deref(), Some("6.14"));
    }

    #[tokio::test]
    async fn set_bool_value_converts_to_string() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"flag\"], true)"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;
        let span = make_span(1, 1, HashMap::new());
        let trace = make_trace(vec![span], None);
        let mut buffer = EventsBuffer::default();
        assert!(buffer.try_push(Event::Trace(trace)).is_none());
        transform.transform_buffer(&mut buffer);
        assert_eq!(get_span_attr(&buffer, 0, "flag").as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn set_from_another_attribute() {
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
    async fn error_mode_ignore_continues_processing() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "error_mode": "ignore",
                "trace_statements": [
                    "set(resource.attributes[\"x\"], \"fail\")",
                    "set(attributes[\"ok\"], \"yes\")"
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
            get_span_attr(&buffer, 0, "ok").as_deref(),
            Some("yes"),
            "ignore mode: subsequent statement should still execute after error"
        );
    }

    #[tokio::test]
    async fn error_mode_silent_continues_processing() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "error_mode": "silent",
                "trace_statements": [
                    "set(resource.attributes[\"x\"], \"fail\")",
                    "set(attributes[\"ok\"], \"yes\")"
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
            get_span_attr(&buffer, 0, "ok").as_deref(),
            Some("yes"),
            "silent mode: subsequent statement should still execute after error"
        );
    }

    #[tokio::test]
    async fn error_mode_propagate_stops_span_processing() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "error_mode": "propagate",
                "trace_statements": [
                    "set(resource.attributes[\"x\"], \"fail\")",
                    "set(attributes[\"should_not_appear\"], \"yes\")"
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
            get_span_attr(&buffer, 0, "should_not_appear"),
            None,
            "propagate mode: processing should stop after the first error"
        );
    }

    // ---- Group 6: Multi-span and multi-trace (integration) ----

    #[tokio::test]
    async fn transform_buffer_applies_to_all_spans() {
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
            .resource_tags()
            .get_single_tag("key")
            .and_then(|t| t.value().map(|v| v.to_string()));
        assert_eq!(
            tag_val.as_deref(),
            Some("original"),
            "resource tag should remain unchanged because set is not supported"
        );
    }

    // ---- Group 8: Edge cases ----

    #[tokio::test]
    async fn set_empty_string_value() {
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
        let cfg_json = value!({
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
                    trace_span_x = t
                        .spans()
                        .first()
                        .and_then(|s| s.meta().get("x").map(|v| v.as_ref().to_string()));
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

    // ---- Group 9: Performance tests ----

    const PERF_NUM_BUFFERS: usize = 1024;
    const PERF_SPANS_PER_BUFFER: usize = 100;

    fn perf_make_buffer(spans: Vec<Span>) -> EventsBuffer {
        let trace = make_trace(spans, None);
        let mut buf = EventsBuffer::default();
        assert!(buf.try_push(Event::Trace(trace)).is_none());
        buf
    }

    /// Run with: `cargo test -p agent-data-plane perf_throughput_set_all --release -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "performance test; run with: cargo test -- --ignored --nocapture perf_throughput"]
    async fn perf_throughput_set_all() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"tag\"], \"value\")"]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;

        let spans: Vec<Span> = (0..PERF_SPANS_PER_BUFFER)
            .map(|i| make_span(1, i as u64, HashMap::new()))
            .collect();
        let template = perf_make_buffer(spans);
        let mut buffers: Vec<EventsBuffer> = (0..PERF_NUM_BUFFERS).map(|_| template.clone()).collect();
        let total_spans = PERF_NUM_BUFFERS * PERF_SPANS_PER_BUFFER;

        let start = std::time::Instant::now();
        for buf in &mut buffers {
            transform.transform_buffer(buf);
        }
        let elapsed = start.elapsed();
        let throughput = total_spans as f64 / elapsed.as_secs_f64();
        println!(
            "perf_throughput_set_all: {} spans in {:?} -> {:.0} spans/s",
            total_spans, elapsed, throughput
        );
    }

    /// Run with: `cargo test -p agent-data-plane perf_throughput_set_half --release -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "performance test; run with: cargo test -- --ignored --nocapture perf_throughput"]
    async fn perf_throughput_set_half() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"tag\"], \"value\") where attributes[\"half\"] == \"yes\""]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;

        let spans: Vec<Span> = (0..PERF_SPANS_PER_BUFFER)
            .map(|i| {
                let half_val = if i % 2 == 0 { "yes" } else { "no" };
                make_span(1, i as u64, HashMap::from([("half".into(), half_val.to_string())]))
            })
            .collect();
        let template = perf_make_buffer(spans);
        let mut buffers: Vec<EventsBuffer> = (0..PERF_NUM_BUFFERS).map(|_| template.clone()).collect();
        let total_spans = PERF_NUM_BUFFERS * PERF_SPANS_PER_BUFFER;

        let start = std::time::Instant::now();
        for buf in &mut buffers {
            transform.transform_buffer(buf);
        }
        let elapsed = start.elapsed();
        let throughput = total_spans as f64 / elapsed.as_secs_f64();
        println!(
            "perf_throughput_set_half: {} spans in {:?} -> {:.0} spans/s",
            total_spans, elapsed, throughput
        );
    }

    /// Run with: `cargo test -p agent-data-plane perf_throughput_set_none --release -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "performance test; run with: cargo test -- --ignored --nocapture perf_throughput"]
    async fn perf_throughput_set_none() {
        let cfg_json = value!({
            "ottl_transform_config": {
                "trace_statements": ["set(attributes[\"tag\"], \"value\") where attributes[\"nomatch\"] == \"yes\""]
            }
        });
        let mut transform = build_transform(Some(cfg_json)).await;

        let spans: Vec<Span> = (0..PERF_SPANS_PER_BUFFER)
            .map(|i| make_span(1, i as u64, HashMap::from([("env".into(), "keep".into())])))
            .collect();
        let template = perf_make_buffer(spans);
        let mut buffers: Vec<EventsBuffer> = (0..PERF_NUM_BUFFERS).map(|_| template.clone()).collect();
        let total_spans = PERF_NUM_BUFFERS * PERF_SPANS_PER_BUFFER;

        let start = std::time::Instant::now();
        for buf in &mut buffers {
            transform.transform_buffer(buf);
        }
        let elapsed = start.elapsed();
        let throughput = total_spans as f64 / elapsed.as_secs_f64();
        println!(
            "perf_throughput_set_none: {} spans in {:?} -> {:.0} spans/s",
            total_spans, elapsed, throughput
        );
    }
}
