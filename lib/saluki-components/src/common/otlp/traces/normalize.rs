/// Normalization functions for OTLP traces
use std::char;

use saluki_common::strings::StringBuilder;
use stringtheory::MetaString;
use tracing::debug;

// Max length in bytes.
pub const MAX_NAME_LEN: usize = 100;
pub const MAX_SERVICE_LEN: usize = 100;
pub const MAX_RESOURCE_LEN: usize = 5000;
pub const MAX_TAG_LEN: usize = 200;

// default service name we assign a span if it's missing and we have no reasonable fallback
const DEFAULT_SERVICE_NAME: MetaString = MetaString::from_static("otlpresourcenoservicename");
// default span name we assign a span if it's missing and we have no reasonable fallback
const DEFAULT_SPAN_NAME: MetaString = MetaString::from_static("unnamed_operation");

// lookup tables for fast lookups of ASCII characters
static IS_ALPHA_LOOKUP: [bool; 256] = {
    let mut lookup = [false; 256];
    let mut i = 0;
    while i < 256 {
        lookup[i] = (i as u8 as char).is_ascii_alphabetic();
        i += 1;
    }
    lookup
};
static IS_ALPHA_NUM_LOOKUP: [bool; 256] = {
    let mut lookup = [false; 256];
    let mut i = 0;
    while i < 256 {
        lookup[i] = (i as u8 as char).is_ascii_alphanumeric();
        i += 1;
    }
    lookup
};
static IS_VALID_ASCII_START_CHAR_LOOKUP: [bool; 256] = {
    let mut lookup = [false; 256];
    let mut i = 0;
    while i < 256 {
        lookup[i] = is_valid_ascii_start_char(i as u8 as char);
        i += 1;
    }
    lookup
};
static IS_VALID_ASCII_TAG_CHAR_LOOKUP: [bool; 256] = {
    let mut lookup = [false; 256];
    let mut i = 0;
    while i < 256 {
        lookup[i] = is_valid_ascii_tag_char(i as u8 as char);
        i += 1;
    }
    lookup
};

/// Normalizes a span name.
///
/// This function truncates the name to `MAX_NAME_LEN`, replaces invalid characters with underscores,
/// and handles consecutive underscores and underscores after periods.
#[allow(dead_code)]
pub fn normalize_name(mut name: MetaString) -> MetaString {
    if name.is_empty() {
        debug!(
            "normalize_name: name is empty, returning default span name: {}",
            DEFAULT_SPAN_NAME
        );
        return DEFAULT_SPAN_NAME.clone();
    }
    if name.len() > MAX_NAME_LEN {
        name = MetaString::from(truncate_utf8(&name, MAX_NAME_LEN));
        debug!("normalize_name: name is too long,truncated name: {}", name);
    }

    // Normalize the name according to the following rules:
    // 1. Skip non-alphabetic characters at the start.
    // 2. Replace non-alphanumeric characters (except '.' and '_') with '_'.
    // 3. Avoid consecutive underscores.
    // 4. Avoid underscores after periods.
    // 5. Avoid trailing underscores.
    let mut normalized = String::with_capacity(name.len());
    // Skip non-alphabetic characters at start
    let mut i = 0;
    let name_bytes = name.as_bytes();
    while i < name_bytes.len() && !IS_ALPHA_LOOKUP[name_bytes[i] as usize] {
        i += 1;
    }

    if i >= name_bytes.len() {
        return DEFAULT_SPAN_NAME.clone();
    }
    if is_valid_metric_name(&name.as_ref()[i..]) {
        if name.ends_with('_') {
            return MetaString::from(&name.as_ref()[i..(name.len() - 1)]);
        }
        return MetaString::from(&name.as_ref()[i..]);
    }
    let mut prev_char = '\0';

    while i < name_bytes.len() {
        let c = name_bytes[i] as char;
        i += 1;
        if (c as u32) < 256 && IS_ALPHA_NUM_LOOKUP[c as usize] {
            normalized.push(c);
            prev_char = c;
        } else if c == '.' {
            if prev_char == '_' {
                // Overwrite underscore with period
                normalized.pop();
                normalized.push('.');
                prev_char = '.';
            } else if prev_char != '.' {
                normalized.push('.');
                prev_char = '.';
            }
        } else {
            // Treat as underscore
            if prev_char != '.' && prev_char != '_' {
                normalized.push('_');
                prev_char = '_';
            }
        }
    }

    if normalized.ends_with('_') {
        normalized.pop();
    }

    if normalized.is_empty() {
        return DEFAULT_SPAN_NAME.clone();
    }

    MetaString::from(normalized)
}

