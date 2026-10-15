//! Local definition retention. Composite definitions keep their dependencies alive.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    mem::size_of,
    time::{Duration, Instant},
};

use crate::proto::stateful::{metric_datum::Data, MetricDatum};

use super::{
    eviction_policy::{score_with_grace, EvictionConfig, Strategy},
    frequency::FrequencySketch,
    key_hash::KeyBuildHasher,
};

// Estimated tree/hash bucket and lookup storage per definition, excluding owned strings.
const ENTRY_OVERHEAD: usize = 128;
// A value counted in at least one batch since the sketch last halved returns as established.
const RETURNING_ESTIMATE: u64 = 1;

/// Live client dictionary usage, shared by every endpoint.
///
/// Once eviction has had to relieve pressure, a value referenced no more often than what it
/// would displace is defined only for the payload that needs it: such transient definitions
/// are reported apart, are not counted against the eviction limits, and are dropped by the
/// first eviction pass after no payload in progress references them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricDictionaryStats {
    /// Retained definitions across all eight metrics dictionary kinds; the count eviction
    /// holds under `max_item_count`.
    pub entries: usize,
    /// Estimate of retained wire definitions, lookup keys, dependency lists, and entry overhead.
    /// This is the figure eviction holds under `max_memory_bytes`. Excludes allocator slack,
    /// buffered metrics, inflight logical batches, and eviction bookkeeping (incoming counts,
    /// protection epochs, frontier and grace indexes, and each entry's 8-byte content hash).
    pub estimated_bytes: usize,
    /// Transient definitions, outside `entries`.
    pub transient_entries: usize,
    /// The same estimate as `estimated_bytes` for transient definitions, outside it.
    pub transient_bytes: usize,
    /// Fixed size of the value frequency sketch, sized from the eviction limits when the
    /// dictionary is created; zero when eviction is disabled. Not part of `estimated_bytes`.
    pub frequency_sketch_bytes: usize,
}

// Ordering puts primitive definitions before composites and prefix tagsets before children.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

// Keep the policy's existing byte estimate independent of eviction bookkeeping.
#[derive(Clone, Debug)]
struct Entry {
    definition: Definition,
    incoming: usize,
    protected_epoch: u64,
    frontier_index: Option<usize>,
    // Zero without a frequency sketch.
    content_hash: u64,
    // Seeded from the sketch on insert, so it is scored at once instead of after a grace period.
    returning: bool,
    // Defined for its batch but not admitted: outside the budget, frontier and grace index.
    transient: bool,
    // The last `touch` call that counted this entry, so one call counts it once.
    touched: u64,
}

// What a value must beat to be retained, from the latest pass that relieved pressure.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Admission {
    // Content hash of the entry that pass would have evicted next, if any was eligible.
    victim: Option<u64>,
    // High watermarks: below both, everything is admitted.
    count_limit: usize,
    bytes_limit: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Retention {
    // Nothing depends on iteration order: eviction ranks candidates by score, then key.
    entries: HashMap<DefinitionKey, Entry, KeyBuildHasher>,
    touches: u64,
    pending: Vec<DefinitionKey>,
    // Retained entries only; transient ones are counted apart.
    bytes: usize,
    transient_bytes: usize,
    transient_len: usize,
    // Every transient key, and keys promoted since they were added.
    transient: Vec<DefinitionKey>,
    admission: Option<Admission>,
    now: Option<Instant>,
    protected: Vec<DefinitionKey>,
    epoch: u64,
    // Mature leaves, including protected entries. Epochs release protection without a scan.
    frontier: Vec<DefinitionKey>,
    protected_frontier: usize,
    // Exactly one deadline per immature entry; removed on maturity or policy rebuild.
    grace: BTreeSet<(Instant, DefinitionKey)>,
    indexed_grace_period: Option<Duration>,
    next_stale_sweep: Option<Instant>,
    frequency: Option<FrequencySketch>,
    #[cfg(test)]
    candidate_passes: usize,
    #[cfg(test)]
    admit_all: bool,
}

impl Retention {
    /// Retention that remembers how often values are referenced across eviction, sized for
    /// the most entries `config` can hold.
    pub(super) fn with_frequency_sketch(config: &EvictionConfig) -> Self {
        let byte_bound = usize::try_from(config.max_memory_bytes).unwrap_or(0) / ENTRY_OVERHEAD;
        Self {
            frequency: Some(FrequencySketch::with_capacity(
                config.max_item_count.min(byte_bound),
            )),
            ..Self::default()
        }
    }

    pub(super) fn begin_batch(&mut self, now: Instant) {
        self.now = Some(self.now.map_or(now, |previous| previous.max(now)));
        self.protected.clear();
        self.protected_frontier = 0;
        self.epoch = self.epoch.checked_add(1).unwrap_or_else(|| {
            for entry in self.entries.values_mut() {
                entry.protected_epoch = 0;
            }
            1
        });
    }

    /// Defines a value being interned. Once the dictionary is full it is retained only if it
    /// is referenced more often than what it would displace, and is otherwise transient.
    pub(super) fn insert(&mut self, datum: &MetricDatum, lookup_bytes: usize) {
        self.add(datum, lookup_bytes, true);
    }

    /// Defines a value a receiver already holds, retaining it whatever its frequency.
    pub(super) fn seed(&mut self, datum: &MetricDatum, lookup_bytes: usize) {
        self.add(datum, lookup_bytes, false);
    }

