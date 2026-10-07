//! Local definition retention. Composite definitions keep their dependencies alive.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    mem::size_of,
    time::Instant,
};

use crate::proto::stateful::{metric_datum::Data, MetricDatum};

use super::eviction_policy::{score_with_grace, EvictionConfig, Strategy};

// Estimated tree/hash bucket and lookup storage per definition, excluding owned strings.
const ENTRY_OVERHEAD: usize = 128;

/// Live client dictionary usage, shared by every endpoint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricDictionaryStats {
    /// Definitions across all eight metrics dictionary kinds.
    pub entries: usize,
    /// Estimate of retained wire definitions, lookup keys, dependency lists, and entry
    /// overhead. Excludes allocator slack, buffered metrics, and inflight logical batches.
    pub estimated_bytes: usize,
}

// Ordering puts primitive definitions before composites and prefix tagsets before children.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum DefinitionKey {
    Name(u64),
    TagString(u64),
    SourceType(u64),
    Unit(u64),
    ResourceString(u64),
    Origin(u64),
    Resource(u64),
    Tagset(u64),
}

impl DefinitionKey {
    pub(super) fn of(datum: &MetricDatum) -> Self {
        match datum
            .data
            .as_ref()
            .expect("dictionary datum has a definition")
        {
            Data::MetricNameDefine(d) => Self::Name(d.id),
            Data::MetricTagStringDefine(d) => Self::TagString(d.id),
            Data::MetricSourceTypeNameDefine(d) => Self::SourceType(d.id),
            Data::MetricUnitDefine(d) => Self::Unit(d.id),
            Data::MetricResourceStringDefine(d) => Self::ResourceString(d.id),
            Data::MetricOriginDefine(d) => Self::Origin(d.id),
            Data::MetricResourceDefine(d) => Self::Resource(d.id),
            Data::MetricTagsetDefine(d) => Self::Tagset(d.id),
            Data::MetricSeriesBatch(_) => unreachable!("series data is not dictionary state"),
        }
    }
}

#[derive(Clone, Debug)]
struct Definition {
    datum: MetricDatum,
    dependencies: Vec<DefinitionKey>,
    created_at: Instant,
    last_access_at: Instant,
    hits: u64,
    bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Retention {
    entries: BTreeMap<DefinitionKey, Definition>,
    bytes: usize,
    now: Option<Instant>,
    protected: BTreeSet<DefinitionKey>,
    next_stale_sweep: Option<Instant>,
}

impl Retention {
    pub(super) fn begin_batch(&mut self, now: Instant) {
        self.now = Some(self.now.map_or(now, |previous| previous.max(now)));
        self.protected.clear();
    }

    pub(super) fn insert(&mut self, datum: &MetricDatum, lookup_bytes: usize) {
        let key = DefinitionKey::of(datum);
        if self.entries.contains_key(&key) {
            return;
        }
        let mut dependencies = match datum.data.as_ref().unwrap() {
            Data::MetricTagsetDefine(d) => {
                let mut keys: Vec<_> = d
                    .tag_string_ids
                    .iter()
                    .copied()
                    .map(DefinitionKey::TagString)
                    .collect();
                if d.prefix_id != 0 {
                    keys.push(DefinitionKey::Tagset(d.prefix_id));
                }
                keys
            }
            Data::MetricResourceDefine(d) => d
                .type_string_ids
                .iter()
                .chain(&d.name_string_ids)
                .copied()
                .map(DefinitionKey::ResourceString)
                .collect(),
            _ => Vec::new(),
        };
        dependencies.sort_unstable();
        dependencies.dedup();
        let bytes = ENTRY_OVERHEAD
            + size_of::<Definition>()
            + datum_heap_bytes(datum)
            + lookup_bytes
            + dependencies.capacity() * size_of::<DefinitionKey>();
        let now = self
            .now
            .expect("encoding supplies the clock before interning");
        self.entries.insert(
            key,
            Definition {
                datum: datum.clone(),
                dependencies,
                created_at: now,
                last_access_at: now,
                hits: 0,
                bytes,
            },
        );
        self.bytes += bytes;
    }

