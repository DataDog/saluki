//! Datadog-Agent-normative `DogStatsD` payload classification.
//!
//! [`is_malformed`] is a predicate on the SERIALIZED datagram bytes a driver emits. It re-parses the
//! datagram the way the Datadog Agent's `DogStatsD` parser at `comp/dogstatsd/server/impl` does. It
//! splits on `\n` and routes each message by prefix: `_e{` to event, `_sc` to service check, else
//! metric. Then it applies that message type's drop rules, and returns `Ok(())` when the Agent
//! forwards every message, or the first [`PayloadError`] it drops on.
//!
//! The Agent is the differential's reference lane, so a message it drops produces no context even on
//! the normative side. Malformed is the Agent's behavior, NOT ADP's and NOT the backend intake's. The
//! Agent does no UTF-8 or charset validation. It forwards non-UTF-8 and exotic names, tags, titles,
//! and text verbatim, so `PayloadError` covers only the hard structural and numeric-parse failures,
//! never content bytes.
//!
//! The `PayloadError` variants are the contract the load generators are written against: the clean
//! generator emits only payloads for which this returns `Ok(())`, and a later malformed generator
//! induces exactly one variant and asserts the Agent drops for it.

/// The first drop rule a serialized `DogStatsD` payload violates, tagged with the 0-based index of
/// the offending message among the payload's `\n`-split segments. Blank segments are counted in the
/// index but are never malformed.
///
/// One variant per Agent drop rule, across all three message types. The metric `T` timestamp rule is
/// intentionally absent. See the note in [`classify_metric`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    /// Metric: the line carries no `|`.
    MetricNoPipe {
        /// Message index.
        line: usize,
    },
    /// Metric: field-0 carries no `:` splitting name from value.
    MetricNameValueNoColon {
        /// Message index.
        line: usize,
    },
    /// Metric: the name is empty.
    MetricEmptyName {
        /// Message index.
        line: usize,
    },
    /// Metric: the value is empty.
    MetricEmptyValue {
        /// Message index.
        line: usize,
    },
    /// Metric: the type is not byte-exactly one of `g` `c` `h` `d` `s` `ms`.
    MetricBadType {
        /// Message index.
        line: usize,
    },
    /// Metric: a non-set value segment fails the Go float parse, or none survives.
    MetricUnparseableValue {
        /// Message index.
        line: usize,
    },
    /// Metric: an `@` sample-rate chunk fails the Go float parse.
    MetricBadRate {
        /// Message index.
        line: usize,
    },
    /// Event: the line carries no `:` splitting header from body.
    EventNoColon {
        /// Message index.
        line: usize,
    },
    /// Event: the header is shorter than the minimal `_e{1,0}`.
    EventHeaderTooShort {
        /// Message index.
        line: usize,
    },
    /// Event: the header carries no `,` between the two lengths.
    EventNoLengthComma {
        /// Message index.
        line: usize,
    },
    /// Event: the title length is not a non-negative integer.
    EventBadTitleLen {
        /// Message index.
        line: usize,
    },
    /// Event: the title length is zero.
    EventEmptyTitle {
        /// Message index.
        line: usize,
    },
    /// Event: the text length is not a non-negative integer.
    EventBadTextLen {
        /// Message index.
        line: usize,
    },
    /// Event: `title_len + 1 + text_len` overflows.
    EventLengthOverflow {
        /// Message index.
        line: usize,
    },
    /// Event: the body is shorter than the declared `title_len + 1 + text_len`.
    EventBodyTooShort {
        /// Message index.
        line: usize,
    },
    /// Service check: fewer than two `|`.
    ServiceCheckTooFewPipes {
        /// Message index.
        line: usize,
    },
    /// Service check: shorter than the four-byte `_sc|` header.
    ServiceCheckTooShort {
        /// Message index.
        line: usize,
    },
    /// Service check: the name is empty.
    ServiceCheckEmptyName {
        /// Message index.
        line: usize,
    },
    /// Service check: the status is not byte-exactly one of `0` `1` `2` `3`.
    ServiceCheckBadStatus {
        /// Message index.
        line: usize,
    },
}

