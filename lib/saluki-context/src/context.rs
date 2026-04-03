use std::{fmt, hash, sync::Arc};

use metrics::Gauge;
use stringtheory::MetaString;

use crate::{
    hash::{hash_context, ContextKey},
    tags::{Tag, TagSet},
};

const BASE_CONTEXT_SIZE: usize = std::mem::size_of::<Context>() + std::mem::size_of::<ContextInner>();

/// A metric context.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Creates a new `Context` from the given static name.
    pub fn from_static_name(name: &'static str) -> Self {
        let tags = TagSet::default();
        let origin_tags = TagSet::default();

        let (key, _) = hash_context(name, &tags, &origin_tags);
        Self {
            inner: Arc::new(ContextInner {
                name: MetaString::from_static(name),
                tags,
                origin_tags,
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

        let origin_tags = TagSet::default();

        let (key, _) = hash_context(name, tags, &origin_tags);
        Self {
            inner: Arc::new(ContextInner {
                name: MetaString::from_static(name),
                tags: tag_set,
                origin_tags,
                key,
                active_count: Gauge::noop(),
            }),
        }
    }

    /// Creates a new `Context` from the given name and given tags.
    pub fn from_parts<S: Into<MetaString>>(name: S, tags: impl Into<TagSet>) -> Self {
        let name = name.into();
        let tags = tags.into();
        let origin_tags = TagSet::default();
        let (key, _) = hash_context(&name, &tags, &origin_tags);
        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                origin_tags,
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
        let origin_tags = self.inner.origin_tags.clone();
        let (key, _) = hash_context(&name, &tags, &origin_tags);

        Self {
            inner: Arc::new(ContextInner {
                name,
                tags,
                origin_tags,
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
    pub fn origin_tags(&self) -> &TagSet {
        &self.inner.origin_tags
    }

    /// Mutates the instrumented tags of this context via a closure.
    ///
    /// Uses copy-on-write semantics: if this context shares its inner data with other clones, the
    /// inner data is cloned first so that mutations do not affect other holders. If this context is
    /// the sole owner, the mutation happens in place.
    ///
    /// The context key is automatically recomputed after the closure returns.
    pub fn mutate_tags(&mut self, f: impl FnOnce(&mut TagSet)) {
        self.mutate_inner(|inner| f(&mut inner.tags));
    }

    /// Mutates the origin tags of this context via a closure.
    ///
    /// Uses copy-on-write semantics: if this context shares its inner data with other clones, the
    /// inner data is cloned first so that mutations do not affect other holders. If this context is
    /// the sole owner, the mutation happens in place.
    ///
    /// The context key is automatically recomputed after the closure returns.
    pub fn mutate_origin_tags(&mut self, f: impl FnOnce(&mut TagSet)) {
        self.mutate_inner(|inner| f(&mut inner.origin_tags));
    }

    /// Runs the given closure on the inner context data, recomputing the context key afterwards.
    ///
    /// When the inner context state is shared (we aren't the only ones with a strong reference), we clone the inner
    /// data first to have our own copy. Otherwise, we modify the inner data in place.
    fn mutate_inner(&mut self, f: impl FnOnce(&mut ContextInner)) {
        let inner = Arc::make_mut(&mut self.inner);
        f(inner);
        let (key, _) = hash_context(&inner.name, &inner.tags, &inner.origin_tags);
        inner.key = key;
    }

    /// Creates a lazy copy-on-write mutable view over this context's tag sets.
    ///
    /// The returned view supports mutations (e.g. [`retain_tags`][TagSetMutView::retain_tags])
    /// without immediately triggering an `Arc` clone. The actual clone, mutation, and context key
    /// recomputation only happen when [`TagSetMutView::finish`] is called, and only if changes
    /// were actually recorded.
    ///
    /// `state` provides reusable scratch space for tracking pending changes. Holding a
    /// long-lived [`TagSetMutViewState`] across calls amortizes any vector allocations.
    pub fn tags_mut_view<'a, 'b>(&'a mut self, state: &'b mut TagSetMutViewState) -> TagSetMutView<'a, 'b> {
        TagSetMutView { context: self, state }
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
    /// potentially being shared by multiple contexts, or whether or not tags are inlined, interned, or heap
    /// allocated. This means that the value returned is essentially the worst-case usage, and should be used as a rough
    /// estimate.
    pub fn size_of(&self) -> usize {
        let name_size = self.inner.name.len();
        let tags_size = self.inner.tags.size_of();
        let origin_tags_size = self.inner.origin_tags.size_of();

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

/// Reusable scratch space for [`TagSetMutView`] operations.
///
/// Holding a long-lived instance across calls amortizes vector allocations. The vectors are
/// cleared automatically when the associated [`TagSetMutView`] is dropped.
#[derive(Debug, Default)]
pub struct TagSetMutViewState {
    tag_base_removals: Vec<usize>,
    tag_addition_removals: Vec<usize>,
    origin_base_removals: Vec<usize>,
    origin_addition_removals: Vec<usize>,
}

impl TagSetMutViewState {
    /// Creates a new, empty state.
    pub fn new() -> Self {
        Self::default()
    }

    fn clear(&mut self) {
        self.tag_base_removals.clear();
        self.tag_addition_removals.clear();
        self.origin_base_removals.clear();
        self.origin_addition_removals.clear();
    }
}

/// A lazy copy-on-write mutable view over a [`Context`]'s tag sets.
///
/// Operations on this view (e.g. [`retain_tags`][Self::retain_tags]) are recorded but not
/// applied immediately. The actual `Arc` clone, mutation, and context key recomputation only
/// occur when [`finish`][Self::finish] is called, and only if changes were recorded.
pub struct TagSetMutView<'a, 'b> {
    context: &'a mut Context,
    state: &'b mut TagSetMutViewState,
}

impl<'a, 'b> TagSetMutView<'a, 'b> {
    /// Scan instrumented tags with the given predicate.
    ///
    /// Tags for which `f` returns `false` are flagged for removal. This is a read-only scan;
    /// no mutation occurs until [`finish`][Self::finish] is called.
    pub fn retain_tags(&mut self, f: impl FnMut(&Tag) -> bool) {
        self.context.inner.tags.collect_removals(
            f,
            &mut self.state.tag_base_removals,
            &mut self.state.tag_addition_removals,
        );
    }

    /// Scan origin tags with the given predicate.
    ///
    /// Tags for which `f` returns `false` are flagged for removal. This is a read-only scan;
    /// no mutation occurs until [`finish`][Self::finish] is called.
    pub fn retain_origin_tags(&mut self, f: impl FnMut(&Tag) -> bool) {
        self.context.inner.origin_tags.collect_removals(
            f,
            &mut self.state.origin_base_removals,
            &mut self.state.origin_addition_removals,
        );
    }

    /// Apply all recorded changes and return the total number of tags affected.
    ///
    /// If no changes were recorded, this is a no-op: no `Arc` clone, no rehash, returns 0.
    /// Otherwise, triggers `Arc::make_mut` on the context, applies the changes to both tag sets,
    /// and recomputes the context key.
    pub fn finish(self) -> usize {
        let total_tags = self.state.tag_base_removals.len() + self.state.tag_addition_removals.len();
        let total_origin = self.state.origin_base_removals.len() + self.state.origin_addition_removals.len();
        let total = total_tags + total_origin;

        if total == 0 {
            return 0;
        }

        let inner = Arc::make_mut(&mut self.context.inner);

        if total_tags > 0 {
            inner
                .tags
                .apply_removals(&self.state.tag_base_removals, &self.state.tag_addition_removals);
        }
        if total_origin > 0 {
            inner
                .origin_tags
                .apply_removals(&self.state.origin_base_removals, &self.state.origin_addition_removals);
        }

        let (key, _) = hash_context(&inner.name, &inner.tags, &inner.origin_tags);
        inner.key = key;

        total
    }
}

impl Drop for TagSetMutView<'_, '_> {
    fn drop(&mut self) {
        self.state.clear();
    }
}

pub(super) struct ContextInner {
    key: ContextKey,
    name: MetaString,
    tags: TagSet,
    origin_tags: TagSet,
    active_count: Gauge,
}

impl ContextInner {
    pub fn from_parts(
        key: ContextKey, name: MetaString, tags: TagSet, origin_tags: TagSet, active_count: Gauge,
    ) -> Self {
        Self {
            key,
            name,
            tags,
            origin_tags,
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
    use super::*;
    use crate::tags::Tag;

    const SIZE_OF_CONTEXT_NAME: &str = "size_of_test_metric";
    const SIZE_OF_CONTEXT_CHANGED_NAME: &str = "size_of_test_metric_changed";
    const SIZE_OF_CONTEXT_TAGS: &[&str] = &["size_of_test_tag1", "size_of_test_tag2"];
    const SIZE_OF_CONTEXT_ORIGIN_TAGS: &[&str] = &["size_of_test_origin_tag1", "size_of_test_origin_tag2"];

    fn tag_set(tags: &[&str]) -> TagSet {
        tags.iter().map(|s| Tag::from(*s)).collect::<TagSet>()
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
        let origin_tags = tag_set(SIZE_OF_CONTEXT_ORIGIN_TAGS);

        let (key, _) = hash_context(SIZE_OF_CONTEXT_NAME, SIZE_OF_CONTEXT_TAGS, SIZE_OF_CONTEXT_ORIGIN_TAGS);

        let context = Context::from_inner(ContextInner {
            key,
            name: MetaString::from_static(SIZE_OF_CONTEXT_NAME),
            tags: tags.clone(),
            origin_tags: origin_tags.clone(),
            active_count: Gauge::noop(),
        });

        // Make sure the size of the context is correct with origin tags.
        assert_eq!(
            context.size_of(),
            BASE_CONTEXT_SIZE + SIZE_OF_CONTEXT_NAME.len() + tags.size_of() + origin_tags.size_of()
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
        let (key, _) = hash_context(name, tags, origin_tags);
        Context::from_inner(ContextInner {
            key,
            name: MetaString::from_static(name),
            tags: tag_set(tags),
            origin_tags: tag_set(origin_tags),
            active_count: Gauge::noop(),
        })
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
        view.retain_tags(predicate);
        view.finish();

        assert_eq!(direct, via_view);
        assert_eq!(direct.tags().len(), via_view.tags().len());
        assert!(via_view.tags().has_tag("env:prod"));
        assert!(!via_view.tags().has_tag("service:web"));
    }

    #[test]
    fn mut_view_equivalent_to_direct_mutate_origin_tags() {
        let base = context_with_origin("metric", &["env:prod"], &["origin:a", "origin:b", "origin:c"]);
        let predicate = |tag: &Tag| tag.as_str() == "origin:a";

        // Path A: direct mutation.
        let mut direct = base.clone();
        direct.mutate_origin_tags(|tags| tags.retain(predicate));

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
}
