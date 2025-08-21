/// A wrapper over raw tags in their unprocessed form.
///
/// This type is meant to handle raw tags that have been extracted from network payloads, such as DogStatsD, where the
/// input byte slice contains the tags -- whether bare or key/value -- packed together and separated by commas (`,`,
/// 0x2C) character.
///
/// `RawTags` supports iteration over these tags in an efficient, zero-copy fashion and returns string references to
/// each individual tag. It supports configuration to control how many tags can be returned, and the maximum allowable
/// length for a tag. This allows easy usage where limits must be enforced, without having to write additional code to
/// filter the resulting iterator.
///
/// ## Cloning
///
/// `RawTags` can be cloned to create a new iterator with its own iteration state. The same underlying input byte slice
/// is retained.
#[derive(Clone)]
pub struct RawTags<'a> {
    raw_tags: &'a str,
    max_tag_count: usize,
    max_tag_len: usize,
}

impl<'a> RawTags<'a> {
    /// Creates a new `RawTags` from the given input byte slice.
    ///
    /// The maximum tag count and maximum tag length control how many tags are returned from the iterator and their
    /// length. If the iterator encounters more tags than the maximum count, it will simply stop returning tags. If the
    /// iterator encounters any tag that is longer than the maximum length, it will truncate the tag to configured
    /// length, or to a smaller length, whichever is closer to a valid UTF-8 character boundary.
    pub const fn new(raw_tags: &'a str, max_tag_count: usize, max_tag_len: usize) -> Self {
        Self {
            raw_tags,
            max_tag_count,
            max_tag_len,
        }
    }

    /// Creates an empty `RawTags`.
    pub const fn empty() -> Self {
        Self {
            raw_tags: "",
            max_tag_count: 0,
            max_tag_len: 0,
        }
    }

    fn tags_iter(&self) -> RawTagsIter<'a> {
        RawTagsIter {
            raw_tags: self.raw_tags,
            parsed_tags: 0,
            max_tag_len: self.max_tag_len,
            max_tag_count: self.max_tag_count,
        }
    }
}

impl<'a> IntoIterator for RawTags<'a> {
    type Item = &'a str;
    type IntoIter = RawTagsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.tags_iter()
    }
}

#[derive(Clone)]
pub struct RawTagsIter<'a> {
    raw_tags: &'a str,
    parsed_tags: usize,
    max_tag_len: usize,
    max_tag_count: usize,
}

impl<'a> Iterator for RawTagsIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let (raw_tag, tail) = split_at_delimiter(self.raw_tags, b',')?;
        self.raw_tags = tail;

        if self.parsed_tags >= self.max_tag_count {
            // We've reached the maximum number of tags, so we just skip the rest.
            return None;
        }

        let tag = limit_str_to_len(raw_tag, self.max_tag_len);

        self.parsed_tags += 1;

        Some(tag)
    }
}

/// A trait for a predicate that can be used to filter raw tags.
pub trait RawTagsFilterPredicate {
    /// Returns `true` if the given tag matches the predicate.
    fn matches(&self, tag: &str) -> bool;
}

impl<F> RawTagsFilterPredicate for F
where
    F: Fn(&str) -> bool,
{
    fn matches(&self, tag: &str) -> bool {
        self(tag)
    }
}

/// An iterator that filters raw tags.
///
/// This iterator can be used to separate out a configured set of tags, in both directions, from a set of raw tags.
///
/// In providing logic to either include or exclude tags based on a predicate, we can ensure that callers can easily
/// extract a disjoint set of tags from a set of raw tags when utilizing the same predicate.
pub struct RawTagsFilter<'a, F> {
    raw: RawTagsIter<'a>,
    predicate: F,
    include: bool,
}

impl<'a, F> RawTagsFilter<'a, F> {
    /// Creates a new `RawTagsFilter` that includes only the tags that match the given predicate.
    pub fn include(raw: RawTags<'a>, predicate: F) -> RawTagsFilter<'a, F> {
        Self {
            raw: raw.into_iter(),
            predicate,
            include: true,
        }
    }

    /// Creates a new `RawTagsFilter` that excludes any tags that match the given predicate.
    pub fn exclude(raw: RawTags<'a>, predicate: F) -> RawTagsFilter<'a, F> {
        Self {
            raw: raw.into_iter(),
            predicate,
            include: false,
        }
    }
}

impl<'a, F> Clone for RawTagsFilter<'a, F>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            predicate: self.predicate.clone(),
            include: self.include,
        }
    }
}

impl<'a, F> Iterator for RawTagsFilter<'a, F>
where
    F: RawTagsFilterPredicate,
{
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        for raw_tag in self.raw.by_ref() {
            if self.predicate.matches(raw_tag) {
                if self.include {
                    // The predicate matched, and we're configured to include only matching tags.
                    return Some(raw_tag);
                }
            } else if !self.include {
                // The predicate did not match, but we're configured to exclude matching tags.
                return Some(raw_tag);
            }
        }

        None
    }
}

#[inline]
fn limit_str_to_len(s: &str, limit: usize) -> &str {
    if limit >= s.len() {
        s
    } else {
        let sb = s.as_bytes();

        // Search through the last four bytes of the string, ending at the index `limit`, and look for the byte that
        // defines the boundary of a full UTF-8 character.
        let start = limit.saturating_sub(3);
        let new_index = sb[start..=limit]
            .iter()
            // Bit twiddling magic for checking if `b` is < 128 or >= 192.
            .rposition(|b| (*b as i8) >= -0x40);

        // SAFETY: UTF-8 characters are a maximum of four bytes, so we know we will have found a valid character
        // boundary by searching over four bytes, regardless of where the slice started.
        //
        // Similarly we know that taking everything from index 0 to the detected character boundary index will be a
        // valid UTF-8 string.
        unsafe {
            let safe_end = start + new_index.unwrap_unchecked();
            std::str::from_utf8_unchecked(&sb[..safe_end])
        }
    }
}

