//! The bucket store: per-series one-second buckets, bounds, retention, and eviction.

use std::collections::{BTreeMap, BTreeSet};

use super::TELEMETRY_NAMESPACE;
use crate::config::StorageConfig;
use crate::identity::{self, Aggregate, NamespaceId, SeriesRef, SeriesRefAllocator};
use crate::model::{MetricContext, Point, Series, SeriesMeta};
use crate::traits::StorageView;

/// The outcome of a [`TimeSeriesStorage::add`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddResult {
    /// `true` when the call created a new series, so the live-series cardinality grew by one.
    pub is_new: bool,
    /// The ref of the series the sample was written to, or `None` when the sample was dropped by value
    /// admission (a non-finite value, or the exact `±f64::MAX` sentinel). Nothing is stored when this is
    /// `None`.
    pub series_ref: Option<SeriesRef>,
}

/// Criteria for selecting series during listing.
///
/// This mirrors the Go `SeriesFilter`: the criteria combine with AND, an empty criterion matches anything,
/// and [`SeriesFilter::exclude_namespaces`] is only consulted when [`SeriesFilter::namespace`] is unset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeriesFilter {
    /// Exact namespace match. When set, `exclude_namespaces` is ignored.
    pub namespace: Option<String>,
    /// Exact host match. `None` matches any host.
    pub host: Option<String>,
    /// Metric-name prefix match. `None` matches any name.
    pub name_prefix: Option<String>,
    /// Required `key:value` tag matchers; every matcher must be present on the series.
    pub tag_matchers: BTreeMap<String, String>,
    /// Namespaces to skip in list-all mode (when `namespace` is `None`).
    pub exclude_namespaces: Vec<String>,
}

impl SeriesFilter {
    /// Returns the canonical workload filter: every namespace except [`TELEMETRY_NAMESPACE`].
    ///
    /// This is what every detector uses, which is how the testbench's own chart series stay out of
    /// detection.
    pub fn workload() -> Self {
        Self {
            exclude_namespaces: vec![TELEMETRY_NAMESPACE.to_string()],
            ..Self::default()
        }
    }
}

/// One one-second bucket: the bucket second and the sum of the samples that landed in it.
#[derive(Clone, Copy, Debug)]
struct PointBucket {
    second: i64,
    sum: f64,
}

/// Per-series state.
#[derive(Clone, Debug)]
struct SeriesState {
    namespace: NamespaceId,
    name: String,
    host: Option<String>,
    tags: Vec<String>,
    storage_key: u64,
    series_ref: SeriesRef,
    context: Option<MetricContext>,
    /// Bit mask of supported aggregates. `0` means every aggregate is supported.
    supported_aggregations: u8,
    /// When positive, replaces the storage-wide point retention for this series.
    retention_override_secs: i64,
    /// Drives capacity eviction; normally the latest stored timestamp, but producers of synthetic points
    /// may override it so generated data does not make an idle series look active.
    last_activity_secs: i64,
    /// Incremented on every write, including same-bucket merges.
    write_generation: u64,
    buckets: Vec<PointBucket>,
    /// Explicit per-bucket counts, allocated lazily on the first same-second merge. `None` means every
    /// bucket holds exactly one sample.
    counts: Option<Vec<u64>>,
}

impl SeriesState {
    /// Returns the sample count for bucket `index`. A missing count vector means every bucket holds one.
    fn count_at(&self, index: usize) -> u64 {
        match &self.counts {
            None => 1,
            Some(counts) => counts[index],
        }
    }

    /// Materialises exact counts on the first same-second merge, then increments bucket `index`.
    fn increment_count(&mut self, index: usize) {
        if self.counts.is_none() {
            let capacity = std::cmp::max(8, self.buckets.capacity());
            let mut values = Vec::with_capacity(capacity);
            values.resize(self.buckets.len(), 1u64);
            self.counts = Some(values);
        }
        if let Some(counts) = self.counts.as_mut() {
            counts[index] += 1;
        }
    }

    /// Inserts the implicit count for a bucket newly inserted at `index`, keeping the vector aligned.
    fn insert_count(&mut self, index: usize) {
        if let Some(counts) = self.counts.as_mut() {
            counts.insert(index, 1);
        }
    }

    /// Removes the oldest `n` buckets, keeping an explicit count vector aligned.
    fn trim_buckets(&mut self, n: usize) {
        self.buckets.drain(..n);
        if let Some(counts) = self.counts.as_mut() {
            counts.drain(..n);
        }
    }

    /// Returns the total number of raw samples across all buckets. A bucket may hold more than one sample
    /// after a same-second merge.
    fn sample_count(&self) -> u64 {
        match &self.counts {
            None => self.buckets.len() as u64,
            Some(counts) => counts.iter().copied().sum(),
        }
    }

    /// Returns the requested statistic for bucket `index`.
    ///
    /// `avg` of a zero-count bucket is `0`; [`Aggregate::None`] and unknown aggregates also read `0`, so
    /// reads default rather than fail.
    fn aggregate_at(&self, index: usize, aggregate: Aggregate) -> f64 {
        let bucket = self.buckets[index];
        let count = self.count_at(index);
        match aggregate {
            Aggregate::Average => {
                if count == 0 {
                    0.0
                } else {
                    bucket.sum / count as f64
                }
            }
            Aggregate::Sum => bucket.sum,
            Aggregate::Count => count as f64,
            Aggregate::None => 0.0,
        }
    }

    /// Returns whether this series allows the given aggregate (empty mask allows everything).
    fn allows_aggregate(&self, aggregate: Aggregate) -> bool {
        self.supported_aggregations == 0 || self.supported_aggregations & aggregate_mask(aggregate) != 0
    }
}

/// Returns the one-bit mask for an aggregate (bit index is the Go enum ordinal).
fn aggregate_mask(aggregate: Aggregate) -> u8 {
    1 << aggregate.ordinal()
}

/// Applies a series' effective point retention, then the global point cap.
fn trim_points(cfg: &StorageConfig, state: &mut SeriesState) {
    let retention_secs = if state.retention_override_secs > 0 {
        state.retention_override_secs
    } else {
        cfg.point_retention_secs
    };
    if retention_secs > 0 {
        if let Some(latest) = state.buckets.last().map(|bucket| bucket.second) {
            // Keep buckets with `second >= latest - retention`. The series' own latest timestamp is used
            // (never the incoming bucket) so that backfilled points cannot shift the cutoff backwards and
            // over-retain stale data.
            let trim = state
                .buckets
                .partition_point(|bucket| bucket.second < latest - retention_secs);
            if trim > 0 {
                state.trim_buckets(trim);
            }
        }
    }
    if cfg.max_points_per_series > 0 {
        // One extra bucket is retained as the pending scheduler bucket.
        let physical_capacity = cfg.max_points_per_series + 1;
        let len = state.buckets.len();
        if len > physical_capacity {
            state.trim_buckets(len - physical_capacity);
        }
    }
}

