//! The context protocol: reusable `DogStatsD` identities a driver renders load against.
//!
//! A [`Context`] is a per-type stable identity — a metric's `(kind, name, tags)`, an event's title +
//! tags + option fields, a service check's name + tags + host. It is minted over the content alphabet in
//! `payload/dogstatsd/common.rs` and accepted only when a probe render is `!is_malformed`, see
//! [`crate::dogstatsd`], conforming to the Agent parser and nothing stricter. The driver varies the
//! per-occurrence payload each render, the value, text, status, extensions and timestamp, so a pooled
//! identity recurs while its load varies.
//!
//! [`Context::mint_non_utf8_within`] mints an identity carrying an invalid UTF-8 byte in its name or a
//! tag. That is the only source of such a byte in generated load, and it lives in the identity so the
//! pool counts it against a cap. Poisoning a rendered datagram instead would invent an identity the pool
//! never issued, one per datagram, which is how bounded cardinality leaks.
//!
//! A shared intake pool mints identities up to a per-kind cap then recurs them, and serves them to
//! drivers over the length-prefixed binary codec here ([`encode_response`] / [`decode_response`]),
//! which carries non-UTF-8 names and tags that JSON could not.

use rand::{Rng, RngExt};

use crate::dogstatsd::is_malformed;
use crate::payload::dogstatsd::common;

pub mod event;
pub mod metric;
pub mod service_check;

/// How many times to re-mint an identity whose probe render the Agent would drop before yielding
/// nothing. Mint is mostly-valid, so an exhausted loop is rare.
const REMINT_TRIES: usize = 16;

/// How many times to re-render a context whose per-occurrence payload the Agent would drop before
/// yielding nothing. 2.3% of single renders need a retry and none has yet exhausted the loop.
const RENDER_TRIES: usize = 8;

/// Digits allowed for a rendered length field, generous so a floor is never an underestimate.
pub(crate) const LEN_DIGITS: usize = 5;

/// Digits allowed for a rendered timestamp, likewise generous.
pub(crate) const TS_DIGITS: usize = 20;

/// The three context kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A metric context.
    Metric,
    /// An event context.
    Event,
    /// A service-check context.
    ServiceCheck,
}

impl Kind {
    /// Sample a kind by the message-type weight — 98% metric, 1% event, 1% service check.
    #[must_use]
    pub fn sample(rng: &mut (impl Rng + ?Sized)) -> Kind {
        match rng.random_range(0..100u32) {
            0 => Kind::Event,
            1 => Kind::ServiceCheck,
            _ => Kind::Metric,
        }
    }
}

/// A reusable `DogStatsD` identity of one of the three kinds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Context {
    /// A metric identity.
    Metric(metric::MetricContext),
    /// An event identity.
    Event(event::EventContext),
    /// A service-check identity.
    ServiceCheck(service_check::ServiceCheckContext),
}

