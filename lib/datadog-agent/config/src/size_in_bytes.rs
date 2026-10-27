//! Parse byte-size settings the way the Agent's `GetSizeInBytes` does.
//!
//! The Agent reads `log_file_max_size` and `dogstatsd_log_file_max_size` with
//! `parseSizeInBytes` (`pkg/config/nodetreemodel/helpers.go`), which accepts a narrower unit grammar
//! than the `bytesize` crate and reads every unit as a power of two.

/// Parses a size setting into a byte count, as the Agent's `GetSizeInBytes` does.
///
/// This ports `parseSizeInBytes` and the string path of `cast.ToInt`:
///
/// 1. Surrounding whitespace is trimmed.
/// 2. If the text is at least three bytes long and ends in `b` or `B`, a preceding `k`, `m`, or `g`
///    (either case) selects a multiplier of 2^10, 2^20, or 2^30 and both letters are removed. Any
///    other preceding byte leaves a multiplier of one and removes only the `b`. A two-byte value such
///    as `1B` is never stripped, so it is not a number. There is no `T`, no bare `K`/`M`/`G`, and no
///    `iB` spelling.
/// 3. The remaining text is trimmed again, an all-zero fraction is dropped (`10.0` is `10`), and the
///    result is read as a Go integer literal with base 0: an optional sign, then decimal digits or a
///    `0x`, `0o`, `0b`, or leading-`0` (octal) prefix, with `_` digit separators only between digits
///    or after a prefix. The value must fit in an `i64`.
/// 4. The count is multiplied by the multiplier and must fit in a `u64`.
///
/// Returns `None` when the Agent's parse fails, the value is negative, or the product overflows. The
/// Agent reads all of these as `0` without a diagnostic, so `None` keeps them apart from a
/// configured `0`. Otherwise, returns the Agent's byte count.
pub fn parse_size_in_bytes(raw: &str) -> Option<u64> {
    let (digits, multiplier) = split_unit(raw.trim());
    let count = u64::try_from(parse_go_int(trim_zero_decimal(digits))?).ok()?;
    count.checked_mul(multiplier)
}

/// Splits trimmed text into its numeric part and the multiplier its unit suffix selects.
///
/// Works on bytes: unit suffixes are ASCII, so removing them preserves UTF-8 character boundaries.
fn split_unit(text: &str) -> (&str, u64) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 3 || !matches!(bytes[len - 1], b'b' | b'B') {
        return (text, 1);
    }

    let (multiplier, suffix_len) = match bytes[len - 2].to_ascii_lowercase() {
        b'k' => (1 << 10, 2),
        b'm' => (1 << 20, 2),
        b'g' => (1 << 30, 2),
        _ => (1, 1),
    };
    (text[..len - suffix_len].trim(), multiplier)
}

/// `cast`'s `trimZeroDecimal`: scans back over `0` and `.` and cuts at the first `.` that has a `0`
/// after it, so `"10.0"` and `"10.0."` are both `"10"`.
///
/// [`crate::cast_de`] keeps a narrower form for integer settings that drops only a single all-zero
/// fraction.
fn trim_zero_decimal(text: &str) -> &str {
    let mut found_zero = false;
    for (index, byte) in text.bytes().enumerate().rev() {
        match byte {
            b'.' if found_zero => return &text[..index],
            b'.' => {}
            b'0' => found_zero = true,
            _ => return text,
        }
    }
    text
}

/// `strconv.ParseInt(s, 0, 0)` on a 64-bit platform, returning `None` where Go returns an error.
fn parse_go_int(text: &str) -> Option<i64> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        _ => (false, text),
    };

    let magnitude = i128::from(parse_go_uint(unsigned)?);
    // `i64::MIN` has a magnitude one larger than `i64::MAX`.
    i64::try_from(if negative { -magnitude } else { magnitude }).ok()
}

/// `strconv.ParseUint(s, 0, 64)`: base prefixes and underscores follow Go's integer literal rules.
fn parse_go_uint(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let &first = bytes.first()?;

    // A prefix needs a digit position after it, so a bare `0x` is an octal `0` followed by `x`.
    let (base, digits) = if first == b'0' {
        match bytes.get(1).map(u8::to_ascii_lowercase) {
            Some(b'b') if bytes.len() >= 3 => (2, &bytes[2..]),
            Some(b'o') if bytes.len() >= 3 => (8, &bytes[2..]),
            Some(b'x') if bytes.len() >= 3 => (16, &bytes[2..]),
            _ => (8, &bytes[1..]),
        }
    } else {
        (10, bytes)
    };

    let mut value: u64 = 0;
    let mut saw_underscore = false;
    for &byte in digits {
        let digit = match byte {
            b'_' => {
                saw_underscore = true;
                continue;
            }
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'z' | b'A'..=b'Z' => byte.to_ascii_lowercase() - b'a' + 10,
            _ => return None,
        };
        if digit >= base {
            return None;
        }
        value = value.checked_mul(u64::from(base))?.checked_add(u64::from(digit))?;
    }

    if saw_underscore && !underscores_ok(bytes) {
        return None;
    }
    Some(value)
}

