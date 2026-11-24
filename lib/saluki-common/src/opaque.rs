//! Opaque payloads: a type name and the encoded bytes of a value of that type.
//!
//! An [`Opaque`] value carries a payload that Saluki's data model doesn't describe, in the same way as protobuf's
//! `Any`. The type name tells a reader how to decode the bytes. A reader that doesn't know the type name forwards the
//! payload or drops it.

use std::fmt;

use bytes::Bytes;
use saluki_error::{ErrorContext as _, GenericError};
use stringtheory::MetaString;

/// A type that can be encoded into, and decoded from, an [`Opaque`] value.
pub trait OpaqueType: Sized {
    /// The fully qualified type name.
    const TYPE_NAME: &'static str;

    /// Encodes this value.
    fn encode(&self) -> Bytes;

    /// Decodes a value from the given data.
    ///
    /// # Errors
    ///
    /// If the data isn't a valid encoding of this type, an error is returned.
    fn try_decode(data: Bytes) -> Result<Self, GenericError>;
}

/// An opaque payload.
///
/// Opaque payloads provide an escape hatch to carry arbitrarily shaped, telemetry-adjacent data across a topology. They
/// follow the common "`Any`" pattern from Protocol Buffers, where an encoded value is carried as raw bytes alongside
/// the qualified name of the data type, which allows callers to do lightweight filtering of events by type name before
/// attempting to fallibly decode the raw bytes into the concrete type identified by the type name field.
#[derive(Clone, PartialEq)]
pub struct Opaque {
    type_name: MetaString,
    data: Bytes,
}

impl Opaque {
    /// Creates a new `Opaque` from the given type name and encoded data.
    pub fn new(type_name: impl Into<MetaString>, data: Bytes) -> Self {
        Self {
            type_name: type_name.into(),
            data,
        }
    }

    /// Creates a new `Opaque` by encoding the given value.
    pub fn encode<T: OpaqueType>(value: &T) -> Self {
        Self {
            type_name: MetaString::from_static(T::TYPE_NAME),
            data: value.encode(),
        }
    }

    /// Returns the type name.
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Returns the encoded data.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// Returns `true` if the type name is the type name of `T`.
    pub fn is<T: OpaqueType>(&self) -> bool {
        self.type_name == T::TYPE_NAME
    }

    /// Decodes the data as a value of type `T`, if the type name is the type name of `T`.
    ///
    /// If the type name isn't the type name of `T`, `Ok(None)` is returned.
    ///
    /// # Errors
    ///
    /// If the type name matches, but the data isn't a valid encoding of `T`, an error is returned.
    pub fn try_decode<T: OpaqueType>(&self) -> Result<Option<T>, GenericError> {
        if !self.is::<T>() {
            return Ok(None);
        }

        T::try_decode(self.data.clone())
            .with_error_context(|| format!("Failed to decode opaque payload of type `{}`.", T::TYPE_NAME))
            .map(Some)
    }
}

impl fmt::Debug for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The data can be large, so we show only its length.
        f.debug_struct("Opaque")
            .field("type_name", &self.type_name)
            .field("data_len", &self.data.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use saluki_error::generic_error;

    use super::*;

    #[derive(Debug, PartialEq)]
    struct Be32(u32);

    impl OpaqueType for Be32 {
        const TYPE_NAME: &'static str = "saluki.test.Be32";

        fn encode(&self) -> Bytes {
            Bytes::copy_from_slice(&self.0.to_be_bytes())
        }

        fn try_decode(data: Bytes) -> Result<Self, GenericError> {
            let raw: [u8; 4] = data[..]
                .try_into()
                .map_err(|_| generic_error!("Expected 4 bytes, got {}.", data.len()))?;
            Ok(Self(u32::from_be_bytes(raw)))
        }
    }

    struct Case {
        name: &'static str,
        opaque: Opaque,
        is_be32: bool,
        /// The expected decode result, or the expected error message.
        expected: Result<Option<Be32>, &'static str>,
    }

    #[test]
    fn try_decode_checks_type_name_then_decodes() {
        let cases = [
            Case {
                name: "encoded",
                opaque: Opaque::encode(&Be32(42)),
                is_be32: true,
                expected: Ok(Some(Be32(42))),
            },
            Case {
                name: "built from parts",
                opaque: Opaque::new(Be32::TYPE_NAME, Bytes::from_static(&[0, 0, 1, 0])),
                is_be32: true,
                expected: Ok(Some(Be32(256))),
            },
            Case {
                name: "other type name",
                opaque: Opaque::new("saluki.test.Other", Bytes::from_static(&[0, 0, 0, 1])),
                is_be32: false,
                expected: Ok(None),
            },
            Case {
                name: "invalid data",
                opaque: Opaque::new(Be32::TYPE_NAME, Bytes::from_static(&[1, 2])),
                is_be32: true,
                expected: Err("Failed to decode opaque payload of type `saluki.test.Be32`."),
            },
        ];

        for Case {
            name,
            opaque,
            is_be32,
            expected,
        } in cases
        {
            assert_eq!(opaque.is::<Be32>(), is_be32, "{name}");

            match (opaque.try_decode::<Be32>(), expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{name}"),
                (Err(e), Err(expected)) => assert_eq!(e.to_string(), expected, "{name}"),
                (actual, expected) => panic!("{name}: expected {expected:?}, got {actual:?}"),
            }
        }
    }
}
