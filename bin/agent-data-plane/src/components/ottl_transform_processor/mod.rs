//! OTTL Transform processor component.
//!
//! Executes OTTL transformation statements against trace spans, following the
//! [OpenTelemetry Transform processor] spec. Currently supports the `set` editor function
//! for span-level attributes (`attributes["key"]`), with read access to resource attributes
//! (`resource.attributes["key"]`) for use in conditions.
//!
//! [OpenTelemetry Transform processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor

use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use ottl::{CallbackMap, EnumMap, OttlParser, Value};
use saluki_config::GenericConfiguration;
use saluki_context::tags::SharedTagSet;
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
    /// Parsed Transform config (error_mode, trace_statements, etc.).
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

        let mut editors = CallbackMap::new();
        editors.insert(
            "set".to_string(),
            Arc::new(|args: &mut dyn ottl::Args| {
                let value = args.get(1)?;
                args.set(0, &value)?;
                Ok(Value::Nil)
            }),
        );

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
    fn transform_span(&self, span: &mut Span, resource_tags: &SharedTagSet) {
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
