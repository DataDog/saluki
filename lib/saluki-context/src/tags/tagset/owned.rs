use std::fmt;

use saluki_common::collections::ContiguousBitSet;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::{frozen::tag_has_name, SharedTagSet};
use crate::tags::Tag;

/// A mutable set of tags, optionally backed by a [`SharedTagSet`] base.
///
/// `TagSet` supports efficient mutation through an overlay approach: it wraps an immutable
/// [`SharedTagSet`] base with a small set of additions and a bitset tracking which base tags have
/// been removed. This avoids full materialization when only a few tags need to change.
///
/// # Construction
///
/// A `TagSet` can be created from scratch (empty base, tags go into additions) or by wrapping an
/// existing [`SharedTagSet`] for mutation. When constructed from scratch, the usage pattern is the
/// same as before: call [`insert_tag`][TagSet::insert_tag] in a loop, then
/// [`into_shared`][TagSet::into_shared] to freeze.
///
/// # Conversion
///
/// When converting back to [`SharedTagSet`] via [`into_shared`][TagSet::into_shared], if no
/// mutations have been made, the base is returned as-is with zero cost. Otherwise, the effective
/// tags are materialized into a new [`SharedTagSet`].
#[derive(Clone, Debug, Default)]
pub struct TagSet {
    /// Immutable base (structurally shared via Arc, never modified).
    base: SharedTagSet,
    /// Tags added or used to replace base tags.
    additions: SmallVec<[Tag; 4]>,
    /// Bitset of flattened base indices that have been removed or shadowed.
    /// Each bit corresponds to a tag position in the flattened base iteration order.
    removals: ContiguousBitSet,
}

