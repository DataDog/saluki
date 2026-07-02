//! Feral `DogStatsD` service-check generation.

use rand::distr::Distribution;
use rand::seq::IndexedRandom;
use rand::Rng;

use super::common::{self, Vibe};
use crate::rand::Boundary;

/// Status payloads: OK, warning, critical, unknown.
const COMPLIANT_STATUS: &[&[u8]] = &[b"0", b"1", b"2", b"3"];

/// A service-check optional field.
#[derive(Clone, Copy)]
enum Opt {
    Timestamp,
    Hostname,
    Message,
}

/// Append one service check `_sc|<NAME>|<STATUS>[|opt...]` to `buf`.
pub(crate) fn write<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    buf.extend_from_slice(b"_sc|");
    common::write_words(rng, buf, vibe);
    buf.push(b'|');
    // Status grammar is exactly [0-3] with zero slack — any aberrant status rejects
    // the whole check, so feral strangeness lives in the name and options, not here.
    common::extend_choice(rng, buf, vibe, COMPLIANT_STATUS, COMPLIANT_STATUS);

    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match [Opt::Timestamp, Opt::Hostname, Opt::Message].choose(rng) {
            Some(Opt::Timestamp) => {
                common::write_field(rng, buf, vibe, b"d:", common::COMPLIANT_TS, common::ABERRANT_TS);
            }
            Some(Opt::Hostname) => {
                buf.extend_from_slice(b"|h:");
                common::write_words(rng, buf, vibe);
            }
            _ => {
                buf.extend_from_slice(b"|m:");
                common::write_words(rng, buf, vibe);
            }
        }
    }

    common::write_tags(rng, buf, vibe);
    buf.push(b'\n');
}