/// Normalizes a service name into `out`.
///
/// The output is always written from a cleared buffer.
pub(super) fn normalize_service_into<I>(service: &str, out: &mut StringBuilder<I>) {
    out.clear();

    // TODO: add fall back service for languages
    // e.g. https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/normalize/normalize.go#L124
    if service.is_empty() {
        let _ = out.push_str(DEFAULT_SERVICE_NAME.as_ref());
        return;
    }

    let truncated = truncate_utf8_str(service, MAX_SERVICE_LEN);
    if is_normalized_ascii_tag(truncated, false) {
        let _ = out.push_str(truncated);
        return;
    }
    normalize_unchecked(truncated, false, out);

    if out.as_str().is_empty() {
        let _ = out.push_str(DEFAULT_SERVICE_NAME.as_ref());
    }
}

/// Normalizes `value` into `out`, clearing any existing contents first.
///
/// Use this when the caller has already determined `value` is not normalized, so we
/// can skip a separate normalized fast-path check.
pub(super) fn normalize_tag_value_into_unchecked<I>(value: &str, out: &mut StringBuilder<I>) {
    out.clear();
    normalize_tag_value_append_unchecked(value, out);
}

/// Appends the normalized form of `value` to `out` without clearing it first.
///
/// Use this when extending an existing normalized prefix with a value already known to
/// require normalization.
pub(super) fn normalize_tag_value_append_unchecked<I>(value: &str, out: &mut StringBuilder<I>) {
    if value.is_empty() {
        return;
    }
    normalize_unchecked(value, false, out);
}

pub(super) fn is_normalized_tag_value(value: &str) -> bool {
    is_normalized_ascii_tag(value, false)
}

/// Normalization function that writes directly into a [`StringBuilder`].
///
/// Characters are emitted as they are processed: valid characters are pushed directly,
/// runs of illegal characters are collapsed into a single `_`, and trailing illegal
/// characters are dropped.
fn normalize_unchecked<I>(value: &str, remove_digit_start_char: bool, out: &mut StringBuilder<I>) {
    let mut chars = 0usize;
    let mut pending_underscore = false;

    for (idx, mut curr_char) in value.char_indices() {
        let jump = curr_char.len_utf8();

        if (curr_char as u32) < 256 && IS_VALID_ASCII_START_CHAR_LOOKUP[curr_char as usize] {
            if pending_underscore {
                let _ = out.push('_');
                pending_underscore = false;
            }
            let _ = out.push(curr_char);
            chars += 1;
        } else if curr_char.is_ascii_uppercase() {
            if pending_underscore {
                let _ = out.push('_');
                pending_underscore = false;
            }
            let _ = out.push(curr_char.to_ascii_lowercase());
            chars += 1;
        } else {
            if curr_char.is_uppercase() {
                let mut lowercase = curr_char.to_lowercase();
                if let Some(lower_char) = lowercase.next() {
                    if lower_char.len_utf8() == jump {
                        curr_char = lower_char;
                    }
                }
            }

            if curr_char.is_alphabetic() {
                if pending_underscore {
                    let _ = out.push('_');
                    pending_underscore = false;
                }
                let _ = out.push(curr_char);
                chars += 1;
            } else if remove_digit_start_char && chars == 0 {
                let end_idx = idx + jump;
                if end_idx >= 2 * MAX_TAG_LEN {
                    break;
                }
                continue;
            } else if curr_char.is_ascii_digit() || matches!(curr_char, '.' | '/' | '-') {
                if pending_underscore {
                    let _ = out.push('_');
                    pending_underscore = false;
                }
                let _ = out.push(curr_char);
                chars += 1;
            } else {
                // illegal character
                chars += 1;
                pending_underscore = true;
            }
        }

        let end_idx = idx + jump;
        if end_idx >= 2 * MAX_TAG_LEN || chars >= MAX_TAG_LEN {
            break;
        }
    }
    // Trailing illegal chars (pending_underscore=true) are intentionally dropped.
}

