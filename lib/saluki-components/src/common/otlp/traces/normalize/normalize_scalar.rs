#[cfg(any(
    test,
    feature = "bench",
    not(any(target_arch = "x86", target_arch = "x86_64", all(target_arch = "aarch64", not(miri))))
))]
use stringtheory::MetaString;

#[cfg(any(
    test,
    feature = "bench",
    not(any(target_arch = "x86", target_arch = "x86_64", all(target_arch = "aarch64", not(miri))))
))]
use super::{is_normalized_ascii_tag_without_simd, normalize_with_ascii_fast_path};

#[cfg(any(
    test,
    feature = "bench",
    not(any(target_arch = "x86", target_arch = "x86_64", all(target_arch = "aarch64", not(miri))))
))]
pub(crate) fn normalize(value: &str, remove_digit_start_char: bool) -> MetaString {
    normalize_with_ascii_fast_path(value, remove_digit_start_char, is_normalized_ascii_tag_without_simd)
}
