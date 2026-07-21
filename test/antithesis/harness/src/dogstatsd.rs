//! Datadog-Agent-normative `DogStatsD` payload classification.
//!
//! [`is_malformed`] reports the first segment of a `DogStatsD` payload that the normative
//! Datadog Agent parser (`comp/dogstatsd/server/impl`) would DROP rather than forward.
//! The Agent is the differential's reference lane, so a segment it drops produces no
//! context even on the normative side. The load generator and the differential oracle use
//! this to know which segments legitimately do not survive.
//!
//! [`is_malformed_line`] classifies a single already-framed line against the Agent's
//! `parseMetricSample` M1-M6 rules. [`is_malformed`] frames the payload the way the Agent
//! does (`server.go` `scanLines`/`nextMessage`/`dropCR`) and applies the line rule to
//! each surviving segment.
//!
//! Malformed is the Agent's behavior, NOT ADP's and NOT the backend intake's. The Agent
//! does no UTF-8 or charset validation; it forwards non-UTF-8 and exotic names/tags
//! verbatim.

/// A malformed segment located within a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformity {
    /// 0-based index of the offending segment among the payload's `\n`-split segments.
    /// Blank segments are counted in the index but are skipped and never malformed.
    pub line: usize,
}

/// The first segment of a `DogStatsD` payload the Datadog Agent would drop, or `None` when
/// every segment forwards.
///
/// Frames the payload like the Agent (`server.go` `scanLines`/`nextMessage`/`dropCR`):
/// split on `\n`, drop a single trailing `\r` from each segment so `\r\n` and `\n` both
/// work, and skip empty segments (a blank line is never malformed). The returned index
/// counts every segment, blanks included. An empty or all-blank payload yields `None`.
///
/// The eol-unterminated final-line drop is out of scope: the default
/// `dogstatsd_eol_required=[]` leaves eol termination off and the generator always
/// `\n`-terminates.
#[must_use]
pub fn is_malformed(payload: &[u8]) -> Option<Malformity> {
    for (line, segment) in payload.split(|&b| b == b'\n').enumerate() {
        let segment = segment.strip_suffix(b"\r").unwrap_or(segment);
        if segment.is_empty() {
            continue;
        }
        if is_malformed_line(segment) {
            return Some(Malformity { line });
        }
    }
    None
}

/// Whether the Datadog Agent parser would drop this single framed `DogStatsD` line
/// (malformed) rather than forward it (well-formed).
///
/// Operates on raw bytes; the Agent never validates UTF-8.
///
/// A line beginning `_e{` or `_sc` is an event or service check, not a metric, so it
/// never forwards as a metric and counts as malformed here. Otherwise the line is
/// malformed when ANY of these hold:
///
/// - M1: it carries no `|`.
/// - M2: field-0 (up to the first `|`) carries no `:`.
/// - M3: the name (field-0 up to its first `:`) or the value (the rest of field-0) is empty.
/// - M4: the type field is not byte-exactly one of `g` `c` `h` `d` `s` `ms`.
/// - M5: for a non-set type, a value segment fails the Go-float parse (or none survive).
/// - M6: an `@` chunk fails the Go-float parse, or a `T` chunk fails the Go-int parse or is `< 1`.
#[must_use]
pub fn is_malformed_line(line: &[u8]) -> bool {
    // Precondition: events and service checks are routed away from the metric parser.
    if line.starts_with(b"_e{") || line.starts_with(b"_sc") {
        return true;
    }

    let mut parts = line.split(|&b| b == b'|');
    let Some(field0) = parts.next() else {
        return true;
    };
    // M1: a metric needs at least one `|`, so a second split part must exist.
    let Some(type_field) = parts.next() else {
        return true;
    };

    // M2: field-0 must carry a `:` splitting name from value.
    let Some(colon) = field0.iter().position(|&b| b == b':') else {
        return true;
    };
    let name = &field0[..colon];
    let value = &field0[colon + 1..];
    // M3: name and value are both non-empty.
    if name.is_empty() || value.is_empty() {
        return true;
    }

    // M4 and M5: the type must match exactly, and a non-set value must parse.
    match type_field {
        b"s" => {}
        b"g" | b"c" | b"h" | b"d" | b"ms" => {
            if value_is_malformed(value) {
                return true;
            }
        }
        _ => return true,
    }

    // M6: sample-rate and timestamp chunks must parse; everything else is skipped.
    for chunk in parts {
        if let Some((&first, rest)) = chunk.split_first() {
            if first == b'@' && !go_parse_float_ok(rest) {
                return true;
            }
            if first == b'T' && !go_parse_int_ge_one(rest) {
                return true;
            }
        }
    }

    false
}