/// An in-memory, bounded, bucketed time-series store.
///
/// The store is owned exclusively by the engine and has no interior locking; every method takes `&self`
/// or `&mut self` and is safe to call from one thread. See the [module documentation](self) for the
/// representation and the host semantics.
#[derive(Debug)]
pub struct TimeSeriesStorage {
    cfg: StorageConfig,
    /// Live series keyed by ref, giving deterministic ascending-ref iteration.
    by_ref: BTreeMap<SeriesRef, SeriesState>,
    /// Series key -> ref, for O(1) lookup on write.
    keys: std::collections::HashMap<u64, SeriesRef>,
    /// Every timestamp at which an observation occurred, including logs that produced no metric.
    observation_timestamps: BTreeSet<i64>,
    refs: SeriesRefAllocator,
    /// Number of live non-telemetry series.
    live_series_count: usize,
    /// Bumped whenever the set of series changes.
    series_gen: u64,
}

impl TimeSeriesStorage {
    /// Creates an empty store with the given configuration.
    pub fn new(cfg: StorageConfig) -> Self {
        Self {
            cfg,
            by_ref: BTreeMap::new(),
            keys: std::collections::HashMap::new(),
            observation_timestamps: BTreeSet::new(),
            refs: SeriesRefAllocator::new(),
            live_series_count: 0,
            series_gen: 0,
        }
    }

    /// Returns the configuration the store was created with.
    pub fn config(&self) -> &StorageConfig {
        &self.cfg
    }

    /// Inserts a sample, creating the series if its identity is new.
    ///
    /// The series key is derived from `(namespace, name, host, tags)` with the Go-compatible
    /// [`identity::storage_key`]; an unset host collapses to the empty string like the Go model's single
    /// host field, so `None` and `Some("")` merge into one series (the first writer's metadata is kept).
    ///
    /// Values are admitted before anything else: non-finite values and the exact `±f64::MAX` sentinels are
    /// dropped, returning [`AddResult::series_ref`] as `None`. Same-second writes add into the existing
    /// bucket; out-of-order writes insert at the correct index. Point-retention and point-cap trimming are
    /// only applied when a new bucket is inserted, not on a same-second merge.
    pub fn add(
        &mut self, namespace: &str, name: &str, host: Option<&str>, value: f64, timestamp_sec: i64, tags: &[String],
    ) -> AddResult {
        if !value.is_finite() || value == f64::MAX || value == -f64::MAX {
            return AddResult {
                is_new: false,
                series_ref: None,
            };
        }

        let key = identity::storage_key(namespace, name, host.unwrap_or(""), tags);
        let (series_ref, is_new) = match self.keys.get(&key).copied() {
            Some(series_ref) => (series_ref, false),
            None => {
                let series_ref = self.refs.allocate();
                let state = SeriesState {
                    namespace: NamespaceId::new(namespace),
                    name: name.to_string(),
                    host: host.map(str::to_string),
                    tags: tags.to_vec(),
                    storage_key: key,
                    series_ref,
                    context: None,
                    supported_aggregations: 0,
                    retention_override_secs: 0,
                    last_activity_secs: timestamp_sec,
                    write_generation: 0,
                    buckets: Vec::new(),
                    counts: None,
                };
                self.keys.insert(key, series_ref);
                self.by_ref.insert(series_ref, state);
                if namespace != TELEMETRY_NAMESPACE {
                    self.live_series_count += 1;
                }
                self.series_gen += 1;
                (series_ref, true)
            }
        };

        let state = self
            .by_ref
            .get_mut(&series_ref)
            .expect("series exists for the key that resolved to it");
        state.write_generation += 1;
        if state.buckets.is_empty() || timestamp_sec > state.last_activity_secs {
            state.last_activity_secs = timestamp_sec;
        }

        let index = state.buckets.partition_point(|bucket| bucket.second < timestamp_sec);
        if index < state.buckets.len() && state.buckets[index].second == timestamp_sec {
            state.buckets[index].sum += value;
            state.increment_count(index);
            return AddResult {
                is_new,
                series_ref: Some(series_ref),
            };
        }

        state.buckets.insert(
            index,
            PointBucket {
                second: timestamp_sec,
                sum: value,
            },
        );
        state.insert_count(index);

        trim_points(&self.cfg, state);
        AddResult {
            is_new,
            series_ref: Some(series_ref),
        }
    }

    /// Records that an observation occurred at `timestamp_sec`, even if it produced no metric.
    ///
    /// This keeps log-only timestamps visible to the replay timeline ([`Self::data_timestamps`]); the set is
    /// never pruned by retention or eviction.
    pub fn record_observation_time(&mut self, timestamp_sec: i64) {
        self.observation_timestamps.insert(timestamp_sec);
    }

    /// Attaches or overwrites the [`MetricContext`] for a series.
    ///
    /// Readers receive a value snapshot ([`StorageView::get_context`]); a no-op for refs that are not live.
    pub fn set_context(&mut self, series_ref: SeriesRef, context: MetricContext) {
        if let Some(state) = self.by_ref.get_mut(&series_ref) {
            state.context = Some(context);
        }
    }

    /// Restricts which aggregates a series supports.
    ///
    /// An empty slice restores the default of supporting every aggregate. A no-op for refs that are not
    /// live.
    pub fn set_supported_aggregations(&mut self, series_ref: SeriesRef, aggregates: &[Aggregate]) {
        if let Some(state) = self.by_ref.get_mut(&series_ref) {
            state.supported_aggregations = aggregates
                .iter()
                .fold(0, |mask, aggregate| mask | aggregate_mask(*aggregate));
        }
    }

    /// Overrides point retention for one series. Zero (or a negative value) restores the store-wide value.
    pub fn set_series_retention(&mut self, series_ref: SeriesRef, retention_secs: i64) {
        if let Some(state) = self.by_ref.get_mut(&series_ref) {
            state.retention_override_secs = std::cmp::max(retention_secs, 0);
        }
    }

    /// Overrides the timestamp used to rank a series for capacity eviction.
    ///
    /// Materialised log-count series use the last real log time so synthetic zero buckets do not keep an
    /// idle series artificially hot.
    pub fn set_series_activity_timestamp(&mut self, series_ref: SeriesRef, timestamp_sec: i64) {
        if let Some(state) = self.by_ref.get_mut(&series_ref) {
            state.last_activity_secs = timestamp_sec;
        }
    }

    /// Removes series by ref, returning the refs actually freed.
    ///
    /// Unknown or already-removed refs are silently skipped. Refs are never reused. The series generation
    /// is bumped when at least one series was removed, so cached listings are invalidated.
    pub fn remove_series_by_refs(&mut self, refs: &[SeriesRef]) -> Vec<SeriesRef> {
        if refs.is_empty() {
            return Vec::new();
        }
        let mut removed = Vec::new();
        for &series_ref in refs {
            if self.remove_series(series_ref) {
                removed.push(series_ref);
            }
        }
        if !removed.is_empty() {
            self.series_gen += 1;
        }
        removed
    }

