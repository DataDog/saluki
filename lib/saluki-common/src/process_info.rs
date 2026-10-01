//! Cached readings of the process's own resource usage.

use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use process_memory::Querier;

/// How long a reading serves before the next refresh.
///
/// Reading the process's memory and CPU time costs a system call, and every consumer wants the same
/// numbers, so one cached reading serves them all. The window is deliberately not a tuning knob:
/// it trades reading freshness for sharing, and nothing consuming these values benefits from a
/// tighter one.
const CACHE_WINDOW: Duration = Duration::from_secs(20);

struct ProcessInfoInner {
    querier: Querier,
    last_refresh: Option<Instant>,
    rss_bytes: u64,
    cpu_time: Option<Duration>,
    cpu_percent: f64,
}

impl Default for ProcessInfoInner {
    fn default() -> Self {
        Self {
            querier: Querier::default(),
            last_refresh: None,
            rss_bytes: 0,
            cpu_time: None,
            cpu_percent: 0.0,
        }
    }
}

impl ProcessInfoInner {
    /// Refreshes the cached readings when the window has elapsed.
    ///
    /// Failed reads keep the last good reading rather than clearing it: a transient read failure
    /// must not read as the process having no memory. The CPU percentage is derived from
    /// consecutive samples, so it stays zero until the second window has a pair to differ.
    fn refresh_at(&mut self, now: Instant, rss: Option<u64>, cpu: Option<Duration>) {
        if self.last_refresh.is_some_and(|last| now - last < CACHE_WINDOW) {
            return;
        }

        if let (Some(previous_cpu), Some(last_refresh), Some(cpu)) = (self.cpu_time, self.last_refresh, cpu) {
            let cpu_delta = cpu.saturating_sub(previous_cpu).as_secs_f64();
            let wall_delta = (now - last_refresh).as_secs_f64();
            if wall_delta > 0.0 {
                self.cpu_percent = cpu_delta / wall_delta * 100.0;
            }
            self.cpu_time = Some(cpu);
        } else if cpu.is_some() {
            self.cpu_time = cpu;
        }

        if let Some(rss) = rss {
            self.rss_bytes = rss;
        }

        self.last_refresh = Some(now);
    }

    fn refresh(&mut self) {
        let now = Instant::now();
        let rss = self.querier.resident_set_size().map(|bytes| bytes as u64);
        let cpu = read_process_cpu_time();
        self.refresh_at(now, rss, cpu);
    }
}

static PROCESS_INFO: LazyLock<Mutex<ProcessInfoInner>> = LazyLock::new(|| Mutex::new(ProcessInfoInner::default()));

fn lock() -> MutexGuard<'static, ProcessInfoInner> {
    // A poisoned lock means a caller panicked mid-refresh; recover the state rather than going
    // permanently silent over it.
    PROCESS_INFO.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Returns the process's resident set size, in bytes.
///
/// The reading is cached for [`CACHE_WINDOW`]; until the first successful read it is zero.
pub fn resident_set_size() -> u64 {
    let mut info = lock();
    info.refresh();
    info.rss_bytes
}

/// Returns the process's CPU utilization as a percentage of one core.
///
/// The value is derived from the CPU-time delta between consecutive cache windows, so it can
/// exceed 100 on multi-core. It reads as zero until the second window provides a pair of samples
/// to difference, and on platforms without a process CPU clock it stays zero.
pub fn cpu_percent() -> f64 {
    let mut info = lock();
    info.refresh();
    info.cpu_percent
}

/// Returns the process's cumulative CPU time, or `None` where no process CPU clock exists.
#[cfg(target_os = "linux")]
fn read_process_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: We pass a valid reference to the `timespec` struct, and `CLOCK_PROCESS_CPUTIME_ID`
    // has been available since Linux 2.6.12.
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    if ret == 0 {
        Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    } else {
        None
    }
}

/// Returns the process's cumulative CPU time, or `None` where no process CPU clock exists.
#[cfg(target_os = "macos")]
fn read_process_cpu_time() -> Option<Duration> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: We pass a valid reference to the `rusage` struct, and `RUSAGE_SELF` targets the
    // calling process.
    let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if ret == 0 {
        let user = Duration::new(usage.ru_utime.tv_sec as u64, (usage.ru_utime.tv_usec as u32) * 1000);
        let system = Duration::new(usage.ru_stime.tv_sec as u64, (usage.ru_stime.tv_usec as u32) * 1000);
        Some(user + system)
    } else {
        None
    }
}

/// Returns the process's cumulative CPU time, or `None` where no process CPU clock exists.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_process_cpu_time() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn cpu_percent_is_zero_until_a_second_window_pairs_the_samples() {
        let mut info = ProcessInfoInner::default();
        let start = Instant::now();

        info.refresh_at(step(start, 0), Some(100), Some(Duration::from_secs(1)));
        assert_eq!(info.cpu_percent, 0.0);

        info.refresh_at(step(start, 20), Some(200), Some(Duration::from_secs(2)));
        // One second of CPU over twenty seconds of wall time: 5%.
        assert!((info.cpu_percent - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn refreshes_within_the_window_are_no_ops() {
        let mut info = ProcessInfoInner::default();
        let start = Instant::now();

        info.refresh_at(step(start, 0), Some(100), Some(Duration::from_secs(1)));
        info.refresh_at(step(start, 10), Some(999), Some(Duration::from_secs(99)));
        assert_eq!(info.rss_bytes, 100);
        assert_eq!(info.cpu_time, Some(Duration::from_secs(1)));
        assert_eq!(info.cpu_percent, 0.0);

        // Past the window, the new readings take hold and the CPU delta derives.
        info.refresh_at(step(start, 25), Some(300), Some(Duration::from_secs(3)));
        assert_eq!(info.rss_bytes, 300);
        // Two seconds of CPU over twenty-five seconds of wall time: 8%.
        assert!((info.cpu_percent - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn multi_core_utilization_exceeds_one_hundred_percent() {
        let mut info = ProcessInfoInner::default();
        let start = Instant::now();

        info.refresh_at(step(start, 0), Some(100), Some(Duration::from_secs(1)));
        // Thirty seconds of CPU time over a twenty second window: two cores fully used.
        info.refresh_at(step(start, 20), Some(200), Some(Duration::from_secs(31)));
        assert!((info.cpu_percent - 150.0).abs() < f64::EPSILON);
    }

    #[test]
    fn failed_reads_keep_the_last_good_reading() {
        let mut info = ProcessInfoInner::default();
        let start = Instant::now();

        info.refresh_at(step(start, 0), Some(100), Some(Duration::from_secs(1)));
        info.refresh_at(step(start, 20), None, None);
        assert_eq!(info.rss_bytes, 100);
        assert_eq!(info.cpu_time, Some(Duration::from_secs(1)));
        // The CPU sample never advanced, so the percentage keeps its previous derivation.
        assert!((info.cpu_percent - 0.0).abs() < f64::EPSILON);
    }
}
