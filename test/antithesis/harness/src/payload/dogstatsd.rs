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

/// Write one `DogStatsD` message of a sampled type to `buf` at the given vibe.
/// Returns the packed value count when a multi-value metric was emitted, else
/// `None`.
pub fn send<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) -> Option<usize> {
    buf.clear();
    match choose_message(rng) {
        Message::Event => {
            events::write(rng, buf, vibe);
            None
        }
        Message::ServiceCheck => {
            service_checks::write(rng, buf, vibe);
            None
        }
        Message::Metric => metrics::write(rng, buf, vibe),
    }
}
