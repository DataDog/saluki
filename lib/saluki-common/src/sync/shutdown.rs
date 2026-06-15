//! Primitives for signalling shutdown.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{
            AtomicBool, AtomicUsize,
            Ordering::{AcqRel, Acquire, Release},
        },
        Arc,
    },
    task::{Context, Poll},
};

use pin_project::{pin_project, pinned_drop};
use tokio::sync::{futures::OwnedNotified, Notify};

#[derive(Default)]
struct ShutdownInner {
    flag: AtomicBool,
    notify: Arc<Notify>,
    outstanding: AtomicUsize,
    all_dropped: Notify,
}

impl ShutdownInner {
    /// Records a newly created handle.
    fn register(&self) {
        self.outstanding.fetch_add(1, Release);
    }

    /// Records a dropped handle, releasing completion waiters once the last handle goes away.
    fn deregister(&self) {
        if self.outstanding.fetch_sub(1, AcqRel) == 1 {
            // Use `notify_waiters` (rather than storing a permit): the last handle may drop _before_ shutdown is
            // signaled, and an emitter only cares about this once it's actually waiting in `signal_and_await_handles`.
            self.all_dropped.notify_waiters();
        }
    }

    /// Signals shutdown, releasing any blocked waiters.
    fn signal(&self) {
        if !self.flag.swap(true, AcqRel) {
            self.notify.notify_waiters();
        }
    }

    /// Signals shutdown and waits until every outstanding handle has been dropped.
    ///
    /// Returns immediately if there are no outstanding handles.
    async fn signal_and_await_handles(&self) {
        // Snapshot the completion notification _before_ signaling, so the last handle dropping can't slip through the
        // gap between signaling and parking.
        let all_dropped = self.all_dropped.notified();

        self.signal();

        if self.outstanding.load(Acquire) == 0 {
            return;
        }

        all_dropped.await;
    }
}

/// A shutdown coordinator for triggering, and optionally waiting for, tasks to shut down.
///
/// Callers register a [`ShutdownHandle`], which can be awaited to observe shutdown being triggered, or checked
/// synchronously via [`ShutdownHandle::is_triggered`]. When a handle is dropped, it reports completion back to the
/// coordinator; this is what allows [`shutdown_and_wait`][Self::shutdown_and_wait] to block until every outstanding
/// handle has finished.
///
/// Shutdown is triggered either fire-and-forget, via [`shutdown`][Self::shutdown], or blocking, via
/// [`shutdown_and_wait`][Self::shutdown_and_wait].
///
/// # Shutdown on drop
///
/// Shutdown is also triggered implicitly on drop if it hasn't already been triggered, to enforce that shutdown always
/// happens even in exceptional circumstances. Shutdown on drop is fire-and-forget: when the coordinator's drop logic
/// has executed, there is no guarantee that all outstanding handles have also been dropped.
#[derive(Default)]
pub struct ShutdownCoordinator {
    state: Arc<ShutdownInner>,
}

impl ShutdownCoordinator {
    /// Registers and returns a new shutdown handle.
    pub fn register(&mut self) -> ShutdownHandle {
        ShutdownHandle::registered(Arc::clone(&self.state))
    }

    /// Signals shutdown to all outstanding handles without waiting.
    ///
    /// When it is desirable to wait until all outstanding handles have completed (been dropped), use
    /// [`shutdown_and_wait`][Self::shutdown_and_wait].
    pub fn shutdown(self) {
        self.state.signal();
    }

    /// Signals shutdown to all outstanding handles, waiting until all handles have completed (been dropped).
    ///
    /// If there are no outstanding handles, this returns immediately.
    pub async fn shutdown_and_wait(self) {
        self.state.signal_and_await_handles().await;
    }
}

impl Drop for ShutdownCoordinator {
    fn drop(&mut self) {
        self.state.signal();
    }
}

/// A handle for observing shutdown from a [`ShutdownCoordinator`].
///
/// The handle is a [`Future`] that resolves once shutdown is triggered, and can also be checked synchronously via
/// [`is_triggered`][Self::is_triggered]. Dropping the handle reports completion back to its coordinator, which is how
/// [`ShutdownCoordinator::shutdown_and_wait`] knows when all outstanding work has finished.
#[pin_project(PinnedDrop)]
pub struct ShutdownHandle {
    state: Arc<ShutdownInner>,

    #[pin]
    notified: OwnedNotified,
}

