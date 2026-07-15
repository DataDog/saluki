//! Decode a raw environment-variable string into the JSON shape the schema declares for a leaf.
//!
//! The Datadog Agent turns an environment string into a typed value in one place
//! (`insertNodeFromString`): a registered per-key transformer when the schema names an
//! `env_parser`, otherwise a cast against the key's default type. This module ports both halves so
//! ADP can build a resolved view of real JSON arrays, objects, numbers, and booleans at each
//! nested path, which a single ordinary deserialization then reads.
//!
//! Supported splitting and delimiter behavior matches the Agent bug-for-bug (see the per-function
//! notes, which cite the Agent source). Error handling deliberately diverges: the Agent logs a malformed
//! value and substitutes an empty result, while this module propagates a hard error so the caller
//! can reject the value instead of silently losing it.
//!
//! One type is intentionally left as a string: a `Duration` leaf keeps its raw text and is parsed by
//! `crate::duration_de` at deserialize time, because a duration also arrives from the file (a Go
//! duration string) and from the Agent stream (integer nanoseconds), so that leaf must stay
//! shape-tolerant regardless of the environment.

use serde_json::{Map, Number, Value};

/// How to decode a raw environment string into a JSON value for one leaf.
///
/// The named-parser variants mirror the schema's `env_parser`; the rest are the type-based fallback
/// the Agent applies when no `env_parser` is declared, keyed by the leaf's declared type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvDecode {
    /// `env_parser: json`: parse the string as a JSON document.
    Json,
    /// `env_parser: comma_separated`: split on `,`, no trimming.
    CommaSeparated,
    /// `env_parser: comma_and_space_separated`: split on `,` or space, dropping empties.
    CommaAndSpaceSeparated,
    /// `env_parser: comma_then_space_separated`: split on `,` if present else space, trimmed.
    CommaThenSpaceSeparated,
    /// `env_parser: json_list_or_comma_separated`: a `[...]` JSON list, else split on `,`.
    JsonListOrCommaSeparated,
    /// `env_parser: json_list_or_space_separated`: a `[...]` JSON list, else split on space.
    JsonListOrSpaceSeparated,
    /// `env_parser: traces_span`: `svc|op=rate,...` into a `{name: rate}` object.
    TracesSpan,
    /// Type fallback: a plain string, taken verbatim.
    RawString,
    /// Type fallback: a boolean, parsed with Go's `strconv.ParseBool` grammar.
    Bool,
    /// Type fallback: a signed integer.
    Integer,
    /// Type fallback: a floating-point number.
    Float,
    /// Type fallback: a string list, split on whitespace (the Agent's `[]string` cast).
    StringList,
    /// Type fallback: a duration, carried through as a string for `crate::duration_de`.
    DurationString,
    /// Type fallback: a map/object (or any non-scalar), parsed as a JSON document.
    JsonValue,
}

/// Decodes `raw` into the JSON value `how` prescribes.
///
/// # Errors
///
/// Returns a human-readable message when `raw` is malformed for `how` (invalid JSON, a bad boolean
/// or number, or a `traces_span` token that is not `name=rate`). The caller pairs it with the
/// environment variable name.
pub fn decode(raw: &str, how: EnvDecode) -> Result<Value, String> {
    match how {
        EnvDecode::Json | EnvDecode::JsonValue => parse_json(raw),
        EnvDecode::CommaSeparated => Ok(comma_separated(raw)),
        EnvDecode::CommaAndSpaceSeparated => Ok(comma_and_space_separated(raw)),
        EnvDecode::CommaThenSpaceSeparated => Ok(comma_then_space_separated(raw)),
        EnvDecode::JsonListOrCommaSeparated => json_list_or_split(raw, ','),
        EnvDecode::JsonListOrSpaceSeparated => json_list_or_split(raw, ' '),
        EnvDecode::TracesSpan => traces_span(raw),
        EnvDecode::RawString | EnvDecode::DurationString => Ok(Value::String(raw.to_string())),
        EnvDecode::Bool => parse_bool(raw).map(Value::Bool),
        EnvDecode::Integer => parse_integer(raw),
        EnvDecode::Float => parse_float(raw),
        EnvDecode::StringList => Ok(whitespace_list(raw)),
    }
}

