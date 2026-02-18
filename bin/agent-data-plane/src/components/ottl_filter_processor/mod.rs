//! OTTL filter processor component.
//!
//! Drops spans (and optionally span events) when OTTL conditions match, following the
//! [OpenTelemetry filterprocessor] spec.
//!
//! [OpenTelemetry filterprocessor]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/filterprocessor

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use ottl::{CallbackMap, EnumMap, EvalContext, OttlParser, Value};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    data_model::event::trace::Span,
    topology::EventsBuffer,
};
use saluki_error::{generic_error, GenericError};
use tracing::{debug, trace};

mod config;
mod span_context;

use config::{ErrorMode, OttlFilterConfig};
use span_context::SpanFilterContext;

/// Configuration for the OTTL filter processor, loaded from the data plane config.
#[derive(Clone, Debug)]
pub struct OttlFilterConfiguration {
    /// Parsed filter config (error_mode, traces.span, etc.).
    config: OttlFilterConfig,
}

impl OttlFilterConfiguration {
    /// Creates configuration from the given generic configuration.
    ///
    /// Reads the OTTL filter config from either:
    /// - `data_plane.otlp.filter` (ADP-specific path), or
    /// - `processors.filter/ottl` (OpenTelemetry Collector-style path, same structure as filterprocessor).
    ///
    /// The YAML structure under either path is the same: `error_mode`, `traces.span` (list of OTTL condition strings).
    /// If neither key is present, a default (no-op) config is used.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let filter_config = config
            .try_get_typed::<OttlFilterConfig>("data_plane.otlp.filter")?
            .or_else(|| {
                config
                    .try_get_typed::<OttlFilterConfig>("processors.filter/ottl")
                    .ok()
                    .flatten()
            });
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
    span_parsers: Vec<ottl::Parser>,
}

impl OttlFilter {
    /// Returns true if the span should be dropped (any condition matched).
    fn should_drop_span(&self, span: &Span, resource_tags: &saluki_context::tags::SharedTagSet) -> bool {
        if self.span_parsers.is_empty() {
            return false;
        }

        let ctx: EvalContext = Box::new(SpanFilterContext {
            span: span as *const _,
            resource_tags: resource_tags as *const _,
        });
        let mut ctx = ctx;

        for parser in &self.span_parsers {
            match parser.execute(&mut ctx) {
                Ok(Value::Bool(true)) => {
                    trace!(span_name = %span.name(), "OTTL filter condition matched; dropping span");
                    return true;
                }
                Ok(Value::Bool(false)) => continue,
                Ok(_) => continue,
                Err(e) => match self.error_mode {
                    ErrorMode::Ignore => {
                        debug!(error = %e, "OTTL filter condition error; ignoring");
                    }
                    ErrorMode::Silent => {}
                    ErrorMode::Propagate => {
                        debug!(error = %e, "OTTL filter condition error; dropping span");
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
                let resource_tags = trace.resource_tags().clone();
                trace.retain_spans(|span| !self.should_drop_span(span, &resource_tags));
            }
        }
    }
}