impl TagSet {
    /// Creates a new, empty `TagSet` with the given capacity for additions.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            base: SharedTagSet::default(),
            additions: SmallVec::with_capacity(capacity),
            removals: ContiguousBitSet::new(),
        }
    }

    /// Returns `true` if the tag set is empty.
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty() && self.base_len_minus_removals() == 0
    }

    /// Returns the number of tags in the set.
    pub fn len(&self) -> usize {
        self.base_len_minus_removals() + self.additions.len()
    }

    /// Inserts a tag into the set.
    ///
    /// If a tag with the same complete value already exists (in additions or the base), this does
    /// nothing. If a tag with the same name but different value exists in the base, the base tag
    /// is shadowed (its removal bit is set) and the new tag is added to additions.
    pub fn insert_tag<T>(&mut self, tag: T)
    where
        T: Into<Tag>,
    {
        let tag = tag.into();

        // Check if the exact tag already exists in additions.
        if self.additions.iter().any(|existing| existing == &tag) {
            return;
        }

        // Check if the exact tag exists in the base (and is not removed).
        let found_in_base = base_indexed_iter(&self.base)
            .any(|(idx, base_tag)| !is_removed_in(&self.removals, idx) && base_tag == &tag);
        if found_in_base {
            return;
        }

        // Shadow any base tag with the same name: collect indices first to avoid borrow conflict.
        let tag_name = tag.name();
        let to_remove: SmallVec<[usize; 4]> = base_indexed_iter(&self.base)
            .filter(|&(idx, base_tag)| !is_removed_in(&self.removals, idx) && tag_has_name(base_tag, tag_name))
            .map(|(idx, _)| idx)
            .collect();
        for idx in to_remove {
            self.set_removed(idx);
        }

        // Replace any existing addition with the same name.
        self.additions.retain(|t| t.name() != tag_name);

        self.additions.push(tag);
    }

    /// Removes all tags with the given name from the set.
    ///
    /// Returns the removed tags, or `None` if no tags matched.
    pub fn remove_tags<T>(&mut self, tag_name: T) -> Option<Vec<Tag>>
    where
        T: AsRef<str>,
    {
        let tag_name = tag_name.as_ref();
        let mut removed = Vec::new();

        // Remove from additions.
        let mut i = 0;
        while i < self.additions.len() {
            if tag_has_name(&self.additions[i], tag_name) {
                removed.push(self.additions.remove(i));
            } else {
                i += 1;
            }
        }

        // Mark matching base tags as removed: collect indices and tags first.
        let base_matches: SmallVec<[usize; 4]> = base_indexed_iter(&self.base)
            .filter(|&(idx, base_tag)| !is_removed_in(&self.removals, idx) && tag_has_name(base_tag, tag_name))
            .map(|(idx, _)| idx)
            .collect();
        for &idx in &base_matches {
            // Clone the tag before we set the removal bit (base is immutable, so order doesn't matter).
            if let Some(tag) = self.base.get_by_flat_index(idx) {
                removed.push(tag.clone());
            }
            self.set_removed(idx);
        }

        if removed.is_empty() {
            None
        } else {
            Some(removed)
        }
    }

    /// Returns `true` if the given tag is contained in the set.
    ///
    /// This matches the complete tag, rather than just the name.
    pub fn has_tag<T>(&self, tag: T) -> bool
    where
        T: AsRef<str>,
    {
        let tag = tag.as_ref();

        // Check additions first.
        if self.additions.iter().any(|t| t.as_str() == tag) {
            return true;
        }

        // Check base, skipping removed.
        for (idx, base_tag) in base_indexed_iter(&self.base) {
            if !is_removed_in(&self.removals, idx) && base_tag.as_str() == tag {
                return true;
            }
        }

        false
    }

    /// Gets a single tag, by name, from the set.
    ///
    /// If multiple tags are present with the same name, the first tag with a matching name will be
    /// returned. Additions are checked before the base. If no tag in the set matches, `None` is
    /// returned.
    pub fn get_single_tag<T>(&self, tag_name: T) -> Option<&Tag>
    where
        T: AsRef<str>,
    {
        let tag_name = tag_name.as_ref();

        // Check additions first (they shadow base tags).
        if let Some(tag) = self.additions.iter().find(|t| tag_has_name(t, tag_name)) {
            return Some(tag);
        }

        // Check base, skipping removed.
        for (idx, base_tag) in base_indexed_iter(&self.base) {
            if !is_removed_in(&self.removals, idx) && tag_has_name(base_tag, tag_name) {
                return Some(base_tag);
            }
        }

        None
    }

    /// Retains only the tags specified by the predicate.
    ///
    /// In other words, remove all tags `t` for which `f(&t)` returns `false`.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&Tag) -> bool,
    {
        // Filter additions in-place.
        self.additions.retain(|tag| f(tag));

        // Set removal bits for rejected base tags: collect indices first.
        let to_remove: SmallVec<[usize; 4]> = base_indexed_iter(&self.base)
            .filter(|&(idx, base_tag)| !is_removed_in(&self.removals, idx) && !f(base_tag))
            .map(|(idx, _)| idx)
            .collect();
        for idx in to_remove {
            self.set_removed(idx);
        }
    }

    /// Merges the tags from another set into this set.
    ///
    /// If a tag from `other` is already present in this set, it will not be added.
    pub fn merge_missing(&mut self, other: Self) {
        for tag in other {
            self.insert_tag(tag);
        }
    }

    /// Merges the tags from a shared set into this set.
    ///
    /// If a tag from `other` is already present in this set, it will not be added.
    pub fn merge_missing_shared(&mut self, other: &SharedTagSet) {
        for tag in other {
            if !self.has_tag(tag.as_str()) {
                // We know this tag doesn't exist, so we can push directly.
                self.additions.push(tag.clone());
            }
        }
    }

    /// Consumes this `TagSet` and returns a shared, read-only version of it.
    ///
    /// If no mutations have been made (no additions, no removals), the base `SharedTagSet` is
    /// returned as-is with zero cost.
    pub fn into_shared(self) -> SharedTagSet {
        if !self.is_modified() {
            return self.base;
        }

        // Materialize: collect effective tags into a new SharedTagSet.
        let effective: Vec<Tag> = self.into_iter().collect();
        SharedTagSet::from_tags(effective)
    }

    /// Returns `true` if this `TagSet` has been modified from its base.
    pub fn is_modified(&self) -> bool {
        !self.additions.is_empty() || !self.removals.is_empty()
    }

    fn set_removed(&mut self, index: usize) {
        self.removals.set(index);
    }

    /// Returns the number of base tags minus the number of removed ones.
    fn base_len_minus_removals(&self) -> usize {
        self.base.len() - self.removals.len()
    }
}

