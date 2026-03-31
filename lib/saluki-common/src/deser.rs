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
/// - `"true"` or `"false"` as a string, case insensitive
/// - `"1"`, `"t"`, or `"T"` as truthy strings; `"0"`, `"f"`, or `"F"` as falsy strings
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
                // Check short forms first, then fall back to case-insensitive "true"/"false".
                match value {
                    "1" | "t" | "T" => Ok(true),
                    "0" | "f" | "F" => Ok(false),
                    _ => match value.to_lowercase().as_str() {
                        "true" => Ok(true),
                        "false" => Ok(false),
                        _ => Err(Error::invalid_value(
                            Unexpected::Str(value),
                            &"a boolean string (\"true\" or \"false\", case insensitive, or short forms: \"1\", \"t\", \"T\", \"0\", \"f\", \"F\")",
                        )),
                    },
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
    use serde::de::{value::StrDeserializer, IntoDeserializer};
    use serde_with::DeserializeAs;

    use super::PermissiveBool;

    fn parse_bool(v: bool) -> Result<bool, serde::de::value::Error> {
        PermissiveBool::deserialize_as(v.into_deserializer())
    }

    fn parse_str(s: &str) -> Result<bool, serde::de::value::Error> {
        let de: StrDeserializer<serde::de::value::Error> = s.into_deserializer();
        PermissiveBool::deserialize_as(de)
    }

    fn parse_int(v: i64) -> Result<bool, serde::de::value::Error> {
        PermissiveBool::deserialize_as(v.into_deserializer())
    }

    // Native boolean
    #[test]
    fn native_true() {
        assert!(parse_bool(true).unwrap());
    }

    #[test]
    fn native_false() {
        assert!(!parse_bool(false).unwrap());
    }

    // String variants
    #[test]
    fn str_truthy() {
        for s in &["1", "t", "T", "true", "True", "tRuE"] {
            assert!(parse_str(s).unwrap(), "expected {s:?} to be truthy");
        }
    }

    #[test]
    fn str_falsy() {
        for s in &["0", "f", "F", "false", "False", "fAlSe"] {
            assert!(!parse_str(s).unwrap(), "expected {s:?} to be falsy");
        }
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
    fn int_true() {
        assert!(parse_int(1).unwrap());
    }

    #[test]
    fn int_false() {
        assert!(!parse_int(0).unwrap());
    }
}
