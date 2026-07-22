//! The differential load's context model and wire codec.
//!
//! A context is a metric's identity: its name, tagset, and type, with no value.
//! The intake mints a bounded pool of contexts and hands them to drivers over the
//! wire; a driver attaches a value and renders a line. Names and
//! tags are built from character-set alphabets so a context is well-formed by
//! construction: rendering it into a line never trips
//! [`is_malformed_line`](crate::dogstatsd::is_malformed_line). Strangeness lives
//! in the alphabet (exotic UTF-8, non-UTF-8 tag bytes), never in the framing.
//!
//! The wire is length-prefixed binary, not JSON: a tag may be non-UTF-8, which a
//! JSON string cannot carry. A response is a `u32` context count followed by that
//! many encoded contexts; each field is a Pascal string (`u16` length then bytes),
//! all little-endian.

use rand::{Rng, RngExt};

/// Upper bound on a minted name's length in bytes. Each context samples its own
/// length in `1..=NAME_LEN_MAX`.
const NAME_LEN_MAX: usize = 128;

/// Upper bound on the number of tags a minted context carries. Each context
/// samples its own count in `0..=TAGS_MAX`.
const TAGS_MAX: usize = 16;

/// Upper bound on a single tag's length in bytes.
const TAG_LEN_MAX: usize = 64;

/// Bytes a name may not contain: the name/value and field delimiters plus the
/// framing bytes. A name carrying any of these would restructure or split the
/// line.
const NAME_FORBIDDEN: &[u8] = b":|\n\r";

/// Bytes a tag may not contain: the field delimiter, the tag separator, and the
/// framing bytes. Everything else forwards, including `:` `#` `@`.
const TAG_FORBIDDEN: &[u8] = b"|,\n\r";

/// The six `DogStatsD` metric types, the type half of a context's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricType {
    /// `c`
    Count,
    /// `g`
    Gauge,
    /// `h`
    Histogram,
    /// `d`
    Distribution,
    /// `s`
    Set,
    /// `ms`
    Timer,
}

/// The metric types in wire-byte order.
const METRIC_TYPES: [MetricType; 6] = [
    MetricType::Count,
    MetricType::Gauge,
    MetricType::Histogram,
    MetricType::Distribution,
    MetricType::Set,
    MetricType::Timer,
];

impl MetricType {
    /// The `DogStatsD` type token this renders as.
    #[must_use]
    pub fn token(self) -> &'static [u8] {
        match self {
            MetricType::Count => b"c",
            MetricType::Gauge => b"g",
            MetricType::Histogram => b"h",
            MetricType::Distribution => b"d",
            MetricType::Set => b"s",
            MetricType::Timer => b"ms",
        }
    }

    /// The wire byte for this type: its index in [`METRIC_TYPES`].
    fn to_byte(self) -> u8 {
        match self {
            MetricType::Count => 0,
            MetricType::Gauge => 1,
            MetricType::Histogram => 2,
            MetricType::Distribution => 3,
            MetricType::Set => 4,
            MetricType::Timer => 5,
        }
    }

    /// The type for a wire byte, or `None` when the byte names no type.
    fn from_byte(b: u8) -> Option<MetricType> {
        METRIC_TYPES.get(b as usize).copied()
    }

    /// Sample a type uniformly.
    fn sample<R: Rng + ?Sized>(rng: &mut R) -> MetricType {
        METRIC_TYPES[rng.random_range(0..METRIC_TYPES.len())]
    }
}

/// The byte alphabet a generated field draws from.
#[derive(Clone, Copy)]
enum CharSet {
    /// 7-bit ASCII, control bytes included; the Agent forwards them.
    Ascii,
    /// Valid UTF-8, including multi-byte scalar values.
    Utf8,
    /// Arbitrary bytes, which need not form valid UTF-8.
    NonUtf8,
}

/// The alphabets a name may draw from: ASCII or UTF-8, never non-UTF-8.
const NAME_SETS: &[CharSet] = &[CharSet::Ascii, CharSet::Utf8];

/// The alphabets a tag may draw from: ASCII, UTF-8, or non-UTF-8.
const TAG_SETS: &[CharSet] = &[CharSet::Ascii, CharSet::Utf8, CharSet::NonUtf8];

/// A metric context: the identity a driver renders a value against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// The metric type.
    pub kind: MetricType,
    /// The name bytes. Non-empty, never a routing prefix, never a delimiter.
    pub name: Vec<u8>,
    /// The tag bytes, one entry per tag.
    pub tags: Vec<Vec<u8>>,
}

