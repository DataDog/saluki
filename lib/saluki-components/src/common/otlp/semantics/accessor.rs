//! Type-strict attribute accessors — port of upstream `pkg/trace/semantics/lookup_pdata.go`.
//!
//! Accessors separate the lookup algorithm from the underlying storage. Each
//! getter returns a value only when the stored OTLP attribute matches the
//! requested type exactly; string-to-numeric conversion is the lookup layer's
//! responsibility.

use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};

/// An abstract view over a bag of typed attributes.
pub trait Accessor {
    fn get_string(&self, key: &str) -> Option<&str>;
    fn get_int64(&self, key: &str) -> Option<i64>;
    fn get_float64(&self, key: &str) -> Option<f64>;
}

/// Accessor over a slice of OTLP [`KeyValue`](otlp_common::KeyValue) attributes.
#[derive(Clone, Copy)]
pub struct OtlpAttributesAccessor<'a> {
    attrs: &'a [otlp_common::KeyValue],
}

impl<'a> OtlpAttributesAccessor<'a> {
    pub fn new(attrs: &'a [otlp_common::KeyValue]) -> Self {
        Self { attrs }
    }

    fn find(&self, key: &str) -> Option<&'a Value> {
        self.attrs
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.as_ref())
    }
}

impl<'a> Accessor for OtlpAttributesAccessor<'a> {
    fn get_string(&self, key: &str) -> Option<&str> {
        match self.find(key)? {
            Value::StringValue(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn get_int64(&self, key: &str) -> Option<i64> {
        match self.find(key)? {
            Value::IntValue(i) => Some(*i),
            _ => None,
        }
    }

    fn get_float64(&self, key: &str) -> Option<f64> {
        match self.find(key)? {
            Value::DoubleValue(f) => Some(*f),
            _ => None,
        }
    }
}

/// Composite accessor for span + resource attributes with span precedence.
///
/// Mirrors upstream's `OTelSpanAccessor` — for each getter, the span's
/// attributes are checked first, then the resource's attributes as a fallback.
#[derive(Clone, Copy)]
pub struct OtelSpanAccessor<'a> {
    span: OtlpAttributesAccessor<'a>,
    resource: OtlpAttributesAccessor<'a>,
}

impl<'a> OtelSpanAccessor<'a> {
    pub fn new(
        span_attributes: &'a [otlp_common::KeyValue], resource_attributes: &'a [otlp_common::KeyValue],
    ) -> Self {
        Self {
            span: OtlpAttributesAccessor::new(span_attributes),
            resource: OtlpAttributesAccessor::new(resource_attributes),
        }
    }
}

impl<'a> Accessor for OtelSpanAccessor<'a> {
    fn get_string(&self, key: &str) -> Option<&str> {
        self.span.get_string(key).or_else(|| self.resource.get_string(key))
    }

    fn get_int64(&self, key: &str) -> Option<i64> {
        self.span.get_int64(key).or_else(|| self.resource.get_int64(key))
    }

    fn get_float64(&self, key: &str) -> Option<f64> {
        self.span.get_float64(key).or_else(|| self.resource.get_float64(key))
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::{AnyValue, KeyValue};

    use super::*;

    fn kv(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    #[test]
    fn otlp_attributes_accessor_returns_only_matching_type() {
        let attrs = vec![
            kv("int_key", Value::IntValue(42)),
            kv("str_key", Value::StringValue("hello".into())),
            kv("dbl_key", Value::DoubleValue(1.5)),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);

        assert_eq!(a.get_int64("int_key"), Some(42));
        assert_eq!(a.get_int64("str_key"), None);
        assert_eq!(a.get_int64("dbl_key"), None);

        assert_eq!(a.get_string("str_key"), Some("hello"));
        assert_eq!(a.get_string("int_key"), None);

        assert_eq!(a.get_float64("dbl_key"), Some(1.5));
        assert_eq!(a.get_float64("int_key"), None);
    }

    #[test]
    fn otlp_attributes_accessor_returns_none_for_missing_key() {
        let attrs: Vec<KeyValue> = vec![];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(a.get_string("nope"), None);
        assert_eq!(a.get_int64("nope"), None);
        assert_eq!(a.get_float64("nope"), None);
    }

    #[test]
    fn otel_span_accessor_prefers_span_over_resource() {
        let span = vec![kv("k", Value::IntValue(1))];
        let resource = vec![kv("k", Value::IntValue(2))];
        let a = OtelSpanAccessor::new(&span, &resource);
        assert_eq!(a.get_int64("k"), Some(1));
    }

    #[test]
    fn otel_span_accessor_falls_back_to_resource() {
        let span: Vec<KeyValue> = vec![];
        let resource = vec![kv("k", Value::StringValue("r".into()))];
        let a = OtelSpanAccessor::new(&span, &resource);
        assert_eq!(a.get_string("k"), Some("r"));
    }

    #[test]
    fn otel_span_accessor_per_method_precedence() {
        // Span has the key as an int, resource has it as a string. A caller
        // that asks for a string should still find the resource's value
        // rather than seeing the span's wrong-typed entry mask it.
        let span = vec![kv("k", Value::IntValue(1))];
        let resource = vec![kv("k", Value::StringValue("r".into()))];
        let a = OtelSpanAccessor::new(&span, &resource);
        assert_eq!(a.get_int64("k"), Some(1));
        assert_eq!(a.get_string("k"), Some("r"));
    }
}
