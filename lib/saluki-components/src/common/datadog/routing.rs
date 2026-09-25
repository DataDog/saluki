//! Routing targets for metrics payloads.
//!
//! A routing target is one physical endpoint the Datadog forwarder delivers to: one intake URL paired with one API key.
//! The [`RoutingTargetCatalog`] enumerates every target once and gives each a stable [`RoutingTargetId`], so an encoder
//! can address a payload to a set of targets and the forwarder can deliver it to exactly those targets.
//!
//! The catalog is built once from configuration and shared by every encoder that addresses payloads and by the
//! forwarder that delivers them. Sharing one catalog is what keeps the two sides agreeing on what each identifier means.
//!
//! A payload addressed with [`MetricsRoutingTargets`] reaches only the targets it names. A payload without it reaches
//! every target, as it always has.

use std::{collections::HashMap, fmt};

use saluki_error::{ErrorContext as _, GenericError};
use smallvec::SmallVec;

use super::endpoints::resolve_additional_endpoints;

/// Identifies one routing target within a [`RoutingTargetCatalog`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RoutingTargetId(u32);

impl RoutingTargetId {
    fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("routing target count must fit in a u32"))
    }

    /// Returns the position of this target in its catalog.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for RoutingTargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A set of routing targets.
///
/// The set is a bitset indexed by [`RoutingTargetId`]. It never holds trailing empty words, so two sets holding the
/// same targets always compare and hash equal.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct RoutingTargetSet {
    words: SmallVec<[u64; 1]>,
}

impl RoutingTargetSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a target to the set.
    pub fn insert(&mut self, id: RoutingTargetId) {
        let (word, bit) = (id.index() / 64, id.index() % 64);
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << bit;
    }

    /// Returns `true` if the set holds the given target.
    pub fn contains(&self, id: RoutingTargetId) -> bool {
        let (word, bit) = (id.index() / 64, id.index() % 64);
        self.words.get(word).is_some_and(|word| word & (1 << bit) != 0)
    }

    /// Returns `true` if the set holds no targets.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Returns the number of targets in the set.
    pub fn len(&self) -> usize {
        self.words.iter().map(|word| word.count_ones() as usize).sum()
    }

    /// Adds every target in `other` to this set.
    pub fn union_with(&mut self, other: &Self) {
        if self.words.len() < other.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (word, other) in self.words.iter_mut().zip(&other.words) {
            *word |= other;
        }
    }

    /// Returns the targets in the set, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = RoutingTargetId> + '_ {
        self.words.iter().enumerate().flat_map(|(word_index, word)| {
            (0..64)
                .filter(move |bit| word & (1 << bit) != 0)
                .map(move |bit| RoutingTargetId::from_index(word_index * 64 + bit))
        })
    }
}

impl FromIterator<RoutingTargetId> for RoutingTargetSet {
    fn from_iter<I: IntoIterator<Item = RoutingTargetId>>(iter: I) -> Self {
        let mut set = Self::new();
        for id in iter {
            set.insert(id);
        }
        set
    }
}

/// How the forwarder reaches a routing target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingTargetKind {
    /// The primary endpoint, built from `dd_url` or `site` and `api_key`.
    ///
    /// When an alternate metrics intake replaces the primary endpoint for metrics, the alternate intake receives the
    /// payloads addressed to this target.
    Primary,

    /// One API key of one configured additional endpoint.
    Additional,
}

/// One routing target in a [`RoutingTargetCatalog`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingTarget {
    id: RoutingTargetId,
    kind: RoutingTargetKind,
    configured_endpoint: String,
    api_key_index: Option<usize>,
}

impl RoutingTarget {
    /// Returns the target's identifier.
    pub const fn id(&self) -> RoutingTargetId {
        self.id
    }

    /// Returns how the forwarder reaches this target.
    pub const fn kind(&self) -> RoutingTargetKind {
        self.kind
    }