impl ShutdownHandle {
    /// Creates a handle sharing the given state, registering it as an outstanding handle.
    fn registered(state: Arc<ShutdownInner>) -> Self {
        state.register();

        // Capture our `OwnedNotified` during registration, which ensures that when we eventually poll it, it will be
        // guaranteed that we observe any notifications sent _after_ this point.
        //
        // We couple that invariant with first checking the atomic flag to ensure we never miss wakeups.
        let notify = Arc::clone(&state.notify);
        let notified = notify.notified_owned();

        Self { state, notified }
    }

    /// Creates a shutdown handle that is never triggered.
    ///
    /// This can be useful when a callee requires a handle but no shutdown signal is expected or available to provide.
    pub fn noop() -> Self {
        Self::registered(Arc::new(ShutdownInner::default()))
    }

    /// Creates a coordinator/handle pair.
    ///
    /// This is equivalent to creating [`ShutdownCoordinator`] and calling [`ShutdownCoordinator::register`]. The
    /// resulting coordinator can still be used to register further handles if need be.
    pub fn paired() -> (ShutdownCoordinator, Self) {
        let mut coordinator = ShutdownCoordinator::default();
        let handle = coordinator.register();
        (coordinator, handle)
    }

    /// Returns `true` if shutdown has been triggered.
    pub fn is_triggered(&self) -> bool {
        self.state.flag.load(Acquire)
    }
}

impl Future for ShutdownHandle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // Fast path: if shutdown has already been triggered, return immediately.
        if this.state.flag.load(Acquire) {
            return Poll::Ready(());
        }

        // Wait to be notified that shutdown has been triggered.
        //
        // We don't need to recheck the flag because we know that by creating `OwnedNotified` before
        // checking the flag, any subsequent poll we do that returns as ready can only be the result
        // of the notify happening after setting the flag.
        this.notified.poll(cx)
    }
}

#[pinned_drop]
impl PinnedDrop for ShutdownHandle {
    fn drop(self: Pin<&mut Self>) {
        self.state.deregister();
    }
}

#[cfg(test)]
mod tests {
    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::*;

    #[test]
    fn handle_starts_untriggered() {
        let (_coordinator, handle) = ShutdownHandle::paired();

        assert!(!handle.is_triggered());
    }

    #[test]
    fn handle_triggered_on_coordinator_shutdown() {
        let (coordinator, handle) = ShutdownHandle::paired();
        assert!(!handle.is_triggered());

        coordinator.shutdown();
        assert!(handle.is_triggered());
    }

    #[test]
    fn handle_triggered_on_coordinator_drop() {
        let (coordinator, handle) = ShutdownHandle::paired();
        assert!(!handle.is_triggered());

        drop(coordinator);
        assert!(handle.is_triggered());
    }

    #[test]
    fn handle_wait_returns_immediately_when_already_triggered() {
        let (coordinator, handle) = ShutdownHandle::paired();
        coordinator.shutdown();

        let mut wait = spawn(handle);
        assert_ready!(wait.poll());
    }

    #[test]
    fn handle_completes_after_coordinator_shutdown() {
        let (coordinator, handle) = ShutdownHandle::paired();

        let mut wait = spawn(handle);
        assert_pending!(wait.poll());
        assert!(!wait.is_woken());

        // Triggering shutdown wakes the parked handle and allows it to resolve:
        coordinator.shutdown();
        assert!(wait.is_woken());
        assert_ready!(wait.poll());
    }

    #[test]
    fn handle_completes_after_coordinator_dropped() {
        let (coordinator, handle) = ShutdownHandle::paired();

        let mut wait = spawn(handle);
        assert_pending!(wait.poll());
        assert!(!wait.is_woken());

        // Dropping the coordinator is equivalent to triggering shutdown and should wake the parked handle and allow it
        // to resolve:
        drop(coordinator);
        assert!(wait.is_woken());
        assert_ready!(wait.poll());
    }

    #[test]
    fn handle_is_fused_after_coordinator_shutdown() {
        let (coordinator, handle) = ShutdownHandle::paired();
        coordinator.shutdown();

        // The flag is latched, so every subsequent wait observes it and resolves immediately:
        let mut wait = spawn(handle);
        assert_ready!(wait.poll());
        assert_ready!(wait.poll());
    }

