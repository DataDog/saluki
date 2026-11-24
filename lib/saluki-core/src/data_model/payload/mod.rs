//! Output payloads.

use std::fmt;

use bitmask_enum::bitmask;

mod http;
pub use self::http::HttpPayload;
use crate::topology::interconnect::Dispatchable;

mod metadata;
pub use self::metadata::PayloadMetadata;

mod raw;
pub use self::raw::RawPayload;

/// Output payload type.
///
/// This type is a bitmask, which means different payload types can be combined together. This makes `PayloadType` mainly
/// useful for defining the type of output payloads that a component emits, or can handle.
#[bitmask(u8)]
#[bitmask_config(vec_debug)]
pub enum PayloadType {
    /// Raw.
    Raw,

    /// HTTP.
    Http,
}

impl Default for PayloadType {
    fn default() -> Self {
        Self::none()
    }
}

impl fmt::Display for PayloadType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut types = Vec::new();

        if self.contains(Self::Raw) {
            types.push("Raw");
        }

        if self.contains(Self::Http) {
            types.push("HTTP");
        }

        write!(f, "{}", types.join("|"))
    }
}

/// An output payload.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Payload {
    /// A raw payload.
    ///
    /// The payload is an opaque collection of bytes.
    Raw(RawPayload),

    /// An HTTP payload.
    ///
    /// Includes the relevant HTTP parameters (host, path, method, headers) and the payload body.
    Http(HttpPayload),
}

impl Payload {
    /// Gets the type of this payload.
    pub fn payload_type(&self) -> PayloadType {
        match self {
            Payload::Raw(_) => PayloadType::Raw,
            Payload::Http(_) => PayloadType::Http,
        }
    }

    /// Returns the inner payload value, if this event is a `RawPayload`.
    ///
    /// Otherwise, `None` is returned and the original payload is consumed.
    pub fn try_into_raw(self) -> Option<RawPayload> {
        match self {
            Payload::Raw(payload) => Some(payload),
            _ => None,
        }
    }

    /// Returns the inner payload value, if this event is an `HttpPayload`.
    ///
    /// Otherwise, `None` is returned and the original payload is consumed.
    pub fn try_into_http_payload(self) -> Option<HttpPayload> {
        match self {
            Payload::Http(payload) => Some(payload),
            _ => None,
        }
    }
}

impl Dispatchable for Payload {
    fn item_count(&self) -> usize {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::PayloadType;

    #[test]
    fn payload_type_display_renders_each_single_flag() {
        // Note that the rendered name deliberately differs from the variant identifier for HTTP.
        assert_eq!(PayloadType::Raw.to_string(), "Raw");
        assert_eq!(PayloadType::Http.to_string(), "HTTP");
    }

    #[test]
    fn payload_type_display_joins_combined_flags_in_declaration_order() {
        // Combined flags render in the fixed declaration order joined by `|`, regardless of how they were OR'd together.
        assert_eq!((PayloadType::Http | PayloadType::Raw).to_string(), "Raw|HTTP");
        assert_eq!(PayloadType::all_bits().to_string(), "Raw|HTTP");

        // The empty bitmask (also the `Default`) renders as an empty string.
        assert_eq!(PayloadType::none().to_string(), "");
        assert_eq!(PayloadType::default().to_string(), "");
    }
}
