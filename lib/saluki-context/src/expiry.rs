use std::{
    sync::{
        atomic::{
            AtomicU32, AtomicU64,
            Ordering::{Acquire, Relaxed, Release},
        },
        Arc,
    },
    time::{Duration, SystemTime},
};

use crossbeam_queue::ArrayQueue;
use papaya::{HashMap, LocalGuard};
use quick_cache::Lifecycle;

use crate::hash::ContextKey;

/// Builder for creating an expiration configuration.
pub struct ExpirationBuilder {
    time_to_idle: Option<Duration>,
}

impl ExpirationBuilder {
    /// Creates a new `ExpirationBuilder`.
    pub fn new() -> Self {
        Self { time_to_idle: None }
    }

    /// Sets the time-to-idle for entries in the cache.
    ///
    /// This controls how long a context will be kept in the cache after its last access or creation time. A lower
    /// time-to-idle will prioritize the reclamation of memory used the resolver cache, ensuring memory usage stays low,
    /// but will potentially lead to a lower hit rate for contexts. A higher time-to-idle will improve the hit rate, and
    /// prioritize the _stability_ of memory usage over time, even though it may hold on to memory for longer than
    /// necessary based on how often the cached contexts are accessed.
    ///
    /// Defaults to no expiration.
    pub fn with_time_to_idle(mut self, time_to_idle: Duration) -> Self {
        self.time_to_idle = Some(time_to_idle);
        self
    }

    /// Builds the expiration configuration.
    pub fn build(self) -> (Expiration, ExpiryCapableLifecycle) {
        match self.time_to_idle {
            None => (Expiration::disabled(), ExpiryCapableLifecycle::disabled()),
            Some(time_to_idle) => {
                let state = Arc::new(State::new(time_to_idle));
                let expiration = Expiration::from_state(Arc::clone(&state));
                let lifecycle = ExpiryCapableLifecycle::from_state(state);

                (expiration, lifecycle)
            }
        }
    }
}

enum ExpirationOp {
    Accessed(ContextKey),
    Evicted(ContextKey),
}

#[derive(Debug)]
struct AccessState {
    accesses: AtomicU32,
    last_accessed: AtomicU64,
}

impl AccessState {
    fn new() -> Self {
        Self {
            accesses: AtomicU32::new(0),
            last_accessed: AtomicU64::new(0),
        }
    }

    fn mark_accessed(&self) {
        self.accesses.fetch_add(1, Relaxed);
    }

    fn update_last_changed(&self, now: u64) {
        if self.accesses.swap(0, Relaxed) > 0 {
            self.last_accessed.store(now, Release);
        }
    }

    fn accessed_since(&self, since: u64) -> bool {
        self.last_accessed.load(Acquire) >= since
    }
}

#[derive(Debug)]
struct State {
    last_seen: HashMap<ContextKey, AccessState, ahash::RandomState>,
    time_to_idle: Duration,
    pending_ops: ArrayQueue<ExpirationOp>,
}

impl State {
    fn new(time_to_idle: Duration) -> Self {
        Self {
            last_seen: HashMap::with_hasher(ahash::RandomState::default()),
            time_to_idle,
            pending_ops: ArrayQueue::new(128),
        }
    }

    fn mark_entry_accessed(&self, key: ContextKey) {
        if let Err(op) = self.pending_ops.push(ExpirationOp::Accessed(key)) {
            self.process_pending_operations(Some(op));
        }
    }

    fn mark_entry_evicted(&self, key: ContextKey) {
        if let Err(op) = self.pending_ops.push(ExpirationOp::Evicted(key)) {
            self.process_pending_operations(Some(op));
        }
    }

    fn process_pending_operations(&self, pending_op: Option<ExpirationOp>) {
        let guard = self.last_seen.guard();
        while let Some(op) = self.pending_ops.pop() {
            self.process_operation(&guard, op);
        }

        if let Some(pending_op) = pending_op {
            self.process_operation(&guard, pending_op);
        }
    }

    fn process_operation(&self, guard: &LocalGuard<'_>, op: ExpirationOp) {
        match op {
            ExpirationOp::Accessed(key) => {
                let state = self.last_seen.get_or_insert_with(key, AccessState::new, guard);
                state.mark_accessed();
            }
            ExpirationOp::Evicted(key) => {
                self.last_seen.remove(&key, guard);
            }
        }
    }
}

/// Expiration management.
///
/// This type provides an interface the core expiration logic, allowing the marking of entries when they're accessed as
/// well as determining when a tracked entry has expired so it can be removed from the cache.
#[derive(Clone, Debug)]
pub struct Expiration {
    state: Option<Arc<State>>,
}

impl Expiration {
    fn disabled() -> Self {
        Self { state: None }
    }

    fn from_state(state: Arc<State>) -> Self {
        Self { state: Some(state) }
    }

    pub fn mark_entry_accessed(&self, key: ContextKey) {
        if let Some(state) = self.state.as_ref() {
            state.mark_entry_accessed(key);
        }
    }

    pub fn drain_expired_entries(&self, entries: &mut Vec<ContextKey>) {
        if let Some(state) = self.state.as_ref() {
            // Calculate the cutoff time for entries to be considered expired.
            //
            // Our math here shouldn't ever fail, but if it does, we just return and wait for the next attempt.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let last_seen_cutoff = match now.checked_sub(state.time_to_idle.as_secs()) {
                Some(cutoff) => cutoff,
                None => return,
            };

            // For every entry, handle updating the last accessed time and checking if it should be expired.
            //
            // If it should be expired, we'll add it to the expired entries list so it can be removed from the cache,
            // and we'll also remove it from the last seen map.
            state.last_seen.pin().retain(|k, state| {
                state.update_last_changed(now);

                let should_expire = !state.accessed_since(last_seen_cutoff);
                if should_expire {
                    entries.push(*k);
                }

                // We have to invert the value since retain will keep the entry if this returns `true`.
                !should_expire
            });
        }
    }
}

/// A cache lifecycle implementation to track when entries are evicted.
///
/// This lifecycle implementation is used for [`quick_cache`] to ensure that we collect eviction events so that those
/// entries can be removed from expiration tracking.
#[derive(Clone)]
pub struct ExpiryCapableLifecycle {
    state: Option<Arc<State>>,
}

impl ExpiryCapableLifecycle {
    fn disabled() -> Self {
        Self { state: None }
    }

    fn from_state(state: Arc<State>) -> Self {
        Self { state: Some(state) }
    }
}

impl<V> Lifecycle<ContextKey, V> for ExpiryCapableLifecycle {
    type RequestState = ();

    #[inline]
    fn begin_request(&self) -> Self::RequestState {}

    #[inline]
    fn on_evict(&self, _state: &mut Self::RequestState, key: ContextKey, _value: V) {
        if let Some(state) = self.state.as_ref() {
            state.mark_entry_evicted(key);
        }
    }
}