    /// Cached composites must count hits on their strings and prefixes as well.
    pub(super) fn touch(&mut self, roots: impl IntoIterator<Item = DefinitionKey>) {
        let mut pending: Vec<_> = roots.into_iter().collect();
        let mut seen = BTreeSet::new();
        while let Some(key) = pending.pop() {
            if !seen.insert(key) {
                continue;
            }
            let entry = self
                .entries
                .get_mut(&key)
                .expect("a reference has a retained definition");
            entry.hits = entry.hits.saturating_add(1);
            entry.last_access_at = self.now.unwrap();
            self.protected.insert(key);
            pending.extend(&entry.dependencies);
        }
    }

    /// Definitions referenced since the last `begin_batch`, closed over dependencies, in
    /// dependency order. Eviction cannot remove them until the next batch begins.
    pub(super) fn references(&self) -> impl Iterator<Item = DefinitionKey> + '_ {
        self.protected.iter().copied()
    }

    pub(super) fn contains(&self, key: DefinitionKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// The wire definition retained under `key`.
    pub(super) fn definition(&self, key: DefinitionKey) -> Option<&MetricDatum> {
        self.entries.get(&key).map(|entry| &entry.datum)
    }

    #[cfg(test)]
    pub(super) fn keys(&self) -> impl Iterator<Item = DefinitionKey> + '_ {
        self.entries.keys().copied()
    }

    pub(super) fn stats(&self) -> MetricDictionaryStats {
        MetricDictionaryStats {
            entries: self.entries.len(),
            estimated_bytes: self.bytes,
        }
    }

    /// Evicts by the policy and returns the removed keys, dependents before dependencies.
    pub(super) fn evict(&mut self, config: &EvictionConfig) -> Vec<DefinitionKey> {
        let mut evicted = Vec::new();
        let now = self.now.expect("maintenance supplies the clock");
        let sweep =
            !config.stale_after.is_zero() && self.next_stale_sweep.is_none_or(|due| now >= due);
        if sweep {
            self.next_stale_sweep = now.checked_add(config.stale_after);
        }
        let (count_over, bytes_over) = config.should_evict(self.entries.len(), self.bytes as i64);
        if !sweep && !count_over && !bytes_over {
            return evicted;
        }
        if sweep {
            self.remove_candidates(config, now, true, 0, 0, &mut evicted);
        }
        let (count_over, bytes_over) = config.should_evict(self.entries.len(), self.bytes as i64);
        let (count, bytes, strategy) = config.eviction_targets(
            self.entries.len(),
            self.bytes as i64,
            count_over,
            bytes_over,
        );
        if strategy != Strategy::None {
            self.remove_candidates(config, now, false, count, bytes as usize, &mut evicted);
        }
        evicted
    }

    fn remove_candidates(
        &mut self,
        config: &EvictionConfig,
        now: Instant,
        stale: bool,
        count: usize,
        bytes: usize,
        evicted: &mut Vec<DefinitionKey>,
    ) {
        // Kahn's topological sort removes dependents before dependencies, prioritizing by eviction score.
        let mut dependents: BTreeMap<_, usize> = self.entries.keys().map(|key| (*key, 0)).collect();
        for entry in self.entries.values() {
            for dependency in &entry.dependencies {
                *dependents
                    .get_mut(dependency)
                    .expect("composite dependencies are retained") += 1;
            }
        }
        let eligible = |key: &DefinitionKey, entry: &Definition| {
            !self.protected.contains(key)
                && now >= config.eligible_at(entry.created_at)
                && (!stale || config.is_stale(entry.created_at, entry.last_access_at, now))
        };
        let mut candidates = BinaryHeap::new();
        for (key, entry) in &self.entries {
            if dependents[key] == 0 && eligible(key, entry) {
                candidates.push(Candidate::new(*key, entry, now, config));
            }
        }
        let mut removed = 0;
        let mut freed = 0;
        while let Some(candidate) = candidates.pop() {
            if !stale && removed >= count && freed >= bytes {
                break;
            }
            let entry = self.entries.remove(&candidate.key).unwrap();
            evicted.push(candidate.key);
            removed += 1;
            freed += entry.bytes;
            self.bytes -= entry.bytes;
            for key in entry.dependencies {
                let remaining = dependents.get_mut(&key).unwrap();
                *remaining -= 1;
                let dependency = &self.entries[&key];
                if *remaining == 0 && eligible(&key, dependency) {
                    candidates.push(Candidate::new(key, dependency, now, config));
                }
            }
        }
    }
}

