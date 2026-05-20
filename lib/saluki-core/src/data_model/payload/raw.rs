use std::net::SocketAddr;

use crate::data_model::payload::PayloadMetadata;

/// Source metadata for a raw payload.
#[derive(Clone)]
pub enum RawPayloadSource {
    /// The source is unavailable or unknown.
    Unavailable,

    /// The source is a socket address.
    SocketAddr(SocketAddr),

    /// The source is a Unix process credential tuple.
    UnixProcessCredentials {
        /// Process ID.
        pid: i32,
        /// User ID.
        uid: u32,
        /// Group ID.
        gid: u32,
    },
}

/// An raw payload.
#[derive(Clone)]
pub struct RawPayload {
    metadata: PayloadMetadata,
    source: RawPayloadSource,
    data: Vec<u8>,
}

impl RawPayload {
    /// Creates a new `RawPayload` from the given data.
    pub fn new(metadata: PayloadMetadata, data: Vec<u8>) -> Self {
        Self::with_source(metadata, RawPayloadSource::Unavailable, data)
    }

    /// Creates a new `RawPayload` from the given source and data.
    pub fn with_source(metadata: PayloadMetadata, source: RawPayloadSource, data: Vec<u8>) -> Self {
        RawPayload { metadata, source, data }
    }

    /// Returns the raw payload source.
    pub fn source(&self) -> &RawPayloadSource {
        &self.source
    }

    /// Consumes the raw payload and returns the individual parts.
    pub fn into_inner(self) -> (PayloadMetadata, Vec<u8>) {
        (self.metadata, self.data)
    }

    /// Consumes the raw payload and returns the individual parts, including source metadata.
    pub fn into_parts(self) -> (PayloadMetadata, RawPayloadSource, Vec<u8>) {
        (self.metadata, self.source, self.data)
    }
}