    /// Removes every series in `namespace` whose metric name equals `name`, returning the freed refs.
    ///
    /// This is used when a log-pattern cluster is evicted: the cluster identity is the deterministic
    /// `(namespace, name)` pair, so all tag variants are removed at once. An empty `name` removes nothing.
    pub fn remove_series_by_metric_name(&mut self, namespace: &str, name: &str) -> Vec<SeriesRef> {
        if name.is_empty() {
            return Vec::new();
        }
        let matching: Vec<SeriesRef> = self
            .by_ref
            .iter()
            .filter(|(_, state)| state.namespace.as_str() == namespace && state.name == name)
            .map(|(series_ref, _)| *series_ref)
            .collect();

        let removed = self.remove_refs(&matching);
        if !removed.is_empty() {
            self.series_gen += 1;
        }
        removed
    }

    /// Evicts the oldest series when the live count exceeds `series_limit`, draining to `target`.
    ///
    /// The trigger and the floor are computed from the **non-telemetry** count, but the candidate set
    /// includes every series, telemetry included. Candidates are ordered by `(last-activity, ref)`, so
    /// ties break on the lowest ref. Returns the freed refs. A `series_limit` of `0` disables eviction.
    pub fn evict_to_capacity(&mut self, series_limit: usize, target: usize) -> Vec<SeriesRef> {
        if series_limit == 0 || self.live_series_count <= series_limit {
            return Vec::new();
        }
        let excess = self.live_series_count.saturating_sub(target);
        if excess == 0 {
            return Vec::new();
        }

        let mut candidates: Vec<(i64, SeriesRef)> = self
            .by_ref
            .iter()
            .map(|(series_ref, state)| (state.last_activity_secs, *series_ref))
            .collect();
        candidates.sort_unstable();

        let doomed: Vec<SeriesRef> = candidates
            .iter()
            .take(excess.min(candidates.len()))
            .map(|(_, series_ref)| *series_ref)
            .collect();

        let freed = self.remove_refs(&doomed);
        if !freed.is_empty() {
            self.series_gen += 1;
        }
        freed
    }

    /// Evicts to capacity using the store's own configuration.
    ///
    /// The floor is `max_series * (1 - eviction_floor_ratio)`, so the default configuration drains a full
    /// store to 50% of its cap. A `max_series` of `0` disables eviction.
    pub fn evict_default(&mut self) -> Vec<SeriesRef> {
        if self.cfg.max_series == 0 {
            return Vec::new();
        }
        let band = (self.cfg.max_series as f64 * self.cfg.eviction_floor_ratio) as usize;
        let target = self.cfg.max_series.saturating_sub(band);
        self.evict_to_capacity(self.cfg.max_series, target)
    }

    /// Removes non-telemetry series whose last activity is at or before `cutoff`.
    ///
    /// The comparison is inclusive, so a series exactly at the cutoff is removed. Telemetry series are
    /// never removed by inactivity. Returns the freed refs.
    pub fn evict_inactive_before(&mut self, cutoff: i64) -> Vec<SeriesRef> {
        let doomed: Vec<SeriesRef> = self
            .by_ref
            .iter()
            .filter(|(_, state)| state.namespace.as_str() != TELEMETRY_NAMESPACE && state.last_activity_secs <= cutoff)
            .map(|(series_ref, _)| *series_ref)
            .collect();

        let freed = self.remove_refs(&doomed);
        if !freed.is_empty() {
            self.series_gen += 1;
        }
        freed
    }

    /// Removes a batch of refs, returning those that were live.
    fn remove_refs(&mut self, refs: &[SeriesRef]) -> Vec<SeriesRef> {
        let mut removed = Vec::new();
        for &series_ref in refs {
            if self.remove_series(series_ref) {
                removed.push(series_ref);
            }
        }
        removed
    }

    /// Removes a single live series from every index and the cardinality counter.
    fn remove_series(&mut self, series_ref: SeriesRef) -> bool {
        let Some(state) = self.by_ref.remove(&series_ref) else {
            return false;
        };
        if self.keys.get(&state.storage_key) == Some(&series_ref) {
            self.keys.remove(&state.storage_key);
        }
        if state.namespace.as_str() != TELEMETRY_NAMESPACE {
            self.live_series_count -= 1;
        }
        true
    }

    /// Returns the set of namespaces that currently hold data, sorted.
    pub fn namespaces(&self) -> Vec<String> {
        let namespaces: BTreeSet<&str> = self.by_ref.values().map(|state| state.namespace.as_str()).collect();
        namespaces.into_iter().map(str::to_string).collect()
    }

    /// Returns every timestamp that has data or a recorded observation, sorted ascending.
    ///
    /// This merges the stored bucket seconds with the persisted observation timestamps, so a log that
    /// produced no metric still appears in the replay timeline.
    pub fn data_timestamps(&self) -> Vec<i64> {
        let mut timestamps: BTreeSet<i64> = BTreeSet::new();
        for state in self.by_ref.values() {
            for bucket in &state.buckets {
                timestamps.insert(bucket.second);
            }
        }
        timestamps.extend(self.observation_timestamps.iter().copied());
        timestamps.into_iter().collect()
    }

    /// Returns the number of live non-telemetry series.
    pub fn total_series_count(&self) -> usize {
        self.live_series_count
    }

    /// Returns the total number of stored samples, optionally excluding one namespace.
    pub fn total_sample_count(&self, exclude_namespace: Option<&str>) -> u64 {
        self.by_ref
            .values()
            .filter(|state| exclude_namespace != Some(state.namespace.as_str()))
            .map(SeriesState::sample_count)
            .sum()
    }

    /// Returns metadata for every live series matching `filter`, ordered by ref.
    pub fn list_series_filtered(&self, filter: &SeriesFilter) -> Vec<SeriesMeta> {
        self.by_ref
            .values()
            .filter(|state| matches_filter(state, filter))
            .map(SeriesState::meta)
            .collect()
    }

    /// Returns the refs of every live series matching `filter`, ordered by ref.
    ///
    /// This is the allocation-light listing used by detectors that only need the stable numeric handles.
    pub fn list_series_refs(&self, filter: &SeriesFilter) -> Vec<SeriesRef> {
        self.by_ref
            .values()
            .filter(|state| matches_filter(state, filter))
            .map(|state| state.series_ref)
            .collect()
    }

    /// Returns the storage key assigned to a series at ingestion time.
    pub fn series_storage_key(&self, series_ref: SeriesRef) -> Option<u64> {
        self.by_ref.get(&series_ref).map(|state| state.storage_key)
    }

    /// Returns the effective point-retention window for a series, in seconds.
    ///
    /// Missing series fall back to the store-wide value, which is also the fallback for anomalies that are
    /// not storage-backed.
    pub fn point_retention_for_series(&self, series_ref: SeriesRef) -> i64 {
        match self.by_ref.get(&series_ref) {
            Some(state) if state.retention_override_secs > 0 => state.retention_override_secs,
            _ => self.cfg.point_retention_secs,
        }
    }

