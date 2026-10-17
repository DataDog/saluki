//! Go `strconv` boolean, integer, and float parsing.
//!
//! A small, dependency-free port of the parts of Go's [`strconv`][strconv] package that turn strings into booleans,
//! integers, and floats. Configuration systems written in Go commonly coerce string values, such as environment
//! variables or quoted YAML scalars, through these functions (often via [spf13/cast][cast]), so a Rust program that
//! reads the same configuration has to accept exactly the same spellings to agree with it: `0x10` is sixteen, `010`
//! is eight, `1_000` is one thousand, and `0x1p-2` is one quarter. This crate is the single owner of those grammars so
//! they aren't approximated in each place that needs them.
//!
//! - [`parse_bool`] ports `strconv.ParseBool`.
//! - [`parse_int`] ports `strconv.ParseInt(s, 0, 64)`: the base is implied by the prefix, and underscores may separate
//!   digits.
//! - [`parse_float`] ports `strconv.ParseFloat(s, 64)`, including hexadecimal floats, underscores, and the special
//!   values `Inf`, `Infinity`, and `NaN`.
//!
//! As in Go, the input is never trimmed: surrounding whitespace is a syntax error.
//!
//! [strconv]: https://pkg.go.dev/strconv
//! [cast]: https://github.com/spf13/cast
#![deny(warnings)]
#![deny(missing_docs)]

use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// Why a string was rejected, mirroring Go's `strconv.ErrSyntax` and `strconv.ErrRange`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseErrorKind {
    /// The string is not a well-formed value of the requested type.
    Syntax,
    /// The string is well-formed, but its value does not fit the requested type.
    Range,
}

/// Error returned when a string can't be parsed, mirroring Go's `strconv.NumError`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    func: &'static str,
    input: String,
    kind: ParseErrorKind,
}

impl ParseError {
    fn new(func: &'static str, input: &str, kind: ParseErrorKind) -> Self {
        Self {
            func,
            input: input.to_string(),
            kind,
        }
    }

    /// Returns why the string was rejected.
    pub fn kind(&self) -> ParseErrorKind {
        self.kind
    }

    /// Returns the rejected string.
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            ParseErrorKind::Syntax => "invalid syntax",
            ParseErrorKind::Range => "value out of range",
        };
        write!(f, "strconv.{}: parsing {:?}: {}", self.func, self.input, reason)
    }
}

impl Error for ParseError {}

/// Parses a boolean exactly as Go's `strconv.ParseBool` does.
///
/// Accepts `1`, `t`, `T`, `TRUE`, `true`, and `True` as true, and `0`, `f`, `F`, `FALSE`, `false`, and `False` as
/// false. No other spelling is accepted.
///
/// # Errors
///
/// Returns a [`ParseErrorKind::Syntax`] error for any other string.
pub fn parse_bool(s: &str) -> Result<bool, ParseError> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(ParseError::new("ParseBool", s, ParseErrorKind::Syntax)),
    }
}

/// Parses a signed 64-bit integer exactly as Go's `strconv.ParseInt(s, 0, 64)` does.
///
/// The string may begin with `+` or `-`. The base is implied by the prefix that follows the sign: `0b` or `0B` for
/// binary, `0o`, `0O`, or a bare leading `0` for octal, `0x` or `0X` for hexadecimal, and decimal otherwise. Underscores
/// may separate digits, or a base prefix from the first digit, as in Go integer literals.
///
/// # Errors
///
/// Returns a [`ParseErrorKind::Syntax`] error if the string is empty, contains a digit invalid for its base, or misplaces
/// an underscore, and a [`ParseErrorKind::Range`] error if the value does not fit in an `i64`.
pub fn parse_int(s: &str) -> Result<i64, ParseError> {
    let error = |kind| ParseError::new("ParseInt", s, kind);

    let (negative, unsigned) = match s.as_bytes().first() {
        None => return Err(error(ParseErrorKind::Syntax)),
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        Some(_) => (false, s),
    };

    let magnitude = parse_uint_base_0(unsigned).map_err(error)?;

    // The magnitude of `i64::MIN` is one more than `i64::MAX`.
    const CUTOFF: u64 = 1 << 63;
    if (!negative && magnitude >= CUTOFF) || (negative && magnitude > CUTOFF) {
        return Err(error(ParseErrorKind::Range));
    }
    let value = magnitude as i64;
    Ok(if negative { value.wrapping_neg() } else { value })
}

