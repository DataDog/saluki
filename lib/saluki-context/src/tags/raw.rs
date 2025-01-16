use crate::Tagged;

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

impl<'a> Tagged for RawTags<'a> {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&str),
    {
        for tag in self.tags_iter() {
            visitor(tag);
        }
    }
}

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
