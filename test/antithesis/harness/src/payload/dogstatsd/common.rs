//! Shared `DogStatsD` content sampling: alphabets, field and value builders, tags.
//!
//! There is no clean/feral split. Every content field ranges over the full forwarded byte alphabet:
//! ASCII words, exotic and non-UTF-8 bytes, AND the message delimiters `:` `|` `,` `#` `@`. Including
//! the delimiters is deliberate. It reaches the context-dependent forwarded cases the Agent keeps,
//! such as a `:` inside a set value or a `:` in a tag value, and the ones it drops. The generator does
//! not decide which is which. It serializes and lets [`crate::dogstatsd::is_malformed`] sort and
//! repair.

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

/// Aberrant identifier segments: whitespace, NUL, ill-formed and non-conforming UTF-8, and exotic
/// Unicode. Omits `\n` and `\r`, which are datagram framing rather than content.
const ABERRANT_WORD: &[&[u8]] = &[
    b" ",
    b"\t",
    b"\0",
    b"\x80",                // lone continuation byte
    b"\xc3",                // truncated two-byte lead
    b"\xed\xa0\x80",        // UTF-16 surrogate, ill-formed UTF-8
    b"\xc0\x80",            // overlong NUL
    b"\xff\xfe",            // non-character bytes
    "café".as_bytes(),      // non-conforming but valid UTF-8
    "Ωμέγα".as_bytes(),     // Greek
    "日本語".as_bytes(),    // CJK
    "🦆".as_bytes(),        // emoji, non-ASCII multi-byte
    "a\u{0301}".as_bytes(), // combining acute accent
    "\u{200d}".as_bytes(),  // zero-width joiner
    "\u{202e}".as_bytes(),  // right-to-left override
    "\u{feff}".as_bytes(),  // byte-order mark
];

/// Message delimiters, mixed into content so the generator explores delimiter-bearing fields. Most
/// land the message in the drop set and are repaired away. The survivors are the forwarded oddities.
const DELIMITERS: &[&[u8]] = &[b":", b"|", b",", b"#", b"@"];

/// The full content alphabet for identifier-like fields.
const WORD_POOLS: &[&[&[u8]]] = &[COMPLIANT_WORD, ABERRANT_WORD, DELIMITERS];

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

/// Segment counts for a required field: at least one, with a large-boundary tail so huge fields stay
/// in the explored surface.
const COUNTS_REQUIRED: &[usize] = &[1, 1, 2, 2, 3, 3, 4, 5, 6, 127, 255];

/// Segment counts for an optional field: the required body plus zero.
const COUNTS_OPTIONAL: &[usize] = &[0, 1, 1, 2, 2, 3, 3, 4, 5, 6, 127, 255];

/// Pick one item from a static, non-empty pool by index.
fn pick<'a>(rng: &mut (impl Rng + ?Sized), pool: &[&'a [u8]]) -> &'a [u8] {
    pool[rng.random_range(0..pool.len())]
}

/// The shortest item `pools` can yield, which is the floor cost of one more segment.
pub(crate) fn min_item(pools: &[&[&[u8]]]) -> usize {
    pools
        .iter()
        .flat_map(|pool| pool.iter())
        .map(|item| item.len())
        .min()
        .unwrap_or(0)
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

/// A required identifier within `budget`. Empty when the budget cannot hold one segment, which the
/// caller must treat as "no line fits" rather than emitting an invalid name.
pub(crate) fn identifier_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<u8> {
    field_within(rng, WORD_POOLS, COUNTS_REQUIRED, budget)
}

/// An optional free-text field within `budget`.
pub(crate) fn optional_text_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<u8> {
    field_within(rng, WORD_POOLS, COUNTS_OPTIONAL, budget)
}

/// A tag set serialized within `budget` bytes, `|#` and separating commas included. Each tag is built
/// against the room left, and the run stops when the next one cannot fit.
pub(crate) fn tags_within(rng: &mut (impl Rng + ?Sized), budget: usize) -> Vec<Vec<u8>> {
    let count = COUNTS_OPTIONAL[rng.random_range(0..COUNTS_OPTIONAL.len())];
    // `|#` before the first tag, then a comma before each later one.
    let Some(mut room) = budget.checked_sub(2) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = Vec::new();
    for _ in 0..count {
        let separator = usize::from(!out.is_empty());
        let Some(tag_room) = room.checked_sub(separator) else {
            break;
        };
        // A tag is `key:value` with a required key, so it needs a segment plus the colon.
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
    field_within(rng, WORD_POOLS, COUNTS_REQUIRED, budget)
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
