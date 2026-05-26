use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_api::{
    extract::{Query, State},
    routing::{get, Router},
    APIHandler, StatusCode,
};
use saluki_common::{collections::FastHashMap, time::get_coarse_unix_timestamp};
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::metric::Metric;
use serde::{Deserialize, Serialize, Serializer};
use stringtheory::MetaString;
use tokio::time::sleep;

const MAXIMUM_COLLECTION_DURATION_SECS: u64 = 600;
const MAXIMUM_DISTINCT_CONTEXTS: usize = 10_000;
const INACTIVE_COLLECTION_ID: u64 = 0;
const INITIAL_COLLECTION_ID: u64 = 1;

#[derive(Debug, Default, Clone, Serialize)]
pub struct MetricSample {
    count: u64,
    last_seen: u64,
}

#[derive(Debug, Serialize)]
enum StatsResponse {
    /// An existing statistics collection request is running.
    AlreadyRunning {
        /// Number of seconds to wait before trying again.
        try_after: u64,
    },

    Statistics(CollectedStatistics),
}

#[derive(Debug, Serialize)]
struct CollectedStatistics {
    /// Start time of the collected metrics, as a Unix timestamp.
    start_time_unix: u64,

    /// End time of the collected metrics, as a Unix timestamp.
    end_time_unix: u64,

    /// Collected statistics.
    stats: FlattenedStats,
}

#[derive(Serialize)]
struct FlattenedMetricStat<'a> {
    #[serde(flatten)]
    context: &'a ContextNoOrigin,

    #[serde(flatten)]
    stats: &'a MetricSample,
}

#[derive(Debug)]
struct FlattenedStats(FastHashMap<ContextNoOrigin, MetricSample>);

impl Serialize for FlattenedStats {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(
            self.0
                .iter()
                .map(|(context, stats)| FlattenedMetricStat { context, stats }),
        )
    }
}

#[derive(Debug, Eq, Hash, PartialEq, Serialize)]
struct ContextNoOrigin {
    name: MetaString,
    tags: TagSet,
}

impl ContextNoOrigin {
    fn from_metric(metric: &Metric) -> Self {
        let context = metric.context();
        Self {
            name: context.name().clone(),
            tags: context.tags().clone(),
        }
    }
}

#[derive(Debug)]
struct ActiveCollection {
    id: u64,
    start_time_unix: u64,
    end_time_unix: u64,
    stats: FastHashMap<ContextNoOrigin, MetricSample>,
}

#[derive(Debug, Default)]
struct CollectorState {
    next_id: u64,
    active: Option<ActiveCollection>,
}

#[derive(Debug, Default)]
struct CollectorInner {
    active_collection_id: AtomicU64,
    state: Mutex<CollectorState>,
}

/// Shared DogStatsD statistics collector.
///
/// The collector is inactive by default. While inactive, recording a metric only reads an atomic collection ID and
/// returns without locking or allocating.
///
/// A collection is activated by the DogStatsD stats API for a bounded duration. Only one collection may be active at a
/// time, and each active collection receives a unique nonzero ID. Metrics observed for an older ID are dropped if a
/// later collection has already started, which prevents back-to-back requests from recording stale metrics into the
/// wrong response.
///
/// Active collections store up to `MAXIMUM_DISTINCT_CONTEXTS` distinct metric contexts. Once the cap is reached, metrics
/// for existing contexts continue to update their count and last-seen timestamp, while metrics for new contexts are
/// ignored.
#[derive(Clone, Debug, Default)]
pub struct DogStatsDStatsCollector {
    inner: Arc<CollectorInner>,
}

impl DogStatsDStatsCollector {
    /// Records a metric if a DogStatsD stats collection is active.
    ///
    /// When no collection is active, this method performs a single atomic load and returns. When a collection is active,
    /// it captures the active collection ID, locks the collector state, allocates a context key for the metric, and then
    /// updates the count and last-seen timestamp for that context.
    ///
    /// If the active collection changes before the state lock is acquired, the metric is treated as stale and dropped
    /// instead of being recorded into the newer collection. If the collection has already reached its distinct-context
    /// cap, only metrics for contexts already present in the collection are recorded.
    pub fn record_metric(&self, metric: &Metric) {
        let collection_id = self.inner.active_collection_id.load(Ordering::Acquire);
        if collection_id == INACTIVE_COLLECTION_ID {
            return;
        }

        self.record_metric_for_collection(collection_id, metric);
    }

