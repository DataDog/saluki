//! OTTL Transform processor component.
//!
//! Drops spans (and optionally span events) when OTTL conditions match, following the
//! [OpenTelemetry Transform processor] spec.
//!
//! [OpenTelemetry Transform processor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor

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

use config::{ErrorMode, OttlTransformConfig};
use span_context::{SpanTransformContext, SpanTransformFamily};

/// Configuration for the OTTL Transform processor, loaded from the data plane config.
#[derive(Clone, Debug)]
pub struct OttlTransformConfiguration {
    /// Parsed Transform config (error_mode, traces.span, etc.).
    config: OttlTransformConfig,
}

impl OttlTransformConfiguration {
    /// Creates configuration from the given generic configuration.
    ///
    /// Reads the OTTL Transform config from the `ottl_transform_config` key at the top level of the
    /// data-plane configuration.
    ///
    /// Returns an error if a value at `ottl_config` exists but fails to deserialize.
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
    /// # Errors
    ///
    /// Returns an error if any OTTL span condition string fails to parse.
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let path_resolvers = span_context::span_transform_path_resolvers();
        let editors = CallbackMap::new();
        let converters = CallbackMap::new();
        let enums = EnumMap::new();

        let mut span_parsers = Vec::new();
        for condition in &self.config.trace_statements {
            let condition = condition.trim();
            if condition.is_empty() {
                continue;
            }
            let parser = ottl::Parser::new(&editors, &converters, &enums, &path_resolvers, condition);

            debug!("Add new parser with condition: \"{}\"", condition);

            parser
                .is_error()
                .map_err(|e| generic_error!("OTTL transform span condition parse error: {}: {}", condition, e))?;
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

/// Synchronous transform that drops spans matching OTTL conditions.
pub struct OttlTransform {
    error_mode: ErrorMode,
    span_parsers: Vec<ottl::Parser<SpanTransformFamily>>,
}

impl OttlTransform {
    /// Returns true if the span should be dropped (any condition matched).
    ///
    /// Uses `self.current_trace` (set in `transform_buffer`) to access resource tags.
    fn should_drop_span(&self, trace: &Trace, span: &Span) -> bool {
        if self.span_parsers.is_empty() {
            return false;
        }

        let mut ctx = SpanTransformContext::new(span, trace.resource_tags());

        for parser in &self.span_parsers {
            match parser.execute(&mut ctx) {
                Ok(Value::Bool(true)) => {
                    //debug!(span_name = %span.name(), "OTTL Transform condition matched; dropping span");
                    return true;
                }
                Ok(Value::Bool(false)) => {
                    //debug!(span_name = %span.name(), "OTTL Transform condition NOT matched; keeping span");
                    continue;
                }
                Ok(_) => continue,
                Err(e) => match self.error_mode {
                    ErrorMode::Ignore => {
                        error!(error = %e, "OTTL Transform condition error; ignoring");
                    }
                    ErrorMode::Silent => {}
                    ErrorMode::Propagate => {
                        //propagate: The processor returns the error up the pipeline. This will result in the payload
                        // being dropped from the collector.
                        // AZH: The current API of SynchronousTransform::transform_buffer does not propagate errors;
                        //  it only logs them.
                        error!(error = %e, "OTTL Transform condition error; dropping span (error_mode=propagate)");
                        return true;
                    }
                },
            }
        }
        false
    }
}

impl SynchronousTransform for OttlTransform {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        for event in event_buffer {
            if let Some(trace) = event.try_as_trace_mut() {
                trace.remove_spans(|trace, span| self.should_drop_span(trace, span));
            }
        }
    }
}
