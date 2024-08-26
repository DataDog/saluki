//! Metric context and context resolving.
#![deny(warnings)]
#![deny(missing_docs)]

use std::{
    collections::HashSet,
    fmt,
    hash::{self, BuildHasher, Hash as _, Hasher as _},
    num::NonZeroUsize,
    ops::Deref as _,
    sync::{Arc, OnceLock, RwLock},
};

use indexmap::{Equivalent, IndexSet};
use metrics::Gauge;
use saluki_metrics::static_metrics;
use stringtheory::{interning::FixedSizeInterner, MetaString};

static DIRTY_CONTEXT_HASH: OnceLock<u64> = OnceLock::new();

static_metrics! {
    name => ContextMetrics,
    prefix => context_resolver,
    labels => [resolver_id: String],
    metrics => [
        counter(resolved_existing_context_total),
        counter(resolved_new_context_total),
        gauge(active_contexts),
        gauge(interner_capacity_bytes),
        gauge(interner_len_bytes),
        gauge(interner_entries),
        counter(intern_fallback_total)
    ],
}

type PrehashedHashSet = HashSet<u64, NoopU64Hasher>;

#[derive(Debug)]
struct State {
    resolved_contexts: IndexSet<Context, ahash::RandomState>,
}

/// A centralized store for resolved contexts.
///
/// Contexts are the combination of a name and a set of tags. They are used to identify a specific metric series. As contexts
/// are constructed entirely of strings, they are expensive to construct in a way that allows sending between tasks, as
/// this usually requires allocations. Even further, the same context may be "hot", used frequently by the
/// applications/services sending us metrics.
///
/// In order to optimize this, the context resolver is responsible for both interning the strings involved where
/// possible, as well as keeping a map of contexts that can be referred to with a cheap handle. We can cheaply search
/// for an existing context without needing to allocate an entirely new one, and get a clone of the handle to use going
/// forward.
///
/// ## Design
///
/// `ContextResolver` specifically manages interning and mapping of contexts. It can be cheaply cloned itself.
///
/// In order to resolve a context, `resolve` must be called which requires taking a lock to check for an existing
/// context. A read/write lock is used in order to prioritize lookups over inserts, as lookups are expected to be more
/// common given how often a given context is used and resolved.
///
/// Once a context is resolved, a cheap handle -- `Context` -- is returned. This handle, like `ContextResolver`, can be
/// cheaply cloned. It points directly to the underlying context data (name and tags) and provides access to these
/// components.
#[derive(Debug)]
pub struct ContextResolver<const SHARD_FACTOR: usize = 8> {
    context_metrics: ContextMetrics,
    interner: FixedSizeInterner<SHARD_FACTOR>,
    state: Arc<RwLock<State>>,
    hash_seen_buffer: PrehashedHashSet,
    allow_heap_allocations: bool,
}

impl<const SHARD_FACTOR: usize> ContextResolver<SHARD_FACTOR> {
    /// Creates a new `ContextResolver` with the given interner.
    pub fn from_interner<S>(name: S, interner: FixedSizeInterner<SHARD_FACTOR>) -> Self
    where
        S: Into<String>,
    {
        let context_metrics = ContextMetrics::new(name.into());

        context_metrics
            .interner_capacity_bytes()
            .set(interner.capacity_bytes() as f64);

        Self {
            context_metrics,
            interner,
            state: Arc::new(RwLock::new(State {
                resolved_contexts: IndexSet::with_hasher(ahash::RandomState::new()),
            })),
            hash_seen_buffer: PrehashedHashSet::with_hasher(NoopU64Hasher::new()),
            allow_heap_allocations: true,
        }
    }

    /// Creates a new `ContextResolver` with a no-op interner.
    pub fn with_noop_interner() -> Self {
        // It's not _really_ a no-op, but it's as small as we can possibly make it which will effectively make it a
        // no-op after only a single string has been interned.
        Self::from_interner(
            "noop".to_string(),
            FixedSizeInterner::<SHARD_FACTOR>::new(NonZeroUsize::new(1).unwrap()),
        )
    }

    /// Sets whether or not to allow heap allocations when interning strings.
    ///
    /// In cases where the interner is full, this setting determines whether or not we refuse to resolve a context, or
    /// if we instead allocate strings normally (which will not be interned and will not be shared with other contexts)
    /// to satisfy the request.
    ///
    /// Defaults to `true`.
    pub fn with_heap_allocations(mut self, allow: bool) -> Self {
        self.allow_heap_allocations = allow;
        self
    }