    fn add(&mut self, datum: &MetricDatum, lookup_bytes: usize, contended: bool) {
        let key = DefinitionKey::of(datum);
        if self.entries.contains_key(&key) {
            return;
        }
        let mut dependencies: Vec<_> = match datum.data.as_ref().unwrap() {
            Data::MetricTagsetDefine(d) => d
                .tag_string_ids
                .iter()
                .copied()
                .map(DefinitionKey::TagString)
                .chain((d.prefix_id != 0).then_some(DefinitionKey::Tagset(d.prefix_id)))
                .collect(),
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
        dependencies.shrink_to_fit();
        let bytes = ENTRY_OVERHEAD
            + size_of::<Definition>()
            + datum_heap_bytes(datum)
            + lookup_bytes
            + dependencies.capacity() * size_of::<DefinitionKey>();
        let now = self
            .now
            .expect("encoding supplies the clock before interning");
        let (content_hash, estimate) = match &self.frequency {
            Some(sketch) => {
                let hash = self.content_hash(datum);
                (hash, sketch.estimate(hash))
            }
            None => (0, 0),
        };
        let returning = estimate >= RETURNING_ESTIMATE;
        let transient = contended && !self.admits(estimate);
        for dependency in &dependencies {
            self.remove_from_frontier(*dependency);
            self.entries
                .get_mut(dependency)
                .expect("composite dependencies are defined")
                .incoming += 1;
        }
        if !transient && !self.transient.is_empty() {
            for dependency in &dependencies {
                self.promote(*dependency);
            }
        }
        self.entries.insert(
            key,
            Entry {
                definition: Definition {
                    datum: datum.clone(),
                    dependencies,
                    created_at: now,
                    last_access_at: now,
                    // Half, so a returning value outranks colder entries but not incumbents of
                    // equal frequency: a full seed displaces those, and a hot set above the
                    // cap then cycles through the dictionary like LRU.
                    hits: if returning { estimate / 2 } else { 0 },
                    bytes,
                },
                incoming: 0,
                protected_epoch: 0,
                frontier_index: None,
                content_hash,
                returning,
                transient,
                touched: 0,
            },
        );
        if transient {
            self.transient_bytes += bytes;
            self.transient_len += 1;
            self.transient.push(key);
            // Protected from the start, so its first touch in this batch is not taken for a
            // transient value recurring in a later batch.
            self.protect(key);
        } else {
            self.bytes += bytes;
            if let Some(period) = self.indexed_grace_period {
                self.index_grace(key, now, period);
            }
        }
    }

    /// Whether a value the sketch estimates at `estimate` is retained rather than transient.
    /// Once a pass has relieved pressure, a dictionary at its high watermark retains only
    /// values referenced more often than the entry that pass would have evicted next. Both
    /// sides are read from the sketch now, so halving ages them alike; ties keep the
    /// incumbent, so values of equal frequency do not displace one another.
    fn admits(&self, estimate: u64) -> bool {
        #[cfg(test)]
        if self.admit_all {
            return true;
        }
        let (Some(sketch), Some(admission)) = (&self.frequency, &self.admission) else {
            return true;
        };
        if self.retained_len() < admission.count_limit && self.bytes < admission.bytes_limit {
            return true;
        }
        estimate > admission.victim.map_or(0, |victim| sketch.estimate(victim))
    }

    fn retained_len(&self) -> usize {
        self.entries.len() - self.transient_len
    }

    /// Pins `key` until the next batch begins and counts its value once for this batch.
    fn protect(&mut self, key: DefinitionKey) {
        let entry = self.entries.get_mut(&key).unwrap();
        entry.protected_epoch = self.epoch;
        self.protected.push(key);
        self.protected_frontier += usize::from(entry.frontier_index.is_some());
        if let Some(sketch) = &mut self.frequency {
            sketch.increment(entry.content_hash);
        }
    }

    /// Retains a transient entry and its transient dependencies.
    fn promote(&mut self, key: DefinitionKey) {
        let entry = self.entries.get_mut(&key).unwrap();
        if !entry.transient {
            return;
        }
        entry.transient = false;
        let (bytes, dependencies) = (entry.definition.bytes, entry.definition.dependencies.len());
        self.transient_bytes -= bytes;
        self.transient_len -= 1;
        self.bytes += bytes;
        for index in 0..dependencies {
            self.promote(self.entries[&key].definition.dependencies[index]);
        }
        if let Some(period) = self.indexed_grace_period {
            self.index_grace(key, self.now.unwrap(), period);
        }
    }

    /// Cached composites must count hits on their strings and prefixes as well.
    pub(super) fn touch(&mut self, roots: impl IntoIterator<Item = DefinitionKey>) {
        self.touches += 1;
        let mut pending = std::mem::take(&mut self.pending);
        pending.clear();
        pending.extend(roots);
        while let Some(key) = pending.pop() {
            let entry = self
                .entries
                .get_mut(&key)
                .expect("a reference has a defined value");
            if entry.touched == self.touches {
                continue;
            }
            entry.touched = self.touches;
            entry.definition.hits = entry.definition.hits.saturating_add(1);
            entry.definition.last_access_at = self.now.unwrap();
            pending.extend(&entry.definition.dependencies);
            if entry.protected_epoch == self.epoch {
                continue;
            }
            if entry.transient {
                // A transient value recurring before its removal competes for admission again.
                let hash = entry.content_hash;
                if self.admits(self.frequency.as_ref().map_or(0, |s| s.estimate(hash))) {
                    self.entries.get_mut(&key).unwrap().returning = true;
                    self.promote(key);
                }
                self.protect(key);
                continue;
            }
            entry.protected_epoch = self.epoch;
            self.protected.push(key);
            self.protected_frontier += usize::from(entry.frontier_index.is_some());
            if let Some(sketch) = &mut self.frequency {
                sketch.increment(entry.content_hash);
            }
        }
        self.pending = pending;
    }

    /// Definitions referenced since the last `begin_batch`, closed over dependencies, in
    /// dependency order. Eviction cannot remove them until the next batch begins.
    pub(super) fn references(&mut self) -> impl Iterator<Item = DefinitionKey> + '_ {
        self.protected.sort_unstable();
        self.protected.iter().copied()
    }

    #[cfg(test)]
    pub(super) fn contains(&self, key: DefinitionKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// The wire definition retained under `key`.
    pub(super) fn definition(&self, key: DefinitionKey) -> Option<&MetricDatum> {
        self.entries.get(&key).map(|entry| &entry.definition.datum)
    }

    #[cfg(test)]
    pub(super) fn keys(&self) -> impl Iterator<Item = DefinitionKey> + '_ {
        let mut keys: Vec<_> = self.entries.keys().copied().collect();
        keys.sort_unstable();
        keys.into_iter()
    }

    /// The content hash and whether the entry returned seeded from the sketch.
    #[cfg(test)]
    pub(super) fn frequency_state(&self, key: DefinitionKey) -> Option<(u64, bool)> {
        self.entries
            .get(&key)
            .map(|entry| (entry.content_hash, entry.returning))
    }

    pub(super) fn stats(&self) -> MetricDictionaryStats {
        MetricDictionaryStats {
            entries: self.retained_len(),
            estimated_bytes: self.bytes,
            transient_entries: self.transient_len,
            transient_bytes: self.transient_bytes,
            frequency_sketch_bytes: self.frequency.as_ref().map_or(0, FrequencySketch::bytes),
        }
    }

    /// Whether `key` is defined only for the batch in progress or the one before it.
    #[cfg(test)]
    pub(super) fn is_transient(&self, key: DefinitionKey) -> bool {
        self.entries.get(&key).is_some_and(|entry| entry.transient)
    }

    /// Makes every value retained, as before admission existed.
    #[cfg(test)]
    pub(super) fn admit_all(&mut self) {
        self.admit_all = true;
    }

    /// Identifies a value by content rather than ID. Composites hash their dependencies'
    /// stored hashes, so no string is hashed twice and reseeded definitions agree.
    fn content_hash(&self, datum: &MetricDatum) -> u64 {
        let dependency = |key| self.entries[&key].content_hash;
        let mut hasher = DefaultHasher::new();
        match datum.data.as_ref().unwrap() {
            Data::MetricNameDefine(d) => (0u8, &d.value).hash(&mut hasher),
            Data::MetricTagStringDefine(d) => (1u8, &d.value).hash(&mut hasher),
            Data::MetricSourceTypeNameDefine(d) => (2u8, &d.value).hash(&mut hasher),
            Data::MetricUnitDefine(d) => (3u8, &d.value).hash(&mut hasher),
            Data::MetricResourceStringDefine(d) => (4u8, &d.value).hash(&mut hasher),
            Data::MetricOriginDefine(d) => {
                (5u8, d.product, d.category, d.service).hash(&mut hasher)
            }
            Data::MetricResourceDefine(d) => {
                (6u8, d.type_string_ids.len()).hash(&mut hasher);
                for (kind, name) in d.type_string_ids.iter().zip(&d.name_string_ids) {
                    dependency(DefinitionKey::ResourceString(*kind)).hash(&mut hasher);
                    dependency(DefinitionKey::ResourceString(*name)).hash(&mut hasher);
                }
            }
            Data::MetricTagsetDefine(d) => {
                let prefix =
                    (d.prefix_id != 0).then(|| dependency(DefinitionKey::Tagset(d.prefix_id)));
                (7u8, prefix, d.tag_string_ids.len()).hash(&mut hasher);
                for id in &d.tag_string_ids {
                    dependency(DefinitionKey::TagString(*id)).hash(&mut hasher);
                }
            }
            Data::MetricSeriesBatch(_) => unreachable!("series data is not dictionary state"),
        }
        hasher.finish()
    }

