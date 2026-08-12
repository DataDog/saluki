//! Structured `DogStatsD` event generation.

use rand::{Rng, RngExt};

use super::common;

/// Optional-field prefixes: hostname, aggregation key, priority, source type, alert type. Bad
/// priority and alert values are logged and defaulted by the Agent, never dropped, so any content
/// forwards.
const OPT_PREFIXES: &[&[u8]] = &[b"h:", b"k:", b"p:", b"s:", b"t:"];

/// Optional-field counts: mostly none, with a boundary tail.
const OPT_COUNTS: &[usize] = &[0, 0, 0, 0, 1, 1, 2, 3, 127, 255];

/// A structured event: `_e{title_len,text_len}:title|text[|opt...]`.
#[derive(Clone, Debug)]
pub(crate) struct Event {
    /// Title content, required non-empty.
    title: Vec<u8>,
    /// Text content, may be empty.
    text: Vec<u8>,
    /// `key:value` tags.
    tags: Vec<Vec<u8>>,
    /// Optional chunks, each carrying its own prefix.
    options: Vec<Vec<u8>>,
}

impl Event {
    /// Sample an event with content over the full forwarded alphabet. The body is length-delimited,
    /// so title and text carry any bytes including delimiters.
    pub(crate) fn generate(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        // `_e{N,M}:title|text` is the skeleton. The header digits grow with the field lengths, so a
        // conservative four bytes per length keeps the estimate above what serialization will write for
        // any field this generator can build.
        const HEADER: usize = "_e{".len() + 4 + 1 + 4 + "}:".len() + 1;
        let title_room = budget.checked_sub(HEADER)?;
        let title = common::identifier_within(rng, title_room);
        if title.is_empty() {
            return None;
        }
        let spent = HEADER + title.len();
        let text = common::optional_text_within(rng, budget.saturating_sub(spent));
        let spent = spent + text.len();
        let tags = common::tags_within(rng, budget.saturating_sub(spent));
        let spent = spent + common::tags_len(&tags);
        Some(Self {
            title,
            text,
            tags,
            options: options(rng, budget.saturating_sub(spent)),
        })
    }

    /// Serialize the event to datagram bytes, without the trailing `\n`. The header lengths are the
    /// true byte lengths of the title and text, so the Agent never rejects on a length mismatch.
    pub(crate) fn serialize(&self, out: &mut Vec<u8>) {
        let mut itoa = itoa::Buffer::new();
        out.extend_from_slice(b"_e{");
        out.extend_from_slice(itoa.format(self.title.len()).as_bytes());
        out.push(b',');
        out.extend_from_slice(itoa.format(self.text.len()).as_bytes());
        out.extend_from_slice(b"}:");
        out.extend_from_slice(&self.title);
        out.push(b'|');
        out.extend_from_slice(&self.text);
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
