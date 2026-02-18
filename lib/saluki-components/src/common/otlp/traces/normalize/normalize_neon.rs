use std::arch::aarch64::*;
#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
use std::sync::atomic::Ordering;

#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
use super::SIMD_HIT_COUNT;
use super::{IS_VALID_ASCII_TAG_CHAR_LOOKUP, SIMD_CHUNK_SIZE};

const NEON_FIRST_LANE: i32 = 0;
const NEON_LAST_LANE: i32 = 15;
const NEON_FOLLOWER_SHIFT_BYTES: i32 = 15;

#[target_feature(enable = "neon")]
#[allow(missing_docs)]
pub unsafe fn is_normalized_ascii_tag_simd_neon(tag: &[u8]) -> bool {
    #[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    SIMD_HIT_COUNT.fetch_add(1, Ordering::Relaxed);

    let mut i = 0;
    let len = tag.len();
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

    while i + SIMD_CHUNK_SIZE <= len {
        let chunk = vld1q_u8(tag.as_ptr().add(i));

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

    while i < len {
        let b = tag[i];
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