    fn intern(&self, s: &str) -> Option<MetaString> {
        // First we'll see if we can inline the string, and if we can't, then we try to actually intern it. If interning
        // fails, then we just fall back to allocating a new `MetaString` instance.
        MetaString::try_inline(s)
            .or_else(|| self.interner.try_intern(s).map(MetaString::from))
            .or_else(|| self.allow_heap_allocations.then(|| MetaString::from(s)))
    }

    fn create_context_from_ref<I, T>(&self, context_ref: ContextRef<'_, I>, active_count: Gauge) -> Option<Context>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str> + hash::Hash + std::fmt::Debug,
    {
        let name = self.intern(context_ref.name)?;
        let mut tags = TagSet::with_capacity(context_ref.tag_len);
        for tag in context_ref.tags {
            let tag = self.intern(tag.as_ref())?;
            tags.insert_tag(tag);
        }

        Some(Context {
            inner: Arc::new(ContextInner {
                name,
                tags,
                hash: context_ref.hash,
                active_count,
            }),
        })
    }

    /// Creates a `ContextRef<'a, I>` from the given name and tags.
    pub fn create_context_ref<'a, I, T>(&mut self, name: &'a str, tags: I) -> ContextRef<'a, I>
    where
        I: IntoIterator<Item = T> + Clone,
        T: AsRef<str> + hash::Hash,
    {
        let (context_hash, tag_len) = hash_context_with_seen(name, tags.clone(), &mut self.hash_seen_buffer);

        ContextRef {
            name,
            tags,
            tag_len,
            hash: context_hash,
        }
    }

