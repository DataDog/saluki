//! Deserialization helpers.
//!
//! This module provides various helpers for handling the deserialization of common data types in more flexible and
//! permissive ways. These helpers are designed to be used with the `serde_with` crate.

use std::fmt;

use serde::{
    de::{Error, Unexpected},
    Deserializer,
};
use serde_with::DeserializeAs;

/// Permissively deserializes a boolean.
///
/// This helper module allows deserializing a `bool` from a number of possible data types:
///
/// - `true` or `false` as a native boolean
/// - `1` or `0` as an integer (signed, unsigned, or floating point)
/// - any of the following strings (matching Go's `strconv.ParseBool`):
///   - truthy: `"1"`, `"t"`, `"T"`, `"TRUE"`, `"true"`, `"True"`
///   - falsy: `"0"`, `"f"`, `"F"`, `"FALSE"`, `"false"`, `"False"`
pub struct PermissiveBool;

impl<'de> DeserializeAs<'de, bool> for PermissiveBool {
    fn deserialize_as<D>(deserializer: D) -> Result<bool, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'vde> serde::de::Visitor<'vde> for Visitor {
            type Value = bool;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a boolean, string, integer, or floating-point number")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: Error,
            {
                Ok(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: Error,
            {
                // Accept the same set of strings as Go's strconv.ParseBool.
                match value {
                    "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
                    "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
                    _ => Err(Error::invalid_value(
                        Unexpected::Str(value),
                        &"a boolean string (\"1\", \"t\", \"T\", \"TRUE\", \"true\", \"True\", \"0\", \"f\", \"F\", \"FALSE\", \"false\", \"False\")",
                    )),
                }
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: Error,
            {
                match value {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(Error::invalid_value(Unexpected::Signed(value), &"0 or 1")),
                }
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: Error,
            {
                match value {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(Error::invalid_value(Unexpected::Unsigned(value), &"0 or 1")),
                }
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: Error,
            {
                match value {
                    0.0 => Ok(false),
                    1.0 => Ok(true),
                    _ => Err(Error::invalid_value(Unexpected::Float(value), &"0.0 or 1.0")),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_with::serde_as;

    use super::PermissiveBool;

    #[serde_as]
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde_as(as = "PermissiveBool")]
        value: bool,
    }

    fn parse(s: &str) -> Result<bool, serde_json::Error> {
        let json = format!("{{\"value\": {}}}", s);
        let w: Wrapper = serde_json::from_str(&json)?;
        Ok(w.value)
    }

    fn parse_str(s: &str) -> Result<bool, serde_json::Error> {
        let json = format!("{{\"value\": \"{}\"}}", s);
        let w: Wrapper = serde_json::from_str(&json)?;
        Ok(w.value)
    }

    // Native boolean
    #[test]
    fn native_true() {
        assert_eq!(parse("true").unwrap(), true);
    }

    #[test]
    fn native_false() {
        assert_eq!(parse("false").unwrap(), false);
    }

    // Truthy strings
    #[test]
    fn str_1() {
        assert_eq!(parse_str("1").unwrap(), true);
    }

    #[test]
    fn str_t_lower() {
        assert_eq!(parse_str("t").unwrap(), true);
    }

    #[test]
    fn str_t_upper() {
        assert_eq!(parse_str("T").unwrap(), true);
    }

    #[test]
    fn str_true_all_caps() {
        assert_eq!(parse_str("TRUE").unwrap(), true);
    }

    #[test]
    fn str_true_lower() {
        assert_eq!(parse_str("true").unwrap(), true);
    }

    #[test]
    fn str_true_title() {
        assert_eq!(parse_str("True").unwrap(), true);
    }

    // Falsy strings
    #[test]
    fn str_0() {
        assert_eq!(parse_str("0").unwrap(), false);
    }

    #[test]
    fn str_f_lower() {
        assert_eq!(parse_str("f").unwrap(), false);
    }

    #[test]
    fn str_f_upper() {
        assert_eq!(parse_str("F").unwrap(), false);
    }

    #[test]
    fn str_false_all_caps() {
        assert_eq!(parse_str("FALSE").unwrap(), false);
    }

    #[test]
    fn str_false_lower() {
        assert_eq!(parse_str("false").unwrap(), false);
    }

    #[test]
    fn str_false_title() {
        assert_eq!(parse_str("False").unwrap(), false);
    }

    // Invalid string
    #[test]
    fn str_invalid_rejected() {
        assert!(parse_str("yes").is_err());
        assert!(parse_str("no").is_err());
        assert!(parse_str("2").is_err());
        assert!(parse_str("").is_err());
    }

    // Integer variants
    #[test]
    fn int_1_true() {
        assert_eq!(parse("1").unwrap(), true);
    }

    #[test]
    fn int_0_false() {
        assert_eq!(parse("0").unwrap(), false);
    }
}
