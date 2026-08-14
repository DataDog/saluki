//! Metric contexts: the `(kind, name, tags)` identity a driver renders values against.

use rand::{Rng, RngExt};

use super::{get_bytes, get_tags, get_u8, put_bytes, put_tags, put_u8};
use crate::payload::dogstatsd::common;

/// The six metric types.
const METRIC_TYPES: [MetricType; 6] = [
    MetricType::Count,
    MetricType::Gauge,
    MetricType::Timing,
    MetricType::Histogram,
    MetricType::Set,
    MetricType::Distribution,
];

/// Extension-chunk counts per render: mostly none, with a boundary tail.
const EXT_COUNTS: &[usize] = &[0, 0, 0, 0, 1, 1, 2, 3, 127, 255];

/// A metric type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetricType {
    /// Count.
    Count,
    /// Gauge.
    Gauge,
    /// Timing.
    Timing,
    /// Histogram.
    Histogram,
    /// Set.
    Set,
    /// Distribution.
    Distribution,
}

impl MetricType {
    /// The on-wire type symbol.
    fn token(self) -> &'static [u8] {
        match self {
            MetricType::Count => b"c",
            MetricType::Gauge => b"g",
            MetricType::Timing => b"ms",
            MetricType::Histogram => b"h",
            MetricType::Set => b"s",
            MetricType::Distribution => b"d",
        }
    }

    /// A stable codec byte.
    fn to_byte(self) -> u8 {
        match self {
            MetricType::Count => 0,
            MetricType::Gauge => 1,
            MetricType::Timing => 2,
            MetricType::Histogram => 3,
            MetricType::Set => 4,
            MetricType::Distribution => 5,
        }
    }

    /// Decode a codec byte.
    fn from_byte(b: u8) -> Option<MetricType> {
        Some(match b {
            0 => MetricType::Count,
            1 => MetricType::Gauge,
            2 => MetricType::Timing,
            3 => MetricType::Histogram,
            4 => MetricType::Set,
            5 => MetricType::Distribution,
            _ => return None,
        })
    }

    /// Whether this is the set type, whose value the Agent never parses.
    fn is_set(self) -> bool {
        matches!(self, MetricType::Set)
    }
}

/// A metric identity: type, name, and tags. The value and extensions vary per render.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MetricContext {
    /// The metric type.
    pub kind: MetricType,
    /// Name content.
    pub name: Vec<u8>,
    /// `key:value` tags.
    pub tags: Vec<Vec<u8>>,
}

