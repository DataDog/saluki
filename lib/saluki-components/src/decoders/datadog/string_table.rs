//! Decode-time string table for the v1.0 APM trace wire format.

use stringtheory::MetaString;

use super::error::DecodeError;
use super::read::ResolvedBytes;

/// A growing table of strings referenced by index within a single tracer payload.
///
/// The v1.0 wire format deduplicates strings: the first time a string appears it is serialized as a
/// literal and appended here; later occurrences are `uint32` indices into this table. Index 0 is
/// always the empty string.
///
/// This is a decode-time-only structure. String references are resolved to owned [`MetaString`]
/// values as the payload is decoded, so the table is not retained in the decoded output. Each resolution is charged
/// to the payload's [`ResolvedBytes`] budget.
pub struct StringTable {
    strings: Vec<MetaString>,
    resolved: ResolvedBytes,
}

impl StringTable {
    /// Creates a new string table, seeded with the empty string at index 0, for a payload of `payload_len` bytes.
    pub fn new(payload_len: usize) -> Self {
        Self {
            strings: vec![MetaString::empty()],
            resolved: ResolvedBytes::for_payload(payload_len),
        }
    }

    /// Appends a string to the table and returns its index.
    ///
    /// Unlike the general-purpose reference table, this performs no deduplication: on the read path
    /// each literal is known to be new (repeated strings arrive as indices, not literals).
    pub fn add(&mut self, s: impl Into<MetaString>) -> u32 {
        self.strings.push(s.into());
        (self.strings.len() - 1) as u32
    }

    /// Resolves an index to its string.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::UnseenStringIndex`] if the index is out of range, or
    /// [`DecodeError::ResolvedStringsTooLarge`] if the payload's references have resolved to too many bytes.
    pub fn get(&self, index: u32) -> Result<MetaString, DecodeError> {
        let value = self.strings.get(index as usize).ok_or(DecodeError::UnseenStringIndex {
            index,
            len: self.strings.len(),
        })?;
        self.resolved.charge(value.len())?;
        Ok(value.clone())
    }

    /// Returns the number of strings currently in the table.
    pub fn len(&self) -> usize {
        self.strings.len()
    }
}
