use std::marker::PhantomData;

use crate::{collections::PrehashedHashSet, hash::hash_single_fast};

/// A helper for deduplicating items in an iterator while amortizing the storage cost of deduplication.
///
/// When deduplicating items in an iterator, a set of which items have been seen must be kept. This involves underlying
/// storage that grows as more items are seen for the first time. When many iterators are being deduplicated, this
/// storage cost can add up in terms of the number of underlying allocations, and resizing operations of the set.
///
/// `ReusableDeduplicator` allows for reuse of the underlying set between iterations.
///
/// # Safety
///
/// In order to support reusing the underlying storage between iterations, without having to clone the items, we
/// directly hash items and track the presence of their _hash_ value. This is a subtle distinction compared to using the
/// items themselves, as it means that we lose the typical fallback of comparing items for equality if there is a hash
/// collision detected. Consistent with other code in this repository that deals with things like metric tags, we use a
/// high-quality hash function and make a calculated bet that collisions are _extremely_ unlikely.
#[derive(Debug)]
pub struct ReusableDeduplicator<T> {
    seen: PrehashedHashSet<u64>,
    _item: PhantomData<T>,
}

impl<T: Eq + std::hash::Hash> ReusableDeduplicator<T> {
    /// Create a new `ReusableDeduplicator`.
    pub fn new() -> Self {
        Self {
            seen: PrehashedHashSet::default(),
            _item: PhantomData,
        }
    }

    /// Creates a wrapper iterator over the given iterator that deduplicates the items.
    pub fn deduplicated<'item, I>(&mut self, iter: I) -> Deduplicated<'_, 'item, I, T>
    where
        I: Iterator<Item = &'item T>,
    {
        self.seen.clear();

        Deduplicated {
            iter,
            seen: &mut self.seen,
        }
    }
}

/// An iterator that deduplicates items based on their hash values.
pub struct Deduplicated<'seen, 'item, I, T>
where
    I: Iterator<Item = &'item T>,
    T: Eq + std::hash::Hash + 'item,
{
    iter: I,
    seen: &'seen mut PrehashedHashSet<u64>,
}

impl<'seen, 'item, I, T> Iterator for Deduplicated<'seen, 'item, I, T>
where
    I: Iterator<Item = &'item T>,
    T: Eq + std::hash::Hash + 'item,
{
    type Item = &'item T;

    fn next(&mut self) -> Option<Self::Item> {
        for item in self.iter.by_ref() {
            let hash = hash_single_fast(item);
            if !self.seen.contains(&hash) {
                self.seen.insert(hash);
                return Some(item);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::{prelude::*, proptest};

    use super::*;

    #[test]
    fn basic_no_duplicates() {
        let mut deduplicator = ReusableDeduplicator::new();

        let input = vec![1, 2, 3];
        assert!(!input.is_empty());

        let expected = input.clone();
        let actual = deduplicator.deduplicated(input.iter()).copied().collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn basic_duplicates() {
        let mut deduplicator = ReusableDeduplicator::new();

        let input = vec![1, 3, 2, 1, 3, 2, 3, 1];
        assert!(!input.is_empty());

        let mut expected = input.clone();
        expected.sort();
        expected.dedup();

        // We have to sort the deduplicated results otherwise it might not match the order of `expected`, which
        // we had to sort to actually deduplicate it.
        let mut actual = deduplicator.deduplicated(input.iter()).copied().collect::<Vec<_>>();
        actual.sort();

        assert_eq!(actual, expected);
    }

    #[test]
    fn empty() {
        let mut deduplicator = ReusableDeduplicator::<i32>::new();

        let input = [];
        assert!(input.is_empty());

        let actual = deduplicator.deduplicated(input.iter()).copied().collect::<Vec<_>>();
        assert!(actual.is_empty());
    }

    #[test]
    fn overlapping_seen_with_reuse() {
        // We're specifically exercising here that the deduplicator clears its set storage after each iteration
        // so that subsequent iterations do not incorrectly exclude elements that were seen in a previous iteration.
        let mut deduplicator = ReusableDeduplicator::new();

        // Create two sets of inputs that have an overlap, and assert that they have an overlap:
        let input1 = vec![1, 2, 3, 4, 5];
        assert!(!input1.is_empty());

        let input2 = vec![4, 5, 6, 7, 8];
        assert!(!input2.is_empty());

        let input1_set = input1.iter().collect::<HashSet<_>>();
        let input2_set = input2.iter().collect::<HashSet<_>>();
        assert!(!input1_set.is_disjoint(&input2_set));

        // Run both sets of inputs through the deduplicator as separate usages:
        let expected1 = input1.clone();
        let actual1 = deduplicator.deduplicated(input1.iter()).copied().collect::<Vec<_>>();
        assert_eq!(actual1, expected1);

        let expected2 = input2.clone();
        let actual2 = deduplicator.deduplicated(input2.iter()).copied().collect::<Vec<_>>();
        assert_eq!(actual2, expected2);
    }

    proptest! {
        #[test]
        fn property_test_basic(input in any::<Vec<i32>>()) {
            let mut deduplicator = ReusableDeduplicator::new();

            let mut expected = input.clone();
            expected.sort();
            expected.dedup();

            // We have to sort the deduplicated results otherwise it might not match the order of `expected`, which
            // we had to sort to actually deduplicate it.
            let mut actual = deduplicator.deduplicated(input.iter()).copied().collect::<Vec<_>>();
            actual.sort();

            prop_assert_eq!(actual, expected);
        }
    }
}
