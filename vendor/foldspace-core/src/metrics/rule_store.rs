//! Metric rule state as of the last seal.
//!
//! [`MetricRuleStore`] holds every dictionary definition in force, keyed by
//! [`DefinitionKey`]. It has exactly one writer: [`MetricRuleStore::seal`]
//! applies the state changes the core makes — the definitions an encoding
//! introduced and the evictions that followed it — so the store reflects
//! exactly the definitions the shared dictionary holds.
//!
//! Endpoints never own dictionary state. Each endpoint stream tracks only the
//! keys it has sent, and [`MetricRuleStore::definitions_for`] answers "what
//! does this payload reference that this stream has not seen yet" from the
//! store.
//!
//! No per-entry order stamp is kept. [`DefinitionKey`] orders primitives before
//! composites, and ids within a kind are allocated monotonically and never
//! reused, so a prefix tagset sorts before every tagset built on it. Iterating
//! keys in key order is therefore already a dependency order.

use std::collections::{BTreeMap, BTreeSet};

use crate::proto::stateful::MetricDatum;

use super::retention::DefinitionKey;

/// One change the core makes to metric rule state.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum MetricStateChange {
    /// A definition an encoding introduced.
    Define(MetricDatum),
    /// A definition dropped by local eviction. Dictionary ids are never reused,
    /// so an evicted definition stays inert on any server that has it.
    Evict(DefinitionKey),
}

/// The metric definitions in force, keyed by definition.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct MetricRuleStore {
    entries: BTreeMap<DefinitionKey, MetricDatum>,
}

impl MetricRuleStore {
    /// Applies state changes in order.
    ///
    /// This is the store's only writer. A define inserts or replaces its key; an
    /// eviction removes it.
    pub(super) fn seal(&mut self, changes: &[MetricStateChange]) {
        for change in changes {
            match change {
                MetricStateChange::Define(datum) => {
                    self.entries.insert(DefinitionKey::of(datum), datum.clone());
                }
                MetricStateChange::Evict(key) => {
                    self.entries.remove(key);
                }
            }
        }
    }

