//! OTLP internal utilities.

/// Formats a key and value into a Datadog tag string.
/// If the value is empty, it is replaced with "n/a".
pub fn format_key_value_tag(key: &str, value: &str) -> String {
    let final_value = if value.is_empty() { "n/a" } else { value };
    format!("{}:{}", key, final_value)
}
