#[cfg(all(target_arch = "aarch64", not(miri)))]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Normalization functions for OTLP traces.
///
/// # Missing
/// - Add language-specific fallback service names in `normalize_service`.
use stringtheory::MetaString;
use tracing::debug;

// Max length in bytes.
pub const MAX_NAME_LEN: usize = 100;
pub const MAX_SERVICE_LEN: usize = 100;
pub const MAX_RESOURCE_LEN: usize = 5000;
pub const MAX_TAG_LEN: usize = 200;

#[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
const SIMD_CHUNK_SIZE: usize = 16;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
// Bitmask where all 16 lanes are set (0xFFFF = 0b1111_1111_1111_1111). Used to
// check if every lane satisfied a condition via _mm_movemask_epi8.
const SIMD_ALL_LANES_MASK: i32 = 0xFFFF;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
// Bitmask for the last lane only (bit 15 -> 0x8000 = 0b1000_0000_0000_0000). Used
// to detect whether the final byte of a 16-byte chunk is an underscore.
const SIMD_LAST_LANE_MASK: i32 = 0x8000;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
// Bitmask for all lanes except the last (0x7FFF = 0b0111_1111_1111_1111). Used
// when checking that an underscore has a valid follower byte within the same
// chunk.
const SIMD_MASK_EXCEPT_LAST: i32 = 0x7FFF;
#[cfg(all(target_arch = "aarch64", not(miri)))]
const NEON_FIRST_LANE: i32 = 0;
#[cfg(all(target_arch = "aarch64", not(miri)))]
const NEON_LAST_LANE: i32 = 15;
#[cfg(all(target_arch = "aarch64", not(miri)))]
const NEON_FOLLOWER_SHIFT_BYTES: i32 = 15;

// Used to verify that the SIMD optimization was utilized.
#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
static SIMD_HIT_COUNT: AtomicUsize = AtomicUsize::new(0);

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

/// Normalizes a service name.
///
/// Truncates to `MAX_SERVICE_LEN` and ensures characters are valid for tags.
pub fn normalize_service(service: &MetaString) -> MetaString {
    if service.is_empty() {
        return DEFAULT_SERVICE_NAME.clone();
    }

    let truncated = truncate_utf8(service, MAX_SERVICE_LEN);
    let normalized = normalize_tag_value(truncated);

    if normalized.is_empty() {
        return DEFAULT_SERVICE_NAME.clone();
    }

    normalized
}

/// Normalizes a tag value.
///
/// Truncates to `MAX_TAG_LEN` and ensures characters are valid ASCII tag characters.
/// Replaces invalid characters with underscores and trims underscores.
pub fn normalize_tag_value(value: &str) -> MetaString {
    normalize(value, false)
}

pub(super) fn is_normalized_tag_value(value: &str) -> bool {
    is_normalized_ascii_tag(value, false)
}

/// Normalizes a tag (key:value).
#[allow(dead_code)]
pub fn normalize_tag(value: &str) -> MetaString {
    normalize(value, true)
}

/// Normalizes a tag using a scalar-only fast-path validity check.
///
/// This function is exposed for A/B benchmarking and keeps normalization behavior
/// identical to `normalize_tag`.
#[cfg(feature = "bench")]
pub fn normalize_tag_scalar(value: &str) -> MetaString {
    normalize_with_ascii_fast_path(value, true, is_normalized_ascii_tag_scalar_only)
}

/// Normalizes a tag using the SIMD-accelerated fast-path validity check when available.
///
/// This function is exposed for A/B benchmarking and keeps normalization behavior
/// identical to `normalize_tag`.
#[cfg(feature = "bench")]
pub fn normalize_tag_simd(value: &str) -> MetaString {
    normalize_with_ascii_fast_path(value, true, is_normalized_ascii_tag_simd_preferred)
}

fn normalize(value: &str, remove_digit_start_char: bool) -> MetaString {
    normalize_with_ascii_fast_path(value, remove_digit_start_char, is_normalized_ascii_tag)
}