    /// Resolves the given context.
    ///
    /// If the context has not yet been resolved, the name and tags are interned and a new context is created and
    /// stored. Otherwise, the existing context is returned.
    ///
    /// `None` may be returned if the interner is full and outside allocations are disallowed. See
    /// `allow_heap_allocations` for more information.
    pub fn resolve<I, T>(&self, context_ref: ContextRef<'_, I>) -> Option<Context>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str> + hash::Hash + std::fmt::Debug,
    {
        let state = self.state.read().unwrap();
        match state.resolved_contexts.get(&context_ref) {
            Some(context) => {
                self.context_metrics.resolved_existing_context_total().increment(1);
                Some(context.clone())
            }
            None => {
                // Switch from read to write lock.
                drop(state);
                let mut state = self.state.write().unwrap();

                // Create our new context and store it.
                let active_count = self.context_metrics.active_contexts().clone();
                let context = self.create_context_from_ref(context_ref, active_count)?;
                state.resolved_contexts.insert(context.clone());

                // TODO: This is lazily updated during resolve, which means this metric might lag behind the actual
                // count as interned strings are dropped/reclaimed... but we don't have a way to figure out if a given
                // `MetaString` is an interned string and if dropping it would actually reclaim the interned string...
                // so this is our next best option short of instrumenting `FixedSizeInterner` directly.
                //
                // We probably want to do that in the future, but this is just a little cleaner without adding extra
                // fluff to `FixedSizeInterner` which is already complex as-is.
                self.context_metrics.interner_entries().set(self.interner.len() as f64);
                self.context_metrics
                    .interner_len_bytes()
                    .set(self.interner.len_bytes() as f64);
                self.context_metrics.resolved_new_context_total().increment(1);
                self.context_metrics.active_contexts().increment(1);

                Some(context)
            }
        }
    }
}

impl<const SHARD_FACTOR: usize> Clone for ContextResolver<SHARD_FACTOR> {
    fn clone(&self) -> Self {
        Self {
            context_metrics: self.context_metrics.clone(),
            interner: self.interner.clone(),
            state: self.state.clone(),
            hash_seen_buffer: PrehashedHashSet::with_hasher(NoopU64Hasher::new()),
            allow_heap_allocations: self.allow_heap_allocations,
        }
    }
}

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
    pub fn from_static_parts(name: &'static str, tags: &'static [&'static str]) -> Self {
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

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner.name)?;
        if !self.inner.tags.0.is_empty() {
            write!(f, "{{")?;

            let mut needs_separator = false;
            for tag in &self.inner.tags.0 {
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

struct ContextInner {
    name: MetaString,
    tags: TagSet,
    hash: u64,
    active_count: Gauge,
}

impl Clone for ContextInner {
    fn clone(&self) -> Self {
        // Increment the context count when cloning the context, since we only get here when we're about to create a
        // brand new context for the purpose of mutating the data... so we have a new context.
        self.active_count.increment(1);

        Self {
            name: self.name.clone(),
            tags: self.tags.clone(),
            hash: self.hash,
            active_count: self.active_count.clone(),
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
    name: &'a str,
    tags: I,
    tag_len: usize,
    hash: u64,
}

impl<'a, I, T> hash::Hash for ContextRef<'a, I>
where
    I: IntoIterator<Item = T>,
    T: hash::Hash,
{
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl<I, T> Equivalent<Context> for ContextRef<'_, I>
where
    I: IntoIterator<Item = T>,
    T: hash::Hash + std::fmt::Debug,
{
    fn equivalent(&self, other: &Context) -> bool {
        self.hash == other.inner.hash
    }
}

/// A metric tag.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Tag(MetaString);

impl Tag {
    /// Creates a new, empty tag.
    pub const fn empty() -> Self {
        Self(MetaString::empty())
    }

    /// Returns `true` if the tag is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the length of the tag, in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Gets the name of the tag.
    ///
    /// For bare tags (e.g. `production`), this is simply the tag value itself. For key/value-style tags (e.g.
    /// `service:web`), this is the key part of the tag, or `service` based on the example.
    pub fn name(&self) -> &str {
        let s = self.0.deref();
        match s.split_once(':') {
            Some((name, _)) => name,
            None => s,
        }
    }

    /// Gets the value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `None`. For key/value-style tags (e.g. `service:web`),
    /// this is the value part of the tag, or `web` based on the example.
    pub fn value(&self) -> Option<&str> {
        self.0.deref().split_once(':').map(|(_, value)| value)
    }

    /// Consumes the tag and returns the inner `MetaString`.
    pub fn into_inner(self) -> MetaString {
        self.0
    }
}

impl PartialEq<str> for Tag {
    fn eq(&self, other: &str) -> bool {
        self.0.deref() == other
    }
}

impl hash::Hash for Tag {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<T> From<T> for Tag
where
    T: Into<MetaString>,
{
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// A borrowed metric tag.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BorrowedTag<'a>(&'a str);

impl<'a> BorrowedTag<'a> {
    /// Returns `true` if the tag is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the length of the tag, in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Gets the name of the tag.
    ///
    /// For bare tags (e.g. `production`), this is simply the tag value itself. For key/value-style tags (e.g.
    /// `service:web`), this is the key part of the tag, or `service` based on the example.
    pub fn name(&self) -> &str {
        match self.0.split_once(':') {
            Some((name, _)) => name,
            None => self.0,
        }
    }

    /// Gets the value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `None`. For key/value-style tags (e.g. `service:web`),
    /// this is the value part of the tag, or `web` based on the example.
    pub fn value(&self) -> Option<&str> {
        self.0.split_once(':').map(|(_, value)| value)
    }

    /// Gets the name and value of the tag.
    ///
    /// For bare tags (e.g. `production`), this always returns `(Some(...), None)`.
    pub fn name_and_value(&self) -> (Option<&str>, Option<&str>) {
        match self.0.split_once(':') {
            Some((name, value)) => (Some(name), Some(value)),
            None => (Some(self.0), None),
        }
    }
}

impl<'a> From<&'a str> for BorrowedTag<'a> {
    fn from(s: &'a str) -> Self {
        Self(s)
    }
}

/// A set of tags.
#[derive(Clone, Debug, Default)]
pub struct TagSet(Vec<Tag>);

impl TagSet {
    /// Creates a new, empty tag set with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    /// Returns `true` if the tag set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of tags in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Inserts a tag into the set.
    ///
    /// If the tag is already present in the set, this does nothing.
    pub fn insert_tag<T>(&mut self, tag: T)
    where
        T: Into<Tag>,
    {
        let tag = tag.into();
        if !self.0.iter().any(|existing| existing == &tag) {
            self.0.push(tag);
        }
    }

    /// Removes a tag, by name, from the set.
    pub fn remove_tags<T>(&mut self, tag_name: T) -> Option<Vec<Tag>>
    where
        T: AsRef<str>,
    {
        let tag_name = tag_name.as_ref();

        let mut tags = Vec::new();
        let mut idx = 0;
        while idx < self.0.len() {
            if tag_has_name(&self.0[idx], tag_name) {
                tags.push(self.0.swap_remove(idx));
            } else {
                idx += 1;
            }
        }

        if tags.is_empty() {
            None
        } else {
            Some(tags)
        }
    }

    /// Retains only the tags specified by the predicate.
    ///
    /// In other words, remove all tags `t` for which `f(&t)` returns `false`. This method operates in place, visiting
    /// each element exactly once in the original order, and preserves the order of the retained tags.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&Tag) -> bool,
    {
        self.0.retain(|tag| f(tag));
    }

    /// Merges the tags from another set into this set.
    ///
    /// If a tag from `other` is already present in this set, it will not be added.
    pub fn merge_missing(&mut self, other: Self) {
        for tag in other.0 {
            self.insert_tag(tag);
        }
    }

    /// Returns a sorted version of the tag set.
    pub fn as_sorted(&self) -> Self {
        let mut tags = self.0.clone();
        tags.sort_unstable();
        Self(tags)
    }
}

fn tag_has_name(tag: &Tag, tag_name: &str) -> bool {
    // Try matching it as a bare tag (i.e. `production`).
    if tag.0.deref() == tag_name {
        return true;
    }

    // Try matching it as a key-value pair (i.e. `env:production`).
    tag.0
        .deref()
        .split_once(':')
        .map_or(false, |(name, _)| name == tag_name)
}

impl PartialEq<TagSet> for TagSet {
    fn eq(&self, other: &TagSet) -> bool {
        // NOTE: We could try storing tags in sorted order internally, which would make this moot... but for now, we'll
        // avoid the sort (which lets us avoid an allocation) and just do the naive per-item comparison.
        if self.0.len() != other.0.len() {
            return false;
        }

        for other_tag in &other.0 {
            if !self.0.iter().any(|tag| tag == other_tag) {
                return false;
            }
        }

        true
    }
}

impl IntoIterator for TagSet {
    type Item = Tag;
    type IntoIter = std::vec::IntoIter<Tag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a TagSet {
    type Item = &'a Tag;
    type IntoIter = std::slice::Iter<'a, Tag>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl FromIterator<Tag> for TagSet {
    fn from_iter<I: IntoIterator<Item = Tag>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<Tag> for TagSet {
    fn extend<T: IntoIterator<Item = Tag>>(&mut self, iter: T) {
        self.0.extend(iter)
    }
}

impl From<Tag> for TagSet {
    fn from(tag: Tag) -> Self {
        Self(vec![tag])
    }
}

fn hash_context<I, T>(name: &str, tags: I) -> (u64, usize)
where
    I: IntoIterator<Item = T>,
    T: hash::Hash,
{
    let mut seen = PrehashedHashSet::with_hasher(NoopU64Hasher::new());
    hash_context_with_seen(name, tags, &mut seen)
}

fn hash_context_with_seen<I, T>(name: &str, tags: I, seen: &mut PrehashedHashSet) -> (u64, usize)
where
    I: IntoIterator<Item = T>,
    T: hash::Hash,
{
    seen.clear();

    let mut hasher = ahash::AHasher::default();
    name.hash(&mut hasher);

    // Hash the tags individually and XOR their hashes together, which allows us to be order-oblivious:
    let mut combined_tags_hash = 0;
    let mut tag_count = 0;
    for tag in tags {
        let mut tag_hasher = ahash::AHasher::default();
        tag.hash(&mut tag_hasher);
        let tag_hash = tag_hasher.finish();

        // If we've already seen this tag before, skip combining it again.
        if !seen.insert(tag_hash) {
            continue;
        }

        combined_tags_hash ^= tag_hash;
        tag_count += 1;
    }

    hasher.write_u64(combined_tags_hash);

    (hasher.finish(), tag_count)
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

struct NoopU64Hasher(u64);

impl NoopU64Hasher {
    fn new() -> Self {
        Self(0)
    }
}

impl hash::Hasher for NoopU64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn write(&mut self, _: &[u8]) {
        panic!("NoopU64Hasher is only valid for hashing `u64` values");
    }
}

impl BuildHasher for NoopU64Hasher {
    type Hasher = NoopU64Hasher;

    fn build_hasher(&self) -> Self::Hasher {
        Self(0)
    }
}

#[cfg(test)]
mod tests {
    use metrics::{SharedString, Unit};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey,
    };

    use super::*;

    fn get_context_arc_pointer_value(context: &Context) -> usize {
        Arc::as_ptr(&context.inner) as usize
    }

    fn get_gauge_value(metrics: &[(CompositeKey, Option<Unit>, Option<SharedString>, DebugValue)], key: &str) -> f64 {
        metrics
            .iter()
            .find(|(k, _, _, _)| k.key().name() == key)
            .map(|(_, _, _, value)| match value {
                DebugValue::Gauge(value) => value.into_inner(),
                other => panic!("expected a gauge, got: {:?}", other),
            })
            .unwrap_or_else(|| panic!("no metric found with key: {}", key))
    }

    fn refs_approx_eq<I1, I2, T>(ref1: &ContextRef<'_, I1>, ref2: &ContextRef<'_, I2>) -> bool
    where
        I1: IntoIterator<Item = T>,
        I2: IntoIterator<Item = T>,
        T: hash::Hash,
    {
        ref1.name == ref2.name && ref1.hash == ref2.hash && ref1.tag_len == ref2.tag_len
    }

    #[test]
    fn basic() {
        let mut resolver: ContextResolver = ContextResolver::with_noop_interner();

        // Create two distinct contexts with the same name but different tags:
        let name = "metric_name";
        let tags1: [&str; 0] = [];
        let tags2 = ["tag1"];

        let ref1 = resolver.create_context_ref(name, &tags1);
        let ref2 = resolver.create_context_ref(name, &tags2);
        assert!(!refs_approx_eq(&ref1, &ref2));

        let context1 = resolver.resolve(ref1).expect("should not fail to resolve");
        let context2 = resolver.resolve(ref2).expect("should not fail to resolve");

        // The contexts should not be equal to each other, and should have distinct underlying pointers to the shared
        // context state:
        assert_ne!(context1, context2);
        assert_ne!(
            get_context_arc_pointer_value(&context1),
            get_context_arc_pointer_value(&context2)
        );

        // If we create the context references again, we _should_ get back the same contexts as before:
        let ref1 = resolver.create_context_ref(name, &tags1);
        let ref2 = resolver.create_context_ref(name, &tags2);
        assert!(!refs_approx_eq(&ref1, &ref2));

        let context1_redo = resolver.resolve(ref1).expect("should not fail to resolve");
        let context2_redo = resolver.resolve(ref2).expect("should not fail to resolve");

        assert_ne!(context1_redo, context2_redo);
        assert_eq!(context1, context1_redo);
        assert_eq!(context2, context2_redo);
        assert_eq!(
            get_context_arc_pointer_value(&context1),
            get_context_arc_pointer_value(&context1_redo)
        );
        assert_eq!(
            get_context_arc_pointer_value(&context2),
            get_context_arc_pointer_value(&context2_redo)
        );
    }

    #[test]
    fn tag_order() {
        let mut resolver: ContextResolver = ContextResolver::with_noop_interner();

        // Create two distinct contexts with the same name and tags, but with the tags in a different order:
        let name = "metric_name";
        let tags1 = ["tag1", "tag2"];
        let tags2 = ["tag2", "tag1"];

        let ref1 = resolver.create_context_ref(name, &tags1);
        let ref2 = resolver.create_context_ref(name, &tags2);
        assert!(refs_approx_eq(&ref1, &ref2));

        let context1 = resolver.resolve(ref1).expect("should not fail to resolve");
        let context2 = resolver.resolve(ref2).expect("should not fail to resolve");

        // The contexts should be equal to each other, and should have the same underlying pointer to the shared context
        // state:
        assert_eq!(context1, context2);
        assert_eq!(
            get_context_arc_pointer_value(&context1),
            get_context_arc_pointer_value(&context2)
        );
    }

    #[test]
    fn active_contexts() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        // Create our resolver and then create a context, which will have its metrics attached to our local recorder:
        let context = metrics::with_local_recorder(&recorder, || {
            let mut resolver: ContextResolver = ContextResolver::with_noop_interner();
            let context_ref = resolver.create_context_ref("name", &["tag1"]);
            resolver.resolve(context_ref).expect("should not fail to resolve")
        });

        // We should be able to see that the active context count is one, representing the context we created:
        let metrics_before = snapshotter.snapshot().into_vec();
        let active_contexts = get_gauge_value(&metrics_before, ContextMetrics::active_contexts_name());
        assert_eq!(active_contexts, 1.0);

        // Now drop the context, and observe the active context count drop to zero:
        drop(context);
        let metrics_after = snapshotter.snapshot().into_vec();
        let active_contexts = get_gauge_value(&metrics_after, ContextMetrics::active_contexts_name());
        assert_eq!(active_contexts, 0.0);
    }

    #[test]
    fn mutate_tags() {
        let mut resolver: ContextResolver = ContextResolver::with_noop_interner();

        // Create a basic context.
        //
        // We create two identical references so that we can later try and resolve the original context again to make
        // sure things are still working as expected:
        let name = "metric_name";
        let tags = ["tag1"];

        let ref1 = resolver.create_context_ref(name, &tags);
        let ref2 = resolver.create_context_ref(name, &tags);
        assert!(refs_approx_eq(&ref1, &ref2));

        let context1 = resolver.resolve(ref1).expect("should not fail to resolve");
        let mut context2 = context1.clone();

        // Mutate the tags of `context2`, which should end up cloning the inner state and becoming its own instance:
        let tags = context2.tags_mut();
        tags.insert_tag("tag2");

        // The contexts should no longer be equal to each other, and should have distinct underlying pointers to the
        // shared context state:
        assert_ne!(context1, context2);
        assert_ne!(
            get_context_arc_pointer_value(&context1),
            get_context_arc_pointer_value(&context2)
        );

        let expected_tags_context1 = TagSet::from_iter(vec![Tag::from("tag1")]);
        assert_eq!(context1.tags(), &expected_tags_context1);

        let expected_tags_context2 = TagSet::from_iter(vec![Tag::from("tag1"), Tag::from("tag2")]);
        assert_eq!(context2.tags(), &expected_tags_context2);

        // And just for good measure, check that we can still resolve the original context reference and get back a
        // context that is equal to `context1`:
        let context1_redo = resolver.resolve(ref2).expect("should not fail to resolve");
        assert_eq!(context1, context1_redo);
        assert_eq!(
            get_context_arc_pointer_value(&context1),
            get_context_arc_pointer_value(&context1_redo)
        );
    }

    #[test]
    fn duplicate_tags() {
        let mut resolver: ContextResolver = ContextResolver::with_noop_interner();

        // Two contexts with the same name, but each with a different set of duplicate tags:
        let name = "metric_name";
        let tags1 = ["tag1"];
        let tags1_duplicated = ["tag1", "tag1"];
        let tags2 = ["tag2"];
        let tags2_duplicated = ["tag2", "tag2"];

        let ref1 = resolver.create_context_ref(name, &tags1);
        let ref1_duplicated = resolver.create_context_ref(name, &tags1_duplicated);
        let ref2 = resolver.create_context_ref(name, &tags2);
        let ref2_duplicated = resolver.create_context_ref(name, &tags2_duplicated);

        let context1 = resolver.resolve(ref1).expect("should not fail to resolve");
        let context1_duplicated = resolver.resolve(ref1_duplicated).expect("should not fail to resolve");
        let context2 = resolver.resolve(ref2).expect("should not fail to resolve");
        let context2_duplicated = resolver.resolve(ref2_duplicated).expect("should not fail to resolve");

        // Each non-duplicated/duplicated context pair should be equal to one another:
        assert_eq!(context1, context1_duplicated);
        assert_eq!(context2, context2_duplicated);

        // Each pair should not be equal to the other pair, however.
        //
        // What we're asserting here is that, if we didn't handle duplicate tags correctly, the XOR hashing of [tag1,
        // tag1] and [tag2, tag2] would result in the same hash value, since the second duplicate hash of tag1/tag2
        // would cancel out the first... and thus all that would be left is the hash of the name itself, which is the
        // same in this test. This would lead to the contexts being equal, which is obviously wrong.
        //
        // If we're handling duplicates properly, then the resulting context hashes _shouldn't_ be equal.
        assert_ne!(context1, context2);
        assert_ne!(context1_duplicated, context2_duplicated);
        assert_ne!(context1, context2_duplicated);
        assert_ne!(context2, context1_duplicated);
    }
}
