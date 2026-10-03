use std::{
    future::pending,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use saluki_error::{generic_error, GenericError};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{timeout, Instant},
};

use super::*;
use crate::runtime::{self, AutoShutdown, NodeSnapshot, NodeState, SupervisionTreeHandle, Supervisor, SupervisorError};
use crate::test_support::wait_until;

/// The maximum time to wait for a supervisor run that is expected to finish.
const RUN_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `supervisor` until the returned sender fires, and returns after the supervisor starts.
async fn run_with_trigger(supervisor: Supervisor) -> (oneshot::Sender<()>, JoinHandle<Result<(), SupervisorError>>) {
    let handle = supervisor.handle();
    let mut supervisor = supervisor;

    let (tx, rx) = oneshot::channel();
    let run = tokio::spawn(async move { supervisor.run_with_shutdown(rx).await });

    wait_until("supervisor is running", || handle.is_running()).await;
    (tx, run)
}

/// Runs `supervisor` until it exits on its own.
///
/// Use this for a supervisor that is expected to fail quickly. That supervisor can exit before [`run_with_trigger`]
/// sees it start.
async fn run_to_exit(mut supervisor: Supervisor) -> Result<(), SupervisorError> {
    let (_tx, rx) = oneshot::channel::<()>();
    timeout(RUN_TIMEOUT, supervisor.run_with_shutdown(rx))
        .await
        .expect("supervisor should exit on its own")
}

async fn join(run: JoinHandle<Result<(), SupervisorError>>) -> Result<(), SupervisorError> {
    timeout(RUN_TIMEOUT, run)
        .await
        .expect("supervisor should exit promptly")
        .expect("supervisor task should not panic")
}

/// Finds the first node named `name` with a depth-first search.
fn find<'a>(node: &'a NodeSnapshot, name: &str) -> Option<&'a NodeSnapshot> {
    if node.name == name {
        return Some(node);
    }

    node.children.iter().find_map(|child| find(child, name))
}

fn node_state(tree: &SupervisionTreeHandle, name: &str) -> Option<NodeState> {
    find(&tree.snapshot().root, name).map(|node| node.state)
}

fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// A child that runs until it is told to stop, and records that it started and that it stopped.
fn until_stopped(
    name: &'static str, started: &Arc<AtomicBool>, stopped: &Arc<AtomicBool>,
) -> ChildSpecification<WorkerSpec> {
    let started = Arc::clone(started);
    let stopped = Arc::clone(stopped);
    runtime::worker_with_shutdown(name, move |shutdown| async move {
        started.store(true, Ordering::SeqCst);
        shutdown.await;
        stopped.store(true, Ordering::SeqCst);
    })
    .build()
}

/// A supervisable worker that counts how many times it starts, and exits at once or at shutdown, cleanly or not.
struct CountingWorker {
    name: &'static str,
    starts: Arc<AtomicUsize>,
    exit: Exit,
}

#[derive(Clone, Copy)]
enum Exit {
    /// Exits cleanly at once.
    Immediately,

    /// Runs until it is told to stop.
    OnShutdown,

    /// Fails to initialize.
    FailInit,
}

#[async_trait]
impl Supervisable for CountingWorker {
    fn name(&self) -> &str {
        self.name
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        if matches!(self.exit, Exit::FailInit) {
            return Err(generic_error!("init failed").into());
        }

        self.starts.fetch_add(1, Ordering::SeqCst);
        let exit = self.exit;
        Ok(Box::pin(async move {
            if matches!(exit, Exit::OnShutdown) {
                process_shutdown.await;
            }
            Ok(())
        }))
    }
}

/// A supervisable worker that counts how many times it starts, and exits cleanly when the test releases it.
///
/// An instance that starts after the release exits at once.
struct ReleasedWorker {
    name: &'static str,
    starts: Arc<AtomicUsize>,
    released: watch::Receiver<bool>,
}

