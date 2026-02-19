//! OTTL evaluation context for span filtering.
//!
//! Provides path accessors for span fields (name, resource, service, attributes,
//! resource.attributes) so OTTL conditions can be evaluated against the current span.
//! Uses a generic context type so no type erasure or allocation is required: the
//! context is passed by reference with minimal copying.
//!
//! To avoid copying span or resource data, the context holds pointers that are
//! created from references in the caller; the only unsafe dereference is isolated
//! in [`SpanFilterContext::with_borrow`].
//!
//! **Why unsafe cannot be removed** (without changing the OTTL library): the parser
//! is built once and stored as `Parser<SpanFilterContext>`; its context type `C`
//! must not carry a lifetime. Each call to `should_drop_span(span, resource_tags)`
//! has references with a call-specific lifetime. To pass those into the parser
//! without copying, we must erase the lifetime (raw pointers) and dereference once
//! inside a controlled API. Using `SpanFilterContext<'a>` with references would
//! require storing a parser that is generic over `'a`, which Rust's type system
//! does not allow in a single `Vec` without higher-rank or library support.

use std::collections::HashMap;
use std::sync::Arc;

use ottl::{IndexExpr, PathAccessor, PathResolverMap, Value};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace::Span;

/// Context holding the current span and trace resource tags for OTTL evaluation.
///
/// Used when evaluating filter conditions: the same context is passed to each
/// condition with the current span and the trace's resource tags. Created from
/// references via [`SpanFilterContext::new`]; access to the span and tags is
/// through [`SpanFilterContext::with_borrow`] so that the single dereference
/// is in one place.
///
/// # Send
///
/// Implemented as `Send` so that `Parser<SpanFilterContext>` can be stored in
/// the transform (which is `Send`). No `SpanFilterContext` value is ever sent
/// across threads; it is only created on the stack in `should_drop_span` and
/// used there.
pub struct SpanFilterContext {
    span: *const Span,
    resource_tags: *const SharedTagSet,
}

impl SpanFilterContext {
    /// Creates a context from references to the current span and resource tags.
    ///
    /// The pointers are created with [`std::ptr::from_ref`]; no allocation or copy.
    /// The caller must use this context only while `span` and `resource_tags` are
    /// valid and only on the same thread.
    #[inline]
    pub fn new(span: &Span, resource_tags: &SharedTagSet) -> Self {
        Self {
            span: std::ptr::from_ref(span),
            resource_tags: std::ptr::from_ref(resource_tags),
        }
    }

    /// Runs a closure with borrowed references to the span and resource tags.
    ///
    /// This is the only place where the stored pointers are dereferenced. See the
    /// module docs for why this cannot be replaced with a fully safe design without
    /// changing the OTTL library.
    ///
    /// # Safety
    ///
    /// The pointers must be valid for the duration of the call and must not be
    /// used from another thread. This is guaranteed when the context is created
    /// with [`SpanFilterContext::new`] and used only in the same stack frame.
    #[inline]
    pub fn with_borrow<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Span, &SharedTagSet) -> R,
    {
        // SAFETY: SpanFilterContext is only constructed from references in the same
        // call stack (should_drop_span) and is used synchronously before returning.
        // The pointers are not sent across threads; see Send impl and module docs.
        unsafe { f(&*self.span, &*self.resource_tags) }
    }
}

unsafe impl Send for SpanFilterContext {}

/// Path accessor for the span's `attributes` path (span-level metadata).
#[derive(Debug)]
pub struct SpanAttributesAccessor;

impl PathAccessor<SpanFilterContext> for SpanAttributesAccessor {
    fn get(&self, ctx: &SpanFilterContext, _path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        ctx.with_borrow(|span, _resource_tags| {
            let value = if let Some(IndexExpr::String(key)) = indexes.first() {
                span.meta()
                    .get(key.as_str())
                    .map(|v| Value::string(v.as_ref()))
                    .unwrap_or(Value::Nil)
            } else {
                Value::Nil
            };
            Ok(value)
        })
    }

    fn set(
        &self, _ctx: &mut SpanFilterContext, path: &str, _indexes: &[IndexExpr], _value: &Value,
    ) -> ottl::Result<()> {
        Err(format!("Filter context is read-only; cannot set path {}", path).into())
    }
}

/// Path accessor for the `resource.attributes` path (trace resource tags).
#[derive(Debug)]
pub struct ResourceAttributesAccessor;

impl PathAccessor<SpanFilterContext> for ResourceAttributesAccessor {
    fn get(&self, ctx: &SpanFilterContext, _path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        ctx.with_borrow(|_span, resource_tags| {
            let value = if let Some(IndexExpr::String(key)) = indexes.first() {
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
            };
            Ok(value)
        })
    }

    fn set(
        &self, _ctx: &mut SpanFilterContext, path: &str, _indexes: &[IndexExpr], _value: &Value,
    ) -> ottl::Result<()> {
        Err(format!("Filter context is read-only; cannot set path {}", path).into())
    }
}

/// Builds the path resolver map for span filter conditions.
///
/// Registers accessors for: attributes, resource.attributes.
/// These paths match the OTTL Span context used by the OpenTelemetry filterprocessor.
pub fn span_filter_path_resolvers() -> PathResolverMap<SpanFilterContext> {
    let attributes_accessor: Arc<dyn PathAccessor<SpanFilterContext> + Send + Sync> =
        Arc::new(SpanAttributesAccessor);
    let resource_attributes_accessor: Arc<dyn PathAccessor<SpanFilterContext> + Send + Sync> =
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
