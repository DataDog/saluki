use std::{
    fmt,
    hash::{self, Hash as _, Hasher as _},
    num::NonZeroUsize,
    ops::Deref as _,
    sync::{Arc, RwLock},
};

use indexmap::{Equivalent, IndexSet};
use metrics::Gauge;
use saluki_metrics::static_metrics;
use stringtheory::{interning::FixedSizeInterner, MetaString};

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

#[derive(Debug)]
struct State {
    resolved_contexts: IndexSet<Arc<ContextInner>>,
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
#[derive(Clone, Debug)]
pub struct ContextResolver {
    context_metrics: ContextMetrics,
    interner: FixedSizeInterner,
    state: Arc<RwLock<State>>,
}

impl ContextResolver {
    /// Creates a new `ContextResolver` with the given interner.
    pub fn from_interner<S>(name: S, interner: FixedSizeInterner) -> Self
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
                resolved_contexts: IndexSet::new(),
            })),
        }
    }

    /// Creates a new `ContextResolver` with a no-op interner.
    pub fn with_noop_interner() -> Self {
        // It's not _really_ a no-op, but it's as small as we can possibly make it which  will effectively make it a
        // no-op after only a single string has been interned.
        Self::from_interner(
            "noop".to_string(),
            FixedSizeInterner::new(NonZeroUsize::new(1).unwrap()),
        )
    }

    fn intern_with_fallback(&self, s: &str) -> MetaString {
        match self.interner.try_intern(s) {
            Some(interned) => MetaString::from(interned),
            None => {
                self.context_metrics.intern_fallback_total().increment(1);
                MetaString::from(s)
            }
        }
    }

    /// Resolves the given context.
    ///
    /// If the context has not yet been resolved, the name and tags are interned and a new context is created and
    /// stored. Otherwise, the existing context is returned.
    pub fn resolve<T>(&self, context_ref: ContextRef<'_, T>) -> Context
    where
        T: AsRef<str> + hash::Hash + std::fmt::Debug,
    {
        let state = self.state.read().unwrap();
        match state.resolved_contexts.get(&context_ref) {
            Some(context) => {
                self.context_metrics.resolved_existing_context_total().increment(1);
                Context {
                    inner: Arc::clone(context),
                }
            }
            None => {
                // Switch from read to write lock.
                drop(state);
                let mut state = self.state.write().unwrap();

                // Interning is fallible so what we do here is just allocate them -- yes, hold on, keep reading -- and do
                // it via `MetaString::shared`, which at least lets us potentially share those allocations the next time
                // the same context is resolved.
                //
                // Not great, but also not maximally wasteful.
                let name = self.intern_with_fallback(context_ref.name);
                let tags = context_ref
                    .tags
                    .iter()
                    .map(|tag| self.intern_with_fallback(tag.as_ref()).into())
                    .collect();

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

                let active_count = self.context_metrics.active_contexts().clone();
                let context = Arc::new(ContextInner::from_name_and_tags(name, tags, active_count));
                state.resolved_contexts.insert(Arc::clone(&context));

                self.context_metrics.resolved_new_context_total().increment(1);
                self.context_metrics.active_contexts().increment(1);

                Context { inner: context }
            }
        }
    }
}