impl PartialEq<TagSet> for TagSet {
    fn eq(&self, other: &TagSet) -> bool {
        if self.len() != other.len() {
            return false;
        }

        for other_tag in other {
            if !self.has_tag(other_tag.as_str()) {
                return false;
            }
        }

        true
    }
}

impl IntoIterator for TagSet {
    type Item = Tag;
    type IntoIter = TagSetIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        TagSetIntoIter {
            base: self.base,
            removals: self.removals,
            // Flatten the base tag sets into a collected vec of (index, tag) pairs that aren't
            // removed. We need to own the tags, so we clone from the Arc'd base.
            base_index: 0,
            base_phase_done: false,
            additions: self.additions.into_iter(),
        }
    }
}

/// Owned iterator over a `TagSet`.
pub struct TagSetIntoIter {
    base: SharedTagSet,
    removals: ContiguousBitSet,
    base_index: usize,
    base_phase_done: bool,
    additions: smallvec::IntoIter<[Tag; 4]>,
}

impl Iterator for TagSetIntoIter {
    type Item = Tag;

    fn next(&mut self) -> Option<Self::Item> {
        // First yield non-removed base tags.
        if !self.base_phase_done {
            loop {
                let idx = self.base_index;
                if let Some(tag) = self.base.get_by_flat_index(idx) {
                    self.base_index += 1;
                    if !self.removals.is_set(idx) {
                        return Some(tag.clone());
                    }
                } else {
                    self.base_phase_done = true;
                    break;
                }
            }
        }

        // Then yield additions.
        self.additions.next()
    }
}

impl<'a> IntoIterator for &'a TagSet {
    type Item = &'a Tag;
    type IntoIter = TagSetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        TagSetIter {
            base_iter: self.base.into_iter(),
            base_index: 0,
            removals: &self.removals,
            additions_iter: self.additions.iter(),
            base_phase_done: false,
        }
    }
}

/// Borrowing iterator over a `TagSet`.
pub struct TagSetIter<'a> {
    base_iter: super::SharedTagSetIterator<'a>,
    base_index: usize,
    removals: &'a ContiguousBitSet,
    additions_iter: std::slice::Iter<'a, Tag>,
    base_phase_done: bool,
}

impl<'a> Iterator for TagSetIter<'a> {
    type Item = &'a Tag;

    fn next(&mut self) -> Option<Self::Item> {
        // First yield non-removed base tags.
        if !self.base_phase_done {
            loop {
                if let Some(tag) = self.base_iter.next() {
                    let idx = self.base_index;
                    self.base_index += 1;
                    if !self.removals.is_set(idx) {
                        return Some(tag);
                    }
                } else {
                    self.base_phase_done = true;
                    break;
                }
            }
        }

        // Then yield additions.
        self.additions_iter.next()
    }
}

impl FromIterator<Tag> for TagSet {
    fn from_iter<I: IntoIterator<Item = Tag>>(iter: I) -> Self {
        Self {
            base: SharedTagSet::default(),
            additions: iter.into_iter().collect(),
            removals: ContiguousBitSet::new(),
        }
    }
}

impl Extend<Tag> for TagSet {
    fn extend<T: IntoIterator<Item = Tag>>(&mut self, iter: T) {
        self.additions.extend(iter)
    }
}

impl From<Tag> for TagSet {
    fn from(tag: Tag) -> Self {
        let mut additions = SmallVec::new();
        additions.push(tag);
        Self {
            base: SharedTagSet::default(),
            additions,
            removals: ContiguousBitSet::new(),
        }
    }
}

impl From<SharedTagSet> for TagSet {
    fn from(shared: SharedTagSet) -> Self {
        Self {
            base: shared,
            additions: SmallVec::new(),
            removals: ContiguousBitSet::new(),
        }
    }
}