impl Context {
    /// Mint a fresh, well-formed context from `rng`.
    #[must_use]
    pub fn mint<R: Rng + ?Sized>(rng: &mut R) -> Context {
        let kind = MetricType::sample(rng);

        let name_len = rng.random_range(1..=NAME_LEN_MAX);
        let mut name = gen_field(rng, NAME_SETS, NAME_FORBIDDEN, name_len);
        // A leading `_` risks the `_e{`/`_sc` routing prefixes, which divert a line
        // away from the metric parser. Only an ASCII `_` is byte `0x5f`; a
        // multi-byte first char is always >= 0x80, so this is the whole guard.
        if name.first() == Some(&b'_') {
            name[0] = b'a';
        }

        let tag_count = rng.random_range(0..=TAGS_MAX);
        let tags = (0..tag_count)
            .map(|_| {
                let len = rng.random_range(1..=TAG_LEN_MAX);
                gen_field(rng, TAG_SETS, TAG_FORBIDDEN, len)
            })
            .collect();

        Context { kind, name, tags }
    }

    /// Render `<name>:<value>|<type>[|#<tag>,...][<extensions>]` plus a trailing
    /// `\n` into `out`. The caller supplies a `value` valid for `kind` and pre-built
    /// `extensions` bytes, each a `|`-prefixed chunk and possibly empty. The identity
    /// is well-formed by construction.
    pub fn render_line(&self, value: &[u8], extensions: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name);
        out.push(b':');
        out.extend_from_slice(value);
        out.push(b'|');
        out.extend_from_slice(self.kind.token());
        for (i, tag) in self.tags.iter().enumerate() {
            out.extend_from_slice(if i == 0 { b"|#" } else { b"," });
            out.extend_from_slice(tag);
        }
        out.extend_from_slice(extensions);
        out.push(b'\n');
    }

    /// Append this context's length-prefixed encoding to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.kind.to_byte());
        put_bytes(out, &self.name);
        put_u16(out, self.tags.len());
        for tag in &self.tags {
            put_bytes(out, tag);
        }
    }

    /// Decode one context starting at `*pos`, advancing `*pos` past it. Returns
    /// `None` when the buffer is truncated or names an unknown type.
    fn decode(buf: &[u8], pos: &mut usize) -> Option<Context> {
        let kind = MetricType::from_byte(get_u8(buf, pos)?)?;
        let name = get_bytes(buf, pos)?.to_vec();
        let tag_count = get_u16(buf, pos)?;
        let mut tags = Vec::with_capacity(tag_count);
        for _ in 0..tag_count {
            tags.push(get_bytes(buf, pos)?.to_vec());
        }
        Some(Context { kind, name, tags })
    }
}