/// M5 for a non-set type: a value carrying `:` splits into colon segments, empty
/// segments are discarded, and the value is malformed when any surviving segment fails
/// the Go-float parse or when no segment survives. A value with no `:` is malformed when
/// the whole value fails the Go-float parse.
fn value_is_malformed(value: &[u8]) -> bool {
    if value.contains(&b':') {
        let mut survived = 0usize;
        for seg in value.split(|&b| b == b':') {
            if seg.is_empty() {
                continue;
            }
            survived += 1;
            if !go_parse_float_ok(seg) {
                return true;
            }
        }
        survived == 0
    } else {
        !go_parse_float_ok(value)
    }
}

/// Whether Go's `strconv.ParseFloat(s, 64)` would accept these bytes without error.
///
/// This mirrors Go, not Rust: it accepts hex-floats (`0x1p-2`), underscore digit
/// separators (`1_000`), and the unsigned specials `nan`/`inf`/`infinity` (with an
/// optional sign on the infinities), and it rejects `+nan`/`-nan` and any finite
/// decimal that overflows f64. Underflow to `0.0` is not an error.
fn go_parse_float_ok(s: &[u8]) -> bool {
    if s.is_empty() {
        return false;
    }
    if is_special_float(s) {
        return true;
    }
    let Some(scan) = scan_float(s) else {
        return false;
    };
    if scan.hex {
        !hex_value_overflows(s)
    } else {
        decimal_is_finite(s)
    }
}

/// Whether the bytes match one of Go's special float tokens: `inf`/`infinity` with an
/// optional sign, or `nan` with no sign, all case-insensitive. Go rejects `+nan`/`-nan`.
fn is_special_float(s: &[u8]) -> bool {
    let (rest, signed) = match s.first() {
        Some(b'+' | b'-') => (&s[1..], true),
        _ => (s, false),
    };
    let eq_ci = |token: &[u8]| {
        rest.len() == token.len() && rest.iter().zip(token).all(|(&a, &b)| (a | 0x20) == b)
    };
    if eq_ci(b"inf") || eq_ci(b"infinity") {
        return true;
    }
    !signed && eq_ci(b"nan")
}

/// Result of a successful Go-float syntax scan.
#[derive(Clone, Copy, Debug)]
struct FloatScan {
    hex: bool,
}

/// Scan the bytes as a Go decimal or hex float, returning `Some` only when the whole
/// slice is consumed as a syntactically valid number. This mirrors Go's `readFloat`
/// plus `underscoreOK`; it does not decide magnitude overflow.
fn scan_float(s: &[u8]) -> Option<FloatScan> {
    if !underscore_ok(s) {
        return None;
    }
    let n = s.len();
    let mut i = 0usize;

    if i < n && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }

    // Go enters hex mode only when at least one byte follows the `0x` prefix.
    let mut hex = false;
    let mut exp_char = b'e';
    if i + 2 < n && s[i] == b'0' && (s[i + 1] | 0x20) == b'x' {
        hex = true;
        exp_char = b'p';
        i += 2;
    }

    let mut saw_digits = false;
    let mut saw_dot = false;
    while i < n {
        let c = s[i];
        if c == b'_' {
            i += 1;
            continue;
        }
        if c == b'.' {
            if saw_dot {
                break;
            }
            saw_dot = true;
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            saw_digits = true;
            i += 1;
            continue;
        }
        if hex && (b'a'..=b'f').contains(&(c | 0x20)) {
            saw_digits = true;
            i += 1;
            continue;
        }
        break;
    }
    if !saw_digits {
        return None;
    }

    if i < n && (s[i] | 0x20) == exp_char {
        i += 1;
        if i < n && (s[i] == b'+' || s[i] == b'-') {
            i += 1;
        }
        if i >= n || !s[i].is_ascii_digit() {
            return None;
        }
        while i < n {
            let c = s[i];
            if c == b'_' || c.is_ascii_digit() {
                i += 1;
            } else {
                break;
            }
        }
    } else if hex {
        // A hex float requires a binary `p` exponent.
        return None;
    }

    if i != n {
        return None;
    }
    Some(FloatScan { hex })
}

