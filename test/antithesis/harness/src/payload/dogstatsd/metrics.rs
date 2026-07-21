//! Metric value-line generation for a sampled context.
//!
//! The context carries its name, tagset and type from the pool. This attaches a
//! value and renders the line. Every value is well-formed. A set value is opaque,
//! and every other value is a `:`-packed run of Go-float-parseable scalars, so the
//! rendered line is never malformed.

use rand::distr::Distribution;
use rand::{Rng, RngExt};

use crate::context::{Context, MetricType};
use crate::rand::{Boundary, Wide};

/// Go-float-valid special literals, kept for value diversity. They exercise the
/// NaN/Inf, hex-float, underscore, trailing/leading-dot, and long-mantissa parse
/// paths without ever being malformed.
const SPECIAL_VALUES: &[&[u8]] = &[
    b"0",
    b"-0",
    b"inf",
    b"-inf",
    b"+inf",
    b"nan",
    b"infinity",
    b"0x1p4",
    b"1_000",
    b"1.",
    b".5",
    b"3.141592653589793115997963468544185161590576171875",
];

/// Compact, or a cursed-but-equivalent zero-padded encoding.
#[derive(Clone, Copy)]
enum Form {
    Compact,
    Expanded,
}

/// Render `<name>:<value>|<type>[|#tags]` plus a newline for `context`. Returns the
/// packed value count when the value is a multi-value run, else `None`.
pub(crate) fn write_value_line<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, context: &Context) -> Option<usize> {
    let mut value = Vec::new();
    let packed = write_value(rng, &mut value, context.kind);
    let mut extensions = Vec::new();
    write_extensions(rng, &mut extensions);
    context.render_line(&value, &extensions, buf);
    packed
}

/// Sample-rate literals for the `@` field, all Go-float-valid so the line stays
/// well-formed.
const RATES: &[&[u8]] = &[
    b"1", b"0.5", b"0.25", b"0.1", b"0.001", b"2", b"+0.5", b"1.", b".5", b"0x1p-1", b"1_000", b"inf", b"+inf", b"-inf",
    b"nan",
];

/// Container-id literals for the `c:` local-data field.
const CONTAINERS: &[&[u8]] = &[b"ci-0a1b2c3d4e5f", b"cid-deadbeef", b"in-4026531840"];

/// External-data literals for the `e:` field, ASCII in the RFC-1123 shape the parser expects.
const EXTERNAL: &[&[u8]] = &[b"it-true", b"it-false", b"cn-redis", b"cn-web", b"pu-810fe89d", b"pu-abc"];

/// Cardinality literals for the `card:` field.
const CARDINALITIES: &[&[u8]] = &[b"none", b"low", b"orchestrator", b"high"];

/// Append a boundary-sampled run of metric extension chunks. Each chunk keeps the
/// line well-formed: `@` is a Go-float sample rate, and `c:`/`e:`/`card:` are chunks
/// the Agent skips.
fn write_extensions<R: Rng + ?Sized>(rng: &mut R, out: &mut Vec<u8>) {
    let count = Boundary::<u8>::new().sample(rng);
    for _ in 0..count {
        match rng.random_range(0..4u8) {
            0 => {
                out.extend_from_slice(b"|@");
                out.extend_from_slice(RATES[rng.random_range(0..RATES.len())]);
            }
            1 => {
                out.extend_from_slice(b"|c:");
                out.extend_from_slice(CONTAINERS[rng.random_range(0..CONTAINERS.len())]);
            }
            2 => {
                out.extend_from_slice(b"|e:");
                out.extend_from_slice(EXTERNAL[rng.random_range(0..EXTERNAL.len())]);
            }
            _ => {
                out.extend_from_slice(b"|card:");
                out.extend_from_slice(CARDINALITIES[rng.random_range(0..CARDINALITIES.len())]);
            }
        }
    }
}

/// Build the value field. A set value is one opaque integer. Every other type is a
/// `:`-packed run of scalars, single-valued about 99% of the time.
fn write_value<R: Rng + ?Sized>(rng: &mut R, out: &mut Vec<u8>, kind: MetricType) -> Option<usize> {
    if matches!(kind, MetricType::Set) {
        let mut itoa = itoa::Buffer::new();
        let v: i64 = Wide.sample(rng);
        out.extend_from_slice(itoa.format(v).as_bytes());
        return None;
    }
    let count: u8 = match rng.random_range(0..800u16) {
        0..792 => 1,
        792..796 => 2,
        796..798 => 3,
        798 => 4,
        _ => 5,
    };
    for i in 0..count {
        if i > 0 {
            out.push(b':');
        }
        write_scalar(rng, out);
    }
    (count > 1).then_some(usize::from(count))
}

