//! The snapshot bridge: answers the DogStatsD context dump API from aggregation running in a region.
//!
//! Natively, the aggregation transform answers context snapshot requests itself, between batches. When aggregation runs
//! as a stateful kernel in the `dsd_agg` isolation slot, no component holds the requests: the topology takes them when
//! it adds the DogStatsD pipeline ([`SnapshotBridgeState`]), and the [`SnapshotBridge`] worker hands each request to
//! the region as a kernel control message, which every lineage of every state owner in the region answers in turn,
//! between batches and between the steps of a flush. The requester sees what it sees natively: the contexts aggregation
//! holds, or, if the region has stopped, that the owner stopped before responding.

use std::sync::Arc;

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_components::transforms::{
    AggregateContextSnapshotPendingResponse, AggregateContextSnapshotReceiver, AggregateContextSnapshotRequests,
};
use saluki_core::{
    execution::ControlSender,
    runtime::{InitializationError, Supervisable, SupervisorFuture},
};
use saluki_error::GenericError;
use saluki_metric_kernels::aggregate::ContextSnapshot;
use saluki_metric_plan::runtime::{KernelControl, KernelTarget, RegionControl};
use tokio::{select, sync::Mutex};
use tracing::debug;

/// The snapshot requests of DogStatsD aggregation, held outside the worker that answers them, so that a restarted
/// worker answers them again.
///
/// The requests can be taken from their receiver only once, so they are taken when the topology is built, outside any
/// worker. Clones share the same requests.
#[derive(Clone, Debug)]
pub struct SnapshotBridgeState {
    requests: Arc<Mutex<AggregateContextSnapshotRequests>>,
}

impl SnapshotBridgeState {
    /// Takes the requests from the owner side of the snapshot channel.
    ///
    /// # Errors
    ///
    /// Returns an error if the requests were already taken.
    pub fn new(receiver: AggregateContextSnapshotReceiver) -> Result<Self, GenericError> {
        Ok(Self {
            requests: Arc::new(Mutex::new(receiver.into_requests()?)),
        })
    }
}

/// The supervised worker that answers DogStatsD aggregation's context snapshot requests from its region.
pub struct SnapshotBridge {
    state: SnapshotBridgeState,
    region: ControlSender<RegionControl>,
}

impl SnapshotBridge {
    /// Creates the bridge that answers the requests of `state` from the region that `region` sends control messages
    /// to.
    pub fn new(state: SnapshotBridgeState, region: ControlSender<RegionControl>) -> Self {
        Self { state, region }
    }
}

#[async_trait]
impl Supervisable for SnapshotBridge {
    fn name(&self) -> &str {
        "programmable-metrics-snapshot-bridge"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let requests = Arc::clone(&self.state.requests);
        let region = self.region.clone();
        Ok(Box::pin(async move {
            tokio::pin!(process_shutdown);
            // Held for the life of this run: a run that stopped released it, so a restarted run takes it again.
            let mut requests = requests.lock().await;
            loop {
                let pending = select! {
                    biased;
                    _ = &mut process_shutdown => return Ok(()),
                    pending = requests.next() => pending,
                };
                let Some(pending) = pending else {
                    // Every handle is gone: no request will come again.
                    process_shutdown.await;
                    return Ok(());
                };
                select! {
                    biased;
                    _ = &mut process_shutdown => return Ok(()),
                    () = answer(&region, pending) => {}
                }
            }
        }))
    }
}

/// Answers one snapshot request from the region, and waits until the region has handled it, so that requests are
/// answered one at a time, as aggregation answers them natively.
async fn answer(region: &ControlSender<RegionControl>, pending: AggregateContextSnapshotPendingResponse) {
    if pending.is_canceled() {
        return;
    }
    let control = KernelControl {
        target: KernelTarget::Stateful,
        message: Box::new(ContextSnapshot::default()),
        // The region calls the reply with the message once every lineage has added its contexts to it, and drops it
        // without calling it if it stops or a kernel panics, which fails the request as an owner that stops does.
        reply: Box::new(move |message| {
            if let Ok(snapshot) = message.downcast::<ContextSnapshot>() {
                pending.respond(snapshot.entries);
            }
        }),
    };
    if let Err(error) = region.send(RegionControl::Kernel(control)).await {
        debug!(error = %error, "The region running DogStatsD aggregation did not answer a context snapshot request.");
    }
}

#[cfg(test)]
mod tests;