    /// Evicts by the policy and returns the removed definitions, dependents before
    /// dependencies. Transient entries no batch in progress references go first, whatever the
    /// pressure. While admission applies, pressure passes evict only to the high watermark:
    /// what they remove is what admission let in, so each admitted value displaces one
    /// resident.
    pub(super) fn evict(&mut self, config: &EvictionConfig) -> Vec<(DefinitionKey, MetricDatum)> {
        let mut evicted = Vec::new();
        let now = self.now.expect("maintenance supplies the clock");
        self.advance_grace(config, now);
        self.remove_transients(config, now, &mut evicted);
        let count_limit = (config.max_item_count as f64 * config.high_watermark) as usize;
        let bytes_limit = (config.max_memory_bytes as f64 * config.high_watermark) as usize;
        let targets = match &mut self.admission {
            Some(admission) => {
                admission.count_limit = count_limit;
                admission.bytes_limit = bytes_limit;
                EvictionConfig {
                    low_watermark: config.high_watermark,
                    ..*config
                }
            }
            None => *config,
        };
        let sweep =
            !config.stale_after.is_zero() && self.next_stale_sweep.is_none_or(|due| now >= due);
        if sweep {
            self.next_stale_sweep = now.checked_add(config.stale_after);
        }
        let (count_over, bytes_over) = config.should_evict(self.retained_len(), self.bytes as i64);
        if !sweep && !count_over && !bytes_over {
            return evicted;
        }
        if sweep {
            self.remove_candidates(config, now, true, 0, 0, &mut evicted);
        }
        let (count_over, bytes_over) = config.should_evict(self.retained_len(), self.bytes as i64);
        let (count, bytes, strategy) = targets.eviction_targets(
            self.retained_len(),
            self.bytes as i64,
            count_over,
            bytes_over,
        );
        if strategy != Strategy::None {
            let victim =
                self.remove_candidates(config, now, false, count, bytes as usize, &mut evicted);
            if self.frequency.is_some() {
                self.admission = Some(Admission {
                    victim,
                    count_limit,
                    bytes_limit,
                });
            }
        }
        evicted
    }

    /// Removes unprotected transient entries, dependents before dependencies.
    fn remove_transients(
        &mut self,
        config: &EvictionConfig,
        now: Instant,
        evicted: &mut Vec<(DefinitionKey, MetricDatum)>,
    ) {
        if self.transient.is_empty() {
            return;
        }
        let returning = EvictionConfig {
            grace_period: Duration::ZERO,
            ..*config
        };
        let mut keys = std::mem::take(&mut self.transient);
        keys.retain(|key| self.entries[key].transient);
        keys.sort_unstable_by(|a, b| b.cmp(a));
        for key in keys {
            let entry = &self.entries[&key];
            // A protected transient composite pins its dependencies until its batch ends.
            if entry.protected_epoch == self.epoch || entry.incoming > 0 {
                self.transient.push(key);
                continue;
            }
            let Definition {
                datum,
                dependencies,
                bytes,
                ..
            } = self.entries.remove(&key).unwrap().definition;
            evicted.push((key, datum));
            self.transient_bytes -= bytes;
            self.transient_len -= 1;
            for key in dependencies {
                let dependency = self.entries.get_mut(&key).unwrap();
                dependency.incoming -= 1;
                let policy = if dependency.returning {
                    &returning
                } else {
                    config
                };
                if dependency.incoming == 0
                    && !dependency.transient
                    && now >= policy.eligible_at(dependency.definition.created_at)
                {
                    self.add_to_frontier(key);
                }
            }
        }
    }

    /// Removes candidates by score and returns the content hash of the next one a pressure
    /// pass would have removed.
    fn remove_candidates(
        &mut self,
        config: &EvictionConfig,
        now: Instant,
        stale: bool,
        count: usize,
        bytes: usize,
        evicted: &mut Vec<(DefinitionKey, MetricDatum)>,
    ) -> Option<u64> {
        // Scores are rebuilt at this pass's clock; only dependency eligibility is persistent.
        if self.frontier.len() == self.protected_frontier {
            return None;
        }
        #[cfg(test)]
        {
            self.candidate_passes += 1;
        }
        let epoch = self.epoch;
        let returning = EvictionConfig {
            grace_period: Duration::ZERO,
            ..*config
        };
        let policy = |entry: &Entry| if entry.returning { &returning } else { config };
        let eligible = |entry: &Entry| {
            entry.protected_epoch != epoch
                && (!stale
                    || policy(entry).is_stale(
                        entry.definition.created_at,
                        entry.definition.last_access_at,
                        now,
                    ))
        };
        let score = |key, entry: &Entry| Candidate::new(key, &entry.definition, now, policy(entry));
        let candidates: Vec<_> = self
            .frontier
            .iter()
            .filter_map(|key| {
                let entry = &self.entries[key];
                eligible(entry).then(|| score(*key, entry))
            })
            .collect();
        let mut candidates = BinaryHeap::from(candidates);
        let mut removed = 0;
        let mut freed = 0;
        while let Some(candidate) = candidates.pop() {
            if !stale && removed >= count && freed >= bytes {
                return Some(self.entries[&candidate.key].content_hash);
            }
            self.remove_from_frontier(candidate.key);
            let Definition {
                datum,
                dependencies,
                bytes: entry_bytes,
                ..
            } = self.entries.remove(&candidate.key).unwrap().definition;
            evicted.push((candidate.key, datum));
            removed += 1;
            freed += entry_bytes;
            self.bytes -= entry_bytes;
            for key in dependencies {
                let dependency = self.entries.get_mut(&key).unwrap();
                dependency.incoming -= 1;
                if dependency.incoming == 0
                    && now >= policy(dependency).eligible_at(dependency.definition.created_at)
                {
                    self.add_to_frontier(key);
                    let dependency = &self.entries[&key];
                    if eligible(dependency) {
                        candidates.push(score(key, dependency));
                    }
                }
            }
        }
        None
    }

    fn add_to_frontier(&mut self, key: DefinitionKey) {
        let entry = self.entries.get_mut(&key).unwrap();
        debug_assert_eq!(entry.incoming, 0);
        debug_assert!(entry.frontier_index.is_none());
        entry.frontier_index = Some(self.frontier.len());
        self.frontier.push(key);
        self.protected_frontier += usize::from(entry.protected_epoch == self.epoch);
    }

    fn remove_from_frontier(&mut self, key: DefinitionKey) {
        let entry = self.entries.get_mut(&key).unwrap();
        if let Some(index) = entry.frontier_index.take() {
            self.protected_frontier -= usize::from(entry.protected_epoch == self.epoch);
            self.frontier.swap_remove(index);
            if let Some(moved) = self.frontier.get(index) {
                self.entries.get_mut(moved).unwrap().frontier_index = Some(index);
            }
        }
    }

    fn index_grace(&mut self, key: DefinitionKey, now: Instant, period: Duration) {
        let entry = &self.entries[&key];
        let period = if entry.returning {
            Duration::ZERO
        } else {
            period
        };
        // Match eligible_at's overflow fallback, including its existing scoring semantics.
        let created = entry.definition.created_at;
        let due = created.checked_add(period).unwrap_or(created);
        if due > now {
            self.grace.insert((due, key));
        } else if entry.incoming == 0 {
            self.add_to_frontier(key);
        }
    }