    /// The definitions in `references` that `sent` lacks, in dependency order.
    ///
    /// `references` must be in key order and live in the store, which holds for
    /// the reference closure of the batch most recently encoded: eviction cannot
    /// remove it until the next encoding begins.
    pub(super) fn definitions_for<'a>(
        &'a self,
        references: &'a [DefinitionKey],
        sent: &'a BTreeSet<DefinitionKey>,
    ) -> impl Iterator<Item = &'a MetricDatum> + 'a {
        debug_assert!(references.is_sorted(), "references are in dependency order");
        references
            .iter()
            .filter(|key| !sent.contains(key))
            .map(|key| {
                self.entries
                    .get(key)
                    .expect("a referenced definition is sealed before it is sent")
            })
    }

    #[cfg(test)]
    pub(super) fn contains(&self, key: DefinitionKey) -> bool {
        self.entries.contains_key(&key)
    }

    #[cfg(test)]
    pub(super) fn get(&self, key: DefinitionKey) -> Option<&MetricDatum> {
        self.entries.get(&key)
    }

    #[cfg(test)]
    pub(super) fn keys(&self) -> impl Iterator<Item = DefinitionKey> + '_ {
        self.entries.keys().copied()
    }

    /// Every live definition, in dependency order: replaying them onto an empty
    /// dictionary defines each id before anything references it.
    #[cfg(test)]
    pub(super) fn definitions(&self) -> impl Iterator<Item = &MetricDatum> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{
        LogicalMetricBatch, LogicalMetricSeries, MetricDictionaryEvictionConfig, MetricPoint,
        MetricSeriesEncoder, MetricSeriesType, MetricTagSet,
    };

    fn series(name: &str, prefix: &[&str], values: &[&str]) -> LogicalMetricBatch {
        LogicalMetricBatch::new(vec![LogicalMetricSeries::new(
            name,
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, 1.0)],
        )
        .with_tags(MetricTagSet {
            prefix: prefix.iter().map(ToString::to_string).collect(),
            values: values.iter().map(ToString::to_string).collect(),
        })])
    }

    fn defines(definitions: &[MetricDatum]) -> Vec<MetricStateChange> {
        definitions
            .iter()
            .cloned()
            .map(MetricStateChange::Define)
            .collect()
    }

    fn evict_everything() -> MetricDictionaryEvictionConfig {
        MetricDictionaryEvictionConfig {
            max_item_count: 0,
            high_watermark: 1.0,
            low_watermark: 0.0,
            grace_period: Duration::ZERO,
            stale_after: Duration::ZERO,
            ..MetricDictionaryEvictionConfig::default()
        }
    }

    /// A seal applies its changes in order, so a define followed by an eviction
    /// of the same key leaves nothing behind.
    #[test]
    fn seal_applies_defines_and_evictions_in_order() {
        let mut encoder = MetricSeriesEncoder::default();
        let encoded = encoder.encode(&series("requests", &[], &[]), Instant::now());
        let define = encoded.definitions()[0].clone();
        let key = DefinitionKey::of(&define);

        let mut store = MetricRuleStore::default();
        store.seal(&[MetricStateChange::Define(define.clone())]);
        assert_eq!(store.get(key), Some(&define));

        store.seal(&[MetricStateChange::Evict(key)]);
        assert!(!store.contains(key));

        store.seal(&[
            MetricStateChange::Define(define),
            MetricStateChange::Evict(key),
        ]);
        assert!(!store.contains(key));
    }

    /// A stream that has seen part of a payload's closure gets only the rest,
    /// still in dependency order.
    #[test]
    fn definitions_for_skips_what_the_stream_has_sent() {
        let now = Instant::now();
        let mut encoder = MetricSeriesEncoder::default();
        let mut store = MetricRuleStore::default();
        let first = encoder.encode(&series("requests", &["env:prod"], &[]), now);
        store.seal(&defines(first.definitions()));
        let sent: BTreeSet<_> = first.references().iter().copied().collect();

        let second = encoder.encode(&series("requests", &["env:prod"], &["team:a"]), now);
        store.seal(&defines(second.definitions()));

        let needed: Vec<_> = store
            .definitions_for(second.references(), &sent)
            .map(DefinitionKey::of)
            .collect();
        let mut introduced: Vec<_> = second.definitions().iter().map(DefinitionKey::of).collect();
        introduced.sort();
        assert_eq!(needed, introduced);

        let fresh: Vec<_> = store
            .definitions_for(second.references(), &BTreeSet::new())
            .map(DefinitionKey::of)
            .collect();
        assert_eq!(fresh, second.references());
    }

    /// Key order is a dependency order, including after a prefix tagset is
    /// evicted and its value returns under a fresh id: replaying the store onto
    /// an empty encoder never meets a reference to an undefined id, and the
    /// replayed encoder needs no new definitions for the same series.
    #[test]
    fn key_order_replays_as_a_dependency_order_after_eviction() {
        let now = Instant::now();
        let mut encoder = MetricSeriesEncoder::default();
        let mut store = MetricRuleStore::default();

        let first = series("requests", &["env:prod"], &["service:api"]);
        let encoded = encoder.encode(&first, now);
        store.seal(&defines(encoded.definitions()));

        // Drop everything the next payload does not reference, then bring the
        // prefix back so it is re-created under a higher id than before.
        let other = series("latency", &[], &[]);
        let encoded = encoder.encode(&other, now);
        store.seal(&defines(encoded.definitions()));
        let evicted = encoder.evict(&evict_everything());
        assert!(evicted.contains(&DefinitionKey::Tagset(1)));
        store.seal(
            &evicted
                .into_iter()
                .map(MetricStateChange::Evict)
                .collect::<Vec<_>>(),
        );

        let returning = series("requests", &["env:prod"], &["service:web"]);
        let encoded = encoder.encode(&returning, now);
        store.seal(&defines(encoded.definitions()));
        assert!(!store.contains(DefinitionKey::Tagset(1)));

        let definitions: Vec<_> = store.definitions().cloned().collect();
        let mut replayed = MetricSeriesEncoder::default();
        replayed.apply_definitions(&definitions, now);
        assert!(replayed.encode(&returning, now).definitions().is_empty());
        assert!(replayed.encode(&other, now).definitions().is_empty());
    }
}
