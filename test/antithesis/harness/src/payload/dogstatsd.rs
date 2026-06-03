//! `DogStatsD` payload generation.

// Here's the basic idea.
//
// Dogstatsd is three message types:
//
// * metric
// * event
// * service check
//
// # Metrics
//
//     <NAME>:<VALUE>|<TYPE>|@<SAMPLE_RATE>|#<TAG>,<TAG>...|c:<CONTAINER>|T<TS>|e:<EXT>|card:<CARD>
//
// Required: <NAME>:<VALUE>|<TYPE>.
//
// * <NAME>        := [^:|\n]+
// * <VALUE>       := <NUMBER>(:<NUMBER>)*        ':'-packed multi-value, non-set
//                  | [^|\n]+                     raw string, set type
// * <NUMBER>      := [+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)? | [+-]?(inf|infinity|nan)
// * <TYPE>        := c|g|ms|h|s|d                count gauge timer histogram set distribution
// * <SAMPLE_RATE> := @<NUMBER>
// * <TAG>         := [^,|\n]+                    conventionally <KEY>:<VALUE>, the ':' is not required
// * <CONTAINER>   := c:[^|\n]+                   e.g. ci-<id>, in-<inode>
// * <TS>          := T\d+                        unix seconds
// * <EXT>         := e:[^|\n]+                   e.g. it-,cn-,pu-
// * <CARD>        := card:[^|\n]+                recognized: none|low|orchestrator|high
//
// # Events
//
//     _e{<TITLE_LEN>,<TEXT_LEN>}:<TITLE>|<TEXT>|d:<TS>|h:<HOST>|k:<AGGKEY>|p:<PRIO>|s:<SRC>|t:<ALERT>|#<TAGS>
//
// Required: _e{<TITLE_LEN>,<TEXT_LEN>}:<TITLE>|<TEXT>. c: / e: / card: are valid here too.
//
// * <TITLE_LEN>,
//   <TEXT_LEN>    := \d+                         byte length of TITLE / TEXT
// * <TITLE>,
//   <TEXT>        := [^\n]{LEN}                  length-delimited, so '|' and ':' are allowed; '\\n' -> newline
// * <TS>          := d:\d+                       unix seconds
// * <HOST>        := h:[^|\n]+
// * <AGGKEY>      := k:[^|\n]+
// * <PRIO>        := p:[^|\n]+                    recognized: normal|low (else default)
// * <SRC>         := s:[^|\n]+
// * <ALERT>       := t:[^|\n]+                    recognized: error|warning|info|success (else default)
// * <TAGS>        := #<TAG>(,<TAG>)*
//
// # Service checks
//
//     _sc|<NAME>|<STATUS>|d:<TS>|h:<HOST>|#<TAG>,<TAG>...|m:<MESSAGE>
//
// Required: _sc|<NAME>|<STATUS>. c: / e: / card: are valid here too.
//
// * <NAME>        := [^|\n]+
// * <STATUS>      := [0-3]                       OK warning critical unknown
// * <TS>          := d:\d+                       unix seconds
// * <HOST>        := h:[^|\n]+
// * <TAGS>        := #<TAG>(,<TAG>)*
// * <MESSAGE>     := m:[^|\n]+

use antithesis_sdk::random::random_choice;
use rand::Rng;

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

/// Write one `DogStatsD` message of a random type to `buf` at the given vibe.
pub fn send<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    buf.clear();
    match random_choice(&[Message::Metric, Message::Event, Message::ServiceCheck]) {
        Some(Message::Event) => events::write(rng, buf, vibe),
        Some(Message::ServiceCheck) => service_checks::write(rng, buf, vibe),
        _ => metrics::write(rng, buf, vibe),
    }
}
