//! Shared `DogStatsD` content sampling: alphabets, field and value builders, tags.
//!
//! There is no clean/feral split. Every pool here is valid UTF-8, from plain ASCII words to exotic
//! scripts, and every content field also draws the message delimiters `:` `|` `,` `#` `@`. Including the
//! delimiters is deliberate. It reaches the context-dependent forwarded cases the Agent keeps, such as a
//! `:` inside a set value or a `:` in a tag value, and the ones it drops. The generator does not decide
//! which is which. It serializes and lets [`crate::dogstatsd::is_malformed`] sort and repair. Value
//! tokens are the one exception and omit `|`, which closes the value field and would drop every line
//! carrying it.
//!
//! An invalid UTF-8 byte never comes from these pools. It is minted into an identity by
//! [`crate::contexts::Context::mint_non_utf8_within`], which is the only caller of
//! [`invalid_utf8_byte`].

use rand::distr::Distribution;
use rand::{Rng, RngExt};

use crate::rand::Wide;

/// The Agent's name-legal separators, for joining segments.
const NAME_SEPARATORS: &[u8] = b"._- ";

/// Compliant identifier segments: names, hosts, keys, source types.
const COMPLIANT_WORD: &[&[u8]] = &[
    b"adp",
    b"dogstatsd",
    b"requests",
    b"latency",
    b"errors",
    b"count",
    b"total",
    b"bytes",
    b"queue",
    b"workers",
];

/// Aberrant identifier segments: whitespace, NUL, and exotic but valid UTF-8. Invalid-UTF-8 bytes are
/// deliberately absent. [`crate::contexts::Context::mint_non_utf8_within`] puts one into an identity
/// instead, for the fraction of pulls the intake picks, so the blast radius stays a budgeted fraction of
/// datagrams rather than compounding across every minted context into a near-certain whole-payload v3
/// reject. Editing a rendered datagram would invent an identity the pool never issued, which is how
/// bounded cardinality leaks. Omits `\n` and `\r`, which are datagram framing rather than content.
const ABERRANT_WORD: &[&[u8]] = &[
    b" ",
    b"\t",
    b"\0",
    "café".as_bytes(),      // non-ASCII but valid UTF-8
    "Ωμέγα".as_bytes(),     // Greek
    "日本語".as_bytes(),    // CJK
    "🦆".as_bytes(),        // emoji, non-ASCII multi-byte
    "a\u{0301}".as_bytes(), // combining acute accent
    "\u{200d}".as_bytes(),  // zero-width joiner
    "\u{202e}".as_bytes(),  // right-to-left override
    "\u{feff}".as_bytes(),  // byte-order mark
];

/// Bytes no valid UTF-8 sequence can lead with, for poisoning a minted identity. `0x80` is a valid
/// continuation byte, so a poisoned field is verified rather than assumed invalid.
const INVALID_UTF8: &[u8] = &[0x80, 0xC0, 0xC1, 0xF5, 0xFE, 0xFF];

/// One byte that makes any surrounding ASCII field invalid UTF-8.
pub(crate) fn invalid_utf8_byte(rng: &mut (impl Rng + ?Sized)) -> u8 {
    INVALID_UTF8[rng.random_range(0..INVALID_UTF8.len())]
}

/// Message delimiters, mixed into content so the generator explores delimiter-bearing fields. Most
/// land the message in the drop set and are repaired away. The survivors are the forwarded oddities.
const DELIMITERS: &[&[u8]] = &[b":", b"|", b",", b"#", b"@"];

/// The full content alphabet for identifier-like fields.
const WORD_POOLS: &[&[&[u8]]] = &[COMPLIANT_WORD, ABERRANT_WORD, DELIMITERS];

/// Delimiters a value token may carry. `|` is absent: it closes the value field, so the type token
/// after it becomes value content and the Agent drops the line. A long value drawn from a pool holding
/// `|` almost always carries one, which is how a set metric exhausted its repair tries at a wide budget.
/// The byte is not lost to the generator, only to values, and a value bearing it never reached the SUT.
const VALUE_DELIMITERS: &[&[u8]] = &[b":", b",", b"#", b"@"];

/// Pools a value token is built from.
const VALUE_POOLS: &[&[&[u8]]] = &[COMPLIANT_WORD, ABERRANT_WORD, VALUE_DELIMITERS];

/// Tag keys. `host` is excluded. `DogStatsD` promotes a `host` tag to the metric host resource, and
/// varying host instances break the intake host-consistency check.
const COMPLIANT_TAG_KEYS: &[&[u8]] = &[b"env", b"service", b"region", b"version", b"team", b"shard"];

/// Tag values.
const COMPLIANT_TAG_VALUES: &[&[u8]] = &[
    b"prod",
    b"staging",
    b"adp",
    b"us-east-1",
    b"eu-west-1",
    b"1.2.3",
    b"web01",
    b"0",
];

