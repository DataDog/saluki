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
    /// Reads the OTTL filter config from one of (first present wins):
    /// - `data_plane.otlp.filter` (ADP-specific path),
    /// - `processors.filter/ottl` (OpenTelemetry Collector-style path), or
    /// - `ottl_config` (top-level key, e.g. in test/one-off/ottl-filter-processor.yaml).
    ///
    /// The YAML structure under any path is the same: `error_mode`, `traces.span` (list of OTTL condition strings).
    /// If none is present, a default (no-op) config is used.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let filter_config = config.try_get_typed::<OttlFilterConfig>("ottl_config")?;
        Ok(Self {
            config: filter_config.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for OttlFilterConfiguration {
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
                trace.retain_spans(|trace, span| !self.should_drop_span(trace, span));
            }
        }
    }
}
