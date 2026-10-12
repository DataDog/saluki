//! Counters that accumulate locally and are published to shared handles.

use std::sync::{atomic::Ordering, Arc};

use metrics::{atomics::AtomicU64, Counter};

/// Counts of one owner, such as a cache or a resolver, kept apart from the shared telemetry handles they belong to,
/// and added to those handles when the owner publishes.
///
/// Telemetry handles are shared by every owner registered under the same name and labels, so owners used in parallel
/// contend on them when they write on every operation. Each owner instead writes to [local counters][Self::local] of
/// its own, which nothing else writes, and [publishes][Self::publish] what they counted periodically, one write per
/// handle. Whatever is left is published when the counts are dropped.
pub(crate) struct AccumulatedCounters {
    counts: Vec<Arc<AtomicU64>>,
    handles: Vec<Counter>,
}

impl AccumulatedCounters {
    /// Creates counts for `handles`, none counted yet.
    pub(crate) fn new(handles: Vec<Counter>) -> Self {
        let counts = handles.iter().map(|_| Arc::new(AtomicU64::new(0))).collect();
        Self { counts, handles }
    }

    /// Returns a counter that counts locally for the handle at `index`.
    ///
    /// # Panics
    ///
    /// Panics if there is no handle at `index`.
    pub(crate) fn local(&self, index: usize) -> Counter {
        Counter::from_arc(Arc::clone(&self.counts[index]))
    }

    /// Adds what was counted since the last publication to the handles, one write per handle whose count changed.
    pub(crate) fn publish(&self) {
        for (count, handle) in self.counts.iter().zip(&self.handles) {
            let delta = count.swap(0, Ordering::Relaxed);
            if delta > 0 {
                handle.increment(delta);
            }
        }
    }
}

impl Drop for AccumulatedCounters {
    fn drop(&mut self) {
        // Nothing counted is lost: what was not published yet reaches the handles now.
        self.publish();
    }
}