fn datum_heap_bytes(datum: &MetricDatum) -> usize {
    match datum.data.as_ref().unwrap() {
        Data::MetricNameDefine(d) => d.value.len(),
        Data::MetricTagStringDefine(d) => d.value.len(),
        Data::MetricSourceTypeNameDefine(d) => d.value.len(),
        Data::MetricUnitDefine(d) => d.value.len(),
        Data::MetricResourceStringDefine(d) => d.value.len(),
        Data::MetricOriginDefine(_) => 0,
        Data::MetricTagsetDefine(d) => d.tag_string_ids.len() * size_of::<u64>(),
        Data::MetricResourceDefine(d) => {
            (d.type_string_ids.len() + d.name_string_ids.len()) * size_of::<u64>()
        }
        Data::MetricSeriesBatch(_) => unreachable!("series data is not dictionary state"),
    }
}

#[derive(Debug)]
struct Candidate {
    key: DefinitionKey,
    score: f64,
}

impl Candidate {
    fn new(key: DefinitionKey, entry: &Definition, now: Instant, config: &EvictionConfig) -> Self {
        Self {
            key,
            score: score_with_grace(
                entry.hits as f64,
                entry.created_at,
                entry.last_access_at,
                now,
                config,
            ),
        }
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| other.key.cmp(&self.key))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::proto::stateful::{MetricNameDefine, MetricTagStringDefine, MetricTagsetDefine};

    use super::*;

    fn policy() -> EvictionConfig {
        EvictionConfig {
            max_item_count: 4,
            high_watermark: 1.0,
            low_watermark: 0.5,
            grace_period: Duration::ZERO,
            stale_after: Duration::ZERO,
            ..EvictionConfig::default()
        }
    }

    fn add_name(retention: &mut Retention, id: u64) {
        retention.insert(
            &MetricDatum {
                data: Some(Data::MetricNameDefine(MetricNameDefine {
                    id,
                    value: format!("metric-{id}"),
                })),
            },
            8,
        );
    }

