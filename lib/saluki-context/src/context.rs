use std::{fmt, hash, sync::Arc};

use metrics::Gauge;
use stringtheory::MetaString;

use crate::{
    hash::{hash_context, ContextKey},
    tags::{Tag, TagSet, Tagged},
};

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

    /// Gets the name of this context.
    pub fn name(&self) -> &MetaString {
        &self.inner.name
    }

    /// Gets the tags of this context.
    pub fn tags(&self) -> &TagSet {
        &self.inner.tags
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
