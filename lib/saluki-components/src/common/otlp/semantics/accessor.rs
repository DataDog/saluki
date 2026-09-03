//! Type-strict attribute accessors—port of upstream `pkg/trace/semantics/lookup_pdata.go`.
//!
//! Accessors separate the lookup algorithm from the underlying storage. Each
//! getter returns a value only when the stored OTLP attribute matches the
//! requested type exactly; string-to-numeric conversion is the lookup layer's
//! responsibility.

use otlp_protos::opentelemetry::proto::common::v1::{self as otlp_common, any_value::Value};
use saluki_core::data_model::event::trace::{AttributeValue, Span as DdSpan};

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

/// Accessor over a converted span's typed attribute map.
///
/// Port of upstream's `DDSpanAccessor`/`DDSpanAccessorV1`: after conversion to
/// the trace payload format, a span carries string-valued attributes (the
/// equivalent of the upstream `meta` map) and numeric attributes (the
/// equivalent of `metrics`). Attribute routing is strictly typed:
///
/// - `get_string` returns a value only for string-valued attributes.
/// - `get_float64` returns a value only for float-valued attributes.
/// - `get_int64` returns a value for integer-valued attributes, and for
///   float-valued attributes only when the value is exactly representable as an
///   integer (for example: `14.0` yes, `13.5` no).
#[derive(Clone, Copy)]
pub struct DdSpanAccessor<'a> {
    span: &'a DdSpan,
}

impl<'a> DdSpanAccessor<'a> {
    /// Creates a new accessor over the given converted span.
    pub fn new(span: &'a DdSpan) -> Self {
        Self { span }
    }
}

impl<'a> Accessor for DdSpanAccessor<'a> {
    fn get_string(&self, key: &str) -> Option<&str> {
        match self.span.attributes.get(key)? {
            AttributeValue::String(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    fn get_int64(&self, key: &str) -> Option<i64> {
        match self.span.attributes.get(key)? {
            AttributeValue::Int(i) => Some(*i),
            // Exact-integer check, mirroring upstream: a float is only usable as
            // an integer when converting it back to a float round-trips.
            AttributeValue::Float(f) => {
                let i = *f as i64;
                (i as f64 == *f).then_some(i)
            }
            _ => None,
        }
    }

    fn get_float64(&self, key: &str) -> Option<f64> {
        match self.span.attributes.get(key)? {
            AttributeValue::Float(f) => Some(*f),
            _ => None,
        }
    }
}

/// Composite accessor for span + resource attributes with span precedence.
///
/// Mirrors upstream's `OTelSpanAccessor`—for each getter, the span's
/// attributes are checked first, then the resource's attributes as a fallback.
#[derive(Clone, Copy)]
pub struct OtelSpanAccessor<'a> {
    span: OtlpAttributesAccessor<'a>,
    resource: OtlpAttributesAccessor<'a>,
}

impl<'a> OtelSpanAccessor<'a> {
    pub fn new(span_attributes: &'a [otlp_common::KeyValue], resource_attributes: &'a [otlp_common::KeyValue]) -> Self {
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
    use saluki_core::data_model::event::trace::Span as DdSpan;
    use stringtheory::MetaString;

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

    fn converted_span_with(attrs: &[(&'static str, saluki_core::data_model::event::trace::AttributeValue)]) -> DdSpan {
        let mut span = DdSpan::new("svc", "op", "res", "web", 1, 0, 0, 0, 0);
        for (key, value) in attrs {
            span.attributes.insert(MetaString::from(*key), value.clone());
        }
        span
    }

    #[test]
    fn dd_span_accessor_reads_strings_and_numbers() {
        let span = converted_span_with(&[
            ("env", AttributeValue::String(MetaString::from("prod"))),
            ("_dd.top_level", AttributeValue::Float(1.0)),
            ("_sampling_priority_v1", AttributeValue::Int(2)),
        ]);
        let a = DdSpanAccessor::new(&span);

        assert_eq!(a.get_string("env"), Some("prod"));
        assert_eq!(a.get_float64("_dd.top_level"), Some(1.0));
        assert_eq!(a.get_int64("_sampling_priority_v1"), Some(2));
        assert_eq!(a.get_string("nope"), None);
    }

    #[test]
    fn dd_span_accessor_is_type_strict() {
        // Strings are only readable as strings; numbers of one type are not
        // readable as the other numeric type except via the exact-integer rule.
        let span = converted_span_with(&[
            ("str_key", AttributeValue::String(MetaString::from("hello"))),
            ("int_key", AttributeValue::Int(42)),
            ("float_key", AttributeValue::Float(1.5)),
        ]);
        let a = DdSpanAccessor::new(&span);

        assert_eq!(a.get_int64("str_key"), None);
        assert_eq!(a.get_float64("str_key"), None);
        assert_eq!(a.get_float64("int_key"), None);
        assert_eq!(a.get_string("int_key"), None);
    }

    #[test]
    fn dd_span_accessor_exact_integer_check_on_int64() {
        // Floats stored in the metrics-style attributes are usable as integers
        // only when exactly representable: 14.0 yes, 13.5 no.
        let exact = converted_span_with(&[("grpc.code", AttributeValue::Float(14.0))]);
        let fractional = converted_span_with(&[("grpc.code", AttributeValue::Float(13.5))]);

        assert_eq!(DdSpanAccessor::new(&exact).get_int64("grpc.code"), Some(14));
        assert_eq!(DdSpanAccessor::new(&fractional).get_int64("grpc.code"), None);
    }

    #[test]
    fn dd_span_accessor_resolves_concepts_via_lookup() {
        use crate::common::otlp::semantics::{lookup_int64, lookup_string, Concept};

        // End-to-end: registry lookups work against a converted span, including
        // a `when`-gated fallback.
        let span = converted_span_with(&[
            ("rpc.response.status_code", AttributeValue::Int(14)),
            ("rpc.system", AttributeValue::String(MetaString::from("grpc"))),
            ("env", AttributeValue::String(MetaString::from("prod"))),
        ]);
        let a = DdSpanAccessor::new(&span);

        assert_eq!(
            lookup_string(&crate::common::otlp::semantics::REGISTRY, &a, Concept::DdEnv).as_deref(),
            Some("prod"),
        );
        assert_eq!(
            lookup_int64(
                &crate::common::otlp::semantics::REGISTRY,
                &a,
                Concept::RpcGrpcStatusCode
            ),
            Some(14),
        );
    }
}