fn is_valid_metric_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut chars = name.chars();
    if let Some(c) = chars.next() {
        if !IS_ALPHA_LOOKUP[c as usize] {
            return false;
        }
    }

    let mut prev_char = name.chars().next().unwrap_or_default();

    for c in chars {
        if (c as u32) < 256 && IS_ALPHA_NUM_LOOKUP[c as usize] {
            prev_char = c;
            continue;
        }
        if c == '.' {
            if prev_char == '_' {
                return false;
            }
            prev_char = c;
            continue;
        }
        if c == '_' {
            if prev_char == '_' {
                return false;
            }
            prev_char = c;
            continue;
        }
        return false;
    }

    if prev_char == '_' {
        return false;
    }

    true
}

fn is_normalized_ascii_tag(tag: &str, check_valid_start_char: bool) -> bool {
    if tag.is_empty() {
        return true;
    }
    if tag.len() > MAX_TAG_LEN {
        return false;
    }
    let bytes = tag.as_bytes();
    let mut i = 0;
    if check_valid_start_char {
        if (bytes[0] as u32) < 256 && !IS_VALID_ASCII_START_CHAR_LOOKUP[bytes[0] as usize] {
            return false;
        }
        i += 1;
    }

    while i < bytes.len() {
        let b = bytes[i];
        // TODO: Attempt to optimize this check using SIMD/vectorization.
        if (b as u32) < 256 && IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
            i += 1;
            continue;
        }
        if b == b'_' {
            // an underscore is only valid if it is followed by a valid non-underscore character.
            i += 1;
            if i == bytes.len() || ((bytes[i] as u32) < 256 && !IS_VALID_ASCII_TAG_CHAR_LOOKUP[bytes[i] as usize]) {
                return false;
            }
        }
        return false;
    }

    true
}

const fn is_valid_ascii_start_char(c: char) -> bool {
    c.is_ascii_lowercase() || c == ':'
}

const fn is_valid_ascii_tag_char(c: char) -> bool {
    is_valid_ascii_start_char(c) || (c >= '0' && c <= '9') || c == '.' || c == '/' || c == '-'
}

/// Truncate string to max_len bytes, respecting UTF-8 boundaries.
pub(super) fn truncate_utf8(s: &MetaString, max_len: usize) -> &str {
    truncate_utf8_str(s, max_len)
}

fn truncate_utf8_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
#[cfg(test)]
mod tests {
    use std::char;

    use super::*;

    fn normalize_tag(value: &str) -> MetaString {
        let mut sb = StringBuilder::new();
        normalize_unchecked(value, true, &mut sb);
        MetaString::from(sb.as_str())
    }

