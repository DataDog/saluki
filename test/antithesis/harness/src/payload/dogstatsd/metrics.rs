//! Feral `DogStatsD` metric-line generation.

use rand::distr::Distribution;
use rand::seq::IndexedRandom;
use rand::{Rng, RngExt};

use super::common::{self, Vibe};
use crate::rand::{Boundary, Wide};

const METRIC_TYPES: &[&[u8]] = &[b"c", b"g", b"ms", b"h", b"s", b"d"];

/// Sample-rate payloads (the `@` field).
const COMPLIANT_RATE: &[&[u8]] = &[b"1", b"0.5", b"0.25", b"0.1", b"0.001"];

const ABERRANT_RATE: &[&[u8]] = &[
    b"+0.5", b"1.", b".5", b"0x1p-1", b"1_000", b"2", b"inf", b"+inf", b"-inf", b"nan",
];

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

/// Append one metric line `<NAME>:<VALUE>|<TYPE>[|ext...]` to `buf`. Returns
/// the packed value count when the value is a multi-value run, else `None`.
pub(crate) fn write<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) -> Option<usize> {
    common::write_words(rng, buf, vibe);
    buf.push(b':');
    let packed = write_value(rng, buf, vibe);
    buf.push(b'|');
    if let Some(&t) = METRIC_TYPES.choose(rng) {
        buf.extend_from_slice(t);
    }
    common::write_tags(rng, buf, vibe);
    write_extensions(rng, buf, vibe);
    buf.push(b'\n');
    packed
}

/// Clean: a wide log-uniform value. Feral: an aberrant literal, a wide integer,
/// or a wide float in a compact or cursed-but-equivalent expanded encoding.
/// ~5% of the time emits a multi-value `:`-packed run, returning its value count.
fn write_value<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) -> Option<usize> {
    let mut ryu = ryu::Buffer::new();

    // Multi-value packed metric `v1:v2:...`, the form ADP splits on the colon. Type-agnostic by
    // design — the type is chosen after the value, so a packed run can pair with any type, and a Set
    // keeps the run as a single member. A run is 2..=5 values, "multi" being at least two.
    if rng.random_range(0..20u8) == 0 {
        let count = rng.random_range(2..=5u8);
        for i in 0..count {
            if i > 0 {
                buf.push(b':');
            }
            let v: f64 = Wide.sample(rng);
            buf.extend_from_slice(ryu.format(v).as_bytes());
        }
        return Some(usize::from(count));
    }

    match vibe {
        Vibe::Clean => {
            let v: f64 = Wide.sample(rng);
            buf.extend_from_slice(ryu.format(v).as_bytes());
        }
        Vibe::Feral => match [ValueKind::Aberrant, ValueKind::Int, ValueKind::Wide].choose(rng) {
            Some(ValueKind::Aberrant) => {
                if let Some(&v) = common::ABERRANT_VALUE.choose(rng) {
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
    None
}

/// A boundary-sampled count of extension fields, each a random kind. Repeats and
/// zero are allowed.
fn write_extensions<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match [
            Ext::Rate,
            Ext::Container,
            Ext::Timestamp,
            Ext::External,
            Ext::Cardinality,
        ]
        .choose(rng)
        {
            Some(Ext::Rate) => common::write_field(rng, buf, vibe, b"@", COMPLIANT_RATE, ABERRANT_RATE),
            Some(Ext::Container) => {
                common::write_field(rng, buf, vibe, b"c:", COMPLIANT_CONTAINER, common::ABERRANT_WORD);
            }
            Some(Ext::Timestamp) => {
                common::write_field(rng, buf, vibe, b"T", common::COMPLIANT_TS, common::ABERRANT_TS);
            }
            Some(Ext::External) => {
                buf.extend_from_slice(b"|e:");
                common::write_segments(rng, buf, vibe, COMPLIANT_EXT, common::ABERRANT_WORD, EXT_SEPARATORS);
            }
            _ => common::write_field(rng, buf, vibe, b"card:", COMPLIANT_CARD, common::ABERRANT_WORD),
        }
    }
}
