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
/// - `"true"` or `"false"` as a string (case insensitive)
/// - `1` or `0` as an integer (signed, unsigned, or floating point)
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
                match value.to_lowercase().as_str() {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    _ => Err(Error::invalid_value(
                        Unexpected::Str(value),
                        &"\"true\" or \"false\" (case insensitive)",
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

/// Treats empty strings as not being present for optional fields during deserialization.
///
/// This helper module allows deserializing an `Option<String>` as `None` if a value is present but is otherwise empty.
pub struct EmptyStringAsNone;

impl<'de> DeserializeAs<'de, Option<String>> for EmptyStringAsNone {
    fn deserialize_as<D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'vde> serde::de::Visitor<'vde> for Visitor {
            type Value = Option<String>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: Error,
            {
                if value.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(value.to_string()))
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: Error,
            {
                Ok(None)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}
