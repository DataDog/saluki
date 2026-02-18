//! OTTL evaluation context for span filtering.
//!
//! Provides path accessors for span fields (name, resource, service, attributes,
//! resource.attributes) so OTTL conditions can be evaluated against the current span.
//! Uses a generic context type so no type erasure or allocation is required: the
//! context is passed by reference with minimal copying.

use std::collections::HashMap;
use std::sync::Arc;

use ottl::{IndexExpr, PathAccessor, PathResolver, PathResolverMap, Value};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace::Span;

/// Context holding the current span and trace resource tags for OTTL evaluation.
///
/// Used when evaluating filter conditions: the same context is passed to each
/// condition with the current span and the trace's resource tags. Holds raw
/// pointers to avoid copying and to work with the OTTL library's generic context
/// type (no `'static` or type erasure required).
///
/// # Safety
///
/// Implemented as `Send` so that `Parser<SpanFilterContext>` can be stored in
/// the transform (which is `Send`). No `SpanFilterContext` value is ever sent
/// across threads; it is only created on the stack in `should_drop_span` and
/// used there. The pointers are only dereferenced on the same thread that
/// created the context.
pub struct SpanFilterContext {
    /// Reference to the span under evaluation.
    pub span: *const Span,
    /// Reference to the trace's resource tags (resource.attributes in OTTL terms).
    pub resource_tags: *const SharedTagSet,
}

unsafe impl Send for SpanFilterContext {}

/// Path accessor that reads span and resource fields for OTTL conditions.
#[derive(Debug)]
pub struct SpanPathAccessor;

impl PathAccessor<SpanFilterContext> for SpanPathAccessor {
    fn get(&self, ctx: &SpanFilterContext, path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let span = unsafe { &*ctx.span };
        let resource_tags = unsafe { &*ctx.resource_tags };

        let value = match path {
            "attributes" => {
                if let Some(IndexExpr::String(key)) = indexes.first() {
                    span.meta()
                        .get(key.as_str())
                        .map(|v| Value::string(v.as_ref()))
                        .unwrap_or(Value::Nil)
                } else {
                    Value::Nil
                }
            }
            "resource.attributes" => {
                if let Some(IndexExpr::String(key)) = indexes.first() {
                    resource_tags
                        .get_single_tag(key.as_str())
                        .and_then(|t| t.value())
                        .map(|s| Value::string(s))
                        .unwrap_or(Value::Nil)
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
    fn set(
        &self, _ctx: &mut SpanFilterContext, path: &str, _indexes: &[IndexExpr], _value: &Value,
    ) -> ottl::Result<()> {
        Err(format!("Filter context is read-only; cannot set path {}", path).into())
    }
}

/// Builds the path resolver map for span filter conditions.
///
/// Registers accessors for: name, resource, service, attributes, resource.attributes.
/// These paths match the OTTL Span context used by the OpenTelemetry filterprocessor.
pub fn span_filter_path_resolvers() -> PathResolverMap<SpanFilterContext> {
    let accessor: Arc<dyn PathAccessor<SpanFilterContext> + Send + Sync> = Arc::new(SpanPathAccessor);
    let resolver: PathResolver<SpanFilterContext> = Arc::new(move || Ok(accessor.clone()));

    let mut map = PathResolverMap::new();
    map.insert("attributes".to_string(), resolver.clone());
    map.insert("resource.attributes".to_string(), resolver);
    map
}