/// Encode a `GET /contexts` response body: a `u32` count then each context.
#[must_use]
pub fn encode_response(contexts: &[Context]) -> Vec<u8> {
    let mut out = Vec::new();
    // A response holds the N contexts a driver asked for, far below u32::MAX, so
    // the saturating fallback is unreachable.
    let count = u32::try_from(contexts.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for context in contexts {
        context.encode(&mut out);
    }
    out
}

/// Decode a `GET /contexts` response body. Returns `None` on any truncation or
/// malformed field, so a partial or corrupt body is an error, not a panic.
#[must_use]
pub fn decode_response(buf: &[u8]) -> Option<Vec<Context>> {
    let mut pos = 0;
    let count = get_u32(buf, &mut pos)?;
    // Do not pre-size from the wire count: a hostile count must not pre-allocate.
    // Each iteration consumes bytes, so work is bounded by the buffer length.
    let mut contexts = Vec::new();
    for _ in 0..count {
        contexts.push(Context::decode(buf, &mut pos)?);
    }
    Some(contexts)
}

/// Generate a `target_len`-byte field: pick one of `sets`, then fill from that
/// alphabet, never emitting a `forbidden` byte. UTF-8 may overshoot the target by
/// up to three bytes when a multi-byte char straddles the boundary.
fn gen_field<R: Rng + ?Sized>(rng: &mut R, sets: &[CharSet], forbidden: &[u8], target_len: usize) -> Vec<u8> {
    let set = sets[rng.random_range(0..sets.len())];
    let mut out = Vec::with_capacity(target_len);
    while out.len() < target_len {
        match set {
            CharSet::Ascii => {
                let byte = loop {
                    let byte = rng.random_range(0..=0x7fu8);
                    if !forbidden.contains(&byte) {
                        break byte;
                    }
                };
                out.push(byte);
            }
            CharSet::Utf8 => {
                let c = random_scalar(rng);
                // Only a single-byte (ASCII) scalar can hit a forbidden delimiter;
                // a multi-byte char has no byte below 0x80.
                if (c as u32) < 0x80 && forbidden.contains(&(c as u8)) {
                    continue;
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            CharSet::NonUtf8 => {
                let byte = loop {
                    let byte: u8 = rng.random();
                    if !forbidden.contains(&byte) {
                        break byte;
                    }
                };
                out.push(byte);
            }
        }
    }
    out
}

/// Sample a uniform Unicode scalar value, rejecting the surrogate range so the
/// result is always a valid `char`.
fn random_scalar<R: Rng + ?Sized>(rng: &mut R) -> char {
    loop {
        if let Some(c) = char::from_u32(rng.random_range(0..=0x10_ffffu32)) {
            return c;
        }
    }
}

/// Append a `u16` length prefix then the bytes.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u16(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// Append `len` as a little-endian `u16`, saturating an over-long field to
/// `u16::MAX`. Minted fields are bounded well under this, so saturation never
/// fires in practice.
fn put_u16(out: &mut Vec<u8>, len: usize) {
    let len = u16::try_from(len).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
}

/// Read one byte, advancing `*pos`.
fn get_u8(buf: &[u8], pos: &mut usize) -> Option<u8> {
    let byte = *buf.get(*pos)?;
    *pos += 1;
    Some(byte)
}

/// Read a little-endian `u16` as a `usize`, advancing `*pos`.
fn get_u16(buf: &[u8], pos: &mut usize) -> Option<usize> {
    let end = pos.checked_add(2)?;
    let slice = buf.get(*pos..end)?;
    *pos = end;
    Some(u16::from_le_bytes([slice[0], slice[1]]) as usize)
}

/// Read a little-endian `u32` as a `usize`, advancing `*pos`.
fn get_u32(buf: &[u8], pos: &mut usize) -> Option<usize> {
    let end = pos.checked_add(4)?;
    let slice = buf.get(*pos..end)?;
    *pos = end;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]) as usize)
}

/// Read a `u16`-prefixed byte run, advancing `*pos` past prefix and bytes.
fn get_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = get_u16(buf, pos)?;
    let end = pos.checked_add(len)?;
    let slice = buf.get(*pos..end)?;
    *pos = end;
    Some(slice)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{decode_response, encode_response, Context};
    use crate::dogstatsd::{is_malformed, is_malformed_line};

    proptest! {
        // The load-bearing property: a minted context rendered with a valid value
        // is well-formed by construction, so `is_malformed_line` never fires.
        #[test]
        fn minted_context_renders_well_formed(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            for _ in 0..64 {
                let context = Context::mint(&mut rng);
                let mut line = Vec::new();
                context.render_line(b"1", b"", &mut line);
                let line = line.strip_suffix(b"\n").unwrap_or(&line);
                prop_assert!(!is_malformed_line(line), "line={:?}", String::from_utf8_lossy(line));
            }
        }

        // A payload packed from minted contexts is well-formed end to end, which is
        // the driver's assertion #1.
        #[test]
        fn minted_payload_is_not_malformed(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut payload = Vec::new();
            for _ in 0..16 {
                Context::mint(&mut rng).render_line(b"1", b"", &mut payload);
            }
            prop_assert_eq!(is_malformed(&payload), None);
        }

        // Encode then decode is the identity over a batch of minted contexts.
        #[test]
        fn response_round_trips(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts: Vec<Context> = (0..8).map(|_| Context::mint(&mut rng)).collect();
            let wire = encode_response(&contexts);
            prop_assert_eq!(decode_response(&wire), Some(contexts));
        }
    }

    #[test]
    fn minted_name_never_routes_as_event_or_service_check() {
        let mut rng = SmallRng::seed_from_u64(1);
        for _ in 0..10_000 {
            let context = Context::mint(&mut rng);
            assert!(!context.name.starts_with(b"_e{"));
            assert!(!context.name.starts_with(b"_sc"));
            assert!(!context.name.is_empty());
        }
    }

    #[test]
    fn decode_rejects_truncation() {
        let contexts = vec![Context::mint(&mut SmallRng::seed_from_u64(2))];
        let wire = encode_response(&contexts);
        // Every proper prefix short of the full body decodes to None, never panics.
        for cut in 0..wire.len() {
            assert_eq!(decode_response(&wire[..cut]), None, "prefix {cut} should be rejected");
        }
        assert_eq!(decode_response(&wire), Some(contexts));
    }
}