impl Context {
    /// Mint a context of `kind` whose renders fit `budget` and that the Agent forwards, or `None` when
    /// no such identity is available.
    ///
    /// The identity is built against the budget, so it fits by construction and nothing is minted then
    /// measured. The remaining re-mint loop is about content alone: the alphabet carries protocol
    /// delimiters and some combinations land on the drop side, which a probe render is what detects.
    /// An exhausted loop yields `None` rather than an identity of another kind, so the caller never
    /// stores a metric in the event or service-check working set.
    #[must_use]
    pub fn mint_within(kind: Kind, rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Context> {
        for _ in 0..REMINT_TRIES {
            let context = match kind {
                Kind::Metric => Context::Metric(metric::MetricContext::mint_within(rng, budget)?),
                Kind::Event => Context::Event(event::EventContext::mint_within(rng, budget)?),
                Kind::ServiceCheck => {
                    Context::ServiceCheck(service_check::ServiceCheckContext::mint_within(rng, budget)?)
                }
            };
            let mut probe = Vec::new();
            context.render(rng, &mut probe);
            if is_malformed(&probe).is_ok() {
                return Some(context);
            }
        }
        None
    }

    /// Render one datagram line (no trailing `\n`) for this identity with a fresh per-occurrence
    /// payload. Returns the packed multi-value run length, or zero.
    pub fn render(&self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>) -> usize {
        match self {
            Context::Metric(c) => c.render(rng, out),
            Context::Event(c) => c.render(rng, out),
            Context::ServiceCheck(c) => c.render(rng, out),
        }
    }

    /// Replace one byte of this identity with an invalid UTF-8 byte, so the identity itself is the
    /// corrupt one rather than a datagram being edited after the fact.
    ///
    /// The Agent does no charset validation, so the line still forwards and the criterion stays "does
    /// the Agent discard it". Which identity field takes the byte is what the intake distinguishes: a
    /// v3 name dictionary rejects the whole payload, a tag dictionary coerces. A delimiter is never
    /// overwritten, since removing one reshapes the line. Returns whether a byte was replaced.
    fn poison(&mut self, rng: &mut (impl Rng + ?Sized)) -> bool {
        let fields: Vec<&mut Vec<u8>> = match self {
            Context::Metric(c) => std::iter::once(&mut c.name).chain(c.tags.iter_mut()).collect(),
            Context::Event(c) => std::iter::once(&mut c.title).chain(c.tags.iter_mut()).collect(),
            Context::ServiceCheck(c) => std::iter::once(&mut c.name).chain(c.tags.iter_mut()).collect(),
        };
        let targets: Vec<(usize, usize)> = fields
            .iter()
            .enumerate()
            .flat_map(|(f, bytes)| {
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, &b)| !matches!(b, b':' | b'|' | b',' | b'#' | b'@'))
                    .map(move |(i, _)| (f, i))
            })
            .collect();
        if targets.is_empty() {
            return false;
        }
        let (field, at) = targets[rng.random_range(0..targets.len())];
        let mut fields = fields;
        fields[field][at] = common::invalid_utf8_byte(rng);
        true
    }

    /// Mint an identity of `kind` that carries an invalid UTF-8 byte, or `None` when none can be built
    /// within `budget`. Corrupt identities live in the pool like any other, so they recur across
    /// datagrams and count against the kind's cap instead of appearing as fresh one-offs.
    #[must_use]
    pub fn mint_non_utf8_within(kind: Kind, rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Context> {
        for _ in 0..REMINT_TRIES {
            let mut context = Context::mint_within(kind, rng, budget)?;
            if !context.poison(rng) {
                continue;
            }
            // A replaced byte does not always invalidate the field. `0x80` is a valid continuation byte,
            // so poisoning the trailing byte of `café` turns `C3 A9` into the valid `C3 80`. Verify the
            // field instead of trimming `0x80` from the pool, which would drop it from the byte space the
            // SUT ever sees. Unverified, 3.4% of corrupt mints carried no invalid byte at all.
            if !context.has_non_utf8() {
                continue;
            }
            let mut probe = Vec::new();
            context.render(rng, &mut probe);
            if is_malformed(&probe).is_ok() {
                return Some(context);
            }
        }
        None
    }

    /// Whether this identity carries an invalid UTF-8 byte.
    #[must_use]
    pub fn has_non_utf8(&self) -> bool {
        let fields: Vec<&[u8]> = match self {
            Context::Metric(c) => std::iter::once(c.name.as_slice())
                .chain(c.tags.iter().map(Vec::as_slice))
                .collect(),
            Context::Event(c) => std::iter::once(c.title.as_slice())
                .chain(c.tags.iter().map(Vec::as_slice))
                .collect(),
            Context::ServiceCheck(c) => std::iter::once(c.name.as_slice())
                .chain(c.tags.iter().map(Vec::as_slice))
                .collect(),
        };
        fields.iter().any(|f| simdutf8::basic::from_utf8(f).is_err())
    }

    /// Bytes every render of this identity must spend, whatever the per-occurrence payload.
    #[must_use]
    pub fn floor(&self) -> usize {
        match self {
            Context::Metric(c) => c.floor(),
            Context::Event(c) => c.floor(),
            Context::ServiceCheck(c) => c.floor(),
        }
    }

    /// Render one line within `budget`, or `None` when the budget cannot hold this identity.
    fn render_within(&self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>, budget: usize) -> Option<usize> {
        match self {
            Context::Metric(c) => c.render_within(rng, out, budget),
            Context::Event(c) => c.render_within(rng, out, budget),
            Context::ServiceCheck(c) => c.render_within(rng, out, budget),
        }
    }

    /// Render one datagram line the Agent forwards within `budget`, or `None` when this context has no
    /// forwardable rendering that fits. The budget is a construction input to each attempt rather than
    /// a filter on the result, and an exhausted attempt count yields nothing rather than a line
    /// carrying an identity the pool never issued.
    pub fn render_wellformed_within(
        &self, rng: &mut (impl Rng + ?Sized), out: &mut Vec<u8>, budget: usize,
    ) -> Option<usize> {
        for try_index in 0..RENDER_TRIES {
            let start = out.len();
            // The last try renders at the identity's floor, where the occurrence is the shortest the
            // identity admits and no extension chunk has room. Sampling a wide occurrence one more time
            // would be hoping again, and a caller has nowhere to go when the tries run out.
            let attempt = if try_index + 1 == RENDER_TRIES {
                self.floor().min(budget)
            } else {
                budget
            };
            let packed = self.render_within(rng, out, attempt)?;
            if is_malformed(&out[start..]).is_ok() {
                return Some(packed);
            }
            out.truncate(start);
        }
        None
    }

    /// Append this context's tagged, length-prefixed encoding.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Context::Metric(c) => {
                put_u8(out, 0);
                c.encode(out);
            }
            Context::Event(c) => {
                put_u8(out, 1);
                c.encode(out);
            }
            Context::ServiceCheck(c) => {
                put_u8(out, 2);
                c.encode(out);
            }
        }
    }

    /// Decode one context, advancing `*pos`. Returns `None` on truncation or an unknown tag.
    fn decode(buf: &[u8], pos: &mut usize) -> Option<Context> {
        Some(match get_u8(buf, pos)? {
            0 => Context::Metric(metric::MetricContext::decode(buf, pos)?),
            1 => Context::Event(event::EventContext::decode(buf, pos)?),
            2 => Context::ServiceCheck(service_check::ServiceCheckContext::decode(buf, pos)?),
            _ => return None,
        })
    }
}