fn normalize_with_ascii_fast_path<F>(
    value: &str, remove_digit_start_char: bool, is_normalized_ascii_tag_fn: F,
) -> MetaString
where
    F: Fn(&str, bool) -> bool,
{
    if value.is_empty() {
        return MetaString::empty();
    }

    // Fast Path: return right away if it is valid
    if is_normalized_ascii_tag_fn(value, remove_digit_start_char) {
        return MetaString::from(value);
    }
    // Trim is used to to remove invalid characters from the start of the tag
    // Cuts is used to mark the start and end of invalid characters that need to be replaced with underscores
    // Chars is used to count the number of valid characters in the tag
    // Tag is the byte slice of the tag
    // End_idx is the index of the last valid character in the tag
    let mut trim = 0usize;
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut chars = 0usize;
    let mut tag = value.as_bytes().to_vec();
    let mut end_idx = value.len();

    for (idx, mut curr_char) in value.char_indices() {
        let jump = curr_char.len_utf8();
        if (curr_char as u32) < 256 && IS_VALID_ASCII_START_CHAR_LOOKUP[curr_char as usize] {
            chars += 1;
        } else if curr_char.is_ascii_uppercase() {
            tag[idx] = curr_char.to_ascii_lowercase() as u8;
            chars += 1;
        } else {
            // converts a unicode uppercase character to a lowercase character when the UTF-8 width stays the same
            if curr_char.is_uppercase() {
                // we check for the number of bytes in the lowercase character to be the same as the original character
                // this is to avoid the case where the lowercase character is a different number of bytes such as ẞ → ss
                let mut lowercase = curr_char.to_lowercase();
                if let Some(lower_char) = lowercase.next() {
                    if lower_char.len_utf8() == jump {
                        // check if the lowercase character is the same number of bytes as the original character
                        let mut buf = [0u8; 4]; // UTF-8 is four bytes max
                        let encoded = lower_char.encode_utf8(&mut buf);
                        tag[idx..idx + jump].copy_from_slice(encoded.as_bytes());
                        curr_char = lower_char;
                    }
                }
            }

            if curr_char.is_alphabetic() {
                chars += 1;
            } else if remove_digit_start_char && chars == 0 {
                trim = idx + jump;
                end_idx = idx + jump;
                if end_idx >= 2 * MAX_TAG_LEN {
                    break;
                }
                continue;
            } else if curr_char.is_ascii_digit() || matches!(curr_char, '.' | '/' | '-') {
                chars += 1;
            } else {
                // illegal character
                chars += 1;
                if let Some(last) = cuts.last_mut() {
                    if last.1 >= idx {
                        last.1 += jump;
                    } else {
                        cuts.push((idx, idx + jump));
                    }
                } else {
                    cuts.push((idx, idx + jump));
                }
            }
        }

        end_idx = idx + jump;
        if end_idx >= 2 * MAX_TAG_LEN {
            break;
        }
        if chars >= MAX_TAG_LEN {
            break;
        }
    }

    let mut tag = tag[trim..end_idx].to_vec();

    if cuts.is_empty() {
        return MetaString::from(String::from_utf8(tag).unwrap_or_default());
    }

    let mut delta = trim;
    for (cut_start, cut_end) in cuts {
        if cut_end <= delta {
            continue;
        }

        let start = cut_start - delta;
        let end = cut_end - delta;

        if end >= tag.len() {
            tag.truncate(start);
            break;
        }

        tag[start] = b'_';
        if end - start == 1 {
            continue;
        }

        tag.copy_within(end.., start + 1);
        let new_len = tag.len() - (end - start) + 1;
        tag.truncate(new_len);
        delta += (cut_end - cut_start) - 1;
    }
    let final_str = String::from_utf8(tag).unwrap_or_default();

    MetaString::from(final_str)
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
    // We call different `is_normalized_ascii_tag_simd` depending on the target CPU architecture, for x86/x86_64 (intel/amd processors) we use the SSE2 SIMD implementation whereas for
    // aarch64 (apple M chips). The function variant is chosen at compile by the `#[cfg..]`
    is_normalized_ascii_tag_with_simd_check(tag, check_valid_start_char, is_normalized_ascii_tag_simd)
}