impl fmt::Display for TagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;

        let mut first = true;
        for tag in self {
            if !first {
                write!(f, ",")?;
            }
            first = false;
            write!(f, "{}", tag.as_str())?;
        }

        write!(f, "]")
    }
}

impl Serialize for TagSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for tag in self {
            seq.serialize_element(tag)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for TagSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let tags = Vec::<Tag>::deserialize(deserializer)?;
        Ok(Self {
            base: SharedTagSet::default(),
            additions: SmallVec::from_vec(tags),
            removals: ContiguousBitSet::new(),
        })
    }
}

/// Creates an indexed iterator over the tags in a `SharedTagSet`.
fn base_indexed_iter(base: &SharedTagSet) -> BaseIndexIter<'_> {
    BaseIndexIter {
        inner: base.into_iter(),
        index: 0,
    }
}

/// Checks if a given flattened index is marked as removed in the bitset.
fn is_removed_in(removals: &ContiguousBitSet, index: usize) -> bool {
    removals.is_set(index)
}

/// Helper iterator that pairs base tags with their flattened index.
struct BaseIndexIter<'a> {
    inner: super::SharedTagSetIterator<'a>,
    index: usize,
}

impl<'a> Iterator for BaseIndexIter<'a> {
    type Item = (usize, &'a Tag);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|tag| {
            let idx = self.index;
            self.index += 1;
            (idx, tag)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a SharedTagSet from static tag strings.
    fn shared_from(tags: &[&str]) -> SharedTagSet {
        let ts: TagSet = tags.iter().map(|s| Tag::from(*s)).collect();
        ts.into_shared()
    }

    /// Helper: create a SharedTagSet with multiple chained FrozenTagSets.
    fn chained_shared(groups: &[&[&str]]) -> SharedTagSet {
        let mut shared = shared_from(groups[0]);
        for group in &groups[1..] {
            shared.extend_from_shared(&shared_from(group));
        }
        shared
    }

    /// Collect all tags from a TagSet as sorted strings for comparison.
    fn collect_sorted(ts: &TagSet) -> Vec<String> {
        let mut tags: Vec<String> = ts.into_iter().map(|t| t.as_str().to_string()).collect();
        tags.sort();
        tags
    }

    // --- Construction ---

    #[test]
    fn construction_from_scratch() {
        let mut ts = TagSet::with_capacity(3);
        ts.insert_tag(Tag::from("env:prod"));
        ts.insert_tag(Tag::from("service:web"));
        ts.insert_tag(Tag::from("bare_tag"));

        assert_eq!(ts.len(), 3);
        assert!(!ts.is_empty());
        assert!(ts.has_tag("env:prod"));
        assert!(ts.has_tag("service:web"));
        assert!(ts.has_tag("bare_tag"));
    }

    #[test]
    fn construction_empty() {
        let ts = TagSet::default();
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
    }

    // --- Round-trip ---

    #[test]
    fn round_trip_no_mutations() {
        let original = shared_from(&["a:1", "b:2", "c:3"]);
        let ts = TagSet::from(original.clone());

        // No mutations — into_shared should return the base as-is.
        assert!(!ts.is_modified());
        let result = ts.into_shared();
        assert_eq!(result, original);
    }

    #[test]
    fn round_trip_with_mutations() {
        let original = shared_from(&["a:1", "b:2"]);
        let mut ts = TagSet::from(original);
        ts.insert_tag(Tag::from("c:3"));

        let result = ts.into_shared();
        assert_eq!(result.len(), 3);
        assert!(result.has_tag("a:1"));
        assert!(result.has_tag("b:2"));
        assert!(result.has_tag("c:3"));
    }

    // --- Insert ---

    #[test]
    fn insert_duplicate_is_noop() {
        let mut ts = TagSet::default();
        ts.insert_tag(Tag::from("a:1"));
        ts.insert_tag(Tag::from("a:1"));
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn insert_duplicate_in_base_is_noop() {
        let base = shared_from(&["a:1", "b:2"]);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("a:1"));
        // Should not be modified since the exact tag exists in the base.
        assert!(!ts.is_modified());
        assert_eq!(ts.len(), 2);
    }

    #[test]
    fn insert_shadows_base_tag_with_same_name() {
        let base = shared_from(&["env:staging", "service:web"]);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("env:production"));

        assert_eq!(ts.len(), 2);
        let env_tag = ts.get_single_tag("env").unwrap();
        assert_eq!(env_tag.as_str(), "env:production");
        assert!(ts.has_tag("service:web"));
        assert!(!ts.has_tag("env:staging"));
    }