/// `strconv.underscoreOK` for text without a sign: an underscore must sit between two digits, or
/// between a base prefix and a digit.
fn underscores_ok(bytes: &[u8]) -> bool {
    /// What the previous byte was.
    #[derive(PartialEq)]
    enum Previous {
        Start,
        Digit,
        Underscore,
        Other,
    }

    let mut previous = Previous::Start;
    let mut hex = false;
    let mut rest = bytes;
    if let [b'0', prefix, tail @ ..] = bytes {
        if matches!(prefix.to_ascii_lowercase(), b'b' | b'o' | b'x') {
            // A base prefix counts as a digit for separator placement.
            previous = Previous::Digit;
            hex = prefix.eq_ignore_ascii_case(&b'x');
            rest = tail;
        }
    }

    for &byte in rest {
        if byte.is_ascii_digit() || (hex && byte.is_ascii_hexdigit()) {
            previous = Previous::Digit;
        } else if byte == b'_' {
            if previous != Previous::Digit {
                return false;
            }
            previous = Previous::Underscore;
        } else if previous == Previous::Underscore {
            return false;
        } else {
            previous = Previous::Other;
        }
    }
    previous != Previous::Underscore
}

#[cfg(test)]
mod tests {
    use super::parse_size_in_bytes;

    /// Asserts the exact byte count for each input.
    #[track_caller]
    fn assert_sizes(cases: &[(&str, u64)]) {
        for &(input, expected) in cases {
            assert_eq!(parse_size_in_bytes(input), Some(expected), "input {input:?}");
        }
    }

    /// Asserts each input is one the Agent reads as `0` because its parse fails.
    #[track_caller]
    fn assert_unparsed(inputs: &[&str]) {
        for &input in inputs {
            assert_eq!(parse_size_in_bytes(input), None, "input {input:?}");
        }
    }

    // Expected sizes were produced by running the Agent's `parseSizeInBytes` on each input:
    // https://github.com/DataDog/datadog-agent/blob/281d921619d52ce7b99aef40607285992c9c2e89/pkg/config/nodetreemodel/helpers.go#L36-L71
    // Where the Agent returns 0 because its parse failed, the value was negative, or the product
    // overflowed, the input is asserted as unparsed.

    // Ported from the Agent's `TestGetSizeInBytes`:
    // https://github.com/DataDog/datadog-agent/blob/281d921619d52ce7b99aef40607285992c9c2e89/pkg/config/nodetreemodel/getter_test.go#L226-L242
    #[test]
    fn agent_get_size_in_bytes_cases() {
        assert_sizes(&[
            ("123", 123),
            ("1kb", 1024),
            ("1Mb", 1024 * 1024),
            ("1 GB", 1024 * 1024 * 1024),
        ]);
        assert_unparsed(&["-1", "invalid"]);
    }

    #[test]
    fn schema_default_and_common_spellings_are_binary_units() {
        assert_sizes(&[
            ("10Mb", 10_485_760),
            ("10MB", 10_485_760),
            ("10mb", 10_485_760),
            ("10 MB", 10_485_760),
            (" 10MB ", 10_485_760),
            ("10 MB ", 10_485_760),
            ("10.0MB", 10_485_760),
            ("10.00MB", 10_485_760),
            ("+10MB", 10_485_760),
            ("5KB", 5_120),
            ("1GB", 1 << 30),
        ]);
    }

    #[test]
    fn trailing_dot_after_an_all_zero_fraction_is_dropped() {
        // Quirk of the Agent's zero-decimal trimming: it scans backward and cuts at the first `.` with a
        // `0` to its right.
        assert_sizes(&[("10.0.MB", 10 << 20), ("1.0.", 1)]);
    }