/// A metric context.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Gets the name of this context.
    pub fn name(&self) -> &MetaString {
        &self.inner.name
    }

    /// Gets the tags of this context.
    pub fn tags(&self) -> &TagSet {
        &self.inner.tags
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

impl ContextInner {
    fn from_name_and_tags(name: MetaString, tags: TagSet, active_count: Gauge) -> Self {
        let hash = hash_context(name.deref(), tags.0.as_slice());
        Self {
            name,
            tags,
            hash,
            active_count,
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
        state.write_u64(self.hash);
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

/// A helper type for resolving a context without allocations.
///
/// It can be constructed entirely from borrowed strings, which allows for trivially extracting the name and tags of a
/// metric from a byte slice and then resolving the context without needing to allocate any new memory when a context
/// has already been resolved.
///
/// ## Hashing and equality
///
/// `ContextRef` (and `Context` itself) are order-oblivious [1] when it comes to tags, which means that we do not consider
/// the order of the tags to be relevant to the resulting hash or when comparing two contexts for equality. This is
/// acheived by hashing the tags in an order-oblivious way (XORing the hashes of the tags into a single value) and using
/// the hash of the name/tags when comparing equality between two contexts, instead of comparing the names/tags directly
/// to each other.
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
pub struct ContextRef<'a, T> {
    name: &'a str,
    tags: &'a [T],
    hash: u64,
}

impl<'a, T> ContextRef<'a, T>
where
    T: hash::Hash,
{
    /// Creates a new `ContextRef` from the given name and tags.
    pub fn from_name_and_tags(name: &'a str, tags: &'a [T]) -> Self {
        let hash = hash_context(name, tags);
        Self { name, tags, hash }
    }
}

impl<'a, T> hash::Hash for ContextRef<'a, T>
where
    T: hash::Hash,
{
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl<T> Equivalent<Arc<ContextInner>> for ContextRef<'_, T>
where
    T: hash::Hash + std::fmt::Debug,
{
    fn equivalent(&self, other: &Arc<ContextInner>) -> bool {
        self.hash == other.hash
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Tag(MetaString);

impl Tag {
    pub const fn empty() -> Self {
        Self(MetaString::empty())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn name(&self) -> &str {
        let s = self.0.deref();
        match s.split_once(':') {
            Some((name, _)) => name,
            None => s,
        }
    }

    pub fn value(&self) -> Option<&str> {
        self.0.deref().split_once(':').map(|(_, value)| value)
    }

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

#[derive(Clone, Debug, Default)]
pub struct TagSet(Vec<Tag>);

impl TagSet {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn insert_tag<T>(&mut self, tag: T)
    where
        T: Into<Tag>,
    {
        let tag = tag.into();
        if !self.0.iter().any(|existing| existing == &tag) {
            self.0.push(tag);
        }
    }

    pub fn remove_tags<T>(&mut self, tag_name: T) -> Option<Vec<Tag>>
    where
        T: AsRef<str>,
    {
        // TODO: This is a super naive approach, and clobbers insertion order due to `swap_remove`. This wouldn't work,
        // naturally, if we need to depend on keeping a sorted order.
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

    pub fn merge_missing(&mut self, other: Self) {
        for tag in other.0 {
            if !self.0.iter().any(|existing| existing == &tag) {
                self.0.push(tag);
            }
        }
    }

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

fn hash_context<'a, T>(name: &'a str, tags: &'a [T]) -> u64
where
    T: hash::Hash,
{
    let mut hasher = ahash::AHasher::default();
    name.hash(&mut hasher);

    // Hash the tags individually and XOR their hashes together, which allows us to be order-oblivious.
    let mut combined_tags_hash = 0;
    for tag in tags {
        let mut tag_hasher = ahash::AHasher::default();
        tag.hash(&mut tag_hasher);

        combined_tags_hash ^= tag_hasher.finish();
    }

    hasher.write_u64(combined_tags_hash);

    hasher.finish()
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

    #[test]
    fn basic() {
        let resolver = ContextResolver::with_noop_interner();

        // Create two distinct contexts with the same name but different tags.
        let name = "metric_name";
        let tags1: [&str; 0] = [];
        let tags2 = ["tag1"];

        let ref1 = ContextRef::from_name_and_tags(name, &tags1);
        let ref2 = ContextRef::from_name_and_tags(name, &tags2);

        let context1 = resolver.resolve(ref1);
        let context2 = resolver.resolve(ref2);

        // The contexts should not be equal to each other, and should have distinct underlying pointers to the shared
        // context state:
        assert_ne!(context1, context2);
        assert_ne!(
            get_context_arc_pointer_value(&context1),
            get_context_arc_pointer_value(&context2)
        );

        // If we create the context references again, we _should_ get back the same contexts as before:
        let ref1 = ContextRef::from_name_and_tags(name, &tags1);
        let ref2 = ContextRef::from_name_and_tags(name, &tags2);

        let context1_redo = resolver.resolve(ref1);
        let context2_redo = resolver.resolve(ref2);

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
        let resolver = ContextResolver::with_noop_interner();

        // Create two distinct contexts with the same name and tags, but with the tags in a different order:
        let name = "metric_name";
        let tags1 = ["tag1", "tag2"];
        let tags2 = ["tag2", "tag1"];

        let ref1 = ContextRef::from_name_and_tags(name, &tags1);
        let ref2 = ContextRef::from_name_and_tags(name, &tags2);

        let context1 = resolver.resolve(ref1);
        let context2 = resolver.resolve(ref2);

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
            let resolver = ContextResolver::with_noop_interner();
            resolver.resolve(ContextRef::from_name_and_tags("name", &["tag1"]))
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
}
