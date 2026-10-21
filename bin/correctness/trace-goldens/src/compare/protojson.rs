//! Deserialization helpers for the protojson encoding used by the golden files.
//!
//! protojson encodes 64-bit integers as strings and bytes as base64 strings.

use base64::Engine as _;
use serde::{de::Error as _, Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber<T> {
    String(String),
    Number(T),
}

/// Deserializes a `u64` encoded either as a string or as a number.
pub fn u64_str<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    match StringOrNumber::<u64>::deserialize(d)? {
        StringOrNumber::String(s) => s.parse().map_err(D::Error::custom),
        StringOrNumber::Number(n) => Ok(n),
    }
}

/// Deserializes an `i64` encoded either as a string or as a number.
pub fn i64_str<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    match StringOrNumber::<i64>::deserialize(d)? {
        StringOrNumber::String(s) => s.parse().map_err(D::Error::custom),
        StringOrNumber::Number(n) => Ok(n),
    }
}

/// Deserializes an optional `i64` encoded either as a string or as a number.
pub fn opt_i64_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    i64_str(d).map(Some)
}

/// Decodes standard base64.
pub fn decode_base64(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(s)
}

/// Deserializes base64-encoded bytes.
pub fn base64_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    decode_base64(&s).map_err(D::Error::custom)
}

/// Deserializes optional base64-encoded bytes.
pub fn opt_base64_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
    base64_bytes(d).map(Some)
}
