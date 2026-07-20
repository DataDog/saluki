use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use saluki_api::{
    extract::{Query, State},
    routing::{get, Router},
    APIHandler, StatusCode,
};
use saluki_common::time::get_coarse_unix_timestamp;
use saluki_context::tags::TagSet;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
};
use saluki_error::GenericError;
use serde::{Deserialize, Serialize, Serializer};
use serde_json;
use stringtheory::MetaString;
use tokio::time::{sleep, Duration, Instant};
use tokio::{
    pin,
    sync::{Mutex, OwnedMutexGuard},
};
use tokio::{select, sync::mpsc, sync::oneshot};

type StatsRequestReceiver = mpsc::Receiver<(oneshot::Sender<StatsResponse>, u64)>;

#[derive(Debug, Default, Clone, Serialize)]
pub struct MetricSample {
    count: u64,
    last_seen: u64,
}
#[derive(Serialize)]
enum StatsResponse {
    /// An existing statistics collection request is running.
    AlreadyRunning {
        /// Number of seconds to wait before trying again.
        try_after: u64,
    },

    Statistics(CollectedStatistics),
}

#[derive(Serialize)]
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

struct FlattenedStats(HashMap<ContextNoOrigin, MetricSample>);

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

/// Configuration for DogStatsD statistics destination and API handler.
#[derive(Clone)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDStatsAPIHandler,
    rx: Arc<Mutex<StatsRequestReceiver>>,
}
/// State for the DogStatsD API handler.
#[derive(Clone)]
pub struct DogStatsDStatsAPIHandlerState {
    tx: Arc<mpsc::Sender<(oneshot::Sender<StatsResponse>, u64)>>,
}

/// API handler for DogStatsD statistics endpoint.
#[derive(Clone)]
pub struct DogStatsDStatsAPIHandler {
    state: DogStatsDStatsAPIHandlerState,
}

/// DogStatsD destination that collects metrics and processes statistics.
pub struct DogStatsDStats {
    rx: OwnedMutexGuard<StatsRequestReceiver>,
}

#[async_trait::async_trait]
impl Destination for DogStatsDStats {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        let mut collection_active = false;
        let mut stats_response_tx: Option<tokio::sync::oneshot::Sender<StatsResponse>> = None;
        let mut current_stats: Option<HashMap<ContextNoOrigin, MetricSample>> = None;
        let mut stats_collection_start_time = 0;
        let mut stats_collection_end_time: u64 = 0;
        let collection_done = sleep(std::time::Duration::ZERO);
        pin!(collection_done);

        health.mark_ready();

        loop {
            select! {
                _ = health.live() => {
                    continue
                },
                Some((response_tx, collection_period_secs)) = self.rx.recv() => {
                    if collection_active {
                        // We're already collecting statistics for another stats request
                        // so inform the caller they need to try again later.
                        let now = get_coarse_unix_timestamp();
                        saluki_antithesis::always_or_unreachable!(
                            now >= stats_collection_start_time,
                            "dsd_stats collection clock did not move backward",
                            { "now": now, "start_time": stats_collection_start_time }
                        );
                        let try_after = stats_collection_end_time.saturating_sub(now);

                        // We don't care if we can successfully send back a response or not.
                        let _ = response_tx.send(StatsResponse::AlreadyRunning { try_after });
                    } else {
                        // Start collection.
                        collection_active = true;
                        stats_collection_start_time = get_coarse_unix_timestamp();
                        stats_collection_end_time = stats_collection_start_time + collection_period_secs;
                        stats_response_tx = Some(response_tx);
                        current_stats = Some(HashMap::new());
                        collection_done.as_mut().reset(Instant::now() + Duration::from_secs(collection_period_secs));
                    }
                },
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        if let Some(stats) = current_stats.as_mut() {
                            // We're actively collecting, so process the metrics.
                            for event in events {
                                if let Event::Metric(metric) = event {

                                    let context = metric.context();
                                    let new_context = ContextNoOrigin {
                                        name: context.name().clone(),
                                        tags: context.tags().clone(),
                                    };

                                    let timestamp = get_coarse_unix_timestamp();
                                    let sample = stats.entry(new_context).or_default();
                                    sample.count += 1;
                                    sample.last_seen = timestamp;

                            }
                        }
                     }},
                     None => break,
                },
                _ = &mut collection_done, if collection_active => {
                    collection_active = false;

                    // Build the response.
                    let stats = match current_stats.take() {
                        Some(stats) => stats,
                        None => continue,
                    };

                    let response = StatsResponse::Statistics(CollectedStatistics {
                        start_time_unix: stats_collection_start_time,
                        end_time_unix: stats_collection_end_time,
                        stats: FlattenedStats(stats),
                    });

                    let response_tx = match stats_response_tx.take() {
                        Some(tx) => tx,
                        None => continue,
                    };

                    // We don't care if we can successfully send back a response or not.
                    let _ = response_tx.send(response);
                }

            }
        }
        Ok(())
    }
}

