//! Service-check contexts: the stable `_sc` identity a driver renders status, message, and timestamp
//! against.
//!
//! The Agent does not aggregate service checks, but each has a natural stable identity — name, tags,
//! and host — distinct from the per-occurrence status, message, and timestamp. The pool recurs the
//! identity; the driver varies status, message, and timestamp each render.

use rand::{Rng, RngExt};

use super::TS_DIGITS;
use super::{fresh_timestamp, get_bytes, get_tags, put_bytes, put_tags};
use crate::payload::dogstatsd::common;

/// The status symbols: OK, warning, critical, unknown.
const STATUS: &[&[u8]] = &[b"0", b"1", b"2", b"3"];

/// Identity option counts: usually none, sometimes a host.
const OPT_COUNTS: &[usize] = &[0, 0, 0, 1];

/// A service-check identity: name, tags, and fixed option chunks (a host). Status, message, and
/// timestamp vary per render.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceCheckContext {
    /// Name content, non-empty.
    pub name: Vec<u8>,
    /// `key:value` tags.
    pub tags: Vec<Vec<u8>>,
    /// Fixed option chunks (a `h:` host), each carrying its own prefix.
    pub options: Vec<Vec<u8>>,
}

impl ServiceCheckContext {
    /// Mint a service-check identity that renders within `budget`, or `None` when the budget cannot
    /// hold the smallest one. Name, options and tags are built against the room the render's skeleton,
    /// timestamp and trailing message leave.
    pub(crate) fn mint_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        const RESERVED: usize = "_sc|".len() + 1 + 1 + "|d:".len() + TS_DIGITS + "|m:".len();
        let name = common::identifier_within(rng, budget.checked_sub(RESERVED)?);
        if name.is_empty() {
            return None;
        }
        let mut room = budget - RESERVED - name.len();
        let count = OPT_COUNTS[rng.random_range(0..OPT_COUNTS.len())];
        let mut options = Vec::new();
        for _ in 0..count {
            let Some(body_room) = room.checked_sub(1 + "h:".len()) else {
                break;
            };
            let mut chunk = b"h:".to_vec();
            chunk.extend_from_slice(&common::optional_text_within(rng, body_room));
            room -= 1 + chunk.len();
            options.push(chunk);
        }
        Some(Self {
            name,
            tags: common::tags_within(rng, room),
            options,
        })
    }

    /// Render `_sc|name|status[|opt...]|d:ts[|#tags]|m:message` for a fresh status, message, and
    /// timestamp. Returns zero (service checks carry no packed run).
    /// Bytes every render of this identity must spend: `_sc|name|status`, the fixed options, the
    /// timestamp, the tag set and the empty `|m:` message. Only the message body is variable.
    pub(crate) fn floor(&self) -> usize {
        "_sc|".len()
            + self.name.len()
            + 1
            + 1
            + self.options.iter().map(|opt| 1 + opt.len()).sum::<usize>()
            + "|d:".len()
            + TS_DIGITS
            + common::tags_len(&self.tags)
            + "|m:".len()
    }

    /// Render within `budget`, or `None` when the budget cannot hold the identity. Only the message is
    /// sampled, against the room left.
    pub(crate) fn render_within(
        &self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>, budget: usize,
    ) -> Option<usize> {
        let message_room = budget.checked_sub(self.floor())?;
        out.extend_from_slice(b"_sc|");
        out.extend_from_slice(&self.name);
        out.push(b'|');
        out.extend_from_slice(STATUS[rng.random_range(0..STATUS.len())]);
        for opt in &self.options {
            out.push(b'|');
            out.extend_from_slice(opt);
        }
        let mut itoa = itoa::Buffer::new();
        out.extend_from_slice(b"|d:");
        out.extend_from_slice(itoa.format(fresh_timestamp(rng)).as_bytes());
        common::serialize_tags(&self.tags, out);
        out.extend_from_slice(b"|m:");
        out.extend_from_slice(&common::optional_text_within(rng, message_room));
        Some(0)
    }

    pub(crate) fn render(&self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>) -> usize {
        out.extend_from_slice(b"_sc|");
        out.extend_from_slice(&self.name);
        out.push(b'|');
        out.extend_from_slice(STATUS[rng.random_range(0..STATUS.len())]);
        for opt in &self.options {
            out.push(b'|');
            out.extend_from_slice(opt);
        }
        let mut itoa = itoa::Buffer::new();
        out.extend_from_slice(b"|d:");
        out.extend_from_slice(itoa.format(fresh_timestamp(rng)).as_bytes());
        common::serialize_tags(&self.tags, out);
        out.extend_from_slice(b"|m:");
        out.extend_from_slice(&common::optional_text(rng));
        0
    }

    /// Append this context's length-prefixed encoding.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.name);
        put_tags(out, &self.tags);
        put_tags(out, &self.options);
    }

    /// Decode one service-check context, advancing `*pos`.
    pub(crate) fn decode(buf: &[u8], pos: &mut usize) -> Option<Self> {
        let name = get_bytes(buf, pos)?.to_vec();
        let tags = get_tags(buf, pos)?;
        let options = get_tags(buf, pos)?;
        Some(Self { name, tags, options })
    }
}
