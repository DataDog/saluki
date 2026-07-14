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

/// The value field: a `:`-packed run of scalars. Run odds:
///
/// | run | odds   |
/// |-----|--------|
/// | 1   | 99%    |
/// | 2   | 0.5%   |
/// | 3   | 0.25%  |
/// | 4   | 0.125% |
/// | 5   | 0.125% |
fn write_value<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) -> Option<usize> {
    let count: u8 = match rng.random_range(0..800u16) {
        0..792 => 1,
        792..796 => 2,
        796..798 => 3,
        798 => 4,
        799..=u16::MAX => 5,
    };
    for i in 0..count {
        if i > 0 {
            buf.push(b':');
        }
        write_scalar(rng, buf, vibe);
    }
    (count > 1).then_some(usize::from(count))
}

/// Write one scalar. Clean: a wide log-uniform value. Feral: an aberrant literal,
/// a wide integer, or a wide float in a compact or cursed-but-equivalent expanded
/// encoding.
fn write_scalar<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let mut ryu = ryu::Buffer::new();
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
}

/// A boundary-sampled count of extension fields, each a random kind. Repeats and
/// zero are allowed.
fn write_extensions<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, vibe: Vibe) {
    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match [Ext::Rate, Ext::Container, Ext::External, Ext::Cardinality].choose(rng) {
            Some(Ext::Rate) => common::write_field(rng, buf, vibe, b"@", COMPLIANT_RATE, ABERRANT_RATE),
            Some(Ext::Container) => {
                common::write_field(rng, buf, vibe, b"c:", COMPLIANT_CONTAINER, common::ABERRANT_WORD);
            }
            Some(Ext::External) => {
                buf.extend_from_slice(b"|e:");
                common::write_segments(rng, buf, vibe, COMPLIANT_EXT, common::ABERRANT_WORD, EXT_SEPARATORS);
            }
            _ => common::write_field(rng, buf, vibe, b"card:", COMPLIANT_CARD, common::ABERRANT_WORD),
        }
    }
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{write_value, Vibe};

    fn any_vibe() -> impl Strategy<Value = Vibe> {
        prop_oneof![Just(Vibe::Clean), Just(Vibe::Feral)]
    }

    proptest! {
        /// A value carrying a `:`-packed run must report its count. The split
        /// piece count is the run length, so it must equal the returned count.
        #[test]
        fn packed_value_reports_its_count(seed: u64, vibe in any_vibe()) {
            let mut rng = SmallRng::seed_from_u64(seed);
            for _ in 0..1_000 {
                let mut buf = Vec::new();
                let packed = write_value(&mut rng, &mut buf, vibe);
                let values = buf.split(|&b| b == b':').count();
                if values > 1 {
                    prop_assert_eq!(
                        packed,
                        Some(values),
                        "value {:?} packs {} values but write_value returned {:?}",
                        String::from_utf8_lossy(&buf),
                        values,
                        packed
                    );
                }
            }
        }
    }
}
