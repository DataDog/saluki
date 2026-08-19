//! Deserialize a schema scalar leaf the way the Datadog Agent reads one.
//!
//! The Agent never reads a setting as the type its YAML happens to hold: `GetBool`, `GetInt`,
//! `GetFloat64`, and `GetString` each coerce whatever is stored through `spf13/cast`. Permissiveness
//! is therefore a property of the leaf's declared type, not of the key, and `dogstatsd_port: "8125"`
//! or `use_v3_api.series.enabled: true` are configurations the Agent accepts.
//!
//! This module ports `cast.To{Bool,Int64,Float64,String}E` so a leaf accepts every spelling the
//! Agent accepts, while the generated field keeps the schema's type. Codegen attaches these to every
//! scalar leaf, and [`crate::env_decode`] routes environment strings through the same parsers, so one
//! accept-set serves every configuration source.
//!
//! Two deliberate divergences from `cast`:
//!
//! - A value `cast` cannot convert is a hard error here. `cast.To*` swallows its error and yields the
//!   zero value, so the Agent silently reads a malformed setting as `false`/`0`/`""`;
//!   [`crate::env_decode`] already rejects such a value rather than losing it.
//! - A numeric string is accepted in decimal only, not in Go's base-prefixed or underscored integer
//!   literal forms. YAML and JSON parse those spellings into numbers before ADP sees them.

use std::fmt;

use serde::de::{self, Deserializer, Unexpected, Visitor};

/// `cast.ToBoolE` for a string: Go's `strconv.ParseBool` grammar, exactly.
///
/// # Errors
///
/// Returns a message naming the value when it is not one of the accepted spellings.
pub(crate) fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        other => Err(format!("invalid boolean `{other}`")),
    }
}

/// `cast.ToInt64E` for a string.
///
/// # Errors
///
/// Returns a message naming the value when it is not a decimal integer.
pub(crate) fn parse_i64(raw: &str) -> Result<i64, String> {
    trim_zero_decimal(raw.trim())
        .parse::<i64>()
        .map_err(|_| format!("invalid integer `{raw}`"))
}

/// `cast`'s `trimZeroDecimal`, which drops an all-zero fraction before integer parsing, so `"8125.0"`
/// is an integer setting while `"8125.5"` is not.
fn trim_zero_decimal(raw: &str) -> &str {
    match raw.split_once('.') {
        Some((integer, fraction)) if !fraction.is_empty() && fraction.bytes().all(|byte| byte == b'0') => integer,
        _ => raw,
    }
}

/// `cast.ToFloat64E` for a string.
///
/// # Errors
///
/// Returns a message naming the value when it is not a finite number.
pub(crate) fn parse_f64(raw: &str) -> Result<f64, String> {
    let parsed: f64 = raw.trim().parse().map_err(|_| format!("invalid number `{raw}`"))?;
    if !parsed.is_finite() {
        return Err(format!("non-finite number `{raw}`"));
    }
    Ok(parsed)
}

/// Deserializes a `boolean` leaf (`cast.ToBoolE`).
///
/// # Errors
///
/// Returns an error for a value the Agent cannot cast to a boolean: an unrecognized string or a
/// compound value.
pub(crate) fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(BoolVisitor)
}

/// Deserializes an `integer` leaf (`cast.ToInt64E`).
///
/// # Errors
///
/// Returns an error for a value the Agent cannot cast to an integer: a non-numeric string, an
/// out-of-range number, or a compound value.
pub(crate) fn deserialize_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(I64Visitor)
}

/// Deserializes a `number` leaf (`cast.ToFloat64E`).
///
/// # Errors
///
/// Returns an error for a value the Agent cannot cast to a number: a non-numeric string or a
/// compound value.
pub(crate) fn deserialize_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(F64Visitor)
}

/// Deserializes a `string` leaf (`cast.ToStringE`).
///
/// # Errors
///
/// Returns an error for a compound value, the one shape the Agent cannot render as a string.
pub(crate) fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(StringVisitor)
}

/// Renders a JSON value as a `string` leaf (`cast.ToStringE`).
///
/// A witness method that receives a leaf as raw JSON, rather than as a generated field, renders it
/// through this so that `1.0` and a null read as the Agent reads them (`"1"` and `""`) instead of as
/// their JSON spelling.
///
/// # Errors
///
/// Returns an error for a compound value, the one shape the Agent cannot render as a string.
pub fn cast_to_string(value: &::serde_json::Value) -> Result<String, String> {
    value.deserialize_any(StringVisitor).map_err(|e| e.to_string())
}