#[cfg(feature = "bench")]
fn is_normalized_ascii_tag_scalar_only(tag: &str, check_valid_start_char: bool) -> bool {
    is_normalized_ascii_tag_with_simd_check(tag, check_valid_start_char, is_normalized_ascii_tag_no_simd)
}

#[cfg(feature = "bench")]
fn is_normalized_ascii_tag_simd_preferred(tag: &str, check_valid_start_char: bool) -> bool {
    is_normalized_ascii_tag_with_simd_check(tag, check_valid_start_char, is_normalized_ascii_tag_simd)
}

fn is_normalized_ascii_tag_with_simd_check<F>(tag: &str, check_valid_start_char: bool, simd_check_fn: F) -> bool
where
    F: Fn(&[u8], usize) -> Option<bool>,
{
    if tag.is_empty() {
        return true;
    }
    if tag.len() > MAX_TAG_LEN {
        return false;
    }
    let bytes = tag.as_bytes();
    let mut start = 0;
    if check_valid_start_char {
        if !IS_VALID_ASCII_START_CHAR_LOOKUP[bytes[0] as usize] {
            return false;
        }
        start = 1;
    }

    if let Some(valid) = simd_check_fn(bytes, start) {
        return valid;
    }

    is_normalized_ascii_tag_scalar(bytes, start)
}

#[cfg(feature = "bench")]
fn is_normalized_ascii_tag_no_simd(_bytes: &[u8], _start: usize) -> Option<bool> {
    None
}