/// Go's `strconv.ParseUint(s, 0, 64)`, without a sign.
fn parse_uint_base_0(s: &str) -> Result<u64, ParseErrorKind> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(ParseErrorKind::Syntax);
    }

    let (base, digits): (u64, &[u8]) = if bytes[0] == b'0' {
        match bytes.get(1).map(|&c| lower(c)) {
            Some(b'b') if bytes.len() >= 3 => (2, &bytes[2..]),
            Some(b'o') if bytes.len() >= 3 => (8, &bytes[2..]),
            Some(b'x') if bytes.len() >= 3 => (16, &bytes[2..]),
            _ => (8, &bytes[1..]),
        }
    } else {
        (10, bytes)
    };

    let mut underscores = false;
    let mut n: u64 = 0;
    for &c in digits {
        let digit = match c {
            b'_' => {
                underscores = true;
                continue;
            }
            b'0'..=b'9' => c - b'0',
            _ if lower(c).is_ascii_lowercase() => lower(c) - b'a' + 10,
            _ => return Err(ParseErrorKind::Syntax),
        };
        if u64::from(digit) >= base {
            return Err(ParseErrorKind::Syntax);
        }
        // Go reports an overflow as soon as it happens, before looking at any later character.
        n = n
            .checked_mul(base)
            .and_then(|n| n.checked_add(u64::from(digit)))
            .ok_or(ParseErrorKind::Range)?;
    }

    if underscores && !underscore_ok(bytes) {
        return Err(ParseErrorKind::Syntax);
    }
    Ok(n)
}

/// Parses a 64-bit float exactly as Go's `strconv.ParseFloat(s, 64)` does.
///
/// Accepts decimal floats (`1.5`, `.5`, `5.`, `1e-3`), hexadecimal floats with a required binary exponent (`0x1p-2`,
/// `0x1.8P+1`), an optional leading sign, underscores between digits as in Go float literals, and the case-insensitive
/// special values `NaN`, `Inf`, and `Infinity` (the latter two optionally signed). The result is the nearest `f64`,
/// rounded half to even.
///
/// # Errors
///
/// Returns a [`ParseErrorKind::Syntax`] error if the string is not a well-formed float, and a [`ParseErrorKind::Range`]
/// error if its magnitude is too large to represent as a finite `f64`.
pub fn parse_float(s: &str) -> Result<f64, ParseError> {
    let error = |kind| ParseError::new("ParseFloat", s, kind);
    let bytes = s.as_bytes();

    if let Some((value, consumed)) = special(bytes) {
        return if consumed == bytes.len() {
            Ok(value)
        } else {
            Err(error(ParseErrorKind::Syntax))
        };
    }

    let parts = read_float(bytes)
        .filter(|parts| parts.len == bytes.len())
        .ok_or_else(|| error(ParseErrorKind::Syntax))?;

    let value = if parts.hex {
        atof_hex(parts.mantissa, parts.exp, parts.negative, parts.truncated)
    } else {
        // `read_float` has validated the grammar, so what remains is a plain decimal that Rust rounds exactly as Go
        // does. Go caps how far it accumulates a large exponent, so pass along that exponent rather than the written
        // one.
        let mut decimal: String = s[..parts.mantissa_end].chars().filter(|&c| c != '_').collect();
        decimal.push('e');
        decimal.push_str(&parts.exp10.to_string());
        decimal.parse::<f64>().expect("a validated decimal float parses")
    };

    if value.is_infinite() {
        return Err(error(ParseErrorKind::Range));
    }
    Ok(value)
}