    // Test cases taken from the agent codebase
    // https://github.com/DataDog/datadog-agent/blob/instrument-otlp-traffic/pkg/trace/traceutil/normalize/normalize_test.go#L17
    #[test]
    fn test_normalize_tag() {
        let many_dogs = {
            let mut s = String::from("a");
            for _ in 0..799 {
                s.push('🐶');
            }
            s.push('b');
            MetaString::from(s)
        };

        let invalid_utf8 = MetaString::from(String::from_utf8_lossy(b"test\x99\x8faaa").into_owned());
        let invalid_utf8_short = MetaString::from(String::from_utf8_lossy(b"test\x99\x8f").into_owned());

        const LONG_ALPHANUM_INPUT: &str = "A00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000 000000000000";
        const LONG_ALPHANUM_OUTPUT: &str = "a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000_0";

        let replacement = char::REPLACEMENT_CHARACTER;

        let cases: Vec<(MetaString, MetaString)> = vec![
            (
                MetaString::from("#test_starting_hash"),
                MetaString::from("test_starting_hash"),
            ),
            (MetaString::from("TestCAPSandSuch"), MetaString::from("testcapsandsuch")),
            (
                MetaString::from("Test Conversion Of Weird !@#$%^&**() Characters"),
                MetaString::from("test_conversion_of_weird_characters"),
            ),
            (MetaString::from("$#weird_starting"), MetaString::from("weird_starting")),
            (MetaString::from("allowed:c0l0ns"), MetaString::from("allowed:c0l0ns")),
            (MetaString::from("1love"), MetaString::from("love")),
            (MetaString::from("ünicöde"), MetaString::from("ünicöde")),
            (MetaString::from("ünicöde:metäl"), MetaString::from("ünicöde:metäl")),
            (
                MetaString::from("Data🐨dog🐶 繋がっ⛰てて"),
                MetaString::from("data_dog_繋がっ_てて"),
            ),
            (MetaString::from(" spaces   "), MetaString::from("spaces")),
            (
                MetaString::from(" #hashtag!@#spaces #__<>#  "),
                MetaString::from("hashtag_spaces"),
            ),
            (MetaString::from(":testing"), MetaString::from(":testing")),
            (MetaString::from("_foo"), MetaString::from("foo")),
            (MetaString::from(":::test"), MetaString::from(":::test")),
            (
                MetaString::from("contiguous_____underscores"),
                MetaString::from("contiguous_underscores"),
            ),
            (MetaString::from("foo_"), MetaString::from("foo")),
            (
                MetaString::from("\u{017F}odd_\u{017F}case\u{017F}"),
                MetaString::from("\u{017F}odd_\u{017F}case\u{017F}"),
            ),
            (MetaString::from(""), MetaString::from("")),
            (MetaString::from(" "), MetaString::from("")),
            (MetaString::from("ok"), MetaString::from("ok")),
            (MetaString::from("™Ö™Ö™™Ö™"), MetaString::from("ö_ö_ö")),
            (MetaString::from("AlsO:ök"), MetaString::from("also:ök")),
            (MetaString::from(":still_ok"), MetaString::from(":still_ok")),
            (MetaString::from("___trim"), MetaString::from("trim")),
            (MetaString::from("12.:trim@"), MetaString::from(":trim")),
            (MetaString::from("12.:trim@@"), MetaString::from(":trim")),
            (MetaString::from("fun:ky__tag/1"), MetaString::from("fun:ky_tag/1")),
            (MetaString::from("fun:ky@tag/2"), MetaString::from("fun:ky_tag/2")),
            (MetaString::from("fun:ky@@@tag/3"), MetaString::from("fun:ky_tag/3")),
            (MetaString::from("tag:1/2.3"), MetaString::from("tag:1/2.3")),
            (
                MetaString::from("---fun:k####y_ta@#g/1_@@#"),
                MetaString::from("fun:k_y_ta_g/1"),
            ),
            (MetaString::from("AlsO:œ#@ö))œk"), MetaString::from("also:œ_ö_œk")),
            (
                MetaString::from("a".repeat(888)),
                MetaString::from("a".repeat(MAX_TAG_LEN)),
            ),
            (many_dogs, MetaString::from("a")),
            (MetaString::from(format!("a{}", replacement)), MetaString::from("a")),
            (MetaString::from(format!("a{0}{0}", replacement)), MetaString::from("a")),
            (
                MetaString::from(format!("a{0}{0}b", replacement)),
                MetaString::from("a_b"),
            ),
            (invalid_utf8, MetaString::from("test_aaa")),
            (invalid_utf8_short, MetaString::from("test")),
            (
                MetaString::from(LONG_ALPHANUM_INPUT),
                MetaString::from(LONG_ALPHANUM_OUTPUT),
            ),
        ];

        for (input, expected) in cases.iter() {
            let normalized = normalize_tag(input.as_ref());
            assert_eq!(normalized.as_ref(), expected.as_ref(), "input {}", input);
        }
    }