/// Whether the Datadog Agent would forward the whole serialized `DogStatsD` payload.
///
/// Frames the payload like the Agent's `server.go` `scanLines`, `nextMessage`, and `dropCR`. It
/// splits on `\n`, drops a single trailing `\r` from each segment so `\r\n` and `\n` both work, and
/// skips empty segments, since a blank line is never malformed. The returned index counts every
/// segment, blanks included. An empty or all-blank payload is `Ok(())`.
///
/// The eol-unterminated final-line drop is out of scope: the default `dogstatsd_eol_required=[]`
/// leaves eol termination off and the generator always `\n`-terminates.
///
/// # Errors
///
/// Returns the first [`PayloadError`] the Agent would drop on, or `Ok(())` when every message
/// forwards.
pub fn is_malformed(payload: &[u8]) -> Result<(), PayloadError> {
    for (line, segment) in payload.split(|&b| b == b'\n').enumerate() {
        let segment = segment.strip_suffix(b"\r").unwrap_or(segment);
        if segment.is_empty() {
            continue;
        }
        if segment.starts_with(b"_e{") {
            classify_event(line, segment)?;
        } else if segment.starts_with(b"_sc") {
            classify_service_check(line, segment)?;
        } else {
            classify_metric(line, segment)?;
        }
    }
    Ok(())
}

/// Apply the Agent `parseMetricSample` M1-M6 drop rules to a single metric line.
fn classify_metric(line: usize, seg: &[u8]) -> Result<(), PayloadError> {
    let mut parts = seg.split(|&b| b == b'|');
    let field0 = parts.next().unwrap_or(&[]);
    // M1: a metric needs at least one `|`, so a second split part must exist.
    let Some(type_field) = parts.next() else {
        return Err(PayloadError::MetricNoPipe { line });
    };
    // M2: field-0 must carry a `:` splitting name from value.
    let Some(colon) = field0.iter().position(|&b| b == b':') else {
        return Err(PayloadError::MetricNameValueNoColon { line });
    };
    let name = &field0[..colon];
    let value = &field0[colon + 1..];
    // M3: name and value are both non-empty.
    if name.is_empty() {
        return Err(PayloadError::MetricEmptyName { line });
    }
    if value.is_empty() {
        return Err(PayloadError::MetricEmptyValue { line });
    }
    // M4 and M5: the type must match exactly, and a non-set value must parse.
    match type_field {
        b"s" => {}
        b"g" | b"c" | b"h" | b"d" | b"ms" => {
            if value_is_malformed(value) {
                return Err(PayloadError::MetricUnparseableValue { line });
            }
        }
        _ => return Err(PayloadError::MetricBadType { line }),
    }
    // M6: a sample-rate `@` chunk must parse as a Go float. Everything else is skipped.
    //
    // The `T` timestamp chunk is deliberately NOT modeled: it drops only when readTimestamps, gated
    // by `dogstatsd_no_aggregation_pipeline`, is on and the value is not an integer >= 1, a
    // config-gated surface this generator does not emit. The origin chunks c:/e:/card: never drop.
    for chunk in parts {
        if let Some((&first, rest)) = chunk.split_first() {
            if first == b'@' && !go_parse_float_ok(rest) {
                return Err(PayloadError::MetricBadRate { line });
            }
        }
    }
    Ok(())
}