/// The full content alphabet for tag keys.
const TAG_KEY_POOLS: &[&[&[u8]]] = &[COMPLIANT_TAG_KEYS, ABERRANT_WORD, DELIMITERS];

/// The full content alphabet for tag values.
const TAG_VALUE_POOLS: &[&[&[u8]]] = &[COMPLIANT_TAG_VALUES, ABERRANT_WORD, DELIMITERS];

/// Metric values confirmed to parse with Go's `ParseFloat`.
const SPECIAL_VALUE: &[&[u8]] = &[
    b"0",
    b"-0",
    b"inf",
    b"-inf",
    b"+inf",
    b"nan",
    b"infinity",
    b"0x1p4",
    b"1_000",
    b"1.",
    b".5",
    b"00000000000000000000000000000000000000000000000000000001.5",
    b"3.141592653589793115997963468544185161590576171875000000000000000000000000",
];

/// Sample-rate tokens for the `@` field, all Go-`ParseFloat`-able.
const RATE_TOKEN: &[&[u8]] = &[
    b"1", b"0.5", b"0.25", b"0.1", b"0.001", b"+0.5", b"1.", b".5", b"0x1p-1", b"1_000", b"2", b"inf", b"+inf",
    b"-inf", b"nan",
];

/// Segment counts for a required field: at least one, a small body. Field length crosses the intake
/// byte caps through the long-field path below, not through a huge segment count, so a pooled context
/// never compounds into a behemoth.
const COUNTS_REQUIRED: &[usize] = &[1, 1, 2, 2, 3, 3, 4, 5, 6, 7, 8];

/// Segment counts for an optional field: the required body plus zero.
const COUNTS_OPTIONAL: &[usize] = &[0, 1, 1, 2, 2, 3, 3, 4, 5, 6, 7, 8];

/// Tag counts. Reaches past `MaxTags` (100) so the tag-count cap is exercised, but bounded so a
/// context stays a few KiB even at the high end.
const TAG_COUNTS: &[usize] = &[0, 1, 1, 2, 2, 3, 3, 4, 5, 6, 16, 64, 101, 127];

/// Byte-length targets for a long field, crossing `MaxTagLength` (200) and the metric-name cap (350)
/// directly rather than via a huge segment count.
const LONG_TARGETS: &[usize] = &[128, 199, 200, 201, 256, 300, 349, 350, 351, 400];

/// Pick one item from a static, non-empty pool by index.
fn pick<'a>(rng: &mut (impl Rng + ?Sized), pool: &[&'a [u8]]) -> &'a [u8] {
    pool[rng.random_range(0..pool.len())]
}

/// Pick one item from the union of static, non-empty pools by index.
fn pick_union<'a>(rng: &mut (impl Rng + ?Sized), pools: &[&[&'a [u8]]]) -> &'a [u8] {
    let pool = pools[rng.random_range(0..pools.len())];
    pick(rng, pool)
}

/// Pick an item no longer than `budget`, or `None` when the pools hold nothing that small. Counts the
/// affordable items and walks to the chosen one, so a segment that would not fit is never built.
fn pick_within<'a>(rng: &mut (impl Rng + ?Sized), pools: &[&[&'a [u8]]], budget: usize) -> Option<&'a [u8]> {
    let affordable = pools
        .iter()
        .flat_map(|pool| pool.iter())
        .filter(|item| item.len() <= budget)
        .count();
    if affordable == 0 {
        return None;
    }
    let nth = rng.random_range(0..affordable);
    pools
        .iter()
        .flat_map(|pool| pool.iter())
        .filter(|item| item.len() <= budget)
        .nth(nth)
        .copied()
}

/// Build a field of separator-joined segments within `budget` bytes. The segment count comes from
/// `counts`, each segment is drawn from the items that still fit, and the run stops once nothing fits.
/// Sampling against the room left is what keeps the generator from building a field it would have to
/// throw away.
fn field_within(rng: &mut (impl Rng + ?Sized), pools: &[&[&[u8]]], counts: &[usize], budget: usize) -> Vec<u8> {
    // One in eight: fill to a byte target that crosses the intake length caps, taking the largest
    // target the budget affords. Without this an identity built against a budget never reaches the
    // 350-byte name or 200-byte tag boundary, since the segment counts alone do not get there.
    if rng.random_range(0..8u8) == 0 {
        let affordable = LONG_TARGETS.iter().filter(|&&target| target <= budget).count();
        if affordable > 0 {
            let target = LONG_TARGETS[rng.random_range(0..affordable)];
            let mut out = Vec::new();
            while out.len() < target {
                // Separated like the segment run below. Concatenating raw pool items would let a `|`
                // and a `#` land adjacent, which opens a second tags field and swaps the identity's
                // tag set for occurrence content.
                let separator = usize::from(!out.is_empty());
                let Some(item) = pick_within(rng, pools, (target - out.len()).saturating_sub(separator)) else {
                    break;
                };
                if !out.is_empty() {
                    out.push(NAME_SEPARATORS[rng.random_range(0..NAME_SEPARATORS.len())]);
                }
                out.extend_from_slice(item);
            }
            return out;
        }
    }
    let count = counts[rng.random_range(0..counts.len())];
    let mut out = Vec::new();
    for i in 0..count {
        let separator = usize::from(i > 0);
        let room = budget.saturating_sub(out.len() + separator);
        let Some(item) = pick_within(rng, pools, room) else {
            break;
        };
        if i > 0 {
            out.push(NAME_SEPARATORS[rng.random_range(0..NAME_SEPARATORS.len())]);
        }
        out.extend_from_slice(item);
    }
    out
}

