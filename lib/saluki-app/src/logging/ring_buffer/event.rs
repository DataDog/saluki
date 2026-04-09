use std::fmt::Write as _;

use saluki_common::time::get_unix_timestamp_nanos;
use tracing::{Event, Metadata};

use super::codec::{decode_varint, write_length_prefixed};

/// A log event captured from the tracing layer, ready for encoding into the ring buffer.
///
/// This is the write-path representation that travels through the thingbuf channel from the writer
/// thread to the processor thread. It stores a reference to the event's static callsite
/// [`Metadata`] rather than copying individual metadata fields, making hydration cheaper (one
/// pointer store vs four field copies) and the struct smaller.
#[derive(Clone, Default)]
pub(super) struct CondensedEvent {
    /// Unix timestamp when the event was logged, in nanoseconds.
    pub(super) timestamp_nanos: u128,

    /// Static callsite metadata (level, target, file, line). Set by the hydrator; `None` only in
    /// the default-initialized thingbuf arena slots before first use.
    pub(super) metadata: Option<&'static Metadata<'static>>,

    /// The main log message.
    pub(super) message: String,

    /// Additional structured fields, stored as sequential varint-length-prefixed key-value pairs.
    ///
    /// Each field is encoded as: `varint(key_len) key_bytes varint(value_len) value_bytes`.
    pub(super) fields: Vec<u8>,
}

/// A decoded log event reconstructed from a compressed segment.
///
/// This is the read-path representation returned by [`CompressedSegmentReader::next`]. Since
/// decoded events do not originate from a tracing callsite, they carry individual metadata fields
/// rather than a [`Metadata`] pointer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecodedEvent<'a> {
    /// Unix timestamp when the event was logged, in nanoseconds.
    pub timestamp_nanos: u128,

    /// Log level (e.g., "INFO", "WARN", "ERROR").
    pub level: &'a str,

    /// Target module or component that generated the event.
    pub target: &'a str,

    /// The main log message.
    pub message: String,

    /// Additional structured fields, stored as sequential varint-length-prefixed key-value pairs.
    ///
    /// Each field is encoded as: `varint(key_len) key_bytes varint(value_len) value_bytes`.
    pub fields: Vec<u8>,

    /// Source file where the event was logged.
    pub file: Option<&'a str>,

    /// Line number where the event was logged.
    pub line: Option<u32>,
}

impl<'a> DecodedEvent<'a> {
    /// Returns the number of bytes that this event occupies in memory.
    #[allow(dead_code)] // Useful for diagnostics; exercised in tests.
    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.message.capacity() + self.fields.capacity()
    }

    /// Returns an iterator over the decoded field key-value pairs.
    #[allow(dead_code)] // Will be used by the read API; exercised in tests.
    pub fn iter_fields(&self) -> FieldIter<'_> {
        FieldIter {
            buf: &self.fields,
            idx: 0,
        }
    }
}

/// Iterator over varint-length-prefixed field key-value pairs.
#[allow(dead_code)] // Will be used by the read API; exercised in tests.
pub struct FieldIter<'a> {
    pub(super) buf: &'a [u8],
    pub(super) idx: usize,
}

impl<'a> FieldIter<'a> {
    pub const fn from_buf(buf: &'a [u8]) -> Self {
        Self { buf, idx: 0 }
    }
}

impl<'a> Iterator for FieldIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        // Read key.
        let (key_len, consumed) = decode_varint(self.buf, self.idx)?;
        self.idx += consumed;
        if self.idx + key_len > self.buf.len() {
            return None;
        }
        let key = std::str::from_utf8(&self.buf[self.idx..self.idx + key_len]).ok()?;
        self.idx += key_len;

        // Read value.
        let (val_len, consumed) = decode_varint(self.buf, self.idx)?;
        self.idx += consumed;
        if self.idx + val_len > self.buf.len() {
            return None;
        }
        let val = std::str::from_utf8(&self.buf[self.idx..self.idx + val_len]).ok()?;
        self.idx += val_len;

        Some((key, val))
    }
}

/// Wrapper that pairs a [`CondensedEvent`] with a reusable scratch buffer for `Visit` field
/// formatting.
pub(super) struct EventHydrator<'a> {
    event: &'a mut CondensedEvent,
    value_buf: &'a mut String,
}

impl<'a> EventHydrator<'a> {
    pub(super) fn hydrate(event: &'a mut CondensedEvent, value_buf: &'a mut String, tracing_event: &Event<'_>) {
        event.message.clear();
        event.fields.clear();

        event.timestamp_nanos = get_unix_timestamp_nanos();
        event.metadata = Some(tracing_event.metadata());

        let mut hydrator = EventHydrator { event, value_buf };
        tracing_event.record(&mut hydrator);
    }
}

impl tracing::field::Visit for EventHydrator<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.event.message, "{:?}", value);
        } else {
            write_length_prefixed(&mut self.event.fields, field.name().as_bytes());

            self.value_buf.clear();
            let _ = write!(self.value_buf, "{:?}", value);
            write_length_prefixed(&mut self.event.fields, self.value_buf.as_bytes());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.event.message.push_str(value);
        } else {
            write_length_prefixed(&mut self.event.fields, field.name().as_bytes());
            write_length_prefixed(&mut self.event.fields, value.as_bytes());
        }
    }
}
