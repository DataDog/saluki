use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use chrono;

use async_trait::async_trait;
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
    data_model::event::{Event::Metric, EventType},
};
use saluki_error::GenericError;
use serde_json;
use tokio::{select, sync::mpsc};
use tracing::{debug, info};

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct Stats {
    metrics_received: HashMap<String, MetricSample>,
    // TODO: add more stats here
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricSample {
    count: u64,
    last_seen: String,
    name: String,
    tags: String,
}
/// Configuration for DogStatsD internal statistics API.
#[derive(Clone)]
#[allow(dead_code)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDAPIHandler,

    rx: Arc<Mutex<mpsc::Receiver<tokio::sync::oneshot::Sender<Stats>>>>,
}
/// State for the DogStatsD API handler.
#[derive(Clone)]
pub struct DogStatsDAPIState {
    tx: Arc<mpsc::Sender<tokio::sync::oneshot::Sender<Stats>>>,
}

/// API handler for dogstatsd stats endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    state: DogStatsDAPIState,
}
/// DogStatsD destination that collects internal statistics.
#[allow(dead_code)]
pub struct DogStatsDStats {
    rx: Arc<Mutex<mpsc::Receiver<tokio::sync::oneshot::Sender<Stats>>>>,
    stats: Stats,
}

impl DogStatsDStats {}

#[async_trait::async_trait]
impl Destination for DogStatsDStats {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        health.mark_ready();
        debug!("DogStatsD stats destination started.");

        loop {
            // Handle API request first
            if let Ok(mut rx) = self.rx.try_lock() {
                if let Ok(oneshot_tx) = rx.try_recv() {
                    let _ = oneshot_tx.send(self.stats.clone());
                }
            }

            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        println!("DogStatsD stats destination received {} events", events.len());

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
                                let datetime = chrono::DateTime::<chrono::Utc>::from(now);
                                let sample = self.stats.metrics_received.entry(key).or_insert_with(|| MetricSample {
                                    count: 0,
                                    last_seen: datetime.format("%Y-%m-%d %H:%M:%S").to_string(),
                                    name: metric_name.clone(),
                                    tags: tags_formatted.clone(),
                                });
                                sample.count += 1;
                                sample.last_seen = datetime.format("%Y-%m-%d %H:%M:%S").to_string();

                                println!(
                                    "Metric Name: {:?} | Tags: {:?} | Count: {:?} | Last Seen: {:?}",
                                    sample.name, sample.tags, sample.count, sample.last_seen
                                );
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
    async fn stats_handler(State(state): State<DogStatsDAPIState>) -> (StatusCode, String) {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();

        if let Err(e) = state.tx.try_send(oneshot_tx) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to send stats: {}", e),
            );
        }

        match oneshot_rx.await {
            Ok(stats) => {
                info!("stats received back: {:?}", stats);
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
    type State = DogStatsDAPIState;

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
        let state = DogStatsDAPIState { tx: Arc::new(tx) };
        let handler = DogStatsDAPIHandler { state };

        Ok(Self {
            api_handler: handler,
            rx: Arc::new(Mutex::new(rx)),
        })
    }

    /// Returns an API handler for DogStatsD statistics.
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

        builder.firm().with_expr(UsageExpr::constant("api handler state", 64)); // 64 bytes as a placeholder until state is implemented
    }
}
