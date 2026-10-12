//! A worker given a resource group of its own is accounted there: what it allocates counts against that group, and
//! not against its supervisor's.
//!
//! This needs the tracking allocator installed as the global allocator, so it runs in a test binary of its own.

use std::alloc::System;

use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceStatsSnapshot, TrackingAllocator};
use saluki_core::runtime::{self, Supervisor};
use tokio::sync::oneshot;

#[global_allocator]
static ALLOC: TrackingAllocator<System> = TrackingAllocator::new(System);

/// What the worker holds while the test reads the groups.
const HELD: usize = 4 * 1024 * 1024;

/// Returns the bytes the group `group` holds, or 0 if no such group is registered.
fn live_bytes(group: &str) -> usize {
    let mut live = 0;
    ResourceGroupRegistry::global().visit_resource_groups(|name, stats| {
        if name == group {
            live = stats.snapshot_delta(&ResourceStatsSnapshot::empty()).live_bytes();
        }
    });
    live
}

#[tokio::test]
async fn a_worker_given_a_resource_group_allocates_in_that_group_and_not_in_its_supervisors() {
    assert!(ResourceGroupRegistry::allocator_installed());
    let (held_tx, held_rx) = oneshot::channel();
    let mut supervisor = Supervisor::new("group-test-sup").unwrap();
    supervisor.add_worker(
        runtime::worker_with_shutdown("holder", move |shutdown| async move {
            let held = vec![1u8; HELD];
            let _ = held_tx.send(());
            shutdown.await;
            drop(held);
        })
        .with_resource_group("group-test-sup.leases.dd_metrics_encode")
        .build(),
    );
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let run = tokio::spawn(async move {
        supervisor
            .run_with_shutdown(async move {
                let _ = stop_rx.await;
            })
            .await
    });

    held_rx.await.expect("the worker runs");
    let own = live_bytes("group-test-sup.leases.dd_metrics_encode");
    let supervisors = live_bytes("group-test-sup");
    assert!(own >= HELD, "the worker's group holds {} bytes", own);
    assert!(supervisors < HELD, "the supervisor's group holds {} bytes", supervisors);

    stop_tx.send(()).unwrap();
    run.await.unwrap().expect("the supervisor stops cleanly");
}
