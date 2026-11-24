//! Renders OTLP attribute values as strings.

use std::borrow::Cow;
use std::fmt::Write as _;

use base64::{engine::general_purpose, Engine as _};
use otlp_protos::opentelemetry::proto::common::v1::any_value::Value as OtlpValue;
use serde_json::{Map as JsonMap, Value as JsonValue};

/// Renders an OTLP value the way `pcommon.Value.AsString` does, in the `pdata` version that the test vectors record.
///
/// Arrays and maps become JSON, as [`write_raw_json`] describes. Every other value renders as
/// [`otlp_scalar_as_string`] does.
pub(crate) fn otlp_value_as_string(value: Option<&OtlpValue>) -> Cow<'_, str> {
    match value {
        Some(OtlpValue::ArrayValue(_) | OtlpValue::KvlistValue(_)) => {
            let mut json = String::new();
            match write_raw_json(&mut json, value) {
                Ok(()) => Cow::Owned(json),
                Err(NonFiniteDouble) => Cow::Borrowed(""),
            }
        }
        scalar => otlp_scalar_as_string(scalar),
    }
}

/// A double that JSON can't hold: NaN or an infinity.
struct NonFiniteDouble;

/// Writes a value as JSON, as `json.Encoder` with HTML escaping off writes the `pcommon.Value.AsRaw` of the value.
///
/// - Map keys are sorted by their bytes, and the last of duplicate keys wins.
/// - Empty values, and empty bytes, are `null`. Other bytes are base64 strings.
/// - Doubles use the same layout as [`float64_as_string`].
/// - Strings are escaped as [`write_json_string`] describes.
///
/// # Errors
///
/// If the value holds a NaN or an infinite double, an error is returned. `json.Encoder` fails on such a value, and
/// `AsString` ignores the error, so the rendered value is the empty string.
fn write_raw_json(out: &mut String, value: Option<&OtlpValue>) -> Result<(), NonFiniteDouble> {
    match value {
        None => out.push_str("null"),
        Some(OtlpValue::StringValue(value)) => write_json_string(out, value),
        Some(OtlpValue::BoolValue(value)) => out.push_str(if *value { "true" } else { "false" }),
        Some(OtlpValue::IntValue(value)) => {
            let _ = write!(out, "{}", value);
        }
        Some(OtlpValue::DoubleValue(value)) => {
            if !value.is_finite() {
                return Err(NonFiniteDouble);
            }
            out.push_str(&float64_as_string(*value));
        }
        // `AsRaw` copies bytes by appending them to a nil slice, so empty bytes stay nil, which is `null`.
        Some(OtlpValue::BytesValue(value)) if value.is_empty() => out.push_str("null"),
        Some(OtlpValue::BytesValue(value)) => {
            // Base64 has no characters that JSON escapes.
            out.push('"');
            general_purpose::STANDARD.encode_string(value, out);
            out.push('"');
        }
        Some(OtlpValue::ArrayValue(array)) => {
            out.push('[');
            for (index, element) in array.values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_raw_json(out, element.value.as_ref())?;
            }
            out.push(']');
        }
        Some(OtlpValue::KvlistValue(map)) => {
            let mut entries = map
                .values
                .iter()
                .map(|kv| {
                    (
                        kv.key.as_str(),
                        kv.value.as_ref().and_then(|value| value.value.as_ref()),
                    )
                })
                .collect::<Vec<_>>();
            // A stable sort keeps duplicate keys in their original order, so the last of each run is the one that wins.
            entries.sort_by_key(|(key, _)| *key);
            out.push('{');
            let mut first = true;
            for (index, (key, value)) in entries.iter().enumerate() {
                if entries.get(index + 1).is_some_and(|(next, _)| next == key) {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                write_json_string(out, key);
                out.push(':');
                write_raw_json(out, *value)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Writes a JSON string as Go's `encoding/json` does with HTML escaping off.
///
/// Quotes and backslashes are escaped, and so are control characters: `\b`, `\f`, `\n`, `\r`, and `\t` in their
/// short forms, and the others as `\u00xx` in lowercase hex. U+2028 and U+2029 are escaped as `\u2028` and `\u2029`.
/// Every other character, including DEL, `<`, `>`, and `&`, is written as it is.
fn write_json_string(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    out.push('"');
    let mut unescaped_from = 0;
    for (index, c) in value.char_indices() {
        if !matches!(c, '"' | '\\' | '\u{00}'..='\u{1f}' | '\u{2028}' | '\u{2029}') {
            continue;
        }
        out.push_str(&value[unescaped_from..index]);
        unescaped_from = index + c.len_utf8();
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            control => {
                // The other control characters are all below 0x20, so they're a single byte.
                let byte = control as u8;
                out.push_str("\\u00");
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0xf)]));
            }
        }
    }
    out.push_str(&value[unescaped_from..]);
    out.push('"');
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

