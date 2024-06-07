use std::{
    sync::{
        atomic::{
            AtomicBool,
            Ordering::{AcqRel, Acquire},
        },
        Arc,
    },
    time::Duration,
};

use tokio::sync::Notify;
use tracing::debug;

use crate::MemoryGrant;

struct State {
    gate_closed: AtomicBool,
    notifier: Notify,
}

impl State {
    /// Returns `false` if the gate was already closed.
    fn close_gate(&self) -> bool {
        !self.gate_closed.swap(true, AcqRel)
    }

    /// Returns `false`	if the gate was already open.
    fn open_gate(&self) -> bool {
        if self.gate_closed.swap(false, AcqRel) {
            self.notifier.notify_waiters();
            true
        } else {
            false
        }
    }

    async fn wait_for_gate(&self) {
        if self.gate_closed.load(Acquire) {
            // The gate is closed, but we want to be sure we're not just catching it right before it opens, since we'd
            // miss the wake up.
            //
            // We grab our notify future, which when used when `notify_waiters` is **guaranteed** to be woken up as soon
            // as it is constructed, and then we check `gate_closed` again. If it's still `true`, then we know we need
            // to wait to be notified. Otherwise, we can skip waiting for a notification entirely.
            let notified = self.notifier.notified();
            if self.gate_closed.load(Acquire) {
                notified.await;
            }
        }
    }
}

/// A process-wide memory limiter.
///
/// In many cases, it can be useful to know when the process has exceeded a certain memory usage threshold in order to
/// be able to react that: clearing caches, temporarily blocking requests, and so on.
///
/// `MemoryLimiter` watches the process's physical memory usage (Resident Set Size/Working Set) and keeps track of when
/// the usage exceeds the configured limit. This is coupled with an underlying mechanism for allowing interested parties
/// to wait for the memory usage to drop below the limit by calling `wait_for_capacity`.
///
/// When the limit has not been exceeded, these calls return immediately. However, when the limit has been exceeded,
/// they will block until the memory usage drops below the limit. This allows components to cooperatively participate in
/// trying to stay below the configured limit in a straightforward way.
#[derive(Clone)]
pub struct MemoryLimiter {
    state: Arc<State>,
}

impl MemoryLimiter {
    /// Creates a new `MemoryLimiter` based on the given `MemoryGrant`.
    ///
    /// A background task is spawned that frequently checks the used memory for the entire process and will block
    /// callers of `wait_for_capacity` until the used memory is below the effective limit of `grant.`
    pub fn new(grant: MemoryGrant) -> Option<Self> {
        if memory_stats::memory_stats().is_none() {
            return None;
        }

        let state = Arc::new(State {
            gate_closed: AtomicBool::new(false),
            notifier: Notify::new(),
        });

        let state2 = Arc::clone(&state);

        std::thread::Builder::new()
            .name("memory-limiter-checker".to_string())
            .spawn(move || check_memory_usage(state2, grant))
            .unwrap();

        Some(Self { state })
    }

    /// Creates a no-op `MemoryLimiter` that does not perform any limiting.
    ///
    /// All calls to `wait_for_capacity` will return immediately.
    pub fn noop() -> Self {
        Self {
            state: Arc::new(State {
                gate_closed: AtomicBool::new(false),
                notifier: Notify::new(),
            }),
        }
    }

    /// Checks the limiter to ensure there is available memory capacity.
    ///
    /// If there is no capacity currently, it waits until capacity becomes available.
    pub async fn wait_for_capacity(&self) {
        self.state.wait_for_gate().await;
    }
}

fn check_memory_usage(state: Arc<State>, grant: MemoryGrant) {
    // We use the _initial_ limit here, because despite the doc comment for it, it should represent the high-level limit
    // given to the process... which is what we want to limit against. The effective limit is calculated for the purpose
    // of validating bounds because we know we're likely to exceed it given the lossy approach to defining them by hand.
    let rss_limit = grant.initial_limit_bytes();
    debug!(rss_limit, "Memory limiter checker started.");

    loop {
        // Check the physical memory (RSS, or Working Set in Windows) and close the gate if we're over the limit.
        //
        // When we close the gate, or when the gate is still closed, we'll check more frequently since the implication
        // is that we're actively blocking components from doing further work, so we want to let them get back to work
        // as soon as possible... but otherwise, we check less frequently just to avoid constantly flapping between
        // open/closed if we're close to the limit.
        let mem_stats = memory_stats::memory_stats().expect("memory statistics should be available");
        let recheck_duration = if mem_stats.physical_mem > rss_limit {
            debug!(
                rss_limit,
                actual_rss = mem_stats.physical_mem,
                "Memory usage exceeded limit. Gate closed."
            );
            if state.close_gate() {}
            Duration::from_millis(250)
        } else {
            debug!(
                rss_limit,
                actual_rss = mem_stats.physical_mem,
                "Memory usage below limit. Gate opened."
            );
            if state.open_gate() {}
            Duration::from_millis(1000)
        };

        std::thread::sleep(recheck_duration);
    }
}