/// `env_parser: json` and the map/object type fallback (`ParseEnvJSON`, `config.go:734`).
///
/// The Agent decodes into the declared type; we parse to a generic `Value` and let the leaf's
/// own deserializer enforce its shape. A parse error propagates (the Agent logs and yields the zero
/// value).
fn parse_json(raw: &str) -> Result<Value, String> {
    serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))
}

/// `env_parser: comma_separated` (`ParseEnvSplitComma`, `config.go:704`).
///
/// `strings.Split` on `,` with no trimming; an empty string yields an empty list (the Agent's
/// explicit special case), not a one-element list containing `""`.
fn comma_separated(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::Array(Vec::new());
    }
    string_array(raw.split(','))
}

/// `env_parser: comma_and_space_separated` (`ParseEnvSplitCommaAndSpace`, `helper/env.go:87`).
///
/// `strings.FieldsFunc` on `,` or space: consecutive separators collapse and empty fields are
/// dropped.
fn comma_and_space_separated(raw: &str) -> Value {
    string_array(raw.split([',', ' ']).filter(|s| !s.is_empty()))
}

/// `env_parser: comma_then_space_separated` (`ParseEnvSplitCommaThenSpace`, `helper/env.go:99`).
///
/// If the string contains a comma, split on commas only; otherwise split on spaces. Each element is
/// trimmed, and empties are kept (`strings.Split` semantics), so `a,,b` yields three elements.
fn comma_then_space_separated(raw: &str) -> Value {
    let sep = if raw.contains(',') { ',' } else { ' ' };
    string_array(raw.split(sep).map(str::trim))
}

/// `env_parser: json_list_or_{comma,space}_separated` (`jsonOrSplitBy`, `helper/env.go:120`).
///
/// A value delimited by `[` and `]` is parsed as a JSON string list; anything else is split on
/// `sep`. The input is trimmed before the `[...]` test. A JSON list that fails to parse propagates
/// an error (the Agent logs and yields nil).
fn json_list_or_split(raw: &str, sep: char) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let list: Vec<String> = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON string list: {e}"))?;
        return Ok(string_array(list));
    }
    Ok(string_array(trimmed.split(sep)))
}

/// `env_parser: traces_span` (`parseAnalyzedSpans`, `helper/env.go:35`).
///
/// `service|operation=rate,...` into `{ "service|operation": rate }`. An empty string yields an
/// empty object; a token that is not `name=rate` with a numeric rate propagates an error.
fn traces_span(raw: &str) -> Result<Value, String> {
    let mut map = Map::new();
    if raw.is_empty() {
        return Ok(Value::Object(map));
    }
    for token in raw.split(',') {
        let (name, rate) = token
            .split_once('=')
            .ok_or_else(|| format!("bad traces_span token `{token}`: expected name=rate"))?;
        let rate: f64 = rate.parse().map_err(|_| format!("bad traces_span rate in `{token}`"))?;
        let rate = Number::from_f64(rate).ok_or_else(|| format!("non-finite traces_span rate in `{token}`"))?;
        map.insert(name.to_string(), Value::Number(rate));
    }
    Ok(Value::Object(map))
}

/// The `[]string` type fallback (`cast.ToStringSliceE`): split on whitespace, dropping empties.
fn whitespace_list(raw: &str) -> Value {
    string_array(raw.split_whitespace())
}

/// Boolean type fallback (`cast.ToBoolE` → `strconv.ParseBool`).
///
/// Accepts exactly Go's grammar: `1`, `t`, `T`, `TRUE`, `true`, `True`, and the false equivalents.
fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        other => Err(format!("invalid boolean `{other}`")),
    }
}

/// Integer type fallback (`cast.ToIntE`).
fn parse_integer(raw: &str) -> Result<Value, String> {
    raw.trim()
        .parse::<i64>()
        .map(|n| Value::Number(n.into()))
        .map_err(|_| format!("invalid integer `{raw}`"))
}

/// Float type fallback (`cast.ToFloat64E`).
fn parse_float(raw: &str) -> Result<Value, String> {
    let n: f64 = raw.trim().parse().map_err(|_| format!("invalid number `{raw}`"))?;
    Number::from_f64(n)
        .map(Value::Number)
        .ok_or_else(|| format!("non-finite number `{raw}`"))
}