/// One scalar: a Go-valid special, or a wide float or integer in compact or
/// expanded form.
fn write_scalar<R: Rng + ?Sized>(rng: &mut R, out: &mut Vec<u8>) {
    match rng.random_range(0..3u8) {
        0 => out.extend_from_slice(SPECIAL_VALUES[rng.random_range(0..SPECIAL_VALUES.len())]),
        1 => {
            let mut ryu = ryu::Buffer::new();
            let v: f64 = Wide.sample(rng);
            write_number(rng, out, ryu.format(v).as_bytes());
        }
        _ => {
            let mut itoa = itoa::Buffer::new();
            let v: i64 = Wide.sample(rng);
            write_number(rng, out, itoa.format(v).as_bytes());
        }
    }
}

/// Write `digits` as-is, or padded with equivalent leading zeros, and trailing
/// zeros when there is a fractional part. Same value, cursed-but-valid encoding.
fn write_number<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>, digits: &[u8]) {
    match [Form::Compact, Form::Expanded][rng.random_range(0..2)] {
        Form::Expanded => {
            let (sign, rest) = match digits.first() {
                Some(&(b'-' | b'+')) => (&digits[..1], &digits[1..]),
                _ => (&digits[..0], digits),
            };
            buf.extend_from_slice(sign);
            pad_zeros(rng, buf);
            buf.extend_from_slice(rest);
            let fractional = rest.contains(&b'.') && !rest.iter().any(|&c| c == b'e' || c == b'E');
            if fractional {
                pad_zeros(rng, buf);
            }
        }
        Form::Compact => buf.extend_from_slice(digits),
    }
}

/// Append a boundary-sampled run of `0` bytes to `buf`.
fn pad_zeros<R: Rng + ?Sized>(rng: &mut R, buf: &mut Vec<u8>) {
    let zeros = usize::from(Boundary::<u8>::new().sample(rng));
    buf.resize(buf.len() + zeros, b'0');
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::write_value_line;
    use crate::context::Context;
    use crate::dogstatsd::is_malformed_line;

    /// Whether `haystack` contains the `needle` byte run.
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    // The four metric extensions are emitted. Guards against silently dropping the
    // `@`/`c:`/`e:`/`card:` fields from the generator again.
    #[test]
    fn all_extensions_are_emitted() {
        let mut rng = SmallRng::seed_from_u64(9);
        let context = Context::mint(&mut rng);
        let (mut rate, mut container, mut external, mut cardinality) = (false, false, false, false);
        for _ in 0..4_000 {
            let mut line = Vec::new();
            write_value_line(&mut rng, &mut line, &context);
            rate |= contains(&line, b"|@");
            container |= contains(&line, b"|c:");
            external |= contains(&line, b"|e:");
            cardinality |= contains(&line, b"|card:");
        }
        assert!(
            rate && container && external && cardinality,
            "extension missing: @={rate} c={container} e={external} card={cardinality}"
        );
    }

    proptest! {
        // Every rendered value-line is well-formed: the value the generator attaches
        // parses for its type, so `is_malformed_line` never fires.
        #[test]
        fn rendered_value_line_is_well_formed(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            for _ in 0..256 {
                let context = Context::mint(&mut rng);
                let mut line = Vec::new();
                write_value_line(&mut rng, &mut line, &context);
                let line = line.strip_suffix(b"\n").unwrap_or(&line);
                prop_assert!(!is_malformed_line(line), "line={:?}", String::from_utf8_lossy(line));
            }
        }

        // A multi-value run reports its packed count: the colon-split piece count of
        // the value field equals the returned count.
        #[test]
        fn packed_value_reports_its_count(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let context = Context::mint(&mut rng);
            for _ in 0..1_000 {
                let mut line = Vec::new();
                if let Some(count) = write_value_line(&mut rng, &mut line, &context) {
                    // The value field is between the first ':' and the first '|'.
                    let after_name = &line[context.name.len() + 1..];
                    let value = &after_name[..after_name.iter().position(|&b| b == b'|').expect("type delimiter")];
                    prop_assert_eq!(value.split(|&b| b == b':').count(), count);
                }
            }
        }
    }
}