    fn record_metric_for_collection(&self, collection_id: u64, metric: &Metric) {
        let timestamp = get_coarse_unix_timestamp();
        let mut state = lock_state(&self.inner.state);
        let Some(active) = state.active.as_mut() else {
            self.inner
                .active_collection_id
                .store(INACTIVE_COLLECTION_ID, Ordering::Release);
            return;
        };

        if active.id != collection_id {
            return;
        }

        let context = ContextNoOrigin::from_metric(metric);
        if let Some(sample) = active.stats.get_mut(&context) {
            sample.count += 1;
            sample.last_seen = timestamp;
        } else if active.stats.len() < MAXIMUM_DISTINCT_CONTEXTS {
            active.stats.insert(
                context,
                MetricSample {
                    count: 1,
                    last_seen: timestamp,
                },
            );
        }
    }

    async fn collect_for(&self, collection_duration_secs: u64) -> StatsResponse {
        let duration = Duration::from_secs(collection_duration_secs);
        let guard = match self.start_collection(collection_duration_secs) {
            Ok(guard) => guard,
            Err(response) => return response,
        };

        sleep(duration).await;
        guard.finish()
    }

    fn start_collection(&self, collection_duration_secs: u64) -> Result<CollectionGuard, StatsResponse> {
        let mut state = lock_state(&self.inner.state);

        if let Some(active) = &state.active {
            let try_after = active.end_time_unix.saturating_sub(get_coarse_unix_timestamp());
            return Err(StatsResponse::AlreadyRunning { try_after });
        }

        let start_time_unix = get_coarse_unix_timestamp();
        let end_time_unix = start_time_unix + collection_duration_secs;
        let id = state.next_id.max(INITIAL_COLLECTION_ID);
        state.next_id = id.wrapping_add(1);
        if state.next_id == INACTIVE_COLLECTION_ID {
            state.next_id = INITIAL_COLLECTION_ID;
        }
        state.active = Some(ActiveCollection {
            id,
            start_time_unix,
            end_time_unix,
            stats: FastHashMap::default(),
        });
        self.inner.active_collection_id.store(id, Ordering::Release);

        Ok(CollectionGuard {
            collector: self.clone(),
            collection_id: id,
            disarmed: false,
        })
    }

    fn finish_collection(&self, collection_id: u64) -> StatsResponse {
        let mut state = lock_state(&self.inner.state);
        let Some(active) = state.active.take() else {
            self.inner
                .active_collection_id
                .store(INACTIVE_COLLECTION_ID, Ordering::Release);
            return empty_statistics_response();
        };

        if active.id != collection_id {
            state.active = Some(active);
            return empty_statistics_response();
        }

        self.inner
            .active_collection_id
            .store(INACTIVE_COLLECTION_ID, Ordering::Release);

        StatsResponse::Statistics(CollectedStatistics {
            start_time_unix: active.start_time_unix,
            end_time_unix: active.end_time_unix,
            stats: FlattenedStats(active.stats),
        })
    }

    fn cancel_collection(&self, collection_id: u64) {
        let mut state = lock_state(&self.inner.state);
        if state.active.as_ref().is_some_and(|active| active.id == collection_id) {
            state.active = None;
            self.inner
                .active_collection_id
                .store(INACTIVE_COLLECTION_ID, Ordering::Release);
        }
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.inner.active_collection_id.load(Ordering::Acquire) != INACTIVE_COLLECTION_ID
    }

    #[cfg(test)]
    pub(crate) fn start_collection_for_tests(&self, collection_duration_secs: u64) {
        let mut guard = self
            .start_collection(collection_duration_secs)
            .expect("collection should start");
        guard.disarmed = true;
    }