/// Renders a double like `pcommon`'s `float64AsString`: ES6 number-to-string, which is decimal from 1e-6 up to 1e21 and
/// exponent form outside that range, with `NaN`, `Infinity`, and `-Infinity` spelled out. `encoding/json` writes finite
/// doubles the same way.
///
/// The digits are the shortest that round-trip, as Go's `strconv.FormatFloat` writes them with precision -1. When two
/// candidates are equally close, Go keeps the even last digit. The `ryu` crate does the same, but Rust's own formatting
/// doesn't: it writes `1888570120608320.3` where Go writes `1888570120608320.2`. So the digits come from `ryu`, in Go's
/// layout.
fn float64_as_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_string();
    }

    let mut buffer = ryu::Buffer::new();
    let (digits, point) = decimal_digits(buffer.format_finite(value.abs()));
    let mut out = String::with_capacity(digits.len() + 8);
    if value < 0.0 {
        out.push('-');
    }

    let magnitude = value.abs();
    if !(1e-6..1e21).contains(&magnitude) {
        // Go writes `d.ddde±x`, with a sign but no padding on the exponent, as in `1e+21` and `1.5e-7`.
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let exponent = point - 1;
        let _ = write!(
            out,
            "e{}{}",
            if exponent < 0 { '-' } else { '+' },
            exponent.unsigned_abs()
        );
    } else if point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', point.unsigned_abs() as usize));
        out.push_str(&digits);
    } else {
        let point = point as usize;
        if point >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', point - digits.len()));
        } else {
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        }
    }
    out
}

/// Splits the output of `ryu` for a positive double into its significant digits and the position of the decimal point.
///
/// The value is `0.<digits> × 10^point`. The digits have no leading or trailing zeros. `ryu` writes forms such as
/// `1.0`, `0.001`, `1e21`, and `1.5e-7`.
fn decimal_digits(formatted: &str) -> (String, i32) {
    let (mantissa, exponent) = match formatted.split_once('e') {
        Some((mantissa, exponent)) => (
            mantissa,
            exponent.parse::<i32>().expect("Ryu writes a decimal exponent"),
        ),
        None => (formatted, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));

    let mut digits = String::with_capacity(integer.len() + fraction.len());
    digits.push_str(integer);
    digits.push_str(fraction);
    let leading_zeros = digits.len() - digits.trim_start_matches('0').len();
    let digits = digits.trim_start_matches('0').trim_end_matches('0').to_string();
    let point = integer.len() as i32 + exponent - leading_zeros as i32;
    (digits, point)
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
    use base64::engine::general_purpose::STANDARD;
    use otlp_protos::opentelemetry::proto::collector::logs::v1::ExportLogsServiceRequest;
    use prost::Message as _;
    use serde::Deserialize;

    use super::*;

    /// The test vectors of [`otlp_value_as_string`], which `testdata/pdata-as-string` generates with `pdata`.
    const PDATA_AS_STRING_VECTORS: &str = include_str!("testdata/pdata_as_string_vectors.json");

    /// The vectors, and the `pdata` version that generated them, which the Integration version of the Agent requires.
    #[derive(Deserialize)]
    struct Vectors {
        pdata_version: String,
        vectors: Vec<Vector>,
    }

    /// An OTLP logs export request, encoded as protobuf and then base64, whose only log record has a body, and the
    /// `pcommon.Value.AsString` of that body.
    #[derive(Deserialize)]
    struct Vector {
        name: String,
        request: String,
        as_string: String,
    }

    #[test]
    fn otlp_value_as_string_matches_pdata() {
        let vectors: Vectors = serde_json::from_str(PDATA_AS_STRING_VECTORS).expect("the vectors should be JSON");
        assert!(!vectors.pdata_version.is_empty());
        assert!(!vectors.vectors.is_empty());

        for vector in vectors.vectors {
            let request = STANDARD.decode(&vector.request).expect("the request should be base64");
            let request = ExportLogsServiceRequest::decode(request.as_slice()).expect("the request should decode");
            let body = request.resource_logs[0].scope_logs[0].log_records[0].body.as_ref();
            let value = body.and_then(|body| body.value.as_ref());
            assert_eq!(otlp_value_as_string(value), vector.as_string, "{}", vector.name);
        }
    }

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
            // Equally close candidates keep the even last digit, as Go does.
            (f64::from_bits(0x431a_d693_fcb0_6901), "1888570120608320.2"),
            (1e15 + 0.25, "1000000000000000.2"),
            (1e15 + 0.75, "1000000000000000.8"),
        ] {
            assert_eq!(float64_as_string(value), expected, "{value:?}");
        }
    }
}
