//! OTTL evaluation context for the transform processor.
//!
//! Provides path accessors for span fields (`attributes`, `resource.attributes`) so OTTL
//! statements (e.g. `set(attributes["key"], "value")`) can read and write span data.
//!
//! Uses the OTTL library's [`EvalContextFamily`] (GAT-based) design: the parser is
//! parameterized by [`SpanTransformFamily`], and at execution time a short-lived
//! [`SpanTransformContext<'a>`] is created from a mutable reference to the span and an
//! immutable reference to the resource tags.
//!
//! `attributes` supports both read and write via [`SpanAttributesAccessor`].
//! `resource.attributes` is read-only because [`SharedTagSet`] does not expose mutable access.

use std::collections::HashMap;
use std::sync::Arc;

use ottl::{EvalContextFamily, IndexExpr, PathAccessor, PathResolverMap, Value};
use saluki_context::tags::SharedTagSet;
use saluki_core::data_model::event::trace::Span;
use stringtheory::MetaString;

/// Family type for the span transform evaluation context.
///
/// The parser is stored as `Parser<SpanTransformFamily>`. At execution time, a
/// [`SpanTransformContext<'a>`] is created with references valid for the duration
/// of the statement evaluation.
pub struct SpanTransformFamily;

impl EvalContextFamily for SpanTransformFamily {
    type Context<'a> = SpanTransformContext<'a>;
}

/// Context holding a mutable reference to the current span and an immutable reference to
/// the trace resource tags for OTTL evaluation.
///
/// Created on the stack in `transform_span` with the current span and the trace's resource
/// tags, then passed to each statement parser. The mutable span reference allows editor
/// functions like `set` to modify span attributes in place.
pub struct SpanTransformContext<'a> {
    /// Mutable reference to the span being transformed.
    pub(super) span: &'a mut Span,
    /// Reference to the trace's resource-level tags (read-only).
    pub(super) resource_tags: &'a SharedTagSet,
}

impl<'a> SpanTransformContext<'a> {
    /// Creates a context from a mutable span reference and immutable resource tags.
    #[inline]
    pub fn new(span: &'a mut Span, resource_tags: &'a SharedTagSet) -> Self {
        Self { span, resource_tags }
    }
}

/// Path accessor for `attributes` (span-level string metadata).
///
/// Reads from and writes to the span's `meta` map, which stores `MetaString` key-value
/// pairs. On `set`, string values are inserted directly; `Nil` removes the key; other
/// value types are converted to their display representation.
#[derive(Debug)]
pub struct SpanAttributesAccessor;

impl PathAccessor<SpanTransformFamily> for SpanAttributesAccessor {
    fn get<'a>(&self, ctx: &SpanTransformContext<'a>, _path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
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
        &self, ctx: &mut SpanTransformContext<'a>, _path: &str, indexes: &[IndexExpr], value: &Value,
    ) -> ottl::Result<()> {
        if let Some(IndexExpr::String(key)) = indexes.first() {
            match value {
                Value::Nil => {
                    ctx.span.meta_mut().remove(key.as_str());
                }
                Value::String(s) => {
                    ctx.span
                        .meta_mut()
                        .insert(MetaString::from(key.as_str()), MetaString::from(Arc::clone(s)));
                }
                Value::Int(n) => {
                    ctx.span
                        .meta_mut()
                        .insert(MetaString::from(key.as_str()), MetaString::from(n.to_string().as_str()));
                }
                Value::Float(f) => {
                    ctx.span
                        .meta_mut()
                        .insert(MetaString::from(key.as_str()), MetaString::from(f.to_string().as_str()));
                }
                Value::Bool(b) => {
                    ctx.span.meta_mut().insert(
                        MetaString::from(key.as_str()),
                        MetaString::from(if *b { "true" } else { "false" }),
                    );
                }
                _ => {
                    return Err(
                        format!("set on attributes[\"{}\"] does not support value type {:?}", key, value).into(),
                    );
                }
            }
            Ok(())
        } else {
            Err("set on attributes requires a string index, e.g. attributes[\"key\"]".into())
        }
    }
}

/// Path accessor for `resource.attributes` (trace resource tags).
///
/// Read-only: [`SharedTagSet`] does not expose mutable access, so `set` returns an error.
#[derive(Debug)]
pub struct ResourceAttributesAccessor;

impl PathAccessor<SpanTransformFamily> for ResourceAttributesAccessor {
    fn get<'a>(&self, ctx: &SpanTransformContext<'a>, _path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let value = if let Some(IndexExpr::String(key)) = indexes.first() {
            ctx.resource_tags
                .get_single_tag(key.as_str())
                .and_then(|t| t.value())
                .map(Value::string)
                .unwrap_or(Value::Nil)
        } else if indexes.is_empty() {
            Value::Map(HashMap::new())
        } else {
            Value::Nil
        };
        Ok(value)
    }

    /// AZH: TODO
    /// Always returns an error: `SharedTagSet` is an Arc-based immutable type and `Trace`
    /// does not expose yet mutable way to access resource_tags, so there is no way to write changes
    /// back to the trace's resource tags.
    fn set<'a>(
        &self, _ctx: &mut SpanTransformContext<'a>, path: &str, _indexes: &[IndexExpr], _value: &Value,
    ) -> ottl::Result<()> {
        Err(format!(
            "resource.attributes is read-only; setting path `{}` is not supported because SharedTagSet does not expose mutable access",
            path
        )
        .into())
    }
}

/// Builds the path resolver map for span transform statements.
///
/// Registers accessors for `attributes` (read-write) and `resource.attributes` (read-only).
/// These paths match the OTTL span context used by the OpenTelemetry transform processor.
pub fn span_transform_path_resolvers() -> PathResolverMap<SpanTransformFamily> {
    let attributes_accessor: Arc<dyn PathAccessor<SpanTransformFamily> + Send + Sync> =
        Arc::new(SpanAttributesAccessor);
    let resource_attributes_accessor: Arc<dyn PathAccessor<SpanTransformFamily> + Send + Sync> =
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