    /// Returns the target's endpoint as configuration spells it.
    ///
    /// For the primary target this is the effective primary endpoint. For an additional target it is the key of its
    /// `additional_endpoints` entry.
    pub fn configured_endpoint(&self) -> &str {
        &self.configured_endpoint
    }

    /// Returns the position of this target's API key in its `additional_endpoints` entry.
    ///
    /// `None` for the primary target.
    #[cfg(test)]
    pub(crate) const fn api_key_index(&self) -> Option<usize> {
        self.api_key_index
    }
}

/// The routing targets of one Datadog forwarder.
///
/// Identifiers are assigned deterministically: the primary target comes first, followed by every additional endpoint
/// API key sorted by configured endpoint and then by the key's position in that endpoint's list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingTargetCatalog {
    targets: Vec<RoutingTarget>,
}

impl RoutingTargetCatalog {
    /// Creates a catalog holding the primary endpoint and every configured additional endpoint API key.
    ///
    /// Additional endpoint keys that the forwarder skips -- empty keys, repeated keys, and keys that cannot be sent as
    /// an HTTP header -- are skipped here too, so the catalog holds exactly the endpoints the forwarder builds.
    ///
    /// # Errors
    ///
    /// Returns an error if an additional endpoint URL is invalid.
    pub fn new(
        primary_endpoint: &str, additional_endpoints: &HashMap<String, Vec<String>>,
    ) -> Result<Self, GenericError> {
        let mut additional = resolve_additional_endpoints(additional_endpoints)
            .error_context("Failed parsing/resolving the additional destination endpoints.")?
            .into_iter()
            .filter_map(|endpoint| {
                let (configured_endpoint, api_key_index) = endpoint.additional_endpoint_queue_key()?;
                Some((configured_endpoint.to_string(), api_key_index))
            })
            .collect::<Vec<_>>();
        additional.sort_unstable();

        let mut targets = Vec::with_capacity(additional.len() + 1);
        targets.push(RoutingTarget {
            id: RoutingTargetId::from_index(0),
            kind: RoutingTargetKind::Primary,
            configured_endpoint: primary_endpoint.to_string(),
            api_key_index: None,
        });
        for (configured_endpoint, api_key_index) in additional {
            targets.push(RoutingTarget {
                id: RoutingTargetId::from_index(targets.len()),
                kind: RoutingTargetKind::Additional,
                configured_endpoint,
                api_key_index: Some(api_key_index),
            });
        }

        Ok(Self { targets })
    }

    /// Returns the primary target.
    pub fn primary(&self) -> RoutingTargetId {
        self.targets[0].id
    }

    /// Returns every target, ordered by identifier.
    pub fn targets(&self) -> &[RoutingTarget] {
        &self.targets
    }

    /// Returns the target with the given identifier.
    pub fn get(&self, id: RoutingTargetId) -> Option<&RoutingTarget> {
        self.targets.get(id.index())
    }

    /// Returns the targets matching `predicate`.
    pub fn select(&self, mut predicate: impl FnMut(&RoutingTarget) -> bool) -> RoutingTargetSet {
        self.targets
            .iter()
            .filter(|target| predicate(target))
            .map(RoutingTarget::id)
            .collect()
    }

    /// Returns the additional target for the given configured endpoint and API key position, if any.
    pub(crate) fn find_additional(&self, configured_endpoint: &str, api_key_index: usize) -> Option<RoutingTargetId> {
        self.targets
            .iter()
            .find(|target| {
                target.kind == RoutingTargetKind::Additional
                    && target.api_key_index == Some(api_key_index)
                    && target.configured_endpoint == configured_endpoint
            })
            .map(RoutingTarget::id)
    }
}

/// The routing targets an encoded metrics payload is addressed to.
///
/// Encoders attach this to the payloads they emit, and the forwarder delivers a payload carrying it only to the targets
/// it names. It is kept in memory only: a payload that is persisted for retry already belongs to one target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricsRoutingTargets {
    targets: RoutingTargetSet,
}

