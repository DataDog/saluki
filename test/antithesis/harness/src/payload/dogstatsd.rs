//! `DogStatsD` payload generation.
//!
//! Dogstatsd is three message types: metric, event, service check.
//!
//! # Metrics
//!
//! ```text
//! <NAME>:<VALUE>|<TYPE>|@<SAMPLE_RATE>|#<TAG>,<TAG>...|c:<CONTAINER>|T<TS>|e:<EXT>|card:<CARD>
//!
//! Required: <NAME>:<VALUE>|<TYPE>.
//!
//! <NAME>        := [^:|\n]+
//! <VALUE>       := <NUMBER>(:<NUMBER>)*        ':'-packed multi-value, non-set
//!                | [^|\n]+                     raw string, set type
//! <NUMBER>      := [+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)? | [+-]?(inf|infinity|nan)
//! <TYPE>        := c|g|ms|h|s|d                count gauge timer histogram set distribution
//! <SAMPLE_RATE> := @<NUMBER>
//! <TAG>         := [^,|\n]+                    conventionally <KEY>:<VALUE>, the ':' is not required
//! <CONTAINER>   := c:[^|\n]+                   e.g. ci-<id>, in-<inode>
//! <TS>          := T\d+                        unix seconds
//! <EXT>         := e:[^|\n]+                   e.g. it-,cn-,pu-
//! <CARD>        := card:[^|\n]+                recognized: none|low|orchestrator|high
//! ```
//!
//! # Events
//!
//! ```text
//! _e{<TITLE_LEN>,<TEXT_LEN>}:<TITLE>|<TEXT>|d:<TS>|h:<HOST>|k:<AGGKEY>|p:<PRIO>|s:<SRC>|t:<ALERT>|#<TAGS>
//!
//! Required: _e{<TITLE_LEN>,<TEXT_LEN>}:<TITLE>|<TEXT>. c: / e: / card: are valid here too.
//!
//! <TITLE_LEN>,<TEXT_LEN> := \d+               byte length of TITLE / TEXT
//! <TITLE>,<TEXT>         := [^\n]{LEN}         length-delimited, so '|' and ':' are allowed
//! <TS>          := d:\d+                       unix seconds
//! <HOST>        := h:[^|\n]+
//! <AGGKEY>      := k:[^|\n]+
//! <PRIO>        := p:[^|\n]+                   recognized: normal|low (else default)
//! <SRC>         := s:[^|\n]+
//! <ALERT>       := t:[^|\n]+                   recognized: error|warning|info|success (else default)
//! <TAGS>        := #<TAG>(,<TAG>)*
//! ```
//!
//! # Service checks
//!
//! ```text
//! _sc|<NAME>|<STATUS>|d:<TS>|h:<HOST>|#<TAG>,<TAG>...|m:<MESSAGE>
//!
//! Required: _sc|<NAME>|<STATUS>. c: / e: / card: are valid here too.
//!
//! <NAME>        := [^|\n]+
//! <STATUS>      := [0-3]                       OK warning critical unknown
//! <TS>          := d:\d+                       unix seconds
//! <HOST>        := h:[^|\n]+
//! <TAGS>        := #<TAG>(,<TAG>)*
//! <MESSAGE>     := m:[^|\n]+
//! ```
//!
//! # Name combinatorics
//!
//! A clean name is `c` segments from `COMPLIANT_WORD` (10 words) joined by
//! `NAME_SEPARATORS` (4). Distinct names at count `c`: `10^c · 4^(c-1)`. `c` is
//! sampled by vibe: clean draws from `ELEMENT_COUNTS_CLEAN` (a small body, no
//! boundary cases); feral draws from `ELEMENT_COUNTS_FERAL`, which adds the `0`
//! and large boundary counts as a tail.
//!
//! | `c`   | P(c) clean | P(c) feral | distinct names |
//! |-------|------------|------------|----------------|
//! | 0     | —          | 1/12       | 1 (empty)      |
//! | 1-3   | 6/9        | 6/12       | 10 .. ~16e3    |
//! | 4-6   | 3/9        | 3/12       | ~640e3 .. ~1e9 |
//! | 127   | —          | 1/12       | ~10^203        |
//! | 255   | —          | 1/12       | ~10^408        |

use rand::{Rng, RngExt};

mod common;
mod events;
mod metrics;
mod service_checks;

pub use common::{sample_vibe, Vibe};

/// The three `DogStatsD` message types.
#[derive(Clone, Copy)]
enum Message {
    Metric,
    Event,
    ServiceCheck,
}

/// Sample a message type. The mix is heavily metric-weighted — 98% metric, 1%
/// event, 1% service check. Metrics drive the aggregate context map and the
/// sketch bin paths, the invariants worth exercising, so the bulk of load goes
/// there while events and service checks still fire often enough to keep their
/// own anchors non-vacuous.
fn choose_message<R: Rng + ?Sized>(rng: &mut R) -> Message {
    match rng.random_range(0..100u32) {
        0 => Message::Event,
        1 => Message::ServiceCheck,
        _ => Message::Metric,
    }
}