/// Deserializes an optional `string` leaf, where an absent or null value stays `None`.
///
/// # Errors
///
/// Same as [`deserialize_string`] for a present value.
pub(crate) fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptionalStringVisitor)
}

/// Deserializes an optional `integer` leaf, where an absent or null value stays `None`.
///
/// # Errors
///
/// Same as [`deserialize_i64`] for a present value.
pub(crate) fn deserialize_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptionalI64Visitor)
}

struct BoolVisitor;

impl Visitor<'_> for BoolVisitor {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a boolean, a boolean string, or a number")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<bool, E> {
        Ok(value)
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<bool, E> {
        Ok(value != 0)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<bool, E> {
        Ok(value != 0)
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<bool, E> {
        Ok(value != 0.0)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<bool, E> {
        parse_bool(value).map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
    }

    // `cast` maps a nil to the zero value, which for a configuration leaf means an explicit
    // `key: null` reads as if the key were unset.
    fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
        Ok(false)
    }
}

struct I64Visitor;

impl Visitor<'_> for I64Visitor {
    type Value = i64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an integer, a numeric string, or a boolean")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<i64, E> {
        Ok(i64::from(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<i64, E> {
        Ok(value)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<i64, E> {
        i64::try_from(value).map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
    }

    // Go's `int(float64)` truncation, which is what the Agent reads for a leaf written `10.5`.
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<i64, E> {
        if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
            return Err(E::invalid_value(Unexpected::Float(value), &self));
        }
        Ok(value.trunc() as i64)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<i64, E> {
        parse_i64(value).map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
    }

    fn visit_unit<E: de::Error>(self) -> Result<i64, E> {
        Ok(0)
    }
}

struct F64Visitor;

impl Visitor<'_> for F64Visitor {
    type Value = f64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number, a numeric string, or a boolean")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<f64, E> {
        Ok(if value { 1.0 } else { 0.0 })
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<f64, E> {
        Ok(value)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<f64, E> {
        parse_f64(value).map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
    }

    fn visit_unit<E: de::Error>(self) -> Result<f64, E> {
        Ok(0.0)
    }
}

struct StringVisitor;

impl Visitor<'_> for StringVisitor {
    type Value = String;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string, a boolean, or a number")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<String, E> {
        Ok(value.to_string())
    }

    // Rust's `Display` for `f64` matches Go's `FormatFloat(v, 'f', -1, 64)`: the shortest form that
    // round-trips, never in exponent notation.
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        Ok(value.to_owned())
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
        Ok(value)
    }

    fn visit_unit<E: de::Error>(self) -> Result<String, E> {
        Ok(String::new())
    }
}

struct OptionalStringVisitor;

impl<'de> Visitor<'de> for OptionalStringVisitor {
    type Value = Option<String>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string, a boolean, a number, or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<Option<String>, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Option<String>, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Option<String>, D::Error> {
        deserializer.deserialize_any(StringVisitor).map(Some)
    }
}

struct OptionalI64Visitor;

