//! Trace helpers shared by the Datadog and OTLP trace pipelines.

use std::borrow::Cow;

use saluki_core::data_model::event::trace::AttributeValue;

pub mod normalize;

/// Payload or span attribute holding the process tags.
pub(crate) const TAG_PROCESS_TAGS: &str = "_dd.tags.process";

/// Payload or span attribute recording whether the SDK exports its telemetry through OTLP.
pub(crate) const TAG_SDK_OTLP_EXPORT: &str = "_dd.sdk.otlp_export";

/// Formats an attribute value as a string, as the trace-agent does.
pub fn attribute_as_string(value: &AttributeValue) -> Cow<'_, str> {
    match value {
        AttributeValue::String(s) => Cow::Borrowed(s.as_ref()),
        AttributeValue::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        AttributeValue::Int(i) => Cow::Owned(i.to_string()),
        AttributeValue::Float(f) => Cow::Owned(format_float(*f)),
        // shortcut: invalid UTF-8 is replaced rather than kept byte for byte as the trace-agent does; keep bytes if a
        // caller forwards the result somewhere that needs them exact.
        AttributeValue::Bytes(b) => String::from_utf8_lossy(b),
        AttributeValue::Array(values) => {
            let values: Vec<_> = values.iter().map(attribute_as_string).collect();
            Cow::Owned(format!("[{}]", values.join(",")))
        }
        AttributeValue::KeyValueList(entries) => {
            let entries: Vec<_> = entries
                .iter()
                .map(|(key, value)| format!("{}={}", key, attribute_as_string(value)))
                .collect();
            Cow::Owned(format!("{{{}}}", entries.join(",")))
        }
    }
}

/// Formats a float as the trace-agent does: the shortest representation that round-trips, never in exponent form.
fn format_float(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    // Rust's `Display` already prints the shortest round-tripping form without an exponent, and `NaN` as the
    // trace-agent does.
    f.to_string()
}