/// The pieces of a float that `read_float` recognized.
struct FloatParts {
    /// Up to the first 19 decimal or 16 hexadecimal significant digits.
    mantissa: u64,
    /// The power of the base (2 for hexadecimal) that scales `mantissa` to the value.
    exp: i64,
    negative: bool,
    /// Whether nonzero digits were dropped from `mantissa`.
    truncated: bool,
    hex: bool,
    /// Where the mantissa ends and the exponent, if any, begins.
    mantissa_end: usize,
    /// The written exponent, capped as Go caps it.
    exp10: i64,
    /// How many bytes form the float.
    len: usize,
}

/// Go's `readFloat`: recognizes a decimal or hexadecimal float at the start of `s`.
///
/// Returns `None` if no well-formed float starts there. The float may be followed by other bytes; `len` says how many
/// bytes it spans.
fn read_float(s: &[u8]) -> Option<FloatParts> {
    let mut i = 0;
    let mut underscores = false;

    let negative = match s.first()? {
        b'+' => {
            i += 1;
            false
        }
        b'-' => {
            i += 1;
            true
        }
        _ => false,
    };

    let mut base: u64 = 10;
    let mut max_mantissa_digits = 19; // 10^19 fits in a u64.
    let mut exp_char = b'e';
    let mut hex = false;
    if i + 2 < s.len() && s[i] == b'0' && lower(s[i + 1]) == b'x' {
        base = 16;
        max_mantissa_digits = 16; // 16^16 fits in a u64.
        i += 2;
        exp_char = b'p';
        hex = true;
    }

    let mut saw_dot = false;
    let mut saw_digits = false;
    let mut nd: i64 = 0;
    let mut nd_mantissa: i64 = 0;
    let mut dp: i64 = 0;
    let mut mantissa: u64 = 0;
    let mut truncated = false;
    while i < s.len() {
        let c = s[i];
        if c == b'_' {
            underscores = true;
        } else if c == b'.' {
            if saw_dot {
                break;
            }
            saw_dot = true;
            dp = nd;
        } else if c.is_ascii_digit() {
            saw_digits = true;
            if c == b'0' && nd == 0 {
                // Ignore leading zeros.
                dp -= 1;
            } else {
                nd += 1;
                if nd_mantissa < max_mantissa_digits {
                    mantissa = mantissa * base + u64::from(c - b'0');
                    nd_mantissa += 1;
                } else if c != b'0' {
                    truncated = true;
                }
            }
        } else if base == 16 && (b'a'..=b'f').contains(&lower(c)) {
            saw_digits = true;
            nd += 1;
            if nd_mantissa < max_mantissa_digits {
                mantissa = mantissa * 16 + u64::from(lower(c) - b'a' + 10);
                nd_mantissa += 1;
            } else {
                truncated = true;
            }
        } else {
            break;
        }
        i += 1;
    }
    if !saw_digits {
        return None;
    }
    if !saw_dot {
        dp = nd;
    }
    if base == 16 {
        dp *= 4;
        nd_mantissa *= 4;
    }

    let mantissa_end = i;
    let mut exp10 = 0;
    if i < s.len() && lower(s[i]) == exp_char {
        i += 1;
        let sign = match s.get(i)? {
            b'+' => {
                i += 1;
                1
            }
            b'-' => {
                i += 1;
                -1
            }
            _ => 1,
        };
        if !s.get(i)?.is_ascii_digit() {
            return None;
        }
        let mut e: i64 = 0;
        while i < s.len() && (s[i].is_ascii_digit() || s[i] == b'_') {
            if s[i] == b'_' {
                underscores = true;
            } else if e < 10000 {
                e = e * 10 + i64::from(s[i] - b'0');
            }
            i += 1;
        }
        exp10 = e * sign;
        dp += exp10;
    } else if base == 16 {
        // A hexadecimal float must have an exponent.
        return None;
    }

    let exp = if mantissa != 0 { dp - nd_mantissa } else { 0 };

    if underscores && !underscore_ok(&s[..i]) {
        return None;
    }

    Some(FloatParts {
        mantissa,
        exp,
        negative,
        truncated,
        hex,
        mantissa_end,
        exp10,
        len: i,
    })
}