/// Encode a `GET /contexts` response body: a `u32` count then each context.
#[must_use]
pub fn encode_response(contexts: &[Context]) -> Vec<u8> {
    let mut out = Vec::new();
    // A response holds the N contexts a driver asked for, far below u32::MAX.
    let count = u32::try_from(contexts.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for context in contexts {
        context.encode(&mut out);
    }
    out
}

/// Decode a `GET /contexts` response body. Returns `None` on any truncation or malformed field, so a
/// partial or corrupt body is an error, not a panic. Never pre-sizes from the wire count.
#[must_use]
pub fn decode_response(buf: &[u8]) -> Option<Vec<Context>> {
    let mut pos = 0;
    let count = get_u32(buf, &mut pos)?;
    let mut contexts = Vec::new();
    for _ in 0..count {
        contexts.push(Context::decode(buf, &mut pos)?);
    }
    Some(contexts)
}

/// A fresh per-occurrence Unix timestamp for a `d:` field. Any positive integer forwards.
pub(crate) fn fresh_timestamp(rng: &mut (impl Rng + ?Sized)) -> u64 {
    rng.random_range(1..=2_000_000_000u64)
}

// --- shared length-prefixed binary codec ---

/// Append one byte.
pub(crate) fn put_u8(out: &mut Vec<u8>, b: u8) {
    out.push(b);
}

/// Append `len` as a little-endian `u16`, saturating an over-long field. Minted fields are bounded
/// well under `u16::MAX`, so saturation never fires in practice.
fn put_u16(out: &mut Vec<u8>, len: usize) {
    let len = u16::try_from(len).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
}

/// Append a `u16` length prefix then the bytes.
pub(crate) fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u16(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// Append a `u16` count then each byte run.
pub(crate) fn put_tags(out: &mut Vec<u8>, tags: &[Vec<u8>]) {
    put_u16(out, tags.len());
    for tag in tags {
        put_bytes(out, tag);
    }
}

/// Read one byte, advancing `*pos`.
pub(crate) fn get_u8(buf: &[u8], pos: &mut usize) -> Option<u8> {
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

/// Read a `u16`-prefixed byte run, advancing `*pos`.
pub(crate) fn get_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = get_u16(buf, pos)?;
    let end = pos.checked_add(len)?;
    let slice = buf.get(*pos..end)?;
    *pos = end;
    Some(slice)
}

/// Read a `u16`-counted list of byte runs, advancing `*pos`. Does not pre-size from the wire count.
pub(crate) fn get_tags(buf: &[u8], pos: &mut usize) -> Option<Vec<Vec<u8>>> {
    let count = get_u16(buf, pos)?;
    let mut tags = Vec::new();
    for _ in 0..count {
        tags.push(get_bytes(buf, pos)?.to_vec());
    }
    Some(tags)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{decode_response, encode_response, Context, Kind};
    use crate::dogstatsd::is_malformed;

    fn any_kind() -> impl Strategy<Value = Kind> {
        prop_oneof![Just(Kind::Metric), Just(Kind::Event), Just(Kind::ServiceCheck)]
    }

    proptest! {
        /// A corrupt mint always carries an invalid byte. The replacement byte does not guarantee it on
        /// its own, and an identity that looks corrupt but is not lands in the clean half of the working
        /// set and quietly thins the non-UTF-8 rate.
        #[test]
        fn property_test_a_corrupt_mint_is_corrupt(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            for _ in 0..16 {
                if let Some(context) = Context::mint_non_utf8_within(Kind::sample(&mut rng), &mut rng, 8_191) {
                    prop_assert!(context.has_non_utf8(), "a corrupt mint carried no invalid byte: {context:?}");
                }
            }
        }

        /// A minted context conforms to is_malformed. Content carries delimiters, so a raw render may
        /// land on the drop side. The repair loop is the sorter, and any line it does yield forwards.
        #[test]
        fn property_test_render_wellformed_always_forwards(seed: u64, kind in any_kind()) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let Some(context) = Context::mint_within(kind, &mut rng, 8_192) else { return Ok(()) };
            for _ in 0..8 {
                let mut line = Vec::new();
                if context
                    .render_wellformed_within(&mut rng, &mut line, 8_192)
                    .is_some()
                {
                    prop_assert_eq!(is_malformed(&line), Ok(()), "a rendered line was droppable");
                }
            }
        }

        /// A response of minted contexts round-trips through the binary codec, non-UTF-8 and all.
        #[test]
        fn property_test_response_round_trips(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts: Vec<Context> = (0..8)
                .filter_map(|_| Context::mint_within(Kind::sample(&mut rng), &mut rng, 8_192))
                .collect();
            let wire = encode_response(&contexts);
            let decoded = decode_response(&wire);
            prop_assert_eq!(decoded.as_deref(), Some(contexts.as_slice()));
        }

        /// Decode never panics and rejects every truncated prefix of a valid body.
        #[test]
        fn property_test_decode_rejects_truncation(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts: Vec<Context> = (0..4)
                .filter_map(|_| Context::mint_within(Kind::sample(&mut rng), &mut rng, 8_192))
                .collect();
            let wire = encode_response(&contexts);
            for cut in 0..wire.len() {
                let _ = decode_response(&wire[..cut]);
            }
            prop_assert_eq!(decode_response(&wire).map(|c| c.len()), Some(contexts.len()));
        }
    }
}