/// Collects strings into a JSON array of strings.
fn string_array<S: AsRef<str>, I: IntoIterator<Item = S>>(items: I) -> Value {
    Value::Array(
        items
            .into_iter()
            .map(|s| Value::String(s.as_ref().to_string()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn arr(items: &[&str]) -> Value {
        json!(items)
    }

    #[test]
    fn json_parses_arbitrary_documents() {
        assert_eq!(decode(r#"["a","b"]"#, EnvDecode::Json).unwrap(), arr(&["a", "b"]));
        assert_eq!(
            decode(r#"{"k":["v"]}"#, EnvDecode::JsonValue).unwrap(),
            json!({"k": ["v"]})
        );
        assert!(decode("{bad", EnvDecode::Json).is_err());
    }

    #[test]
    fn comma_separated_does_not_trim() {
        assert_eq!(decode("a, b", EnvDecode::CommaSeparated).unwrap(), arr(&["a", " b"]));
        assert_eq!(decode("", EnvDecode::CommaSeparated).unwrap(), Value::Array(vec![]));
    }

    #[test]
    fn comma_and_space_drops_empties() {
        assert_eq!(
            decode("a, b c,,d", EnvDecode::CommaAndSpaceSeparated).unwrap(),
            arr(&["a", "b", "c", "d"])
        );
    }

    #[test]
    fn comma_then_space_keeps_empties_and_trims() {
        assert_eq!(
            decode("a,,b", EnvDecode::CommaThenSpaceSeparated).unwrap(),
            arr(&["a", "", "b"])
        );
        assert_eq!(
            decode("a b  c", EnvDecode::CommaThenSpaceSeparated).unwrap(),
            arr(&["a", "b", "", "c"])
        );
    }

    #[test]
    fn json_list_or_comma_takes_either_branch() {
        assert_eq!(
            decode(r#"  ["a","b"] "#, EnvDecode::JsonListOrCommaSeparated).unwrap(),
            arr(&["a", "b"])
        );
        assert_eq!(
            decode("a,b", EnvDecode::JsonListOrCommaSeparated).unwrap(),
            arr(&["a", "b"])
        );
        assert!(decode("[bad", EnvDecode::JsonListOrCommaSeparated).is_ok()); // no closing ], so split
        assert!(decode(r#"["unterminated"#, EnvDecode::JsonListOrCommaSeparated).is_ok());
        assert!(decode("[1,2]", EnvDecode::JsonListOrCommaSeparated).is_err()); // JSON list of non-strings
    }

    #[test]
    fn json_list_or_space_splits_on_space() {
        assert_eq!(
            decode("a b", EnvDecode::JsonListOrSpaceSeparated).unwrap(),
            arr(&["a", "b"])
        );
    }

    #[test]
    fn traces_span_builds_a_rate_map() {
        assert_eq!(
            decode("svc|op=0.5,other|op2=1", EnvDecode::TracesSpan).unwrap(),
            json!({ "svc|op": 0.5, "other|op2": 1.0 })
        );
        assert_eq!(decode("", EnvDecode::TracesSpan).unwrap(), json!({}));
        assert!(decode("svc|op", EnvDecode::TracesSpan).is_err());
        assert!(decode("svc|op=notnum", EnvDecode::TracesSpan).is_err());
    }

    #[test]
    fn scalar_fallbacks() {
        assert_eq!(decode("hello", EnvDecode::RawString).unwrap(), json!("hello"));
        assert_eq!(decode("true", EnvDecode::Bool).unwrap(), json!(true));
        assert_eq!(decode("T", EnvDecode::Bool).unwrap(), json!(true));
        assert!(decode("yes", EnvDecode::Bool).is_err());
        assert_eq!(decode("9125", EnvDecode::Integer).unwrap(), json!(9125));
        assert!(decode("9125.0", EnvDecode::Integer).is_err());
        assert_eq!(decode("1.5", EnvDecode::Float).unwrap(), json!(1.5));
    }

    #[test]
    fn string_list_splits_on_whitespace() {
        assert_eq!(
            decode("env:prod  team:core", EnvDecode::StringList).unwrap(),
            arr(&["env:prod", "team:core"])
        );
        assert_eq!(decode("   ", EnvDecode::StringList).unwrap(), Value::Array(vec![]));
    }

    #[test]
    fn duration_is_carried_through_as_a_string() {
        // A duration leaf stays shape-tolerant; the raw text reaches `duration_de` unchanged.
        assert_eq!(decode("10s", EnvDecode::DurationString).unwrap(), json!("10s"));
        assert_eq!(decode("30", EnvDecode::DurationString).unwrap(), json!("30"));
    }
}