    /// Returns whether a series allows the given aggregate.
    ///
    /// An unknown ref reports `true`: detectors must tolerate series that were evicted between passes.
    pub fn supports_aggregate(&self, series_ref: SeriesRef, aggregate: Aggregate) -> bool {
        match self.by_ref.get(&series_ref) {
            None => true,
            Some(state) => state.allows_aggregate(aggregate),
        }
    }

    /// Returns the aggregate total over `(start_sec, end_sec]`, without allocating intermediate points.
    ///
    /// Returns `0` when the series is not live or the range is empty.
    pub fn sum_range(&self, series_ref: SeriesRef, start_sec: i64, end_sec: i64, aggregate: Aggregate) -> f64 {
        let Some(state) = self.by_ref.get(&series_ref) else {
            return 0.0;
        };
        let lo = state.buckets.partition_point(|bucket| bucket.second <= start_sec);
        let hi = state.buckets.partition_point(|bucket| bucket.second <= end_sec);
        (lo..hi).map(|index| state.aggregate_at(index, aggregate)).sum()
    }

    /// Returns up to `n` of the newest points with timestamp `<= end_sec`, in time order.
    ///
    /// Returns `None` when `n` is `0` or the series is not live.
    pub fn last_points(
        &self, series_ref: SeriesRef, end_sec: i64, n: usize, aggregate: Aggregate,
    ) -> Option<Vec<Point>> {
        if n == 0 {
            return None;
        }
        let state = self.by_ref.get(&series_ref)?;
        let end_index = state.buckets.partition_point(|bucket| bucket.second <= end_sec);
        let start_index = end_index.saturating_sub(n);
        Some(
            (start_index..end_index)
                .map(|index| Point {
                    second: state.buckets[index].second,
                    value: state.aggregate_at(index, aggregate),
                })
                .collect(),
        )
    }

    /// Returns a materialised series view over `(start_sec, end_sec]` for the requested aggregate.
    fn series_view(&self, state: &SeriesState, start_sec: i64, end_sec: i64, aggregate: Aggregate) -> Series {
        let lo = state.buckets.partition_point(|bucket| bucket.second <= start_sec);
        let hi = state.buckets.partition_point(|bucket| bucket.second <= end_sec);
        let points = (lo..hi)
            .map(|index| Point {
                second: state.buckets[index].second,
                value: state.aggregate_at(index, aggregate),
            })
            .collect();
        Series {
            namespace: state.namespace.clone(),
            name: state.name.clone(),
            host: state.host.clone(),
            tags: state.tags.clone(),
            points,
        }
    }
}

impl SeriesState {
    /// Builds the metadata snapshot for this series.
    fn meta(&self) -> SeriesMeta {
        SeriesMeta {
            series_ref: self.series_ref,
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            tags: self.tags.clone(),
        }
    }
}

impl Default for TimeSeriesStorage {
    /// Creates an empty store with the production [`StorageConfig::default`].
    fn default() -> Self {
        Self::new(StorageConfig::default())
    }
}

impl StorageView for TimeSeriesStorage {
    fn list_series(&self, namespace: Option<&str>) -> Vec<SeriesMeta> {
        self.by_ref
            .values()
            .filter(|state| namespace.is_none_or(|namespace| state.namespace.as_str() == namespace))
            .map(SeriesState::meta)
            .collect()
    }

    fn series_meta(&self, series: SeriesRef) -> Option<SeriesMeta> {
        self.by_ref.get(&series).map(SeriesState::meta)
    }

    fn get_context(&self, series: SeriesRef) -> Option<MetricContext> {
        self.by_ref.get(&series).and_then(|state| state.context.clone())
    }

    fn get_series_range(
        &self, series: SeriesRef, start_sec: i64, end_sec: i64, aggregate: Aggregate,
    ) -> Option<Series> {
        let state = self.by_ref.get(&series)?;
        Some(self.series_view(state, start_sec, end_sec, aggregate))
    }

    fn point_count_up_to(&self, series: SeriesRef, end_sec: i64) -> usize {
        match self.by_ref.get(&series) {
            None => 0,
            Some(state) => state.buckets.partition_point(|bucket| bucket.second <= end_sec),
        }
    }

    fn write_generation(&self, series: SeriesRef) -> u64 {
        self.by_ref.get(&series).map_or(0, |state| state.write_generation)
    }

    fn series_generation(&self) -> u64 {
        self.series_gen
    }
}

/// Returns whether a series matches a filter.
fn matches_filter(state: &SeriesState, filter: &SeriesFilter) -> bool {
    match &filter.namespace {
        Some(namespace) => {
            if state.namespace.as_str() != namespace {
                return false;
            }
        }
        None => {
            if filter
                .exclude_namespaces
                .iter()
                .any(|namespace| state.namespace.as_str() == namespace)
            {
                return false;
            }
        }
    }
    if let Some(prefix) = &filter.name_prefix {
        if !state.name.starts_with(prefix) {
            return false;
        }
    }
    if let Some(host) = &filter.host {
        if state.host.as_deref() != Some(host.as_str()) {
            return false;
        }
    }
    tags_match(&state.tags, &filter.tag_matchers)
}

