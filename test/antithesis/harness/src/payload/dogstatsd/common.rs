//! Shared `DogStatsD` payload sampling: vibe, segment and number builders, tags.

use antithesis_sdk::random::random_choice;
use rand::distr::Distribution;
use rand::{Rng, RngExt};

use crate::rand::Boundary;

/// Clean by-the-book output, or feral.
#[derive(Clone, Copy, Debug)]
pub enum Vibe {
    /// Well-formed.
    Clean,
    /// Aberrant.
    Feral,
}

/// Sample a per-line vibe, evenly.
#[must_use]
pub fn sample_vibe() -> Vibe {
    match random_choice(&[Vibe::Clean, Vibe::Feral]) {
        Some(Vibe::Feral) => Vibe::Feral,
        _ => Vibe::Clean,
    }
}

/// The Agent's name-legal separators, for joining name-like segments.
pub(crate) const NAME_SEPARATORS: &[u8] = b"._- ";

/// Compliant identifier segments: names, hosts, keys, source types.
pub(crate) const COMPLIANT_WORD: &[&[u8]] = &[
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

/// Aberrant identifier segments: empty, whitespace, NUL, embedded delimiters,
/// invalid UTF-8, message-type prefixes.
pub(crate) const ABERRANT_WORD: &[&[u8]] = &[
    b"",
    b" ",
    b"\t",
    b"\0",
    b"a:b",
    b"a|b",
    b"a,b",
    b"#hash",
    b"@at",
    b"_sc",
    b"_e{1,1}",
    b"\x80",
    b"\xc3",
    b"\xed\xa0\x80",
    b"\xc0\x80",
    b"\xff\xfe",
    b"emoji\xf0\x9f\x92\xa9",
];

/// Values that break number parsers, including long encodings and unicode that
/// looks numeric: infinity, fullwidth and Arabic-Indic digits.
pub(crate) const ABERRANT_VALUES: &[&[u8]] = &[
    b"0",
    b"-0",
    b"inf",
    b"-inf",
    b"+inf",
    b"nan",
    b"infinity",
    b"1e999999",
    b"-1e999999",
    b"0x1p4",
    b"1_000",
    b".",
    b"+",
    b"-",
    b"1.",
    b".5",
    b"1:2:3:4:5",
    b"00000000000000000000000000000000000000000000000000000001.5",
    b"3.141592653589793115997963468544185161590576171875000000000000000000000000",
    "\u{221e}".as_bytes(),
    "-\u{221e}".as_bytes(),
    "\u{ff11}\u{ff12}\u{ff13}".as_bytes(),
    "\u{0664}\u{0662}".as_bytes(),
];

/// Unix-timestamp payloads (the `d:` / `T` fields).
pub(crate) const COMPLIANT_TS: &[&[u8]] = &[b"1700000000", b"1", b"1609459200"];

// NOTE `host` is excluded. `DogStatsD` promotes a `host` tag to the metric host
// resource, emitting varying `host` instances plays hell with Pyld17
// host-consistency check.
const COMPLIANT_TAG_KEYS: &[&[u8]] = &[b"env", b"service", b"region", b"version", b"team", b"shard"];
const ABERRANT_TAG_KEYS: &[&[u8]] = &[b"", b" ", b":", b",", b"#", b"\0", b"\x80"];
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
const ABERRANT_TAG_VALUES: &[&[u8]] = &[b"", b",", b"|", b":", b"\xff", b"\xed\xa0\x80", b"a,b"];

/// Compact, or a cursed-but-equivalent padded encoding.
#[derive(Clone, Copy)]
enum Form {
    Compact,
    Expanded,
}

/// Extend `buf` with one item. Clean draws from `compliant`; feral chooses
/// between compliant and aberrant — a choice, never a coin flip.
pub(crate) fn extend_choice(buf: &mut Vec<u8>, vibe: Vibe, compliant: &[&[u8]], aberrant: &[&[u8]]) {
    let pools: &[&[&[u8]]] = match vibe {
        Vibe::Clean => &[compliant],
        Vibe::Feral => &[compliant, aberrant],
    };
    if let Some(&pool) = random_choice(pools) {
        if let Some(&item) = random_choice(pool) {
            buf.extend_from_slice(item);
        }
    }
}

/// Repeated-element counts (segments, tags) for clean payloads: a small body, no boundary cases.
const ELEMENT_COUNTS_CLEAN: &[u8] = &[1, 1, 2, 2, 3, 3, 4, 5, 6];

/// Repeated-element counts for feral payloads: the clean body plus a `0`/large boundary tail.
const ELEMENT_COUNTS_FERAL: &[u8] = &[1, 1, 2, 2, 3, 3, 4, 5, 6, 0, 127, 255];

fn sample_count<R: Rng + ?Sized>(rng: &mut R, vibe: Vibe) -> u8 {
    let counts = match vibe {
        Vibe::Clean => ELEMENT_COUNTS_CLEAN,
        Vibe::Feral => ELEMENT_COUNTS_FERAL,
    };
    counts[rng.random_range(0..counts.len())]
}

/// Sample a count of segments and join them with sampled `separators`. A pool of
/// `N` segments over a count `c` gives `N^c` results.
pub(crate) fn write_segments<R: Rng + ?Sized>(
    rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe, compliant: &[&[u8]], aberrant: &[&[u8]], separators: &[u8],
) {
    let count = sample_count(rng, vibe);
    for i in 0..count {
        if i > 0 {
            if let Some(&sep) = random_choice(separators) {
                buf.push(sep);
            }
        }
        extend_choice(buf, vibe, compliant, aberrant);
    }
}

/// An identifier (name, host, key, source) built from word segments.
pub(crate) fn write_words<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    write_segments(rng, buf, vibe, COMPLIANT_WORD, ABERRANT_WORD, NAME_SEPARATORS);
}

