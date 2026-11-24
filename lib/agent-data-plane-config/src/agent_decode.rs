//! Decodes JSON values into struct fields as the Datadog Agent's structured decoders do.
//!
//! The Agent reads some settings, such as lists of rules, with `structure.UnmarshalKey`. It decodes them with
//! `mapstructure`, and with weakly typed input. For some settings it decodes a JSON string with `encoding/json`
//! instead. These helpers reproduce both decoders for a value that already reached ADP as JSON.
//!
//! Both decoders match a key to a field name ignoring case, and ignore unknown keys. An absent or null field keeps its
//! zero value.

use std::fmt;

use serde_json::{Map, Value};

/// The Agent decoder to reproduce.
#[derive(Clone, Copy, Debug)]
pub enum Decoder {
    /// `mapstructure` with weakly typed input.
    ///
    /// A boolean or a number becomes a string (`true` is `"1"`). A map where a list is expected becomes a list of that
    /// map, or an empty list when the map is empty.
    Mapstructure,

    /// `encoding/json`. A value of another type than the field is invalid.
    Json,
}

/// Reports a value that doesn't decode into its field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Returns the value of the key that sets the field `name`: the key spelled exactly as `name`, or else a key equal to
/// it ignoring case.
///
/// `name` is the Agent's field name: the `mapstructure` or `json` tag, or the Go field name, such as `Name`, when the
/// field has no tag. When several keys match only ignoring case, the Agent picks any one of them.
pub fn field<'a>(entry: &'a Map<String, Value>, name: &str) -> &'a Value {
    entry
        .get(name)
        .or_else(|| {
            entry
                .iter()
                .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value))
        })
        .unwrap_or(&Value::Null)
}

/// Decodes a struct: the value must be a map.
///
/// # Errors
///
/// Returns an error if the value isn't a map.
pub fn map<'a>(path: &str, value: &'a Value) -> Result<&'a Map<String, Value>, DecodeError> {
    match value {
        Value::Object(entry) => Ok(entry),
        other => Err(unexpected(path, "a map", other)),
    }
}

/// Decodes a string field.
///
/// # Errors
///
/// Returns an error for a list or a map, and with [`Decoder::Json`], for a boolean or a number.
pub fn string(path: &str, value: &Value, decoder: Decoder) -> Result<String, DecodeError> {
    match (value, decoder) {
        (Value::Null, _) => Ok(String::new()),
        (Value::String(value), _) => Ok(value.clone()),
        (Value::Bool(value), Decoder::Mapstructure) => Ok(if *value { "1" } else { "0" }.to_string()),
        // Go's `FormatFloat(f, 'f', -1, 64)`, like Rust's `Display`, never uses an exponent.
        (Value::Number(value), Decoder::Mapstructure) if value.is_f64() => {
            Ok(value.as_f64().unwrap_or_default().to_string())
        }
        (Value::Number(value), Decoder::Mapstructure) => Ok(value.to_string()),
        (other, _) => Err(unexpected(path, "a string", other)),
    }
}

/// Decodes a list-of-strings field with [`Decoder::Mapstructure`].
///
/// Like a list of structs, a field that holds another value than a list becomes a list of that value. A string is
/// split at commas first, as the Agent's `StringToSliceHookFunc` does, and an empty string is an empty list.
///
/// # Errors
///
/// Returns an error for an item that doesn't decode as a string, as [`string`] does.
pub fn string_list(path: &str, value: &Value) -> Result<Vec<String>, DecodeError> {
    match value {
        Value::String(value) if value.is_empty() => Ok(Vec::new()),
        Value::String(value) => Ok(value.split(',').map(str::to_string).collect()),
        other => list(path, other, Decoder::Mapstructure)?
            .into_iter()
            .enumerate()
            .map(|(index, item)| string(&format!("{path}[{index}]"), item, Decoder::Mapstructure))
            .collect(),
    }
}

/// Decodes a list field, and returns its items to decode.
///
/// # Errors
///
/// Returns an error with [`Decoder::Json`] if the value isn't a list.
pub fn list<'a>(path: &str, value: &'a Value, decoder: Decoder) -> Result<Vec<&'a Value>, DecodeError> {
    match (value, decoder) {
        (Value::Null, _) => Ok(Vec::new()),
        (Value::Array(items), _) => Ok(items.iter().collect()),
        (Value::Object(entry), Decoder::Mapstructure) if entry.is_empty() => Ok(Vec::new()),
        (other, Decoder::Mapstructure) => Ok(vec![other]),
        (other, Decoder::Json) => Err(unexpected(path, "a list", other)),
    }
}

/// Returns the error for a value of an unexpected type at `path`.
pub fn unexpected(path: &str, expected: &str, value: &Value) -> DecodeError {
    let got = match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a map",
    };
    DecodeError(format!("`{path}` expected {expected}, got {got}"))
}
