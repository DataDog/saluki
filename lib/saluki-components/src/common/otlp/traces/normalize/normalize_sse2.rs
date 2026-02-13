#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
use std::sync::atomic::Ordering;

#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
use super::SIMD_HIT_COUNT;
use super::{IS_VALID_ASCII_TAG_CHAR_LOOKUP, SIMD_CHUNK_SIZE};

// Bitmask where all 16 lanes are set (0xFFFF = 0b1111_1111_1111_1111).
const SIMD_ALL_LANES_MASK: i32 = 0xFFFF;
// Bitmask for the last lane only (bit 15 -> 0x8000 = 0b1000_0000_0000_0000).
const SIMD_LAST_LANE_MASK: i32 = 0x8000;
// Bitmask for all lanes except the last (0x7FFF = 0b0111_1111_1111_1111).
const SIMD_MASK_EXCEPT_LAST: i32 = 0x7FFF;

#[target_feature(enable = "sse2")]
pub(super) unsafe fn is_normalized_ascii_tag_simd_sse2(bytes: &[u8], start: usize) -> bool {
    #[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    SIMD_HIT_COUNT.fetch_add(1, Ordering::Relaxed);

    let mut i = start;
    let len = bytes.len();
    let mut trailing_underscore = false;

    let lower_min = _mm_set1_epi8((b'a' - 1) as i8);
    let lower_max = _mm_set1_epi8((b'z' + 1) as i8);
    let digit_min = _mm_set1_epi8((b'0' - 1) as i8);
    let digit_max = _mm_set1_epi8((b'9' + 1) as i8);

    let dot = _mm_set1_epi8(b'.' as i8);
    let slash = _mm_set1_epi8(b'/' as i8);
    let dash = _mm_set1_epi8(b'-' as i8);
    let colon = _mm_set1_epi8(b':' as i8);
    let underscore = _mm_set1_epi8(b'_' as i8);

    while i + SIMD_CHUNK_SIZE <= len {
        let ptr = bytes.as_ptr().add(i) as *const __m128i;
        let chunk = _mm_loadu_si128(ptr);

        if _mm_movemask_epi8(chunk) != 0 {
            return false;
        }

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

        let mut allowed = _mm_or_si128(is_lower, is_digit);
        allowed = _mm_or_si128(allowed, is_dot);
        allowed = _mm_or_si128(allowed, is_slash);
        allowed = _mm_or_si128(allowed, is_dash);
        allowed = _mm_or_si128(allowed, is_colon);

        let allowed_or_underscore = _mm_or_si128(allowed, is_underscore);
        if _mm_movemask_epi8(allowed_or_underscore) != SIMD_ALL_LANES_MASK {
            return false;
        }

        let allowed_mask = _mm_movemask_epi8(allowed);
        if trailing_underscore && (allowed_mask & 1) == 0 {
            return false;
        }
        trailing_underscore = false;

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