impl MetricContext {
    /// Mint a metric identity that renders within `budget`, or `None` when the budget cannot hold the
    /// smallest one. The name and tags are built against the room the render's own skeleton leaves, so
    /// the identity fits by construction and no probe is needed to find that out.
    pub(crate) fn mint_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        let kind = METRIC_TYPES[rng.random_range(0..METRIC_TYPES.len())];
        // `:value|type` is what every render of this identity must carry beyond the name and tags.
        let reserved = 1 + common::min_value_token() + 1 + kind.token().len();
        let name = common::identifier_within(rng, budget.checked_sub(reserved)?);
        if name.is_empty() {
            return None;
        }
        let tags = common::tags_within(rng, budget - reserved - name.len());
        Some(Self { kind, name, tags })
    }

    /// Bytes every render of this identity must spend: `name:value|type` and the tag set. A render
    /// spends anything past this on extra packed values and extension chunks.
    pub(crate) fn floor(&self) -> usize {
        self.fixed() + common::min_value_token()
    }

    /// The identity's cost without the value placeholder.
    fn fixed(&self) -> usize {
        self.name.len() + 1 + 1 + self.kind.token().len() + common::tags_len(&self.tags)
    }

    /// Render `name:value|type[|#tags][|ext...]` within `budget`, or `None` when the budget cannot
    /// hold the identity. Values and extensions are sampled against the room left, so no byte is built
    /// and then thrown away.
    pub(crate) fn render_within(
        &self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>, budget: usize,
    ) -> Option<usize> {
        let fixed = self.fixed();
        let value_room = budget.checked_sub(fixed)?;
        if value_room < common::min_value_token() {
            return None;
        }
        out.extend_from_slice(&self.name);
        out.push(b':');
        let mut used = 0;
        let packed = if self.kind.is_set() {
            let value = common::opaque_value_within(rng, value_room);
            used += value.len();
            out.extend_from_slice(&value);
            0
        } else {
            let first = common::float_token_within(rng, value_room);
            used += first.len();
            out.extend_from_slice(&first);
            let mut count = 1;
            for _ in 1..value_count(rng) {
                let room = value_room - used;
                let Some(token_room) = room.checked_sub(1) else {
                    break;
                };
                let value = common::float_token_within(rng, token_room);
                if value.is_empty() {
                    break;
                }
                out.push(b':');
                out.extend_from_slice(&value);
                used += 1 + value.len();
                count += 1;
            }
            if count > 1 {
                count
            } else {
                0
            }
        };
        out.push(b'|');
        out.extend_from_slice(self.kind.token());
        common::serialize_tags(&self.tags, out);
        let mut room = budget - (fixed + used);
        let ext_count = EXT_COUNTS[rng.random_range(0..EXT_COUNTS.len())];
        for _ in 0..ext_count {
            let Some(chunk_room) = room.checked_sub(1) else {
                break;
            };
            let Some(chunk) = ext_chunk_within(rng, chunk_room) else {
                break;
            };
            out.push(b'|');
            out.extend_from_slice(&chunk);
            room -= 1 + chunk.len();
        }
        Some(packed)
    }

    /// Render `name:value|type[|#tags][|ext...]` for a fresh value and extensions. Returns the packed
    /// multi-value run length, or zero for a single value or a set.
    pub(crate) fn render(&self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>) -> usize {
        out.extend_from_slice(&self.name);
        out.push(b':');
        let packed = if self.kind.is_set() {
            out.extend_from_slice(&common::opaque_value(rng));
            0
        } else {
            let count = value_count(rng);
            for i in 0..count {
                if i > 0 {
                    out.push(b':');
                }
                out.extend_from_slice(&common::float_token(rng));
            }
            if count > 1 {
                count
            } else {
                0
            }
        };
        out.push(b'|');
        out.extend_from_slice(self.kind.token());
        common::serialize_tags(&self.tags, out);
        let ext_count = EXT_COUNTS[rng.random_range(0..EXT_COUNTS.len())];
        for _ in 0..ext_count {
            out.push(b'|');
            ext_chunk(rng, out);
        }
        packed
    }

    /// Append this context's length-prefixed encoding.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_u8(out, self.kind.to_byte());
        put_bytes(out, &self.name);
        put_tags(out, &self.tags);
    }

    /// Decode one metric context, advancing `*pos`.
    pub(crate) fn decode(buf: &[u8], pos: &mut usize) -> Option<Self> {
        let kind = MetricType::from_byte(get_u8(buf, pos)?)?;
        let name = get_bytes(buf, pos)?.to_vec();
        let tags = get_tags(buf, pos)?;
        Some(Self { kind, name, tags })
    }
}

/// The `:`-packed value run length. Overwhelmingly one, with a short tail.
fn value_count(rng: &mut (impl Rng + ?Sized)) -> usize {
    match rng.random_range(0..800u16) {
        0..792 => 1,
        792..796 => 2,
        796..798 => 3,
        798 => 4,
        _ => 5,
    }
}

/// One extension chunk within `budget`, or `None` when the budget cannot hold the shortest one.
fn ext_chunk_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Vec<u8>> {
    let prefix: &[u8] = match rng.random_range(0..4u8) {
        0 => b"@",
        1 => b"c:",
        2 => b"e:",
        _ => b"card:",
    };
    let body_room = budget.checked_sub(prefix.len())?;
    let body = if prefix == b"@" {
        common::rate_token_within(rng, body_room)?
    } else {
        common::optional_text_within(rng, body_room)
    };
    let mut chunk = prefix.to_vec();
    chunk.extend_from_slice(&body);
    Some(chunk)
}

/// Append one extension chunk (prefix + body). `@` is a parseable rate; the origin chunks carry free
/// content the Agent never drops on.
fn ext_chunk(rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>) {
    let (prefix, body): (&[u8], Vec<u8>) = match rng.random_range(0..4u8) {
        0 => (b"@", common::rate_token(rng)),
        1 => (b"c:", common::optional_text(rng)),
        2 => (b"e:", common::optional_text(rng)),
        _ => (b"card:", common::optional_text(rng)),
    };
    out.extend_from_slice(prefix);
    out.extend_from_slice(&body);
}
