use std::collections::HashMap;
use std::sync::Arc;

use ottl::{EvalContextFamily, Field, IndexExpr, PathAccessor, PathResolverMap, Value};
use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{AttributeValue, Span};
use stringtheory::MetaString;

/// Family type for the span transform evaluation context.
///
/// The parser is stored as `Parser<SpanTransformFamily>`. At execution time, a [`SpanTransformContext<'a>`] is created
/// with references valid for the duration of the statement evaluation.
pub struct SpanTransformFamily;

impl EvalContextFamily for SpanTransformFamily {
    type Context<'a> = SpanTransformContext<'a>;
}

/// Span context for Saluki trace events.
pub struct SpanTransformContext<'a> {
    /// Mutable reference to the span being transformed.
    pub(super) span: &'a mut Span,

    /// Reference to the trace's resource-level attributes (read-only).
    pub(super) resource_attrs: &'a FastHashMap<MetaString, AttributeValue>,
}

impl<'a> SpanTransformContext<'a> {
    /// Creates a context from a mutable span reference and immutable resource attributes.
    #[inline]
    pub fn new(span: &'a mut Span, resource_attrs: &'a FastHashMap<MetaString, AttributeValue>) -> Self {
        Self { span, resource_attrs }
    }
}

/// Path accessor for `attributes` (span-level string metadata).
///
/// Reads from and writes to the span's `meta` map, which stores `MetaString` key-value pairs. On `set`, string values
/// are inserted directly; `Nil` removes the key; other value types are converted to their display representation.
#[derive(Debug)]
pub struct SpanAttributesAccessor;

impl PathAccessor<SpanTransformFamily> for SpanAttributesAccessor {
    fn get<'a>(&self, ctx: &SpanTransformContext<'a>, fields: &[Field]) -> ottl::Result<Value> {
        let value = if let Some(IndexExpr::String(key)) = fields.first().and_then(|f| f.keys.first()) {
            ctx.span
                .attributes
                .get(key.as_str())
                .and_then(AttributeValue::as_string)
                .map(|v| Value::string(v.as_ref()))
                .unwrap_or(Value::Nil)
        } else {
            Value::Nil
        };
        Ok(value)
    }

    fn set<'a>(&self, ctx: &mut SpanTransformContext<'a>, fields: &[Field], value: &Value) -> ottl::Result<()> {
        if let Some(IndexExpr::String(key)) = fields.first().and_then(|f| f.keys.first()) {
            match value {
                Value::Nil => {
                    ctx.span.attributes.remove(key.as_str());
                }
                Value::String(s) => {
                    ctx.span.attributes.insert(
                        MetaString::from(key.as_str()),
                        AttributeValue::String(MetaString::from(Arc::clone(s))),
                    );
                }
                Value::Int(n) => {
                    ctx.span.attributes.insert(
                        MetaString::from(key.as_str()),
                        AttributeValue::String(MetaString::from(n.to_string().as_str())),
                    );
                }
                Value::Float(f) => {
                    ctx.span.attributes.insert(
                        MetaString::from(key.as_str()),
                        AttributeValue::String(MetaString::from(f.to_string().as_str())),
                    );
                }
                Value::Bool(b) => {
                    ctx.span.attributes.insert(
                        MetaString::from(key.as_str()),
                        AttributeValue::String(MetaString::from(if *b { "true" } else { "false" })),
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
            Err("set on attributes requires a string index, for example, attributes[\"key\"]".into())
        }
    }
}

/// Path accessor for `resource.attributes` (trace resource tags).
///
/// Read-only: [`TagSet`] doesn't expose mutable access, so `set` returns an error.
#[derive(Debug)]
pub struct ResourceAttributesAccessor;

impl PathAccessor<SpanTransformFamily> for ResourceAttributesAccessor {
    fn get<'a>(&self, ctx: &SpanTransformContext<'a>, fields: &[Field]) -> ottl::Result<Value> {
        let attrs_field = fields.get(1);
        let value = if let Some(IndexExpr::String(key)) = attrs_field.and_then(|f| f.keys.first()) {
            ctx.resource_attrs
                .get(key.as_str())
                .and_then(AttributeValue::as_string)
                .map(|v| Value::string(v.as_ref()))
                .unwrap_or(Value::Nil)
        } else if attrs_field.is_none_or(|f| f.keys.is_empty()) {
            Value::Map(HashMap::new())
        } else {
            Value::Nil
        };
        Ok(value)
    }

    fn set<'a>(&self, _ctx: &mut SpanTransformContext<'a>, fields: &[Field], _value: &Value) -> ottl::Result<()> {
        // TODO: Add support for mutating tags so we can allow setting them here.
        let path_str: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        Err(format!(
            "resource.attributes is read-only; setting path `{}` is not supported because TagSet does not expose mutable access",
            path_str
        )
        .into())
    }
}

/// Builds the path resolver map for span transform statements.
///
/// Registers accessors for `attributes` (read-write) and `resource.attributes` (read-only). These paths match the OTTL
/// span context used by the OpenTelemetry transform processor.
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
