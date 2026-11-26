//! Metric context and context resolving.

use std::{
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
};

use saluki_common::collections::{ContiguousBitSet, PrehashedHashSet};
use serde::{Serialize, Serializer};
use stringtheory::{CheapMetaString, MetaString};

mod context_cache;

mod hash;
use self::hash::{hash_context, hash_context_with_host_and_seen};
pub use self::hash::{hash_context_with_host, ContextKey};

mod packed;
use self::packed::{Packed, Parts, TagMetadata};

mod resolver;
pub use self::resolver::{ContextResolver, ContextResolverBuilder, TagsResolver, TagsResolverBuilder};
use crate::data_model::tags::{Tag, TagSet, TagStr};

/// A metric name borrowed from a context.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ContextName(str);

impl ContextName {
    fn from_str(name: &str) -> &Self {
        // SAFETY: ContextName has the same representation as str.
        unsafe { &*(name as *const str as *const Self) }
    }
}

impl AsRef<str> for ContextName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl CheapMetaString for ContextName {
    fn try_cheap_clone(&self) -> Option<MetaString> {
        MetaString::try_inline(&self.0)
    }
}

impl From<&ContextName> for MetaString {
    fn from(name: &ContextName) -> Self {
        name.as_ref().into()
    }
}

impl Deref for ContextName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ContextName {
    fn eq(&self, other: &str) -> bool {
        &self.0 == other
    }
}

impl PartialEq<MetaString> for ContextName {
    fn eq(&self, other: &MetaString) -> bool {
        &self.0 == other.as_ref()
    }
}

impl fmt::Display for ContextName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A metric context.
///
/// A context is the combination of a metric name, an optional host, instrumented tags, and origin tags. It
/// identifies one metric series.
///
/// # Design
///
/// A context is an 8-byte handle to one immutable, reference-counted allocation. The allocation holds the name,
/// host, and tags inline, or as IDs in a shared symbol table. Cloning a context only increments a reference count.
///
/// A context never changes after construction. Each method that changes a name, host, or tag set builds a new
/// allocation, and other holders of the old context see no change.
///
/// Equality and hashing use only the 64-bit [`ContextKey`], which hashes the name, host, and both tag sets. This is a
/// trade-off between speed and exactness. A comparison costs one integer comparison, and never compares strings. Two
/// different contexts with the same key compare as equal. For 1,000,000 contexts, the chance of any such collision is
/// about 1 in 37,000,000.
#[derive(Clone)]
pub struct Context {
    inner: Packed,
}

impl Context {
    /// Creates a new `Context` from the given static name.
    pub fn from_static_name(name: &'static str) -> Self {
        Self::from_static_parts(name, &[])
    }

