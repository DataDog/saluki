use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Statistics for an resource group.
pub struct ResourceStats {
    allocated_bytes: AtomicUsize,
    allocated_objects: AtomicUsize,
    deallocated_bytes: AtomicUsize,
    deallocated_objects: AtomicUsize,
    cpu_time_nanos: AtomicU64,
}

impl ResourceStats {
    pub(super) const fn new() -> Self {
        Self {
            allocated_bytes: AtomicUsize::new(0),
            allocated_objects: AtomicUsize::new(0),
            deallocated_bytes: AtomicUsize::new(0),
            deallocated_objects: AtomicUsize::new(0),
            cpu_time_nanos: AtomicU64::new(0),
        }
    }

    /// Returns `true` if the given group has allocated any memory at all.
    pub fn has_allocated(&self) -> bool {
        self.allocated_bytes.load(Relaxed) > 0
    }

    #[inline]
    pub(super) fn track_allocation(&self, size: usize) {
        self.allocated_bytes.fetch_add(size, Relaxed);
        self.allocated_objects.fetch_add(1, Relaxed);
    }

    #[inline]
    pub(super) fn track_deallocation(&self, size: usize) {
        self.deallocated_bytes.fetch_add(size, Relaxed);
        self.deallocated_objects.fetch_add(1, Relaxed);
    }

    #[inline]
    pub(super) fn track_cpu_time(&self, nanos: u64) {
        self.cpu_time_nanos.fetch_add(nanos, Relaxed);
    }

    /// Captures a snapshot of the current statistics based on the delta from a previous snapshot.
    ///
    /// This can be used to keep a single local snapshot of the last delta, and then both track the delta since that
    /// snapshot, as well as update the snapshot to the current statistics.
    ///
    /// Callers should generally create their snapshot via [`ResourceStatsSnapshot::empty`] and then use this method
    /// to get their snapshot delta, utilize those delta values in whatever way is necessary, and then merge the
    /// snapshot delta into the primary snapshot via [`ResourceStatsSnapshot::merge`] to make it current.
    pub fn snapshot_delta(&self, previous: &ResourceStatsSnapshot) -> ResourceStatsSnapshot {
        ResourceStatsSnapshot {
            allocated_bytes: self.allocated_bytes.load(Relaxed) - previous.allocated_bytes,
            allocated_objects: self.allocated_objects.load(Relaxed) - previous.allocated_objects,
            deallocated_bytes: self.deallocated_bytes.load(Relaxed) - previous.deallocated_bytes,
            deallocated_objects: self.deallocated_objects.load(Relaxed) - previous.deallocated_objects,
            cpu_time_nanos: self.cpu_time_nanos.load(Relaxed) - previous.cpu_time_nanos,
        }
    }
}

/// Snapshot of allocation statistics for a group.
pub struct ResourceStatsSnapshot {
    /// Number of allocated bytes since the last snapshot.
    pub allocated_bytes: usize,

    /// Number of allocated objects since the last snapshot.
    pub allocated_objects: usize,

    /// Number of deallocated bytes since the last snapshot.
    pub deallocated_bytes: usize,

    /// Number of deallocated objects since the last snapshot.
    pub deallocated_objects: usize,

    /// Cumulative CPU time in nanoseconds consumed by this group since the last snapshot.
    pub cpu_time_nanos: u64,
}

impl ResourceStatsSnapshot {
    /// Creates an empty `ResourceStatsSnapshot`.
    pub const fn empty() -> Self {
        Self {
            allocated_bytes: 0,
            allocated_objects: 0,
            deallocated_bytes: 0,
            deallocated_objects: 0,
            cpu_time_nanos: 0,
        }
    }

    /// Returns the number of live allocated bytes.
    pub fn live_bytes(&self) -> usize {
        self.allocated_bytes - self.deallocated_bytes
    }

    /// Returns the number of live allocated objects.
    pub fn live_objects(&self) -> usize {
        self.allocated_objects - self.deallocated_objects
    }

    /// Merges `other` into `self`.
    ///
    /// This can be used to accumulate the total number of (de)allocated bytes and objects when handling the deltas
    /// generated from `ResourceStats::consume`.
    pub fn merge(&mut self, other: &Self) {
        self.allocated_bytes += other.allocated_bytes;
        self.allocated_objects += other.allocated_objects;
        self.deallocated_bytes += other.deallocated_bytes;
        self.deallocated_objects += other.deallocated_objects;
        self.cpu_time_nanos += other.cpu_time_nanos;
    }
}

/// Returns the current thread's CPU time in nanoseconds, or `None` if unavailable.
#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn thread_cpu_time_nanos() -> Option<u64> {
    // NOTE: `CLOCK_THREAD_CPUTIME_ID` is not vDSO accelerated and degrades to a full syscall.
    //
    // Practically speaking, this ends up being roughly ~40-50x slower than a vDSO call at around 850ns or so. This is
    // acceptable for our use case, because we're only tracking thread CPU time on task enter/exit, and only doing so on
    // root task futures which are running for appreciable amounts of time so the rate at which we're calling this is fairly
    // low.

    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: We pass a valid reference to the `timespec` struct, and `CLOCK_THREAD_CPUTIME_ID` has been available
    // since Linux 2.6.12.
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if ret == 0 {
        Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
    } else {
        None
    }
}

/// Returns the current thread's CPU time in nanoseconds, or `None` if unavailable.
#[cfg(not(target_os = "linux"))]
#[inline]
pub(crate) fn thread_cpu_time_nanos() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn thread_cpu_time_returns_some() {
        let t = thread_cpu_time_nanos();
        assert!(t.is_some());
        assert!(t.unwrap() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn thread_cpu_time_is_monotonic() {
        let t1 = thread_cpu_time_nanos().unwrap();
        // Do some work to consume CPU.
        let mut sum = 0u64;
        for i in 0..100_000 {
            sum = sum.wrapping_add(i);
        }
        std::hint::black_box(sum);
        let t2 = thread_cpu_time_nanos().unwrap();
        assert!(t2 > t1);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn thread_cpu_time_returns_none_on_non_linux() {
        assert!(thread_cpu_time_nanos().is_none());
    }
}
