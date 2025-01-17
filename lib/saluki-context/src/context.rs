use std::{
    fmt, hash,
    sync::{Arc, OnceLock},
};

use metrics::Gauge;
use stringtheory::MetaString;

use crate::{
    hash::{hash_context, ContextKey},
    tags::TagSet,
};

static DIRTY_CONTEXT_KEY: OnceLock<ContextKey> = OnceLock::new();

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
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Clones this context, and uses the given name for the cloned context.
    pub fn with_name<S: Into<MetaString>>(&self, name: S) -> Self {
        let name = name.into();
        let tags = self.inner.tags.clone();
        let key = hash_context(&name, &tags, None);

        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
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

    fn inner_mut(&mut self) -> &mut ContextInner {
        Arc::make_mut(&mut self.inner)
    }

    fn mark_dirty(&mut self) {
        let inner = self.inner_mut();
        inner.key = get_dirty_context_key_value();
    }

    /// Gets the name of this context.
    pub fn name(&self) -> &MetaString {
        &self.inner.name
    }

    /// Gets the tags of this context.
    pub fn tags(&self) -> &TagSet {
        &self.inner.tags
    }

    /// Gets a mutable reference to the tags of this context.
    pub fn tags_mut(&mut self) -> &mut TagSet {
        // Mark the context as dirty. We have to do this before giving back a mutable reference to the tags, which means
        // we are _potentially_ marking the context dirty even if nothing is changed about the tags.
        //
        // If this somehow became a problem, we could always move part of the hash to `TagSet` itself where we had
        // granular control and could mark ourselves dirty only when the tags were actually changed. Shouldn't matter
        // right now, though.
        self.mark_dirty();

        let inner = self.inner_mut();
        &mut inner.tags
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

pub struct ContextInner {
    pub key: ContextKey,
    pub name: MetaString,
    pub tags: TagSet,
    pub active_count: Gauge,
}

impl Clone for ContextInner {
    fn clone(&self) -> Self {
        Self {
            key: self.key,
            name: self.name.clone(),
            tags: self.tags.clone(),

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
        // If the context is dirty -- has changed since it was originally resolved -- then our cached key is now
        // invalid, so we need to re-hash the context. Otherwise, we can just use the cached key.
        let key = if is_context_dirty(self.key) {
            hash_context(&self.name, &self.tags, None)
        } else {
            self.key
        };

        key.hash(state);
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

/// A value containing tags that can be visited.
pub trait Tagged {
    /// Visits the tags in this value.
    fn visit_tags<F>(&self, visitor: F)
    where
        F: FnMut(&str);
}

impl<'a, T> Tagged for &'a T
where
    T: Tagged,
{
    fn visit_tags<F>(&self, visitor: F)
    where
        F: FnMut(&str),
    {
        (*self).visit_tags(visitor)
    }
}

impl<'a> Tagged for &'a [&'a str] {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&str),
    {
        for tag in self.iter() {
            visitor(tag);
        }
    }
}

impl<'a, const N: usize> Tagged for [&'a str; N] {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&str),
    {
        for tag in self.iter() {
            visitor(tag);
        }
    }
}

impl<'a> Tagged for &'a [MetaString] {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&str),
    {
        for tag in self.iter() {
            visitor(tag);
        }
    }
}

impl<'a> Tagged for &'a TagSet {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&str),
    {
        for tag in self.into_iter() {
            visitor(tag.as_str());
        }
    }
}

fn get_dirty_context_key_value() -> ContextKey {
    const EMPTY_TAGS: &[&str] = &[];
    *DIRTY_CONTEXT_KEY.get_or_init(|| hash_context("", EMPTY_TAGS, None))
}

fn is_context_dirty(key: ContextKey) -> bool {
    key == get_dirty_context_key_value()
}
