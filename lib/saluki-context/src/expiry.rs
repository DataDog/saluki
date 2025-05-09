use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use crossbeam_queue::ArrayQueue;
use quick_cache::Lifecycle;
use saluki_common::collections::PrehashedHashMap;

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
    accesses: u32,
    last_accessed: u64,
}

impl AccessState {
    fn new() -> Self {
        Self {
            accesses: 1,
            last_accessed: 0,
        }
    }

    fn mark_accessed(&mut self) {
        self.accesses += 1;
    }

    fn update_last_changed(&mut self, now: u64) {
        if self.accesses > 0 {
            self.last_accessed = now;
            self.accesses = 0;
        }
    }

    fn accessed_since(&self, since: u64) -> bool {
        self.last_accessed >= since
    }
}

#[derive(Debug)]
struct Inner {
    last_seen: PrehashedHashMap<ContextKey, AccessState>,
    time_to_idle: Duration,
}

impl Inner {
    fn process_operation(&mut self, op: ExpirationOp) {
        match op {
            ExpirationOp::Accessed(key) => match self.last_seen.get_mut(&key) {
                Some(state) => state.mark_accessed(),
                None => {
                    self.last_seen.insert(key, AccessState::new());
                }
            },
            ExpirationOp::Evicted(key) => {
                self.last_seen.remove(&key);
            }
        }
    }
}

#[derive(Debug)]
struct State {
    inner: Mutex<Inner>,
    pending_ops: ArrayQueue<ExpirationOp>,
}

impl State {
    fn new(time_to_idle: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                last_seen: PrehashedHashMap::default(),
                time_to_idle,
            }),
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
        let mut inner = self.inner.lock().unwrap();
        while let Some(op) = self.pending_ops.pop() {
            inner.process_operation(op);
        }

        if let Some(pending_op) = pending_op {
            inner.process_operation(pending_op);
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
            let mut inner = state.inner.lock().unwrap();

            // Calculate the cutoff time for entries to be considered expired.
            //
            // Our math here shouldn't ever fail, but if it does, we just return and wait for the next attempt.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let last_seen_cutoff = match now.checked_sub(inner.time_to_idle.as_secs()) {
                Some(cutoff) => cutoff,
                None => return,
            };

            let expired_entries = inner.last_seen.extract_if(|_, state| {
                state.update_last_changed(now);
                !state.accessed_since(last_seen_cutoff)
            });
            entries.extend(expired_entries.map(|(k, _)| k));
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
