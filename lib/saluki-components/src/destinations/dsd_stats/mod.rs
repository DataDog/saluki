use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_api::{
    extract::{Query, State},
    routing::{get, Router},
    APIHandler, StatusCode,
};
use saluki_common::time::get_coarse_unix_timestamp;
use saluki_context::tags::SharedTagSet;
use saluki_core::{
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        ComponentContext,
    },
    data_model::event::{Event, EventType},
};
use saluki_error::GenericError;
use serde::{Deserialize, Serialize};
use serde_json;
use stringtheory::MetaString;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::{Duration, Instant};
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

    Statistics {
        /// Start time of the collected metrics, as a Unix timestamp.
        start_time_unix: u64,

        /// End time of the collected metrics, as a Unix timestamp.
        end_time_unix: u64,

        /// Collected statistics.
        stats: HashMap<ContextNoOrigin, MetricSample>,
    },
}

/// Configuration for DogStatsD statistics destination and API handler.
#[derive(Clone)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDAPIHandler,
    rx: Arc<Mutex<StatsRequestReceiver>>,
}
/// State for the DogStatsD API handler.
#[derive(Clone)]
pub struct DogStatsDAPIHandlerState {
    tx: Arc<mpsc::Sender<(oneshot::Sender<StatsResponse>, u64)>>,
}

/// API handler for dogstatsd stats endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    state: DogStatsDAPIHandlerState,
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
        let mut stats_collection_end_time = 0;
        let collection_done = tokio::time::sleep(std::time::Duration::ZERO);
        tokio::pin!(collection_done);

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
                        let try_after = stats_collection_end_time - get_coarse_unix_timestamp();

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

                    let response = StatsResponse::Statistics {
                        start_time_unix: stats_collection_start_time,
                        end_time_unix: stats_collection_end_time,
                        stats,
                    };

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

use std::fmt;

#[derive(Eq, Hash, PartialEq)]
struct ContextNoOrigin {
    name: MetaString,
    tags: SharedTagSet,
}

impl fmt::Display for ContextNoOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.tags.is_empty() {
            write!(f, "{{{}}}", self.tags)?;
        }
        Ok(())
    }
}

impl Serialize for ContextNoOrigin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
#[derive(Deserialize)]
struct StatsQueryParams {
    collection_duration_secs: u64,
}

impl DogStatsDAPIHandler {
    async fn stats_handler(
        State(state): State<DogStatsDAPIHandlerState>, Query(query): Query<StatsQueryParams>,
    ) -> (StatusCode, String) {
        const MAXIMUM_COLLECTION_DURATION_SECS: u64 = 600;
        if query.collection_duration_secs > MAXIMUM_COLLECTION_DURATION_SECS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "collection duration cannot be greater than {} seconds",
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
                StatsResponse::Statistics {
                    start_time_unix: _,
                    end_time_unix: _,
                    stats,
                } => match serde_json::to_string(&stats) {
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
    /// Creates a new 'DogStatsDStatisticsConfiguration' from the given configuration.
    pub fn from_configuration() -> Result<Self, GenericError> {
        let (tx, rx) = mpsc::channel(4);
        let state = DogStatsDAPIHandlerState { tx: Arc::new(tx) };
        let handler = DogStatsDAPIHandler { state };

        Ok(Self {
            api_handler: handler,
            rx: Arc::new(Mutex::new(rx)),
        })
    }

    /// Returns an API handler for DogStatsD API.
    pub fn api_handler(&self) -> DogStatsDAPIHandler {
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
