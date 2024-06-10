use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;
use tracing::debug;

use super::{
    collectors::{MetadataCollector, MetadataCollectorWorker},
    metadata::MetadataOperation,
    store::{TagSnapshot, TagStore},
};

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
    tag_store: TagStore,
    shared_tags: Arc<ArcSwap<TagSnapshot>>,
    operations_tx: mpsc::Sender<MetadataOperation>,
    operations_rx: mpsc::Receiver<MetadataOperation>,
}

impl MetadataAggregator {
    /// Create a new `MetadataAggregator`.
    pub fn new() -> Self {
        let (operations_tx, operations_rx) = mpsc::channel(128);
        Self {
            tag_store: TagStore::default(),
            shared_tags: Arc::new(ArcSwap::new(Arc::new(TagSnapshot::default()))),
            operations_tx,
            operations_rx,
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

    /// Gets a shared reference to the latest tag store snapshot.
    ///
    /// This can be used to query for the tags of a specific entity, and is updated as workload changes are observed and
    /// processed.
    pub fn tags(&self) -> Arc<ArcSwap<TagSnapshot>> {
        Arc::clone(&self.shared_tags)
    }

    /// Runs the aggregator.
    ///
    /// This method will run indefinitely, watching for metadata changes seen by the collectors and updating the tag
    /// store based on those changes. Changes to the tag store will be captured as a tag "snapshot", to which a shared
    /// reference can be acquired ([`MetadataAggregator::tags`]), and the snapshot allows querying for the tags of a
    /// specific entity.
    pub async fn run(mut self) {
        debug!("Metadata aggregator started.");

        while let Some(operation) = self.operations_rx.recv().await {
            self.tag_store.process_operation(operation);

            // Update the shared tag state.
            let tags = self.tag_store.snapshot();
            self.shared_tags.store(Arc::new(tags));
        }
    }
}