/// Whether the underscores in `s` sit only between digits or between a base prefix and a
/// digit, mirroring Go's `underscoreOK`.
fn underscore_ok(s: &[u8]) -> bool {
    if !s.contains(&b'_') {
        return true;
    }
    // States: `^` start, `0` digit or base prefix, `_` underscore, `!` other.
    let mut saw = b'^';
    let n = s.len();
    let mut i = 0usize;

    if n >= 1 && (s[0] == b'-' || s[0] == b'+') {
        i = 1;
    }

    let mut hex = false;
    if n - i >= 2 && s[i] == b'0' {
        let lc = s[i + 1] | 0x20;
        if lc == b'b' || lc == b'o' || lc == b'x' {
            saw = b'0';
            hex = lc == b'x';
            i += 2;
        }
    }

    while i < n {
        let c = s[i];
        if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&(c | 0x20))) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
        i += 1;
    }
    saw != b'_'
}

/// Whether a Go-syntactically-valid decimal float has finite magnitude. Go returns
/// `ErrRange` when a finite decimal overflows to infinity; Rust's parser surfaces the
/// same overflow as an infinite result, so a finite parse means Go accepts it.
fn decimal_is_finite(s: &[u8]) -> bool {
    let parsed = if s.contains(&b'_') {
        let cleaned: Vec<u8> = s.iter().copied().filter(|&b| b != b'_').collect();
        parse_ascii_f64(&cleaned)
    } else {
        parse_ascii_f64(s)
    };
    match parsed {
        Some(v) => v.is_finite(),
        // Go accepted the syntax; anything Rust cannot re-parse here is a small-magnitude
        // form (e.g. a trailing-dot mantissa), never an overflow.
        None => true,
    }
}

/// Parse ASCII bytes as an f64, or `None` when they are not valid UTF-8 or not a Rust float.
fn parse_ascii_f64(s: &[u8]) -> Option<f64> {
    simdutf8::basic::from_utf8(s).ok()?.parse::<f64>().ok()
}

/// Whether a Go-syntactically-valid hex float overflows f64.
///
/// This mirrors Go's `readFloat`/`atofHex`: fold at most 16 significant hex digits
/// (>= 64 bits) into the mantissa and track the hex-point position. Integer digits beyond
/// the cap grow the binary exponent rather than the mantissa, and fractional digits beyond
/// the cap are dropped. The binary exponent is `(point_digits - mantissa_digits) * 4`
/// plus the `p` exponent, so the bounded mantissa times `2^exponent` gives the true
/// overflow verdict without relying on f64 saturation. Underflow to zero is not an
/// overflow.
fn hex_value_overflows(s: &[u8]) -> bool {
    const MAX_MANT_HEX_DIGITS: i64 = 16;

    let len = s.len();
    let mut i = 0usize;
    if i < len && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    i += 2; // Skip the validated `0x` prefix.

    let mut mantissa: f64 = 0.0;
    let mut nd: i64 = 0; // Significant digits seen; drives the hex-point position.
    let mut nd_mant: i64 = 0; // Digits folded into the mantissa (capped).
    let mut dp: i64 = 0; // Significant digits before the hex point.
    let mut saw_dot = false;
    while i < len {
        let byte = s[i];
        if byte == b'_' {
            i += 1;
            continue;
        }
        if byte == b'.' {
            if saw_dot {
                break;
            }
            saw_dot = true;
            dp = nd;
            i += 1;
            continue;
        }
        let digit: u8 = if byte.is_ascii_digit() {
            byte - b'0'
        } else {
            let lower = byte | 0x20;
            if (b'a'..=b'f').contains(&lower) {
                lower - b'a' + 10
            } else {
                break;
            }
        };
        // Leading zeros shift the point but never enter the mantissa.
        if digit == 0 && nd == 0 {
            dp -= 1;
            i += 1;
            continue;
        }
        nd += 1;
        if nd_mant < MAX_MANT_HEX_DIGITS {
            mantissa = mantissa * 16.0 + f64::from(digit);
            nd_mant += 1;
        }
        i += 1;
    }
    // No digit folded into the mantissa means the value is zero, never an overflow.
    if nd_mant == 0 {
        return false;
    }
    if !saw_dot {
        dp = nd;
    }
    // Count in bits.
    dp *= 4;
    nd_mant *= 4;

    i += 1; // Skip the `p`/`P` exponent marker.
    let mut esign: i64 = 1;
    if i < len && (s[i] == b'+' || s[i] == b'-') {
        if s[i] == b'-' {
            esign = -1;
        }
        i += 1;
    }
    let mut exp: i64 = 0;
    while i < len {
        let byte = s[i];
        if byte == b'_' {
            i += 1;
        } else if byte.is_ascii_digit() {
            exp = exp.saturating_mul(10).saturating_add(i64::from(byte - b'0'));
            i += 1;
        } else {
            break;
        }
    }
    dp = dp.saturating_add(esign.saturating_mul(exp));

    let total = dp.saturating_sub(nd_mant);
    if total > 1100 {
        return true;
    }
    if total < -1200 {
        return false;
    }
    let Ok(total_i32) = i32::try_from(total) else {
        return true;
    };
    (mantissa * 2f64.powi(total_i32)).is_infinite()
}

