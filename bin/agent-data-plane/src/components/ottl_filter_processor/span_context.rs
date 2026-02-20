//! OTTL evaluation context for span filtering.
//!
//! Provides path accessors for span fields (attributes, resource.attributes) so OTTL
//! conditions can be evaluated against the current span. Uses the OTTL library's
//! [`EvalContextFamily`] (GAT-based) design: the parser is parameterized by
//! [`SpanFilterFamily`], and at execution time a short-lived [`SpanFilterContext<'a>`]
//! is created from references to the span and resource tags. No unsafe code, raw
//! pointers, or data copying are required.

use std::collections::HashMap;
use std::sync::Arc;

use ottl::{EvalContextFamily, IndexExpr, PathAccessor, PathResolverMap, Value};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace::Span;

/// Family type for the span filter evaluation context.
///
/// The parser is stored as `Parser<SpanFilterFamily>`. At execution time, a
/// [`SpanFilterContext<'a>`] is created with references valid for the duration
/// of the condition evaluation.
pub struct SpanFilterFamily;

impl EvalContextFamily for SpanFilterFamily {
    type Context<'a> = SpanFilterContext<'a>;
}

/// Context holding references to the current span and trace resource tags for OTTL evaluation.
///
/// Used when evaluating filter conditions: created on the stack in `should_drop_span`
/// with the current span and the trace's resource tags, then passed to each condition
/// parser. No copying; the context only holds references for the duration of the call.
pub struct SpanFilterContext<'a> {
    /// Reference to the span being evaluated.
    pub(super) span: &'a Span,
    /// Reference to the trace's resource-level tags.
    pub(super) resource_tags: &'a SharedTagSet,
}

impl<'a> SpanFilterContext<'a> {
    /// Creates a context from references to the current span and resource tags.
    #[inline]
    pub fn new(span: &'a Span, resource_tags: &'a SharedTagSet) -> Self {
        Self { span, resource_tags }
    }
}

/// Path accessor for the span's `attributes` path (span-level metadata).
#[derive(Debug)]
pub struct SpanAttributesAccessor;

impl PathAccessor<SpanFilterFamily> for SpanAttributesAccessor {
    fn get<'a>(&self, ctx: &SpanFilterContext<'a>, _path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let value = if let Some(IndexExpr::String(key)) = indexes.first() {
            ctx.span
                .meta()
                .get(key.as_str())
                .map(|v| Value::string(v.as_ref()))
                .unwrap_or(Value::Nil)
        } else {
            Value::Nil
        };
        Ok(value)
    }

    fn set<'a>(
        &self, _ctx: &mut SpanFilterContext<'a>, path: &str, _indexes: &[IndexExpr], _value: &Value,
    ) -> ottl::Result<()> {
        Err(format!("Filter context is read-only; cannot set path {}", path).into())
    }
}

/// Path accessor for the `resource.attributes` path (trace resource tags).
#[derive(Debug)]
pub struct ResourceAttributesAccessor;

impl PathAccessor<SpanFilterFamily> for ResourceAttributesAccessor {
    fn get<'a>(&self, ctx: &SpanFilterContext<'a>, _path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let value = if let Some(IndexExpr::String(key)) = indexes.first() {
            ctx.resource_tags
                .get_single_tag(key.as_str())
                .and_then(|t| t.value())
                .map(Value::string)
                .unwrap_or(Value::Nil)
        } else if indexes.is_empty() {
            // Cannot build full map without iteration; return empty map for consistency.
            Value::Map(HashMap::new())
        } else {
            Value::Nil
        };
        Ok(value)
    }

    fn set<'a>(
        &self, _ctx: &mut SpanFilterContext<'a>, path: &str, _indexes: &[IndexExpr], _value: &Value,
    ) -> ottl::Result<()> {
        Err(format!("Filter context is read-only; cannot set path {}", path).into())
    }
}

/// Builds the path resolver map for span filter conditions.
///
/// Registers accessors for: attributes, resource.attributes.
/// These paths match the OTTL Span context used by the OpenTelemetry filterprocessor.
pub fn span_filter_path_resolvers() -> PathResolverMap<SpanFilterFamily> {
    let attributes_accessor: Arc<dyn PathAccessor<SpanFilterFamily> + Send + Sync> = Arc::new(SpanAttributesAccessor);
    let resource_attributes_accessor: Arc<dyn PathAccessor<SpanFilterFamily> + Send + Sync> =
        Arc::new(ResourceAttributesAccessor);

    let mut map = PathResolverMap::new();
    map.insert(
        "attributes".to_string(),
        Arc::new(move || Ok(attributes_accessor.clone())),
    );
    map.insert(
        "resource.attributes".to_string(),
        Arc::new(move || Ok(resource_attributes_accessor.clone())),
    );
    map
}
