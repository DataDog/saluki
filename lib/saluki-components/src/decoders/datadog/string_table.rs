//! Decode-time string table for the v1.0 APM trace wire format.

use std::sync::Arc;

use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

use super::error::DecodeError;
use super::read::{repair_utf8, ResolvedBytes};

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
    /// Index of each valid UTF-8 string, by its text.
    indexes: FastHashMap<MetaString, u32>,
    /// Index of each string sent with invalid UTF-8, by the bytes sent: two can repair to the same text.
    repaired: FastHashMap<Box<[u8]>, u32>,
    resolved: ResolvedBytes,
}

impl StringTable {
    /// Creates a new string table, seeded with the empty string at index 0, for a payload of `payload_len` bytes.
    pub fn new(payload_len: usize) -> Self {
        let mut indexes = FastHashMap::default();
        indexes.insert(MetaString::empty(), 0);
        Self {
            strings: vec![MetaString::empty()],
            indexes,
            repaired: FastHashMap::default(),
            resolved: ResolvedBytes::for_payload(payload_len),
        }
    }

    /// Adds a string, sent as `raw`, to the table and returns its index.
    ///
    /// As the reference's `StringTable.Add`, which a streaming string goes through, a string the table already holds
    /// keeps its index rather than taking a new one: an encoder sends each string as a literal once, but a payload
    /// that sends one again, or sends the empty string, mustn't shift the indexes of the strings after it. Strings are
    /// told apart by their bytes, as the reference holds them, though invalid UTF-8 is repaired (see
    /// [`repair_utf8`](super::read::repair_utf8)).
    pub fn add(&mut self, raw: &[u8]) -> u32 {
        let existing = match simdutf8::basic::from_utf8(raw) {
            Ok(s) => self.indexes.get(s),
            Err(_) => self.repaired.get(raw),
        };
        match existing {
            Some(index) => *index,
            None => self.add_unchecked(raw),
        }
    }

    /// Appends a string, sent as `raw`, to the table and returns its index, even if the table already holds it.
    ///
    /// As the reference's `StringTable.addUnchecked`, which the string table field goes through: a duplicate takes an
    /// index of its own, and a later [`add`](Self::add) of the same string returns the index added last.
    ///
    /// A string too long to inline gets one shared allocation, so that every reference to it shares it: cloning an
    /// owned [`MetaString`] copies it.
    pub fn add_unchecked(&mut self, raw: &[u8]) -> u32 {
        let index = self.strings.len() as u32;
        let value = match simdutf8::basic::from_utf8(raw) {
            Ok(s) => {
                let value = shared(s);
                self.indexes.insert(value.clone(), index);
                value
            }
            Err(_) => {
                self.repaired.insert(raw.into(), index);
                shared(&repair_utf8(raw))
            }
        };
        self.strings.push(value);
        index
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

    /// Returns the payload's resolved-bytes budget.
    pub fn resolved(&self) -> &ResolvedBytes {
        &self.resolved
    }

    /// Returns the number of strings currently in the table.
    pub fn len(&self) -> usize {
        self.strings.len()
    }
}

/// Returns `s` inlined, or else in one shared allocation.
fn shared(s: &str) -> MetaString {
    MetaString::try_inline(s).unwrap_or_else(|| MetaString::from(Arc::<str>::from(s)))
}