/// Returns whether `tags` contain every required `key:value` matcher.
///
/// A tag participates in matching only when its first `:` is not at the start (`key:value`); later duplicate
/// keys overwrite earlier ones, matching the Go matcher.
fn tags_match(tags: &[String], matchers: &BTreeMap<String, String>) -> bool {
    if matchers.is_empty() {
        return true;
    }
    let mut parsed: BTreeMap<&str, &str> = BTreeMap::new();
    for tag in tags {
        if let Some(index) = tag.find(':') {
            if index > 0 {
                parsed.insert(&tag[..index], &tag[index + 1..]);
            }
        }
    }
    matchers
        .iter()
        .all(|(key, value)| parsed.get(key.as_str()) == Some(&value.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::StorageView;

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn store() -> TimeSeriesStorage {
        TimeSeriesStorage::default()
    }

    /// Adds a sample and unwraps the assigned ref, failing on a dropped value.
    fn add(storage: &mut TimeSeriesStorage, value: f64, second: i64) -> SeriesRef {
        storage
            .add("test", "my.metric", None, value, second, &[])
            .series_ref
            .expect("sample was accepted")
    }

    #[test]
    fn add_stores_point_and_metadata() {
        let mut storage = store();
        let series_ref = storage
            .add("test", "my.metric", None, 10.0, 1000, &tags(&["env:prod"]))
            .series_ref
            .expect("accepted");

        let meta = storage.series_meta(series_ref).expect("series is live");
        assert_eq!(meta.namespace.as_str(), "test");
        assert_eq!(meta.name, "my.metric");
        assert_eq!(meta.host, None);
        assert_eq!(meta.tags, tags(&["env:prod"]));

        let series = storage
            .get_series_range(series_ref, i64::MIN, 2000, Aggregate::Average)
            .expect("series is live");
        assert_eq!(
            series.points,
            vec![Point {
                second: 1000,
                value: 10.0
            }]
        );
    }

    #[test]
    fn add_reports_new_series_and_stable_ref() {
        let mut storage = store();
        let first = storage.add("ns", "m", None, 1.0, 1000, &tags(&["env:prod"]));
        let second = storage.add("ns", "m", None, 2.0, 1001, &tags(&["env:prod"]));

        assert!(first.is_new);
        assert!(!second.is_new);
        assert_eq!(first.series_ref, second.series_ref);
        assert_eq!(storage.total_series_count(), 1);
    }

    #[test]
    fn same_second_merge_adds_sum_and_increments_count() {
        let mut storage = store();
        let series_ref = add(&mut storage, 10.0, 1000);
        add(&mut storage, 20.0, 1000);
        add(&mut storage, 5.0, 1000);

        // sum === 35, count === 3, avg === 35 / 3.
        assert_eq!(
            storage
                .get_series_range(series_ref, 0, 1000, Aggregate::Sum)
                .unwrap()
                .points,
            vec![Point {
                second: 1000,
                value: 35.0
            }]
        );
        assert_eq!(
            storage
                .get_series_range(series_ref, 0, 1000, Aggregate::Count)
                .unwrap()
                .points,
            vec![Point {
                second: 1000,
                value: 3.0
            }]
        );
        let avg = storage
            .get_series_range(series_ref, 0, 1000, Aggregate::Average)
            .unwrap()
            .points[0]
            .value;
        assert!((avg - 11.666_666).abs() < 1e-4);
    }

    #[test]
    fn unit_counts_stay_implicit() {
        let mut storage = store();
        let series_ref = add(&mut storage, 10.0, 1000);
        add(&mut storage, 20.0, 1001);

        let state = storage.by_ref.get(&series_ref).expect("live");
        assert!(state.counts.is_none(), "no same-second merge => no count vector");
        assert_eq!(state.sample_count(), 2);
    }

    #[test]
    fn explicit_counts_stay_aligned_through_insert_and_trim() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            max_points_per_series: 2,
            ..StorageConfig::default()
        });
        let series_ref = add(&mut storage, 30.0, 1002);
        add(&mut storage, 10.0, 1000);
        add(&mut storage, 20.0, 1001);
        add(&mut storage, 40.0, 1002);
        add(&mut storage, 50.0, 1003);

        let state = storage.by_ref.get(&series_ref).expect("live");
        assert_eq!(state.counts.as_deref(), Some(&[1u64, 2, 1][..]));

        assert_eq!(
            storage
                .get_series_range(series_ref, 0, 1003, Aggregate::Average)
                .unwrap()
                .points,
            vec![
                Point {
                    second: 1001,
                    value: 20.0
                },
                Point {
                    second: 1002,
                    value: 35.0
                },
                Point {
                    second: 1003,
                    value: 50.0
                },
            ]
        );
    }

    #[test]
    fn out_of_order_and_duplicate_writes_are_sorted_and_merged() {
        let mut storage = store();
        let series_ref = add(&mut storage, 10.0, 1000);
        add(&mut storage, 20.0, 1002);
        add(&mut storage, 30.0, 1001);
        add(&mut storage, 40.0, 1002);

        let points = storage
            .get_series_range(series_ref, 0, 2000, Aggregate::Average)
            .unwrap()
            .points;
        assert_eq!(
            points,
            vec![
                Point {
                    second: 1000,
                    value: 10.0
                },
                Point {
                    second: 1001,
                    value: 30.0
                },
                Point {
                    second: 1002,
                    value: 30.0
                },
            ]
        );
    }

    #[test]
    fn range_is_start_exclusive_end_inclusive() {
        let mut storage = store();
        let series_ref = add(&mut storage, 10.0, 10);
        for second in [20, 30, 40, 50] {
            add(&mut storage, second as f64, second);
        }

        let points = storage
            .get_series_range(series_ref, 20, 40, Aggregate::Sum)
            .unwrap()
            .points;
        assert_eq!(
            points.iter().map(|point| point.second).collect::<Vec<_>>(),
            vec![30, 40]
        );

        let exact = storage
            .get_series_range(series_ref, 10, 50, Aggregate::Sum)
            .unwrap()
            .points;
        assert_eq!(
            exact.iter().map(|point| point.second).collect::<Vec<_>>(),
            vec![20, 30, 40, 50]
        );

        let zero_start = storage
            .get_series_range(series_ref, 0, 999, Aggregate::Sum)
            .unwrap()
            .points;
        assert_eq!(zero_start.len(), 5);
    }

    #[test]
    fn range_boundaries_exclude_endpoints_and_empty_overlap() {
        let mut storage = store();
        let series_ref = add(&mut storage, 42.0, 100);

        // start == timestamp is exclusive.
        assert!(storage
            .get_series_range(series_ref, 100, 200, Aggregate::Sum)
            .unwrap()
            .points
            .is_empty());
        // end before the point.
        assert!(storage
            .get_series_range(series_ref, 0, 99, Aggregate::Sum)
            .unwrap()
            .points
            .is_empty());
        // range entirely after data.
        assert!(storage
            .get_series_range(series_ref, 100, 100, Aggregate::Sum)
            .unwrap()
            .points
            .is_empty());
    }

    #[test]
    fn range_across_all_aggregates() {
        let mut storage = store();
        let series_ref = add(&mut storage, 10.0, 100);
        add(&mut storage, 20.0, 100);

        let expected = [
            (Aggregate::Sum, 30.0),
            (Aggregate::Count, 2.0),
            (Aggregate::Average, 15.0),
            // The Go zero-value aggregate reads 0.
            (Aggregate::None, 0.0),
        ];
        for (aggregate, value) in expected {
            let points = storage.get_series_range(series_ref, 0, 200, aggregate).unwrap().points;
            assert_eq!(points, vec![Point { second: 100, value }]);
        }
    }

    #[test]
    fn aggregate_at_zero_count_reads_zero() {
        // A bucket can only legally hold zero samples if a count vector is crafted directly; this pins the
        // contract that the average of such a bucket reads 0 rather than NaN/inf.
        let state = SeriesState {
            namespace: NamespaceId::new("ns"),
            name: "m".to_string(),
            host: None,
            tags: Vec::new(),
            storage_key: 0,
            series_ref: SeriesRef::new(0),
            context: None,
            supported_aggregations: 0,
            retention_override_secs: 0,
            last_activity_secs: 0,
            write_generation: 0,
            buckets: vec![PointBucket { second: 1, sum: 10.0 }],
            counts: Some(vec![0]),
        };
        assert_eq!(state.aggregate_at(0, Aggregate::Average), 0.0);
        assert_eq!(state.aggregate_at(0, Aggregate::Sum), 10.0);
        assert_eq!(state.aggregate_at(0, Aggregate::Count), 0.0);
    }

    #[test]
    fn non_finite_and_extreme_values_are_dropped() {
        let mut storage = store();
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, -f64::MAX] {
            let result = storage.add("ns", "m", None, value, 1000, &[]);
            assert!(!result.is_new);
            assert_eq!(result.series_ref, None, "{value} must be dropped");
        }
        assert_eq!(storage.total_series_count(), 0);

        // A large-but-ordinary finite value is accepted.
        let accepted = storage.add("ns", "m", None, f64::MAX / 4.0, 1000, &[]);
        assert!(accepted.series_ref.is_some());
        assert_eq!(storage.total_series_count(), 1);
    }

    #[test]
    fn point_retention_trims_old_buckets() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            point_retention_secs: 3,
            ..StorageConfig::default()
        });
        let series_ref = add(&mut storage, 1.0, 1);
        for second in 2..=6 {
            add(&mut storage, second as f64, second);
        }

        let points = storage
            .get_series_range(series_ref, 0, 100, Aggregate::Average)
            .unwrap()
            .points;
        assert_eq!(
            points
                .iter()
                .map(|point| (point.second, point.value))
                .collect::<Vec<_>>(),
            vec![(3, 3.0), (4, 4.0), (5, 5.0), (6, 6.0)]
        );
    }

    #[test]
    fn point_retention_uses_series_latest_not_incoming_timestamp() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            point_retention_secs: 3,
            ..StorageConfig::default()
        });
        let series_ref = add(&mut storage, 1.0, 10);
        // Backfilled point far outside the window relative to the series' latest timestamp.
        add(&mut storage, 2.0, 5);

        let points = storage
            .get_series_range(series_ref, 0, 100, Aggregate::Sum)
            .unwrap()
            .points;
        assert_eq!(points, vec![Point { second: 10, value: 1.0 }]);
    }

    #[test]
    fn point_retention_override_applies_per_series() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            point_retention_secs: 60,
            ..StorageConfig::default()
        });
        let series_ref = add(&mut storage, 1.0, 1);
        storage.set_series_retention(series_ref, 2);
        for second in 2..=5 {
            add(&mut storage, second as f64, second);
        }

        assert_eq!(storage.point_retention_for_series(series_ref), 2);
        let points = storage
            .get_series_range(series_ref, 0, 100, Aggregate::Average)
            .unwrap()
            .points;
        // Keep buckets with `second >= latest - 2` == 3.
        assert_eq!(
            points.iter().map(|point| point.second).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn max_points_retains_pending_bucket() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            max_points_per_series: 3,
            ..StorageConfig::default()
        });
        let series_ref = add(&mut storage, 1.0, 1);
        for second in 2..=6 {
            add(&mut storage, second as f64, second);
        }

        let points = storage
            .get_series_range(series_ref, 0, 100, Aggregate::Average)
            .unwrap()
            .points;
        assert_eq!(
            points.iter().map(|point| point.second).collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );
        assert_eq!(storage.point_count_up_to(series_ref, 5), 3);
    }

    #[test]
    fn point_cap_and_duration_retention_combine() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            point_retention_secs: 3,
            max_points_per_series: 2,
            ..StorageConfig::default()
        });
        let series_ref = add(&mut storage, 1.0, 1);
        for second in 2..=6 {
            add(&mut storage, second as f64, second);
        }

        let points = storage
            .get_series_range(series_ref, 0, 100, Aggregate::Average)
            .unwrap()
            .points;
        assert_eq!(
            points.iter().map(|point| point.second).collect::<Vec<_>>(),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn capacity_eviction_drains_to_floor_in_activity_order() {
        let mut storage = TimeSeriesStorage::new(StorageConfig {
            max_series: 10,
            eviction_floor_ratio: 0.5,
            ..StorageConfig::default()
        });
        for i in 0..11 {
            storage.add("workload", &format!("m{i}"), None, 1.0, i as i64, &[]);
        }
        assert_eq!(storage.total_series_count(), 11);

        let freed = storage.evict_default();
        // Drains from 11 to the floor of 10 * (1 - 0.5) = 5, freeing the 6 oldest.
        assert_eq!(freed.len(), 6);
        assert_eq!(storage.total_series_count(), 5);
        // The six oldest (refs 0..=5) were evicted; the newest survive.
        for i in 0..6 {
            assert!(storage.series_meta(SeriesRef::new(i)).is_none(), "ref {i} evicted");
        }
        for i in 6..11 {
            assert!(storage.series_meta(SeriesRef::new(i)).is_some(), "ref {i} survives");
        }
    }

    #[test]
    fn capacity_eviction_does_not_fire_at_limit_and_breaks_ties_by_lowest_ref() {
        let mut storage = store();
        let first = storage.add("test", "m1", None, 1.0, 100, &[]).series_ref.unwrap();
        let second = storage.add("test", "m2", None, 1.0, 100, &[]).series_ref.unwrap();
        let third = storage.add("test", "m3", None, 1.0, 100, &[]).series_ref.unwrap();

        // At exactly the limit, nothing is evicted.
        assert!(storage.evict_to_capacity(3, 3).is_empty());

        // All three have identical activity; the lowest ref goes first.
        let freed = storage.evict_to_capacity(2, 2);
        assert_eq!(freed, vec![first]);
        assert!(storage.series_meta(first).is_none());
        assert!(storage.series_meta(second).is_some());
        assert!(storage.series_meta(third).is_some());
    }

    #[test]
    fn capacity_eviction_candidates_include_telemetry() {
        let mut storage = store();
        // The telemetry series is the oldest and is not counted toward the trigger...
        let telemetry = storage
            .add(TELEMETRY_NAMESPACE, "chart", None, 1.0, 1, &[])
            .series_ref
            .unwrap();
        storage.add("test", "old", None, 1.0, 2, &[]);
        storage.add("test", "new", None, 1.0, 3, &[]);
        assert_eq!(
            storage.total_series_count(),
            2,
            "telemetry is not counted toward cardinality"
        );

        // ... but it is a valid candidate once eviction fires, and it has the oldest activity.
        let freed = storage.evict_to_capacity(1, 1);
        assert_eq!(freed, vec![telemetry]);
        assert!(storage.series_meta(telemetry).is_none());
        // Removing telemetry does not reduce the non-telemetry count.
        assert_eq!(storage.total_series_count(), 2);
    }

    #[test]
    fn inactivity_eviction_is_inclusive_and_spares_telemetry() {
        let mut storage = store();
        let old = storage
            .add("workload", "old", None, 1.0, 100, &tags(&["env:test"]))
            .series_ref
            .unwrap();
        let exact = storage
            .add("workload", "exact", None, 1.0, 300, &[])
            .series_ref
            .unwrap();
        let newer = storage
            .add("workload", "newer", None, 1.0, 301, &[])
            .series_ref
            .unwrap();
        let telemetry = storage
            .add(TELEMETRY_NAMESPACE, "internal", None, 1.0, 100, &[])
            .series_ref
            .unwrap();
        let generation_before = storage.series_generation();

        let freed = storage.evict_inactive_before(300);

        // Inclusive cutoff: a series exactly at 300 is removed.
        assert_eq!(freed.len(), 2);
        assert!(freed.contains(&old));
        assert!(freed.contains(&exact));
        assert!(storage.series_meta(newer).is_some());
        assert!(storage.series_meta(telemetry).is_some(), "telemetry is spared");
        assert_eq!(storage.total_series_count(), 1);
        assert!(storage.series_generation() > generation_before);
    }

    #[test]
    fn activity_override_ranks_series_for_capacity_eviction() {
        let mut storage = store();
        let stale = storage.add("test", "stale", None, 1.0, 1000, &[]).series_ref.unwrap();
        let fresh = storage.add("test", "fresh", None, 1.0, 100, &[]).series_ref.unwrap();

        // The "fresh" series has an old stored timestamp but is declared active.
        storage.set_series_activity_timestamp(fresh, 10_000);

        let freed = storage.evict_to_capacity(1, 1);
        assert_eq!(freed, vec![stale]);
        assert!(storage.series_meta(fresh).is_some());
    }

    #[test]
    fn remove_series_by_refs_returns_freed_refs_and_bumps_generation() {
        let mut storage = store();
        let a = storage
            .add("ns", "a", None, 1.0, 1000, &tags(&["k:1"]))
            .series_ref
            .unwrap();
        let b = storage
            .add("ns", "b", None, 2.0, 1000, &tags(&["k:2"]))
            .series_ref
            .unwrap();
        let c = storage
            .add("ns", "c", None, 3.0, 1000, &tags(&["k:3"]))
            .series_ref
            .unwrap();
        let generation_before = storage.series_generation();

        let removed = storage.remove_series_by_refs(&[b, c, SeriesRef::new(999)]);

        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&b) && removed.contains(&c));
        assert_eq!(storage.total_series_count(), 1);
        assert!(storage.series_generation() > generation_before);
        assert!(storage.series_meta(b).is_none());
        assert!(storage.series_meta(a).is_some());
    }

    #[test]
    fn removal_with_no_matches_does_not_bump_generation() {
        let mut storage = store();
        add(&mut storage, 1.0, 1000);
        let generation_before = storage.series_generation();

        assert!(storage.remove_series_by_refs(&[]).is_empty());
        assert!(storage.remove_series_by_refs(&[SeriesRef::new(999)]).is_empty());
        assert!(storage.remove_series_by_metric_name("ns", "").is_empty());
        assert_eq!(storage.series_generation(), generation_before);
    }

    #[test]
    fn remove_series_by_metric_name_removes_all_tag_variants() {
        let mut storage = store();
        let prod = storage
            .add("logs", "pattern.count", None, 1.0, 1000, &tags(&["env:prod"]))
            .series_ref
            .unwrap();
        let staging = storage
            .add("logs", "pattern.count", None, 1.0, 1000, &tags(&["env:staging"]))
            .series_ref
            .unwrap();
        let other = storage
            .add("logs", "other.count", None, 1.0, 1000, &tags(&["env:prod"]))
            .series_ref
            .unwrap();

        let mut removed = storage.remove_series_by_metric_name("logs", "pattern.count");
        removed.sort_unstable();

        assert_eq!(removed, vec![prod, staging]);
        assert!(storage.series_meta(other).is_some());
        assert_eq!(storage.total_series_count(), 1);
    }

    #[test]
    fn removed_refs_are_never_reused() {
        let mut storage = store();
        let first = add(&mut storage, 1.0, 1000);
        assert_eq!(storage.remove_series_by_refs(&[first]), vec![first]);

        let readded = storage
            .add("test", "my.metric", None, 2.0, 1100, &[])
            .series_ref
            .unwrap();
        assert_ne!(first, readded);
        assert!(storage.series_meta(first).is_none());
        assert!(storage.series_meta(readded).is_some());
    }

    #[test]
    fn observation_time_is_recorded_and_never_pruned() {
        let mut storage = store();
        storage.record_observation_time(500);
        // A later metric with retention that would trim 500 does not remove the observation timestamp.
        let series_ref = add(&mut storage, 1.0, 100_000);
        storage.set_series_retention(series_ref, 1);

        let timestamps = storage.data_timestamps();
        assert!(timestamps.contains(&500));
        assert!(timestamps.contains(&100_000));
    }

    #[test]
    fn data_timestamps_unions_buckets_and_observations_in_order() {
        let mut storage = store();
        storage.record_observation_time(20);
        add(&mut storage, 1.0, 10);
        add(&mut storage, 1.0, 30);

        assert_eq!(storage.data_timestamps(), vec![10, 20, 30]);
    }

    #[test]
    fn point_count_up_to_uses_binary_search() {
        let mut storage = store();
        let series_ref = add(&mut storage, 1.0, 10);
        for second in [20, 30, 40, 50] {
            add(&mut storage, 1.0, second);
        }

        assert_eq!(storage.point_count_up_to(series_ref, 50), 5);
        assert_eq!(storage.point_count_up_to(series_ref, 30), 3);
        assert_eq!(storage.point_count_up_to(series_ref, 25), 2);
        assert_eq!(storage.point_count_up_to(series_ref, 9), 0);
        assert_eq!(storage.point_count_up_to(SeriesRef::new(999), 100), 0);
    }

    #[test]
    fn last_points_returns_a_bounded_tail() {
        let mut storage = store();
        let series_ref = add(&mut storage, 1.0, 1);
        for second in 2..=5 {
            add(&mut storage, second as f64, second);
        }

        let points = storage.last_points(series_ref, 4, 3, Aggregate::Average).unwrap();
        assert_eq!(
            points,
            vec![
                Point { second: 2, value: 2.0 },
                Point { second: 3, value: 3.0 },
                Point { second: 4, value: 4.0 },
            ]
        );
    }

    #[test]
    fn last_points_handles_zero_and_unknown() {
        let mut storage = store();
        let series_ref = add(&mut storage, 1.0, 10);
        add(&mut storage, 2.0, 20);

        assert!(storage.last_points(series_ref, 20, 0, Aggregate::Average).is_none());
        assert!(storage
            .last_points(SeriesRef::new(999), 20, 5, Aggregate::Average)
            .is_none());
        let all = storage.last_points(series_ref, 100, 10, Aggregate::Average).unwrap();
        assert_eq!(all.iter().map(|point| point.second).collect::<Vec<_>>(), vec![10, 20]);
    }

    #[test]
    fn sum_range_matches_explicit_range() {
        let mut storage = store();
        let series_ref = add(&mut storage, 10.0, 10);
        for second in [20, 30, 40] {
            add(&mut storage, second as f64, second);
        }

        assert_eq!(storage.sum_range(series_ref, 10, 40, Aggregate::Sum), 90.0);
        assert_eq!(storage.sum_range(series_ref, 10, 40, Aggregate::Count), 3.0);
        // Each bucket holds a single sample, so the per-bucket averages sum to the same total.
        assert_eq!(storage.sum_range(series_ref, 10, 40, Aggregate::Average), 90.0);
        assert_eq!(storage.sum_range(series_ref, 40, 40, Aggregate::Sum), 0.0);
        assert_eq!(storage.sum_range(SeriesRef::new(999), 0, 1000, Aggregate::Sum), 0.0);
    }

    #[test]
    fn total_sample_count_excludes_a_namespace() {
        let mut storage = store();
        add(&mut storage, 1.0, 1000);
        add(&mut storage, 1.0, 1000); // same-second merge -> 2 samples
        storage.add(TELEMETRY_NAMESPACE, "chart", None, 1.0, 1000, &[]);

        assert_eq!(storage.total_sample_count(None), 3);
        assert_eq!(storage.total_sample_count(Some(TELEMETRY_NAMESPACE)), 2);
    }

    #[test]
    fn namespaces_are_sorted_and_unique() {
        let mut storage = store();
        storage.add("work", "a", None, 1.0, 1, &[]);
        storage.add("work", "b", None, 1.0, 1, &[]);
        storage.add(TELEMETRY_NAMESPACE, "c", None, 1.0, 1, &[]);

        assert_eq!(storage.namespaces(), vec!["telemetry".to_string(), "work".to_string()]);
    }

    #[test]
    fn list_series_filters_by_namespace_and_workload() {
        let mut storage = store();
        storage.add(TELEMETRY_NAMESPACE, "internal.gauge", None, 1.0, 1000, &[]);
        let work = storage.add("work", "cpu", None, 2.0, 1000, &[]).series_ref.unwrap();

        assert_eq!(storage.list_series(None).len(), 2);
        let telemetry = storage.list_series(Some(TELEMETRY_NAMESPACE));
        assert_eq!(telemetry.len(), 1);
        assert_eq!(telemetry[0].namespace.as_str(), TELEMETRY_NAMESPACE);

        let workload = storage.list_series_filtered(&SeriesFilter::workload());
        assert_eq!(workload.len(), 1);
        assert_eq!(workload[0].series_ref, work);
        assert_eq!(storage.list_series_refs(&SeriesFilter::workload()), vec![work]);
    }

    #[test]
    fn list_series_filter_matches_host_prefix_and_tags() {
        let mut storage = store();
        let prod_web = storage
            .add(
                "work",
                "cpu.usage",
                Some("web-1"),
                1.0,
                1000,
                &tags(&["env:prod", "service:web"]),
            )
            .series_ref
            .unwrap();
        storage
            .add(
                "work",
                "cpu.load",
                Some("web-1"),
                1.0,
                1000,
                &tags(&["env:prod", "service:api"]),
            )
            .series_ref
            .unwrap();
        storage
            .add("work", "mem.rss", None, 1.0, 1000, &tags(&["env:stage"]))
            .series_ref
            .unwrap();

        let filter = SeriesFilter {
            host: Some("web-1".to_string()),
            name_prefix: Some("cpu.".to_string()),
            tag_matchers: BTreeMap::from([("service".to_string(), "web".to_string())]),
            ..SeriesFilter::default()
        };
        assert_eq!(storage.list_series_refs(&filter), vec![prod_web]);
    }

    #[test]
    fn series_generation_bumps_only_when_the_set_changes() {
        let mut storage = store();
        let initial = storage.series_generation();
        let series_ref = add(&mut storage, 1.0, 1000);
        let after_insert = storage.series_generation();
        assert!(after_insert > initial);

        // Same-series writes do not change the set.
        add(&mut storage, 2.0, 1001);
        assert_eq!(storage.series_generation(), after_insert);

        storage.remove_series_by_refs(&[series_ref]);
        assert!(storage.series_generation() > after_insert);
    }

    #[test]
    fn write_generation_counts_every_write_including_merges() {
        let mut storage = store();
        let series_ref = add(&mut storage, 1.0, 1000);
        assert_eq!(storage.write_generation(series_ref), 1);
        add(&mut storage, 2.0, 1000);
        assert_eq!(storage.write_generation(series_ref), 2);
        add(&mut storage, 3.0, 1001);
        assert_eq!(storage.write_generation(series_ref), 3);
        assert_eq!(storage.write_generation(SeriesRef::new(999)), 0);
    }

    #[test]
    fn context_is_overwritten_and_readers_get_a_snapshot() {
        let mut storage = store();
        let series_ref = add(&mut storage, 1.0, 1);
        assert!(storage.get_context(series_ref).is_none());

        storage.set_context(
            series_ref,
            MetricContext {
                pattern: "first".to_string(),
                example: "first log".to_string(),
                ..MetricContext::default()
            },
        );
        let first = storage.get_context(series_ref).unwrap();

        storage.set_context(
            series_ref,
            MetricContext {
                pattern: "second".to_string(),
                example: "second log".to_string(),
                ..MetricContext::default()
            },
        );
        let second = storage.get_context(series_ref).unwrap();

        assert_eq!(first.pattern, "first");
        assert_eq!(second.pattern, "second");
        assert_eq!(second.example, "second log");

        storage.remove_series_by_refs(&[series_ref]);
        assert!(storage.get_context(series_ref).is_none());
    }

    #[test]
    fn supported_aggregations_mask_is_permissive_by_default() {
        let mut storage = store();
        let series_ref = add(&mut storage, 1.0, 1);
        assert!(storage.supports_aggregate(series_ref, Aggregate::Average));
        assert!(storage.supports_aggregate(series_ref, Aggregate::Sum));

        // Materialised log-count series declare average only.
        storage.set_supported_aggregations(series_ref, &[Aggregate::Average]);
        assert!(storage.supports_aggregate(series_ref, Aggregate::Average));
        assert!(!storage.supports_aggregate(series_ref, Aggregate::Sum));

        // An empty slice restores the default.
        storage.set_supported_aggregations(series_ref, &[]);
        assert!(storage.supports_aggregate(series_ref, Aggregate::Sum));

        // Unknown refs report support so detectors can tolerate eviction races.
        assert!(storage.supports_aggregate(SeriesRef::new(999), Aggregate::Sum));
    }

    #[test]
    fn storage_key_is_exposed_and_go_compatible() {
        let mut storage = store();
        let tags = tags(&["env:prod"]);
        let series_ref = storage
            .add("dogstatsd", "metric.a", Some("host-a"), 1.0, 100, &tags)
            .series_ref
            .unwrap();

        let expected = identity::storage_key("dogstatsd", "metric.a", "host-a", &tags);
        assert_eq!(storage.series_storage_key(series_ref), Some(expected));
    }

    #[test]
    fn unset_and_empty_host_merge_into_one_series() {
        let mut storage = store();
        let unset = storage.add("ns", "m", None, 1.0, 1000, &[]).series_ref.unwrap();
        let empty = storage.add("ns", "m", Some(""), 2.0, 1001, &[]).series_ref.unwrap();

        assert_eq!(unset, empty, "the Go key collapses None and \"\"");
        assert_eq!(storage.total_series_count(), 1);
        // The first writer's metadata is retained.
        assert_eq!(storage.series_meta(unset).unwrap().host, None);
    }

    #[test]
    fn distinct_hosts_are_distinct_series() {
        let mut storage = store();
        let first = storage
            .add("ns", "m", Some("host-a"), 1.0, 1000, &[])
            .series_ref
            .unwrap();
        let second = storage
            .add("ns", "m", Some("host-b"), 1.0, 1000, &[])
            .series_ref
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(storage.total_series_count(), 2);
    }
}
