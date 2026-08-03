//! Event contexts: the stable `_e{}` identity a driver renders text and timestamps against.
//!
//! The Agent does not aggregate events, but each event has a natural stable identity — title, tags,
//! and its option fields (aggregation key, host, source, alert type, priority) — distinct from the
//! per-occurrence text and timestamp. The pool recurs the identity; the driver varies text and
//! timestamp each render.

use rand::{Rng, RngExt};

use super::{fresh_timestamp, get_bytes, get_tags, put_bytes, put_tags};
use super::{LEN_DIGITS, TS_DIGITS};
use crate::payload::dogstatsd::common;

/// Identity option prefixes: aggregation key, hostname, source type, alert type, priority. Bad values
/// are logged and defaulted by the Agent, never dropped, so any content forwards.
const OPT_PREFIXES: &[&[u8]] = &[b"k:", b"h:", b"s:", b"t:", b"p:"];

/// Identity option counts: mostly none, a small body.
const OPT_COUNTS: &[usize] = &[0, 0, 1, 1, 2, 3];

/// An event identity: title, tags, and fixed option chunks. Text and timestamp vary per render.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EventContext {
    /// Title content, non-empty.
    pub title: Vec<u8>,
    /// `key:value` tags.
    pub tags: Vec<Vec<u8>>,
    /// Fixed option chunks, each carrying its own prefix.
    pub options: Vec<Vec<u8>>,
}

impl EventContext {
    /// Mint an event identity that renders within `budget`, or `None` when the budget cannot hold the
    /// smallest one. Title, options and tags are built against the room the render's header, timestamp
    /// and separators leave.
    pub(crate) fn mint_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        const RESERVED: usize = "_e{".len() + LEN_DIGITS + 1 + LEN_DIGITS + "}:".len() + 1 + "|d:".len() + TS_DIGITS;
        let title = common::identifier_within(rng, budget.checked_sub(RESERVED)?);
        if title.is_empty() {
            return None;
        }
        let mut room = budget - RESERVED - title.len();
        let count = OPT_COUNTS[rng.random_range(0..OPT_COUNTS.len())];
        let mut options = Vec::new();
        for _ in 0..count {
            let prefix = OPT_PREFIXES[rng.random_range(0..OPT_PREFIXES.len())];
            let Some(body_room) = room.checked_sub(1 + prefix.len()) else {
                break;
            };
            let mut chunk = prefix.to_vec();
            chunk.extend_from_slice(&common::optional_text_within(rng, body_room));
            room -= 1 + chunk.len();
            options.push(chunk);
        }
        Some(Self {
            title,
            tags: common::tags_within(rng, room),
            options,
        })
    }

    /// Render `_e{title_len,text_len}:title|text[|opt...]|d:ts[|#tags]` for a fresh text and
    /// timestamp. The header lengths are the true byte lengths, so the Agent never rejects on a
    /// length mismatch. Returns zero (events carry no packed run).
    /// Bytes every render of this identity must spend: the header, the title, the fixed options, the
    /// timestamp and the tag set. Only the text is variable. Length and timestamp digits are allowed
    /// generously so the floor is never an underestimate.
    pub(crate) fn floor(&self) -> usize {
        const HEADER: usize = "_e{".len() + LEN_DIGITS + 1 + LEN_DIGITS + "}:".len();
        HEADER
            + self.title.len()
            + 1
            + self.options.iter().map(|opt| 1 + opt.len()).sum::<usize>()
            + "|d:".len()
            + TS_DIGITS
            + common::tags_len(&self.tags)
    }

    /// Render within `budget`, or `None` when the budget cannot hold the identity. Only the text is
    /// sampled, and it is sampled against the room left rather than trimmed afterwards.
    pub(crate) fn render_within(
        &self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>, budget: usize,
    ) -> Option<usize> {
        let text_room = budget.checked_sub(self.floor())?;
        let text = common::optional_text_within(rng, text_room);
        let mut itoa = itoa::Buffer::new();
        out.extend_from_slice(b"_e{");
        out.extend_from_slice(itoa.format(self.title.len()).as_bytes());
        out.push(b',');
        out.extend_from_slice(itoa.format(text.len()).as_bytes());
        out.extend_from_slice(b"}:");
        out.extend_from_slice(&self.title);
        out.push(b'|');
        out.extend_from_slice(&text);
        for opt in &self.options {
            out.push(b'|');
            out.extend_from_slice(opt);
        }
        out.extend_from_slice(b"|d:");
        out.extend_from_slice(itoa.format(fresh_timestamp(rng)).as_bytes());
        common::serialize_tags(&self.tags, out);
        Some(0)
    }

    pub(crate) fn render(&self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>) -> usize {
        let text = common::optional_text(rng);
        let mut itoa = itoa::Buffer::new();
        out.extend_from_slice(b"_e{");
        out.extend_from_slice(itoa.format(self.title.len()).as_bytes());
        out.push(b',');
        out.extend_from_slice(itoa.format(text.len()).as_bytes());
        out.extend_from_slice(b"}:");
        out.extend_from_slice(&self.title);
        out.push(b'|');
        out.extend_from_slice(&text);
        for opt in &self.options {
            out.push(b'|');
            out.extend_from_slice(opt);
        }
        out.extend_from_slice(b"|d:");
        out.extend_from_slice(itoa.format(fresh_timestamp(rng)).as_bytes());
        common::serialize_tags(&self.tags, out);
        0
    }

    /// Append this context's length-prefixed encoding.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.title);
        put_tags(out, &self.tags);
        put_tags(out, &self.options);
    }

    /// Decode one event context, advancing `*pos`.
    pub(crate) fn decode(buf: &[u8], pos: &mut usize) -> Option<Self> {
        let title = get_bytes(buf, pos)?.to_vec();
        let tags = get_tags(buf, pos)?;
        let options = get_tags(buf, pos)?;
        Some(Self { title, tags, options })
    }
}
