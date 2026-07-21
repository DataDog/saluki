//! `DogStatsD` metric payload packing.
//!
//! A driver samples contexts from the intake's pool and packs whole metric lines
//! into a datagram. Each line is `<NAME>:<VALUE>|<TYPE>[|#<TAG>,...]` for one
//! context, with a generated value. Only metrics are emitted: a context is a
//! metric identity, so events and service checks have no place here.
//!
//! ```text
//! <NAME>        := context name bytes, no ':' '|' '\n'
//! <VALUE>       := <NUMBER>(:<NUMBER>)*        ':'-packed multi-value, non-set
//!                | <OPAQUE>                     set type, never parsed
//! <NUMBER>      := any Go strconv.ParseFloat-valid literal
//! <TYPE>        := c|g|ms|h|s|d
//! <TAG>         := context tag bytes, no '|' ',' '\n'
//! ```

use rand::{Rng, RngExt};

use crate::context::Context;

mod metrics;

/// Ceiling on a generated datagram, the Datadog Agent's default
/// `dogstatsd_buffer_size`. A run caps each datagram to the smaller of this and
/// the SUT's sampled receive buffer, so a packed datagram always fits one read
/// and the SUT never truncates a line mid-token.
pub const PAYLOAD_BYTE_LIMIT: usize = 8_192;

/// What a generated payload holds, for anchoring assertions.
#[derive(Clone, Copy, Debug, Default)]
pub struct Payload {
    /// Lines packed into the buffer.
    pub lines: usize,
    /// Largest packed multi-value run among those lines. Zero when none.
    pub max_packed: usize,
}

/// Append one metric line for `context` to `buf`. When the line would exceed
/// `limit_bytes`, drop it whole rather than shear it, leaving `buf` unchanged. A
/// non-empty line always ends in `\n`.
///
/// Returns the packed value count when a multi-value metric was emitted, else
/// `None`. A dropped overflowing line also returns `None`. The caller detects the
/// drop by `buf` being unchanged.
pub fn write_line<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, context: &Context, limit_bytes: usize) -> Option<usize> {
    let start = buf.len();
    let packed = metrics::write_value_line(rng, buf, context);
    if buf.len() - start > limit_bytes {
        // Drop a whole line that exceeds limit_bytes rather than shear it mid-token.
        buf.truncate(start);
        return None;
    }
    packed
}

/// Pack whole `\n`-terminated metric lines for randomly-picked members of
/// `contexts` into `buf` until the next line does not fit the space left under
/// `limit_bytes`. That overflowing line is dropped whole, so the packed datagram
/// never exceeds `limit_bytes` and holds only whole lines. Clears `buf` first. An
/// empty `contexts` yields an empty payload.
pub fn write_payload<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, contexts: &[Context], limit_bytes: usize) -> Payload {
    buf.clear();
    let mut payload = Payload::default();
    if contexts.is_empty() {
        return payload;
    }
    loop {
        let context = &contexts[rng.random_range(0..contexts.len())];
        let start = buf.len();
        // Pass the budget still free so an overflowing line is dropped whole.
        let packed = write_line(rng, buf, context, limit_bytes - buf.len());
        if buf.len() == start {
            // The line did not fit the space left, so the payload is complete.
            break;
        }
        payload.lines += 1;
        if let Some(count) = packed {
            payload.max_packed = payload.max_packed.max(count);
        }
    }
    payload
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{write_payload, PAYLOAD_BYTE_LIMIT};
    use crate::context::Context;
    use crate::dogstatsd::is_malformed;

    /// A small working set of minted contexts, as a driver would fetch from the pool.
    fn working_set(rng: &mut SmallRng, n: usize) -> Vec<Context> {
        (0..n).map(|_| Context::mint(rng)).collect()
    }

    /// Each line is `\n`-terminated with no interior newline, so the line count equals
    /// the newline count.
    #[allow(clippy::naive_bytecount)]
    fn newline_count(buf: &[u8]) -> usize {
        buf.iter().filter(|&&b| b == b'\n').count()
    }

    proptest! {
        // Whole lines only, within the limit, and every packed payload is
        // well-formed end to end (the driver's assertion #1, by construction).
        #[test]
        fn payload_packs_whole_well_formed_lines(seed: u64, limit_bytes in 0..=PAYLOAD_BYTE_LIMIT) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts = working_set(&mut rng, 8);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &mut buf, &contexts, limit_bytes);

            prop_assert!(buf.len() <= limit_bytes);
            prop_assert_eq!(newline_count(&buf), payload.lines);
            if !buf.is_empty() {
                prop_assert_eq!(buf[buf.len() - 1], b'\n');
            }
            prop_assert_eq!(is_malformed(&buf), None);
        }
    }

    #[test]
    fn empty_working_set_yields_empty_payload() {
        let mut rng = SmallRng::seed_from_u64(0);
        let mut buf = Vec::new();
        let payload = write_payload(&mut rng, &mut buf, &[], PAYLOAD_BYTE_LIMIT);
        assert_eq!(payload.lines, 0);
        assert!(buf.is_empty());
    }
}
