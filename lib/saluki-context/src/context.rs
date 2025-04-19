use std::{
    fmt, hash,
    sync::{
        atomic::{
            AtomicUsize,
            Ordering::{Acquire, Relaxed, Release},
        },
        Arc,
    },
};

use metrics::Gauge;
use stringtheory::MetaString;

use crate::{
    hash::{hash_context, ContextKey},
    origin::{OriginKey, OriginTags, OriginTagsResolver},
    tags::{Tag, TagSet, Tagged},
};

const BASE_CONTEXT_SIZE: usize = std::mem::size_of::<Context>() + std::mem::size_of::<ContextInner>();

// We use `usize::MAX` as the default size of the origin tags, as this is a sentinel value that indicates that the size
// has not yet been calculated. The size of the origin tags should not ever, ever, ever be `usize::MAX`, so this is a
// safe value to use.
const UNINITIALIZED_ORIGIN_TAGS_SIZE: usize = usize::MAX;

/// A metric context.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Creates a new `Context` from the given static name.
    pub fn from_static_name(name: &'static str) -> Self {
        const EMPTY_TAGS: &[&str] = &[];

        let key = hash_context(name, EMPTY_TAGS, None);
        Self {
            inner: Arc::new(ContextInner {
                name: MetaString::from_static(name),
                tags: TagSet::default(),
                origin_tags: OriginTags::empty(),
                origin_tags_size: AtomicUsize::new(0),
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Creates a new `Context` from the given static name and given static tags.
    pub fn from_static_parts(name: &'static str, tags: &[&'static str]) -> Self {
        let mut tag_set = TagSet::with_capacity(tags.len());
        for tag in tags {
            tag_set.insert_tag(MetaString::from_static(tag));
        }

        let key = hash_context(name, tags, None);
        Self {
            inner: Arc::new(ContextInner {
                name: MetaString::from_static(name),
                tags: tag_set,
                origin_tags: OriginTags::empty(),
                origin_tags_size: AtomicUsize::new(0),
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Creates a new `Context` from the given name and given tags.
    pub fn from_parts<S: Into<MetaString>>(name: S, tags: TagSet) -> Self {
        let name = name.into();
        let key = hash_context(&name, &tags, None);
        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                origin_tags: OriginTags::empty(),
                origin_tags_size: AtomicUsize::new(0),
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Clones this context, and uses the given name for the cloned context.
    pub fn with_name<S: Into<MetaString>>(&self, name: S) -> Self {
        // Regenerate the context key to account for the new name.
        let name = name.into();
        let tags = self.inner.tags.clone();
        let key = hash_context(&name, &tags, None);

        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                origin_tags: self.inner.origin_tags.clone(),
                origin_tags_size: AtomicUsize::new(self.inner.origin_tags_size.load(Relaxed)),
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Clones this context, and updates the origin tags using the given origin key and origin tags resolver.
    pub fn with_origin(&self, origin_key: OriginKey, origin_tags_resolver: Arc<dyn OriginTagsResolver>) -> Self {
        // Regenerate the context key to account for the new origin tags.
        let name = self.inner.name.clone();
        let tags = self.inner.tags.clone();
        let key = hash_context(&name, &tags, Some(origin_key));
        let origin_tags = OriginTags::from_resolved(origin_key, origin_tags_resolver);

        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                origin_tags,
                origin_tags_size: AtomicUsize::new(UNINITIALIZED_ORIGIN_TAGS_SIZE),
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    pub(crate) fn from_inner(inner: ContextInner) -> Self {
        Self { inner: Arc::new(inner) }
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns the name of this context.
    pub fn name(&self) -> &MetaString {
        &self.inner.name
    }

    /// Returns the instrumented tags of this context.
    pub fn tags(&self) -> &TagSet {
        &self.inner.tags
    }

    /// Returns the origin tags of this context.
    pub fn origin_tags(&self) -> &OriginTags {
        &self.inner.origin_tags
    }

    /// Returns the size of this context in bytes.
    ///
    /// A context's size is the sum of the sizes of its fields and the size of the `Context` struct itself, and
    /// includes:
    /// - the context name
    /// - the context tags (both instrumented and origin)
    ///
    /// Since origin tags can potentially be expensive to calculate, this method will cache the size of the origin tags
    /// when this method is first called.
    ///
    /// Additionally, the value returned by this method does not compensate for externalities such as origin tags
    /// potentially being shared by multiple contexts, or whether or not tags are are inlined, interned, or heap
    /// allocated. This means that the value returned is essentially the worst-case usage, and should be used as a rough
    /// estimate.
    pub fn size_of(&self) -> usize {
        let name_size = self.inner.name.len();
        let tags_size = self.inner.tags.size_of();
        let origin_tags_size = match self.inner.origin_tags_size.load(Acquire) {
            UNINITIALIZED_ORIGIN_TAGS_SIZE => {
                let size = self.inner.origin_tags.size_of();
                self.inner.origin_tags_size.store(size, Release);
                size
            }
            size => size,
        };

        BASE_CONTEXT_SIZE + name_size + tags_size + origin_tags_size
    }
}

impl From<&'static str> for Context {
    fn from(name: &'static str) -> Self {
        Self::from_static_name(name)
    }
}

impl<'a> From<(&'static str, &'a [&'static str])> for Context {
    fn from((name, tags): (&'static str, &'a [&'static str])) -> Self {
        Self::from_static_parts(name, tags)
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner.name)?;
        if !self.inner.tags.is_empty() {
            write!(f, "{{")?;

            let mut needs_separator = false;
            for tag in &self.inner.tags {
                if needs_separator {
                    write!(f, ", ")?;
                } else {
                    needs_separator = true;
                }

                write!(f, "{}", tag)?;
            }

            write!(f, "}}")?;
        }

        Ok(())
    }
}

impl Tagged for Context {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&Tag),
    {
        self.tags().visit_tags(&mut visitor);
        self.origin_tags().visit_tags(&mut visitor);
    }
}

pub(super) struct ContextInner {
    key: ContextKey,
    name: MetaString,
    tags: TagSet,
    origin_tags: OriginTags,
    origin_tags_size: AtomicUsize,
    active_count: Gauge,
}

impl ContextInner {
    pub fn from_parts(
        key: ContextKey, name: MetaString, tags: TagSet, origin_tags: OriginTags, active_count: Gauge,
    ) -> Self {
        Self {
            key,
            name,
            tags,
            origin_tags,
            origin_tags_size: AtomicUsize::new(UNINITIALIZED_ORIGIN_TAGS_SIZE),
            active_count,
        }
    }
}

impl Clone for ContextInner {
    fn clone(&self) -> Self {
        Self {
            key: self.key,
            name: self.name.clone(),
            tags: self.tags.clone(),
            origin_tags: self.origin_tags.clone(),
            origin_tags_size: AtomicUsize::new(self.origin_tags_size.load(Relaxed)),

            // We're specifically detaching this context from the statistics of the resolver from which `self`
            // originated, as we only want to track the statistics of the contexts created _directly_ through the
            // resolver.
            active_count: Gauge::noop(),
        }
    }
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        self.active_count.decrement(1);
    }
}

impl PartialEq<ContextInner> for ContextInner {
    fn eq(&self, other: &ContextInner) -> bool {
        // TODO: Note about why we consider the hash good enough for equality.
        self.key == other.key
    }
}

impl Eq for ContextInner {}

impl hash::Hash for ContextInner {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl fmt::Debug for ContextInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextInner")
            .field("name", &self.name)
            .field("tags", &self.tags)
            .field("key", &self.key)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{
        origin::{OriginKey, OriginTagsResolver, RawOrigin},
        tags::TagVisitor,
    };

    const SIZE_OF_CONTEXT_NAME: &str = "size_of_test_metric";
    const SIZE_OF_CONTEXT_CHANGED_NAME: &str = "size_of_test_metric_changed";
    const SIZE_OF_CONTEXT_TAGS: &[&str] = &["size_of_test_tag1", "size_of_test_tag2"];
    const SIZE_OF_CONTEXT_ORIGIN_TAGS: &[&str] = &["size_of_test_origin_tag1", "size_of_test_origin_tag2"];

    fn tag_set(tags: &[&str]) -> TagSet {
        tags.iter().map(|s| Tag::from(*s)).collect::<TagSet>()
    }

    #[derive(Clone, Default)]
    struct MockOriginTagsResolver {
        tags: Arc<Mutex<TagSet>>,
    }

    impl MockOriginTagsResolver {
        fn set_tags(&self, tag_set: TagSet) {
            let mut tags = self.tags.lock().unwrap();
            *tags = tag_set;
        }
    }

    impl OriginTagsResolver for MockOriginTagsResolver {
        fn resolve_origin_key(&self, _: RawOrigin<'_>) -> Option<OriginKey> {
            // Doesn't matter. We never call this.
            None
        }

        fn visit_origin_tags(&self, _: OriginKey, visitor: &mut dyn TagVisitor) {
            // Visit our internal tags, regardless of origin key.
            self.tags.lock().unwrap().visit_tags(|tag| {
                visitor.visit_tag(tag);
            });
        }
    }

    #[test]
    fn size_of_context_from_static_name() {
        let context = Context::from_static_name(SIZE_OF_CONTEXT_NAME);
        assert_eq!(context.size_of(), BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_NAME.len());
    }

    #[test]
    fn size_of_context_from_static_parts() {
        let tags = tag_set(SIZE_OF_CONTEXT_TAGS);

        let context = Context::from_static_parts(SIZE_OF_CONTEXT_NAME, SIZE_OF_CONTEXT_TAGS);
        assert_eq!(
            context.size_of(),
            BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_NAME.len() + tags.size_of()
        );
    }

    #[test]
    fn size_of_context_from_parts() {
        let tags = tag_set(SIZE_OF_CONTEXT_TAGS);

        let context = Context::from_parts(SIZE_OF_CONTEXT_NAME, tags.clone());
        assert_eq!(
            context.size_of(),
            BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_NAME.len() + tags.size_of()
        );
    }

    #[test]
    fn size_of_context_with_name() {
        // Check the check after `with_name` when there's both tags and no tags.
        let context = Context::from_static_name(SIZE_OF_CONTEXT_NAME).with_name(SIZE_OF_CONTEXT_CHANGED_NAME);
        assert_eq!(
            context.size_of(),
            BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_CHANGED_NAME.len()
        );

        let tags = tag_set(SIZE_OF_CONTEXT_TAGS);

        let context = Context::from_static_parts(SIZE_OF_CONTEXT_NAME, SIZE_OF_CONTEXT_TAGS)
            .with_name(SIZE_OF_CONTEXT_CHANGED_NAME);
        assert_eq!(
            context.size_of(),
            BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_CHANGED_NAME.len() + tags.size_of()
        );
    }

    #[test]
    fn size_of_context_origin_tags() {
        let tags = tag_set(SIZE_OF_CONTEXT_TAGS);

        let tags_resolver = MockOriginTagsResolver::default();
        tags_resolver.set_tags(tag_set(SIZE_OF_CONTEXT_ORIGIN_TAGS));

        // We're manually constructing our context here because we don't want to add an entire builder method to handle
        // this test case, and it's annoying with how interconnected the hashing of the context key is with the origin
        // key, and all of that.
        //
        // We just care about getting our mocked `OriginTags` into the right place.
        let shared_tags_resolver = Arc::new(tags_resolver.clone());

        let origin_key = OriginKey::from_opaque(42);
        let origin_tags = OriginTags::from_resolved(origin_key, shared_tags_resolver);

        let context = Context::from_inner(ContextInner {
            key: hash_context(SIZE_OF_CONTEXT_NAME, SIZE_OF_CONTEXT_TAGS, None),
            name: MetaString::from_static(SIZE_OF_CONTEXT_NAME),
            tags: tags.clone(),
            origin_tags: origin_tags.clone(),
            origin_tags_size: AtomicUsize::new(UNINITIALIZED_ORIGIN_TAGS_SIZE),
            active_count: Gauge::noop(),
        });

        // Check the first of the size of the context when the origin tags size hasn't yet been cached.
        let initial_context_size = context.size_of();
        assert_eq!(
            initial_context_size,
            BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_NAME.len() + tags.size_of() + origin_tags.size_of()
        );

        // Update the mock origin tags resolver with a different set of tags.
        //
        // We'll capture the before/after size of the origin tags to ensure that the size has changed, which ensures
        // that when we check the size of the context again, we know that caching is working if the size stays the same
        // as the first time around.
        let initial_origin_tags_len = origin_tags.size_of();
        tags_resolver.set_tags(tags.clone());

        assert_ne!(initial_origin_tags_len, origin_tags.size_of());
        assert_eq!(initial_context_size, context.size_of());
    }
}