    /// Creates a new `Context` from the given static name and given static tags.
    pub fn from_static_parts(name: &'static str, tags: &[&'static str]) -> Self {
        let origin = TagSet::default();
        let key = hash_context(name, tags, &origin).0;
        Self {
            inner: Packed::new(
                Parts {
                    key,
                    name,
                    host: None,
                    origin,
                    domain: None,
                    tracked: false,
                    lease: None,
                    metadata: TagMetadata {
                        deduplicate: true,
                        additions: tags.len(),
                        modified: !tags.is_empty(),
                    },
                },
                tags.iter().copied(),
            ),
        }
    }

    /// Creates a new `Context` from the given name and given tags.
    pub fn from_parts<S: Into<MetaString>>(name: S, tags: impl Into<TagSet>) -> Self {
        let name = name.into();
        let tags = tags.into();
        let origin = TagSet::default();
        let key = hash_context(&name, &tags, &origin).0;
        Self {
            inner: Packed::new(
                Parts {
                    key,
                    name: &name,
                    host: None,
                    origin,
                    domain: None,
                    tracked: false,
                    lease: None,
                    metadata: TagMetadata::from_tag_set(&tags),
                },
                (&tags).into_iter().map(Tag::as_str),
            ),
        }
    }

    /// Clones this context, and uses the given name for the cloned context.
    ///
    /// The host and both tag sets of this context are preserved. The clone is a new allocation with a new key.
    pub fn with_name<S: Into<MetaString>>(&self, name: S) -> Self {
        let name = name.into();
        self.derive(
            &name,
            self.host(),
            self.tags().into_iter().map(TagStr::as_str),
            self.origin_tags().clone(),
            self.inner.metadata(),
            false,
        )
    }

    /// Clones this context, and uses the given tags for the cloned context.
    ///
    /// The name, host and origin tags are preserved. The clone is a new allocation with a new key.
    pub fn with_tags(&self, tags: impl Into<TagSet>) -> Self {
        self.with_tags_and_origin_tags(tags, self.origin_tags().clone())
    }

    /// Clones this context, and uses the given origin tags for the cloned context.
    ///
    /// The name, host and instrumented tags are preserved. The clone is a new allocation with a new key.
    pub fn with_origin_tags(&self, origin_tags: impl Into<TagSet>) -> Self {
        self.derive(
            self.name(),
            self.host(),
            self.tags().into_iter().map(TagStr::as_str),
            origin_tags.into(),
            self.inner.metadata(),
            false,
        )
    }

    /// Clones this context, replacing both instrumented tags and origin tags in one allocation.
    ///
    /// Use this method instead of separate `with_tags` and `with_origin_tags` calls when you replace both tag sets.
    /// It builds one new allocation and computes one key, instead of two of each.
    pub fn with_tags_and_origin_tags(&self, tags: impl Into<TagSet>, origin_tags: impl Into<TagSet>) -> Self {
        let tags = tags.into();
        self.derive(
            self.name(),
            self.host(),
            (&tags).into_iter().map(Tag::as_str),
            origin_tags.into(),
            TagMetadata::from_tag_set(&tags),
            false,
        )
    }

    /// Returns whether `self` and `other` share the same backing allocation.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner.ptr_eq(&other.inner)
    }

    /// Returns the name of this context.
    pub fn name(&self) -> &ContextName {
        ContextName::from_str(self.inner.name())
    }

    /// Returns the host of this context, if one has been set.
    pub fn host(&self) -> Option<&str> {
        self.inner.host()
    }

    /// Clones this context, and uses the given host for the cloned context.
    pub fn with_host<S: Into<Option<MetaString>>>(&self, host: S) -> Self {
        let host = host.into();
        self.derive(
            self.name(),
            host.as_deref(),
            self.tags().into_iter().map(TagStr::as_str),
            self.origin_tags().clone(),
            self.inner.metadata(),
            false,
        )
    }

    /// Returns a borrowed view of the instrumented tags of this context.
    pub fn tags(&self) -> ContextTags<'_> {
        ContextTags(&self.inner)
    }

    /// Returns the origin tags of this context.
    pub fn origin_tags(&self) -> &TagSet {
        self.inner.origin()
    }

    /// Mutates the instrumented tags of this context via a closure.
    ///
    /// The closure gets an owned copy of the tags. After the closure returns, this method builds a new allocation and
    /// computes a new context key, even if the closure changed nothing. Other holders of this context keep the old
    /// allocation, so the mutation does not affect them.
    pub fn mutate_tags(&mut self, f: impl FnOnce(&mut TagSet)) {
        self.with_tag_sets_mut(|tags, _| f(tags));
    }

    /// Mutates the origin tags of this context via a closure.
    ///
    /// The closure gets a copy of the origin tags. After the closure returns, this method builds a new allocation and
    /// computes a new context key, even if the closure changed nothing. Other holders of this context keep the old
    /// allocation, so the mutation does not affect them.
    pub fn mutate_origin_tags(&mut self, f: impl FnOnce(&mut TagSet)) {
        let mut origin = self.origin_tags().clone();
        f(&mut origin);
        *self = self.derive(
            self.name(),
            self.host(),
            self.tags().into_iter().map(TagStr::as_str),
            origin,
            self.inner.metadata(),
            self.inner.track_rewrite(),
        );
    }

    /// Mutates both instrumented tags and origin tags via a single closure.
    ///
    /// The closure gets owned copies of both tag sets. After the closure returns, this method builds one new
    /// allocation and computes the context key once, even if the closure changed nothing. Other holders of this
    /// context keep the old allocation, so the mutation does not affect them.
    pub fn with_tag_sets_mut(&mut self, f: impl FnOnce(&mut TagSet, &mut TagSet)) {
        let mut tags = self.tags().to_mutable();
        let mut origin = self.origin_tags().clone();
        f(&mut tags, &mut origin);
        *self = self.derive(
            self.name(),
            self.host(),
            (&tags).into_iter().map(Tag::as_str),
            origin,
            TagMetadata::from_tag_set(&tags),
            self.inner.track_rewrite(),
        );
    }

    /// Builds a new context from the given parts, and computes its context key.
    ///
    /// The new context keeps the resolver domain of this context. A strict-mode lease carries over and reuses each
    /// interned string whose value did not change.
    ///
    /// If `tracked` is `false`, the new context does not count toward the active-context gauge of the resolver. A
    /// copy of a context is detached from the resolver statistics, because the resolver counts only the contexts
    /// that it creates. A caller that replaces its only reference passes `true`, so the replacement takes over the
    /// count of the old context.
    fn derive<'a>(
        &self, name: &str, host: Option<&str>, tags: impl Iterator<Item = &'a str> + Clone, origin: TagSet,
        metadata: TagMetadata, tracked: bool,
    ) -> Self {
        let key = hash_context_with_host(name, host, tags.clone(), &origin).0;
        let lease = self.inner.lease().map(|lease| lease.rewrite(name, host, tags.clone()));
        Self {
            inner: Packed::new(
                Parts {
                    key,
                    name,
                    host,
                    origin,
                    domain: self.inner.domain(),
                    tracked,
                    metadata,
                    lease: lease.as_ref(),
                },
                tags,
            ),
        }
    }

    /// Creates a lazy copy-on-write mutable view over the tag sets of this context.
    ///
    /// The view records mutations, such as [`retain_tags`][TagSetMutView::retain_tags], without a new allocation.
    /// [`TagSetMutView::finish`] builds the new allocation and computes the new context key, and only if the view
    /// recorded a change.
    ///
    /// `state` provides reusable scratch space for tracking pending changes. Hold a long-lived
    /// [`TagSetMutViewState`] across calls to amortize its allocations.
    pub fn tags_mut_view<'a, 'b>(&'a mut self, state: &'b mut TagSetMutViewState) -> TagSetMutView<'a, 'b> {
        TagSetMutView { context: self, state }
    }

    /// Returns the size of this context in bytes.
    ///
    /// The size is the sum of these parts:
    ///
    /// - the 8-byte handle
    /// - the packed allocation, which holds the name, host, instrumented tags, and the origin tag handle
    /// - an estimate of the origin tag data
    ///
    /// The value doesn't count the shared symbol table or a strict-mode lease, because many contexts share them. It
    /// counts the full origin tag data for each context, even when contexts share it. Thus, use the value as a rough
    /// estimate.
    pub fn size_of(&self) -> usize {
        size_of::<Self>() + self.inner.allocation_bytes() + self.origin_tags().size_of()
    }
}

impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        // The 64-bit key stands in for the full contents. See the `# Design` section of `Context`.
        self.inner.key() == other.inner.key()
    }
}

impl Eq for Context {}

impl Hash for Context {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.key().hash(state);
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("name", &self.name())
            .field("host", &self.host())
            .field("tags", &self.tags())
            .field("key", &self.inner.key())
            .finish()
    }
}

impl From<&'static str> for Context {
    fn from(name: &'static str) -> Self {
        Self::from_static_name(name)
    }
}

impl From<(&'static str, &[&'static str])> for Context {
    fn from((name, tags): (&'static str, &[&'static str])) -> Self {
        Self::from_static_parts(name, tags)
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())?;
        let tags = self.tags();
        if !tags.is_empty() {
            write!(f, "{{")?;
            for (i, tag) in tags.into_iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{tag}")?;
            }
            write!(f, "}}")?;
        }
        Ok(())
    }
}

/// The instrumented tags of a context, in insertion order.
#[derive(Clone, Copy)]
pub struct ContextTags<'a>(&'a Packed);

impl<'a> ContextTags<'a> {
    /// Returns the number of tags.
    pub fn len(self) -> usize {
        self.0.tag_count()
    }

    /// Returns whether the context has no instrumented tags.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Returns whether the tag set has an overlay change.
    pub fn is_modified(self) -> bool {
        self.0.metadata().modified
    }

    /// Matches a complete tag, including its value if present.
    pub fn has_tag(self, text: impl AsRef<str>) -> bool {
        self.into_iter().any(|tag| tag == text.as_ref())
    }

    /// Returns the first matching tag by key, checking additions before base tags.
    pub fn get_single_tag(self, name: impl AsRef<str>) -> Option<&'a TagStr> {
        let mut base_match = None;
        let base_count = self.len() - self.0.metadata().additions;
        for (i, tag) in self.into_iter().enumerate() {
            if tag.has_name(name.as_ref()) {
                if i >= base_count {
                    return Some(tag);
                }
                if base_match.is_none() {
                    base_match = Some(tag);
                }
                if base_count == self.len() {
                    break;
                }
            }
        }
        base_match
    }

    /// Creates an owned mutable tag set in the same order.
    pub fn to_mutable(self) -> TagSet {
        let metadata = self.0.metadata();
        TagSet::from_context_tags(
            self.into_iter().map(TagStr::as_str),
            self.len() - metadata.additions,
            metadata.modified,
        )
    }
}

impl fmt::Debug for ContextTags<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.into_iter().map(TagStr::as_str)).finish()
    }
}

impl Serialize for ContextTags<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(*self)
    }
}

impl<'a> IntoIterator for ContextTags<'a> {
    type Item = &'a TagStr;
    type IntoIter = ContextTagsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        ContextTagsIter(self.0.tag_iter())
    }
}

/// An iterator over the borrowed instrumented tags of a context.
#[derive(Clone)]
pub struct ContextTagsIter<'a>(packed::TagIter<'a>);

impl<'a> Iterator for ContextTagsIter<'a> {
    type Item = &'a TagStr;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for ContextTagsIter<'_> {}

/// Reusable scratch space for [`TagSetMutView`] operations.
///
/// Hold a long-lived instance across calls to amortize bitset allocations. The bitsets are cleared automatically when
/// the associated [`TagSetMutView`] is dropped.
#[derive(Debug, Default)]
pub struct TagSetMutViewState {
    tag_removals: ContiguousBitSet,
    origin_base_removals: ContiguousBitSet,
    origin_addition_removals: ContiguousBitSet,
    hash_seen: PrehashedHashSet<u64>,
}

impl TagSetMutViewState {
    /// Creates a new, empty state.
    pub fn new() -> Self {
        Self::default()
    }

    fn clear(&mut self) {
        self.tag_removals.clear_all();
        self.origin_base_removals.clear_all();
        self.origin_addition_removals.clear_all();
    }
}

/// A lazy copy-on-write mutable view over the two tag sets of a [`Context`].
///
/// The view records operations, such as [`retain_tags`][Self::retain_tags], but does not apply them immediately.
/// [`finish`][Self::finish] builds the new allocation and computes the new context key, and only if the view recorded
/// a change.
pub struct TagSetMutView<'a, 'b> {
    context: &'a mut Context,
    state: &'b mut TagSetMutViewState,
}

impl TagSetMutView<'_, '_> {
    /// Scans the instrumented tags with the given predicate.
    ///
    /// Tags for which `f` returns `false` are flagged for removal. This is a read-only scan: no mutation occurs until
    /// [`finish`][Self::finish] is called. The scan visits added tags before base tags.
    pub fn retain_tags(&mut self, mut f: impl FnMut(&TagStr) -> bool) {
        let tags = self.context.tags();
        let additions = tags.0.metadata().additions;
        let base_count = tags.len() - additions;
        let removals = &mut self.state.tag_removals;
        let mut consider = |(index, tag)| {
            if !removals.is_set(index) && !f(tag) {
                removals.set(index);
            }
        };
        if additions != 0 {
            tags.into_iter().enumerate().skip(base_count).for_each(&mut consider);
        }
        tags.into_iter().enumerate().take(base_count).for_each(consider);
    }

