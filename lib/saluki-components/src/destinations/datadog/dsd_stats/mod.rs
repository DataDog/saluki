use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    data_model::event::EventType,
};
use saluki_error::GenericError;
use tokio::sync::mpsc;
use tracing::debug;

#[derive(Debug)]
#[allow(dead_code)]
pub struct Stats {
    metrics_received: HashMap<String, u64>,
    // TODO: add more stats here
}
/// Configuration for DogStatsD internal statistics API.
#[allow(dead_code)]
pub struct DogStatsDStatisticsConfiguration {
    api_handler: DogStatsDAPIHandler,

    rx: Arc<Mutex<mpsc::Receiver<tokio::sync::oneshot::Sender<Stats>>>>,
}
/// API handler for dogstatsd stats endpoint.
#[derive(Clone)]
pub struct DogStatsDAPIHandler {
    tx: mpsc::Sender<tokio::sync::oneshot::Sender<Stats>>,
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
        loop {
            // Handle events
            if let Some(events) = context.events().next().await {
                // TODO: Process metrics and update state
                debug!("DogStatsD stats destination received {} events", events.len());
            } else {
                break;
            }

            // Handle API requests
            if let Ok(mut rx) = self.rx.try_lock() {
                if let Ok(tx) = rx.try_recv() {
                    // TODO: handle request from API handler
                    let _ = tx.send(Stats {
                        metrics_received: HashMap::new(),
                    });
                }
            }
        }

        Ok(())
    }
}

impl DogStatsDAPIHandler {
    async fn stats_handler(State(handler): State<DogStatsDAPIHandler>) -> (StatusCode, String) {
        let (tx, rx) = tokio::sync::oneshot::channel();

        if let Err(e) = handler.tx.try_send(tx) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to request stats: {}", e),
            );
        }

        match rx.await {
            Ok(stats) => {
                println!("stats received back: {:?}", stats);
                (StatusCode::OK, format!("{:?}", stats))
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to receive stats: {}", e),
            ),
        }
    }
}

impl APIHandler for DogStatsDAPIHandler {
    type State = DogStatsDAPIHandler;

    fn generate_initial_state(&self) -> Self::State {
        self.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/dogstatsd/stats", get(Self::stats_handler))
    }
}

impl DogStatsDStatisticsConfiguration {
    /// Creates a new 'DogStatsDStatisticsConfiguration' from the given configuration.
    pub fn from_configuration(_: &GenericConfiguration) -> Result<Self, GenericError> {
        let (tx, rx) = mpsc::channel(100);
        let handler = DogStatsDAPIHandler { tx };

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
        // Share the receiver via Arc<Mutex<>>
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
