//! Feral `DogStatsD` event generation.

use rand::distr::Distribution;
use rand::seq::IndexedRandom;
use rand::Rng;

use super::common::{self, Vibe};
use crate::rand::Boundary;

/// Priority payloads (the `p:` field).
const COMPLIANT_PRIO: &[&[u8]] = &[b"normal", b"low"];

/// Alert-type payloads (the `t:` field).
const COMPLIANT_ALERT: &[&[u8]] = &[b"error", b"warning", b"info", b"success"];

/// An event optional field.
#[derive(Clone, Copy)]
enum Opt {
    Timestamp,
    Hostname,
    AggKey,
    Priority,
    Source,
    Alert,
}

/// Append one event `_e{<TLEN>,<XLEN>}:<TITLE>|<TEXT>[|opt...]` to `buf`.
pub(crate) fn write<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let mut title = Vec::new();
    common::write_words(rng, &mut title, vibe);
    let mut text = Vec::new();
    common::write_words(rng, &mut text, vibe);

    buf.extend_from_slice(b"_e{");
    write_len(buf, title.len());
    buf.push(b',');
    write_len(buf, text.len());
    buf.extend_from_slice(b"}:");
    buf.extend_from_slice(&title);
    buf.push(b'|');
    buf.extend_from_slice(&text);

    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match [
            Opt::Timestamp,
            Opt::Hostname,
            Opt::AggKey,
            Opt::Priority,
            Opt::Source,
            Opt::Alert,
        ]
        .choose(rng)
        {
            Some(Opt::Timestamp) => {
                common::write_field(rng, buf, vibe, b"d:", common::COMPLIANT_TS, common::ABERRANT_TS);
            }
            Some(Opt::Hostname) => {
                buf.extend_from_slice(b"|h:");
                common::write_words(rng, buf, vibe);
            }
            Some(Opt::AggKey) => {
                buf.extend_from_slice(b"|k:");
                common::write_words(rng, buf, vibe);
            }
            Some(Opt::Priority) => common::write_field(rng, buf, vibe, b"p:", COMPLIANT_PRIO, common::ABERRANT_WORD),
            Some(Opt::Source) => {
                buf.extend_from_slice(b"|s:");
                common::write_words(rng, buf, vibe);
            }
            _ => common::write_field(rng, buf, vibe, b"t:", COMPLIANT_ALERT, common::ABERRANT_WORD),
        }
    }

    common::write_tags(rng, buf, vibe);
    buf.push(b'\n');
}

/// The event header length: the true byte length of the title or text. A header
/// length that disagrees with the payload makes the Agent reject the event, so
/// the feral strangeness lives in the title and text content, not a lied length.
fn write_len(buf: &mut Vec<u8>, actual: usize) {
    let mut itoa = itoa::Buffer::new();
    buf.extend_from_slice(itoa.format(actual).as_bytes());
}