#[derive(Eq, Hash, PartialEq, Serialize)]
struct ContextNoOrigin {
    name: MetaString,
    tags: TagSet,
}
#[derive(Deserialize)]
struct StatsQueryParams {
    collection_duration_secs: u64,
}

impl DogStatsDStatsAPIHandler {
    async fn stats_handler(
        State(state): State<DogStatsDStatsAPIHandlerState>, Query(query): Query<StatsQueryParams>,
    ) -> (StatusCode, String) {
        const MAXIMUM_COLLECTION_DURATION_SECS: u64 = 600;
        if query.collection_duration_secs > MAXIMUM_COLLECTION_DURATION_SECS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "Collection duration cannot be greater than {} seconds.",
                    MAXIMUM_COLLECTION_DURATION_SECS
                ),
            );
        }

        let (oneshot_tx, oneshot_rx) = oneshot::channel();

        state
            .tx
            .send((oneshot_tx, query.collection_duration_secs))
            .await
            .unwrap(); // TODO: use config to set collection period

        match oneshot_rx.await {
            Ok(stats) => match stats {
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
            },
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to collect statistics.".to_string(),
            ),
        }
    }
}

impl APIHandler for DogStatsDStatsAPIHandler {
    type State = DogStatsDStatsAPIHandlerState;

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
        let (tx, rx) = mpsc::channel(4);
        let state = DogStatsDStatsAPIHandlerState { tx: Arc::new(tx) };
        let handler = DogStatsDStatsAPIHandler { state };

        Self {
            api_handler: handler,
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    /// Returns an API handler for DogStatsD API.
    pub fn api_handler(&self) -> DogStatsDStatsAPIHandler {
        self.api_handler.clone()
    }
}

#[async_trait]
impl DestinationBuilder for DogStatsDStatisticsConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        let rx = self.rx.clone().try_lock_owned()?;
        Ok(Box::new(DogStatsDStats { rx }))
    }
}