    #[test]
    fn test_normalize_name() {
        let cases: Vec<(MetaString, MetaString)> = vec![
            (MetaString::from(""), DEFAULT_SPAN_NAME.clone()),
            (MetaString::from("good"), MetaString::from("good")),
            (
                MetaString::from("last.underscore_trunc_"),
                MetaString::from("last.underscore_trunc"),
            ),
            (
                MetaString::from("last.double_underscore_trunc__"),
                MetaString::from("last.double_underscore_trunc"),
            ),
            (
                MetaString::from(
                    "Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.Too-Long-.",
                ),
                MetaString::from(
                    "Too_Long.Too_Long.Too_Long.Too_Long.Too_Long.Too_Long.Too_Long.Too_Long.Too_Long.Too_Long.",
                ),
            ),
            (MetaString::from("double..point"), MetaString::from("double..point")),
            (
                MetaString::from("other_^.character^^_than_underscore"),
                MetaString::from("other.character_than_underscore"),
            ),
            (MetaString::from("bad-name"), MetaString::from("bad_name")),
            (
                MetaString::from("^^_.non_alpha.prefix"),
                MetaString::from("non_alpha.prefix"),
            ),
            (
                MetaString::from("_"),
                MetaString::from("unnamed_operation"),
            ),
        ];

        for (name, expected) in cases.iter() {
            let normalized = normalize_name(name.clone());
            assert_eq!(normalized.as_ref(), expected.as_ref(), "name {}", name);
        }
    }

    #[test]
    fn test_normalize_service() {
        let mut sb = StringBuilder::new();
        for (service, expected) in base_service_cases().iter() {
            normalize_service_into(service, &mut sb);
            assert_eq!(sb.as_str(), expected.as_ref(), "service {}", service);
        }

        normalize_service_into("", &mut sb);
        assert_eq!(sb.as_str(), DEFAULT_SERVICE_NAME.as_ref());
    }

    fn base_service_cases() -> Vec<(MetaString, MetaString)> {
        vec![
            (MetaString::from("good"), MetaString::from("good")),
            (MetaString::from("127.0.0.1"), MetaString::from("127.0.0.1")),
            (
                MetaString::from("127.site.platform-db-replica1"),
                MetaString::from("127.site.platform-db-replica1"),
            ),
            (
                MetaString::from("hyphenated-service-name"),
                MetaString::from("hyphenated-service-name"),
            ),
            (
                MetaString::from("🐨animal-db🐶"),
                MetaString::from("_animal-db"),
            ),
            (
                MetaString::from("🐨1animal-db🐶"),
                MetaString::from("_1animal-db"),
            ),
            (
                MetaString::from("1🐨1animal-db🐶"),
                MetaString::from("1_1animal-db"),
            ),
            (
                MetaString::from(
                    "Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.Too$Long$.",
                ),
                MetaString::from(
                    "too_long_.too_long_.too_long_.too_long_.too_long_.too_long_.too_long_.too_long_.too_long_.too_long_.",
                ),
            ),
            (
                MetaString::from("bad$service"),
                MetaString::from("bad_service"),
            ),
        ]
    }

    #[test]
    fn test_truncate_utf8() {
        let e_acute = MetaString::from("é");
        assert!("é".len() == 2);
        assert_eq!(truncate_utf8(&e_acute, 1), "");
        assert_eq!(truncate_utf8(&e_acute, 2), "é");

        let crab = MetaString::from("🦀");
        assert!("🦀".len() == 4);
        assert_eq!(truncate_utf8(&crab, 1), "");
        assert_eq!(truncate_utf8(&crab, 2), "");
        assert_eq!(truncate_utf8(&crab, 3), "");
        assert_eq!(truncate_utf8(&crab, 4), "🦀");

        let a_crab_b = MetaString::from("a🦀b");
        assert!("a🦀b".len() == 6);
        assert_eq!(truncate_utf8(&a_crab_b, 1), "a");
        assert_eq!(truncate_utf8(&a_crab_b, 2), "a");
        assert_eq!(truncate_utf8(&a_crab_b, 3), "a");
        assert_eq!(truncate_utf8(&a_crab_b, 4), "a");
        assert_eq!(truncate_utf8(&a_crab_b, 5), "a🦀");
        assert_eq!(truncate_utf8(&a_crab_b, 6), "a🦀b");

        let empty = MetaString::from("");
        assert_eq!(truncate_utf8(&empty, 5), "");
    }
}
