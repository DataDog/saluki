//! Feral `DogStatsD` service-check generation.

use antithesis_sdk::random::random_choice;
use rand::distr::Distribution;
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
    common::extend_choice(buf, vibe, COMPLIANT_STATUS, common::ABERRANT_VALUES);

    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match random_choice(&[Opt::Timestamp, Opt::Hostname, Opt::Message]) {
            Some(Opt::Timestamp) => {
                common::write_field(buf, vibe, b"d:", common::COMPLIANT_TS, common::ABERRANT_VALUES);
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