    #[test]
    fn insert_replaces_existing_addition_with_same_name() {
        let mut ts = TagSet::default();
        ts.insert_tag(Tag::from("env:staging"));
        ts.insert_tag(Tag::from("env:production"));

        assert_eq!(ts.len(), 1);
        assert_eq!(ts.get_single_tag("env").unwrap().as_str(), "env:production");
    }

    // --- Remove ---

    #[test]
    fn remove_from_base() {
        let base = shared_from(&["a:1", "b:2", "c:3"]);
        let mut ts = TagSet::from(base);

        let removed = ts.remove_tags("b").unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].as_str(), "b:2");

        assert_eq!(ts.len(), 2);
        assert!(ts.has_tag("a:1"));
        assert!(!ts.has_tag("b:2"));
        assert!(ts.has_tag("c:3"));
    }

    #[test]
    fn remove_from_additions() {
        let mut ts = TagSet::default();
        ts.insert_tag(Tag::from("a:1"));
        ts.insert_tag(Tag::from("b:2"));

        let removed = ts.remove_tags("a").unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].as_str(), "a:1");
        assert_eq!(ts.len(), 1);
        assert!(!ts.has_tag("a:1"));
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let base = shared_from(&["a:1"]);
        let mut ts = TagSet::from(base);
        assert!(ts.remove_tags("z").is_none());
    }

    #[test]
    fn remove_then_reinsert() {
        let base = shared_from(&["env:staging"]);
        let mut ts = TagSet::from(base);

        ts.remove_tags("env");
        assert!(!ts.has_tag("env:staging"));

        ts.insert_tag(Tag::from("env:production"));
        assert_eq!(ts.len(), 1);
        assert_eq!(ts.get_single_tag("env").unwrap().as_str(), "env:production");
        assert!(!ts.has_tag("env:staging"));
    }

    // --- Iteration ---

    #[test]
    fn iteration_order_base_then_additions() {
        let base = shared_from(&["a:1", "b:2"]);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("c:3"));

        let tags: Vec<&str> = (&ts).into_iter().map(|t| t.as_str()).collect();
        assert_eq!(tags, vec!["a:1", "b:2", "c:3"]);
    }

    #[test]
    fn iteration_skips_removed() {
        let base = shared_from(&["a:1", "b:2", "c:3"]);
        let mut ts = TagSet::from(base);
        ts.remove_tags("b");

        let tags: Vec<&str> = (&ts).into_iter().map(|t| t.as_str()).collect();
        assert_eq!(tags, vec!["a:1", "c:3"]);
    }

    // --- Chained bases ---

    #[test]
    fn remove_from_second_chain() {
        let base = chained_shared(&[&["a:1", "b:2"], &["c:3", "d:4"]]);
        assert_eq!(base.len(), 4);

        let mut ts = TagSet::from(base);
        ts.remove_tags("c");

        assert_eq!(ts.len(), 3);
        assert_eq!(collect_sorted(&ts), vec!["a:1", "b:2", "d:4"]);
    }

    #[test]
    fn insert_shadowing_tag_in_second_chain() {
        let base = chained_shared(&[&["a:1"], &["b:2"]]);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("b:replaced"));

        assert_eq!(ts.len(), 2);
        assert_eq!(ts.get_single_tag("b").unwrap().as_str(), "b:replaced");
    }

    // --- Clone independence ---

    #[test]
    fn clone_is_independent() {
        let base = shared_from(&["a:1", "b:2"]);
        let mut ts = TagSet::from(base);
        let mut cloned = ts.clone();

        ts.insert_tag(Tag::from("c:3"));
        cloned.remove_tags("a");

        assert_eq!(ts.len(), 3);
        assert_eq!(cloned.len(), 1);
        assert!(ts.has_tag("a:1"));
        assert!(!cloned.has_tag("a:1"));
    }

    // --- into_shared optimization ---

    #[test]
    fn into_shared_unmodified_returns_base() {
        let base = shared_from(&["a:1", "b:2"]);
        let ts = TagSet::from(base.clone());
        let result = ts.into_shared();
        // They should be equal.
        assert_eq!(result, base);
    }

    // --- len correctness ---

    #[test]
    fn len_accounts_for_all_layers() {
        let base = shared_from(&["a:1", "b:2", "c:3"]);
        let mut ts = TagSet::from(base);

        assert_eq!(ts.len(), 3);

        ts.remove_tags("b");
        assert_eq!(ts.len(), 2);

        ts.insert_tag(Tag::from("d:4"));
        assert_eq!(ts.len(), 3);

        ts.insert_tag(Tag::from("a:replaced")); // Shadows base a:1
        assert_eq!(ts.len(), 3);
    }

    // --- Retain ---

    #[test]
    fn retain_filters_base_and_additions() {
        let base = shared_from(&["a:1", "b:2"]);
        let mut ts = TagSet::from(base);
        ts.insert_tag(Tag::from("c:3"));

        // Keep only tags whose name starts with 'a' or 'c'.
        ts.retain(|tag| {
            let name = tag.name();
            name.starts_with('a') || name.starts_with('c')
        });

        assert_eq!(ts.len(), 2);
        assert_eq!(collect_sorted(&ts), vec!["a:1", "c:3"]);
    }

    // --- Large base (bitset growth) ---

    #[test]
    fn large_base_beyond_128_tags() {
        let tags: Vec<String> = (0..200).map(|i| format!("tag{}:{}", i, i)).collect();
        let tag_strs: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
        let base = shared_from(&tag_strs);

        let mut ts = TagSet::from(base);
        // Remove a tag near the end (index > 128, requiring bitset growth).
        ts.remove_tags("tag199");
        assert_eq!(ts.len(), 199);
        assert!(!ts.has_tag("tag199:199"));
        assert!(ts.has_tag("tag0:0"));
        assert!(ts.has_tag("tag150:150"));
    }

    // --- Merge ---

    #[test]
    fn merge_missing_skips_duplicates() {
        let base = shared_from(&["a:1", "b:2"]);
        let mut ts = TagSet::from(base);

        let other: TagSet = vec![Tag::from("b:2"), Tag::from("c:3")].into_iter().collect();
        ts.merge_missing(other);

        assert_eq!(ts.len(), 3);
        assert_eq!(collect_sorted(&ts), vec!["a:1", "b:2", "c:3"]);
    }

    #[test]
    fn merge_missing_shared() {
        let base = shared_from(&["a:1"]);
        let mut ts = TagSet::from(base);

        let other = shared_from(&["a:1", "b:2"]);
        ts.merge_missing_shared(&other);

        assert_eq!(ts.len(), 2);
        assert_eq!(collect_sorted(&ts), vec!["a:1", "b:2"]);
    }

    // --- to_mutable convenience ---

    #[test]
    fn shared_to_mutable() {
        let shared = shared_from(&["a:1", "b:2"]);
        let mut ts = shared.to_mutable();
        ts.insert_tag(Tag::from("c:3"));
        assert_eq!(ts.len(), 3);
    }
}

