//! Deserialize schema values the way the Datadog Agent reads them.
//!
//! The Agent never reads a setting as the type its YAML happens to hold: `GetBool`, `GetInt`,
//! `GetFloat64`, and `GetString` each coerce whatever is stored through `spf13/cast`. Permissiveness
//! is therefore a property of the leaf's declared type, not of the key, and `dogstatsd_port: "8125"`
//! or `use_v3_api.series.enabled: true` are configurations the Agent accepts.
//!
//! This module ports `cast.To{Bool,Int64,Float64,String}E` so scalar leaves and string-map values
//! accept every spelling the Agent accepts while keeping the schema's type. [`crate::env_decode`]
//! uses the same parsers for environment strings.
//!
//! Three deliberate divergences from `cast`:
//!
//! - A value `cast` cannot convert is a hard error here. `cast.To*` swallows its error and yields the
//!   zero value, so the Agent silently reads a malformed setting as `false`/`0`/`""`;
//!   [`crate::env_decode`] already rejects such a value rather than losing it. The exception is an
//!   integer string with a leading `0` and an `8` or `9` digit (see [`parse_i64`]), which reads as `0`
//!   as it does in the Agent.
//! - Surrounding whitespace is trimmed from a numeric string, which Go's parsers reject.
//! - A float string must be finite. Go accepts `inf` and `nan`, which a JSON number cannot hold.

use std::{collections::HashMap, fmt, marker::PhantomData};

use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Unexpected, Visitor};

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

/// `cast.ToInt64E` for a string: Go's `strconv.ParseInt(s, 0, 64)` after `trimZeroDecimal`.
///
/// Accepts an optional sign; `0x`, `0o` and `0b` prefixes in either case; a bare leading `0` as octal;
/// `_` digit separators where Go allows them; and the full `i64` range. `"0x10.0"` is `16` because the
/// zero fraction is dropped before the base is read.
///
/// A signed or unsigned run of decimal digits with a leading `0` and an `8` or `9`, such as `"08"`,
/// `"-09"`, `"0128"` or `"08.0"`, is octal with a digit out of range, so Go rejects it. The Agent
/// still starts with such a value and reads it as `0` (`GetInt` logs and returns the zero value), so it
/// reads as `0` here, with a warning.
///
/// # Errors
///
/// Returns a message naming the value when it is not an integer in that grammar.
pub(crate) fn parse_i64(raw: &str) -> Result<i64, String> {
    let literal = trim_zero_decimal(raw.trim());
    if let Some(value) = parse_go_int(literal) {
        return Ok(value);
    }
    if is_invalid_octal(literal) {
        tracing::warn!(
            "Integer `{raw}` has a leading 0, so it is octal, but it holds an 8 or 9. Reading it as 0, as the \
             Datadog Agent does."
        );
        return Ok(0);
    }
    Err(format!("invalid integer `{raw}`"))
}

/// Whether `s` is an optionally signed run of decimal digits with a leading `0` and an `8` or `9`.
fn is_invalid_octal(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    digits.len() > 1
        && digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && digits.bytes().any(|byte| byte >= b'8')
}

/// A decimal integer, optionally followed by an all-zero fraction.
///
/// This is the grammar of Saluki-only settings, which have no Agent counterpart: no base prefixes,
/// no underscores, and a leading `0` is decimal (`"010"` is `10`).
///
/// # Errors
///
/// Returns a message naming the value when it is not in that grammar.
pub(crate) fn parse_decimal_i64(raw: &str) -> Result<i64, String> {
    trim_zero_fraction(raw.trim())
        .parse::<i64>()
        .map_err(|_| format!("invalid integer `{raw}`"))
}

/// Drops an all-zero fraction, so `"8125.0"` is an integer while `"8125.5"` is not.
fn trim_zero_fraction(raw: &str) -> &str {
    match raw.split_once('.') {
        Some((integer, fraction)) if !fraction.is_empty() && fraction.bytes().all(|byte| byte == b'0') => integer,
        _ => raw,
    }
}

