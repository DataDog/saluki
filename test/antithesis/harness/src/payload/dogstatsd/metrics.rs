//! Structured `DogStatsD` metric generation.

use rand::{Rng, RngExt};

use super::common;

/// The metric types: count, gauge, timing, histogram, set, distribution.
const METRIC_TYPES: &[&[u8]] = &[b"c", b"g", b"ms", b"h", b"s", b"d"];

/// Extension-field counts: mostly none, with a boundary tail.
const EXT_COUNTS: &[usize] = &[0, 0, 0, 0, 1, 1, 2, 3, 127, 255];

/// A structured metric: `name:value[:value...]|type[|ext...]`.
#[derive(Clone, Debug)]
pub(crate) struct Metric {
    /// Metric name content.
    name: Vec<u8>,
    /// One value for a set, else a `:`-packed run of Go-float tokens.
    values: Vec<Vec<u8>>,
    /// The type symbol.
    kind: &'static [u8],
    /// `key:value` tags.
    tags: Vec<Vec<u8>>,
    /// Extension chunks, each carrying its own prefix: `@`, `c:`, `e:`, or `card:`.
    extensions: Vec<Vec<u8>>,
}

impl Metric {
    /// Sample a metric with content over the full forwarded alphabet.
    pub(crate) fn generate(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        let kind = METRIC_TYPES[rng.random_range(0..METRIC_TYPES.len())];
        // `name:value|kind` is the whole of a valid metric, so the name is built against what is left
        // once the rest of that skeleton is reserved.
        let reserved = 1 + common::min_value_token() + 1 + kind.len();
        let name_room = budget.checked_sub(reserved)?;
        let name = common::identifier_within(rng, name_room);
        if name.is_empty() {
            return None;
        }
        let mut spent = name.len() + reserved;
        let values = if kind == b"s" {
            vec![common::opaque_value_within(
                rng,
                budget - spent + common::min_value_token(),
            )]
        } else {
            // The first value is already reserved. Each extra one costs a `:` and its own bytes.
            let mut values = vec![common::float_token_within(
                rng,
                budget - spent + common::min_value_token(),
            )];
            spent = spent - common::min_value_token() + values[0].len();
            for _ in 1..value_count(rng) {
                let room = budget.saturating_sub(spent + 1);
                let value = common::float_token_within(rng, room);
                if value.is_empty() {
                    break;
                }
                spent += 1 + value.len();
                values.push(value);
            }
            values
        };
        if values.iter().any(Vec::is_empty) {
            return None;
        }
        let spent: usize =
            name.len() + 1 + values.iter().map(Vec::len).sum::<usize>() + values.len() - 1 + 1 + kind.len();
        let tags = common::tags_within(rng, budget.saturating_sub(spent));
        let spent = spent + common::tags_len(&tags);
        Some(Self {
            name,
            values,
            kind,
            tags,
            extensions: extensions(rng, budget.saturating_sub(spent)),
        })
    }

    /// Serialize the metric to datagram bytes, without the trailing `\n`.
    pub(crate) fn serialize(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name);
        out.push(b':');
        for (i, value) in self.values.iter().enumerate() {
            if i > 0 {
                out.push(b':');
            }
            out.extend_from_slice(value);
        }
        out.push(b'|');
        out.extend_from_slice(self.kind);
        common::serialize_tags(&self.tags, out);
        for ext in &self.extensions {
            out.push(b'|');
            out.extend_from_slice(ext);
        }
    }

    /// The packed multi-value run length, or zero for a single value or a set.
    pub(crate) fn packed(&self) -> usize {
        if self.kind != b"s" && self.values.len() > 1 {
            self.values.len()
        } else {
            0
        }
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

/// Sample a run of extension chunks.
fn extensions(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<Vec<u8>> {
    let count = EXT_COUNTS[rng.random_range(0..EXT_COUNTS.len())];
    let mut out = Vec::new();
    let mut room = budget;
    for _ in 0..count {
        // Each chunk carries a leading `|`.
        let Some(chunk_room) = room.checked_sub(1) else {
            break;
        };
        let Some(chunk) = ext_chunk(rng, chunk_room) else {
            break;
        };
        room -= 1 + chunk.len();
        out.push(chunk);
    }
    out
}

/// One extension chunk with its prefix. The `@` rate is a parseable token. The origin chunks carry
/// free content the Agent never drops on.
fn ext_chunk(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Vec<u8>> {
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
