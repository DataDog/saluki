//! Structured `DogStatsD` service-check generation.

use rand::{Rng, RngExt};

use super::common;

/// Status symbols: OK, warning, critical, unknown.
const STATUS: &[&[u8]] = &[b"0", b"1", b"2", b"3"];

/// Optional-field prefixes: hostname, message.
const OPT_PREFIXES: &[&[u8]] = &[b"h:", b"m:"];

/// Optional-field counts: mostly none, with a boundary tail.
const OPT_COUNTS: &[usize] = &[0, 0, 0, 0, 1, 1, 2, 3, 127, 255];

/// A structured service check: `_sc|name|status[|opt...]`.
#[derive(Clone, Debug)]
pub(crate) struct ServiceCheck {
    /// Name content, required non-empty.
    name: Vec<u8>,
    /// The status symbol, always one of `0` `1` `2` `3`.
    status: &'static [u8],
    /// `key:value` tags.
    tags: Vec<Vec<u8>>,
    /// Optional chunks, each carrying its own prefix.
    options: Vec<Vec<u8>>,
}

impl ServiceCheck {
    /// Sample a service check with content over the full forwarded alphabet.
    pub(crate) fn generate(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        // `_sc|name|status` is the skeleton, with a one-byte status.
        const SKELETON: usize = "_sc|".len() + 1 + 1;
        let name_room = budget.checked_sub(SKELETON)?;
        let name = common::identifier_within(rng, name_room);
        if name.is_empty() {
            return None;
        }
        let spent = SKELETON + name.len();
        let tags = common::tags_within(rng, budget.saturating_sub(spent));
        let spent = spent + common::tags_len(&tags);
        Some(Self {
            name,
            status: STATUS[rng.random_range(0..STATUS.len())],
            tags,
            options: options(rng, budget.saturating_sub(spent)),
        })
    }

    /// Serialize the service check to datagram bytes, without the trailing `\n`.
    pub(crate) fn serialize(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"_sc|");
        out.extend_from_slice(&self.name);
        out.push(b'|');
        out.extend_from_slice(self.status);
        common::serialize_tags(&self.tags, out);
        for opt in &self.options {
            out.push(b'|');
            out.extend_from_slice(opt);
        }
    }
}

/// Sample a run of optional chunks.
fn options(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<Vec<u8>> {
    let count = OPT_COUNTS[rng.random_range(0..OPT_COUNTS.len())];
    let mut out = Vec::new();
    let mut room = budget;
    for _ in 0..count {
        let prefix = OPT_PREFIXES[rng.random_range(0..OPT_PREFIXES.len())];
        // Each chunk carries a leading `|` on the wire.
        let Some(body_room) = room.checked_sub(1 + prefix.len()) else {
            break;
        };
        let mut chunk = prefix.to_vec();
        chunk.extend_from_slice(&common::optional_text_within(rng, body_room));
        room -= 1 + chunk.len();
        out.push(chunk);
    }
    out
}
