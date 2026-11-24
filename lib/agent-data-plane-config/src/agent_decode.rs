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
/// field has no tag.
///
/// When several keys match only ignoring case, this function takes the first one in key order. The Agent's decoders
/// differ here: `mapstructure` picks any one of them, and `encoding/json` takes the last one in the document, even over
/// the exact spelling. The first match is one of the results that `mapstructure` can give. Matching `encoding/json`
/// would need the order of the keys in the document, which a parsed `Value` doesn't keep, and only a configuration
/// that sets a field twice can tell the difference.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(entry) => entry,
            other => panic!("not a map: {other}"),
        }
    }

    #[test]
    fn a_field_matches_its_exact_key_first_and_else_ignoring_case() {
        let entry = object(json!({ "Name": "exact", "name": "lower" }));
        assert_eq!(field(&entry, "Name"), &json!("exact"));
        assert_eq!(field(&entry, "name"), &json!("lower"));

        let entry = object(json!({ "NAME": "upper" }));
        assert_eq!(field(&entry, "name"), &json!("upper"));
        assert_eq!(field(&entry, "Name"), &json!("upper"));
    }

    #[test]
    fn an_absent_field_is_null() {
        let entry = object(json!({ "names": "x" }));
        assert_eq!(field(&entry, "name"), &Value::Null);
    }

    #[test]
    fn a_struct_must_be_a_map() {
        assert_eq!(map("rule", &json!({ "a": 1 })), Ok(&object(json!({ "a": 1 }))));
        assert_eq!(
            map("rule", &json!("x")).map_err(|error| error.to_string()),
            Err("`rule` expected a map, got a string".to_string())
        );
    }

    #[test]
    fn mapstructure_converts_scalars_to_strings() {
        for (value, expected) in [
            (json!(null), ""),
            (json!("text"), "text"),
            (json!(true), "1"),
            (json!(false), "0"),
            (json!(123), "123"),
            (json!(-7), "-7"),
            (json!(1.5), "1.5"),
            (json!(1e21), "1000000000000000000000"),
        ] {
            assert_eq!(
                string("f", &value, Decoder::Mapstructure),
                Ok(expected.to_string()),
                "{value}"
            );
        }
    }

    #[test]
    fn json_accepts_only_a_string_or_null_for_a_string() {
        assert_eq!(string("f", &json!(null), Decoder::Json), Ok(String::new()));
        assert_eq!(string("f", &json!("text"), Decoder::Json), Ok("text".to_string()));
        for (value, got) in [(json!(true), "a boolean"), (json!(7), "a number")] {
            assert_eq!(
                string("f", &value, Decoder::Json).map_err(|error| error.to_string()),
                Err(format!("`f` expected a string, got {got}")),
                "{value}"
            );
        }
    }

    #[test]
    fn no_decoder_converts_a_list_or_a_map_to_a_string() {
        for decoder in [Decoder::Mapstructure, Decoder::Json] {
            for (value, got) in [(json!(["a"]), "a list"), (json!({}), "a map")] {
                assert_eq!(
                    string("f", &value, decoder).map_err(|error| error.to_string()),
                    Err(format!("`f` expected a string, got {got}")),
                    "{value} with {decoder:?}"
                );
            }
        }
    }

    #[test]
    fn mapstructure_reads_another_value_than_a_list_as_a_list_of_it() {
        let rule = json!({ "name": "x" });
        assert_eq!(list("l", &rule, Decoder::Mapstructure), Ok(vec![&rule]));
        assert_eq!(list("l", &json!(5), Decoder::Mapstructure), Ok(vec![&json!(5)]));
        assert_eq!(list("l", &json!({}), Decoder::Mapstructure), Ok(vec![]));
        assert_eq!(list("l", &json!(null), Decoder::Mapstructure), Ok(vec![]));
        assert_eq!(
            list("l", &json!([1, 2]), Decoder::Mapstructure),
            Ok(vec![&json!(1), &json!(2)])
        );
    }

    #[test]
    fn json_accepts_only_a_list_or_null_for_a_list() {
        assert_eq!(list("l", &json!(null), Decoder::Json), Ok(vec![]));
        assert_eq!(list("l", &json!([1]), Decoder::Json), Ok(vec![&json!(1)]));
        for (value, got) in [(json!({}), "a map"), (json!("a"), "a string"), (json!(5), "a number")] {
            assert_eq!(
                list("l", &value, Decoder::Json).map_err(|error| error.to_string()),
                Err(format!("`l` expected a list, got {got}")),
                "{value}"
            );
        }
    }

    #[test]
    fn a_string_list_splits_a_string_at_commas_and_converts_its_items() {
        for (value, expected) in [
            (json!(""), vec![]),
            (json!("a,b"), vec!["a", "b"]),
            (json!(" a, b"), vec![" a", " b"]),
            (json!(["a", 1, true]), vec!["a", "1", "1"]),
            (json!(5), vec!["5"]),
            (json!({}), vec![]),
            (json!(null), vec![]),
        ] {
            assert_eq!(
                string_list("l", &value),
                Ok(expected.into_iter().map(str::to_string).collect()),
                "{value}"
            );
        }
    }

    #[test]
    fn a_string_list_reports_the_item_that_does_not_convert() {
        assert_eq!(
            string_list("l", &json!(["a", ["b"]])).map_err(|error| error.to_string()),
            Err("`l[1]` expected a string, got a list".to_string())
        );
        assert_eq!(
            string_list("l", &json!({ "a": 1 })).map_err(|error| error.to_string()),
            Err("`l[0]` expected a string, got a map".to_string())
        );
    }
}
