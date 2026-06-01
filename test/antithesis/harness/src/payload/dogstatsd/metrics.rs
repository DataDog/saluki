//! Feral `DogStatsD` metric-line generation.

use antithesis_sdk::random::random_choice;
use rand::distr::Distribution;
use rand::Rng;

use super::common::{self, Vibe};
use crate::rand::{Boundary, Wide};

const METRIC_TYPES: &[&[u8]] = &[b"c", b"g", b"ms", b"h", b"s", b"d"];

/// Sample-rate payloads (the `@` field).
const COMPLIANT_RATE: &[&[u8]] = &[b"1", b"0.5", b"0.25", b"0.1", b"0.001"];

/// Container-id payloads (the `c:` field).
const COMPLIANT_CONTAINER: &[&[u8]] = &[b"ci-0a1b2c3d4e5f", b"cid-deadbeef", b"in-4026531840"];

/// External-data items (the `e:` field), joined by ',' at runtime.
const COMPLIANT_EXT: &[&[u8]] = &[
    b"it-true",
    b"it-false",
    b"cn-redis",
    b"cn-web",
    b"pu-810fe89d",
    b"pu-abc",
];

/// Cardinality payloads (the `card:` field).
const COMPLIANT_CARD: &[&[u8]] = &[b"none", b"low", b"orchestrator", b"high"];

/// The `e:` external-data item separator.
const EXT_SEPARATORS: &[u8] = b",";

/// How to build a value.
#[derive(Clone, Copy)]
enum ValueKind {
    Aberrant,
    Int,
    Wide,
}

/// A metric extension field.
#[derive(Clone, Copy)]
enum Ext {
    Rate,
    Container,
    Timestamp,
    External,
    Cardinality,
}

/// Append one metric line `<NAME>:<VALUE>|<TYPE>[|ext...]` to `buf`.
pub(crate) fn write<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    common::write_words(rng, buf, vibe);
    buf.push(b':');
    write_value(rng, buf, vibe);
    buf.push(b'|');
    if let Some(&t) = random_choice(METRIC_TYPES) {
        buf.extend_from_slice(t);
    }
    common::write_tags(rng, buf, vibe);
    write_extensions(rng, buf, vibe);
    buf.push(b'\n');
}

/// Clean: a wide log-uniform value. Feral: an aberrant literal, a wide integer,
/// or a wide float in a compact or cursed-but-equivalent expanded encoding.
fn write_value<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let mut ryu = ryu::Buffer::new();
    match vibe {
        Vibe::Clean => {
            let v: f64 = Wide.sample(rng);
            buf.extend_from_slice(ryu.format(v).as_bytes());
        }
        Vibe::Feral => match random_choice(&[ValueKind::Aberrant, ValueKind::Int, ValueKind::Wide]) {
            Some(ValueKind::Aberrant) => {
                if let Some(&v) = random_choice(common::ABERRANT_VALUES) {
                    buf.extend_from_slice(v);
                }
            }
            Some(ValueKind::Wide) => {
                let v: f64 = Wide.sample(rng);
                common::write_number(rng, buf, ryu.format(v).as_bytes());
            }
            _ => {
                let mut itoa = itoa::Buffer::new();
                let v: i64 = Wide.sample(rng);
                common::write_number(rng, buf, itoa.format(v).as_bytes());
            }
        },
    }
}

/// A boundary-sampled count of extension fields, each a random kind. Repeats and
/// zero are allowed.
fn write_extensions<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match random_choice(&[
            Ext::Rate,
            Ext::Container,
            Ext::Timestamp,
            Ext::External,
            Ext::Cardinality,
        ]) {
            Some(Ext::Rate) => common::write_field(buf, vibe, b"@", COMPLIANT_RATE, common::ABERRANT_VALUES),
            Some(Ext::Container) => common::write_field(buf, vibe, b"c:", COMPLIANT_CONTAINER, common::ABERRANT_WORD),
            Some(Ext::Timestamp) => common::write_field(buf, vibe, b"T", common::COMPLIANT_TS, common::ABERRANT_VALUES),
            Some(Ext::External) => {
                buf.extend_from_slice(b"|e:");
                common::write_segments(rng, buf, vibe, COMPLIANT_EXT, common::ABERRANT_WORD, EXT_SEPARATORS);
            }
            _ => common::write_field(buf, vibe, b"card:", COMPLIANT_CARD, common::ABERRANT_WORD),
        }
    }
}
