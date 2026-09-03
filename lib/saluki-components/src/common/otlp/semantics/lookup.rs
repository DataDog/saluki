//! Concept-driven attribute lookups.

use super::accessor::Accessor;
use super::registry::{Condition, Registry, TagInfo, ValueType};
use super::Concept;

/// Reports whether a single condition holds against the accessor's raw attribute store.
///
/// Conditions read attributes by exact key—no fallback chaining—so a condition may
/// be true even if a lookup for the same attribute as a concept would resolve it
/// through a different name.
fn condition_matches<A: Accessor>(accessor: &A, condition: &Condition) -> bool {
    let value = accessor.get_string(&condition.attribute);
    let found = value.is_some_and(|v| !v.is_empty());

    if let Some(present) = condition.present {
        if found != present {
            return false;
        }
    }

    if let Some(expected) = condition.eq.as_deref() {
        if !found || value != Some(expected) {
            return false;
        }
    }

    true
}

/// Reports whether all of a fallback tag's conditions hold (logical AND).
///
/// An empty condition list always matches.
fn conditions_match<A: Accessor>(accessor: &A, tag: &TagInfo) -> bool {
    tag.when.iter().all(|condition| condition_matches(accessor, condition))
}

/// Look up a concept as an `i64`.
///
/// Mirrors upstream `LookupInt64`:
/// - `Int64` tags match only integer-typed attributes.
/// - `Float64` tags accept float-typed attributes whose value is losslessly
///   representable as an `i64`.
/// - `String` tags accept string-typed attributes, parsing via `i64::from_str`
///   first and then a float-parse fallback for integer-valued floats.
pub fn lookup_int64<A: Accessor>(registry: &Registry, accessor: &A, concept: Concept) -> Option<i64> {
    let tags = registry.get_attribute_precedence(concept)?;
    for tag in tags {
        if !conditions_match(accessor, tag) {
            continue;
        }
        match tag.value_type {
            ValueType::Int64 => {
                if let Some(v) = accessor.get_int64(&tag.name) {
                    return Some(v);
                }
            }
            ValueType::Float64 => {
                if let Some(v) = accessor.get_float64(&tag.name) {
                    let i = v as i64;
                    if i as f64 == v {
                        return Some(i);
                    }
                }
            }
            ValueType::String => {
                if let Some(s) = accessor.get_string(&tag.name) {
                    if !s.is_empty() {
                        if let Ok(i) = s.parse::<i64>() {
                            return Some(i);
                        }
                        if let Ok(f) = s.parse::<f64>() {
                            let i = f as i64;
                            if i as f64 == f {
                                return Some(i);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Look up a concept as an `f64`. Mirrors upstream `LookupFloat64`.
pub fn lookup_float64<A: Accessor>(registry: &Registry, accessor: &A, concept: Concept) -> Option<f64> {
    let tags = registry.get_attribute_precedence(concept)?;
    for tag in tags {
        if !conditions_match(accessor, tag) {
            continue;
        }
        match tag.value_type {
            ValueType::Float64 => {
                if let Some(v) = accessor.get_float64(&tag.name) {
                    return Some(v);
                }
            }
            ValueType::Int64 => {
                if let Some(v) = accessor.get_int64(&tag.name) {
                    return Some(v as f64);
                }
            }
            ValueType::String => {
                if let Some(s) = accessor.get_string(&tag.name) {
                    if !s.is_empty() {
                        if let Ok(f) = s.parse::<f64>() {
                            return Some(f);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Look up a concept as a `String`. Mirrors upstream `LookupString`.
///
/// Non-string-typed tags are stringified via `format!` to match upstream's
/// `strconv.FormatInt(_, 10)` and `strconv.FormatFloat(_, 'f', -1, 64)` output.
pub fn lookup_string<A: Accessor>(registry: &Registry, accessor: &A, concept: Concept) -> Option<String> {
    let tags = registry.get_attribute_precedence(concept)?;
    for tag in tags {
        if !conditions_match(accessor, tag) {
            continue;
        }
        match tag.value_type {
            ValueType::String => {
                if let Some(s) = accessor.get_string(&tag.name) {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
            ValueType::Int64 => {
                if let Some(v) = accessor.get_int64(&tag.name) {
                    return Some(v.to_string());
                }
            }
            ValueType::Float64 => {
                if let Some(v) = accessor.get_float64(&tag.name) {
                    return Some(format_float(v));
                }
            }
        }
    }
    None
}

// Mirror Go's `strconv.FormatFloat(v, 'f', -1, 64)`: integer-valued floats
// print without a fractional part, otherwise the shortest decimal form.
fn format_float(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

#[cfg(test)]
mod tests {
    use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, KeyValue};

    use super::super::accessor::{OtelSpanAccessor, OtlpAttributesAccessor};
    use super::super::registry::REGISTRY;
    use super::*;

    fn kv(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    #[test]
    fn int64_int_attr_matches() {
        let attrs = vec![kv("http.status_code", Value::IntValue(500))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), Some(500));
    }

    #[test]
    fn int64_string_attr_parses() {
        // The lading reproduction: http.response.status_code sent as StringValue.
        let attrs = vec![kv("http.response.status_code", Value::StringValue("500".into()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), Some(500));
    }

    #[test]
    fn int64_malformed_string_returns_none() {
        let attrs = vec![kv("http.status_code", Value::StringValue("not-a-number".into()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), None);
    }

    #[test]
    fn int64_empty_string_returns_none() {
        let attrs = vec![kv("http.status_code", Value::StringValue(String::new()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), None);
    }

    #[test]
    fn int64_string_with_fractional_float_parses() {
        // "200.0" is an integer-valued float — accept it.
        let attrs = vec![kv("http.status_code", Value::StringValue("200.0".into()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), Some(200));
    }

    #[test]
    fn int64_string_with_true_fractional_returns_none() {
        let attrs = vec![kv("http.status_code", Value::StringValue("200.5".into()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), None);
    }

    #[test]
    fn int64_span_precedence_over_resource() {
        let span = vec![kv("http.status_code", Value::IntValue(201))];
        let resource = vec![kv("http.status_code", Value::IntValue(500))];
        let a = OtelSpanAccessor::new(&span, &resource);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), Some(201));
    }

    #[test]
    fn int64_falls_back_to_resource() {
        let span: Vec<KeyValue> = vec![];
        let resource = vec![kv("http.response.status_code", Value::StringValue("418".into()))];
        let a = OtelSpanAccessor::new(&span, &resource);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), Some(418));
    }

    #[test]
    fn int64_int_tag_ignores_string_value_at_same_name() {
        // If only a string exists for http.status_code, the int-tag entry for
        // that same name should skip, and the string-tag entry later in the
        // precedence list should match.
        let attrs = vec![kv("http.status_code", Value::StringValue("204".into()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::HttpStatusCode), Some(204));
    }

    #[test]
    fn string_int_attr_stringifies() {
        // rpc.grpc.status_code is registered as int64 first; looking it up as
        // a string should format the int.
        let attrs = vec![kv("rpc.grpc.status_code", Value::IntValue(14))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(
            lookup_string(&REGISTRY, &a, Concept::RpcGrpcStatusCode).as_deref(),
            Some("14"),
        );
    }

    #[test]
    fn string_string_attr_matches() {
        let attrs = vec![kv("service.name", Value::StringValue("cart".into()))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(
            lookup_string(&REGISTRY, &a, Concept::ServiceName).as_deref(),
            Some("cart"),
        );
    }

    #[test]
    fn float64_float_matches_first() {
        let attrs = vec![kv("_dd.top_level", Value::DoubleValue(1.0))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_float64(&REGISTRY, &a, Concept::DdTopLevel), Some(1.0));
    }

    #[test]
    fn float64_missing_concept_returns_none() {
        let attrs: Vec<KeyValue> = vec![];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_float64(&REGISTRY, &a, Concept::DdTopLevel), None);
    }

    #[test]
    fn int64_when_condition_failure_skips_fallback() {
        // A non-gRPC span carrying rpc.response.status_code: the `when`
        // clause (rpc.system == "grpc") must block the fallback, so the lookup
        // returns None rather than resolving a gRPC status code from it.
        let attrs = vec![
            kv("rpc.response.status_code", Value::IntValue(14)),
            kv("rpc.system", Value::StringValue("apache_dubbo".into())),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::RpcGrpcStatusCode), None);
    }

    #[test]
    fn int64_when_condition_pass_accepts_fallback() {
        let attrs = vec![
            kv("rpc.response.status_code", Value::IntValue(14)),
            kv("rpc.system", Value::StringValue("grpc".into())),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::RpcGrpcStatusCode), Some(14));
    }

    #[test]
    fn int64_when_condition_missing_attribute_blocks_fallback() {
        // No rpc.system attribute at all: the `when` clause cannot hold, so the
        // fallback must be skipped.
        let attrs = vec![kv("rpc.response.status_code", Value::IntValue(14))];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::RpcGrpcStatusCode), None);
    }

    #[test]
    fn int64_when_requires_all_conditions() {
        // The dual-condition fallback (rpc.system == "grpc" AND rpc.system.name
        // absent) requires both. Here rpc.system.name is present, so the
        // multi-condition fallback is skipped; rpc.grpc.status_code matches.
        let attrs = vec![
            kv("rpc.system", Value::StringValue("grpc".into())),
            kv("rpc.system.name", Value::StringValue("something".into())),
            kv("rpc.response.status_code", Value::IntValue(14)),
            kv("rpc.grpc.status_code", Value::IntValue(3)),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::RpcGrpcStatusCode), Some(3));
    }

    #[test]
    fn int64_when_partial_condition_set_blocks_fallback() {
        // rpc.system matches "grpc" but rpc.system.name is present: the second
        // condition (present: false) fails, so the fallback is skipped entirely.
        let attrs = vec![
            kv("rpc.system", Value::StringValue("grpc".into())),
            kv("rpc.system.name", Value::StringValue("something".into())),
            kv("rpc.response.status_code", Value::IntValue(14)),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_int64(&REGISTRY, &a, Concept::RpcGrpcStatusCode), None);
    }

    #[test]
    fn string_when_condition_failure_skips_fallback() {
        let attrs = vec![
            kv("rpc.response.status_code", Value::StringValue("14".into())),
            kv("rpc.system", Value::StringValue("apache_dubbo".into())),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(lookup_string(&REGISTRY, &a, Concept::RpcGrpcStatusCode), None);
    }

    #[test]
    fn string_when_condition_pass_accepts_fallback() {
        let attrs = vec![
            kv("rpc.response.status_code", Value::StringValue("14".into())),
            kv("rpc.system.name", Value::StringValue("grpc".into())),
        ];
        let a = OtlpAttributesAccessor::new(&attrs);
        assert_eq!(
            lookup_string(&REGISTRY, &a, Concept::RpcGrpcStatusCode).as_deref(),
            Some("14"),
        );
    }
}