/// Go's `atofHex` for `float64`: rounds `mantissa * 2^exp` to the nearest `f64`, half to even.
///
/// When `truncated` is set, nonzero bits below `mantissa` were dropped, which breaks ties upward. Overflow yields an
/// infinity, which the caller reports as a range error.
fn atof_hex(mut mantissa: u64, mut exp: i64, negative: bool, truncated: bool) -> f64 {
    const MANTISSA_BITS: u32 = 52;
    const EXPONENT_BITS: u32 = 11;
    const BIAS: i64 = -1023;
    const MAX_EXP: i64 = (1 << EXPONENT_BITS) + BIAS - 2;
    const MIN_EXP: i64 = BIAS + 1;

    // The mantissa is now implicitly divided by 2^MANTISSA_BITS.
    exp += i64::from(MANTISSA_BITS);

    // Normalize to a leading 1 bit followed by MANTISSA_BITS bits, plus two rounding bits, the lower of which is sticky.
    while mantissa != 0 && mantissa >> (MANTISSA_BITS + 2) == 0 {
        mantissa <<= 1;
        exp -= 1;
    }
    if truncated {
        mantissa |= 1;
    }
    while mantissa >> (1 + MANTISSA_BITS + 2) != 0 {
        mantissa = (mantissa >> 1) | (mantissa & 1);
        exp += 1;
    }

    // If the exponent is too small, denormalize in hopes of making it representable.
    while mantissa > 1 && exp < MIN_EXP - 2 {
        mantissa = (mantissa >> 1) | (mantissa & 1);
        exp += 1;
    }

    // Round using the two bottom bits, half to even.
    let mut round = mantissa & 3;
    mantissa >>= 2;
    round |= mantissa & 1;
    exp += 2;
    if round == 3 {
        mantissa += 1;
        if mantissa == 1 << (1 + MANTISSA_BITS) {
            mantissa >>= 1;
            exp += 1;
        }
    }

    if mantissa >> MANTISSA_BITS == 0 {
        // Subnormal or zero.
        exp = BIAS;
    }
    if exp > MAX_EXP {
        mantissa = 1 << MANTISSA_BITS;
        exp = MAX_EXP + 1;
    }

    let mut bits = mantissa & ((1 << MANTISSA_BITS) - 1);
    bits |= (((exp - BIAS) & ((1 << EXPONENT_BITS) - 1)) as u64) << MANTISSA_BITS;
    if negative {
        bits |= 1 << (MANTISSA_BITS + EXPONENT_BITS);
    }
    f64::from_bits(bits)
}

/// Go's `special`: recognizes `NaN`, `Inf`, or `Infinity` at the start of `s`, ignoring case.
///
/// An infinity may be signed; `NaN` may not. Returns the value and how many bytes it spans.
fn special(s: &[u8]) -> Option<(f64, usize)> {
    let (sign, sign_len, rest) = match *s.first()? {
        b'+' => (1.0, 1, &s[1..]),
        b'-' => (-1.0, 1, &s[1..]),
        b'i' | b'I' => (1.0, 0, s),
        b'n' | b'N' => return (common_prefix_len_ignore_case(s, b"nan") == 3).then_some((f64::NAN, 3)),
        _ => return None,
    };
    let mut n = common_prefix_len_ignore_case(rest, b"infinity");
    // Anything longer than "inf" is fine, but without all of "infinity" only "inf" is consumed.
    if 3 < n && n < 8 {
        n = 3;
    }
    (n == 3 || n == 8).then_some((sign * f64::INFINITY, sign_len + n))
}

/// The length of the common prefix of `s` and the lowercase `prefix`, ignoring the case of `s`.
fn common_prefix_len_ignore_case(s: &[u8], prefix: &[u8]) -> usize {
    s.iter()
        .zip(prefix)
        .take_while(|(c, p)| c.to_ascii_lowercase() == **p)
        .count()
}