/// Append `|<prefix><item>`, the item chosen for the vibe.
pub(crate) fn write_field(buf: &mut Vec<u8>, vibe: Vibe, prefix: &[u8], compliant: &[&[u8]], aberrant: &[&[u8]]) {
    buf.push(b'|');
    buf.extend_from_slice(prefix);
    extend_choice(buf, vibe, compliant, aberrant);
}

/// A vibe-sampled count of `key:value` tags joined by ','. Feral can sample a
/// count of zero (no tags) or a large boundary count; clean stays in the small
/// body. Clean draws compliant keys and values; feral mixes aberrant ones in,
/// key and value independently.
pub(crate) fn write_tags<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let count = sample_count(rng, vibe);
    for t in 0..count {
        if t == 0 {
            buf.extend_from_slice(b"|#");
        } else {
            buf.push(b',');
        }
        write_segments(rng, buf, vibe, COMPLIANT_TAG_KEYS, ABERRANT_TAG_KEYS, NAME_SEPARATORS);
        buf.push(b':');
        write_segments(
            rng,
            buf,
            vibe,
            COMPLIANT_TAG_VALUES,
            ABERRANT_TAG_VALUES,
            NAME_SEPARATORS,
        );
    }
}

/// Write `digits` to `buf` as-is, or padded with equivalent leading zeros (and
/// trailing zeros when there is a fractional part). Same value, cursed encoding.
pub(crate) fn write_number<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, digits: &[u8]) {
    match random_choice(&[Form::Compact, Form::Expanded]) {
        Some(Form::Expanded) => {
            let (sign, rest) = match digits.first() {
                Some(&(b'-' | b'+')) => (&digits[..1], &digits[1..]),
                _ => (&digits[..0], digits),
            };
            buf.extend_from_slice(sign);
            pad_zeros(rng, buf);
            buf.extend_from_slice(rest);
            let fractional = rest.contains(&b'.') && !rest.iter().any(|&c| c == b'e' || c == b'E');
            if fractional {
                pad_zeros(rng, buf);
            }
        }
        _ => buf.extend_from_slice(digits),
    }
}

/// Append a boundary-sampled run of '0' bytes to `buf`.
fn pad_zeros<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>) {
    let zeros = usize::from(Boundary::<u8>::new().sample(rng));
    buf.resize(buf.len() + zeros, b'0');
}