impl<'de> Visitor<'de> for OptionalI64Visitor {
    type Value = Option<i64>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an integer, a numeric string, a boolean, or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<Option<i64>, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Option<i64>, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Option<i64>, D::Error> {
        deserializer.deserialize_any(I64Visitor).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::{json, Value};

    use super::*;

    #[derive(Deserialize)]
    struct Bool(#[serde(deserialize_with = "deserialize_bool")] bool);

    #[derive(Deserialize)]
    struct Int(#[serde(deserialize_with = "deserialize_i64")] i64);

    #[derive(Deserialize)]
    struct Float(#[serde(deserialize_with = "deserialize_f64")] f64);

    #[derive(Deserialize)]
    struct Str(#[serde(deserialize_with = "deserialize_string")] String);

    #[derive(Deserialize)]
    struct OptStr(#[serde(deserialize_with = "deserialize_optional_string")] Option<String>);

    fn as_bool(value: Value) -> Result<bool, String> {
        serde_json::from_value::<Bool>(value)
            .map(|b| b.0)
            .map_err(|e| e.to_string())
    }

    fn as_int(value: Value) -> Result<i64, String> {
        serde_json::from_value::<Int>(value)
            .map(|i| i.0)
            .map_err(|e| e.to_string())
    }

    fn as_float(value: Value) -> Result<f64, String> {
        serde_json::from_value::<Float>(value)
            .map(|f| f.0)
            .map_err(|e| e.to_string())
    }

    fn as_string(value: Value) -> Result<String, String> {
        serde_json::from_value::<Str>(value)
            .map(|s| s.0)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn bool_accepts_every_spelling_go_accepts() {
        for truthy in [
            json!(true),
            json!("true"),
            json!("True"),
            json!("TRUE"),
            json!("t"),
            json!("T"),
            json!("1"),
        ] {
            assert_eq!(as_bool(truthy.clone()), Ok(true), "{truthy}");
        }
        for falsy in [
            json!(false),
            json!("false"),
            json!("False"),
            json!("FALSE"),
            json!("f"),
            json!("F"),
            json!("0"),
        ] {
            assert_eq!(as_bool(falsy.clone()), Ok(false), "{falsy}");
        }

        // Any non-zero number is truthy, and a null reads as the zero value.
        assert_eq!(as_bool(json!(2)), Ok(true));
        assert_eq!(as_bool(json!(-1)), Ok(true));
        assert_eq!(as_bool(json!(1.0)), Ok(true));
        assert_eq!(as_bool(json!(0.0)), Ok(false));
        assert_eq!(as_bool(json!(null)), Ok(false));
    }

    #[test]
    fn bool_rejects_what_go_rejects() {
        // `strconv.ParseBool` accepts none of these.
        for rejected in [json!("yes"), json!("on"), json!(""), json!([true]), json!({"a": true})] {
            assert!(as_bool(rejected.clone()).is_err(), "{rejected}");
        }
    }

    #[test]
    fn integer_accepts_numeric_strings_floats_and_booleans() {
        assert_eq!(as_int(json!(8125)), Ok(8125));
        assert_eq!(as_int(json!("8125")), Ok(8125));
        assert_eq!(as_int(json!(" -7 ")), Ok(-7));
        assert_eq!(as_int(json!(true)), Ok(1));
        assert_eq!(as_int(json!(null)), Ok(0));

        // `cast` drops an all-zero fraction from a numeric string.
        assert_eq!(as_int(json!("8125.0")), Ok(8125));
        assert_eq!(as_int(json!("8125.000")), Ok(8125));
        assert_eq!(as_int(json!("-8125.0")), Ok(-8125));

        // Go truncates toward zero rather than rounding.
        assert_eq!(as_int(json!(10.9)), Ok(10));
        assert_eq!(as_int(json!(-10.9)), Ok(-10));
    }

    #[test]
    fn integer_rejects_unparseable_and_out_of_range_values() {
        for rejected in [
            json!("8125ms"),
            json!(""),
            json!("0x1f"),
            json!("8125.5"),
            json!("8125."),
            json!(1e300),
            json!(["8125"]),
        ] {
            assert!(as_int(rejected.clone()).is_err(), "{rejected}");
        }
    }

    #[test]
    fn number_accepts_numeric_strings_integers_and_booleans() {
        assert_eq!(as_float(json!(1.5)), Ok(1.5));
        assert_eq!(as_float(json!("1.5")), Ok(1.5));
        assert_eq!(as_float(json!(2)), Ok(2.0));
        assert_eq!(as_float(json!(true)), Ok(1.0));
        assert_eq!(as_float(json!(null)), Ok(0.0));
        assert!(as_float(json!("half")).is_err());
    }

    #[test]
    fn string_accepts_every_scalar() {
        assert_eq!(as_string(json!("datadog_only")), Ok("datadog_only".to_owned()));
        assert_eq!(as_string(json!(true)), Ok("true".to_owned()));
        assert_eq!(as_string(json!(false)), Ok("false".to_owned()));
        assert_eq!(as_string(json!(10485760)), Ok("10485760".to_owned()));
        assert_eq!(as_string(json!(-1)), Ok("-1".to_owned()));
        assert_eq!(as_string(json!(10.5)), Ok("10.5".to_owned()));
        assert_eq!(as_string(json!(null)), Ok(String::new()));

        // A float with no fractional part renders without one, as Go's shortest form does.
        assert_eq!(as_string(json!(1.0)), Ok("1".to_owned()));
    }

    #[test]
    fn string_rejects_compound_values() {
        for rejected in [json!(["a"]), json!({"a": "b"})] {
            assert!(as_string(rejected.clone()).is_err(), "{rejected}");
        }
    }

    #[test]
    fn optional_string_distinguishes_null_from_a_coerced_scalar() {
        let absent = serde_json::from_value::<OptStr>(json!(null))
            .expect("null deserializes")
            .0;
        assert_eq!(absent, None);

        let coerced = serde_json::from_value::<OptStr>(json!(3))
            .expect("scalar deserializes")
            .0;
        assert_eq!(coerced, Some("3".to_owned()));
    }
}
