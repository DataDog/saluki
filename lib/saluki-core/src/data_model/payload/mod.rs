//! Output payloads.

mod http;
use std::fmt;

use bitmask_enum::bitmask;

pub use self::http::HttpPayload;

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
pub enum Payload {
    /// A raw payload.
    ///
    /// The payload is an opaque collection of bytes.
    Raw(Vec<u8>),

    /// An HTTP payload.
    ///
    /// Includes the relevant HTTP parameters (host, path, method, headers) and the payload body.
    Http(HttpPayload),
}