    /// Scans the origin tags with the given predicate.
    ///
    /// Tags for which `f` returns `false` are flagged for removal. This is a read-only scan: no mutation occurs until
    /// [`finish`][Self::finish] is called.
    pub fn retain_origin_tags(&mut self, mut f: impl FnMut(&TagStr) -> bool) {
        self.context.origin_tags().collect_removals(
            |tag: &Tag| f(tag.as_tag_str()),
            &mut self.state.origin_base_removals,
            &mut self.state.origin_addition_removals,
        );
    }

    /// Applies all recorded changes, and returns the number of tags removed.
    ///
    /// If no changes were recorded, this does nothing and returns 0: no new allocation and no rehash. Otherwise, it
    /// builds a new allocation without the flagged tags and recomputes the context key. Other holders of the old
    /// context see no change.
    pub fn finish(self) -> usize {
        let removed = self.state.tag_removals.len()
            + self.state.origin_base_removals.len()
            + self.state.origin_addition_removals.len();
        if removed == 0 {
            return 0;
        }

        let context = &mut *self.context;
        let mut origin = context.origin_tags().clone();
        origin.apply_removals(&self.state.origin_base_removals, &self.state.origin_addition_removals);
        let removals = &self.state.tag_removals;
        let mut metadata = context.inner.metadata();
        let base_count = context.tags().len() - metadata.additions;
        metadata.additions -= removals.into_iter().filter(|&i| i >= base_count).count();
        metadata.modified |= !removals.is_empty();
        let tags = context
            .tags()
            .into_iter()
            .map(TagStr::as_str)
            .enumerate()
            .filter_map(|(i, tag)| (!removals.is_set(i)).then_some(tag));
        let name = context.inner.name();
        let host = context.host();
        let key = hash_context_with_host_and_seen(name, host, tags.clone(), &origin, &mut self.state.hash_seen).0;
        let lease = context
            .inner
            .lease()
            .map(|lease| lease.rewrite(name, host, tags.clone()));
        context.inner = Packed::new(
            Parts {
                key,
                name,
                host,
                origin,
                domain: context.inner.domain(),
                tracked: context.inner.track_rewrite(),
                metadata,
                lease: lease.as_ref(),
            },
            tags,
        );
        removed
    }
}

impl Drop for TagSetMutView<'_, '_> {
    fn drop(&mut self) {
        self.state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_set(tags: &[&str]) -> TagSet {
        tags.iter().map(|s| Tag::from(*s)).collect()
    }

    #[test]
    fn reports_inline_allocation_and_shared_origin_size() {
        let context = Context::from_static_parts("metric", &["env:prod", "region:us"]);
        let origin: TagSet = tag_set(&["origin:a"]).into_shared().into();
        let enriched = context.with_origin_tags(origin.clone());
        assert_eq!(context.size_of(), 96);
        assert_eq!(enriched.size_of(), 96 + size_of::<TagSet>() + origin.size_of());
        assert!(std::ptr::eq(
            origin.get_single_tag("origin").unwrap(),
            enriched.origin_tags().get_single_tag("origin").unwrap(),
        ));
    }

    #[test]
    fn packed_allocation_sizes_and_tag_order() {
        use std::{num::NonZeroUsize, sync::Arc};

        use stringtheory::interning::SymbolTable;

        let symbols = Arc::new(SymbolTable::new(1024 * 1024));
        let mut resolver = ContextResolverBuilder::for_tests()
            .with_interner_capacity_bytes(NonZeroUsize::new(1024 * 1024).unwrap())
            .with_symbol_table(Arc::clone(&symbols))
            .without_caching()
            .build();
        let name = "metric.a";
        assert!(symbols.try_intern(name).is_none());
        let _name_symbol = symbols.try_intern(name).unwrap();
        let shared: Vec<_> = (0..50)
            .map(|n| format!("shared_workload_context_distinct_tag_value_{n:08x}"))
            .collect();
        let unique: Vec<_> = (0..9)
            .map(|n| format!("unique_workload_context_distinct_tag_value_{n:08x}"))
            .collect();
        assert!(shared.iter().chain(&unique).all(|tag| tag.len() == 51));
        // Warm one symbol at a time so the fixture does not depend on the witness index distribution.
        let _shared_symbols: Vec<_> = shared
            .iter()
            .map(|tag| {
                assert!(symbols.try_intern(tag).is_none());
                symbols.try_intern(tag).unwrap()
            })
            .collect();
        let mut mixed = shared[..8].to_vec();
        mixed[3].clone_from(&unique[8]);

        assert_eq!(size_of::<Context>(), 8);
        for (tags, bytes) in [
            (&[][..], 56),
            (&shared[..8], 88),
            (&mixed[..], 144),
            (&unique[..8], 472),
            (&shared[..], 256),
        ] {
            // Use disjoint inline observations in each case. The mixed tag must remain inline in the unique case.
            let context = resolver.resolve(name, tags, None).unwrap();
            assert_eq!(context.inner.allocation_bytes(), bytes, "{tags:?}");
            assert_eq!(context.tags().into_iter().map(TagStr::as_str).collect::<Vec<_>>(), tags);
        }
    }

    #[test]
    fn borrowed_tags_preserve_inline_unicode_duplicates_and_addition_priority() {
        let mut tags = tag_set(&["k:base", "bare"]).into_shared().to_mutable();
        let long = format!("uni:{}", "𝄞".repeat(40));
        tags.insert_tag(Tag::from(long.as_str()));
        tags.insert_tag(Tag::from("k:added"));
        let context = Context::from_parts("metric", tags);
        assert_eq!(context.tags().get_single_tag("k").unwrap().value(), Some("added"));
        assert_eq!(
            context.tags().into_iter().map(TagStr::as_str).collect::<Vec<_>>(),
            ["k:base", "bare", &long, "k:added"]
        );
        let deduplicated = Context::from_static_parts("metric", &["k:1", "k:1", "k:2"]);
        assert_eq!(
            deduplicated.tags().into_iter().map(TagStr::as_str).collect::<Vec<_>>(),
            ["k:1", "k:2"]
        );
        assert_eq!(
            deduplicated.tags().to_mutable().get_single_tag("k").unwrap().value(),
            Some("1")
        );
    }

    #[test]
    fn with_tags_mut_clones_shared_context() {
        let original = Context::from_static_parts("metric", &["env:prod"]);
        let mut mutated = original.clone();

        // They share the same Arc before mutation.
        assert!(original.ptr_eq(&mutated));

        mutated.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("service:web"));
        });