impl MetricsRoutingTargets {
    /// Creates routing metadata addressing the given targets.
    pub const fn new(targets: RoutingTargetSet) -> Self {
        Self { targets }
    }

    /// Returns the addressed targets.
    pub const fn targets(&self) -> &RoutingTargetSet {
        &self.targets
    }

    /// Returns `true` if the payload is addressed to the given target.
    pub(crate) fn includes(&self, id: RoutingTargetId) -> bool {
        self.targets.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn additional(entries: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(url, keys)| (url.to_string(), keys.iter().map(|key| key.to_string()).collect()))
            .collect()
    }

    #[test]
    fn catalog_orders_primary_first_then_additional_targets_deterministically() {
        let catalog = RoutingTargetCatalog::new(
            "https://primary.example.com",
            &additional(&[
                ("https://b.example.com", &["key-1", "", "key-1", "key-2"]),
                ("https://a.example.com", &["key-3"]),
            ]),
        )
        .expect("catalog should build");

        let targets = catalog
            .targets()
            .iter()
            .map(|target| (target.kind(), target.configured_endpoint(), target.api_key_index))
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                (RoutingTargetKind::Primary, "https://primary.example.com", None),
                (RoutingTargetKind::Additional, "https://a.example.com", Some(0)),
                (RoutingTargetKind::Additional, "https://b.example.com", Some(0)),
                (RoutingTargetKind::Additional, "https://b.example.com", Some(3)),
            ]
        );
        assert_eq!(catalog.primary(), RoutingTargetId::from_index(0));
        assert_eq!(
            catalog.find_additional("https://b.example.com", 3),
            Some(RoutingTargetId::from_index(3))
        );
        assert_eq!(catalog.find_additional("https://b.example.com", 1), None);
    }

    #[test]
    fn catalog_rejects_an_invalid_additional_endpoint() {
        let error = RoutingTargetCatalog::new("https://primary.example.com", &additional(&[("http://[::1", &["key"])]))
            .expect_err("an invalid URL should be rejected");
        assert!(error.to_string().contains("additional destination endpoints"));
    }

    #[test]
    fn select_returns_matching_targets() {
        let catalog = RoutingTargetCatalog::new(
            "https://primary.example.com",
            &additional(&[("https://a.example.com", &["key-1", "key-2"])]),
        )
        .unwrap();

        let selected = catalog.select(|target| target.configured_endpoint() == "https://a.example.com");
        assert_eq!(
            selected.iter().collect::<Vec<_>>(),
            [RoutingTargetId::from_index(1), RoutingTargetId::from_index(2)]
        );
        assert!(catalog.select(|_| false).is_empty());
    }

    #[test]
    fn target_sets_compare_equal_regardless_of_growth() {
        let mut grown = RoutingTargetSet::new();
        grown.union_with(&[RoutingTargetId::from_index(100)].into_iter().collect());
        let mut direct = RoutingTargetSet::new();
        direct.insert(RoutingTargetId::from_index(100));
        assert_eq!(grown, direct);
        assert_ne!(direct, RoutingTargetSet::new());
    }

    proptest! {
        #[test]
        fn property_test_target_set_matches_a_btreeset(
            left in proptest::collection::btree_set(0usize..200, 0..20),
            right in proptest::collection::btree_set(0usize..200, 0..20),
        ) {
            let to_set = |ids: &std::collections::BTreeSet<usize>| {
                ids.iter().copied().map(RoutingTargetId::from_index).collect::<RoutingTargetSet>()
            };
            let mut union = to_set(&left);
            union.union_with(&to_set(&right));

            let expected = left.union(&right).copied().collect::<Vec<_>>();
            prop_assert_eq!(union.iter().map(RoutingTargetId::index).collect::<Vec<_>>(), expected.clone());
            prop_assert_eq!(union.len(), expected.len());
            prop_assert_eq!(union.is_empty(), expected.is_empty());
            for id in 0..200 {
                prop_assert_eq!(union.contains(RoutingTargetId::from_index(id)), expected.contains(&id));
            }
        }
    }
}
