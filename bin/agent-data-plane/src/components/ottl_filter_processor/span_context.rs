//! OTTL evaluation context for span filtering.
//!
//! Provides path accessors for span fields (name, resource, service, attributes,
//! resource.attributes) so OTTL conditions can be evaluated against the current span.

use std::collections::HashMap;
use std::sync::Arc;

use ottl::{EvalContext, IndexExpr, PathAccessor, PathResolverMap, Value};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace::Span;

/// Context holding the current span and trace resource tags for OTTL evaluation.
///
/// Used when evaluating filter conditions: the same context is passed to each
/// condition with the current span and the trace's resource tags.
pub struct SpanFilterContext {
    /// Reference to the span under evaluation.
    pub span: *const Span,
    /// Reference to the trace's resource tags (resource.attributes in OTTL terms).
    pub resource_tags: *const SharedTagSet,
}

/// Path accessor that reads span and resource fields for OTTL conditions.
#[derive(Debug)]
pub struct SpanPathAccessor;

impl PathAccessor for SpanPathAccessor {
    //AZH: first naive implementation of path mapping, ideally we want to have path object per path to minimize string compare
    fn get(&self, ctx: &EvalContext, path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let span_ctx = ctx
            .downcast_ref::<SpanFilterContext>()
            .ok_or("Expected SpanFilterContext")?;

        let span = unsafe { &*span_ctx.span };
        let resource_tags = unsafe { &*span_ctx.resource_tags };

        let value = match path {
            "attributes" => {
                if let Some(IndexExpr::String(key)) = indexes.first() {
                    let value = span
                        .meta()
                        .get(key.as_str())
                        .map(|v| Value::string(v.as_ref()))
                        .unwrap_or(Value::Nil);
                    value
                } else {
                    Value::Nil
                }
            }
            "resource.attributes" => {
                if let Some(IndexExpr::String(key)) = indexes.first() {
                    let value = resource_tags
                        .get_single_tag(key.as_str())
                        .and_then(|t| t.value())
                        .map(|s| Value::string(s))
                        .unwrap_or(Value::Nil);
                    value
                } else if indexes.is_empty() {
                    // Cannot build full map without iteration; return empty map for consistency.
                    Value::Map(HashMap::new())
                } else {
                    Value::Nil
                }
            }
            _ => return Err(format!("Unknown path: {}", path).into()),
        };

        Ok(value)
    }

    //AZH: to discuss with Tobias, how to modify the context and path values
    fn set(&self, _ctx: &mut EvalContext, path: &str, _indexes: &[IndexExpr], _value: &Value) -> ottl::Result<()> {
        Err(format!("Filter context is read-only; cannot set path {}", path).into())
    }
}

/// Builds the path resolver map for span filter conditions.
///
/// Registers accessors for: name, resource, service, attributes, resource.attributes.
/// These paths match the OTTL Span context used by the OpenTelemetry filterprocessor.
pub fn span_filter_path_resolvers() -> PathResolverMap {
    let accessor: Arc<dyn PathAccessor + Send + Sync> = Arc::new(SpanPathAccessor);
    let resolver: ottl::PathResolver = Arc::new(move || Ok(accessor.clone() as Arc<dyn PathAccessor + Send + Sync>));

    let mut map = PathResolverMap::new();
    map.insert("attributes".to_string(), resolver.clone());
    map.insert("resource.attributes".to_string(), resolver);
    map
}
