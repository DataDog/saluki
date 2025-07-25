use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use chrono;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr};
use saluki_api::{
    extract::State,
    routing::{get, Router},
    APIHandler, StatusCode,
};
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        ComponentContext,
    },
    data_model::event::metric::Metric as MetricType,
    data_model::event::{Event::Metric, EventType},
};
use saluki_error::GenericError;
use serde_json;
use tokio::{select, sync::mpsc};
use tracing::info;
#[derive(Debug, Clone, serde::Serialize)]
pub struct Stats {
    metrics_received: HashMap<String, MetricSample>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricSample {
    count: u64,
    last_seen: String,
    name: String,
    tags: String,
}
/// Configuration for DogStatsD statistics destination and API handler.
#[derive(Clone)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDAPIHandler,
    rx: Arc<Mutex<mpsc::Receiver<tokio::sync::oneshot::Sender<Stats>>>>,

    /// Maximum number of input metrics to encode into a single request payload.
    ///
    /// This applies both to the series and sketches endpoints.
    ///
    /// Defaults to 10,000.
    max_metrics_per_payload: usize,
}
/// State for the DogStatsD API handler.
#[derive(Clone)]
pub struct DogStatsDAPIHandlerState {
    tx: Arc<mpsc::Sender<tokio::sync::oneshot::Sender<Stats>>>,
}

/// API handler for dogstatsd stats endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    state: DogStatsDAPIHandlerState,
}

/// DogStatsD destination that collects metrics and processes statistics.
pub struct DogStatsDStats {
    rx: Arc<Mutex<mpsc::Receiver<tokio::sync::oneshot::Sender<Stats>>>>,
    stats: Stats,
}

#[async_trait::async_trait]
impl Destination for DogStatsDStats {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();

        loop {
            select! {
                _ = health.live() => {
                    continue
                },
                api_request = async {
                    if let Ok(mut rx) = self.rx.try_lock() {
                        if let Ok(oneshot_tx) = rx.try_recv() {
                            let _ = oneshot_tx.send(self.stats.clone());
                        }
                    }
                } => {
                    api_request
                },
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        for event in events {
                            if let Metric(metric) = event {
                                let context = metric.context();
                                let metric_name = context.name().to_string();
                                let tags: Vec<String> =
                                    context.tags().into_iter().map(|tag| tag.as_str().to_string()).collect();
                                let tags_formatted = tags.join(",");
                                let key = if tags.is_empty() {
                                    metric_name.clone()
                                } else {
                                    format!("{}|{}", metric_name.clone(), tags_formatted)
                                };

                                let now = SystemTime::now();
                                let datetime = chrono::DateTime::<chrono::Local>::from(now);
                                let sample = self.stats.metrics_received.entry(key).or_insert_with(|| MetricSample {
                                    count: 0,
                                    last_seen: datetime.format("%Y-%m-%d %H:%M:%S").to_string(),
                                    name: metric_name.clone(),
                                    tags: tags_formatted.clone(),
                                });
                                sample.count += 1;
                                sample.last_seen = datetime.format("%Y-%m-%d %H:%M:%S").to_string();

                            }
                        }
                    }
                    None => break,
                },
            }
        }

        Ok(())
    }
}

impl DogStatsDAPIHandler {
    async fn stats_handler(State(state): State<DogStatsDAPIHandlerState>) -> (StatusCode, String) {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();

        if let Err(e) = state.tx.try_send(oneshot_tx) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send stats: {}", e),
            );
        }

        match oneshot_rx.await {
            Ok(stats) => {
                info!("Stats on received events: {:?}", stats);
                (StatusCode::OK, serde_json::to_string(&stats).unwrap())
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to receive stats: {}", e),
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
    pub fn from_configuration(_: &GenericConfiguration) -> Result<Self, GenericError> {
        let (tx, rx) = mpsc::channel(100);
        let state = DogStatsDAPIHandlerState { tx: Arc::new(tx) };
        let handler = DogStatsDAPIHandler { state };

        Ok(Self {
            api_handler: handler,
            rx: Arc::new(Mutex::new(rx)),
            max_metrics_per_payload: default_max_metrics_per_payload(),
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
        let rx = Arc::clone(&self.rx);
        Ok(Box::new(DogStatsDStats {
            rx,
            stats: Stats {
                metrics_received: HashMap::new(),
            },
        }))
    }
}

impl MemoryBounds for DogStatsDStatisticsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDStatisticsConfiguration>("configuration struct");

        builder.firm().with_expr(UsageExpr::constant("api handler state", 48));
        builder.firm().with_expr(UsageExpr::constant("destination state", 24));
        builder
            .firm()
            // Capture the size of the "split re-encode" buffers in the request builders, which is where we keep owned
            // versions of metrics that we encode in case we need to actually re-encode them during a split operation.
            .with_array::<MetricType>("series metrics split re-encode buffer", self.max_metrics_per_payload)
            .with_array::<MetricType>("sketch metrics split re-encode buffer", self.max_metrics_per_payload);
    }
}

const fn default_max_metrics_per_payload() -> usize {
    10_000
}
