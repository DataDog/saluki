use saluki_common::collections::FastHashMap;

use super::codec::{encode_varint, write_length_prefixed};

/// A string intern table that maps strings to compact varint indices.
///
/// Used during encoding to replace repeated target/file strings with small integer indices, and during decoding to look
/// up the original strings.
#[derive(Default)]
pub struct StringTable {
    strings: Vec<String>,
    index: FastHashMap<String, usize>,
}

impl StringTable {
    /// Interns a string and returns its index.
    pub fn intern(&mut self, s: &str) -> usize {
        if let Some(&idx) = self.index.get(s) {
            return idx;
        }
        let idx = self.strings.len();
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), idx);
        idx
    }

    /// Serializes the string table as: varint(count) then for each entry varint(len) bytes.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        encode_varint(self.strings.len(), buf);
        for s in &self.strings {
            write_length_prefixed(buf, s.as_bytes());
        }
    }

    pub fn clear(&mut self) {
        self.strings.clear();
        self.index.clear();
    }
}
