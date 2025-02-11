use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_health::Health;
use tokio::{select, sync::mpsc};
use tracing::debug;

use super::{
    collectors::{MetadataCollector, MetadataCollectorWorker},
    metadata::MetadataOperation,
};

// TODO: Make this configurable.
const OPERATIONS_CHANNEL_SIZE: usize = 128;

/// Aggregates the metadata from multiple collectors.
///
/// Metadata collectors are used to either scrape or listen for changes in workload metadata, and convert those into
/// [`MetadataOperation`]s that are applied to a [`TagStore`]. [`MetadataAggregator`] is a simple manager type that
/// controls the lifecycle of each collector added, and processes the metadata operations produced by them in order to
/// update a single, unified tag store.
///
/// Tags can then be accessed through [`TagSnapshot`] (via [`MetadataAggregator::tags`]), which is a shared reference
/// to the most up-to-date, consistent view of the tag store. This tag snapshot can be used to query for entity tags
/// directly, with an equivalent API to [`WorkloadProvider`].
pub struct MetadataAggregator {
    stores: Vec<Box<dyn MetadataStore + Send>>,
    operations_tx: mpsc::Sender<MetadataOperation>,
    operations_rx: mpsc::Receiver<MetadataOperation>,
    health: Health,
}

impl MetadataAggregator {
    /// Create a new `MetadataAggregator`.
    pub fn new(health: Health) -> Self {
        let (operations_tx, operations_rx) = mpsc::channel(OPERATIONS_CHANNEL_SIZE);
        Self {
            stores: Vec::new(),
            operations_tx,
            operations_rx,
            health,
        }
    }

    /// Adds a metadata collector to the aggregator.
    pub fn add_collector<C>(&mut self, collector: C)
    where
        C: MetadataCollector + Send + 'static,
    {
        let worker = MetadataCollectorWorker::new(collector);
        tokio::spawn(worker.run(self.operations_tx.clone()));
    }

    /// Adds a metadata store to the aggregator.
    ///
    /// This store will receive a copy of every metadata operation that is emitted from the configured metadata
    /// collectors.
    pub fn add_store<S>(&mut self, store: S)
    where
        S: MetadataStore + Send + 'static,
    {
        self.stores.push(Box::new(store));
    }

    /// Runs the aggregator.
    ///
    /// This method will run indefinitely, watching for metadata changes seen by the collectors and updating the tag
    /// store based on those changes. Changes to the tag store will be captured as a tag "snapshot", to which a shared
    /// reference can be acquired ([`MetadataAggregator::tags`]), and the snapshot allows querying for the tags of a
    /// specific entity.
    pub async fn run(mut self) {
        debug!("Metadata aggregator started.");
        self.health.mark_ready();

        loop {
            select! {
                _ = self.health.live() => {},
                maybe_operation = self.operations_rx.recv() => match maybe_operation {
                    Some(operation) => {
                        // Send the operation to all stores, taking care to only clone the operation if we have two or
                        // more stores configured.
                        let stores_to_clone_for = self.stores.len().saturating_sub(1);
                        for store in self.stores.iter_mut().take(stores_to_clone_for) {
                            store.process_operation(operation.clone());
                        }

                        if let Some(last_store) = self.stores.last_mut() {
                            last_store.process_operation(operation);
                        }
                    },
                    None => {
                        debug!("Metadata aggregator operations channel closed. Stopping...");
                        break
                    },
                },
            }
        }

        self.health.mark_not_ready();

        debug!("Metadata aggregator stopped.");
    }
}

impl MemoryBounds for MetadataAggregator {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .firm()
            // Operations channel.
            .with_array::<MetadataOperation>("metadata ops channel", OPERATIONS_CHANNEL_SIZE);

        for store in &self.stores {
            builder.with_subcomponent(store.name(), store);
        }
    }
}

/// A store which receives a stream of metadata operations.
pub trait MetadataStore: MemoryBounds {
    /// Returns the name of the store.
    fn name(&self) -> &'static str;

    /// Process a metadata operation.
    fn process_operation(&mut self, operation: MetadataOperation);
}