/// Apply the Agent `parseEvent` header drop rules to a single `_e{...}` line. The body and every
/// optional field forward verbatim, so only the header can drop.
fn classify_event(line: usize, seg: &[u8]) -> Result<(), PayloadError> {
    let Some(colon) = seg.iter().position(|&b| b == b':') else {
        return Err(PayloadError::EventNoColon { line });
    };
    let header = &seg[..colon];
    let body = &seg[colon + 1..];
    // The minimal header is `_e{1,0}`, seven bytes. `_e{` and the closing byte are stripped by
    // position. The closing `}` is assumed, never validated.
    if header.len() < 7 {
        return Err(PayloadError::EventHeaderTooShort { line });
    }
    let raw_lengths = &header[3..header.len() - 1];
    let Some(comma) = raw_lengths.iter().position(|&b| b == b',') else {
        return Err(PayloadError::EventNoLengthComma { line });
    };
    let raw_title_len = &raw_lengths[..comma];
    let raw_text_len = &raw_lengths[comma + 1..];
    let Some(title_len) = atoi(raw_title_len).filter(|&len| len >= 0) else {
        return Err(PayloadError::EventBadTitleLen { line });
    };
    if title_len == 0 {
        return Err(PayloadError::EventEmptyTitle { line });
    }
    let Some(text_len) = atoi(raw_text_len).filter(|&len| len >= 0) else {
        return Err(PayloadError::EventBadTextLen { line });
    };
    // Both lengths are non-negative here, so the magnitude is the value. Go widens them to uint for
    // the framing math below.
    let (title_len, text_len) = (title_len.unsigned_abs(), text_len.unsigned_abs());
    // Go frames the body as title_len + 1, the title/text `|`, plus text_len, computed on uint64.
    let Some(content_len) = title_len.checked_add(1).and_then(|v| v.checked_add(text_len)) else {
        return Err(PayloadError::EventLengthOverflow { line });
    };
    let Ok(body_len) = u64::try_from(body.len()) else {
        // A body longer than u64::MAX cannot be too short.
        return Ok(());
    };
    if body_len < content_len {
        return Err(PayloadError::EventBodyTooShort { line });
    }
    Ok(())
}

/// Parse an event length exactly as Go's `strconv.Atoi` does: an optional leading sign, decimal
/// digits, no underscores, overflow beyond the 64-bit `int` is a failure. Returns the signed value so
/// the caller applies the Agent's own `< 0` test rather than treating the sign byte as the verdict.
/// That is why `-0` parses as a valid zero-length field: Go parses it to zero, and the Agent accepts.
fn atoi(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: i128 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + i128::from(c - b'0');
        if acc > i128::from(u64::MAX) {
            return None;
        }
    }
    let value = if neg { -acc } else { acc };
    i64::try_from(value).ok()
}

/// Apply the Agent `parseServiceCheck` drop rules to a single `_sc` line.
fn classify_service_check(line: usize, seg: &[u8]) -> Result<(), PayloadError> {
    // The parser needs a name terminator and a status terminator, so at least two `|`.
    let two_pipes = seg
        .iter()
        .position(|&b| b == b'|')
        .is_some_and(|i| seg[i + 1..].contains(&b'|'));
    if !two_pipes {
        return Err(PayloadError::ServiceCheckTooFewPipes { line });
    }
    // The parser strips a four-byte `_sc|` header by position.
    if seg.len() < 4 {
        return Err(PayloadError::ServiceCheckTooShort { line });
    }
    let mut fields = seg[4..].split(|&b| b == b'|');
    let name = fields.next().unwrap_or(&[]);
    if name.is_empty() {
        return Err(PayloadError::ServiceCheckEmptyName { line });
    }
    let status = fields.next().unwrap_or(&[]);
    if !matches!(status, b"0" | b"1" | b"2" | b"3") {
        return Err(PayloadError::ServiceCheckBadStatus { line });
    }
    Ok(())
}

/// M5 for a non-set type: a value carrying `:` splits into colon segments, empty segments are
/// discarded, and the value is malformed when any surviving segment fails the Go-float parse or when
/// no segment survives. A value with no `:` is malformed when the whole value fails the Go-float
/// parse.
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
/// This mirrors Go, not Rust. It accepts hex-floats such as `0x1p-2`, underscore digit separators
/// such as `1_000`, and the unsigned specials `nan`, `inf`, and `infinity` with an optional sign on
/// the infinities. It rejects `+nan`/`-nan` and any finite decimal that overflows f64. Underflow to
/// `0.0` is not an error.
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

