//! Shared `#[cfg(test)]` fixtures for the interner implementations.
//!
//! `FixedSizeInterner` and `GenericMapInterner` share identical property-test scaffolding: the
//! `arb_alphanum_strings` generator and the entry-count invariant. Keeping a single copy here -- rather than a
//! byte-identical copy in each module -- stops the two suites from silently drifting apart.
//!
//! Each interner still owns its own `property_test_`-prefixed entry point, so CI routing (which selects tests by the
//! `property_test_` prefix) and shrink reporting stay per-implementation. Those entry points build their
//! implementation-specific interner and delegate to the shared helper below.

use std::{collections::HashSet, ops::RangeInclusive};

use prop::sample::Index;
use proptest::{
    collection::{hash_set, vec as arb_vec},
    prelude::*,
    test_runner::TestCaseError,
};

use super::Interner;

/// Generates a `Vec` of distinct printable-ASCII strings.
///
/// Each string's length is drawn from `str_len` (bytes) and the number of distinct strings from `unique_strs`.
pub(crate) fn arb_alphanum_strings(
    str_len: RangeInclusive<usize>, unique_strs: RangeInclusive<usize>,
) -> impl Strategy<Value = Vec<String>> {
    // Create characters between 0x20 (32) and 0x7E (126), which are all printable ASCII characters.
    let char_gen = any::<u8>().prop_map(|c| std::cmp::max(c % 127, 32));

    let str_gen = any::<usize>()
        .prop_map(move |n| std::cmp::max(n % *str_len.end(), *str_len.start()))
        .prop_flat_map(move |len| arb_vec(char_gen.clone(), len))
        // SAFETY: We know our characters are all valid UTF-8 because they're in the ASCII range.
        .prop_map(|xs| unsafe { String::from_utf8_unchecked(xs) });

    // Create a hash set, which handles the deduplication aspect for us, ensuring we have N unique strings where N
    // is within the `unique_strs` range... and then convert it to `Vec<String>` for easier consumption.
    hash_set(str_gen, unique_strs).prop_map(|unique_strs| unique_strs.into_iter().collect::<Vec<_>>())
}

/// Shared body of each interner's `property_test_entry_count_accurate`.
///
/// Selects strings from `strs` by `indices` (so the same string is interned multiple times) and asserts that the
/// interner's entry count equals the number of distinct strings selected: for example, re-interning a string that is
/// already present must not inflate the count. The interned handles are held alive for the duration of the check so
/// that no entry can be reclaimed mid-way and skew the count.
pub(crate) fn assert_entry_count_matches_unique_strings<I: Interner>(
    interner: &I, strs: &[String], indices: &[Index],
) -> Result<(), TestCaseError> {
    let mut interned = Vec::new();
    let mut unique_strs = HashSet::new();
    for index in indices {
        let s = index.get(strs);
        unique_strs.insert(s);

        let s_interned = interner.try_intern(s).expect("should never fail to intern");
        interned.push(s_interned);
    }

    prop_assert_eq!(unique_strs.len(), interner.len());
    Ok(())
}