    #[cfg(test)]
    pub(crate) fn active_metric_count_for_tests(&self, name: &str) -> Option<u64> {
        let state = lock_state(&self.inner.state);
        state.active.as_ref().and_then(|active| {
            active
                .stats
                .iter()
                .find(|(context, _)| context.name.as_ref() == name)
                .map(|(_, sample)| sample.count)
        })
    }

    #[cfg(test)]
    pub(crate) fn active_metric_last_seen_for_tests(&self, name: &str) -> Option<u64> {
        let state = lock_state(&self.inner.state);
        state.active.as_ref().and_then(|active| {
            active
                .stats
                .iter()
                .find(|(context, _)| context.name.as_ref() == name)
                .map(|(_, sample)| sample.last_seen)
        })
    }

    #[cfg(test)]
    pub(crate) fn clear_collection_for_tests(&self) {
        let mut state = lock_state(&self.inner.state);
        state.active = None;
        self.inner
            .active_collection_id
            .store(INACTIVE_COLLECTION_ID, Ordering::Release);
    }
}

impl MemoryBounds for DogStatsDStatsCollector {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<CollectorInner>("collector state");
        builder
            .firm()
            .with_map::<ContextNoOrigin, MetricSample>("active collection contexts", MAXIMUM_DISTINCT_CONTEXTS);
    }
}

#[derive(Debug)]
struct CollectionGuard {
    collector: DogStatsDStatsCollector,
    collection_id: u64,
    disarmed: bool,
}

impl CollectionGuard {
    fn finish(mut self) -> StatsResponse {
        self.disarmed = true;
        self.collector.finish_collection(self.collection_id)
    }
}

impl Drop for CollectionGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.collector.cancel_collection(self.collection_id);
        }
    }
}

fn lock_state(state: &Mutex<CollectorState>) -> MutexGuard<'_, CollectorState> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn empty_statistics_response() -> StatsResponse {
    StatsResponse::Statistics(CollectedStatistics {
        start_time_unix: 0,
        end_time_unix: 0,
        stats: FlattenedStats(FastHashMap::default()),
    })
}

/// Configuration for DogStatsD statistics API handling.
#[derive(Clone)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDAPIHandler,
    collector: DogStatsDStatsCollector,
}

/// State for the DogStatsD API handler.
#[derive(Clone)]
pub struct DogStatsDAPIHandlerState {
    collector: DogStatsDStatsCollector,
}

/// API handler for DogStatsD statistics endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    state: DogStatsDAPIHandlerState,
}

#[derive(Deserialize)]
struct StatsQueryParams {
    collection_duration_secs: u64,
}

impl DogStatsDAPIHandler {
    async fn stats_handler(
        State(state): State<DogStatsDAPIHandlerState>, Query(query): Query<StatsQueryParams>,
    ) -> (StatusCode, String) {
        if query.collection_duration_secs > MAXIMUM_COLLECTION_DURATION_SECS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "Collection duration cannot be greater than {} seconds.",
                    MAXIMUM_COLLECTION_DURATION_SECS
                ),
            );
        }

        match state.collector.collect_for(query.collection_duration_secs).await {
            StatsResponse::Statistics(collected_stats) => match serde_json::to_string(&collected_stats) {
                Ok(json) => (StatusCode::OK, json),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to serialize stats: {}", e),
                ),
            },
            StatsResponse::AlreadyRunning { try_after } => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Statistics collection already active. Please try again in {} seconds.",
                    try_after
                ),
            ),
        }
    }
}

impl APIHandler for DogStatsDAPIHandler {
    type State = DogStatsDAPIHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/dogstatsd/stats", get(Self::stats_handler))
    }
}

impl DogStatsDStatisticsConfiguration {
    /// Creates a new `DogStatsDStatisticsConfiguration`.
    pub fn new() -> Self {
        let collector = DogStatsDStatsCollector::default();
        let state = DogStatsDAPIHandlerState {
            collector: collector.clone(),
        };
        let handler = DogStatsDAPIHandler { state };

        Self {
            api_handler: handler,
            collector,
        }
    }

    /// Returns an API handler for DogStatsD API.
    pub fn api_handler(&self) -> DogStatsDAPIHandler {
        self.api_handler.clone()
    }