    #[test]
    fn multiple_handles_all_wake_on_shutdown() {
        let mut coordinator = ShutdownCoordinator::default();
        let handle1 = coordinator.register();
        let handle2 = coordinator.register();
        let handle3 = coordinator.register();

        let mut wait1 = spawn(handle1);
        let mut wait2 = spawn(handle2);
        let mut wait3 = spawn(handle3);

        assert_pending!(wait1.poll());
        assert_pending!(wait2.poll());
        assert_pending!(wait3.poll());

        // Triggering shutdown should immediately wake all parked handles and allow them to resolve:
        coordinator.shutdown();
        assert!(wait1.is_woken());
        assert!(wait2.is_woken());
        assert!(wait3.is_woken());

        assert_ready!(wait1.poll());
        assert_ready!(wait2.poll());
        assert_ready!(wait3.poll());
    }

    #[test]
    fn noop_handle_never_completes() {
        let handle = ShutdownHandle::noop();

        // A no-op handle has no trigger, so it reports as untriggered and never resolves:
        assert!(!handle.is_triggered());

        let mut wait = spawn(handle);
        assert_pending!(wait.poll());
        assert_pending!(wait.poll());
        assert!(!wait.is_woken());
    }

    #[test]
    fn coordinator_shutdown_and_wait_returns_immediately_with_no_handles() {
        let coordinator = ShutdownCoordinator::default();

        // With no registered handles, `shutdown_and_wait` resolves on the first poll:
        let mut shutdown = spawn(coordinator.shutdown_and_wait());
        assert_ready!(shutdown.poll());
    }

    #[test]
    fn coordinator_shutdown_and_wait_ignores_handle_dropped_before_shutdown() {
        let mut coordinator = ShutdownCoordinator::default();

        // A handle registered and then dropped _before_ shutdown deregisters itself, so it doesn't count toward
        // outstanding handles:
        let handle = coordinator.register();
        drop(handle);

        // With no outstanding handles remaining, `shutdown_and_wait` resolves on the first poll:
        let mut shutdown = spawn(coordinator.shutdown_and_wait());
        assert_ready!(shutdown.poll());
    }

    #[test]
    fn coordinator_shutdown_waits_for_outstanding_handle_to_drop_single() {
        let mut coordinator = ShutdownCoordinator::default();
        let handle = coordinator.register();

        let mut wait = spawn(handle);
        assert!(!wait.is_woken());
        assert_pending!(wait.poll());

        // The first poll signals the outstanding handle and then parks until that handle is dropped:
        let mut shutdown = spawn(coordinator.shutdown_and_wait());
        assert!(!shutdown.is_woken());
        assert_pending!(shutdown.poll());
        assert!(!shutdown.is_woken());

        // Our handle should now be woken up and able to complete, but the coordinator should _not_ be woken up until
        // the handle is actually dropped:
        assert!(wait.is_woken());
        assert_ready!(wait.poll());

        assert_pending!(shutdown.poll());
        assert!(!shutdown.is_woken());

        drop(wait);

        assert!(shutdown.is_woken());
        assert_ready!(shutdown.poll());
    }

    #[test]
    fn coordinator_shutdown_waits_for_outstanding_handle_to_drop_multiple() {
        let mut coordinator = ShutdownCoordinator::default();
        let handle1 = coordinator.register();
        let handle2 = coordinator.register();

        let mut wait1 = spawn(handle1);
        let mut wait2 = spawn(handle2);

        assert!(!wait1.is_woken());
        assert!(!wait2.is_woken());
        assert_pending!(wait1.poll());
        assert_pending!(wait2.poll());

        // The first poll signals the outstanding handles and then parks until those handles are dropped:
        let mut shutdown = spawn(coordinator.shutdown_and_wait());
        assert!(!shutdown.is_woken());
        assert_pending!(shutdown.poll());
        assert!(!shutdown.is_woken());

        // Our handles should now be woken up and able to complete, but the coordinator should _not_ be woken up until
        // the handles are actually dropped:
        assert!(wait1.is_woken());
        assert_ready!(wait1.poll());

        assert!(wait2.is_woken());
        assert_ready!(wait2.poll());

        assert!(!shutdown.is_woken());
        assert_pending!(shutdown.poll());

        // Drop the first handle, which should not resolve the shutdown yet:
        drop(wait1);

        assert!(!shutdown.is_woken());
        assert_pending!(shutdown.poll());

        // Drop the second handle, which should resolve the shutdown:
        drop(wait2);

        assert!(shutdown.is_woken());
        assert_ready!(shutdown.poll());
    }
}
