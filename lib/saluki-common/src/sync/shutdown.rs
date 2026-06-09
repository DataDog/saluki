//! Primitives for signalling shutdown between synchronous and asynchronous code.

use std::sync::{
    atomic::{
        AtomicBool,
        Ordering::{AcqRel, Acquire},
    },
    Arc,
};

use tokio::sync::Notify;

struct ShutdownInner {
    flag: AtomicBool,
    notify: Notify,
}

/// Shutdown trigger.
///
/// This type allows creating a matched pair of values for emitting and consuming a shutdown signal, both of which can
/// happen either in synchronous or asynchronous code.
///
/// # Drop behavior
///
/// Normally, shutdown is triggered by calling [`ShutdownTrigger::trigger`], but in some cases, the code that make the
/// call may never be reached, even though we still want to try and ensure an orderly shutdown. For that reason,
/// shutdown will _also_ be triggered implicitly on drop if it hasn't already been triggered.
pub struct ShutdownTrigger {
    state: Arc<ShutdownInner>,
}

impl ShutdownTrigger {
    /// Creates a new shutdown trigger/handle pair.
    pub fn new() -> (Self, ShutdownHandle) {
        let state = Arc::new(ShutdownInner {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        });

        (
            Self {
                state: Arc::clone(&state),
            },
            ShutdownHandle { state },
        )
    }

    fn trigger_inner(&self) {
        if !self.state.flag.swap(true, AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    /// Triggers shutdown, releasing any blocked asynchronous waiters.
    pub fn trigger(self) {
        self.trigger_inner();
    }
}

impl Drop for ShutdownTrigger {
    fn drop(&mut self) {
        self.trigger_inner();
    }
}

/// Handle for checking if shutdown has been triggered.
pub struct ShutdownHandle {
    state: Arc<ShutdownInner>,
}

impl ShutdownHandle {
    /// Returns `true` if shutdown has been triggered.
    pub fn is_triggered(&self) -> bool {
        self.state.flag.load(Acquire)
    }

    /// Waits for shutdown to be triggered.
    ///
    /// If shutdown has already been triggered, this returns immediately. This is safe to await concurrently from
    /// multiple tasks: when shutdown is triggered, every waiter is released.
    pub async fn wait_for_shutdown(&self) {
        // Capture our `Notified` _before_ checking the flag, so no wakeup can slip through the gap between the check
        // and parking.
        //
        // Creating the `Notified` snapshots the current `notify_waiters` generation, and `notify_waiters` is delivered
        // to any `Notified` created before that call -- independent of when (or whether) it is polled.
        let notified = self.state.notify.notified();

        if self.is_triggered() {
            return;
        }

        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use super::*;

    #[test]
    fn handle_starts_untriggered() {
        let (_trigger, handle) = ShutdownTrigger::new();

        assert!(!handle.is_triggered());
    }

    #[test]
    fn trigger_sets_flag() {
        let (trigger, handle) = ShutdownTrigger::new();
        assert!(!handle.is_triggered());

        trigger.trigger();
        assert!(handle.is_triggered());
    }

    #[test]
    fn dropping_trigger_sets_flag() {
        let (trigger, handle) = ShutdownTrigger::new();
        assert!(!handle.is_triggered());

        // Dropping the trigger without ever calling `trigger` must still signal shutdown.
        drop(trigger);
        assert!(handle.is_triggered());
    }

    #[test]
    fn wait_returns_immediately_when_already_triggered() {
        let (trigger, handle) = ShutdownTrigger::new();
        trigger.trigger();

        // Shutdown was triggered before we ever waited, so the first poll resolves.
        let mut wait = spawn(handle.wait_for_shutdown());
        assert_ready!(wait.poll());
    }

    #[test]
    fn wait_completes_after_trigger() {
        let (trigger, handle) = ShutdownTrigger::new();

        let mut wait = spawn(handle.wait_for_shutdown());

        // The waiter parks until shutdown is triggered.
        assert_pending!(wait.poll());
        assert!(!wait.is_woken());

        // Triggering shutdown wakes the parked waiter (checked before the next poll, which would clear the flag)...
        trigger.trigger();
        assert!(wait.is_woken());

        // ...and the subsequent poll resolves.
        assert_ready!(wait.poll());
    }

    #[test]
    fn wait_completes_when_trigger_dropped() {
        let (trigger, handle) = ShutdownTrigger::new();

        let mut wait = spawn(handle.wait_for_shutdown());
        assert_pending!(wait.poll());

        // Dropping the trigger is equivalent to triggering it: the parked waiter is woken and resolves.
        drop(trigger);
        assert!(wait.is_woken());
        assert_ready!(wait.poll());
    }

    #[test]
    fn wait_stays_pending_until_triggered() {
        let (trigger, handle) = ShutdownTrigger::new();

        let mut wait = spawn(handle.wait_for_shutdown());

        // Repeated polls before triggering stay pending and don't spuriously wake.
        assert_pending!(wait.poll());
        assert_pending!(wait.poll());
        assert!(!wait.is_woken());

        trigger.trigger();
        assert_ready!(wait.poll());
    }

    #[test]
    fn wait_is_repeatable_after_trigger() {
        let (trigger, handle) = ShutdownTrigger::new();
        trigger.trigger();

        // The flag is latched, so every subsequent wait observes it and resolves immediately.
        let mut first = spawn(handle.wait_for_shutdown());
        assert_ready!(first.poll());

        let mut second = spawn(handle.wait_for_shutdown());
        assert_ready!(second.poll());
    }

    #[test]
    fn multiple_waiters_all_wake_on_trigger() {
        let (trigger, handle) = ShutdownTrigger::new();

        // `wait_for_shutdown` takes `&self`, so several tasks can park on the same handle at once.
        let mut first = spawn(handle.wait_for_shutdown());
        let mut second = spawn(handle.wait_for_shutdown());
        let mut third = spawn(handle.wait_for_shutdown());

        assert_pending!(first.poll());
        assert_pending!(second.poll());
        assert_pending!(third.poll());

        // A single trigger must release *every* parked waiter. This is the property that `notify_one` could not
        // provide -- it would wake exactly one and strand the rest -- and is the reason the trigger uses
        // `notify_waiters`.
        trigger.trigger();
        assert!(first.is_woken());
        assert!(second.is_woken());
        assert!(third.is_woken());

        assert_ready!(first.poll());
        assert_ready!(second.poll());
        assert_ready!(third.poll());
    }

    #[test]
    fn multiple_waiters_after_trigger_return_immediately() {
        let (trigger, handle) = ShutdownTrigger::new();
        trigger.trigger();

        // Waiters that arrive after shutdown was already triggered each observe the latched flag and resolve on the
        // first poll, with no dependence on a notification.
        let mut first = spawn(handle.wait_for_shutdown());
        let mut second = spawn(handle.wait_for_shutdown());

        assert_ready!(first.poll());
        assert_ready!(second.poll());
    }
}