    #[test]
    fn unit_letters_fold_case_independently() {
        assert_sizes(&[
            ("1kB", 1 << 10),
            ("1Kb", 1 << 10),
            ("1KB", 1 << 10),
            ("1mB", 1 << 20),
            ("1Mb", 1 << 20),
            ("1gb", 1 << 30),
            ("1gB", 1 << 30),
            ("1Gb", 1 << 30),
        ]);
    }

    #[test]
    fn plain_numbers_and_bare_byte_suffix_are_literal() {
        assert_sizes(&[
            ("100", 100),
            ("100b", 100),
            ("100 b", 100),
            ("3 b", 3),
            ("0", 0),
            ("0KB", 0),
            ("-0", 0),
            ("10485760", 10_485_760),
        ]);
    }

    #[test]
    fn two_byte_byte_suffix_is_not_stripped() {
        // The Agent only strips a suffix from text of three bytes or more, so `1B` is not a number while
        // `10b` and `1 b` are.
        assert_unparsed(&["1B", "1b", "kb", "KB", "b", "B"]);
        assert_sizes(&[("10b", 10), ("1 b", 1)]);
    }

    #[test]
    fn base_prefixes_follow_go_integer_literals() {
        assert_sizes(&[
            ("0x10KB", 16 << 10),
            ("0X10KB", 16 << 10),
            ("0xffkb", 255 << 10),
            ("0x10", 16),
            ("0o10KB", 8 << 10),
            ("0O10KB", 8 << 10),
            ("010KB", 8 << 10),
            ("010", 8),
            ("0b10KB", 2 << 10),
            ("0B10KB", 2 << 10),
            ("0b1", 1),
            ("0o7", 7),
            ("00", 0),
            ("0x1.0", 1),
        ]);
        assert_unparsed(&["08", "0x", "0b", "0o", "0b2", "0o8", "0xg", "0x_"]);
    }

    #[test]
    fn underscores_must_separate_digits() {
        assert_sizes(&[
            ("1_000KB", 1_000 << 10),
            ("1_0", 10),
            ("1_0_0", 100),
            ("0_7", 7),
            ("0_10", 8),
            ("0x_1", 1),
            ("0x_ff", 255),
            ("0xf_f", 255),
            ("0b_1", 1),
            ("0o_7", 7),
            ("0b1_0KB", 2 << 10),
        ]);
        assert_unparsed(&["_1KB", "1_KB", "1__0", "1__0KB", "1_", "0_x1", "0xf__f", "0b1_"]);
    }

    #[test]
    fn units_the_agent_does_not_know_are_unparsed() {
        assert_unparsed(&[
            "10MiB", "10K", "10M", "1G", "10TB", "3tb", "1.5MB", "1KBB", "1 K B", "2k b", "MB",
        ]);
    }

    #[test]
    fn negative_empty_and_malformed_numbers_are_unparsed() {
        assert_unparsed(&[
            "-1MB", "-1", "", " ", "abc", "+", "-", "+-1", "--1", "1 0", "10.MB", ".0MB", "1.0.0MB", "10 . 0MB",
        ]);
    }

    #[test]
    fn integer_range_is_the_signed_64_bit_range() {
        assert_sizes(&[
            ("9223372036854775807", i64::MAX as u64),
            ("9223372036854775807b", i64::MAX as u64),
        ]);
        // A negative value is unparsed even when it is out of range, and `i64::MIN` is still negative.
        assert_unparsed(&["9223372036854775808", "-9223372036854775808", "-9223372036854775809"]);
        assert_unparsed(&["99999999999999999999999"]);
    }

    #[test]
    fn product_may_exceed_i64_but_not_u64() {
        // 2^63 bytes is representable as a `u64` even though the count times the unit exceeds `i64::MAX`.
        assert_sizes(&[
            ("8589934592GB", 1 << 63),
            ("17592186044415MB", 18_446_744_073_708_503_040),
        ]);
        assert_unparsed(&["17592186044416MB", "9223372036854775807KB", "9223372036854775807GB"]);
    }

    #[test]
    fn whitespace_follows_go_trim_space() {
        assert_sizes(&[
            ("10\u{a0}MB", 10 << 20),
            ("10\u{3000}MB", 10 << 20),
            ("\u{a0}1\u{a0}kb\u{a0}", 1 << 10),
            ("1\tKB", 1 << 10),
            ("1\nKB", 1 << 10),
        ]);
    }

    #[test]
    fn non_ascii_input_is_unparsed_without_panicking() {
        // Slicing off a suffix by byte index must not split a multi-byte character.
        assert_unparsed(&["é", "10éB", "10MéB", "10Mé", "１０MB", "10€b", "ééb", "日本語"]);
    }
}