#[async_trait]
impl Supervisable for ReleasedWorker {
    fn name(&self) -> &str {
        self.name
    }

    async fn initialize(&self, _process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let mut released = self.released.clone();
        Ok(Box::pin(async move {
            let _ = released.wait_for(|released| *released).await;
            Ok(())
        }))
    }
}

/// A supervisable worker that spawns a child each time it starts.
///
/// Its first instance fails after its child ran, so that its supervisor restarts it.
struct RespawningOwner {
    starts: Arc<AtomicUsize>,
    child_runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Supervisable for RespawningOwner {
    fn name(&self) -> &str {
        "owner"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let first = self.starts.fetch_add(1, Ordering::SeqCst) == 0;
        let child_runs = Arc::clone(&self.child_runs);
        Ok(Box::pin(async move {
            let (ran_tx, ran_rx) = oneshot::channel();
            runtime::worker("child", async move {
                child_runs.fetch_add(1, Ordering::SeqCst);
                let _ = ran_tx.send(());
            })
            .spawn_child();
            let _ = ran_rx.await;

            if first {
                return Err(generic_error!("the first instance fails"));
            }
            process_shutdown.await;
            Ok(())
        }))
    }
}

#[test]
fn spawn_child_outside_a_scope_panics() {
    let result = std::panic::catch_unwind(|| runtime::worker("child", async {}).spawn_child());
    assert!(result.is_err(), "spawning into a scope outside supervision must panic");
}

#[test]
fn nested_outside_a_scope_panics() {
    let result = std::panic::catch_unwind(|| nested("cache"));
    assert!(result.is_err(), "nesting a scope outside supervision must panic");
}

#[tokio::test]
async fn spawn_child_or_detached_runs_the_child_outside_supervision() {
    let ran = flag();
    let child_ran = Arc::clone(&ran);

    let id = runtime::worker("child", async move {
        child_ran.store(true, Ordering::SeqCst);
    })
    .spawn_child_or_detached();

    assert!(
        id.is_none(),
        "there is no scope to spawn into, so the child runs detached"
    );
    wait_until("the detached child runs", || ran.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn owner_does_not_finish_until_its_children_have() {
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let child_done = flag();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    let done = Arc::clone(&child_done);
    sup.add_worker(
        runtime::worker("owner", async move {
            // The child ignores the shutdown signal and runs until the test releases it. The body returns at once.
            runtime::worker("child", async move {
                let _ = release_rx.await;
                done.store(true, Ordering::SeqCst);
            })
            .spawn_child();
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;

    // The body of the owner returned, but the owner continues to run while its child runs.
    wait_until("the child is running under the owner", || {
        node_state(&tree, "child") == Some(NodeState::Running)
    })
    .await;
    assert_eq!(node_state(&tree, "owner"), Some(NodeState::Running));

    release_tx.send(()).unwrap();
    wait_until("the owner finishes once its child has", || {
        node_state(&tree, "owner") == Some(NodeState::Exited)
    })
    .await;
    assert!(child_done.load(Ordering::SeqCst));

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn children_spawned_right_before_the_body_returns_still_run() {
    // The body spawns and returns in the same poll, before the scope can receive its spawns. The set of children that
    // run must not depend on that timing.
    let started = flag();
    let stopped = flag();

    let mut sup = Supervisor::new("sup").unwrap();
    let (child_started, child_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    sup.add_worker(
        runtime::worker("owner", async move {
            current().expect("a supervised worker has a scope").spawn(until_stopped(
                "child",
                &child_started,
                &child_stopped,
            ));
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child ran and was told to stop", || {
        started.load(Ordering::SeqCst) && stopped.load(Ordering::SeqCst)
    })
    .await;

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn children_spawned_by_a_failing_body_never_start() {
    let ran = flag();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    let child_ran = Arc::clone(&ran);
    sup.add_worker(
        runtime::worker("owner", async move {
            // The body spawns and fails in the same poll. Thus, the child still waits in the queue of the scope.
            runtime::worker("child", async move {
                child_ran.store(true, Ordering::SeqCst);
            })
            .spawn_child();
            Err::<(), GenericError>(generic_error!("owner failed"))
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the owner fails", || {
        node_state(&tree, "owner") == Some(NodeState::Exited)
    })
    .await;

    // The owner does not finish until its children finish. Thus, if the child had started, it has run by now.
    assert!(!ran.load(Ordering::SeqCst), "nothing must start after the owner failed");

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn hosted_child_failure_is_reaped_and_the_owner_keeps_running() {
    let failed = flag();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    let child_failed = Arc::clone(&failed);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            runtime::worker("child", async move {
                child_failed.store(true, Ordering::SeqCst);
                Err::<(), GenericError>(generic_error!("child failed"))
            })
            .spawn_child();
            shutdown.await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child fails", || failed.load(Ordering::SeqCst)).await;
    wait_until("the failed child is reaped", || node_state(&tree, "child").is_none()).await;
    assert_eq!(node_state(&tree, "owner"), Some(NodeState::Running));

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn needed_child_terminating_fails_the_owner() {
    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_auto_shutdown(AutoShutdown::AnySignificant);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", |shutdown| async move {
            // The child exits cleanly, but without a request to stop. Because the child is needed, this exit fails the
            // owner.
            runtime::worker("child", async {}).needed().spawn_child();
            shutdown.await;
        })
        .with_significant(true)
        .build(),
    );

    let result = run_to_exit(sup).await;
    assert!(
        matches!(result, Err(SupervisorError::SignificantChildExited)),
        "the owner should have failed, taking the supervisor down with it, got {result:?}"
    );
}

#[tokio::test]
async fn needed_child_failing_to_initialize_fails_the_owners_initialization() {
    let mut sup = Supervisor::new("sup").unwrap();
    sup.add_worker(
        runtime::worker_with_shutdown("owner", |shutdown| async move {
            runtime::supervisable(CountingWorker {
                name: "child",
                starts: counter(),
                exit: Exit::FailInit,
            })
            .needed()
            .spawn_child();
            shutdown.await;
        })
        .build(),
    );

    match run_to_exit(sup).await {
        Err(SupervisorError::FailedToInitialize { child_name, .. }) => assert_eq!(child_name, "owner/child"),
        other => panic!("expected the child's initialization failure to surface through the owner, got {other:?}"),
    }
}

#[tokio::test]
async fn hosted_child_exceeding_the_scope_limit_fails_the_owner() {
    let starts = counter();

    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_auto_shutdown(AutoShutdown::AnySignificant);
    let child_starts = Arc::clone(&starts);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            runtime::supervisable(CountingWorker {
                name: "child",
                starts: child_starts,
                exit: Exit::Immediately,
            })
            .spawn_child();
            shutdown.await;
        })
        .with_significant(true)
        .build(),
    );

    // A scope allows the default intensity, which is one restart in the period. The second exit exceeds this
    // intensity. The usual result for a hosted child is that the scope reaps it and the owner continues. Here, the
    // owner fails instead, so that the level above can retry the failure.
    let result = run_to_exit(sup).await;
    assert!(
        matches!(result, Err(SupervisorError::SignificantChildExited)),
        "the owner should have failed, taking the supervisor down with it, got {result:?}"
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn nested_scope_child_exceeding_the_limit_fails_the_scopes_owner() {
    let starts = counter();

    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_auto_shutdown(AutoShutdown::AnySignificant);
    let child_starts = Arc::clone(&starts);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            let mut guard = nested("cache");
            guard.spawn(
                runtime::supervisable(CountingWorker {
                    name: "driver",
                    starts: child_starts,
                    exit: Exit::Immediately,
                })
                .build(),
            );
            shutdown.await;
            drop(guard);
        })
        .with_significant(true)
        .build(),
    );

    // The host of the nested scope only holds the driver. Thus, the failure of the driver goes through the host to the
    // owner, and does not end at the host.
    let result = run_to_exit(sup).await;
    assert!(
        matches!(result, Err(SupervisorError::SignificantChildExited)),
        "the owner should have failed, taking the supervisor down with it, got {result:?}"
    );
    assert_eq!(starts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn exits_during_close_are_requested_and_never_restarted() {
    let starts = counter();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    let child_starts = Arc::clone(&starts);
    sup.add_worker(
        runtime::worker("owner", async move {
            runtime::supervisable(CountingWorker {
                name: "child",
                starts: child_starts,
                exit: Exit::OnShutdown,
            })
            .spawn_child();
            let _ = stop_rx.await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child is running", || starts.load(Ordering::SeqCst) == 1).await;

    // When the body returns, the scope closes and stops the child. The child is permanent. Thus, if it stops on its
    // own, the scope restarts it.
    stop_tx.send(()).unwrap();
    wait_until("the owner finishes", || {
        node_state(&tree, "owner") == Some(NodeState::Exited)
    })
    .await;
    assert_eq!(starts.load(Ordering::SeqCst), 1);

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn exits_after_the_owner_is_told_to_stop_are_requested() {
    let starts = counter();
    let drained = flag();
    let (release_tx, release_rx) = watch::channel(false);

    let mut sup = Supervisor::new("sup").unwrap();
    let (child_starts, owner_drained) = (Arc::clone(&starts), Arc::clone(&drained));
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            runtime::supervisable(ReleasedWorker {
                name: "consumer",
                starts: child_starts,
                released: release_rx,
            })
            .spawn_child();
            shutdown.await;

            // The owner closes the input of its child, and then drains. The child is permanent, and it exits when its
            // input closes. If the scope treated this exit as an exit that nobody requested, it would restart the child.
            // The next exit would then exceed the restart limit, and fail the owner before its drain is done.
            release_tx.send_replace(true);
            tokio::time::sleep(Duration::from_millis(200)).await;
            owner_drained.store(true, Ordering::SeqCst);
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child is running", || starts.load(Ordering::SeqCst) == 1).await;

    tx.send(()).unwrap();
    let result = join(run).await;
    assert!(result.is_ok(), "the owner should have stopped cleanly, got {result:?}");
    assert!(drained.load(Ordering::SeqCst), "the owner must finish its drain");
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "a child that exits after its owner is told to stop is not restarted"
    );
}

#[tokio::test]
async fn exits_in_a_nested_scope_after_its_owner_is_told_to_stop_are_requested() {
    let starts = counter();
    let drained = flag();
    let (release_tx, release_rx) = watch::channel(false);

    let mut sup = Supervisor::new("sup").unwrap();
    let (driver_starts, owner_drained) = (Arc::clone(&starts), Arc::clone(&drained));
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            let mut guard = nested("cache");
            guard.spawn(
                runtime::supervisable(ReleasedWorker {
                    name: "driver",
                    starts: driver_starts,
                    released: release_rx,
                })
                .build(),
            );
            shutdown.await;

            // Nothing told the host of the nested scope to stop yet, but its owner was told to stop. Thus, an exit of
            // the driver is a requested exit. Otherwise, the restart limit of the nested scope fails the host, and the
            // host passes that failure to the owner in the middle of its drain.
            release_tx.send_replace(true);
            tokio::time::sleep(Duration::from_millis(200)).await;
            owner_drained.store(true, Ordering::SeqCst);
            drop(guard);
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the driver is running", || starts.load(Ordering::SeqCst) == 1).await;

    tx.send(()).unwrap();
    let result = join(run).await;
    assert!(result.is_ok(), "the owner should have stopped cleanly, got {result:?}");
    assert!(drained.load(Ordering::SeqCst), "the owner must finish its drain");
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

/// Panics as a function that returns `()` instead of `!`, so that a body that ends with this call still returns `()`.
fn owner_panics() {
    panic!("owner panicked");
}

#[tokio::test]
async fn owner_panicking_stops_its_children_before_it_goes() {
    let started = flag();
    let stopped = flag();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    let (child_started, child_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    sup.add_worker(
        runtime::worker("owner", async move {
            current().expect("a supervised worker has a scope").spawn(until_stopped(
                "child",
                &child_started,
                &child_stopped,
            ));
            while !child_started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            owner_panics();
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the owner exits", || {
        node_state(&tree, "owner") == Some(NodeState::Exited)
    })
    .await;
    assert!(
        stopped.load(Ordering::SeqCst),
        "the child should have been stopped before the owner went"
    );

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn children_ignoring_shutdown_are_stopped_before_the_owners_deadline_and_counted() {
    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_shutdown_budget(Duration::from_millis(300));
    sup.add_worker(
        runtime::worker_with_shutdown("owner", |shutdown| async move {
            runtime::worker("stubborn", pending::<()>()).spawn_child();
            shutdown.await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    tx.send(()).unwrap();

    // The owner stops when it is told to stop, but its child never stops. The scope aborts the child itself, and
    // reports the abort.
    let result = join(run).await;
    assert!(
        matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
        "expected exactly the stubborn child to be counted, got {result:?}"
    );
}

#[tokio::test]
async fn owner_ignoring_shutdown_is_cut_off_with_its_children_and_all_counted() {
    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_shutdown_budget(Duration::from_millis(300));
    sup.add_worker(
        runtime::worker("owner", async {
            runtime::worker("stubborn", pending::<()>()).spawn_child();
            pending::<()>().await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    tx.send(()).unwrap();

    // If the scope does not act, the supervisor aborts the owner at its deadline and counts one worker. The child then
    // stops with the owner, but nothing reports it.
    let result = join(run).await;
    assert!(
        matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 2 })),
        "expected both the owner and its child to be counted, got {result:?}"
    );
}

#[tokio::test]
async fn children_of_an_owner_that_is_cut_off_still_get_time_to_stop() {
    let started = flag();
    let stopped = flag();

    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_shutdown_budget(Duration::from_millis(300));
    let (child_started, child_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    sup.add_worker(
        runtime::worker("owner", async move {
            runtime::worker_with_shutdown("child", move |shutdown| async move {
                child_started.store(true, Ordering::SeqCst);
                shutdown.await;
                tokio::time::sleep(Duration::from_millis(5)).await;
                child_stopped.store(true, Ordering::SeqCst);
            })
            .spawn_child();
            pending::<()>().await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child is running", || started.load(Ordering::SeqCst)).await;
    tx.send(()).unwrap();

    // The owner never stops, so its scope cuts it off. The child needs a moment to stop after it is told to stop, and
    // the scope gives it part of the time that remains. Thus, the child stops by itself, and the scope counts only the
    // owner.
    let result = join(run).await;
    assert!(
        matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
        "expected only the owner to be counted, got {result:?}"
    );
    assert!(stopped.load(Ordering::SeqCst), "the child must have stopped by itself");
}

#[tokio::test]
async fn owner_told_to_stop_after_its_scope_began_to_close_bounds_its_children() {
    let running = counter();
    let returned = flag();

    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_shutdown_budget(Duration::from_millis(300));
    let (children_running, body_returned) = (Arc::clone(&running), Arc::clone(&returned));
    sup.add_worker(
        runtime::worker("owner", async move {
            for name in ["stubborn_1", "stubborn_2"] {
                let children_running = Arc::clone(&children_running);
                runtime::worker(name, async move {
                    children_running.fetch_add(1, Ordering::SeqCst);
                    pending::<()>().await;
                })
                .spawn_child();
            }
            body_returned.store(true, Ordering::SeqCst);
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the body returned and both children run", || {
        returned.load(Ordering::SeqCst) && running.load(Ordering::SeqCst) == 2
    })
    .await;

    // The scope began to close before the owner was told to stop, so the children have only their own timeouts at
    // first. When the owner is told to stop, its deadline also bounds the children. Thus, the scope stops both children
    // itself and counts them, before the supervisor must abort the owner.
    tx.send(()).unwrap();
    let result = join(run).await;
    assert!(
        matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 2 })),
        "expected both children to be counted, got {result:?}"
    );
}

#[tokio::test]
async fn nested_scope_closes_when_its_guard_is_dropped() {
    let started = flag();
    let stopped = flag();
    let (drop_tx, drop_rx) = oneshot::channel::<()>();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    let (driver_started, driver_stopped) = (Arc::clone(&started), Arc::clone(&stopped));
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            let mut guard = nested("cache");
            assert!(!guard.is_detached());
            guard.spawn(until_stopped("driver", &driver_started, &driver_stopped));

            let _ = drop_rx.await;
            drop(guard);
            shutdown.await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;

    // In the tree, the children of the nested scope appear under a node for that scope, and that node is under the
    // owner.
    wait_until("the driver runs under the nested scope", || {
        let root = tree.snapshot().root;
        let owner = find(&root, "owner").cloned();
        owner
            .and_then(|owner| find(&owner, "cache").cloned())
            .is_some_and(|cache| find(&cache, "driver").is_some())
    })
    .await;

    drop_tx.send(()).unwrap();
    wait_until("dropping the guard stops the driver", || stopped.load(Ordering::SeqCst)).await;
    wait_until("the nested scope is gone", || node_state(&tree, "cache").is_none()).await;
    assert_eq!(node_state(&tree, "owner"), Some(NodeState::Running));

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn nested_scope_closed_while_its_owner_stops_holds_its_children_to_the_owners_deadline() {
    let running = flag();
    let (deadlines_tx, deadlines_rx) = oneshot::channel();

    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_shutdown_budget(Duration::from_millis(300));
    let driver_running = Arc::clone(&running);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            let (driver_tx, driver_rx) = oneshot::channel();
            let mut guard = nested("cache");
            guard.spawn(
                runtime::worker_with_shutdown("driver", move |shutdown| async move {
                    driver_running.store(true, Ordering::SeqCst);
                    tokio::pin!(shutdown);
                    (&mut shutdown).await;
                    let _ = driver_tx.send(shutdown.deadline());
                })
                .build(),
            );

            tokio::pin!(shutdown);
            (&mut shutdown).await;
            let owner_deadline = shutdown.deadline();

            // The owner drops the guard while it stops. Nothing told the host of the nested scope to stop, but the
            // owner was told to stop, so the children of the nested scope must stop before the owner does.
            drop(guard);
            let driver_deadline = driver_rx.await.ok().flatten();
            let _ = deadlines_tx.send((owner_deadline, driver_deadline));
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the driver is running", || running.load(Ordering::SeqCst)).await;
    tx.send(()).unwrap();

    let (owner_deadline, driver_deadline) = timeout(RUN_TIMEOUT, deadlines_rx)
        .await
        .expect("the owner should report the deadlines")
        .expect("the owner should not be cut off");
    let owner_deadline = owner_deadline.expect("the owner was told to stop with a deadline");
    let driver_deadline = driver_deadline.expect("the driver was told to stop with a deadline");
    assert!(
        driver_deadline < owner_deadline,
        "the driver must be told to stop before the deadline of its owner"
    );

    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn nested_scope_closed_while_its_owner_stops_counts_the_children_that_it_aborts() {
    let running = flag();

    let mut sup = Supervisor::new("sup")
        .unwrap()
        .with_shutdown_budget(Duration::from_secs(1));
    let driver_running = Arc::clone(&running);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            let (gone_tx, gone_rx) = oneshot::channel::<()>();
            let mut guard = nested("cache");
            guard.spawn(
                runtime::worker("driver", async move {
                    // The driver ignores the signal to stop. The sender is dropped when the driver is aborted.
                    let _gone = gone_tx;
                    driver_running.store(true, Ordering::SeqCst);
                    pending::<()>().await;
                })
                .build(),
            );

            shutdown.await;

            // The owner drops the guard while it stops, so the host of the nested scope aborts the driver before the
            // deadline of the owner. The owner continues to run after the host exits. Thus, the host reports the abort
            // while the scope of the owner is still open, and the abort must still be counted.
            drop(guard);
            let _ = gone_rx.await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the driver is running", || running.load(Ordering::SeqCst)).await;
    tx.send(()).unwrap();

    let result = join(run).await;
    assert!(
        matches!(result, Err(SupervisorError::ShutdownTimedOut { aborted: 1 })),
        "expected the aborted driver to be counted, got {result:?}"
    );
}

#[tokio::test]
async fn nested_scope_whose_host_cannot_start_drops_its_children() {
    let (scope_tx, scope_rx) = oneshot::channel();

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    sup.add_worker(
        runtime::worker("owner", async move {
            let _ = scope_tx.send(current().expect("a supervised worker has a scope"));
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    let owner_scope = scope_rx.await.unwrap();
    wait_until("the owner finishes, and its scope closes", || {
        node_state(&tree, "owner") == Some(NodeState::Exited)
    })
    .await;

    // The host of the nested scope cannot start in the closed scope of the owner, and nothing else can adopt the nested
    // scope. Thus, the nested scope must drop the children that are spawned into it, and not hold them.
    let mut guard = owner_scope.enter_sync(|| nested("cache"));
    let token = Arc::new(());
    let child_token = Arc::clone(&token);
    guard.spawn(
        runtime::worker("driver", async move {
            drop(child_token);
        })
        .build(),
    );
    assert_eq!(Arc::strong_count(&token), 1, "the child must be dropped");

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn detached_child_runs_on_the_runtime_that_it_was_placed_on() {
    let pool = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("detached-pool")
        .enable_all()
        .build()
        .unwrap();

    let (thread_tx, thread_rx) = oneshot::channel();
    let id = runtime::worker("child", async move {
        let _ = thread_tx.send(std::thread::current().name().map(String::from));
    })
    .on_runtime(pool.handle().clone())
    .spawn_child_or_detached();
    assert!(
        id.is_none(),
        "there is no scope to spawn into, so the child runs detached"
    );

    let thread = timeout(RUN_TIMEOUT, thread_rx)
        .await
        .expect("the detached child should run")
        .expect("the detached child should report its thread");
    assert_eq!(thread.as_deref(), Some("detached-pool"));

    pool.shutdown_background();
}

#[tokio::test]
async fn detached_guard_signals_its_children_when_dropped() {
    let started = flag();
    let stopped = flag();

    let mut guard = nested_or_detached("cache");
    assert!(guard.is_detached());
    assert!(guard.spawn(until_stopped("driver", &started, &stopped)).is_none());

    wait_until("the detached driver runs", || started.load(Ordering::SeqCst)).await;
    drop(guard);
    wait_until("dropping the guard signals the driver", || {
        stopped.load(Ordering::SeqCst)
    })
    .await;
}

#[tokio::test]
async fn current_scope_can_be_spawned_into_from_another_task() {
    let ran = flag();

    let mut sup = Supervisor::new("sup").unwrap();
    let child_ran = Arc::clone(&ran);
    sup.add_worker(
        runtime::worker_with_shutdown("owner", move |shutdown| async move {
            let scope = current().expect("a supervised worker has a scope");
            tokio::spawn(async move {
                scope.spawn(
                    runtime::worker("child", async move {
                        child_ran.store(true, Ordering::SeqCst);
                    })
                    .build(),
                );
            });
            shutdown.await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child spawned from another task runs", || {
        ran.load(Ordering::SeqCst)
    })
    .await;

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn precreated_scope_holds_its_children_until_adopted() {
    let started = flag();
    let stopped = flag();

    // The test spawns the child before anything can run it.
    let scope = Scope::new("pre");
    scope.spawn(until_stopped("child", &started, &stopped));
    tokio::task::yield_now().await;
    assert!(!started.load(Ordering::SeqCst));

    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    sup.add_worker(scope.into_host());

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the held child starts once the scope is adopted", || {
        started.load(Ordering::SeqCst)
    })
    .await;
    let root = tree.snapshot().root;
    let host = find(&root, "pre").expect("the host is in the tree");
    assert!(
        find(host, "child").is_some(),
        "the child is shown beneath the host that adopted it"
    );

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
    assert!(stopped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn restarted_adopter_gets_a_new_scope() {
    let starts = counter();
    let child_runs = counter();

    let mut sup = Supervisor::new("sup").unwrap();
    sup.add_worker(
        runtime::supervisable(RespawningOwner {
            starts: Arc::clone(&starts),
            child_runs: Arc::clone(&child_runs),
        })
        .build()
        .adopting(Scope::new("pre")),
    );

    let (tx, run) = run_with_trigger(sup).await;

    // The first instance adopted the pre-created scope, and the scope closed when that instance failed. The restarted
    // instance cannot adopt the scope again, so it gets a new scope, and the child that it spawns still runs.
    wait_until("the child of the restarted owner runs", || {
        child_runs.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(starts.load(Ordering::SeqCst), 2);

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn children_spawned_while_a_scope_is_entered_go_into_it() {
    let ran = flag();

    let scope = Scope::new("pre");
    let child_ran = Arc::clone(&ran);
    scope.enter_sync(|| {
        runtime::worker("child", async move {
            child_ran.store(true, Ordering::SeqCst);
        })
        .spawn_child();
    });

    let mut sup = Supervisor::new("sup").unwrap();
    sup.add_worker(scope.into_host());

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the child spawned into the entered scope runs", || {
        ran.load(Ordering::SeqCst)
    })
    .await;

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn ambient_spawn_from_a_scope_child_still_lands_on_the_supervisor() {
    let mut sup = Supervisor::new("sup").unwrap();
    let handle = sup.handle();
    sup.add_worker(
        runtime::worker_with_shutdown("owner", |shutdown| async move {
            runtime::worker_with_shutdown("child", |shutdown| async move {
                runtime::worker_with_shutdown("sibling", |shutdown| shutdown).spawn();
                shutdown.await;
            })
            .spawn_child();
            shutdown.await;
        })
        .build(),
    );

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the sibling is running on the supervisor", || {
        handle.active_children() == 1
    })
    .await;

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn a_worker_that_never_spawns_has_no_children_in_the_tree() {
    let mut sup = Supervisor::new("sup").unwrap();
    let tree = sup.tree_handle();
    sup.add_worker(runtime::worker_with_shutdown("owner", |shutdown| shutdown).build());

    let (tx, run) = run_with_trigger(sup).await;
    wait_until("the owner is running", || {
        node_state(&tree, "owner") == Some(NodeState::Running)
    })
    .await;
    let root = tree.snapshot().root;
    assert!(find(&root, "owner").unwrap().children.is_empty());

    tx.send(()).unwrap();
    assert!(join(run).await.is_ok());
}

#[tokio::test]
async fn reap_deadline_leaves_room_ahead_of_the_owners_deadline() {
    let now = Instant::now();

    // If a large amount of time remains, the full margin is reserved.
    let far = now + Duration::from_secs(10);
    assert_eq!(drive::reap_deadline(far, now), far - Duration::from_millis(50));

    // If little time remains, half of the remaining time is reserved. Thus, a nested scope still leaves time for the
    // scope above it.
    let near = now + Duration::from_millis(20);
    assert_eq!(drive::reap_deadline(near, now), near - Duration::from_millis(10));

    // If the deadline is already in the past, no time remains to reserve.
    assert_eq!(drive::reap_deadline(now, now + Duration::from_secs(1)), now);
}