fn is_normalized_ascii_tag_scalar(bytes: &[u8], mut i: usize) -> bool {
    while i < bytes.len() {
        let b = bytes[i];
        if IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
            i += 1;
            continue;
        }
        if b == b'_' {
            // An underscore is only valid if it is followed by a valid non-underscore character.
            i += 1;
            if i == bytes.len() || !IS_VALID_ASCII_TAG_CHAR_LOOKUP[bytes[i] as usize] {
                return false;
            }
            i += 1;
            continue;
        }
        return false;
    }

    true
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn is_normalized_ascii_tag_simd(bytes: &[u8], start: usize) -> Option<bool> {
    if bytes.len().saturating_sub(start) < SIMD_CHUNK_SIZE {
        return None;
    }
    if !is_x86_feature_detected!("sse2") {
        return None;
    }
    // SAFETY: SSE2 is available and we only load within bounds.
    Some(unsafe { is_normalized_ascii_tag_simd_sse2(bytes, start) })
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
fn is_normalized_ascii_tag_simd(bytes: &[u8], start: usize) -> Option<bool> {
    if bytes.len().saturating_sub(start) < SIMD_CHUNK_SIZE {
        return None;
    }

    // SAFETY: AArch64 guarantees NEON availability and we only load within bounds.
    Some(unsafe { is_normalized_ascii_tag_simd_neon(bytes, start) })
}

#[cfg(any(
    all(target_arch = "aarch64", miri),
    not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))
))]
fn is_normalized_ascii_tag_simd(_bytes: &[u8], _start: usize) -> Option<bool> {
    None
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn is_normalized_ascii_tag_simd_sse2(bytes: &[u8], start: usize) -> bool {
    #[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    SIMD_HIT_COUNT.fetch_add(1, Ordering::Relaxed);

    // Validate `bytes[start..]` as a normalized ASCII tag using 16-byte SSE2 chunks.
    //
    // Rules enforced here:
    // - Allowed characters: lowercase a-z, digits 0-9, '.', '/', '-', ':', '_' (underscore).
    // - Any non-ASCII byte (high bit set) is invalid.
    // - Underscore is a separator: it MUST be followed by a non-underscore allowed character.
    // - Trailing underscore is invalid.
    //
    // The SIMD loop walks 16-byte chunks and validates character classes in parallel.
    // Underscore follower rules are enforced in-chunk for lanes 0..14 and with a
    // direct next-byte check for lane 15.

    let mut i = start;
    let len = bytes.len();

    // Tracks if the previous chunk ended with an underscore. If true, the next
    // byte must be an allowed non-underscore character.
    let mut trailing_underscore = false;

    // Build repeated-byte constants for range comparisons. _mm_set1_epi8 sets the 16 lanes to the same value for parallel comparison.
    // We use (min - 1) and (max + 1) with signed gt comparisons:
    // `x > (min - 1)` && `(max + 1) > x`  <=>  min <= x <= max.
    let lower_min = _mm_set1_epi8((b'a' - 1) as i8);
    let lower_max = _mm_set1_epi8((b'z' + 1) as i8);
    let digit_min = _mm_set1_epi8((b'0' - 1) as i8);
    let digit_max = _mm_set1_epi8((b'9' + 1) as i8);

    let dot = _mm_set1_epi8(b'.' as i8);
    let slash = _mm_set1_epi8(b'/' as i8);
    let dash = _mm_set1_epi8(b'-' as i8);
    let colon = _mm_set1_epi8(b':' as i8);
    let underscore = _mm_set1_epi8(b'_' as i8);

    // Validate chunks and enforce underscore follower rules.
    while i + SIMD_CHUNK_SIZE <= len {
        // Load 16 bytes from memory into a SIMD register starting at ptr
        let ptr = bytes.as_ptr().add(i) as *const __m128i;
        let chunk = _mm_loadu_si128(ptr);

        // Reject any byte with the high bit set. _mm_movemask_epi8 returns a 16-bit
        // mask of the most significant bits for the 16 lanes. Any 1 bit means a non-ASCII (>= 0x80) byte.
        if _mm_movemask_epi8(chunk) != 0 {
            return false;
        }

        // Build masks for allowed character classes.
        // Range check uses strict greater-than on adjusted bounds:
        //   ge_a:   chunk > (b'a' - 1)  == chunk >= b'a'
        //   le_z:   (b'z' + 1) > chunk  == chunk <= b'z'
        // Example: for byte 'm' (0x6D), both comparisons are true, so is_lower
        // has 0xFF (1111_1111 in binary) in that lane. For byte '{' (0x7B), ge_a is true but le_z
        // is false, so is_lower becomes 0x00 in that lane.
        let ge_a = _mm_cmpgt_epi8(chunk, lower_min);
        let le_z = _mm_cmpgt_epi8(lower_max, chunk);
        let is_lower = _mm_and_si128(ge_a, le_z);

        let ge_0 = _mm_cmpgt_epi8(chunk, digit_min);
        let le_9 = _mm_cmpgt_epi8(digit_max, chunk);
        let is_digit = _mm_and_si128(ge_0, le_9);

        let is_dot = _mm_cmpeq_epi8(chunk, dot);
        let is_slash = _mm_cmpeq_epi8(chunk, slash);
        let is_dash = _mm_cmpeq_epi8(chunk, dash);
        let is_colon = _mm_cmpeq_epi8(chunk, colon);
        let is_underscore = _mm_cmpeq_epi8(chunk, underscore);

        // "Allowed" excludes underscore so we can validate underscore follower rules.
        let mut allowed = _mm_or_si128(is_lower, is_digit);
        allowed = _mm_or_si128(allowed, is_dot);
        allowed = _mm_or_si128(allowed, is_slash);
        allowed = _mm_or_si128(allowed, is_dash);
        allowed = _mm_or_si128(allowed, is_colon);

        // First, ensure every byte is either allowed or underscore.
        let allowed_or_underscore = _mm_or_si128(allowed, is_underscore);
        if _mm_movemask_epi8(allowed_or_underscore) != SIMD_ALL_LANES_MASK {
            return false;
        }

        let allowed_mask = _mm_movemask_epi8(allowed);
        if trailing_underscore && (allowed_mask & 1) == 0 {
            return false;
        }
        trailing_underscore = false;

        // For each underscore in lanes 0..14, the immediately following byte
        // must be "allowed" (lower/digit/dot/slash/dash/colon).
        let underscore_mask = _mm_movemask_epi8(is_underscore);
        let required_followers = (underscore_mask & SIMD_MASK_EXCEPT_LAST) << 1;
        if (required_followers & !allowed_mask) != 0 {
            return false;
        }

        if (underscore_mask & SIMD_LAST_LANE_MASK) != 0 {
            let next_idx = i + SIMD_CHUNK_SIZE;
            if next_idx < len {
                if !IS_VALID_ASCII_TAG_CHAR_LOOKUP[bytes[next_idx] as usize] {
                    return false;
                }
            } else {
                trailing_underscore = true;
            }
        }

        i += SIMD_CHUNK_SIZE;
    }

    // handle remaining bytes and any trailing underscore from the last chunk.
    while i < len {
        let b = bytes[i];
        if trailing_underscore {
            // The byte after an underscore must be an allowed non-underscore character.
            if !IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
                return false;
            }
            trailing_underscore = false;
            i += 1;
            continue;
        }
        if IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
            i += 1;
            continue;
        }
        if b == b'_' {
            trailing_underscore = true;
            i += 1;
            continue;
        }
        return false;
    }

    // Reject a trailing underscore at end-of-input.
    !trailing_underscore
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
#[target_feature(enable = "neon")]
unsafe fn is_normalized_ascii_tag_simd_neon(bytes: &[u8], start: usize) -> bool {
    // this function is equivalent to the `is_normalized_ascii_tag_simd_sse2` function and the `is_normalized_ascii_tag_simd_sse2` function contains the comments explaining the logic.
    #[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    SIMD_HIT_COUNT.fetch_add(1, Ordering::Relaxed);

    let mut i = start;
    let len = bytes.len();
    let mut trailing_underscore = false;

    let ascii_high_bit = vdupq_n_u8(0x80);
    let lower_min = vdupq_n_u8(b'a');
    let lower_max = vdupq_n_u8(b'z');
    let digit_min = vdupq_n_u8(b'0');
    let digit_max = vdupq_n_u8(b'9');
    let dot = vdupq_n_u8(b'.');
    let slash = vdupq_n_u8(b'/');
    let dash = vdupq_n_u8(b'-');
    let colon = vdupq_n_u8(b':');
    let underscore = vdupq_n_u8(b'_');
    let zero = vdupq_n_u8(0);

    // Validate chunks and enforce underscore follower rules.
    while i + SIMD_CHUNK_SIZE <= len {
        let chunk = vld1q_u8(bytes.as_ptr().add(i));

        if vmaxvq_u8(vandq_u8(chunk, ascii_high_bit)) != 0 {
            return false;
        }

        let is_lower = vandq_u8(vcgeq_u8(chunk, lower_min), vcleq_u8(chunk, lower_max));
        let is_digit = vandq_u8(vcgeq_u8(chunk, digit_min), vcleq_u8(chunk, digit_max));
        let is_dot = vceqq_u8(chunk, dot);
        let is_slash = vceqq_u8(chunk, slash);
        let is_dash = vceqq_u8(chunk, dash);
        let is_colon = vceqq_u8(chunk, colon);
        let is_underscore = vceqq_u8(chunk, underscore);

        let mut allowed = vorrq_u8(is_lower, is_digit);
        allowed = vorrq_u8(allowed, is_dot);
        allowed = vorrq_u8(allowed, is_slash);
        allowed = vorrq_u8(allowed, is_dash);
        allowed = vorrq_u8(allowed, is_colon);

        let allowed_or_underscore = vorrq_u8(allowed, is_underscore);
        if vminvq_u8(allowed_or_underscore) != u8::MAX {
            return false;
        }

        if trailing_underscore && vgetq_lane_u8(allowed, NEON_FIRST_LANE) == 0 {
            return false;
        }
        trailing_underscore = false;

        let required_followers = vextq_u8(zero, is_underscore, NEON_FOLLOWER_SHIFT_BYTES);
        let missing_followers = vandq_u8(required_followers, vmvnq_u8(allowed));
        if vmaxvq_u8(missing_followers) != 0 {
            return false;
        }

        if vgetq_lane_u8(is_underscore, NEON_LAST_LANE) != 0 {
            let next_idx = i + SIMD_CHUNK_SIZE;
            if next_idx < len {
                if !IS_VALID_ASCII_TAG_CHAR_LOOKUP[bytes[next_idx] as usize] {
                    return false;
                }
            } else {
                trailing_underscore = true;
            }
        }

        i += SIMD_CHUNK_SIZE;
    }

    // Handle remaining bytes and any trailing underscore from the last chunk.
    while i < len {
        let b = bytes[i];
        if trailing_underscore {
            if !IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
                return false;
            }
            trailing_underscore = false;
            i += 1;
            continue;
        }
        if IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
            i += 1;
            continue;
        }
        if b == b'_' {
            trailing_underscore = true;
            i += 1;
            continue;
        }
        return false;
    }

    !trailing_underscore
}