    /// Returns the shared DogStatsD stats collector.
    pub fn collector(&self) -> DogStatsDStatsCollector {
        self.collector.clone()
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;

    use super::*;

    #[test]
    fn inactive_record_metric_is_noop() {
        let collector = DogStatsDStatsCollector::default();
        collector.record_metric(&Metric::counter("foo", 1.0));

        assert!(!collector.is_active());
        assert!(lock_state(&collector.inner.state).active.is_none());
    }

    #[tokio::test]
    async fn active_collection_counts_metrics() {
        let collector = DogStatsDStatsCollector::default();
        let guard = collector.start_collection(10).expect("collection should start");

        let context = Context::from_static_parts("foo", &["env:test"]);
        collector.record_metric(&Metric::counter(context.clone(), 1.0));
        collector.record_metric(&Metric::counter(context, 2.0));

        let response = guard.finish();
        let StatsResponse::Statistics(collected) = response else {
            panic!("expected statistics response");
        };

        assert_eq!(collected.stats.0.len(), 1);
        let sample = collected
            .stats
            .0
            .values()
            .next()
            .expect("expected collected metric sample");
        assert_eq!(sample.count, 2);
        assert!(sample.last_seen >= collected.start_time_unix);
        assert!(!collector.is_active());
    }

    #[test]
    fn second_collection_reports_already_running() {
        let collector = DogStatsDStatsCollector::default();
        let _guard = collector.start_collection(10).expect("collection should start");

        let response = collector
            .start_collection(10)
            .expect_err("second collection should fail");

        match response {
            StatsResponse::AlreadyRunning { try_after } => assert!(try_after <= 10),
            StatsResponse::Statistics(_) => panic!("expected already running response"),
        }
    }

    #[test]
    fn dropped_collection_guard_clears_active_state() {
        let collector = DogStatsDStatsCollector::default();
        let guard = collector.start_collection(10).expect("collection should start");
        assert!(collector.is_active());

        drop(guard);

        assert!(!collector.is_active());
        assert!(lock_state(&collector.inner.state).active.is_none());
    }

    #[test]
    fn stale_collection_id_does_not_record_into_new_collection() {
        let collector = DogStatsDStatsCollector::default();
        let guard = collector.start_collection(10).expect("collection should start");
        let stale_collection_id = guard.collection_id;
        let response = guard.finish();
        assert!(matches!(response, StatsResponse::Statistics(_)));

        let guard = collector.start_collection(10).expect("collection should start");
        collector.record_metric_for_collection(stale_collection_id, &Metric::counter("foo", 1.0));

        let response = guard.finish();
        let StatsResponse::Statistics(collected) = response else {
            panic!("expected statistics response");
        };
        assert!(collected.stats.0.is_empty());
    }

    #[test]
    fn active_collection_caps_distinct_contexts() {
        let collector = DogStatsDStatsCollector::default();
        let guard = collector.start_collection(10).expect("collection should start");

        for index in 0..MAXIMUM_DISTINCT_CONTEXTS {
            let context = Context::from_parts(MetaString::from(format!("metric_{}", index)), TagSet::default());
            collector.record_metric(&Metric::counter(context, 1.0));
        }
        collector.record_metric(&Metric::counter("metric_over_cap", 1.0));

        let existing_context = Context::from_parts(MetaString::from("metric_0"), TagSet::default());
        collector.record_metric(&Metric::counter(existing_context, 1.0));

        let response = guard.finish();
        let StatsResponse::Statistics(collected) = response else {
            panic!("expected statistics response");
        };

        assert_eq!(collected.stats.0.len(), MAXIMUM_DISTINCT_CONTEXTS);
        assert!(!collected
            .stats
            .0
            .keys()
            .any(|context| context.name.as_ref() == "metric_over_cap"));

        let first_metric = collected
            .stats
            .0
            .iter()
            .find(|(context, _)| context.name.as_ref() == "metric_0")
            .map(|(_, sample)| sample)
            .expect("expected existing context to remain recorded");
        assert_eq!(first_metric.count, 2);
    }
}
