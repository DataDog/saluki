use super::{splitter::RawTagsIter, RawTags, SharedTagSet, Tag};

pub enum TagChunk<'a> {
    RawTags(RawTags<'a>),
    SharedTags(SharedTagSet),
}

impl<'a> From<RawTags<'a>> for TagChunk<'a> {
    fn from(raw: RawTags<'a>) -> Self {
        Self::RawTags(raw)
    }
}

impl<'a> From<SharedTagSet> for TagChunk<'a> {
    fn from(shared: SharedTagSet) -> Self {
        Self::SharedTags(shared)
    }
}

enum TagChunkIter<'a> {
    RawTags(RawTagsIter<'a>),
    SharedTags(std::slice::Iter<'a, Tag>),
}

impl<'a> TagChunkIter<'a> {
    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::RawTags(iter) => iter.next(),
            Self::SharedTags(iter) => iter.next().map(|tag| tag.as_str()),
        }
    }
}

/// A collection of tags that are chained together to allow for continuous iteration.
///
/// In some cases, callers may require iterating over all the relevant tags of a metric in a single pass, despite the
/// tags coming from different sources. This struct allows "chaining" together multiple tag sources and iterating over
/// them with a single concretely-typed iterator.
#[derive(Default)]
pub struct ChainedTags<'a> {
    chunks: Vec<TagChunk<'a>>,
}

impl<'a> ChainedTags<'a> {
    /// Push a new set of tags onto the chain.
    pub fn push_tags<T>(&mut self, tags: T)
    where
        T: Into<TagChunk<'a>>,
    {
        self.chunks.push(tags.into());
    }
}

impl<'a> IntoIterator for &'a ChainedTags<'a> {
    type Item = &'a str;
    type IntoIter = ChainedTagsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        ChainedTagsIter {
            chunks: self.chunks.iter(),
            current: None,
        }
    }
}

pub struct ChainedTagsIter<'a> {
    chunks: std::slice::Iter<'a, TagChunk<'a>>,
    current: Option<TagChunkIter<'a>>,
}

impl<'a> Iterator for ChainedTagsIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(current) = &mut self.current {
                if let Some(tag) = current.next() {
                    return Some(tag);
                }
            }

            self.current = Some(self.chunks.next().map(|chunk| match chunk {
                TagChunk::RawTags(splitter) => TagChunkIter::RawTags(splitter.into_iter()),
                TagChunk::SharedTags(tag_set) => TagChunkIter::SharedTags(tag_set.into_iter()),
            })?);
        }
    }
}
