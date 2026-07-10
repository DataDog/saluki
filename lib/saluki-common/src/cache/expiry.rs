use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam_queue::ArrayQueue;
use quick_cache::Lifecycle;
use saluki_metrics::reexport::metrics::Counter;

use crate::{
    collections::FastHashMap,
    time::{get_coarse_unix_timestamp, get_unix_timestamp},
};

/// Builder for creating an expiration configuration.
pub struct ExpirationBuilder<K> {
    time_to_idle: Option<Duration>,
    items_evicted: Counter,
    _key: PhantomData<K>,
}

impl<K> ExpirationBuilder<K>
where
    K: Eq + std::hash::Hash,
{
    /// Creates a new `ExpirationBuilder`.
    pub fn new(items_evicted: Counter) -> Self {
        Self {
            time_to_idle: None,
            items_evicted,
            _key: PhantomData,
        }
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
    pub fn build(self) -> (Expiration<K>, ExpiryCapableLifecycle<K>) {
        match self.time_to_idle {
            None => (
                Expiration::disabled(),
                ExpiryCapableLifecycle::disabled(self.items_evicted),
            ),
            Some(time_to_idle) => {
                let state = Arc::new(State::new(time_to_idle));
                let expiration = Expiration::from_state(Arc::clone(&state));
                let lifecycle = ExpiryCapableLifecycle::with_state(state, self.items_evicted);

                (expiration, lifecycle)
            }
        }
    }
}

enum ExpirationOp<K> {
    /// The item was accessed.
    Accessed(K),

    /// The item was removed.
    ///
    /// It may have been removed manually, or due to eviction.
    Removed(K),
}

#[derive(Debug)]
struct AccessState {
    last_accessed: u64,
}

impl AccessState {
    fn new() -> Self {
        Self {
            last_accessed: get_coarse_unix_timestamp(),
        }
    }

    fn mark_accessed(&mut self) {
        self.last_accessed = get_coarse_unix_timestamp();
    }

    fn accessed_since(&self, since: u64) -> bool {
        self.last_accessed >= since
    }
}

#[derive(Debug)]
struct Inner<K> {
    last_seen: FastHashMap<K, AccessState>,
    time_to_idle: Duration,
}

impl<K> Inner<K>
where
    K: Eq + std::hash::Hash,
{
    fn process_operation(&mut self, op: ExpirationOp<K>) {
        match op {
            ExpirationOp::Accessed(key) => match self.last_seen.get_mut(&key) {
                Some(state) => state.mark_accessed(),
                None => {
                    self.last_seen.insert(key, AccessState::new());
                }
            },
            ExpirationOp::Removed(key) => {
                // When an entry is removed, we also remove it from the last seen map.
                self.last_seen.remove(&key);
            }
        }
    }
}

#[derive(Debug)]
struct State<K> {
    inner: Mutex<Inner<K>>,
    pending_ops: ArrayQueue<ExpirationOp<K>>,
}

impl<K> State<K>
where
    K: Eq + std::hash::Hash,
{
    fn new(time_to_idle: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                last_seen: FastHashMap::default(),
                time_to_idle,
            }),
            pending_ops: ArrayQueue::new(128),
        }
    }

    fn mark_entry_accessed(&self, key: K) {
        if let Err(op) = self.pending_ops.push(ExpirationOp::Accessed(key)) {
            self.process_pending_operations(Some(op));
        }
    }

    fn mark_entry_removed(&self, key: K) {
        if let Err(op) = self.pending_ops.push(ExpirationOp::Removed(key)) {
            self.process_pending_operations(Some(op));
        }
    }

    fn process_pending_operations(&self, pending_op: Option<ExpirationOp<K>>) {
        let mut inner = self.inner.lock().unwrap();
        self.process_pending_operations_with_inner(&mut inner, pending_op);
    }

    fn process_pending_operations_with_inner(&self, inner: &mut Inner<K>, pending_op: Option<ExpirationOp<K>>) {
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
pub struct Expiration<K> {
    state: Option<Arc<State<K>>>,
}

impl<K> Expiration<K>
where
    K: Eq + std::hash::Hash,
{
    fn disabled() -> Self {
        Self { state: None }
    }

    fn from_state(state: Arc<State<K>>) -> Self {
        Self { state: Some(state) }
    }

    pub fn mark_entry_accessed(&self, key: K) {
        if let Some(state) = self.state.as_ref() {
            state.mark_entry_accessed(key);
        }
    }

    pub fn mark_entry_removed(&self, key: K) {
        if let Some(state) = self.state.as_ref() {
            state.mark_entry_removed(key);
        }
    }

    pub fn drain_expired_items(&self, entries: &mut Vec<K>) {
        if let Some(state) = self.state.as_ref() {
            let mut inner = state.inner.lock().unwrap();

            // With the inner lock held, process any pending operations first to ensure that the state is up-to-date.
            state.process_pending_operations_with_inner(&mut inner, None);

            // Calculate the cutoff time for entries to be considered expired.
            //
            // Our math here shouldn't ever fail, but if it does, we just return and wait for the next attempt.
            let now = get_unix_timestamp();
            let last_seen_cutoff = match now.checked_sub(inner.time_to_idle.as_secs()) {
                Some(cutoff) => cutoff,
                None => return,
            };

            let expired_entries = inner
                .last_seen
                .extract_if(|_, state| !state.accessed_since(last_seen_cutoff));
            entries.extend(expired_entries.map(|(k, _)| k));
        }
    }
}

/// A cache lifecycle implementation to track when entries are evicted.
///
/// This lifecycle implementation is used for [`quick_cache`] to ensure that we collect eviction events so that those
/// entries can be removed from expiration tracking.
#[derive(Clone)]
pub(super) struct ExpiryCapableLifecycle<K> {
    state: Option<Arc<State<K>>>,
    items_evicted: Counter,
}

impl<K> ExpiryCapableLifecycle<K> {
    fn disabled(items_evicted: Counter) -> Self {
        Self {
            state: None,
            items_evicted,
        }
    }

    fn with_state(state: Arc<State<K>>, items_evicted: Counter) -> Self {
        Self {
            state: Some(state),
            items_evicted,
        }
    }
}

impl<K, V> Lifecycle<K, V> for ExpiryCapableLifecycle<K>
where
    K: Eq + std::hash::Hash,
{
    type RequestState = ();

    #[inline]
    fn begin_request(&self) -> Self::RequestState {}

    #[inline]
    fn on_evict(&self, _state: &mut Self::RequestState, key: K, _value: V) {
        // Note: this fires for all capacity-driven evictions including rejected overweight
        // inserts (items whose weight exceeds the cache capacity). Callers using custom
        // weighters may see slight inflation from rejected inserts.
        self.items_evicted.increment(1);
        if let Some(state) = self.state.as_ref() {
            state.mark_entry_removed(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Arc,
    };

    use quick_cache::Lifecycle;
    use saluki_metrics::reexport::metrics::{Counter, CounterFn};

    use super::*;

    /// A `CounterFn` that records the running total so tests can assert the documented
    /// eviction-counter side effects without installing a full metrics recorder.
    #[derive(Default)]
    struct RecordingCounter(AtomicU64);

    impl CounterFn for RecordingCounter {
        fn increment(&self, value: u64) {
            self.0.fetch_add(value, SeqCst);
        }

        fn absolute(&self, value: u64) {
            self.0.store(value, SeqCst);
        }
    }

    /// Builds a `Counter` alongside a handle for reading its current total.
    fn recording_counter() -> (Counter, Arc<RecordingCounter>) {
        let storage = Arc::new(RecordingCounter::default());
        (Counter::from_arc(Arc::clone(&storage)), storage)
    }

    /// Invokes the lifecycle's `on_evict` for the given key, using `()` as the value type.
    fn trigger_eviction<K>(lifecycle: &ExpiryCapableLifecycle<K>, key: K)
    where
        K: Eq + std::hash::Hash,
    {
        <ExpiryCapableLifecycle<K> as Lifecycle<K, ()>>::on_evict(lifecycle, &mut (), key, ());
    }

    #[test]
    fn disabled_expiration_never_drains() {
        let (counter, _storage) = recording_counter();
        let (expiration, _lifecycle) = ExpirationBuilder::<&str>::new(counter).build();

        expiration.mark_entry_accessed("only");

        let mut expired = Vec::new();
        expiration.drain_expired_items(&mut expired);
        assert!(
            expired.is_empty(),
            "expiration is disabled, so nothing should ever drain"
        );
    }

    #[test]
    fn time_to_idle_drains_stale_entries_and_retains_fresh() {
        let (counter, _storage) = recording_counter();
        let (expiration, _lifecycle) = ExpirationBuilder::<&str>::new(counter)
            .with_time_to_idle(Duration::from_secs(60))
            .build();

        expiration.mark_entry_accessed("fresh");
        expiration.mark_entry_accessed("stale");

        // The first drain flushes the queued access ops into the last-seen map. Both entries were
        // just accessed, so neither is beyond the 60s time-to-idle window yet.
        let mut expired = Vec::new();
        expiration.drain_expired_items(&mut expired);
        assert!(expired.is_empty(), "freshly accessed entries must not expire");

        // Force one entry's last-access timestamp far into the past, beyond the time-to-idle window,
        // while leaving the other untouched.
        {
            let state = expiration.state.as_ref().expect("expiration is enabled");
            let mut inner = state.inner.lock().unwrap();
            inner
                .last_seen
                .get_mut("stale")
                .expect("stale entry is tracked")
                .last_accessed = 0;
        }

        expiration.drain_expired_items(&mut expired);
        assert_eq!(expired, vec!["stale"], "only the idle entry should be drained");

        // The idle entry is removed from tracking; the fresh entry remains tracked.
        let state = expiration.state.as_ref().unwrap();
        let inner = state.inner.lock().unwrap();
        assert!(inner.last_seen.contains_key("fresh"));
        assert!(!inner.last_seen.contains_key("stale"));
    }

    #[test]
    fn eviction_increments_counter_and_untracks_entry() {
        let (counter, storage) = recording_counter();
        let (expiration, lifecycle) = ExpirationBuilder::<&str>::new(counter)
            .with_time_to_idle(Duration::from_secs(60))
            .build();

        // Track an entry, then flush the queued access op into the last-seen map.
        expiration.mark_entry_accessed("evicted");
        let mut expired = Vec::new();
        expiration.drain_expired_items(&mut expired);
        assert!(expired.is_empty());

        // Simulate the underlying cache evicting the entry: the counter is bumped and the entry is
        // queued for removal from expiration tracking.
        trigger_eviction(&lifecycle, "evicted");
        assert_eq!(
            storage.0.load(SeqCst),
            1,
            "on_evict must increment the eviction counter"
        );

        // Draining processes the queued removal, so the evicted key is no longer tracked.
        expiration.drain_expired_items(&mut expired);
        assert!(expired.is_empty());

        let state = expiration.state.as_ref().unwrap();
        let inner = state.inner.lock().unwrap();
        assert!(
            !inner.last_seen.contains_key("evicted"),
            "eviction must untrack the entry"
        );
    }

    #[test]
    fn eviction_counter_increments_even_when_expiration_disabled() {
        // The documented contract is that `on_evict` counts every capacity-driven eviction, even
        // when time-to-idle tracking is disabled (the lifecycle then has no state to update).
        let (counter, storage) = recording_counter();
        let lifecycle = ExpiryCapableLifecycle::<&str>::disabled(counter);

        trigger_eviction(&lifecycle, "one");
        trigger_eviction(&lifecycle, "two");

        assert_eq!(storage.0.load(SeqCst), 2);
    }
}