/// Build a field of separator-joined segments drawn from `pools`, with the segment count sampled from
/// `counts`. A `counts` slice containing `0` can yield an empty field. Used to mint an identity, which
/// carries no budget: the budget applies when a context is rendered, not when it is minted.
fn field(rng: &mut (impl Rng + ?Sized), pools: &[&[&[u8]]], counts: &[usize]) -> Vec<u8> {
    // One in eight: a long field filled to a byte target that crosses the intake length caps. This
    // reaches the same length surface as a huge segment count without the behemoth a pooled context
    // cannot afford.
    if rng.random_range(0..8u8) == 0 {
        let target = LONG_TARGETS[rng.random_range(0..LONG_TARGETS.len())];
        let mut out = Vec::new();
        while out.len() < target {
            if !out.is_empty() {
                out.push(NAME_SEPARATORS[rng.random_range(0..NAME_SEPARATORS.len())]);
            }
            out.extend_from_slice(pick_union(rng, pools));
        }
        return out;
    }
    let count = counts[rng.random_range(0..counts.len())];
    let mut out = Vec::new();
    for i in 0..count {
        if i > 0 {
            out.push(NAME_SEPARATORS[rng.random_range(0..NAME_SEPARATORS.len())]);
        }
        out.extend_from_slice(pick_union(rng, pools));
    }
    out
}

/// The shortest item `pools` can yield, the floor cost of one more segment.
fn min_item(pools: &[&[&[u8]]]) -> usize {
    pools
        .iter()
        .flat_map(|pool| pool.iter())
        .map(|item| item.len())
        .min()
        .unwrap_or(0)
}

/// A required identifier within `budget`. Empty when the budget cannot hold one segment, which the
/// caller treats as "no identity fits" rather than minting an invalid name.
pub(crate) fn identifier_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<u8> {
    field_within(rng, WORD_POOLS, COUNTS_REQUIRED, budget)
}

/// A tag set serialized within `budget` bytes, `|#` and separating commas included. Each tag is built
/// against the room left and the run stops when the next one cannot fit.
pub(crate) fn tags_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<Vec<u8>> {
    let count = TAG_COUNTS[rng.random_range(0..TAG_COUNTS.len())];
    let Some(mut room) = budget.checked_sub(2) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = Vec::new();
    for _ in 0..count {
        let separator = usize::from(!out.is_empty());
        let Some(tag_room) = room.checked_sub(separator) else {
            break;
        };
        if tag_room < min_item(TAG_KEY_POOLS) + 1 {
            break;
        }
        let key = field_within(rng, TAG_KEY_POOLS, COUNTS_REQUIRED, tag_room - 1);
        if key.is_empty() {
            break;
        }
        let mut tag = key;
        tag.push(b':');
        let value = field_within(rng, TAG_VALUE_POOLS, COUNTS_OPTIONAL, tag_room - tag.len());
        tag.extend_from_slice(&value);
        room -= separator + tag.len();
        out.push(tag);
    }
    out
}

/// An optional free-text field for an identity.
pub(crate) fn optional_text(rng: &mut (impl Rng + ?Sized)) -> Vec<u8> {
    field(rng, WORD_POOLS, COUNTS_OPTIONAL)
}

/// An optional free-text field within `budget`.
pub(crate) fn optional_text_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<u8> {
    field_within(rng, WORD_POOLS, COUNTS_OPTIONAL, budget)
}

/// Serialize a tag set as `|#key:value,key:value`. An empty set appends nothing.
pub(crate) fn serialize_tags(tags: &[Vec<u8>], out: &mut Vec<u8>) {
    for (i, tag) in tags.iter().enumerate() {
        if i == 0 {
            out.extend_from_slice(b"|#");
        } else {
            out.push(b',');
        }
        out.extend_from_slice(tag);
    }
}

