use super::BorrowedTag;

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

/// An iterator for splitting tags out of an input byte slice.
///
/// Extracts individual tags from the input byte slice by splitting on the comma (`,`, 0x2C) character.
///
/// ## Cloning
///
/// `TagSplitter` can be cloned to create a new iterator with its own iteration state. The same underlying input byte
/// slice is retained.
#[derive(Clone)]
pub struct RawTagsSplitter<'a> {
    raw_tags: &'a [u8],
    max_tag_count: usize,
    max_tag_len: usize,
}

impl<'a> RawTagsSplitter<'a> {
    /// Creates a new `TagSplitter` from the given input byte slice.
    ///
    /// The maximum tag count and maximum tag length control how many tags are returned from the iterator and their
    /// length. If the iterator encounters more tags than the maximum count, it will simply stop returning tags. If the
    /// iterator encounters any tag that is longer than the maximum length, it will truncate the tag to configured
    /// length, or to a smaller length, whichever is closer to a valid UTF-8 character boundary.
    ///
    /// # Safety
    ///
    /// The caller is responsible for ensuring that the byte slice given contains only valid UTF-8 data.
    pub const unsafe fn new(raw_tags: &'a [u8], max_tag_count: usize, max_tag_len: usize) -> Self {
        Self {
            raw_tags,
            max_tag_count,
            max_tag_len,
        }
    }

    /// Creates an empty `TagSplitter`.
    pub const fn empty() -> Self {
        Self {
            raw_tags: &[],
            max_tag_count: 0,
            max_tag_len: 0,
        }
    }
}

impl<'a> IntoIterator for &RawTagsSplitter<'a> {
    type Item = BorrowedTag<'a>;
    type IntoIter = RawTagsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        RawTagsIter {
            raw_tags: self.raw_tags,
            parsed_tags: 0,
            max_tag_len: self.max_tag_len,
            max_tag_count: self.max_tag_count,
        }
    }
}

pub struct RawTagsIter<'a> {
    raw_tags: &'a [u8],
    parsed_tags: usize,
    max_tag_len: usize,
    max_tag_count: usize,
}

impl<'a> Iterator for RawTagsIter<'a> {
    type Item = BorrowedTag<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let (raw_tag, tail) = split_at_delimiter(self.raw_tags, b',')?;
        self.raw_tags = tail;

        if self.parsed_tags >= self.max_tag_count {
            // We've reached the maximum number of tags, so we just skip the rest.
            return None;
        }

        // SAFETY: The caller that creates `TagSplitter` is responsible for ensuring that the entire byte slice is
        // valid UTF-8, which means we should also have valid UTF-8 here since only `TagSplitter` creates `TagIter`.
        let tag = unsafe { std::str::from_utf8_unchecked(raw_tag) };
        let tag = limit_str_to_len(tag, self.max_tag_len);

        self.parsed_tags += 1;

        Some(BorrowedTag::from(tag))
    }
}

// TODO: I don't love that this is duplicated between `saluki-io` in the DSD codec module and here... but I think it's
// cleaner to have `TagSplitter<'a>` here to make working with it for `ChainedTags<'a>` easier.
#[inline]
fn split_at_delimiter(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    match memchr::memchr(delimiter, input) {
        Some(index) => Some((&input[0..index], &input[index + 1..input.len()])),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, &[]))
            }
        }
    }
}