impl MemoryBounds for DogStatsDStatisticsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDStats>("component struct");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use saluki_context::tags::Tag;
    use serde_json::json;

    use super::*;

    fn tag_set<const N: usize>(tags: [&'static str; N]) -> TagSet {
        tags.into_iter().map(Tag::from_static).collect()
    }

    #[test]
    fn collected_statistics_serialize_as_flat_metric_entries() {
        // The `/dogstatsd/stats` endpoint returns `CollectedStatistics`: a start/end window plus a flat array of
        // per-context samples, where each entry inlines the context (`name`, `tags`) and its `count`/`last_seen`.
        let mut stats = HashMap::new();
        stats.insert(
            ContextNoOrigin {
                name: MetaString::from("my.counter"),
                tags: tag_set(["env:prod", "service:web"]),
            },
            MetricSample {
                count: 3,
                last_seen: 100,
            },
        );

        let collected = CollectedStatistics {
            start_time_unix: 10,
            end_time_unix: 70,
            stats: FlattenedStats(stats),
        };
        let json = serde_json::to_value(&collected).expect("collected statistics should serialize");

        assert_eq!(json!(10), json["start_time_unix"]);
        assert_eq!(json!(70), json["end_time_unix"]);

        let entries = json["stats"].as_array().expect("stats should serialize as an array");
        assert_eq!(1, entries.len());
        let entry = &entries[0];
        assert_eq!(json!("my.counter"), entry["name"]);
        assert_eq!(json!(3), entry["count"]);
        assert_eq!(json!(100), entry["last_seen"]);
        let tags = entry["tags"]
            .as_array()
            .expect("tags should serialize as an array")
            .iter()
            .map(|tag| tag.as_str().expect("each tag should be a string"))
            .collect::<BTreeSet<_>>();
        assert_eq!(BTreeSet::from(["env:prod", "service:web"]), tags);
    }

    #[tokio::test]
    async fn stats_handler_rejects_excessive_collection_duration() {
        // The handler caps collection at 600 seconds and short-circuits longer requests with a 400 before any
        // collection is started.
        let config = DogStatsDStatisticsConfiguration::new();
        let state = config.api_handler.state.clone();

        let (status, body) = DogStatsDStatsAPIHandler::stats_handler(
            State(state),
            Query(StatsQueryParams {
                collection_duration_secs: 601,
            }),
        )
        .await;

        assert_eq!(StatusCode::BAD_REQUEST, status);
        assert_eq!("Collection duration cannot be greater than 600 seconds.", body);
    }

    #[tokio::test]
    async fn collection_request_accumulates_metrics_then_responds_on_timeout() {
        use saluki_core::accounting::{ComponentRegistry, MemoryLimiter};
        use saluki_core::components::ComponentContext;
        use saluki_core::data_model::event::metric::Metric;
        use saluki_core::health::HealthRegistry;
        use saluki_core::runtime::state::DataspaceRegistry;
        use saluki_core::runtime::Supervisor;
        use saluki_core::topology::interconnect::Consumer;
        use saluki_core::topology::{EventsBuffer, TopologyContext};
        use tokio::runtime::Handle;
        use tokio::time::timeout;

        // Build the destination and grab the request sender the API handler would normally use.
        let config = DogStatsDStatisticsConfiguration::new();
        let request_tx = config.api_handler.state.tx.clone();

        let component_context = ComponentContext::test_destination("test");
        let destination = config
            .build(component_context.clone())
            .await
            .expect("dsd_stats destination should build");

        // Wire up the destination context: an events channel we control and an idle health handle.
        let (events_tx, events_rx) = mpsc::channel::<EventsBuffer>(4);
        let consumer = Consumer::new(component_context.clone(), events_rx);
        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = HealthRegistry::new()
            .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("test"))
            .expect("component was not previously registered");
        let supervisor_handle = Supervisor::new("test").expect("valid supervisor name").handle();
        let context = DestinationContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            consumer,
            supervisor_handle,
        );

        let run_handle = tokio::spawn(async move { destination.run(context).await });

        // Start a one-second collection window. Yield afterwards so the current-thread runtime lets the run loop
        // process the request (marking collection active) before the metrics arrive; otherwise the metrics would be
        // dropped as "not collecting".
        let (response_tx, response_rx) = oneshot::channel();
        request_tx
            .send((response_tx, 1))
            .await
            .expect("collection request should be accepted");
        tokio::task::yield_now().await;

        // The same context seen twice accumulates a single entry with count 2; a distinct context yields count 1.
        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::Metric(Metric::counter("dsd.stats.repeated", 1.0)))
            .is_none());
        assert!(events
            .try_push(Event::Metric(Metric::counter("dsd.stats.repeated", 1.0)))
            .is_none());
        assert!(events
            .try_push(Event::Metric(Metric::counter("dsd.stats.single", 1.0)))
            .is_none());
        events_tx.send(events).await.expect("metrics should be accepted");
        tokio::task::yield_now().await;

        // The collection window elapses after one second, completing collection and sending the response; the
        // recv is bounded well above that window so a stalled collection surfaces as a failure, not a hang.
        let response = timeout(Duration::from_secs(5), response_rx)
            .await
            .expect("collection response should arrive before timeout")
            .expect("collection response channel should remain open");

        let collected = match response {
            StatsResponse::Statistics(collected) => collected,
            StatsResponse::AlreadyRunning { .. } => panic!("first request should not report an active collection"),
        };
        let samples = collected.stats.0;
        assert_eq!(2, samples.len(), "each distinct context should have its own sample");

        let repeated = samples
            .iter()
            .find(|(ctx, _)| ctx.name.as_ref() == "dsd.stats.repeated")
            .map(|(_, sample)| sample)
            .expect("repeated context should be collected");
        assert_eq!(2, repeated.count, "the repeated context should be counted twice");

        let single = samples
            .iter()
            .find(|(ctx, _)| ctx.name.as_ref() == "dsd.stats.single")
            .map(|(_, sample)| sample)
            .expect("single context should be collected");
        assert_eq!(1, single.count);

        // Closing the events channel lets the run loop terminate cleanly.
        drop(events_tx);
        timeout(Duration::from_secs(1), run_handle)
            .await
            .expect("run task should stop before timeout")
            .expect("run task should not panic")
            .expect("run should complete cleanly");
    }
}