/// Whether the bytes match one of Go's special float tokens: `inf`/`infinity` with an optional sign,
/// or `nan` with no sign, all case-insensitive. Go rejects `+nan`/`-nan`.
fn is_special_float(s: &[u8]) -> bool {
    let (rest, signed) = match s.first() {
        Some(b'+' | b'-') => (&s[1..], true),
        _ => (s, false),
    };
    let eq_ci = |token: &[u8]| rest.len() == token.len() && rest.iter().zip(token).all(|(&a, &b)| (a | 0x20) == b);
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

/// Scan the bytes as a Go decimal or hex float, returning `Some` only when the whole slice is
/// consumed as a syntactically valid number. This mirrors Go's `readFloat` plus `underscoreOK`. It
/// does not decide magnitude overflow.
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

/// Whether the underscores in `s` sit only between digits or between a base prefix and a digit,
/// mirroring Go's `underscoreOK`.
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

/// Whether a Go-syntactically-valid decimal float has finite magnitude. Go returns `ErrRange` when a
/// finite decimal overflows to infinity. Rust's parser surfaces the same overflow as an infinite
/// result, so a finite parse means Go accepts it.
fn decimal_is_finite(s: &[u8]) -> bool {
    let parsed = if s.contains(&b'_') {
        let cleaned: Vec<u8> = s.iter().copied().filter(|&b| b != b'_').collect();
        parse_ascii_f64(&cleaned)
    } else {
        parse_ascii_f64(s)
    };
    match parsed {
        Some(v) => v.is_finite(),
        // Go accepted the syntax. Anything Rust cannot re-parse here is a small-magnitude form such
        // as a trailing-dot mantissa, never an overflow.
        None => true,
    }
}

/// Parse ASCII bytes as an f64, or `None` when they are not valid UTF-8 or not a Rust float.
fn parse_ascii_f64(s: &[u8]) -> Option<f64> {
    simdutf8::basic::from_utf8(s).ok()?.parse::<f64>().ok()
}

/// Whether a Go-syntactically-valid hex float overflows f64.
///
/// This mirrors Go's `readFloat`/`atofHex`. It folds at most 16 significant hex digits, at least 64
/// bits, into the mantissa and tracks the hex-point position. Integer digits beyond the cap grow the
/// binary exponent rather than the mantissa, and fractional digits beyond the cap are dropped. The
/// binary exponent is `(point_digits - mantissa_digits) * 4` plus the `p` exponent, so the bounded
/// mantissa times `2^exponent` gives the true overflow verdict without relying on f64 saturation.
/// Underflow to zero is not an overflow.
fn hex_value_overflows(s: &[u8]) -> bool {
    const MAX_MANT_HEX_DIGITS: i64 = 16;

    let len = s.len();
    let mut i = 0usize;
    if i < len && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    i += 2; // Skip the validated `0x` prefix.

    let mut mantissa: f64 = 0.0;
    let mut nd: i64 = 0; // Significant digits seen. Drives the hex-point position.
    let mut nd_mant: i64 = 0; // Digits folded into the mantissa, capped.
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{is_malformed, PayloadError};

    // --- metrics ---

    #[test]
    fn metric_well_formed_forwards() {
        // Covers basic, set with an unparsed value, multi-value packed, special values, @rate, tags
        // and origin chunks with delimiters, and a non-UTF-8 name. All forward through the lenient
        // Agent.
        for line in [
            &b"m:1|c"[..],
            b"m:1|g",
            b"m:1|ms",
            b"m:1|h",
            b"m:1|d",
            b"m:anything goes|s",
            b"m:1:2:3|d",
            b"m:nan|g",
            b"m:inf|g",
            b"m:-inf|g",
            b"m:0x1p4|g",
            b"m:1_000|g",
            b"m:1.|g",
            b"m:.5|g",
            b"m:1|c|@0.5",
            b"m:1|c|@nan",
            b"m:1|c|#a:b,c:d",
            b"m:1|c|c:cid-deadbeef",
            b"m:1|c|card:high",
            b"na\xff\x00me:1|c",
        ] {
            assert_eq!(is_malformed(line), Ok(()), "expected forward: {line:?}");
        }
    }

    #[test]
    fn metric_drop_rules() {
        assert_eq!(is_malformed(b"m:1"), Err(PayloadError::MetricNoPipe { line: 0 }));
        assert_eq!(
            is_malformed(b"noColon|g"),
            Err(PayloadError::MetricNameValueNoColon { line: 0 })
        );
        assert_eq!(is_malformed(b":1|g"), Err(PayloadError::MetricEmptyName { line: 0 }));
        assert_eq!(is_malformed(b"m:|g"), Err(PayloadError::MetricEmptyValue { line: 0 }));
        assert_eq!(is_malformed(b"m:1|gg"), Err(PayloadError::MetricBadType { line: 0 }));
        assert_eq!(is_malformed(b"m:1|"), Err(PayloadError::MetricBadType { line: 0 }));
        assert_eq!(
            is_malformed(b"m:notafloat|g"),
            Err(PayloadError::MetricUnparseableValue { line: 0 })
        );
        assert_eq!(
            is_malformed(b"m:::|d"),
            Err(PayloadError::MetricUnparseableValue { line: 0 })
        );
        assert_eq!(
            is_malformed(b"m:1|c|@bad"),
            Err(PayloadError::MetricBadRate { line: 0 })
        );
    }

    // --- events ---

    #[test]
    fn event_well_formed_forwards() {
        for line in [
            &b"_e{1,0}:a|"[..],
            b"_e{5,4}:title|text",
            b"_e{5,0}:title|",
            b"_e{5,4}:title|text|h:host|k:key|p:normal|t:error|s:src|#a:b",
            b"_e{5,4}:title|text|p:bogus|t:bogus|d:notanint", // bad optional fields still forward
            b"_e{5,4}:title|text|extra bytes past declared body", // body longer than declared is fine
            b"_e{5,4}:t\xff\x00le|te\xffxt",                  // non-UTF-8 title/text forward
        ] {
            assert_eq!(is_malformed(line), Ok(()), "expected forward: {line:?}");
        }
    }

    #[test]
    fn event_drop_rules() {
        assert_eq!(
            is_malformed(b"_e{5,4}title|text"),
            Err(PayloadError::EventNoColon { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_e{}:x"),
            Err(PayloadError::EventHeaderTooShort { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_e{500}:title"),
            Err(PayloadError::EventNoLengthComma { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_e{x,0}:title"),
            Err(PayloadError::EventBadTitleLen { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_e{0,0}:"),
            Err(PayloadError::EventEmptyTitle { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_e{5,x}:title"),
            Err(PayloadError::EventBadTextLen { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_e{5,4}:t"),
            Err(PayloadError::EventBodyTooShort { line: 0 })
        );
    }

    // Go parses the header lengths with `strconv.Atoi` and drops only on `< 0`, so a signed zero is a
    // valid zero-length field the Agent forwards. Rejecting it would hide an Agent-accepted line from
    // the state search.
    #[test]
    fn event_signed_zero_length_matches_atoi() {
        assert_eq!(is_malformed(b"_e{1,-0}:a|"), Ok(()));
        assert_eq!(is_malformed(b"_e{1,-00}:a|"), Ok(()));
        // A genuinely negative length still drops, as `textLength < 0` does in the Agent.
        assert_eq!(
            is_malformed(b"_e{1,-1}:a|"),
            Err(PayloadError::EventBadTextLen { line: 0 })
        );
        // A signed-zero title parses to zero, so the Agent's own empty-title check is what drops it.
        assert_eq!(
            is_malformed(b"_e{-0,0}:"),
            Err(PayloadError::EventEmptyTitle { line: 0 })
        );
    }

    // --- service checks ---

    #[test]
    fn service_check_well_formed_forwards() {
        for line in [
            &b"_sc|name|0"[..],
            b"_sc|name|1",
            b"_sc|name|2",
            b"_sc|name|3",
            b"_sc|name|0|h:host|#a:b|m:message text",
            b"_sc|na\xffme|0", // non-UTF-8 name forwards
        ] {
            assert_eq!(is_malformed(line), Ok(()), "expected forward: {line:?}");
        }
    }

    #[test]
    fn service_check_drop_rules() {
        assert_eq!(
            is_malformed(b"_sc|nameonly"),
            Err(PayloadError::ServiceCheckTooFewPipes { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_sc"),
            Err(PayloadError::ServiceCheckTooFewPipes { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_sc||0"),
            Err(PayloadError::ServiceCheckEmptyName { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_sc|name|9"),
            Err(PayloadError::ServiceCheckBadStatus { line: 0 })
        );
        assert_eq!(
            is_malformed(b"_sc|name|"),
            Err(PayloadError::ServiceCheckBadStatus { line: 0 })
        );
    }

    // --- framing / routing ---

    #[test]
    fn framing_skips_blanks_and_reports_first_offending_line() {
        // Blank lines are counted in the index but never malformed. The first bad message wins.
        let payload = b"m:1|c\n\n\nbad|line\nm:2|c";
        assert_eq!(
            is_malformed(payload),
            Err(PayloadError::MetricNameValueNoColon { line: 3 })
        );
    }

    #[test]
    fn framing_handles_crlf_and_empty_payload() {
        assert_eq!(is_malformed(b"m:1|c\r\nm:2|g\r\n"), Ok(()));
        assert_eq!(is_malformed(b""), Ok(()));
        assert_eq!(is_malformed(b"\n\n"), Ok(()));
    }

    #[test]
    fn routing_by_prefix() {
        // `_e` without `{` and any gibberish route to the metric parser.
        assert_eq!(is_malformed(b"_e:1|g"), Ok(()));
        assert_eq!(is_malformed(b"gibberish"), Err(PayloadError::MetricNoPipe { line: 0 }));
    }

    // --- Go strconv parity port ---

    #[test]
    fn go_float_accepts_go_specific_forms() {
        // Each rides in as a metric value: forwarded iff Go ParseFloat accepts it.
        for value in [
            &b"0x1p-2"[..],
            b"1_000",
            b"nan",
            b"inf",
            b"infinity",
            b"+inf",
            b"-inf",
            b"1.",
            b".5",
            b"0x1.8p3",
        ] {
            let mut line = b"m:".to_vec();
            line.extend_from_slice(value);
            line.extend_from_slice(b"|g");
            assert_eq!(is_malformed(&line), Ok(()), "expected forward for value {value:?}");
        }
    }

    #[test]
    fn go_float_rejects_what_go_rejects() {
        for value in [
            &b"+nan"[..],
            b"-nan",
            b"0x1",
            b"_1",
            b"1_",
            b"1__0",
            b"1e",
            b"1e+",
            b"abc",
        ] {
            let mut line = b"m:".to_vec();
            line.extend_from_slice(value);
            line.extend_from_slice(b"|g");
            assert_eq!(
                is_malformed(&line),
                Err(PayloadError::MetricUnparseableValue { line: 0 }),
                "expected drop for value {value:?}"
            );
        }
    }

    #[test]
    fn hex_float_overflow_boundary() {
        // 0x1p1023 is finite, 0x1p1024 overflows f64.
        assert_eq!(is_malformed(b"m:0x1p1023|g"), Ok(()));
        assert_eq!(
            is_malformed(b"m:0x1p1024|g"),
            Err(PayloadError::MetricUnparseableValue { line: 0 })
        );
    }

    proptest! {
        // The predicate must be total over arbitrary bytes: never panic, whatever the input.
        #[test]
        fn property_test_never_panics(payload in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = is_malformed(&payload);
        }
    }
}
