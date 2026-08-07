//! `DogStatsD` payload generation.
//!
//! Generation builds three structured message types, metric, event, and service check, then
//! serializes each to datagram bytes. Every message is checked with [`crate::dogstatsd::is_malformed`]
//! on its serialized bytes and repaired until the Datadog Agent would forward it. There is no
//! clean/feral/mixed configuration and no per-line vibe. The legal space is exactly the set of
//! payloads `is_malformed` accepts. Content fields range over the full forwarded alphabet including
//! delimiter bytes, and the predicate sorts the delimiter-bearing cases the Agent keeps from the ones
//! it drops.
//!
//! ```text
//! metric:        <NAME>:<VALUE>(:<VALUE>)*|<TYPE>[|@<RATE>][|#<TAGS>][|c:..][|e:..][|card:..]
//! event:         _e{<TITLE_LEN>,<TEXT_LEN>}:<TITLE>|<TEXT>[|h:..][|k:..][|p:..][|s:..][|t:..][|#<TAGS>]
//! service check: _sc|<NAME>|<STATUS>[|h:..][|m:..][|#<TAGS>]
//! ```

use rand::{Rng, RngExt};

use crate::dogstatsd::is_malformed;

mod common;
mod events;
mod metrics;
mod service_checks;

/// How many times to re-sample a message whose serialized bytes the Agent would drop before falling
/// back to a guaranteed-well-formed line. Construction is mostly-valid, so the fallback is rare.
const REPAIR_TRIES: usize = 16;

/// A guaranteed-well-formed metric, used only if repair is exhausted, so the pack loop always
/// terminates.
const FALLBACK_LINE: &[u8] = b"harness.fallback:1|c";

/// Ceiling on a generated datagram, the Datadog Agent's default `dogstatsd_buffer_size`. A run caps
/// each datagram to the smaller of this and the SUT's sampled receive buffer, so a packed datagram
/// always fits one read and the SUT never truncates a line mid-token.
pub const PAYLOAD_BYTE_LIMIT: usize = 8_192;

/// What a generated payload holds, for anchoring assertions.
#[derive(Clone, Copy, Debug, Default)]
pub struct Payload {
    /// Lines packed into the buffer.
    pub lines: usize,
    /// Largest packed multi-value run among those lines. Zero when none.
    pub max_packed: usize,
}

/// One of the three `DogStatsD` message types, structured.
enum Message {
    Metric(metrics::Metric),
    Event(events::Event),
    ServiceCheck(service_checks::ServiceCheck),
}