        // After mutation, they no longer share the same inner.
        assert!(!original.ptr_eq(&mutated));
    }

    #[test]
    fn with_tags_mut_does_not_affect_original() {
        let original = Context::from_static_parts("metric", &["env:prod"]);
        let mut mutated = original.clone();

        mutated.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("service:web"));
        });

        // Original is unchanged.
        assert_eq!(original.tags().len(), 1);
        assert!(original.tags().has_tag("env:prod"));
        assert!(!original.tags().has_tag("service:web"));

        // Mutated has both tags.
        assert_eq!(mutated.tags().len(), 2);
        assert!(mutated.tags().has_tag("env:prod"));
        assert!(mutated.tags().has_tag("service:web"));
    }

    #[test]
    fn with_tags_mut_rehashes() {
        // Build a context and mutate it to add a tag.
        let mut mutated = Context::from_static_parts("metric", &["env:prod"]);
        mutated.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("service:web"));
        });

        // Build an equivalent context from scratch with both tags.
        let expected = Context::from_static_parts("metric", &["env:prod", "service:web"]);

        // The recomputed key should match a freshly-constructed context with the same state.
        assert_eq!(mutated, expected);

        // Modify a tag on the mutated context that _isn't_ shared with `expected` to ensure that there's no asymmetric
        // equality logic.
        mutated.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("cluster:foo"));
        });
        assert_ne!(mutated, expected);
    }

    #[test]
    fn with_origin_tags_mut_clones_shared_context() {
        let original = Context::from_static_name("metric");
        let mut mutated = original.clone();

        assert!(original.ptr_eq(&mutated));

        mutated.mutate_origin_tags(|tags| {
            tags.insert_tag(Tag::from("origin:tag"));
        });

        assert!(!original.ptr_eq(&mutated));
        assert!(original.origin_tags().is_empty());
        assert_eq!(mutated.origin_tags().len(), 1);
        assert!(mutated.origin_tags().has_tag("origin:tag"));
    }

    // --- Helper for contexts with origin tags ---

    fn context_with_origin(name: &'static str, tags: &[&'static str], origin_tags: &[&'static str]) -> Context {
        Context::from_static_parts(name, tags).with_origin_tags(tag_set(origin_tags))
    }

    fn context_with_host(
        name: &'static str, host: &'static str, tags: &[&'static str], origin_tags: &[&'static str],
    ) -> Context {
        context_with_origin(name, tags, origin_tags).with_host(MetaString::from_static(host))
    }

    #[test]
    fn host_participates_in_context_identity() {
        let host_a = context_with_host("metric", "host-a", &["env:prod"], &["origin:a"]);
        let host_b = context_with_host("metric", "host-b", &["env:prod"], &["origin:a"]);
        let host_a_again = context_with_host("metric", "host-a", &["env:prod"], &["origin:a"]);
        let no_host = context_with_origin("metric", &["env:prod"], &["origin:a"]);

        assert_ne!(host_a, host_b);
        assert_ne!(host_a, no_host);
        assert_eq!(host_a, host_a_again);
        assert_eq!(host_a.host(), Some("host-a"));
        assert_eq!(no_host.host(), None);
    }

    #[test]
    fn host_is_preserved_when_context_is_copied_with_new_parts() {
        let base = context_with_host("metric", "host-a", &["env:prod"], &["origin:a"]);

        let renamed = base.with_name("renamed");
        assert_eq!(
            renamed,
            context_with_host("renamed", "host-a", &["env:prod"], &["origin:a"])
        );
        assert_ne!(renamed, context_with_origin("renamed", &["env:prod"], &["origin:a"]));

        let retagged = base.with_tags(tag_set(&["service:web"]));
        assert_eq!(
            retagged,
            context_with_host("metric", "host-a", &["service:web"], &["origin:a"])
        );
        assert_ne!(retagged, context_with_origin("metric", &["service:web"], &["origin:a"]));

        let reorigined = base.with_origin_tags(tag_set(&["origin:b"]));
        assert_eq!(
            reorigined,
            context_with_host("metric", "host-a", &["env:prod"], &["origin:b"])
        );
        assert_ne!(reorigined, context_with_origin("metric", &["env:prod"], &["origin:b"]));

        let replaced = base.with_tags_and_origin_tags(tag_set(&["service:web"]), tag_set(&["origin:b"]));
        assert_eq!(
            replaced,
            context_with_host("metric", "host-a", &["service:web"], &["origin:b"])
        );
        assert_ne!(replaced, context_with_origin("metric", &["service:web"], &["origin:b"]));
    }

    #[test]
    fn host_is_preserved_when_context_tags_are_mutated() {
        let expected_with_tag = context_with_host("metric", "host-a", &["env:prod", "service:web"], &["origin:a"]);
        let expected_with_origin = context_with_host("metric", "host-a", &["env:prod"], &["origin:a", "origin:b"]);

        let mut tag_mutated = context_with_host("metric", "host-a", &["env:prod"], &["origin:a"]);
        tag_mutated.mutate_tags(|tags| tags.insert_tag(Tag::from("service:web")));
        assert_eq!(tag_mutated, expected_with_tag);
        assert_eq!(tag_mutated.host(), Some("host-a"));

        let mut origin_mutated = context_with_host("metric", "host-a", &["env:prod"], &["origin:a"]);
        origin_mutated.mutate_origin_tags(|tags| tags.insert_tag(Tag::from("origin:b")));
        assert_eq!(origin_mutated, expected_with_origin);
        assert_eq!(origin_mutated.host(), Some("host-a"));
    }

    #[test]
    fn host_is_preserved_when_tag_mut_view_rekeys_context() {
        let mut ctx = context_with_host(
            "metric",
            "host-a",
            &["env:prod", "service:web"],
            &["origin:a", "origin:b"],
        );
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "env");
        view.retain_origin_tags(|tag| tag.as_str() == "origin:a");
        assert_eq!(view.finish(), 2);

        assert_eq!(ctx, context_with_host("metric", "host-a", &["env:prod"], &["origin:a"]));
        assert_ne!(ctx, context_with_origin("metric", &["env:prod"], &["origin:a"]));
        assert_eq!(ctx.host(), Some("host-a"));
    }

    // --- TagSetMutView ---

    #[test]
    fn mut_view_retain_tags_removes_matching() {
        let mut ctx = Context::from_static_parts("metric", &["env:prod", "service:web", "region:us"]);
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "env");
        let removed = view.finish();

        assert_eq!(removed, 2);
        assert_eq!(ctx.tags().len(), 1);
        assert!(ctx.tags().has_tag("env:prod"));
        assert!(!ctx.tags().has_tag("service:web"));
        assert!(!ctx.tags().has_tag("region:us"));
    }

    #[test]
    fn mut_view_retain_origin_tags_removes_matching() {
        let mut ctx = context_with_origin("metric", &[], &["origin:a", "origin:b", "origin:c"]);
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_origin_tags(|tag| tag.as_str() == "origin:a");
        let removed = view.finish();

        assert_eq!(removed, 2);
        assert_eq!(ctx.origin_tags().len(), 1);
        assert!(ctx.origin_tags().has_tag("origin:a"));
        assert!(!ctx.origin_tags().has_tag("origin:b"));
        assert!(!ctx.origin_tags().has_tag("origin:c"));
    }

    #[test]
    fn mut_view_retain_both_tag_sets() {
        let mut ctx = context_with_origin("metric", &["env:prod", "service:web"], &["origin:a", "origin:b"]);
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "env");
        view.retain_origin_tags(|tag| tag.as_str() == "origin:a");
        let removed = view.finish();

        assert_eq!(removed, 2);
        assert_eq!(ctx.tags().len(), 1);
        assert!(ctx.tags().has_tag("env:prod"));
        assert!(!ctx.tags().has_tag("service:web"));
        assert_eq!(ctx.origin_tags().len(), 1);
        assert!(ctx.origin_tags().has_tag("origin:a"));
        assert!(!ctx.origin_tags().has_tag("origin:b"));
    }

    #[test]
    fn mut_view_retain_all_is_noop() {
        let original = Context::from_static_parts("metric", &["env:prod", "service:web"]);
        let mut ctx = original.clone();
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|_| true);
        let removed = view.finish();

        assert_eq!(removed, 0);
        assert!(ctx.ptr_eq(&original));
    }

    #[test]
    fn mut_view_retain_none_removes_all() {
        let mut ctx = Context::from_static_parts("metric", &["env:prod", "service:web", "region:us"]);
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|_| false);
        let removed = view.finish();

        assert_eq!(removed, 3);
        assert!(ctx.tags().is_empty());
    }

    #[test]
    fn mut_view_finish_returns_correct_count() {
        let mut ctx = context_with_origin("metric", &["a:1", "b:2", "c:3"], &["origin:x", "origin:y"]);
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        // Remove b:2 and c:3 (keep a:1).
        view.retain_tags(|tag| tag.name() == "a");
        // Remove origin:y (keep origin:x).
        view.retain_origin_tags(|tag| tag.as_str() == "origin:x");
        let removed = view.finish();

        assert_eq!(removed, 3);
        assert_eq!(ctx.tags().len(), 1);
        assert_eq!(ctx.origin_tags().len(), 1);
    }

    #[test]
    fn mut_view_equivalent_to_direct_mutate_tags() {
        let base = Context::from_static_parts("metric", &["env:prod", "service:web", "region:us"]);
        let predicate = |tag: &Tag| tag.name() == "env";

        // Path A: direct mutation.
        let mut direct = base.clone();
        direct.mutate_tags(|tags| tags.retain(predicate));

        // Path B: mut view.
        let mut via_view = base.clone();
        let mut state = TagSetMutViewState::new();
        let mut view = via_view.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "env");
        view.finish();

        assert_eq!(direct, via_view);
        assert_eq!(direct.tags().len(), via_view.tags().len());
        assert!(via_view.tags().has_tag("env:prod"));
        assert!(!via_view.tags().has_tag("service:web"));
    }

    #[test]
    fn mut_view_equivalent_to_direct_mutate_origin_tags() {
        let base = context_with_origin("metric", &["env:prod"], &["origin:a", "origin:b", "origin:c"]);
        let predicate = |tag: &TagStr| tag == "origin:a";

        // Path A: direct mutation.
        let mut direct = base.clone();
        direct.mutate_origin_tags(|tags| tags.retain(|tag| predicate(tag)));

        // Path B: mut view.
        let mut via_view = base.clone();
        let mut state = TagSetMutViewState::new();
        let mut view = via_view.tags_mut_view(&mut state);
        view.retain_origin_tags(predicate);
        view.finish();

        assert_eq!(direct, via_view);
        assert_eq!(direct.origin_tags().len(), via_view.origin_tags().len());
    }

    #[test]
    fn mut_view_does_not_affect_cloned_context() {
        let original = Context::from_static_parts("metric", &["env:prod", "service:web"]);
        let mut mutated = original.clone();
        let mut state = TagSetMutViewState::new();

        let mut view = mutated.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "env");
        view.finish();

        // Original is unchanged.
        assert_eq!(original.tags().len(), 2);
        assert!(original.tags().has_tag("env:prod"));
        assert!(original.tags().has_tag("service:web"));

        // Mutated has only the retained tag.
        assert_eq!(mutated.tags().len(), 1);
        assert!(!original.ptr_eq(&mutated));
    }

    #[test]
    fn mut_view_drop_without_finish_discards_changes() {
        let original = Context::from_static_parts("metric", &["env:prod", "service:web"]);
        let mut ctx = original.clone();
        let mut state = TagSetMutViewState::new();

        {
            let mut view = ctx.tags_mut_view(&mut state);
            view.retain_tags(|_| false); // Flag all for removal.
                                         // Drop without calling finish().
        }

        // Nothing changed.
        assert_eq!(ctx.tags().len(), 2);
        assert!(ctx.ptr_eq(&original));
    }

    #[test]
    fn mut_view_state_reuse_across_operations() {
        let mut state = TagSetMutViewState::new();

        // First operation.
        let mut ctx1 = Context::from_static_parts("metric1", &["a:1", "b:2"]);
        let mut view1 = ctx1.tags_mut_view(&mut state);
        view1.retain_tags(|tag| tag.name() == "a");
        let removed1 = view1.finish();

        assert_eq!(removed1, 1);
        assert_eq!(ctx1.tags().len(), 1);
        assert!(ctx1.tags().has_tag("a:1"));

        // Second operation reusing the same state.
        let mut ctx2 = Context::from_static_parts("metric2", &["x:1", "y:2", "z:3"]);
        let mut view2 = ctx2.tags_mut_view(&mut state);
        view2.retain_tags(|tag| tag.name() == "z");
        let removed2 = view2.finish();

        assert_eq!(removed2, 2);
        assert_eq!(ctx2.tags().len(), 1);
        assert!(ctx2.tags().has_tag("z:3"));
    }

    #[test]
    fn mut_view_retain_tags_with_additions() {
        // Start with a base tag, then add one via mutation to create an overlay.
        let mut ctx = Context::from_static_parts("metric", &["base:tag"]);
        ctx.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("added:tag"));
        });
        assert_eq!(ctx.tags().len(), 2);

        let mut state = TagSetMutViewState::new();
        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "added");
        let removed = view.finish();

        assert_eq!(removed, 1);
        assert_eq!(ctx.tags().len(), 1);
        assert!(ctx.tags().has_tag("added:tag"));
        assert!(!ctx.tags().has_tag("base:tag"));
    }

    #[test]
    fn mut_view_retain_tags_removes_only_additions() {
        let mut ctx = Context::from_static_parts("metric", &["base:tag"]);
        ctx.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("added:tag"));
        });

        let mut state = TagSetMutViewState::new();
        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() == "base");
        let removed = view.finish();

        assert_eq!(removed, 1);
        assert_eq!(ctx.tags().len(), 1);
        assert!(ctx.tags().has_tag("base:tag"));
        assert!(!ctx.tags().has_tag("added:tag"));
    }

    #[test]
    fn mut_view_retain_tags_removes_base_and_additions() {
        let mut ctx = Context::from_static_parts("metric", &["base:tag"]);
        ctx.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("added:tag"));
        });

        let mut state = TagSetMutViewState::new();
        let mut view = ctx.tags_mut_view(&mut state);
        view.retain_tags(|_| false);
        let removed = view.finish();

        assert_eq!(removed, 2);
        assert!(ctx.tags().is_empty());
    }

    #[test]
    fn mut_view_multiple_retain_calls_deduplicates() {
        // Two retain calls that both reject the same addition tag must not panic.
        // Semantics: a tag survives only if ALL predicates accept it.
        let mut ctx = Context::from_static_parts("metric", &["base:tag"]);
        ctx.mutate_tags(|tags| {
            tags.insert_tag(Tag::from("added:a"));
            tags.insert_tag(Tag::from("added:b"));
        });
        assert_eq!(ctx.tags().len(), 3);

        let mut state = TagSetMutViewState::new();
        let mut view = ctx.tags_mut_view(&mut state);
        // First predicate removes "added:a" (keeps base:tag and added:b).
        view.retain_tags(|tag| tag.as_str() != "added:a");
        // Second predicate removes "base:tag" (keeps added:a and added:b).
        // Combined effect: only "added:b" survives both predicates.
        // "added:a" is flagged by both calls -- its duplicate index must be deduplicated.
        view.retain_tags(|tag| tag.name() != "base");
        let removed = view.finish();

        assert_eq!(removed, 2);
        assert_eq!(ctx.tags().len(), 1);
        assert!(ctx.tags().has_tag("added:b"));
    }

    #[test]
    fn mut_view_multiple_retain_origin_calls_deduplicates() {
        let mut ctx = context_with_origin("metric", &[], &["origin:a", "origin:b", "origin:c"]);
        let mut state = TagSetMutViewState::new();

        let mut view = ctx.tags_mut_view(&mut state);
        // Both predicates reject "origin:c".
        view.retain_origin_tags(|tag| tag.as_str() != "origin:c");
        view.retain_origin_tags(|tag| tag.as_str() == "origin:a");
        let removed = view.finish();

        assert_eq!(removed, 2);
        assert_eq!(ctx.origin_tags().len(), 1);
        assert!(ctx.origin_tags().has_tag("origin:a"));
    }

    #[test]
    fn mut_view_multiple_retain_equivalent_to_combined_predicate() {
        let base = Context::from_static_parts("metric", &["env:prod", "service:web", "region:us", "cluster:main"]);

        // Path A: two separate retain calls.
        let mut via_two = base.clone();
        let mut state = TagSetMutViewState::new();
        let mut view = via_two.tags_mut_view(&mut state);
        view.retain_tags(|tag| tag.name() != "region");
        view.retain_tags(|tag| tag.name() != "cluster");
        view.finish();

        // Path B: single combined predicate.
        let mut via_one = base.clone();
        let mut state2 = TagSetMutViewState::new();
        let mut view2 = via_one.tags_mut_view(&mut state2);
        view2.retain_tags(|tag| tag.name() != "region" && tag.name() != "cluster");
        view2.finish();

        // Path C: direct mutation.
        let mut via_direct = base.clone();
        via_direct.mutate_tags(|tags| tags.retain(|tag| tag.name() != "region" && tag.name() != "cluster"));

        assert_eq!(via_two, via_one);
        assert_eq!(via_two, via_direct);
    }

    #[test]
    fn with_tag_sets_mut_mutates_both_and_recomputes_key() {
        // `with_tag_sets_mut` mutates both tag sets and recomputes the context key a single time
        // for the combined change. The observable guarantee is that the result matches a context
        // built from scratch with the combined tags, and matches applying the same two mutations
        // via separate `with_tags`/`with_origin_tags` calls (which would each rehash).
        let mut combined = context_with_origin("metric", &["env:prod"], &["origin:a"]);
        combined.with_tag_sets_mut(|tags, origin_tags| {
            tags.insert_tag(Tag::from("service:web"));
            origin_tags.insert_tag(Tag::from("origin:b"));
        });

        // Both tag sets were updated in the single call.
        assert!(combined.tags().has_tag("env:prod"));
        assert!(combined.tags().has_tag("service:web"));
        assert!(combined.origin_tags().has_tag("origin:a"));
        assert!(combined.origin_tags().has_tag("origin:b"));

        // The recomputed key matches a freshly-built context with the same final state.
        let expected = context_with_origin("metric", &["env:prod", "service:web"], &["origin:a", "origin:b"]);
        assert_eq!(combined, expected);

        // ...and matches applying the two mutations separately (i.e. via two rehashes).
        let separate = context_with_origin("metric", &["env:prod"], &["origin:a"])
            .with_tags(tag_set(&["env:prod", "service:web"]))
            .with_origin_tags(tag_set(&["origin:a", "origin:b"]));
        assert_eq!(combined, separate);
    }

    #[test]
    fn display_renders_name_and_instrumented_tags() {
        // With no tags, only the metric name is rendered.
        assert_eq!(Context::from_static_name("metric").to_string(), "metric");

        // With tags, they are rendered in a brace-delimited, comma-space-separated list in
        // insertion order. Origin tags are intentionally not part of the `Display` output.
        let context = Context::from_static_parts("metric", &["env:prod", "service:web"]);
        assert_eq!(context.to_string(), "metric{env:prod, service:web}");
    }
}