/// Go's `underscoreOK`: whether every underscore in the number `s` sits between digits, or between a base prefix and a
/// digit.
fn underscore_ok(s: &[u8]) -> bool {
    #[derive(PartialEq)]
    enum Saw {
        Start,
        Digit,
        Underscore,
        Other,
    }

    let s = match s.first() {
        Some(b'+' | b'-') => &s[1..],
        _ => s,
    };

    let mut saw = Saw::Start;
    let mut i = 0;
    let mut hex = false;
    if s.len() >= 2 && s[0] == b'0' && matches!(lower(s[1]), b'b' | b'o' | b'x') {
        i = 2;
        // A base prefix counts as a digit for "underscore as digit separator".
        saw = Saw::Digit;
        hex = lower(s[1]) == b'x';
    }

    for &c in &s[i..] {
        if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&lower(c))) {
            saw = Saw::Digit;
        } else if c == b'_' {
            if saw != Saw::Digit {
                return false;
            }
            saw = Saw::Underscore;
        } else if saw == Saw::Underscore {
            return false;
        } else {
            saw = Saw::Other;
        }
    }
    saw != Saw::Underscore
}

/// Go's `lower`: converts an ASCII letter to lowercase. Other bytes may change, but never into a letter.
fn lower(c: u8) -> u8 {
    c | (b'x' - b'X')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_error(s: &str) -> ParseErrorKind {
        parse_int(s).expect_err(s).kind()
    }

    fn float_error(s: &str) -> ParseErrorKind {
        parse_float(s).expect_err(s).kind()
    }

    #[test]
    fn bool_accepts_exactly_go_spellings() {
        for s in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(s), Ok(true), "{s}");
        }
        for s in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(s), Ok(false), "{s}");
        }
        for s in ["", "yes", "on", "tRUE", " true", "2"] {
            assert!(parse_bool(s).is_err(), "{s}");
        }
    }

    // Expected values below were produced by Go 1.27's `strconv.ParseInt(s, 0, 64)`.
    #[test]
    fn int_infers_the_base_from_the_prefix() {
        let cases = [
            ("0", 0),
            ("00", 0),
            ("10", 10),
            ("+10", 10),
            ("-10", -10),
            ("010", 8),
            ("-010", -8),
            ("0o17", 15),
            ("0O17", 15),
            ("0b101", 5),
            ("0B101", 5),
            ("0x10", 16),
            ("0X1f", 31),
            ("0xABCDEF", 0xabcdef),
            ("-0x10", -16),
            ("+0x10", 16),
        ];
        for (s, expected) in cases {
            assert_eq!(parse_int(s), Ok(expected), "{s}");
        }
    }

    #[test]
    fn int_accepts_underscores_between_digits() {
        let cases = [
            ("1_000", 1000),
            ("-1_000_000", -1_000_000),
            ("0x_10", 16),
            ("0x1_0", 16),
            ("0b_1_0_1", 5),
            ("0_7", 7),
            ("0o_17", 15),
        ];
        for (s, expected) in cases {
            assert_eq!(parse_int(s), Ok(expected), "{s}");
        }
    }

    #[test]
    fn int_rejects_malformed_strings() {
        for s in [
            "", "+", "-", " 1", "1 ", "08", "09", "0b2", "0o8", "0xg", "0x", "0b", "0o", "0x.1", "1.0", "1e3", "_1",
            "1_", "1__0", "0x__1", "0_x10", "+-1", "--1", "1a", "0x10.0",
        ] {
            assert_eq!(int_error(s), ParseErrorKind::Syntax, "{s:?}");
        }
    }

    #[test]
    fn int_reports_values_outside_i64() {
        assert_eq!(parse_int("9223372036854775807"), Ok(i64::MAX));
        assert_eq!(parse_int("-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(parse_int("0x7fffffffffffffff"), Ok(i64::MAX));
        assert_eq!(parse_int("-0x8000000000000000"), Ok(i64::MIN));
        for s in [
            "9223372036854775808",
            "-9223372036854775809",
            "0x8000000000000000",
            "18446744073709551616",
            // Go reports the overflow before it reaches the invalid trailing byte.
            "99999999999999999999x",
        ] {
            assert_eq!(int_error(s), ParseErrorKind::Range, "{s}");
        }
    }

    // Expected values below were produced by Go 1.27's `strconv.ParseFloat(s, 64)`.
    #[test]
    fn float_accepts_decimal_spellings() {
        let cases = [
            ("0", 0.0),
            ("1", 1.0),
            ("+1.5", 1.5),
            ("-1.5", -1.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e3", 1000.0),
            ("1E3", 1000.0),
            ("1e+3", 1000.0),
            ("1.5e-3", 0.0015),
            ("1_000.5", 1000.5),
            ("1_0e1_0", 1e11),
            ("010", 10.0),
            ("0.1", 0.1),
            ("1.342177295e+08", 134217729.5),
            ("1e-400", 0.0),
            ("4.9e-324", 5e-324),
        ];
        for (s, expected) in cases {
            assert_eq!(parse_float(s), Ok(expected), "{s}");
        }
        assert!(parse_float("-0").unwrap().is_sign_negative());
    }

    #[test]
    fn float_caps_large_exponents_as_go_does() {
        // Go stops accumulating an exponent once it reaches 10000, so here it reads `e10000` rather than `e100000` and
        // the value underflows instead of being scaled back to one.
        let s = format!("0.{}1e100000", "0".repeat(99999));
        assert_eq!(parse_float(&s), Ok(0.0));

        // Below the cap, the written exponent applies in full.
        let s = format!("0.{}1e20000", "0".repeat(20000));
        assert_eq!(parse_float(&s), Ok(0.1));
    }

    #[test]
    fn float_accepts_hexadecimal_spellings() {
        let cases = [
            ("0x1p0", 1.0),
            ("0x1p-2", 0.25),
            ("0X1P+4", 16.0),
            ("0x1.8p1", 3.0),
            ("-0x1.8p1", -3.0),
            ("0x.8p1", 1.0),
            ("0x10p0", 16.0),
            ("0x_1p0", 1.0),
            ("0x1_0p0", 16.0),
            ("0x0p0", 0.0),
            ("0x1p-1074", 5e-324),
            ("0x1p-1075", 0.0),
            ("0x1.fffffffffffffp1023", f64::MAX),
            // Ties round to even, and dropped nonzero digits break a tie upward.
            ("0x1.00000000000008p0", 1.0),
            ("0x1.00000000000018p0", 1.0000000000000004),
            ("0x1.000000000000080000001p0", 1.0000000000000002),
        ];
        for (s, expected) in cases {
            assert_eq!(parse_float(s), Ok(expected), "{s}");
        }
    }

    #[test]
    fn float_accepts_special_values() {
        assert!(parse_float("NaN").unwrap().is_nan());
        assert!(parse_float("nan").unwrap().is_nan());
        assert_eq!(parse_float("Inf"), Ok(f64::INFINITY));
        assert_eq!(parse_float("+inf"), Ok(f64::INFINITY));
        assert_eq!(parse_float("-Infinity"), Ok(f64::NEG_INFINITY));
        assert_eq!(parse_float("INFINITY"), Ok(f64::INFINITY));
    }

    #[test]
    fn float_rejects_malformed_strings() {
        for s in [
            "",
            "+",
            "-",
            ".",
            " 1",
            "1 ",
            "1e",
            "1e+",
            "e1",
            "1..0",
            "1.0.0",
            "0x1",
            "0x1.8",
            "0x",
            "0xp1",
            "0x1e1",
            "1p1",
            "_1",
            "1_",
            "1__0",
            "1_.0",
            "1._0",
            "0x1p_1",
            "+nan",
            "-nan",
            "infin",
            "infinityy",
            "nana",
            "1.5x",
            "0x10",
        ] {
            assert_eq!(float_error(s), ParseErrorKind::Syntax, "{s:?}");
        }
    }

    #[test]
    fn float_reports_overflow_as_a_range_error() {
        for s in ["1e309", "-1e309", "0x1p1024", "0x1.fffffffffffff8p1023", "1e100000"] {
            assert_eq!(float_error(s), ParseErrorKind::Range, "{s}");
        }
    }

    #[test]
    fn errors_read_like_go_errors() {
        assert_eq!(
            parse_int("0x").unwrap_err().to_string(),
            r#"strconv.ParseInt: parsing "0x": invalid syntax"#
        );
        assert_eq!(
            parse_float("1e309").unwrap_err().to_string(),
            r#"strconv.ParseFloat: parsing "1e309": value out of range"#
        );
    }
}