/// `cast`'s `trimZeroDecimal`, byte for byte.
///
/// Scanning from the end, a run of `0`s followed by a `.` drops both. Any other byte stops the scan,
/// and a `.` met before any `0` is skipped. It is not limited to numbers: `"1.0.0"` becomes `"1.0"`
/// and `".0"` becomes `""`.
fn trim_zero_decimal(s: &str) -> &str {
    let mut found_zero = false;
    for (index, byte) in s.bytes().enumerate().rev() {
        match byte {
            b'.' if found_zero => return &s[..index],
            b'.' => {}
            b'0' => found_zero = true,
            _ => return s,
        }
    }
    s
}

/// Go's lowercase fold for ASCII letters, as used throughout `strconv`.
fn lower(byte: u8) -> u8 {
    byte | 0x20
}

/// Go's `strconv.ParseInt(s, 0, 64)`.
fn parse_go_int(s: &str) -> Option<i64> {
    let (negative, unsigned) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let magnitude = parse_go_uint(unsigned.as_bytes())?;
    if negative {
        i64::try_from(-i128::from(magnitude)).ok()
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// Go's `strconv.ParseUint(s, 0, 64)`: the base comes from the prefix.
fn parse_go_uint(s: &[u8]) -> Option<u64> {
    let (base, digits) = match s {
        [b'0', prefix, rest @ ..] if !rest.is_empty() && lower(*prefix) == b'b' => (2, rest),
        [b'0', prefix, rest @ ..] if !rest.is_empty() && lower(*prefix) == b'o' => (8, rest),
        [b'0', prefix, rest @ ..] if !rest.is_empty() && lower(*prefix) == b'x' => (16, rest),
        [b'0', rest @ ..] => (8, rest),
        [] => return None,
        _ => (10, s),
    };

    let mut value: u64 = 0;
    let mut saw_underscore = false;
    for &byte in digits {
        let digit = match byte {
            b'_' => {
                saw_underscore = true;
                continue;
            }
            b'0'..=b'9' => byte - b'0',
            _ if lower(byte).is_ascii_lowercase() => lower(byte) - b'a' + 10,
            _ => return None,
        };
        if digit >= base {
            return None;
        }
        value = value.checked_mul(u64::from(base))?.checked_add(u64::from(digit))?;
    }
    if saw_underscore && !underscore_ok(s) {
        return None;
    }
    Some(value)
}

/// Go's `underscoreOK`: whether `_` appears only between digits, or between a base prefix and a digit.
fn underscore_ok(s: &[u8]) -> bool {
    #[derive(PartialEq)]
    enum Last {
        Start,
        Digit,
        Underscore,
        Other,
    }

    let s = match s.first() {
        Some(b'+' | b'-') => &s[1..],
        _ => s,
    };

    let mut last = Last::Start;
    let mut hex = false;
    let mut rest = s;
    if let [b'0', prefix, tail @ ..] = s {
        if matches!(lower(*prefix), b'b' | b'o' | b'x') {
            // The prefix counts as a digit, so `0x_1` is fine.
            last = Last::Digit;
            hex = lower(*prefix) == b'x';
            rest = tail;
        }
    }

    for &byte in rest {
        if byte.is_ascii_digit() || (hex && matches!(lower(byte), b'a'..=b'f')) {
            last = Last::Digit;
        } else if byte == b'_' {
            if last != Last::Digit {
                return false;
            }
            last = Last::Underscore;
        } else if last == Last::Underscore {
            return false;
        } else {
            last = Last::Other;
        }
    }
    last != Last::Underscore
}

/// `cast.ToFloat64E` for a string: Go's `strconv.ParseFloat(s, 64)`, finite values only.
///
/// Accepts an optional sign, decimal literals with an optional exponent, `_` digit separators where
/// Go allows them, and hexadecimal literals, which require a `p` exponent (`"0x1p4"` is `16`).
///
/// # Errors
///
/// Returns a message naming the value when it is not a finite number in that grammar. `inf`, `nan`
/// and a literal that overflows are rejected.
pub(crate) fn parse_f64(raw: &str) -> Result<f64, String> {
    finite_number(raw, parse_go_float(raw.trim()))
}

/// A finite number in Rust's `f64` grammar: decimal only, with no `_` separators.
///
/// This is the grammar of Saluki-only settings, which have no Agent counterpart.
///
/// # Errors
///
/// Returns a message naming the value when it is not a finite number in that grammar.
pub(crate) fn parse_decimal_f64(raw: &str) -> Result<f64, String> {
    finite_number(raw, raw.trim().parse().ok())
}

/// Names `raw` in the error for a missing or non-finite `parsed`.
fn finite_number(raw: &str, parsed: Option<f64>) -> Result<f64, String> {
    let parsed = parsed.ok_or_else(|| format!("invalid number `{raw}`"))?;
    if !parsed.is_finite() {
        return Err(format!("non-finite number `{raw}`"));
    }
    Ok(parsed)
}

/// The syntax of a Go float literal, as `strconv.readFloat` reads it. Mantissa digits past what a `u64`
/// holds only set `truncated`.
struct FloatLiteral {
    negative: bool,
    hex: bool,
    mantissa: u64,
    /// The power of the literal's base, 2 for hex and 10 otherwise, by which `mantissa` is scaled.
    exponent: i64,
    truncated: bool,
}

/// Go's `strconv.ParseFloat(s, 64)`, returning an infinity for an overflowing literal and `None` for
/// a syntax error. `inf` and `nan` are syntax errors here.
fn parse_go_float(s: &str) -> Option<f64> {
    let literal = FloatLiteral::read(s.as_bytes())?;
    if literal.hex {
        return Some(literal.hex_to_f64());
    }
    // Without its underscores, the literal is in Rust's grammar, which rounds correctly at any length.
    s.replace('_', "").parse().ok()
}

impl FloatLiteral {
    /// Go's `strconv.readFloat`, requiring that it consume all of `s`.
    fn read(s: &[u8]) -> Option<Self> {
        let mut i = 0;
        let mut negative = false;
        match s.first() {
            Some(b'+') => i += 1,
            Some(b'-') => {
                negative = true;
                i += 1;
            }
            _ => {}
        }

        let hex = s.len() > i + 2 && s[i] == b'0' && lower(s[i + 1]) == b'x';
        let (max_mantissa_digits, exponent_char) = if hex {
            i += 2;
            (16, b'p')
        } else {
            (19, b'e')
        };
        let base: u64 = if hex { 16 } else { 10 };

        let mut underscores = false;
        let mut saw_dot = false;
        let mut saw_digits = false;
        let mut digits = 0;
        let mut mantissa_digits = 0;
        let mut mantissa: u64 = 0;
        let mut truncated = false;
        let mut dot_position: i64 = 0;
        while i < s.len() {
            let byte = s[i];
            match byte {
                b'_' => underscores = true,
                b'.' => {
                    if saw_dot {
                        break;
                    }
                    saw_dot = true;
                    dot_position = digits;
                }
                b'0'..=b'9' => {
                    saw_digits = true;
                    if byte == b'0' && digits == 0 {
                        dot_position -= 1;
                    } else {
                        digits += 1;
                        if mantissa_digits < max_mantissa_digits {
                            mantissa = mantissa * base + u64::from(byte - b'0');
                            mantissa_digits += 1;
                        } else if byte != b'0' {
                            truncated = true;
                        }
                    }
                }
                _ if hex && matches!(lower(byte), b'a'..=b'f') => {
                    saw_digits = true;
                    digits += 1;
                    if mantissa_digits < max_mantissa_digits {
                        mantissa = mantissa * 16 + u64::from(lower(byte) - b'a' + 10);
                        mantissa_digits += 1;
                    } else {
                        truncated = true;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        if !saw_digits {
            return None;
        }
        if !saw_dot {
            dot_position = digits;
        }
        let mut mantissa_digits = i64::from(mantissa_digits);
        if hex {
            dot_position *= 4;
            mantissa_digits *= 4;
        }

        if i < s.len() && lower(s[i]) == exponent_char {
            i += 1;
            let mut exponent_sign = 1;
            match s.get(i) {
                Some(b'+') => i += 1,
                Some(b'-') => {
                    exponent_sign = -1;
                    i += 1;
                }
                _ => {}
            }
            if !s.get(i)?.is_ascii_digit() {
                return None;
            }
            let mut exponent: i64 = 0;
            while let Some(&byte) = s.get(i) {
                if byte == b'_' {
                    underscores = true;
                } else if byte.is_ascii_digit() {
                    // Go's cap, which keeps a huge exponent from overflowing.
                    if exponent < 10_000 {
                        exponent = exponent * 10 + i64::from(byte - b'0');
                    }
                } else {
                    break;
                }
                i += 1;
            }
            dot_position += exponent * exponent_sign;
        } else if hex {
            return None;
        }

        if i != s.len() || (underscores && !underscore_ok(s)) {
            return None;
        }
        Some(Self {
            negative,
            hex,
            mantissa,
            exponent: if mantissa == 0 {
                0
            } else {
                dot_position - mantissa_digits
            },
            truncated,
        })
    }

    /// Go's `atofHex` for `float64`: rounds `mantissa * 2^exponent` to nearest even, with a sticky bit for
    /// bits beyond the 64 that `mantissa` holds. Overflow is infinity.
    fn hex_to_f64(&self) -> f64 {
        const MANTISSA_BITS: u32 = 52;
        const EXPONENT_BIAS: i64 = -1023;
        const MAX_EXPONENT: i64 = 1023;
        const MIN_EXPONENT: i64 = EXPONENT_BIAS + 1;

        let mut mantissa = self.mantissa;
        // The mantissa is now implicitly divided by 2^MANTISSA_BITS.
        let mut exponent = self.exponent + i64::from(MANTISSA_BITS);

        // Normalize to a leading 1 bit followed by MANTISSA_BITS bits plus two bits for rounding, the
        // lowest of which records whether anything non-zero lies beyond it.
        while mantissa != 0 && mantissa >> (MANTISSA_BITS + 2) == 0 {
            mantissa <<= 1;
            exponent -= 1;
        }
        if self.truncated {
            mantissa |= 1;
        }
        while mantissa >> (MANTISSA_BITS + 3) != 0 {
            mantissa = mantissa >> 1 | (mantissa & 1);
            exponent += 1;
        }
        // Denormalize an exponent that is too small.
        while mantissa > 1 && exponent < MIN_EXPONENT - 2 {
            mantissa = mantissa >> 1 | (mantissa & 1);
            exponent += 1;
        }

        let mut round = mantissa & 3;
        mantissa >>= 2;
        round |= mantissa & 1;
        exponent += 2;
        if round == 3 {
            mantissa += 1;
            if mantissa == 1 << (MANTISSA_BITS + 1) {
                mantissa >>= 1;
                exponent += 1;
            }
        }
        if mantissa >> MANTISSA_BITS == 0 {
            exponent = EXPONENT_BIAS;
        }
        if exponent > MAX_EXPONENT {
            return if self.negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }

        let mut bits = mantissa & ((1 << MANTISSA_BITS) - 1);
        bits |= (((exponent - EXPONENT_BIAS) & 0x7ff) as u64) << MANTISSA_BITS;
        if self.negative {
            bits |= 1 << 63;
        }
        f64::from_bits(bits)
    }
}

/// `cast.ToStringSliceE` for a string.
///
/// A string that is wholly a JSON array of strings (a `null` element is `""`) or the JSON `null` (an
/// empty list) is decoded as JSON. Anything else, including JSON that is not a string array, is split
/// on whitespace.
///
/// One difference remains: Go repairs a lone UTF-16 surrogate escape in a JSON string, while here
/// that string is not JSON and is split on whitespace.
pub(crate) fn parse_string_list(raw: &str) -> Vec<String> {
    match serde_json::from_str::<Option<Vec<Option<String>>>>(raw) {
        Ok(elements) => elements
            .unwrap_or_default()
            .into_iter()
            .map(Option::unwrap_or_default)
            .collect(),
        Err(_) => raw.split_whitespace().map(str::to_owned).collect(),
    }
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

/// Deserializes a map, or a string holding a JSON-encoded map (`cast.ToStringMap*E`).
///
/// The Agent stores a map-typed environment variable as its raw string and decodes the JSON when the
/// key is read, so its configuration stream carries the string.
///
/// A null is an empty map: the Agent's configuration stream can carry `null` for an empty or
/// cleared map, and its map accessors read it as empty.
/// # Errors
///
/// Returns an error when the value is neither a map, a null, nor a string that decodes to a map, or
/// when a map value does not deserialize as `T`.
pub(crate) fn deserialize_map_or_json_string<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    struct MapOrJsonString<T>(PhantomData<T>);

    impl<'de, T: DeserializeOwned> Visitor<'de> for MapOrJsonString<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map or a JSON-encoded map string")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            serde_json::from_str(value).map_err(|e| E::custom(format_args!("invalid JSON-encoded map: {e}")))
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            T::deserialize(de::value::MapAccessDeserializer::new(map))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            serde_json::from_value(serde_json::Value::Object(serde_json::Map::new())).map_err(E::custom)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            serde_json::from_value(serde_json::Value::Object(serde_json::Map::new())).map_err(E::custom)
        }
    }

    deserializer.deserialize_any(MapOrJsonString(PhantomData))
}

/// Deserializes a string map, coercing each value as the Agent does.
///
/// # Errors
///
/// Returns an error when the map is malformed (see [`deserialize_map_or_json_string`]) or a value is
/// not scalar.
pub(crate) fn deserialize_string_map<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = deserialize_map_or_json_string::<_, HashMap<String, serde_json::Value>>(deserializer)?;
    values
        .into_iter()
        .map(|(key, value)| {
            cast_to_string(&value)
                .map(|value| (key, value))
                .map_err(de::Error::custom)
        })
        .collect()
}

/// Deserializes a number map, coercing each value as the Agent does.
///
/// # Errors
///
/// Returns an error when a value cannot be cast to a number.
pub(crate) fn deserialize_number_map<'de, D>(deserializer: D) -> Result<HashMap<String, f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = deserialize_map_or_json_string::<_, HashMap<String, serde_json::Value>>(deserializer)?;
    values
        .into_iter()
        .map(|(key, value)| cast_to_f64(&value).map(|value| (key, value)).map_err(de::Error::custom))
        .collect()
}

/// Renders a JSON value as a `number` leaf (`cast.ToFloat64E`).
///
/// A witness method that receives a leaf as raw JSON, rather than as a generated field, renders
/// it through this so that `1` and `"1.5"` read as the Agent reads them (`1.0` and `1.5`) instead
/// of rejecting their JSON spelling.
///
/// # Errors
///
/// Returns an error for a value the Agent cannot cast to a number: a non-numeric string or a
/// compound value.
pub fn cast_to_f64(value: &::serde_json::Value) -> Result<f64, String> {
    value.deserialize_any(F64Visitor).map_err(|e| e.to_string())
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

/// Strings with the list that Go's `cast.ToStringSliceE` returns for each.
#[cfg(test)]
pub(crate) const STRING_LIST_CASES: &[(&str, &[&str])] = &[
    (r#"["cr-a","cr-b"]"#, &["cr-a", "cr-b"]),
    (r#"["client_id", "product_id"]"#, &["client_id", "product_id"]),
    (r#" ["a b","c"] "#, &["a b", "c"]),
    (r#"["a",null]"#, &["a", ""]),
    ("[null]", &[""]),
    ("null", &[]),
    ("[]", &[]),
    ("", &[]),
    ("   ", &[]),
    ("a b", &["a", "b"]),
    ("a,b", &["a,b"]),
    ("cr-a cr-b,cr-c", &["cr-a", "cr-b,cr-c"]),
    ("[a,b]", &["[a,b]"]),
    (r#"["a",1]"#, &[r#"["a",1]"#]),
    (r#"{"a":1}"#, &[r#"{"a":1}"#]),
    (r#""abc""#, &[r#""abc""#]),
    (r#"["a"] x"#, &[r#"["a"]"#, "x"]),
    ("a\tb\nc", &["a", "b", "c"]),
];

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

    #[derive(Deserialize)]
    struct StringMap(#[serde(deserialize_with = "deserialize_string_map")] HashMap<String, String>);

    #[derive(Deserialize)]
    struct NumberMap(#[serde(deserialize_with = "deserialize_number_map")] HashMap<String, f64>);

    #[derive(Deserialize)]
    struct FreeFormMap(
        #[serde(deserialize_with = "deserialize_map_or_json_string")] ::serde_json::Map<String, ::serde_json::Value>,
    );

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
            json!("8125.5"),
            json!("8125."),
            json!(1e300),
            json!(["8125"]),
        ] {
            assert!(as_int(rejected.clone()).is_err(), "{rejected}");
        }
    }

    // Each pair is the result of Go's `cast.ToInt64E` (DataDog/cast v1.8.0) for that string.
    #[test]
    fn integer_string_accepts_exactly_the_go_spellings() {
        let accepted: &[(&str, i64)] = &[
            ("0x10", 16),
            ("0X1F", 31),
            ("010", 8),
            ("0010", 8),
            ("0b11", 3),
            ("0B1", 1),
            ("0o17", 15),
            ("0O7", 7),
            ("1_000", 1000),
            ("0x_10", 16),
            ("0_7", 7),
            ("+5", 5),
            ("+0x1", 1),
            ("-0x10", -16),
            ("00", 0),
            ("-0", 0),
            ("0.0", 0),
            ("0x10.0", 16),
            ("10.00", 10),
            ("-9223372036854775808", i64::MIN),
            ("-0x8000000000000000", i64::MIN),
            ("9223372036854775807", i64::MAX),
        ];
        for (raw, expected) in accepted {
            assert_eq!(parse_i64(raw), Ok(*expected), "{raw}");
            assert_eq!(as_int(json!(raw)), Ok(*expected), "{raw} through the leaf deserializer");
        }
    }

    // Go rejects each of these, but the Agent's integer getter reads it as 0 and the Agent starts.
    #[test]
    fn integer_string_with_an_invalid_octal_digit_reads_as_zero() {
        for raw in [
            "08", "09", "-09", "+08", "0128", "00009", "08.0", "08.00", "-08.0", " 08 ",
        ] {
            assert_eq!(parse_i64(raw), Ok(0), "{raw}");
            assert_eq!(as_int(json!(raw)), Ok(0), "{raw} through the leaf deserializer");
        }
    }

    // Go also rejects each of these. The Agent reads them as 0, but only the invalid octal digit family
    // above follows it here.
    #[test]
    fn integer_string_rejects_other_strings_go_rejects() {
        for raw in [
            "",
            "-",
            "08.",
            "08.5",
            "0_8",
            "08e1",
            "0x8g",
            "1__0",
            "_1",
            "1_",
            "0_",
            "0x1_",
            "0x",
            "0b",
            "0b2",
            "0xg",
            "10.50",
            "1e3",
            "5.",
            ".0",
            "1.0.0",
            "9223372036854775808",
            "0x8000000000000000",
            "-9223372036854775809",
            "+-5",
        ] {
            assert_eq!(parse_i64(raw), Err(format!("invalid integer `{raw}`")), "{raw}");
        }
    }

    #[test]
    fn integer_string_trims_surrounding_whitespace_unlike_go() {
        assert_eq!(parse_i64(" 10 "), Ok(10));
        assert_eq!(parse_i64("\t0x10\n"), Ok(16));
    }

    #[test]
    fn decimal_integer_keeps_the_saluki_only_grammar() {
        let accepted: &[(&str, i64)] = &[
            ("010", 10),
            ("08", 8),
            ("+5", 5),
            ("-7", -7),
            ("8125.0", 8125),
            (" 10 ", 10),
        ];
        for (raw, expected) in accepted {
            assert_eq!(parse_decimal_i64(raw), Ok(*expected), "{raw}");
        }
        for raw in ["0x10", "0b11", "1_000", "8125.5", "8125.", ""] {
            assert_eq!(parse_decimal_i64(raw), Err(format!("invalid integer `{raw}`")), "{raw}");
        }
    }

    #[test]
    fn decimal_float_keeps_the_saluki_only_grammar() {
        for (raw, expected) in [("1.5", 1.5), ("1e3", 1000.0), (".5", 0.5), (" 2 ", 2.0)] {
            assert_eq!(parse_decimal_f64(raw), Ok(expected), "{raw}");
        }
        for raw in ["0x1p4", "1_000.5", ""] {
            assert_eq!(parse_decimal_f64(raw), Err(format!("invalid number `{raw}`")), "{raw}");
        }
        assert_eq!(parse_decimal_f64("inf"), Err("non-finite number `inf`".to_owned()));
    }

    // Each pair is the result of Go's `cast.ToFloat64E` for that string.
    #[test]
    fn float_string_accepts_go_literals() {
        let accepted: &[(&str, f64)] = &[
            ("1.5", 1.5),
            ("+1.5", 1.5),
            ("-1.5", -1.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("010", 10.0),
            ("1e3", 1000.0),
            ("1_000.5", 1000.5),
            ("1e5_0", 1e50),
            ("1e-400", 0.0),
            ("0x1p4", 16.0),
            ("0X1P4", 16.0),
            ("-0x1p4", -16.0),
            ("0x1.8p1", 3.0),
            ("0x.8p1", 1.0),
            ("0x1_0p0", 16.0),
            ("0x0p0", 0.0),
        ];
        for (raw, expected) in accepted {
            assert_eq!(parse_f64(raw), Ok(*expected), "{raw}");
            assert_eq!(
                as_float(json!(raw)),
                Ok(*expected),
                "{raw} through the leaf deserializer"
            );
        }
    }

    #[test]
    fn float_string_rejects_what_go_or_json_rejects() {
        for raw in [
            "", ".", "e5", "0x10", "0x1p", "0x1p+", "1_e5", "1__0.5", "_1.5", "1.5_", "1e", "half", "inf", "nan",
            "+Inf", "1e400", "0x1p1024",
        ] {
            assert!(parse_f64(raw).is_err(), "{raw}");
        }
    }

    // Go rounds hex floats to nearest even, so digits past the 64 bits of a `u64` still count.
    #[test]
    fn hex_float_rounds_like_go() {
        let cases: &[(&str, u64)] = &[
            ("0x1p-1074", 0x1),
            ("0x1p-1075", 0x0),
            ("-0x0p0", 0x8000_0000_0000_0000),
            ("0x1.fffffffffffffp1023", 0x7fef_ffff_ffff_ffff),
            ("0x123456789abcdef01p0", 0x43f2_3456_789a_bcdf),
            ("0x1.00000000000008p0", 0x3ff0_0000_0000_0000),
            ("0x1.00000000000018p0", 0x3ff0_0000_0000_0002),
            ("0x1.0000000000000800000001p0", 0x3ff0_0000_0000_0001),
            ("0x1.0000000000000800000p0", 0x3ff0_0000_0000_0000),
        ];
        for (raw, bits) in cases {
            assert_eq!(parse_f64(raw).map(f64::to_bits), Ok(*bits), "{raw}");
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
    fn string_map_coerces_scalar_values() {
        let values = serde_json::from_value::<StringMap>(json!({
            "bool": true,
            "integer": 3,
            "null": null,
            "string": "datadog_only"
        }))
        .expect("scalar values deserialize")
        .0;

        assert_eq!(values["bool"], "true");
        assert_eq!(values["integer"], "3");
        assert_eq!(values["null"], "");
        assert_eq!(values["string"], "datadog_only");
        assert!(serde_json::from_value::<StringMap>(json!({ "compound": [] })).is_err());
    }

    #[test]
    fn string_map_accepts_a_json_encoded_string() {
        let values = serde_json::from_value::<StringMap>(json!(r#"{"bool": true, "string": "datadog_only"}"#))
            .expect("JSON-encoded map deserializes")
            .0;

        assert_eq!(values["bool"], "true");
        assert_eq!(values["string"], "datadog_only");
        for rejected in [json!("not json"), json!(r#"["a"]"#), json!(r#"{"compound": []}"#)] {
            assert!(
                serde_json::from_value::<StringMap>(rejected.clone()).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn string_map_reads_null_as_empty() {
        for empty in [json!(null), json!({})] {
            let values = serde_json::from_value::<StringMap>(empty.clone())
                .expect("empty map deserializes")
                .0;
            assert!(values.is_empty(), "{empty}");
        }
    }

    #[test]
    fn number_map_coerces_numeric_values() {
        let values = serde_json::from_value::<NumberMap>(json!({
            "integer": 3,
            "float": 0.8,
            "string": "0.25"
        }))
        .expect("numeric values deserialize")
        .0;

        assert_eq!(values["integer"], 3.0);
        assert_eq!(values["float"], 0.8);
        assert_eq!(values["string"], 0.25);
        assert!(serde_json::from_value::<NumberMap>(json!({ "compound": [] })).is_err());
        assert!(serde_json::from_value::<NumberMap>(json!({ "text": "datadog_only" })).is_err());
    }

    #[test]
    fn number_map_accepts_a_json_encoded_string() {
        let values = serde_json::from_value::<NumberMap>(json!(r#"{"one": 0.5, "two": 2}"#))
            .expect("JSON-encoded map deserializes")
            .0;

        assert_eq!(values["one"], 0.5);
        assert_eq!(values["two"], 2.0);
        for rejected in [json!("not json"), json!(r#"["a"]"#), json!(r#"{"compound": []}"#)] {
            assert!(
                serde_json::from_value::<NumberMap>(rejected.clone()).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn free_form_map_accepts_a_json_encoded_string() {
        let values = serde_json::from_value::<FreeFormMap>(json!(r#"{"svc|op": 0.75}"#))
            .expect("JSON-encoded map deserializes")
            .0;

        assert_eq!(values["svc|op"], json!(0.75));
        // The parsed form arrives from YAML-sourced settings and must keep working.
        let parsed = serde_json::from_value::<FreeFormMap>(json!({ "svc|op": 0.75 }))
            .expect("parsed map deserializes")
            .0;
        assert_eq!(parsed["svc|op"], json!(0.75));
        for rejected in [json!("not json"), json!(r#"["a"]"#), json!(5)] {
            assert!(
                serde_json::from_value::<FreeFormMap>(rejected.clone()).is_err(),
                "{rejected}"
            );
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

    #[test]
    fn string_list_follows_the_go_slice_cast() {
        for (raw, expected) in STRING_LIST_CASES {
            assert_eq!(parse_string_list(raw), *expected, "{raw:?}");
        }
    }
}
