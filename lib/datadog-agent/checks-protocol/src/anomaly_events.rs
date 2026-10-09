//! Anomaly detection events published by the isolated AAD process over FIT.
//!
//! This is a separate application protocol from the Checks records in [`crate::model`],
//! with its own descriptor (`AAD-EVNT`), one record kind, and three fields. AAD is the
//! producer; both ADP and the Agent subscribe to it over the broadcast shared-memory
//! transport, so each subscriber receives every event.
//!
//! The byte layout mirrors the Checks conventions: little-endian scalars with `u32`
//! length-prefixed strings, in fixed field order, with every field always present.

use std::io;

use saluki_fit::ProtocolDescriptor;

use crate::wire::{payload_size, string_size, Reader, Writer};

/// FIT record type ID of one anomaly event.
pub const ANOMALY_EVENT_TYPE_ID: u32 = 1;

/// Protocol descriptor of the anomaly event channel.
pub const ANOMALY_EVENTS_DESCRIPTOR: ProtocolDescriptor = ProtocolDescriptor {
    id: *b"AAD-EVNT",
    version: 1,
    message_types: &[ANOMALY_EVENT_TYPE_ID],
};

/// Largest encoded anomaly event either side accepts.
///
/// The transport allows payloads up to a gigabyte, but an event carries a title and a
/// description; anything past this bound is a producer bug, so both sides reject it
/// early instead of moving it through shared memory.
pub const MAX_EVENT_PAYLOAD: usize = 64 * 1024;

/// One anomaly notification, from AAD to every subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnomalyEvent {
    /// Short human-readable summary, for example `AAD anomaly: system.load.1`.
    pub title: String,
    /// Details of the event, for example the severity transition and anomaly count.
    pub description: String,
    /// Unix timestamp in seconds of the anomaly, not of the publication.
    pub timestamp: u64,
}

impl AnomalyEvent {
    /// Returns the exact encoded payload size, rejecting oversized events.
    pub fn encoded_len(&self) -> io::Result<usize> {
        let size = payload_size(&[string_size(&self.title)?, string_size(&self.description)?, 8])?;
        if size > MAX_EVENT_PAYLOAD {
            return Err(too_large());
        }
        Ok(size)
    }

    /// Encodes this event with the versioned `AAD-EVNT` byte layout.
    pub fn encode_payload(&self) -> io::Result<Vec<u8>> {
        let size = self.encoded_len()?;
        let mut out = Writer::new(size)?;
        out.string(&self.title);
        out.string(&self.description);
        out.u64(self.timestamp);
        Ok(out.finish(size))
    }

    /// Decodes and validates one complete anomaly event payload.
    pub fn decode_payload(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_EVENT_PAYLOAD {
            return Err(too_large());
        }
        let mut input = Reader::new(bytes)?;
        let value = Self {
            title: input.string()?,
            description: input.string()?,
            timestamp: input.u64()?,
        };
        input.finish()?;
        Ok(value)
    }
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "anomaly event exceeds the 64 KiB payload cap",
    )
}
