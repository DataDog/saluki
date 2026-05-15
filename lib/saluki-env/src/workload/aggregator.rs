//! Workload metadata aggregation.

use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::health::Health;
use saluki_core::runtime::{InitializationError, ProcessShutdown, Supervisable, SupervisorFuture};
use saluki_error::{generic_error, GenericError};
use tokio::{select, sync::mpsc, sync::Mutex};
use tracing::debug;

use super::metadata::MetadataOperation;

// TODO: Make this configurable.
const OPERATIONS_CHANNEL_SIZE: usize = 128;

/// Metadata aggregator based on configurable collectors.
///
/// Metadata collectors are used to either scrape or listen for changes in workload metadata, which converts those
/// changes into [`MetadataOperation`]s that are applied to a [`MetadataStore`]. `MetadataAggregator` receives those
/// operations and applies them to all configured [`MetadataStore`]s.
pub struct MetadataAggregator {
    state: Arc<Mutex<MetadataAggregatorState>>,
}

struct MetadataAggregatorState {
    stores: Vec<Box<dyn MetadataStore + Send>>,
    operations_rx: mpsc::Receiver<MetadataOperation>,
    health: Health,
}

impl MetadataAggregator {
    /// Creates a new `MetadataAggregator`, returning it along with the operations sender that should be cloned into
    /// each collector worker.
    pub fn new(health: Health) -> (Self, mpsc::Sender<MetadataOperation>) {
        let (operations_tx, operations_rx) = mpsc::channel(OPERATIONS_CHANNEL_SIZE);
        let state = MetadataAggregatorState {
            stores: Vec::new(),
            operations_rx,
            health,
        };
        (
            Self {
                state: Arc::new(Mutex::new(state)),
            },
            operations_tx,
        )
    }

    /// Adds a metadata store to the aggregator.
    ///
    /// This store will receive a copy of every metadata operation that is emitted from the configured metadata
    /// collectors.
    pub fn add_store<S>(&mut self, store: S)
    where
        S: MetadataStore + Send + 'static,
    {
        // Setup-time access: nothing else holds the lock yet, so this never contends.
        let mut state = self
            .state
            .try_lock()
            .expect("aggregator state should not be locked during setup");
        state.stores.push(Box::new(store));
    }
}

impl MemoryBounds for MetadataAggregator {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .firm()
            // Operations channel.
            .with_array::<MetadataOperation>("metadata ops channel", OPERATIONS_CHANNEL_SIZE);

        let state = self
            .state
            .try_lock()
            .expect("aggregator state should not be locked during bounds calculation");
        for store in &state.stores {
            builder.with_subcomponent(store.name(), store);
        }
    }
}

#[async_trait]
impl Supervisable for MetadataAggregator {
    fn name(&self) -> &str {
        "workload-aggregator"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let state = Arc::clone(&self.state);

        Ok(Box::pin(async move {
            let mut state_guard = state.lock_owned().await;

            select! {
                _ = process_shutdown => Ok(()),
                result = run_aggregator(&mut state_guard) => result,
            }
        }))
    }
}

async fn run_aggregator(state: &mut MetadataAggregatorState) -> Result<(), GenericError> {
    debug!("Metadata aggregator started.");
    state.health.mark_ready();

    loop {
        select! {
            _ = state.health.live() => {},
            maybe_operation = state.operations_rx.recv() => match maybe_operation {
                Some(operation) => {
                    // Send the operation to all stores, taking care to only clone the operation if we have two
                    // or more stores configured.
                    let stores_to_clone_for = state.stores.len().saturating_sub(1);
                    for store in state.stores.iter_mut().take(stores_to_clone_for) {
                        store.process_operation(operation.clone());
                    }

                    if let Some(last_store) = state.stores.last_mut() {
                        last_store.process_operation(operation);
                    }
                },
                None => {
                    debug!("Metadata aggregator operations channel closed. Stopping...");
                    break;
                },
            },
        }
    }

    state.health.mark_not_ready();
    debug!("Metadata aggregator stopped.");

    Err(generic_error!(
        "Metadata aggregator operation channel closed unexpectedly."
    ))
}

/// A store which receives a stream of metadata operations.
pub trait MetadataStore: MemoryBounds {
    /// Returns the name of the store.
    fn name(&self) -> &'static str;

    /// Process a metadata operation.
    fn process_operation(&mut self, operation: MetadataOperation);
}
