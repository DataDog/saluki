//! CPU time measurement for per-group tracking.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Statistics for CPU time consumed by a resource group.
pub struct CpuStats {
    cpu_time_nanos: AtomicU64,
}

impl CpuStats {
    pub(crate) const fn new() -> Self {
        Self {
            cpu_time_nanos: AtomicU64::new(0),
        }
    }

    #[inline]
    pub(crate) fn track_cpu_time(&self, nanos: u64) {
        self.cpu_time_nanos.fetch_add(nanos, Relaxed);
    }

    pub(crate) fn cpu_time_nanos(&self) -> u64 {
        self.cpu_time_nanos.load(Relaxed)
    }
}

/// Returns the current thread's CPU time in nanoseconds, or `None` if unavailable.
#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn thread_cpu_time_nanos() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: We pass a valid pointer to a timespec struct. CLOCK_THREAD_CPUTIME_ID is available
    // on Linux 2.6.12+ and is implemented as a vDSO call (~20ns overhead).
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if ret == 0 {
        Some(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
    } else {
        None
    }
}

/// Returns `None` on non-Linux platforms where `CLOCK_THREAD_CPUTIME_ID` is not available.
#[cfg(not(target_os = "linux"))]
#[inline]
pub(crate) fn thread_cpu_time_nanos() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_stats_accumulate() {
        let stats = CpuStats::new();
        assert_eq!(stats.cpu_time_nanos(), 0);
        stats.track_cpu_time(100);
        stats.track_cpu_time(200);
        assert_eq!(stats.cpu_time_nanos(), 300);
    }

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
