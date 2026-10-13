//! The snapshot bridge: answers the DogStatsD context dump API from aggregation running in a region.
//!
//! Natively, the aggregation transform answers context snapshot requests itself, between batches. When aggregation runs
//! as a stateful kernel in the `dsd_agg` isolation slot, no component holds the requests: the topology takes them when
//! it adds the DogStatsD pipeline ([`SnapshotBridgeState`]), and the [`SnapshotBridge`] worker hands each request to
//! the region as a kernel control message, which every lineage of every state owner in the region answers in turn,
//! between batches and between the steps of a flush. The requester sees what it sees natively: the contexts aggregation
//! holds, or, if the region has stopped, that the owner stopped before responding.
//!
//! When aggregation runs on state lanes, each lane holds a disjoint share of the contexts: the bridge asks every lane
//! in turn, one request each, and answers with every lane's entries, concatenated, once each lane has answered. If a
//! lane does not answer, the request fails, as if the owner had stopped.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_components::transforms::{
    AggregateContextSnapshotEntry, AggregateContextSnapshotPendingResponse, AggregateContextSnapshotReceiver,
    AggregateContextSnapshotRequests,
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
    lanes: u16,
}

impl SnapshotBridge {
    /// Creates the bridge that answers the requests of `state` from the region that `region` sends control messages
    /// to, which runs on `lanes` lanes.
    pub fn new(state: SnapshotBridgeState, region: ControlSender<RegionControl>, lanes: u16) -> Self {
        Self { state, region, lanes }
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
        let lanes = self.lanes;
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
                    () = answer(&region, pending, lanes) => {}
                }
            }
        }))
    }
}

/// The entries a snapshot request collects from the lanes of its region, and the request, answered once every lane
/// has added its own.
struct Collected {
    entries: Vec<AggregateContextSnapshotEntry>,
    remaining: u16,
    pending: Option<AggregateContextSnapshotPendingResponse>,
}

/// Answers one snapshot request from the region, which runs on `lanes` lanes, and waits until the region has handled
/// it, so that requests are answered one at a time, as aggregation answers them natively.
async fn answer(region: &ControlSender<RegionControl>, pending: AggregateContextSnapshotPendingResponse, lanes: u16) {
    if pending.is_canceled() {
        return;
    }
    if lanes > 1 {
        return answer_on_lanes(region, pending, lanes).await;
    }
    let control = KernelControl {
        target: KernelTarget::Stateful,
        // A region on one lane takes kernel control messages without naming a lane.
        lane: None,
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

/// Answers one snapshot request from a region on `lanes` lanes: asks each lane in turn for the contexts it holds, and
/// answers with every lane's, concatenated, once the last lane has added its own.
///
/// A lane that stops, or panics, drops its reply without calling it, as a region on one lane does: once every lane's
/// reply is gone, the request is dropped unanswered, which fails it as an owner that stops does.
async fn answer_on_lanes(
    region: &ControlSender<RegionControl>, pending: AggregateContextSnapshotPendingResponse, lanes: u16,
) {
    let collected = Arc::new(StdMutex::new(Collected {
        entries: Vec::new(),
        remaining: lanes,
        pending: Some(pending),
    }));
    for lane in 0..lanes {
        let collected = Arc::clone(&collected);
        let control = KernelControl {
            target: KernelTarget::Stateful,
            lane: Some(lane),
            message: Box::new(ContextSnapshot::default()),
            reply: Box::new(move |message| {
                let Ok(snapshot) = message.downcast::<ContextSnapshot>() else {
                    return;
                };
                let mut collected = collected.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                collected.entries.extend(snapshot.entries);
                collected.remaining = collected.remaining.saturating_sub(1);
                if collected.remaining == 0 {
                    let entries = std::mem::take(&mut collected.entries);
                    if let Some(pending) = collected.pending.take() {
                        pending.respond(entries);
                    }
                }
            }),
        };
        if let Err(error) = region.send(RegionControl::Kernel(control)).await {
            debug!(
                error = %error,
                lane,
                "A lane of the region running DogStatsD aggregation did not answer a context snapshot request."
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests;