    fn advance_grace(&mut self, config: &EvictionConfig, now: Instant) {
        if self.indexed_grace_period != Some(config.grace_period) {
            self.frontier.clear();
            self.protected_frontier = 0;
            self.grace.clear();
            // A changed grace period can make mature entries young again.
            let keys: Vec<_> = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.transient)
                .map(|(key, _)| *key)
                .collect();
            for key in keys {
                self.entries.get_mut(&key).unwrap().frontier_index = None;
                self.index_grace(key, now, config.grace_period);
            }
            self.indexed_grace_period = Some(config.grace_period);
        }
        while let Some(&(due, key)) = self.grace.first() {
            if due > now {
                break;
            }
            self.grace.pop_first();
            if self.entries[&key].incoming == 0 {
                self.add_to_frontier(key);
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
    use crate::proto::stateful::{
        MetricNameDefine, MetricResourceDefine, MetricResourceStringDefine, MetricTagStringDefine,
        MetricTagsetDefine,
    };

    use std::collections::BTreeMap;

    use super::{super::eviction_policy::calculate_score, *};

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
        add_named(retention, id, &format!("metric-{id}"));
    }

    fn add_named(retention: &mut Retention, id: u64, value: &str) {
        retention.insert(
            &MetricDatum {
                data: Some(Data::MetricNameDefine(MetricNameDefine {
                    id,
                    value: value.to_string(),
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

    /// Dependency lists hold exactly their distinct dependencies, so the estimate charges no
    /// slack for a prefix, repeated strings, or resource strings shared by kind and name.
    #[test]
    fn dependency_lists_are_charged_at_exact_size() {
        let mut retention = Retention::default();
        retention.begin_batch(Instant::now());
        let resource_string = |id| MetricDatum {
            data: Some(Data::MetricResourceStringDefine(
                MetricResourceStringDefine {
                    id,
                    value: format!("resource-{id}"),
                },
            )),
        };
        let tagset = |id, prefix_id, tag_string_ids| MetricDatum {
            data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                id,
                prefix_id,
                tag_string_ids,
            })),
        };
        let resources = |id, type_string_ids, name_string_ids| MetricDatum {
            data: Some(Data::MetricResourceDefine(MetricResourceDefine {
                id,
                type_string_ids,
                name_string_ids,
            })),
        };
        let mut datums: Vec<_> = (1..=5).map(|id| tag(id, &format!("tag-{id}"))).collect();
        datums.extend((1..=3).map(resource_string));
        datums.extend([
            tagset(1, 0, vec![1, 2, 3, 4, 5]),
            tagset(2, 1, vec![1, 2, 3, 4, 5]),
            tagset(3, 2, vec![1, 1, 2, 2, 5]),
            tagset(4, 0, vec![3, 3]),
            resources(1, vec![1, 1, 1], vec![2, 3, 1]),
            resources(2, vec![1, 2], vec![3, 3]),
        ]);
        for datum in &datums {
            retention.insert(datum, 24);
        }
        for datum in &datums {
            let definition = &retention.entries[&DefinitionKey::of(datum)].definition;
            assert_eq!(
                definition.dependencies.capacity(),
                definition.dependencies.len(),
                "{datum:?}"
            );
            assert_eq!(
                definition.bytes,
                ENTRY_OVERHEAD
                    + size_of::<Definition>()
                    + datum_heap_bytes(datum)
                    + 24
                    + definition.dependencies.len() * size_of::<DefinitionKey>(),
                "{datum:?}"
            );
        }
        let child = &retention.entries[&DefinitionKey::Tagset(2)].definition;
        assert_eq!(child.dependencies.len(), 6, "five strings and the prefix");
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
                .map(|entry| entry.definition.bytes)
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
            assert_eq!(entry.definition.hits, 1);
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
            retention.entries[&DefinitionKey::Name(1)]
                .definition
                .last_access_at,
            now + Duration::from_secs(10)
        );
    }

    // Independent copy of the original scan algorithm: the oracle does not consult indexes.
    // Returning entries have no grace period, so they are scored and aged like any other.
    // Transient entries are outside the budget and never scored; unprotected ones nothing
    // depends on go first, in reverse key order.
    struct ScanRetention {
        entries: BTreeMap<DefinitionKey, Definition>,
        returning: BTreeSet<DefinitionKey>,
        transient: BTreeSet<DefinitionKey>,
        hashes: BTreeMap<DefinitionKey, u64>,
        protected: BTreeSet<DefinitionKey>,
        bytes: usize,
        transient_bytes: usize,
        // Whether a pressure pass has made admission apply: passes then stop at the high
        // watermark.
        admitting: bool,
        now: Option<Instant>,
        next_stale_sweep: Option<Instant>,
    }

    impl ScanRetention {
        /// Evicts by the policy and returns the removed keys, dependents before dependencies,
        /// and, after a pressure pass, the content hash of the next entry it would remove.
        fn evict(&mut self, config: &EvictionConfig) -> (Vec<DefinitionKey>, Option<Option<u64>>) {
            let mut evicted = Vec::new();
            let now = self.now.expect("maintenance supplies the clock");
            for key in self.transient.clone().into_iter().rev() {
                let depended = self
                    .entries
                    .values()
                    .any(|entry| entry.dependencies.contains(&key));
                if self.protected.contains(&key) || depended {
                    continue;
                }
                self.transient.remove(&key);
                self.transient_bytes -= self.entries.remove(&key).unwrap().bytes;
                evicted.push(key);
            }
            let sweep =
                !config.stale_after.is_zero() && self.next_stale_sweep.is_none_or(|due| now >= due);
            if sweep {
                self.next_stale_sweep = now.checked_add(config.stale_after);
            }
            let retained = |scan: &Self| scan.entries.len() - scan.transient.len();
            let (count_over, bytes_over) = config.should_evict(retained(self), self.bytes as i64);
            if !sweep && !count_over && !bytes_over {
                return (evicted, None);
            }
            if sweep {
                self.remove_candidates(config, now, true, 0, 0, &mut evicted);
            }
            let (count_over, bytes_over) = config.should_evict(retained(self), self.bytes as i64);
            let targets = EvictionConfig {
                low_watermark: if self.admitting {
                    config.high_watermark
                } else {
                    config.low_watermark
                },
                ..*config
            };
            let (count, bytes, strategy) =
                targets.eviction_targets(retained(self), self.bytes as i64, count_over, bytes_over);
            let victim = (strategy != Strategy::None).then(|| {
                self.remove_candidates(config, now, false, count, bytes as usize, &mut evicted)
            });
            (evicted, victim)
        }

        fn remove_candidates(
            &mut self,
            config: &EvictionConfig,
            now: Instant,
            stale: bool,
            count: usize,
            bytes: usize,
            evicted: &mut Vec<DefinitionKey>,
        ) -> Option<u64> {
            // Kahn's topological sort removes dependents before dependencies, prioritizing by eviction score.
            let mut dependents: BTreeMap<_, usize> =
                self.entries.keys().map(|key| (*key, 0)).collect();
            for entry in self.entries.values() {
                for dependency in &entry.dependencies {
                    *dependents
                        .get_mut(dependency)
                        .expect("composite dependencies are defined") += 1;
                }
            }
            let eligible = |key: &DefinitionKey, entry: &Definition| {
                let idle = now.saturating_duration_since(entry.last_access_at);
                !self.protected.contains(key)
                    && !self.transient.contains(key)
                    && if self.returning.contains(key) {
                        !stale || (!config.stale_after.is_zero() && idle >= config.stale_after)
                    } else {
                        now >= config.eligible_at(entry.created_at)
                            && (!stale
                                || config.is_stale(entry.created_at, entry.last_access_at, now))
                    }
            };
            let score = |key: DefinitionKey, entry: &Definition| {
                if self.returning.contains(&key) {
                    Candidate {
                        key,
                        score: calculate_score(
                            entry.hits as f64,
                            entry.created_at,
                            entry.last_access_at,
                            now,
                            config.age_decay_factor,
                        ),
                    }
                } else {
                    Candidate::new(key, entry, now, config)
                }
            };
            let mut candidates = BinaryHeap::new();
            for (key, entry) in &self.entries {
                if dependents[key] == 0 && eligible(key, entry) {
                    candidates.push(score(*key, entry));
                }
            }
            let mut removed = 0;
            let mut freed = 0;
            while let Some(candidate) = candidates.pop() {
                if !stale && removed >= count && freed >= bytes {
                    return Some(self.hashes[&candidate.key]);
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
                        candidates.push(score(key, dependency));
                    }
                }
            }
            None
        }
    }

    fn assert_matches_scan(
        retention: &mut Retention,
        config: &EvictionConfig,
    ) -> Vec<DefinitionKey> {
        let keys_where = |predicate: fn(&Entry) -> bool| {
            retention
                .entries
                .iter()
                .filter(|(_, entry)| predicate(entry))
                .map(|(key, _)| *key)
                .collect()
        };
        let mut scan = ScanRetention {
            entries: retention
                .entries
                .iter()
                .map(|(key, entry)| (*key, entry.definition.clone()))
                .collect(),
            returning: keys_where(|entry| entry.returning),
            transient: keys_where(|entry| entry.transient),
            hashes: retention
                .entries
                .iter()
                .map(|(key, entry)| (*key, entry.content_hash))
                .collect(),
            protected: retention.protected.iter().copied().collect(),
            bytes: retention.bytes,
            transient_bytes: retention.transient_bytes,
            admitting: retention.admission.is_some(),
            now: retention.now,
            next_stale_sweep: retention.next_stale_sweep,
        };
        let admission = retention.admission;
        let datums: BTreeMap<_, _> = scan
            .entries
            .iter()
            .map(|(key, entry)| (*key, entry.datum.clone()))
            .collect();
        let (expected, victim) = scan.evict(config);
        let evicted = retention.evict(config);
        assert_eq!(
            evicted.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            expected
        );
        for (key, datum) in &evicted {
            assert_eq!(&datums[key], datum, "eviction hands back the datum");
        }
        assert_eq!(
            retention.keys().collect::<Vec<_>>(),
            scan.entries.keys().copied().collect::<Vec<_>>()
        );
        assert_eq!(retention.bytes, scan.bytes);
        assert_eq!(retention.transient_bytes, scan.transient_bytes);
        assert_eq!(retention.next_stale_sweep, scan.next_stale_sweep);
        let expected_victim = match victim {
            Some(victim) if retention.frequency.is_some() => Some(victim),
            _ => admission.map(|admission| admission.victim),
        };
        assert_eq!(
            retention.admission.map(|admission| admission.victim),
            expected_victim
        );
        assert_indexes(retention, config);
        expected
    }

    fn assert_indexes(retention: &Retention, config: &EvictionConfig) {
        let now = retention.now.unwrap();
        let mut incoming: BTreeMap<_, usize> = retention.keys().map(|key| (key, 0)).collect();
        for entry in retention.entries.values() {
            for key in &entry.definition.dependencies {
                *incoming.get_mut(key).unwrap() += 1;
                assert!(
                    entry.transient || !retention.entries[key].transient,
                    "a retained composite retains its dependencies"
                );
            }
        }
        let transient: BTreeSet<_> = retention
            .entries
            .iter()
            .filter(|(_, entry)| entry.transient)
            .map(|(key, _)| *key)
            .collect();
        assert_eq!(retention.transient_len, transient.len());
        assert!(transient
            .iter()
            .all(|key| retention.transient.contains(key)));
        let sum = |transient: bool| {
            retention
                .entries
                .values()
                .filter(|entry| entry.transient == transient)
                .map(|entry| entry.definition.bytes)
                .sum::<usize>()
        };
        assert_eq!(retention.bytes, sum(false));
        assert_eq!(retention.transient_bytes, sum(true));
        let mut expected_frontier = BTreeSet::new();
        let mut expected_grace = BTreeSet::new();
        for (key, entry) in &retention.entries {
            assert_eq!(entry.incoming, incoming[key]);
            if entry.transient {
                assert_eq!(entry.frontier_index, None);
                continue;
            }
            let due = if entry.returning {
                entry.definition.created_at
            } else {
                config.eligible_at(entry.definition.created_at)
            };
            if due > now {
                expected_grace.insert((due, *key));
            } else if incoming[key] == 0 {
                expected_frontier.insert(*key);
            }
            assert_eq!(
                entry.frontier_index.is_some(),
                expected_frontier.contains(key)
            );
        }
        assert_eq!(
            retention.frontier.iter().copied().collect::<BTreeSet<_>>(),
            expected_frontier
        );
        assert_eq!(retention.grace, expected_grace);
        for (index, key) in retention.frontier.iter().enumerate() {
            assert_eq!(retention.entries[key].frontier_index, Some(index));
        }
        assert_eq!(
            retention.protected_frontier,
            expected_frontier
                .intersection(&retention.protected.iter().copied().collect())
                .count()
        );
    }

    #[test]
    fn impossible_pressure_passes_do_not_enumerate_candidates() {
        let now = Instant::now();
        let config = EvictionConfig {
            max_item_count: 0,
            grace_period: Duration::from_secs(30),
            ..policy()
        };
        let mut retention = Retention::default();
        retention.begin_batch(now);
        for id in 1..=20 {
            add_name(&mut retention, id);
        }
        for second in 0..30 {
            retention.begin_batch(now + Duration::from_secs(second));
            assert_matches_scan(&mut retention, &config);
        }
        assert_eq!(retention.candidate_passes, 0);
        retention.begin_batch(now + Duration::from_secs(30));
        retention.touch((1..=20).map(DefinitionKey::Name));
        assert_matches_scan(&mut retention, &config);
        assert_eq!(retention.protected_frontier, 20);
        assert_eq!(retention.candidate_passes, 0);
        retention.begin_batch(now + Duration::from_secs(31));
        assert_matches_scan(&mut retention, &config);
        assert_eq!(retention.stats(), MetricDictionaryStats::default());
        assert_eq!(retention.candidate_passes, 1);
    }

    #[test]
    fn epoch_wrap_releases_previous_protection() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now);
        add_name(&mut retention, 1);
        add_name(&mut retention, 2);
        retention.touch([DefinitionKey::Name(1)]);
        retention.evict(&policy());
        retention.epoch = u64::MAX;
        retention.touch([DefinitionKey::Name(2)]);
        retention.begin_batch(now);
        retention.touch([DefinitionKey::Name(2)]);
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 0,
                ..policy()
            },
        );
        assert_eq!(
            retention.keys().collect::<Vec<_>>(),
            vec![DefinitionKey::Name(2)]
        );
    }

    #[test]
    fn overlapping_roots_count_once_per_series_and_again_in_the_same_batch() {
        let mut retention = Retention::default();
        retention.begin_batch(Instant::now());
        retention.insert(
            &MetricDatum {
                data: Some(Data::MetricTagStringDefine(MetricTagStringDefine {
                    id: 1,
                    value: "tag".into(),
                })),
            },
            0,
        );
        for id in 1..=2 {
            retention.insert(
                &MetricDatum {
                    data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                        id,
                        prefix_id: id - 1,
                        tag_string_ids: vec![1, 1],
                    })),
                },
                0,
            );
        }
        for _ in 0..2 {
            retention.touch([
                DefinitionKey::Tagset(2),
                DefinitionKey::Tagset(1),
                DefinitionKey::TagString(1),
            ]);
        }
        assert!(retention
            .entries
            .values()
            .all(|entry| entry.definition.hits == 2));
        assert_eq!(
            retention.references().collect::<Vec<_>>(),
            vec![
                DefinitionKey::TagString(1),
                DefinitionKey::Tagset(1),
                DefinitionKey::Tagset(2)
            ]
        );
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 0,
                ..policy()
            },
        );
    }

    #[test]
    fn grace_policy_changes_and_overflow_match_the_scan_policy() {
        let now = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(now);
        for id in 1..=8 {
            add_name(&mut retention, id);
        }
        retention.touch((1..=8).map(DefinitionKey::Name));
        for grace_period in [
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::MAX,
            Duration::from_secs(1),
        ] {
            assert_matches_scan(
                &mut retention,
                &EvictionConfig {
                    grace_period,
                    ..policy()
                },
            );
        }
        retention.begin_batch(now + Duration::from_secs(2));
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                grace_period: Duration::MAX,
                ..policy()
            },
        );
        assert_eq!(retention.entries.len(), 2);
    }

    #[test]
    fn mixed_dependency_churn_matches_full_scan_eviction() {
        let start = Instant::now();
        let mut retention = Retention::with_frequency_sketch(&EvictionConfig {
            max_item_count: 64,
            ..policy()
        });
        let mut random = 12345u64;
        let mut next = || {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            random
        };
        let mut id = 0;
        let mut inserted = [0; 2];
        // Values inserted while admission applied, by whether they became transient.
        let mut contended = [0; 2];
        let mut promoted = 0;
        for round in 0..400u64 {
            // Include backward clock inputs and repeated calls at the same time.
            let now = start + Duration::from_secs(round.saturating_sub(next() % 7));
            retention.begin_batch(now);
            for _ in 0..(next() % 5) {
                id += 1;
                if next() % 2 == 0 {
                    // A name's value under a new ID, as after eviction, or before its own.
                    let value = next() % (4 * id);
                    let gated = !retention.admits(0);
                    add_named(&mut retention, id + 1_000_000, &format!("metric-{value}"));
                    let entry = &retention.entries[&DefinitionKey::Name(id + 1_000_000)];
                    inserted[usize::from(entry.returning)] += 1;
                    if gated {
                        contended[usize::from(entry.transient)] += 1;
                    }
                }
                add_name(&mut retention, id);
                retention.insert(
                    &MetricDatum {
                        data: Some(Data::MetricTagStringDefine(MetricTagStringDefine {
                            id,
                            value: format!("tag-{id}"),
                        })),
                    },
                    12,
                );
                let prefix = retention
                    .keys()
                    .filter_map(|key| match key {
                        DefinitionKey::Tagset(id) => Some(id),
                        _ => None,
                    })
                    .last()
                    .unwrap_or(0);
                let shared = retention
                    .keys()
                    .find_map(|key| match key {
                        DefinitionKey::TagString(id) => Some(id),
                        _ => None,
                    })
                    .unwrap();
                retention.insert(
                    &MetricDatum {
                        data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                            id,
                            prefix_id: if next() % 3 == 0 { prefix } else { 0 },
                            tag_string_ids: vec![id, shared, id],
                        })),
                    },
                    24,
                );
                if next() % 2 == 0 {
                    // A recurring composite under a new ID, over a possibly transient string.
                    let gated = !retention.admits(0);
                    retention.insert(&tagset(id + 2_000_000, vec![shared]), 8);
                    let key = DefinitionKey::Tagset(id + 2_000_000);
                    retention.touch([key]);
                    if gated {
                        contended[usize::from(retention.is_transient(key))] += 1;
                    }
                }
            }
            let roots: Vec<_> = retention.keys().filter(|_| next() % 5 == 0).collect();
            let transient = roots
                .iter()
                .filter(|key| retention.is_transient(**key))
                .count();
            retention.touch(roots.iter().copied());
            promoted += transient
                - roots
                    .iter()
                    .filter(|key| retention.is_transient(**key))
                    .count();
            retention.touch(roots);
            let config = EvictionConfig {
                max_item_count: if round % 9 == 0 { 10000 } else { 20 },
                max_memory_bytes: if round % 3 == 0 { 4000 } else { i64::MAX },
                grace_period: Duration::from_secs([0, 3, 9, 30][(round / 19 % 4) as usize]),
                stale_after: Duration::from_secs([0, 5, 11][(round / 13 % 3) as usize]),
                age_decay_factor: [0.0, 0.5, 2.0][(round / 7 % 3) as usize],
                ..policy()
            };
            assert_matches_scan(&mut retention, &config);
            assert_matches_scan(&mut retention, &config);
        }
        retention.begin_batch(start + Duration::from_secs(1000));
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 0,
                ..policy()
            },
        );
        assert!(retention.entries.is_empty());
        assert!(retention.grace.is_empty());
        assert!(retention.frontier.is_empty());
        assert!(
            inserted.iter().all(|count| *count > 20),
            "both fresh and returning values were inserted: {inserted:?}"
        );
        assert!(
            contended.iter().all(|count| *count > 10) && promoted > 5,
            "admission both retained and rejected values, and promoted recurring ones: \
             {contended:?}, {promoted}"
        );
    }

    fn sketched() -> Retention {
        Retention::with_frequency_sketch(&EvictionConfig {
            max_item_count: 1000,
            ..policy()
        })
    }

    /// A value evicted while in use returns under a new ID with half its estimated frequency
    /// and no grace period, so it competes with established entries at once: it outranks a
    /// colder one but is still evictable, while a new value keeps its grace period.
    #[test]
    fn returning_values_are_seeded_and_skip_the_grace_period() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let config = EvictionConfig {
            grace_period: Duration::from_secs(30),
            ..policy()
        };
        let mut retention = sketched();
        retention.begin_batch(at(0));
        add_named(&mut retention, 1, "returning");
        add_named(&mut retention, 2, "cold");
        retention.touch([DefinitionKey::Name(2)]);
        for second in 0..6 {
            retention.begin_batch(at(second));
            retention.touch([DefinitionKey::Name(1)]);
        }
        let (hash, _) = retention.frequency_state(DefinitionKey::Name(1)).unwrap();
        retention.begin_batch(at(31));
        retention.touch([DefinitionKey::Name(2)]);
        assert_eq!(
            assert_matches_scan(
                &mut retention,
                &EvictionConfig {
                    max_item_count: 0,
                    ..config
                }
            ),
            [DefinitionKey::Name(1)]
        );

        // Below the low watermark of the configuration in force, every value is admitted.
        retention.begin_batch(at(32));
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 1000,
                ..config
            },
        );
        add_named(&mut retention, 3, "returning");
        add_named(&mut retention, 4, "new");
        let returning = &retention.entries[&DefinitionKey::Name(3)];
        assert!(returning.returning);
        assert_eq!(returning.definition.hits, 3, "half of six batches");
        assert_eq!(returning.content_hash, hash);
        let new = &retention.entries[&DefinitionKey::Name(4)];
        assert!(!new.returning);
        assert_eq!(new.definition.hits, 0);
        assert!(retention.grace.contains(&(at(62), DefinitionKey::Name(4))));
        assert!(!retention
            .grace
            .iter()
            .any(|(_, key)| *key == DefinitionKey::Name(3)));
        retention.touch([DefinitionKey::Name(3), DefinitionKey::Name(4)]);
        assert_indexes(&retention, &config);
        assert_eq!(
            retention.protected_frontier, 1,
            "the returning leaf is on the frontier"
        );

        retention.begin_batch(at(33));
        assert_eq!(
            assert_matches_scan(
                &mut retention,
                &EvictionConfig {
                    max_item_count: 1,
                    low_watermark: 1.0,
                    ..config
                }
            ),
            [DefinitionKey::Name(2), DefinitionKey::Name(3)],
            "the colder resident goes first; the new value is still in its grace period"
        );
    }

    /// Composite values return seeded too, and their dependencies' content hashes keep a
    /// prefix tagset recognizable under its new ID.
    #[test]
    fn returning_composites_rejoin_the_frontier_consistently() {
        let start = Instant::now();
        let config = EvictionConfig {
            grace_period: Duration::from_secs(30),
            ..policy()
        };
        let tag = |id| MetricDatum {
            data: Some(Data::MetricTagStringDefine(MetricTagStringDefine {
                id,
                value: "env:prod".into(),
            })),
        };
        let tagset = |id, prefix_id, tag_id| MetricDatum {
            data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                id,
                prefix_id,
                tag_string_ids: vec![tag_id],
            })),
        };
        let mut retention = sketched();
        retention.begin_batch(start);
        for datum in [tag(1), tagset(1, 0, 1), tagset(2, 1, 1)] {
            retention.insert(&datum, 0);
        }
        retention.touch([DefinitionKey::Tagset(2)]);
        let hashes: Vec<_> = [
            DefinitionKey::TagString(1),
            DefinitionKey::Tagset(1),
            DefinitionKey::Tagset(2),
        ]
        .map(|key| retention.frequency_state(key).unwrap().0)
        .into();
        retention.begin_batch(start + Duration::from_secs(30));
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 0,
                ..config
            },
        );
        assert_eq!(retention.stats().entries, 0);

        retention.begin_batch(start + Duration::from_secs(31));
        for datum in [tag(2), tagset(3, 0, 2), tagset(4, 3, 2)] {
            retention.insert(&datum, 0);
        }
        let returned: Vec<_> = [
            DefinitionKey::TagString(2),
            DefinitionKey::Tagset(3),
            DefinitionKey::Tagset(4),
        ]
        .map(|key| retention.frequency_state(key).unwrap())
        .into();
        assert_eq!(
            returned,
            hashes.iter().map(|hash| (*hash, true)).collect::<Vec<_>>()
        );
        assert!(retention.grace.is_empty());
        assert_eq!(retention.frontier, [DefinitionKey::Tagset(4)]);
        assert_indexes(&retention, &config);
        retention.begin_batch(start + Duration::from_secs(32));
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 0,
                ..config
            },
        );
        assert_eq!(
            retention.stats().entries,
            0,
            "no grace holds a returning chain"
        );
    }

    #[test]
    fn without_eviction_there_is_no_frequency_state() {
        let start = Instant::now();
        let mut retention = Retention::default();
        retention.begin_batch(start);
        add_named(&mut retention, 1, "returning");
        for second in 0..6 {
            retention.begin_batch(start + Duration::from_secs(second));
            retention.touch([DefinitionKey::Name(1)]);
        }
        retention.begin_batch(start + Duration::from_secs(30));
        retention.evict(&EvictionConfig {
            max_item_count: 0,
            ..policy()
        });
        add_named(&mut retention, 2, "returning");

        assert!(retention.frequency.is_none());
        assert_eq!(retention.stats().frequency_sketch_bytes, 0);
        assert_eq!(
            retention.frequency_state(DefinitionKey::Name(2)),
            Some((0, false))
        );
        assert_eq!(
            retention.entries[&DefinitionKey::Name(2)].definition.hits,
            0
        );
        assert!(sketched().stats().frequency_sketch_bytes > 0);
    }

    fn at(start: Instant, seconds: u64) -> Instant {
        start + Duration::from_secs(seconds)
    }

    fn tag(id: u64, value: &str) -> MetricDatum {
        MetricDatum {
            data: Some(Data::MetricTagStringDefine(MetricTagStringDefine {
                id,
                value: value.into(),
            })),
        }
    }

    fn tagset(id: u64, tag_string_ids: Vec<u64>) -> MetricDatum {
        MetricDatum {
            data: Some(Data::MetricTagsetDefine(MetricTagsetDefine {
                id,
                prefix_id: 0,
                tag_string_ids,
            })),
        }
    }

    /// Counts `datum`'s value `times` more, as if referenced in that many more batches.
    fn reference(retention: &mut Retention, datum: &MetricDatum, times: usize) {
        let hash = retention.content_hash(datum);
        for _ in 0..times {
            retention.frequency.as_mut().unwrap().increment(hash);
        }
    }

    /// Five values referenced in three batches; a pressure pass at `start + 3s` keeps the
    /// last `4 * watermark` and makes the first of them the victim to beat, at an estimate
    /// of 3.
    fn full(watermark: f64) -> (Retention, EvictionConfig, Instant) {
        let start = Instant::now();
        let config = EvictionConfig {
            high_watermark: watermark,
            low_watermark: watermark,
            ..policy()
        };
        let mut retention = sketched();
        retention.begin_batch(start);
        for id in 1..=5 {
            add_named(&mut retention, id, &format!("resident-{id}"));
        }
        for second in 0..3 {
            retention.begin_batch(at(start, second));
            retention.touch((1..=5).map(DefinitionKey::Name));
        }
        retention.begin_batch(at(start, 3));
        assert_matches_scan(&mut retention, &config);
        let victim = retention.admission.unwrap().victim.unwrap();
        assert_eq!(retention.frequency.as_ref().unwrap().estimate(victim), 3);
        (retention, config, start)
    }

    /// At the cap, values no more frequent than the victim, new or returning, are defined
    /// for their batch without counting against the cap or displacing anyone, and are the
    /// first entries the next pass removes, dependents first.
    #[test]
    fn at_the_cap_cold_values_are_transient_for_one_batch() {
        let (mut retention, config, start) = full(0.5);
        assert_eq!(
            retention.keys().collect::<Vec<_>>(),
            [DefinitionKey::Name(4), DefinitionKey::Name(5)]
        );

        retention.begin_batch(at(start, 4));
        add_named(&mut retention, 10, "new");
        // Returning at the victim's frequency: the tie keeps the incumbent.
        add_named(&mut retention, 11, "resident-1");
        retention.insert(&tag(1, "env:new"), 0);
        retention.insert(&tagset(1, vec![1]), 0);
        assert!(retention.entries[&DefinitionKey::Name(11)].returning);
        let defined = [
            DefinitionKey::Name(10),
            DefinitionKey::Name(11),
            DefinitionKey::TagString(1),
            DefinitionKey::Tagset(1),
        ];
        assert!(defined.iter().all(|key| retention.is_transient(*key)));
        retention.touch([
            DefinitionKey::Name(10),
            DefinitionKey::Name(11),
            DefinitionKey::Tagset(1),
        ]);
        assert!(retention.references().eq(defined));
        assert!(defined
            .iter()
            .all(|key| retention.definition(*key).is_some()));
        let stats = retention.stats();
        assert_eq!((stats.entries, stats.transient_entries), (2, 4));
        assert!(stats.transient_bytes > 0);
        assert!(assert_matches_scan(&mut retention, &config).is_empty());
        assert_eq!(
            retention.stats(),
            stats,
            "the batch's own transient values survive it"
        );

        retention.begin_batch(at(start, 5));
        assert_eq!(
            assert_matches_scan(&mut retention, &config),
            [
                DefinitionKey::Tagset(1),
                DefinitionKey::TagString(1),
                DefinitionKey::Name(11),
                DefinitionKey::Name(10),
            ]
        );
        let stats = retention.stats();
        assert_eq!(
            (
                stats.entries,
                stats.transient_entries,
                stats.transient_bytes
            ),
            (2, 0, 0)
        );
    }

    /// Values referenced more often than the victim are retained, and each displaces the
    /// coldest resident when its batch is done; a transient value displaces nothing and goes
    /// first.
    #[test]
    fn hot_values_are_admitted_and_displace_the_coldest_residents() {
        let (mut retention, config, start) = full(0.75);
        assert_eq!(retention.stats().entries, 3);

        retention.begin_batch(at(start, 4));
        reference(&mut retention, &name_datum(11, "resident-1"), 2);
        add_named(&mut retention, 11, "resident-1");
        add_named(&mut retention, 10, "new");
        retention.touch([DefinitionKey::Name(10), DefinitionKey::Name(11)]);
        assert!(!retention.is_transient(DefinitionKey::Name(11)));
        assert!(retention.is_transient(DefinitionKey::Name(10)));
        assert!(retention.frontier.contains(&DefinitionKey::Name(11)));
        assert_eq!(
            assert_matches_scan(&mut retention, &config),
            [DefinitionKey::Name(3)]
        );

        retention.begin_batch(at(start, 5));
        reference(&mut retention, &name_datum(12, "resident-2"), 2);
        add_named(&mut retention, 12, "resident-2");
        retention.touch([DefinitionKey::Name(11), DefinitionKey::Name(12)]);
        assert_eq!(retention.stats().entries, 4);
        assert_eq!(
            assert_matches_scan(&mut retention, &config),
            [DefinitionKey::Name(10), DefinitionKey::Name(4)]
        );
        assert_eq!(
            retention.keys().collect::<Vec<_>>(),
            [
                DefinitionKey::Name(5),
                DefinitionKey::Name(11),
                DefinitionKey::Name(12)
            ]
        );
    }

    fn name_datum(id: u64, value: &str) -> MetricDatum {
        MetricDatum {
            data: Some(Data::MetricNameDefine(MetricNameDefine {
                id,
                value: value.into(),
            })),
        }
    }

    /// Counting includes transient references, so a recurring value is admitted once it is
    /// referenced more often than the victim: whether it stays defined by recurring before
    /// its removal, or is redefined under a fresh ID each time it returns.
    #[test]
    fn a_recurring_value_is_admitted_once_it_outnumbers_the_victim() {
        let (mut retention, config, start) = full(0.5);
        let mut returning_id = 20;
        let mut admitted = None;
        for batch in 0..9u64 {
            retention.begin_batch(at(start, 4 + batch));
            if batch == 0 {
                add_named(&mut retention, 10, "recurring");
            }
            retention.touch([DefinitionKey::Name(10)]);
            assert_eq!(
                retention.is_transient(DefinitionKey::Name(10)),
                batch < 4,
                "estimate {batch} against the victim's 3"
            );
            if batch % 2 == 0 {
                returning_id += 1;
                add_named(&mut retention, returning_id, "returning");
                retention.touch([DefinitionKey::Name(returning_id)]);
                if !retention.is_transient(DefinitionKey::Name(returning_id)) {
                    admitted.get_or_insert(batch);
                }
            }
            assert_matches_scan(&mut retention, &config);
        }
        let recurring = &retention.entries[&DefinitionKey::Name(10)];
        assert!(recurring.returning && recurring.frontier_index.is_some());
        assert_eq!(
            admitted,
            Some(8),
            "the fifth definition, at an estimate of 4"
        );
        assert_eq!(retention.stats().transient_entries, 0);
    }

    /// Admission applies only once a pass has relieved pressure, and only at or above the
    /// low watermark of the configuration in force.
    #[test]
    fn below_the_cap_everything_is_admitted() {
        let start = Instant::now();
        let config = EvictionConfig {
            max_item_count: 10,
            ..policy()
        };
        let mut retention = sketched();
        for batch in 0..8u64 {
            retention.begin_batch(at(start, batch));
            add_named(&mut retention, batch + 1, &format!("value-{}", batch % 3));
            retention.touch([DefinitionKey::Name(batch + 1)]);
            assert_matches_scan(&mut retention, &config);
        }
        assert_eq!(retention.stats().entries, 8, "between the watermarks");
        assert_eq!(retention.stats().transient_entries, 0);
        assert!(retention.admission.is_none());

        let (mut retention, config, start) = full(0.5);
        retention.begin_batch(at(start, 4));
        assert_matches_scan(
            &mut retention,
            &EvictionConfig {
                max_item_count: 8,
                ..config
            },
        );
        add_named(&mut retention, 10, "new");
        assert!(!retention.is_transient(DefinitionKey::Name(10)));
    }

    /// A retained composite retains its dependencies, whether it is admitted as it is
    /// defined or when it recurs; a transient composite holds retained dependencies only
    /// until it is removed.
    #[test]
    fn admitted_composites_promote_their_dependencies() {
        let (mut retention, config, start) = full(0.5);
        retention.begin_batch(at(start, 4));
        retention.insert(&tag(1, "env:new"), 0);
        assert!(retention.is_transient(DefinitionKey::TagString(1)));
        let hot = tagset(1, vec![1]);
        reference(&mut retention, &hot, 4);
        retention.insert(&hot, 0);
        retention.insert(&tagset(2, vec![1, 1]), 0);
        retention.touch([DefinitionKey::Tagset(1), DefinitionKey::Tagset(2)]);
        assert!(!retention.is_transient(DefinitionKey::TagString(1)));
        assert!(!retention.is_transient(DefinitionKey::Tagset(1)));
        assert!(retention.is_transient(DefinitionKey::Tagset(2)));
        assert_eq!(retention.entries[&DefinitionKey::TagString(1)].incoming, 2);
        assert_matches_scan(&mut retention, &config);

        retention.begin_batch(at(start, 5));
        retention.insert(&tag(2, "env:other"), 0);
        let recurring = tagset(3, vec![2]);
        retention.insert(&recurring, 0);
        retention.touch([DefinitionKey::Tagset(3)]);
        assert_eq!(
            assert_matches_scan(&mut retention, &config),
            [DefinitionKey::Tagset(2)]
        );
        assert_eq!(retention.entries[&DefinitionKey::TagString(1)].incoming, 1);

        retention.begin_batch(at(start, 6));
        reference(&mut retention, &recurring, 4);
        retention.touch([DefinitionKey::Tagset(3)]);
        assert!(!retention.is_transient(DefinitionKey::Tagset(3)));
        assert!(!retention.is_transient(DefinitionKey::TagString(2)));
        assert_matches_scan(&mut retention, &config);
        assert_eq!(retention.stats().transient_entries, 0);
    }
}
