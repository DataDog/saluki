use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};

use super::codec::{decode_varint, encode_varint};

/// Sentinel index meaning "no file recorded for this callsite".
pub const FILE_INDEX_NONE: usize = usize::MAX;

/// The static attributes of a single log callsite.
///
/// A callsite corresponds to one `&'static tracing::Metadata`: its target, source file, line, and
/// level are fixed for the life of the process. Rather than re-encoding those four fields on every
/// event, each distinct callsite is interned once per segment and events store only a compact index
/// into the callsite table. This is a large win in practice because a running component emits many
/// events from the same handful of callsites in bursts.
///
/// `target_idx` and `file_idx` reference the segment's string table; `file_idx` is
/// [`FILE_INDEX_NONE`] when the callsite has no associated file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallsiteEntry {
    pub target_idx: usize,
    pub file_idx: usize,
    pub line: Option<u32>,
    pub level: u8,
}

/// Encoder-side callsite intern table, keyed by `&'static Metadata` pointer identity.
///
/// The pointer value is stable and unique per callsite, so it is a cheap, exact identity key -- no
/// string comparison is needed to recognise a repeated callsite.
#[derive(Default)]
pub struct CallsiteTable {
    entries: Vec<CallsiteEntry>,
    index: FastHashMap<usize, usize>,
}

impl CallsiteTable {
    /// Returns the interned index for a callsite pointer key, if it has been seen this segment.
    pub fn get(&self, key: usize) -> Option<usize> {
        self.index.get(&key).copied()
    }

    /// Interns a new callsite entry under `key` and returns its index.
    ///
    /// The caller is expected to have already checked [`get`](Self::get); this unconditionally
    /// appends a new entry.
    pub fn insert(&mut self, key: usize, entry: CallsiteEntry) -> usize {
        let idx = self.entries.len();
        self.entries.push(entry);
        self.index.insert(key, idx);
        idx
    }

    /// Clears the table for reuse across segments.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }

    /// Serializes the table: `varint(count)` then, per entry,
    /// `varint(target_idx) varint(file_idx+1 | 0) varint(line+1 | 0) varint(level)`.
    ///
    /// `file_idx` and `line` use `0` as a "none" sentinel (stored as value+1) so absent files/lines
    /// cost a single byte rather than a 10-byte `usize::MAX` varint.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        encode_varint(self.entries.len(), buf);
        for e in &self.entries {
            encode_varint(e.target_idx, buf);
            let file_enc = if e.file_idx == FILE_INDEX_NONE {
                0
            } else {
                e.file_idx + 1
            };
            encode_varint(file_enc, buf);
            encode_varint(e.line.map_or(0, |l| l as usize + 1), buf);
            encode_varint(e.level as usize, buf);
        }
    }
}

/// Decodes a callsite table previously written by [`CallsiteTable::encode`], advancing `idx`.
pub fn decode_callsite_table(buf: &[u8], idx: &mut usize) -> Result<Vec<CallsiteEntry>, GenericError> {
    let (count, consumed) =
        decode_varint(buf, *idx).ok_or_else(|| generic_error!("Failed to decode callsite table count"))?;
    *idx += consumed;

    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let (target_idx, consumed) =
            decode_varint(buf, *idx).ok_or_else(|| generic_error!("Failed to decode callsite target index"))?;
        *idx += consumed;
        let (file_enc, consumed) =
            decode_varint(buf, *idx).ok_or_else(|| generic_error!("Failed to decode callsite file index"))?;
        *idx += consumed;
        let (line_enc, consumed) =
            decode_varint(buf, *idx).ok_or_else(|| generic_error!("Failed to decode callsite line"))?;
        *idx += consumed;
        let (level, consumed) =
            decode_varint(buf, *idx).ok_or_else(|| generic_error!("Failed to decode callsite level"))?;
        *idx += consumed;

        entries.push(CallsiteEntry {
            target_idx,
            file_idx: if file_enc == 0 { FILE_INDEX_NONE } else { file_enc - 1 },
            line: if line_enc == 0 {
                None
            } else {
                Some((line_enc - 1) as u32)
            },
            level: level as u8,
        });
    }
    Ok(entries)
}