/// Whether Go's `strconv.ParseInt(s, 10, 0)` would accept these bytes AND the value is
/// `>= 1`, which is what the Agent requires of a `DogStatsD` timestamp. An optional sign is
/// allowed; underscores are not (base 10 forbids them); overflow is an error.
fn go_parse_int_ge_one(s: &[u8]) -> bool {
    let (neg, digits) = match s.first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return false;
    }
    let mut acc: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return false;
        }
        acc = match acc.checked_mul(10).and_then(|v| v.checked_add(i64::from(c - b'0'))) {
            Some(acc) => acc,
            None => return false,
        };
    }
    // A negative value is always below the `>= 1` floor, so its exact magnitude never matters.
    !neg && acc >= 1
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{go_parse_float_ok, go_parse_int_ge_one, is_malformed, is_malformed_line, Malformity};

    #[test]
    fn well_formed_lines_forward() {
        // Non-UTF-8 and exotic bytes forward verbatim; the Agent does no charset check.
        let cases: &[&[u8]] = &[
            b"m\xff:1|g",
            b"m:1|g|#t\xff",
            b"my metric:1|g",
            b"m:NaN|d",
            b"m:1|g|@2",
            b"m:1|g|@-1",
            b"m:1|g|@nan",
            b"m:0x1p-2|g",
            b"m:1_000|c",
            b"m:1|c|#",
            b"m:1|c|x:hi",
            b"m:1|c||#a:b",
            b"m:a:b:c|s",
            b"m:1|c",
            b"m:1|ms",
            b"m:1:2:3|d",
            b"m:1::|c",
            b"m:1e-400|g",
            b"m:1|g|T1",
            // Routing: a `_`-prefixed name that is not exactly `_e{`/`_sc` is a metric.
            b"_x:1|g",
            b"_e:1|g",
            b"_s:1|g",
        ];
        for line in cases {
            assert!(!is_malformed_line(line), "expected well-formed: {line:?}");
        }
    }

    #[test]
    fn malformed_lines_drop() {
        let cases: &[&[u8]] = &[
            b"m:1",       // M1: no pipe
            b"m|c",       // M2: no colon in field-0
            b":1|c",      // M3: empty name
            b"m:|c",      // M3: empty value
            b"m:1|count", // M4: type not exact
            b"m:1|",      // M4: empty type
            b"m:1||",     // M4: empty type via double pipe
            b"m:1|G",     // M4: type match is case-sensitive
            b"m:x|c",     // M5: non-numeric value
            b"m:+nan|g",  // M5: Go rejects signed nan
            b"m:1e309|g", // M5: decimal overflow
            b"m:a:b|c",   // M5: a segment fails the float parse
            b"m:::|c",    // M5: no segment survives
            b"m:1|g|@abc", // M6: bad sample rate
            b"m:1|g|Tbad", // M6: bad timestamp
            b"m:1|g|T0",  // M6: timestamp below 1
            b"m:1|g|T-5", // M6: negative timestamp
            b"m:1|g|T99999999999999999999999", // M6: Go ParseInt range error
            b"_e{1,1}:x|y", // routing: event
            b"_scooter:1|g", // routing: service check
        ];
        for line in cases {
            assert!(is_malformed_line(line), "expected malformed: {line:?}");
        }
    }

    #[test]
    fn hex_float_overflow_boundary() {
        assert!(is_malformed_line(b"m:0x1p1024|g")); // Go ErrRange
        assert!(!is_malformed_line(b"m:0x1p1023|g")); // finite
        assert!(!is_malformed_line(b"m:0x1p-2000|g")); // underflow to 0.0
        // A long mantissa must not saturate to Inf before the p-exponent applies:
        // Go parses this as ~1.07e59 with no error.
        let mut long = b"m:0x".to_vec();
        long.resize(long.len() + 300, b'1');
        long.extend_from_slice(b"p-1000|g");
        assert!(!is_malformed_line(&long), "long-mantissa hex must be well-formed");
    }

    #[test]
    fn payload_framing_examples() {
        // Good then bad; the reported index is the bad segment's physical position.
        assert_eq!(is_malformed(b"a:1|g\nb:1|g\nbad\n"), Some(Malformity { line: 2 }));
        // CRLF line endings: the trailing `\r` is dropped before classification.
        assert_eq!(is_malformed(b"a:1|g\r\nbad\r\n"), Some(Malformity { line: 1 }));
        // Blank lines are skipped but still counted in the index.
        assert_eq!(is_malformed(b"\n\na:1|g\nbad"), Some(Malformity { line: 3 }));
        // Leading and trailing blank lines are well-formed.
        assert_eq!(is_malformed(b"\na:1|g\n\n"), None);
        // A single line with no `\n` is still classified.
        assert_eq!(is_malformed(b"bad"), Some(Malformity { line: 0 }));
        assert_eq!(is_malformed(b"a:1|g"), None);
        // Empty and all-blank payloads forward.
        assert_eq!(is_malformed(b""), None);
        assert_eq!(is_malformed(b"\n\n\n"), None);
    }

    #[test]
    fn go_float_accepts_go_specific_forms() {
        for s in [
            "1", "-1", "+1", "1.5", ".5", "5.", "1e5", "1E5", "1e-5", "-1.5e+3", "0x1p-2",
            "0x1.8p3", "0x_1p0", "1_000", "1_000.5", "1e1_0", "nan", "NaN", "inf", "INF", "+inf",
            "-inf", "infinity", "-INFINITY", "1e-400",
        ] {
            assert!(go_parse_float_ok(s.as_bytes()), "expected accept: {s}");
        }
    }

    #[test]
    fn go_float_rejects_what_go_rejects() {
        for s in [
            "", "+nan", "-nan", "NANX", "abc", "infi", "1e309", "-1e309", "0x1", "1_", "_1",
            "1__0", "1_.5", "1e", "1.2.3", "1 ", " 1", "0x", "1p5", "0xg p2",
        ] {
            assert!(!go_parse_float_ok(s.as_bytes()), "expected reject: {s}");
        }
    }

    #[test]
    fn go_int_requires_ge_one() {
        assert!(go_parse_int_ge_one(b"1"));
        assert!(go_parse_int_ge_one(b"+7"));
        assert!(go_parse_int_ge_one(b"9999999999"));
        assert!(!go_parse_int_ge_one(b"0"));
        assert!(!go_parse_int_ge_one(b"-1"));
        assert!(!go_parse_int_ge_one(b""));
        assert!(!go_parse_int_ge_one(b"+"));
        assert!(!go_parse_int_ge_one(b"1_000")); // base 10 forbids underscores
        assert!(!go_parse_int_ge_one(b"1.0"));
        assert!(!go_parse_int_ge_one(b"99999999999999999999999")); // overflow
    }

    fn valid_float_literal() -> impl Strategy<Value = String> {
        prop_oneof![
            (any::<i32>()).prop_map(|n| n.to_string()),
            (any::<i16>(), 0u16..=9999).prop_map(|(a, b)| format!("{a}.{b}")),
            Just("nan".to_string()),
            Just("inf".to_string()),
            Just("0x1p-2".to_string()),
            Just("1_000".to_string()),
        ]
    }

    fn metric_type() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("g"),
            Just("c"),
            Just("h"),
            Just("d"),
            Just("ms"),
            Just("s"),
        ]
    }

    // A name byte that never introduces `:`, `|`, `\n`, or the event/service-check prefixes.
    fn name_bytes() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(
            (0u8..=255).prop_filter("no colon, pipe, or newline", |&b| {
                b != b':' && b != b'|' && b != b'\n' && b != b'\r'
            }),
            1..12,
        )
    }

    proptest! {
        // Parse robustness: the payload classifier must never panic, whatever the bytes.
        #[test]
        fn property_test_never_panics(payload in proptest::collection::vec(any::<u8>(), 0..96)) {
            let _ = is_malformed(&payload);
        }

        // M1: a line with no `|` (and no routing prefix) is always malformed.
        #[test]
        fn property_test_missing_pipe_is_malformed(
            line in proptest::collection::vec(
                (0u8..=255).prop_filter("no pipe", |&b| b != b'|'),
                0..32,
            ).prop_filter("not a routing prefix", |l| {
                !l.starts_with(b"_e{") && !l.starts_with(b"_sc")
            }),
        ) {
            prop_assert!(is_malformed_line(&line));
        }

        // A constructed valid metric always forwards, tags and all.
        #[test]
        fn property_test_well_formed_metric_forwards(
            name in name_bytes().prop_filter("no routing prefix", |n| {
                !n.starts_with(b"_e{") && !n.starts_with(b"_sc")
            }),
            value in valid_float_literal(),
            ty in metric_type(),
            tag in "[a-z]{1,8}",
            with_tag in any::<bool>(),
        ) {
            let mut line = name.clone();
            line.push(b':');
            line.extend_from_slice(value.as_bytes());
            line.push(b'|');
            line.extend_from_slice(ty.as_bytes());
            if with_tag {
                line.extend_from_slice(b"|#");
                line.extend_from_slice(tag.as_bytes());
            }
            prop_assert!(!is_malformed_line(&line), "line={line:?}");
        }

        // M4: swapping in a type outside the exact set drops the line.
        #[test]
        fn property_test_unknown_type_is_malformed(
            name in "[a-z]{1,8}",
            ty in "[a-zA-Z]{1,4}".prop_filter("not a valid type", |t| {
                !matches!(t.as_str(), "g" | "c" | "h" | "d" | "s" | "ms")
            }),
        ) {
            let line = format!("{name}:1|{ty}");
            prop_assert!(is_malformed_line(line.as_bytes()), "line={line}");
        }

        // M3: an empty name or empty value drops the line.
        #[test]
        fn property_test_empty_name_or_value_is_malformed(
            token in "[a-z]{1,8}",
            empty_name in any::<bool>(),
        ) {
            let line = if empty_name {
                format!(":{token}|g")
            } else {
                format!("{token}:|g")
            };
            prop_assert!(is_malformed_line(line.as_bytes()), "line={line}");
        }

        // Payload framing: the reported index is the first bad segment by physical
        // position, blanks are skipped, and `\r\n` and `\n` endings behave alike.
        #[test]
        fn property_test_reports_first_malformed_segment(
            kinds in proptest::collection::vec(0u8..3, 1..8),
            crlf in any::<bool>(),
        ) {
            // 0 = good line, 1 = blank line, 2 = malformed line.
            let eol: &[u8] = if crlf { b"\r\n" } else { b"\n" };
            let mut payload = Vec::new();
            let mut first_bad = None;
            for (idx, &kind) in kinds.iter().enumerate() {
                match kind {
                    0 => payload.extend_from_slice(b"g:1|g"),
                    1 => {}
                    _ => {
                        payload.extend_from_slice(b"bad");
                        if first_bad.is_none() {
                            first_bad = Some(idx);
                        }
                    }
                }
                payload.extend_from_slice(eol);
            }
            let expected = first_bad.map(|line| Malformity { line });
            prop_assert_eq!(is_malformed(&payload), expected);
        }
    }
}