    #[test]
    fn count_pressure_preserves_hot_and_current_entries() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now);
        for id in 1..=5 {
            add_name(&mut retention, id);
        }
        for _ in 0..10 {
            retention.touch([DefinitionKey::Name(1)]);
        }
        retention.begin_batch(now + Duration::from_secs(1));
        retention.touch([DefinitionKey::Name(5)]);
        retention.evict(&policy());
        assert_eq!(retention.stats().entries, 2);
        assert!(retention.contains(DefinitionKey::Name(1)));
        assert!(retention.contains(DefinitionKey::Name(5)));
    }

    #[test]
    fn stats_count_the_retained_wire_definition() {
        let now = Instant::now();
        let name = |value: &str| MetricDatum {
            data: Some(Data::MetricNameDefine(MetricNameDefine {
                id: 1,
                value: value.to_string(),
            })),
        };
        let mut short = Retention::default();
        short.begin_batch(now);
        short.insert(&name("a"), 0);
        let mut long = Retention::default();
        long.begin_batch(now);
        long.insert(&name(&"a".repeat(1001)), 0);

        assert_eq!(
            long.stats().estimated_bytes - short.stats().estimated_bytes,
            1000,
            "the retained datum's value is charged even without lookup bytes"
        );
        assert_eq!(
            long.definition(DefinitionKey::Name(1)),
            Some(&name(&"a".repeat(1001)))
        );
        long.evict(&EvictionConfig {
            max_item_count: 0,
            ..policy()
        });
        assert_eq!(long.definition(DefinitionKey::Name(1)), None);
        assert_eq!(long.stats(), MetricDictionaryStats::default());
    }

    #[test]
    fn grace_period_definitions_survive_pressure() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now);
        for id in 1..=5 {
            add_name(&mut retention, id);
        }
        let config = EvictionConfig {
            grace_period: Duration::from_secs(30),
            ..policy()
        };
        retention.begin_batch(now + Duration::from_secs(29));
        retention.evict(&config);
        assert_eq!(retention.stats().entries, 5);
        retention.begin_batch(now + Duration::from_secs(30));
        retention.touch([DefinitionKey::Name(5)]);
        retention.evict(&config);
        assert_eq!(retention.stats().entries, 2);
        assert!(retention.contains(DefinitionKey::Name(5)));
    }

    #[test]
    fn byte_pressure_removes_entries_and_keeps_accounting_consistent() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now);
        for id in 1..=5 {
            add_name(&mut retention, id);
        }
        let budget = retention.bytes / 2;
        retention.evict(&EvictionConfig {
            max_item_count: 1000,
            max_memory_bytes: budget as i64,
            ..policy()
        });
        assert!(retention.bytes <= budget / 2);
        assert_eq!(
            retention.bytes,
            retention
                .entries
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>()
        );
    }

    #[test]
    fn stale_sweep_is_independent_of_pressure_and_rate_limited() {
        let now = Instant::now();
        let mut retention = Retention::default();
        let config = EvictionConfig {
            max_item_count: 1000,
            stale_after: Duration::from_secs(60),
            ..policy()
        };
        retention.begin_batch(now);
        add_name(&mut retention, 1);
        retention.evict(&config);
        retention.begin_batch(now + Duration::from_secs(30));
        retention.touch([DefinitionKey::Name(1)]);
        retention.begin_batch(now + Duration::from_secs(60));
        retention.evict(&config);
        assert_eq!(retention.stats().entries, 1);
        retention.begin_batch(now + Duration::from_secs(91));
        retention.evict(&config);
        assert_eq!(retention.stats().entries, 1);
        retention.begin_batch(now + Duration::from_secs(120));
        retention.evict(&config);
        assert_eq!(retention.stats(), MetricDictionaryStats::default());
    }

    #[test]
    fn batch_references_close_over_dependencies_and_pin_them() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now);
        let tag = MetricDatum {
            data: Some(Data::MetricTagStringDefine(MetricTagStringDefine {
                id: 1,
                value: "env:test".into(),
            })),
        };
        let prefix = MetricDatum {
            data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                id: 1,
                prefix_id: 0,
                tag_string_ids: vec![1],
            })),
        };
        let child = MetricDatum {
            data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                id: 2,
                prefix_id: 1,
                tag_string_ids: vec![1],
            })),
        };
        for datum in [&tag, &prefix, &child] {
            retention.insert(datum, 0);
        }
        retention.touch([DefinitionKey::of(&child)]);
        for entry in retention.entries.values() {
            assert_eq!(entry.hits, 1);
        }
        assert_eq!(
            retention.references().collect::<Vec<_>>(),
            [&tag, &prefix, &child].map(DefinitionKey::of)
        );
        let config = EvictionConfig {
            max_item_count: 0,
            ..policy()
        };
        retention.evict(&config);
        assert_eq!(
            retention.stats().entries,
            3,
            "the current batch pins the full dependency chain"
        );
        retention.begin_batch(now + Duration::from_secs(1));
        assert_eq!(retention.references().count(), 0);
        retention.evict(&config);
        assert_eq!(retention.stats(), MetricDictionaryStats::default());
    }

    #[test]
    fn clock_regression_does_not_move_access_times_backwards() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now + Duration::from_secs(10));
        add_name(&mut retention, 1);
        retention.begin_batch(now);
        retention.touch([DefinitionKey::Name(1)]);
        assert_eq!(
            retention.entries[&DefinitionKey::Name(1)].last_access_at,
            now + Duration::from_secs(10)
        );
    }
}
