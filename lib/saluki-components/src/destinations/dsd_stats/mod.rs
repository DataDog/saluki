use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_api::{
    extract::State,
    routing::{get, Router},
    APIHandler, StatusCode,
};
use saluki_common::time::get_coarse_unix_timestamp;
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
use tokio::sync::Mutex;
use tokio::{select, sync::mpsc};
use tracing::info;
#[derive(Debug, Clone, serde::Serialize)]
pub struct Stats {
    metrics_received: HashMap<String, MetricSample>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricSample {
    count: u64,
    last_seen: u64,
}
/// Configuration for DogStatsD statistics destination and API handler.
#[derive(Clone)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDAPIHandler,
    rx: Arc<Mutex<mpsc::Receiver<tokio::sync::oneshot::Sender<Stats>>>>,
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
                maybe_request = async { (self.rx.lock().await).recv().await } => {
                    if let Some(oneshot_tx) = maybe_request {
                        let _ = oneshot_tx.send(self.stats.clone());
                    }
                }
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        for event in events {
                            if let Metric(metric) = event {
                                let key = metric.context().to_string();

                                let timestamp = get_coarse_unix_timestamp();
                                let sample = self.stats.metrics_received.entry(key).or_insert_with(|| MetricSample {
                                    count: 0,
                                    last_seen: 0,
                                });
                                sample.count += 1;
                                sample.last_seen = timestamp;

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

        state.tx.send(oneshot_tx).await.unwrap();

        match oneshot_rx.await {
            Ok(stats) => {
                info!("Stats on received events: {:?}", stats);
                match serde_json::to_string(&stats) {
                    Ok(json) => (StatusCode::OK, json),
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to serialize stats: {}", e),
                    ),
                }
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
            .with_single_value::<DogStatsDStats>("component struct");
    }
}