/// The `dogstatsd_buffer_size` default, via Datadog Agent.
pub const PAYLOAD_BYTE_LIMIT: usize = 8_192;

/// What a generated payload holds, for anchoring assertions.
#[derive(Clone, Copy, Debug, Default)]
pub struct Payload {
    /// Lines packed into the buffer.
    pub lines: usize,
    /// Largest packed multi-value run among those lines. Zero when none.
    pub max_packed: usize,
}

/// Append one `DogStatsD` line of a sampled type to `buf`. When a line would
/// exceed `limit`, drop it whole rather than shear it, leaving `buf` unchanged.
/// A non-empty line always ends in `\n`.
///
/// Returns the packed value count when a multi-value metric was emitted, else
/// `None`.
pub fn write_line<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe, limit: usize) -> Option<usize> {
    let start = buf.len();
    let packed = match choose_message(rng) {
        Message::Event => {
            events::write(rng, buf, vibe);
            None
        }
        Message::ServiceCheck => {
            service_checks::write(rng, buf, vibe);
            None
        }
        Message::Metric => metrics::write(rng, buf, vibe),
    };
    if buf.len() - start > limit {
        // Drop a whole line that exceeds the limit rather than shear it mid-token.
        // A sheared fragment is exactly the parse-error spew this generator avoids.
        buf.truncate(start);
        return None;
    }
    packed
}

/// Per-run line composition: every line clean, every line feral, or a per-line
/// clean-or-feral mix.
#[derive(Clone, Copy, Debug)]
pub enum Batch {
    /// Every line clean.
    Clean,
    /// Every line feral.
    Feral,
    /// Each line independently clean or feral.
    Mixed,
}

impl Batch {
    /// The vibe for one line of this batch, sampled per call so `Mixed` interleaves.
    fn vibe<R: Rng + ?Sized>(self, rng: &mut R) -> Vibe {
        match self {
            Batch::Clean => Vibe::Clean,
            Batch::Feral => Vibe::Feral,
            Batch::Mixed => sample_vibe(rng),
        }
    }
}

/// Fill `buf` with `\n`-terminated lines, packing whole lines straight into it
/// until the next would exceed `limit` total bytes. A line that overruns the
/// remaining budget rolls back and ends the payload. A single line too large to
/// fit at all gets skipped so a later, smaller line can still pack. `buf` holds
/// only whole lines and never exceeds `limit`. Each line takes its vibe from
/// `batch`, so a `Mixed` payload interleaves clean and feral lines. Clears `buf`
/// first.
pub fn write_payload<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, batch: Batch, limit: usize) -> Payload {
    buf.clear();
    let mut payload = Payload::default();
    // Consecutive oversized lines write_line dropped. Bounded so a run of them
    // cannot spin when `limit` is smaller than any line.
    let mut skipped = 0u8;
    loop {
        let vibe = batch.vibe(rng);
        let start = buf.len();
        let packed = write_line(rng, buf, vibe, limit);
        if buf.len() > limit {
            // This whole line overruns the remaining budget. Roll it back and stop.
            buf.truncate(start);
            break;
        }
        if buf.len() == start {
            // write_line dropped a line too large to fit at all. Skip it and try
            // another rather than end the payload early.
            skipped += 1;
            if skipped >= 16 {
                break;
            }
            continue;
        }
        skipped = 0;
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

    use super::{write_line, write_payload, Batch, Vibe};

    fn any_vibe() -> impl Strategy<Value = Vibe> {
        prop_oneof![Just(Vibe::Clean), Just(Vibe::Feral)]
    }

    fn any_batch() -> impl Strategy<Value = Batch> {
        prop_oneof![Just(Batch::Clean), Just(Batch::Feral), Just(Batch::Mixed)]
    }

    /// Lines carry no interior newline and each is `\n`-terminated, so the line
    /// count equals the newline count.
    #[allow(clippy::naive_bytecount)]
    fn newline_count(buf: &[u8]) -> usize {
        buf.iter().filter(|&&b| b == b'\n').count()
    }

    proptest! {
        #[test]
        fn write_line_stays_within_its_limit(seed: u64, limit: u16, vibe in any_vibe()) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let limit = usize::from(limit);
            let mut buf = Vec::new();
            write_line(&mut rng, &mut buf, vibe, limit);

            prop_assert!(buf.len() <= limit);
            if !buf.is_empty() {
                prop_assert_eq!(buf[buf.len() - 1], b'\n');
                prop_assert_eq!(newline_count(&buf), 1);
            }
        }

        #[test]
        fn write_payload_stays_within_its_limit(seed: u64, limit: u16, batch in any_batch()) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let limit = usize::from(limit);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &mut buf, batch, limit);

            prop_assert!(buf.len() <= limit);
            prop_assert_eq!(newline_count(&buf), payload.lines);
        }
    }
}
