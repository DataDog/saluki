use std::{
    fmt, hash,
    sync::{Arc, OnceLock},
};

use metrics::Gauge;
use stringtheory::MetaString;

use crate::{hash::hash_context, tags::TagSet};

static DIRTY_CONTEXT_HASH: OnceLock<u64> = OnceLock::new();

/// A metric context.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Creates a new `Context` from the given static name.
    pub fn from_static_name(name: &'static str) -> Self {
        const EMPTY_TAGS: &[&str] = &[];

        let (hash, _) = hash_context(name, EMPTY_TAGS);
        Self {
            inner: Arc::new(ContextInner {
                name: MetaString::from_static(name),
                tags: TagSet::default(),
                hash,
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

        let (hash, _) = hash_context(name, tags);
        Self {
            inner: Arc::new(ContextInner {
                name: MetaString::from_static(name),
                tags: tag_set,
                hash,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Creates a new `Context` from the given name and given tags.
    pub fn from_parts<S: Into<MetaString>>(name: S, tags: TagSet) -> Self {
        let name = name.into();
        let (hash, _) = hash_context(&name, &tags);
        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                hash,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Clones this context, and uses the given name for the cloned context.
    pub fn with_name<S: Into<MetaString>>(&self, name: S) -> Self {
        let name = name.into();
        let tags = self.inner.tags.clone();
        let (hash, _) = hash_context(&name, &tags);

        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                hash,
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
        inner.hash = get_dirty_context_hash_value();
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
    pub name: MetaString,
    pub tags: TagSet,
    pub hash: u64,
    pub active_count: Gauge,
}

impl Clone for ContextInner {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            tags: self.tags.clone(),
            hash: self.hash,

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
        // NOTE: See the documentation for `ContextRef` on why/how we only check equality using the hash of the context.
        self.hash == other.hash
    }
}

impl Eq for ContextInner {}

impl hash::Hash for ContextInner {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        // If the context is dirty -- has changed since it was originally resolved -- then our cached hash is now
        // invalid, so we need to re-hash the context. Otherwise, we can just use the cached hash.
        if is_context_dirty(self.hash) {
            let (hash, _) = hash_context(&self.name, &self.tags);
            state.write_u64(hash);
        } else {
            state.write_u64(self.hash);
        }
    }
}

impl fmt::Debug for ContextInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextInner")
            .field("name", &self.name)
            .field("tags", &self.tags)
            .field("hash", &self.hash)
            .finish()
    }
}

/// A context reference that requires zero allocations.
///
/// It can be constructed entirely from borrowed strings, which allows for trivially extracting the name and tags of a
/// metric from a byte slice and then resolving the context without needing to allocate any new memory when a context
/// has already been resolved.
///
/// ## Hashing and equality
///
/// `ContextRef` (and `Context` itself) are order-oblivious [1] when it comes to tags, which means that we do not
/// consider the order of the tags to be relevant to the resulting hash or when comparing two contexts for equality.
/// This is achieved by hashing the tags in an order-oblivious way (XORing the hashes of the tags into a single value)
/// and using the hash of the name/tags when comparing equality between two contexts, instead of comparing the
/// names/tags directly to each other.
///
/// Normally, hash maps would instruct you to not use the hash values directly as a proxy for equality because of the
/// risk of hash collisions, and they're right: this approach _does_ theoretically allow for incorrectly considering two
/// contexts as equal purely due to a hash collision.
///
/// However, the risk of this is low enough that we're willing to accept it. Theoretically, for a perfectly uniform hash
/// function with a 64-bit output, we would expect a 50% chance of observing a hash collision after hashing roughly **5
/// billion unique inputs**. At a more practical number, like 1 million unique inputs, the chance of a collision drops
/// down to around a 1 in 50 million chance, which we find acceptable to contend with.
///
/// [1]: https://stackoverflow.com/questions/5889238/why-is-xor-the-default-way-to-combine-hashes
#[derive(Debug)]
pub struct ContextRef<'a, I> {
    pub name: &'a str,
    pub tags: I,
    pub tag_len: usize,
    pub hash: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextHashKey(pub u64);

impl hash::Hash for ContextHashKey {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0);
    }
}

fn get_dirty_context_hash_value() -> u64 {
    const EMPTY_TAGS: &[&str] = &[];
    *DIRTY_CONTEXT_HASH.get_or_init(|| {
        let (hash, _) = hash_context("", EMPTY_TAGS);
        hash
    })
}

fn is_context_dirty(hash: u64) -> bool {
    hash == get_dirty_context_hash_value()
}