/// A metric value token guaranteed to parse with Go's `ParseFloat`: a special literal, or a wide
/// float or integer rendered compactly or in a cursed-but-equivalent zero-padded form.
pub(crate) fn float_token(rng: &mut (impl Rng + ?Sized)) -> Vec<u8> {
    match rng.random_range(0..3u8) {
        0 => pick(rng, SPECIAL_VALUE).to_vec(),
        1 => {
            let v: f64 = Wide.sample(rng);
            let mut ryu = ryu::Buffer::new();
            render_number(rng, ryu.format(v).as_bytes())
        }
        _ => {
            let v: i64 = Wide.sample(rng);
            let mut itoa = itoa::Buffer::new();
            render_number(rng, itoa.format(v).as_bytes())
        }
    }
}

/// A sample-rate token.
pub(crate) fn rate_token(rng: &mut (impl Rng + ?Sized)) -> Vec<u8> {
    pick(rng, RATE_TOKEN).to_vec()
}

/// A set-type value: an opaque required field over the alphabet a value may carry.
pub(crate) fn opaque_value(rng: &mut (impl Rng + ?Sized)) -> Vec<u8> {
    field(rng, VALUE_POOLS, COUNTS_REQUIRED)
}

/// The floor cost of a value token, the shortest `SPECIAL_VALUE` entry.
pub(crate) fn min_value_token() -> usize {
    SPECIAL_VALUE.iter().map(|item| item.len()).min().unwrap_or(1)
}

/// A float value token within `budget`. Falls back to the shortest affordable literal rather than
/// rendering a number that would not fit, and is empty only when nothing fits.
pub(crate) fn float_token_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<u8> {
    let token = float_token(rng);
    if token.len() <= budget {
        return token;
    }
    pick_within(rng, &[SPECIAL_VALUE], budget)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

/// A sample-rate token within `budget`, or `None` when no rate literal fits.
pub(crate) fn rate_token_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Vec<u8>> {
    pick_within(rng, &[RATE_TOKEN], budget).map(<[u8]>::to_vec)
}

/// A set-type value within `budget`.
pub(crate) fn opaque_value_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<u8> {
    field_within(rng, VALUE_POOLS, COUNTS_REQUIRED, budget)
}

/// The serialized length of a tag set, `|#` and separating commas included.
pub(crate) fn tags_len(tags: &[Vec<u8>]) -> usize {
    if tags.is_empty() {
        0
    } else {
        2 + tags.iter().map(Vec::len).sum::<usize>() + tags.len() - 1
    }
}

/// Render `digits` as-is, or padded with equivalent leading zeros, and trailing zeros for a
/// fractional part. Same value, cursed encoding.
fn render_number(rng: &mut (impl Rng + ?Sized), digits: &[u8]) -> Vec<u8> {
    if rng.random_range(0..2u8) == 0 {
        return digits.to_vec();
    }
    let (sign, rest) = match digits.first() {
        Some(&(b'-' | b'+')) => (&digits[..1], &digits[1..]),
        _ => (&digits[..0], digits),
    };
    let mut out = Vec::new();
    out.extend_from_slice(sign);
    pad_zeros(rng, &mut out);
    out.extend_from_slice(rest);
    let fractional = rest.contains(&b'.') && !rest.iter().any(|&c| c == b'e' || c == b'E');
    if fractional {
        pad_zeros(rng, &mut out);
    }
    out
}

/// Append a boundary-sampled run of `0` bytes.
fn pad_zeros(rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>) {
    // A short run of leading/trailing zeros: enough to exercise padded encodings without bloating.
    let zeros = usize::from(pick_padding_run(rng));
    out.resize(out.len() + zeros, b'0');
}

/// A boundary-biased zero-run length.
fn pick_padding_run(rng: &mut (impl Rng + ?Sized)) -> u8 {
    const RUNS: &[u8] = &[0, 1, 2, 8, 16, 32, 64, 127];
    RUNS[rng.random_range(0..RUNS.len())]
}

#[cfg(test)]
mod tests {
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{identifier_within, tags_within};

    // A field built against a budget must still reach the intake's length boundaries, 350 bytes for a
    // metric name and 200 for a tag. The segment counts stop well short of both, so the long-target arm
    // is what gets there, and giving the builder a budget must not cost that coverage.
    #[test]
    fn budgeted_fields_reach_the_length_boundaries() {
        let mut max_name = 0;
        let mut max_tag = 0;
        for seed in 0..512u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            max_name = max_name.max(identifier_within(&mut rng, 8_000).len());
            max_tag = max_tag.max(tags_within(&mut rng, 8_000).iter().map(Vec::len).max().unwrap_or(0));
        }
        assert!(
            max_name > 350,
            "longest field {max_name} never crossed the 350-byte name cap"
        );
        assert!(
            max_tag > 200,
            "longest tag {max_tag} never crossed the 200-byte tag cap"
        );
    }
}