#[cfg(test)]
mod proptests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    /// Operations we can apply to a TagSet.
    #[derive(Clone, Debug)]
    enum Op {
        Insert(String),
        RemoveByName(String),
    }

    /// Strategy for generating a tag string like "key:value".
    fn arb_tag() -> impl Strategy<Value = String> {
        ("[a-z]{1,4}", "[a-z0-9]{1,4}").prop_map(|(k, v)| format!("{k}:{v}"))
    }

    /// Strategy for generating a tag key name.
    fn arb_key() -> impl Strategy<Value = String> {
        "[a-z]{1,4}".prop_map(|s| s)
    }

    /// Strategy for generating a random operation.
    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![arb_tag().prop_map(Op::Insert), arb_key().prop_map(Op::RemoveByName),]
    }

    /// Strategy for generating a group of tags (for one FrozenTagSet in the chain).
    fn arb_tag_group() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(arb_tag(), 0..10)
    }

    /// Strategy for generating a base with 1-3 chained tag groups.
    fn arb_base_groups() -> impl Strategy<Value = Vec<Vec<String>>> {
        prop::collection::vec(arb_tag_group(), 1..4)
    }

    /// Build a SharedTagSet from multiple groups (each becomes a chained FrozenTagSet).
    /// Deduplicates tags by name across groups to avoid cross-chain duplicates, which
    /// SharedTagSet allows but would complicate the reference model.
    fn build_chained_base(groups: &[Vec<String>]) -> SharedTagSet {
        let mut seen_names = std::collections::HashSet::new();

        let mut shared = {
            let ts: TagSet = groups[0]
                .iter()
                .filter(|s| {
                    let name = s.split_once(':').map_or(s.as_str(), |(n, _)| n);
                    seen_names.insert(name.to_string())
                })
                .map(|s| Tag::from(s.as_str()))
                .collect();
            ts.into_shared()
        };
        for group in &groups[1..] {
            let ts: TagSet = group
                .iter()
                .filter(|s| {
                    let name = s.split_once(':').map_or(s.as_str(), |(n, _)| n);
                    seen_names.insert(name.to_string())
                })
                .map(|s| Tag::from(s.as_str()))
                .collect();
            shared.extend_from_shared(&ts.into_shared());
        }
        shared
    }

    /// Apply operations to a naive reference model (BTreeMap<name, full_tag>).
    /// This mirrors the semantics of TagSet: insert_tag replaces by name, remove_tags removes by name.
    fn apply_to_reference(reference: &mut BTreeMap<String, String>, op: &Op) {
        match op {
            Op::Insert(tag) => {
                let name = tag.split_once(':').map_or(tag.as_str(), |(n, _)| n);
                reference.insert(name.to_string(), tag.clone());
            }
            Op::RemoveByName(name) => {
                reference.remove(name);
            }
        }
    }

    /// Apply operations to a TagSet.
    fn apply_to_tagset(ts: &mut TagSet, op: &Op) {
        match op {
            Op::Insert(tag) => {
                ts.insert_tag(Tag::from(tag.as_str()));
            }
            Op::RemoveByName(name) => {
                ts.remove_tags(name.as_str());
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10000))]

        #[test]
        #[cfg_attr(miri, ignore)]
        fn overlay_matches_reference(
            base_groups in arb_base_groups(),
            ops in prop::collection::vec(arb_op(), 0..20),
        ) {
            let base = build_chained_base(&base_groups);

            // Build the reference model from the base.
            let mut reference = BTreeMap::new();
            for tag in &base {
                let name = tag.name().to_string();
                // First occurrence wins for duplicate names (matching TagSet semantics).
                reference.entry(name).or_insert_with(|| tag.as_str().to_string());
            }

            // Build the TagSet from the base.
            let mut ts = TagSet::from(base);

            // Apply operations to both.
            for op in &ops {
                apply_to_reference(&mut reference, op);
                apply_to_tagset(&mut ts, op);
            }

            // Compare: collect TagSet tags as a BTreeMap for comparison.
            let mut ts_map = BTreeMap::new();
            for tag in &ts {
                let name = tag.name().to_string();
                ts_map.entry(name).or_insert_with(|| tag.as_str().to_string());
            }

            prop_assert_eq!(&reference, &ts_map,
                "TagSet and reference diverged after ops: {:?}", ops);

            // Also verify len matches.
            prop_assert_eq!(ts.len(), reference.len(),
                "len mismatch: TagSet={}, reference={}", ts.len(), reference.len());
        }
    }
}
