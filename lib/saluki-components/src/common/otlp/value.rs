//! Renders OTLP attribute values as strings.

use std::borrow::Cow;

use base64::{engine::general_purpose, Engine as _};
use otlp_protos::opentelemetry::proto::common::v1::any_value::Value as OtlpValue;
use serde_json::{Map as JsonMap, Value as JsonValue};

/// Renders an OTLP value as a string, as the Datadog Agent does wherever it needs a string.
///
/// The Agent converts values with `pcommon.Value.AsString` from the OpenTelemetry Collector's `pdata` module
/// ([source](https://github.com/open-telemetry/opentelemetry-collector/blob/pdata/v1.71.0/pdata/pcommon/value.go)):
///
/// | Value | String |
/// |---|---|
/// | String | The string itself |
/// | Boolean | `true` or `false` |
/// | Integer | Decimal, as in `-12` |
/// | Double | The ES6 number format, as in `1.5`, `1e+21`, and `-0`, with `NaN`, `Infinity`, and `-Infinity` spelled out |
/// | Bytes | Base64, with padding |
/// | Absent | The empty string |
/// | Array or map | JSON, with map keys sorted |
///
/// # Missing
///
/// The JSON of arrays and maps differs from `pcommon` (#3397). Inside them, doubles use the `serde_json` layout and
/// empty values are dropped. Empty bytes become `""` instead of `null`. A non-finite double is dropped instead of
/// making the whole string empty, and U+2028 and U+2029 aren't escaped.
pub(crate) fn otlp_value_as_string(value: Option<&OtlpValue>) -> Cow<'_, str> {
    match value {
        Some(value @ (OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_))) => {
            otlp_value_to_string(value).map_or(Cow::Borrowed(""), Cow::Owned)
        }
        scalar => otlp_scalar_as_string(scalar),
    }
}

/// Renders a scalar OTLP value the way `pcommon.Value.AsString` does. An absent value is the empty string.
pub(crate) fn otlp_scalar_as_string(value: Option<&OtlpValue>) -> Cow<'_, str> {
    match value {
        Some(OtlpValue::StringValue(value)) => Cow::Borrowed(value.as_str()),
        Some(OtlpValue::BoolValue(value)) => Cow::Borrowed(if *value { "true" } else { "false" }),
        Some(OtlpValue::IntValue(value)) => Cow::Owned(value.to_string()),
        Some(OtlpValue::DoubleValue(value)) => Cow::Owned(float64_as_string(*value)),
        Some(OtlpValue::BytesValue(value)) => Cow::Owned(general_purpose::STANDARD.encode(value)),
        // Callers flatten arrays and maps, so they never reach this point.
        Some(OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_)) | None => Cow::Borrowed(""),
    }
}

/// Renders a double like `pcommon`'s `float64AsString` in `pdata` v1.70.0, the version the Agent pins: ES6
/// number-to-string, which is decimal from 1e-6 up to 1e21 and exponent form outside that range, with `NaN`,
/// `Infinity`, and `-Infinity` spelled out.
fn float64_as_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }

    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-6..1e21).contains(&magnitude) {
        // Rust writes `1e21` and `1.5e-7`; ES6 writes a sign on positive exponents, as in `1e+21`.
        let exponent_form = format!("{value:e}");
        return match exponent_form.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => format!("{mantissa}e+{exponent}"),
            _ => exponent_form,
        };
    }
    value.to_string()
}

/// Renders an OTLP value as a string for trace metadata. Arrays and maps become JSON.
///
/// This isn't `pcommon.Value.AsString`: doubles use Rust's formatting and bytes become a `<N bytes>`
/// placeholder. Use [`otlp_value_as_string`] to render a value the way the Agent does.
pub(crate) fn otlp_value_to_string(value: &OtlpValue) -> Option<String> {
    match value {
        OtlpValue::StringValue(v) => Some(v.clone()),
        OtlpValue::BoolValue(v) => Some(if *v { "true" } else { "false" }.to_string()),
        OtlpValue::IntValue(v) => Some(v.to_string()),
        OtlpValue::DoubleValue(v) => Some(v.to_string()),
        OtlpValue::BytesValue(bytes) => Some(format!("<{} bytes>", bytes.len())),
        OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_) => {
            otlp_value_to_json_value(value).map(|json| json.to_string())
        }
    }
}

/// Converts an OTLP value into JSON. Bytes become base64 strings, and absent values in arrays and maps are dropped.
pub(crate) fn otlp_value_to_json_value(value: &OtlpValue) -> Option<JsonValue> {
    match value {
        OtlpValue::StringValue(v) => Some(JsonValue::String(v.clone())),
        OtlpValue::BoolValue(v) => Some(JsonValue::Bool(*v)),
        OtlpValue::IntValue(v) => Some(JsonValue::Number((*v).into())),
        OtlpValue::DoubleValue(v) => serde_json::Number::from_f64(*v).map(JsonValue::Number),
        OtlpValue::BytesValue(bytes) => Some(JsonValue::String(general_purpose::STANDARD.encode(bytes))),
        OtlpValue::ArrayValue(array) => {
            let mut arr = Vec::with_capacity(array.values.len());
            for item in &array.values {
                if let Some(inner) = item.value.as_ref().and_then(otlp_value_to_json_value) {
                    arr.push(inner);
                }
            }
            Some(JsonValue::Array(arr))
        }
        OtlpValue::KvlistValue(kvlist) => {
            let mut obj = JsonMap::new();
            for kv in &kvlist.values {
                if let Some(inner) = kv
                    .value
                    .as_ref()
                    .and_then(|wrapper| wrapper.value.as_ref())
                    .and_then(otlp_value_to_json_value)
                {
                    obj.insert(kv.key.clone(), inner);
                }
            }
            Some(JsonValue::Object(obj))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float64_as_string_matches_pcommon() {
        for (value, expected) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (1.5, "1.5"),
            (-2.25, "-2.25"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (1e-10, "1e-10"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (-1e21, "-1e+21"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(float64_as_string(value), expected, "{value:?}");
        }
    }
}