#[inline]
fn split_at_delimiter(input: &str, delimiter: u8) -> Option<(&str, &str)> {
    match memchr::memchr(delimiter, input.as_bytes()) {
        Some(index) => Some((&input[0..index], &input[index + 1..input.len()])),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, ""))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use proptest::{collection::vec as arb_vec, prelude::*, proptest, sample::subsequence};

    use super::*;

    fn filter_key2_tag(tag: &str) -> bool {
        tag.starts_with("key2:")
    }

    fn arb_raw_tags_and_filters() -> impl Strategy<Value = (String, Vec<String>)> {
        // We want to create a set of random tags and then take a random subset of those tags.
        arb_vec("key[0-9]+:[a-zA-Z0-9]+", 1..=10).prop_flat_map(|raw_tags| {
            // Take a subsequence of up to half of the raw tags.
            let subset = 0..=raw_tags.len() / 2;
            (Just(raw_tags.clone().join(",")), subsequence(raw_tags, subset))
        })
    }

    #[test]
    fn raw_tags_filter_include() {
        let raw_tags_input = "key1:value1,key2:value2,key3:value3";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);
        let filtered_raw_tags = RawTagsFilter::include(raw_tags, filter_key2_tag);

        let filtered_tags = filtered_raw_tags.into_iter().collect::<Vec<_>>();
        assert_eq!(filtered_tags.len(), 1);
        assert_eq!(filtered_tags[0], "key2:value2");
    }

    #[test]
    fn raw_tags_filter_include_duplicates() {
        let raw_tags_input = "key1:value1,key2:value2,key3:value3,key2:value4";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);
        let filtered_raw_tags = RawTagsFilter::include(raw_tags, filter_key2_tag);

        let filtered_tags = filtered_raw_tags.into_iter().collect::<Vec<_>>();
        assert_eq!(filtered_tags.len(), 2);
        assert!(filtered_tags.contains(&"key2:value2"));
        assert!(filtered_tags.contains(&"key2:value4"));
    }

    #[test]
    fn raw_tags_filter_exclude() {
        let raw_tags_input = "key1:value1,key2:value2,key3:value3";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);
        let filtered_raw_tags = RawTagsFilter::exclude(raw_tags, filter_key2_tag);

        let filtered_tags = filtered_raw_tags.into_iter().collect::<Vec<_>>();
        assert_eq!(filtered_tags.len(), 2);
        assert_eq!(filtered_tags[0], "key1:value1");
        assert_eq!(filtered_tags[1], "key3:value3");
    }

    #[test]
    fn raw_tags_filter_exclude_duplicates() {
        let raw_tags_input = "key1:value1,key2:value2,key3:value3,key1:value9,key2:value6";
        let raw_tags = RawTags::new(raw_tags_input, usize::MAX, usize::MAX);
        let filtered_raw_tags = RawTagsFilter::exclude(raw_tags, filter_key2_tag);

        let filtered_tags = filtered_raw_tags.into_iter().collect::<Vec<_>>();
        assert_eq!(filtered_tags.len(), 3);
        assert_eq!(filtered_tags[0], "key1:value1");
        assert_eq!(filtered_tags[1], "key3:value3");
        assert_eq!(filtered_tags[2], "key1:value9");
    }

    proptest! {
        #[test]
        fn property_test_raw_tags_filter_include_exclude_dispoint((inputs, filters) in arb_raw_tags_and_filters()) {
            // We handle the raw tags, and the filtered tags, as hash sets.
            //
            // This is because we may have duplicate tags in the input, and so we need to discard all of thos duplicates
            // when ensuring that the resulting filtered sets are disjoint, and that they both add up to the original
            // set of raw tags.

            let raw_tags = RawTags::new(&inputs, usize::MAX, usize::MAX)
                .into_iter()
                .collect::<HashSet<_>>();
            let predicate = |tag: &str| filters.iter().any(|filter| tag.starts_with(filter));

            let raw_tags_include = RawTags::new(&inputs, usize::MAX, usize::MAX);
            let filtered_raw_tags_include = RawTagsFilter::include(raw_tags_include, predicate);

            let raw_tags_exclude = RawTags::new(&inputs, usize::MAX, usize::MAX);
            let filtered_raw_tags_exclude = RawTagsFilter::exclude(raw_tags_exclude, predicate);

            let filtered_tags_include = filtered_raw_tags_include.into_iter().collect::<HashSet<_>>();
            let filtered_tags_exclude = filtered_raw_tags_exclude.into_iter().collect::<HashSet<_>>();

            prop_assert!(filtered_tags_include.is_disjoint(&filtered_tags_exclude));

            // Combine the filtered tags into a single set, which should be equal to the original set of raw tags (deduplicated, of course).
            //
            // We check in both directions since `is_superset` only checks if the first set is a superset of the second set.
            let filtered_tags_combined = filtered_tags_include.union(&filtered_tags_exclude).copied().collect::<HashSet<_>>();
            prop_assert_eq!(filtered_tags_combined.symmetric_difference(&raw_tags).count(), 0);
        }
    }
}
