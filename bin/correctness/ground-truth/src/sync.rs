use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use tokio::sync::Notify;

struct State {
    notify: Notify,
    count: AtomicUsize,
}

impl State {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }
}

/// An asynchronous task coordinator.
///
/// `Coordinator` allows both tracking when a group of tasks have completed.
#[derive(Clone)]
pub struct Coordinator {
    state: Arc<State>,
}

impl Coordinator {
    /// Creates a new `Coordinator`.
    pub fn new() -> Self {
        Self {
            state: Arc::new(State::new()),
        }
    }

    /// Registers a new coordinated task.
    pub fn register(&mut self) -> TaskToken {
        self.state.count.fetch_add(1, Relaxed);

        TaskToken {
            state: self.state.clone(),
        }
    }

    /// Wait until all coordinated tasks are marked as done.
    pub async fn wait(&mut self) {
        if self.state.count.load(Relaxed) == 0 {
            return;
        }

        self.state.notify.notified().await;
    }
}

/// A token representing a coordinated task.
pub struct TaskToken {
    state: Arc<State>,
}

impl TaskToken {
    /// Mark this task as done.
    ///
    /// If no other tasks are still running, this will wake up the waiting task, if any.
    pub fn done(self) {
        drop(self)
    }
}

impl Drop for TaskToken {
    fn drop(&mut self) {
        // If we're the last worker attached to the coordinator, wake up the waiting task, if any.
        let count = self.state.count.fetch_sub(1, Relaxed);
        if count == 1 {
            self.state.notify.notify_waiters();
        }
    }
}