impl Message {
    /// Sample a message type. The mix is heavily metric-weighted: 98% metric, 1% event, 1% service
    /// check. Metrics drive the aggregate context and sketch paths, so the bulk of load goes there
    /// while events and service checks still fire often enough to keep their anchors non-vacuous.
    fn generate(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<Self> {
        match rng.random_range(0..100u32) {
            0 => events::Event::generate(rng, budget).map(Message::Event),
            1 => service_checks::ServiceCheck::generate(rng, budget).map(Message::ServiceCheck),
            _ => metrics::Metric::generate(rng, budget).map(Message::Metric),
        }
    }

    /// Serialize to datagram bytes, without the trailing `\n`.
    fn serialize(&self, out: &mut Vec<u8>) {
        match self {
            Message::Metric(m) => m.serialize(out),
            Message::Event(e) => e.serialize(out),
            Message::ServiceCheck(s) => s.serialize(out),
        }
    }

    /// The packed multi-value run length this message contributes, or zero.
    fn packed(&self) -> usize {
        match self {
            Message::Metric(m) => m.packed(),
            Message::Event(_) | Message::ServiceCheck(_) => 0,
        }
    }
}

/// Sample one message and serialize it to bytes the Agent forwards, within `budget` bytes including
/// the trailing `\n`. Re-samples up to [`REPAIR_TRIES`] times a message the Agent would drop or one
/// that overruns the budget, then falls back to [`FALLBACK_LINE`]. Returns `None` only when even the
/// fallback does not fit.
///
/// The budget is a construction input, not a filter applied afterwards: a sample that does not fit is
/// another sample to take, never a reason to end the payload. Ending it would leave the datagram
/// short, or empty when the very first sample overran, and a short datagram is workload the SUT never
/// sees while the driver still spends one of its configured sends.
fn wellformed_line(rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<(Vec<u8>, usize)> {
    for _ in 0..REPAIR_TRIES {
        // The message is built against the budget, so it fits by construction. The retry is only for
        // the Agent's own drop rules, which are about content rather than size.
        let Some(message) = Message::generate(rng, budget.saturating_sub(1)) else {
            break;
        };
        let mut bytes = Vec::new();
        message.serialize(&mut bytes);
        if bytes.len() < budget && is_malformed(&bytes).is_ok() {
            return Some((bytes, message.packed()));
        }
    }
    (FALLBACK_LINE.len() < budget).then(|| (FALLBACK_LINE.to_vec(), 0))
}

/// Pack whole `\n`-terminated lines into `buf` until the budget cannot hold another line. Each line is
/// sampled against the budget still free, so the datagram fills rather than ending on the first
/// oversized sample, never exceeds `limit_bytes`, and holds only whole lines every one of which the
/// Agent forwards. Clears `buf` first.
pub fn write_payload(rng: &mut (impl Rng + ?Sized), buf: &mut Vec<u8>, limit_bytes: usize) -> Payload {
    buf.clear();
    let mut payload = Payload::default();
    loop {
        // The budget left, `\n` included. Every accepted line consumes at least one byte, so the
        // remaining budget strictly shrinks and the loop ends once nothing fits.
        let Some((line, packed)) = wellformed_line(rng, limit_bytes - buf.len()) else {
            break;
        };
        buf.extend_from_slice(&line);
        buf.push(b'\n');
        payload.lines += 1;
        payload.max_packed = payload.max_packed.max(packed);
    }
    payload
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{write_payload, FALLBACK_LINE, PAYLOAD_BYTE_LIMIT};
    use crate::dogstatsd::is_malformed;

    /// Lines carry no interior newline and each is `\n`-terminated, so the line count equals the
    /// newline count.
    #[allow(clippy::naive_bytecount)]
    fn newline_count(buf: &[u8]) -> usize {
        buf.iter().filter(|&&b| b == b'\n').count()
    }

    proptest! {
        /// Every datagram the generator emits is one the Agent forwards. A packed datagram must be
        /// entirely well-formed.
        #[test]
        fn property_test_every_payload_is_well_formed(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut buf = Vec::new();
            for _ in 0..8 {
                write_payload(&mut rng, &mut buf, PAYLOAD_BYTE_LIMIT);
                prop_assert_eq!(is_malformed(&buf), Ok(()), "emitted a droppable datagram: {:?}", String::from_utf8_lossy(&buf));
            }
        }

        #[test]
        fn property_test_payload_stays_within_its_limit(seed: u64, limit_bytes: u16) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let limit_bytes = usize::from(limit_bytes);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &mut buf, limit_bytes);

            prop_assert!(buf.len() <= limit_bytes);
            prop_assert_eq!(newline_count(&buf), payload.lines);
            if !buf.is_empty() {
                prop_assert_eq!(buf[buf.len() - 1], b'\n');
            }
        }

        /// A budget that admits any line at all yields load. An empty datagram burns one of the
        /// driver's configured sends without reaching the SUT, so the generator must fill the budget
        /// it was given rather than give up on the first oversized sample.
        #[test]
        fn property_test_payload_is_never_empty_when_a_line_fits(seed: u64, limit_bytes in (FALLBACK_LINE.len() + 1)..4096usize) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &mut buf, limit_bytes);

            prop_assert!(!buf.is_empty(), "empty datagram at limit {}", limit_bytes);
            prop_assert!(payload.lines > 0);
        }
    }
}