const fn is_valid_ascii_start_char(c: char) -> bool {
    c.is_ascii_lowercase() || c == ':'
}

const fn is_valid_ascii_tag_char(c: char) -> bool {
    is_valid_ascii_start_char(c) || (c >= '0' && c <= '9') || c == '.' || c == '/' || c == '-'
}

/// Truncate string to max_len bytes, respecting UTF-8 boundaries.
pub(super) fn truncate_utf8(s: &MetaString, max_len: usize) -> &str {
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
        for (service, expected) in base_service_cases().iter() {
            let normalized = normalize_service(service);
            assert_eq!(normalized.as_ref(), expected.as_ref(), "service {}", service);
        }

        let empty = normalize_service(&MetaString::from(""));
        assert_eq!(empty.as_ref(), DEFAULT_SERVICE_NAME.as_ref());
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

    #[cfg(all(target_arch = "aarch64", not(miri)))]
    #[test]
    fn test_neon_simd_matches_scalar_curated_cases() {
        let mut chunk_boundary_valid = b"aaaaaaaaaaaaaaa_".to_vec();
        chunk_boundary_valid.extend_from_slice(b"bccccccccccccccc");

        let mut chunk_boundary_invalid = b"aaaaaaaaaaaaaaa_".to_vec();
        chunk_boundary_invalid.extend_from_slice(b"_ccccccccccccccc");

        let mut start_offset_valid = b":aaaaaaaaaaaaaaa_".to_vec();
        start_offset_valid.push(b'b');

        let mut start_offset_invalid = b":aaaaaaaaaaaaaaa_".to_vec();
        start_offset_invalid.push(b'_');

        let cases: Vec<(Vec<u8>, usize)> = vec![
            (b"abcdefghijklmnopabcdefghijklmnop".to_vec(), 0),
            (b"abc_def".to_vec(), 0),
            (b"abc__def".to_vec(), 0),
            (b"abc_".to_vec(), 0),
            (chunk_boundary_valid, 0),
            (chunk_boundary_invalid, 0),
            (vec![b'a', b'b', b'c', 0xFF, b'd'], 0),
            (start_offset_valid, 1),
            (start_offset_invalid, 1),
        ];

        for (bytes, start) in cases {
            let scalar = is_normalized_ascii_tag_scalar(&bytes, start);
            // SAFETY: AArch64 guarantees NEON support, and tests pass valid slice bounds.
            let neon = unsafe { is_normalized_ascii_tag_simd_neon(&bytes, start) };
            assert_eq!(
                neon, scalar,
                "NEON/Scalar mismatch for bytes {:?} at start {}",
                bytes, start
            );
        }
    }
}
