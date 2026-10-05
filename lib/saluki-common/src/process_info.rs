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
    cpu_sampled_at: Option<Instant>,
    cpu_percent: f64,
}

impl Default for ProcessInfoInner {
    fn default() -> Self {
        Self {
            querier: Querier::default(),
            last_refresh: None,
            rss_bytes: 0,
            cpu_time: None,
            cpu_sampled_at: None,
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

        // The percentage derives from consecutive samples, so the wall-time baseline is the
        // timestamp of the last valid CPU sample: a failed read keeps the older pair intact,
        // rather than letting the skipped window inflate the next derivation.
        if let (Some(previous_cpu), Some(previous_at), Some(cpu)) = (self.cpu_time, self.cpu_sampled_at, cpu) {
            let cpu_delta = cpu.saturating_sub(previous_cpu).as_secs_f64();
            let wall_delta = (now - previous_at).as_secs_f64();
            if wall_delta > 0.0 {
                self.cpu_percent = cpu_delta / wall_delta * 100.0;
            }
        }
        if cpu.is_some() {
            self.cpu_time = cpu;
            self.cpu_sampled_at = Some(now);
        }

        if let Some(rss) = rss {
            self.rss_bytes = rss;
        }

        self.last_refresh = Some(now);
    }

    fn refresh(&mut self) {
        let now = Instant::now();
        // The window check comes before the reads so a caller within the cache window pays no
        // syscalls: both getters consult the cache on every call, and only an expired window
        // reaches the operating system.
        if self.last_refresh.is_some_and(|last| now - last < CACHE_WINDOW) {
            return;
        }
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
/// to difference.
pub fn cpu_percent() -> f64 {
    let mut info = lock();
    info.refresh();
    info.cpu_percent
}

/// Returns the process's cumulative CPU time, or `None` where the platform provides no
/// per-process accounting.
#[cfg(unix)]
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

/// Returns the process's cumulative CPU time, or `None` where the platform provides no
/// per-process accounting.
#[cfg(windows)]
fn read_process_cpu_time() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let mut user_time: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel_time: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: Both parameters are valid references to zeroed `FILETIME` structs, and
    // `GetCurrentProcess` returns a pseudo-handle that is valid for the calling process. The
    // creation and exit times are required parameters but not needed here; passing valid
    // references satisfies the contract without the results being read.
    let ret = unsafe {
        let mut creation_time: FILETIME = std::mem::zeroed();
        let mut exit_time: FILETIME = std::mem::zeroed();
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    };
    if ret == 0 {
        return None;
    }

    Some(filetime_as_duration(user_time) + filetime_as_duration(kernel_time))
}

/// Converts a `FILETIME` count of 100-nanosecond intervals into a duration.
#[cfg(windows)]
fn filetime_as_duration(time: FILETIME) -> Duration {
    let intervals = (time.dwHighDateTime as u64) << 32 | time.dwLowDateTime as u64;
    Duration::from_nanos(intervals * 100)
}

/// Returns the process's cumulative CPU time, or `None` where the platform provides no
/// per-process accounting.
#[cfg(not(any(unix, windows)))]
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
    fn a_failed_read_does_not_shorten_the_next_derivations_wall_window() {
        let mut info = ProcessInfoInner::default();
        let start = Instant::now();

        info.refresh_at(step(start, 0), Some(100), Some(Duration::from_secs(1)));
        // A transient failure at the 20-second mark advances the cache window but not the sample
        // pair.
        info.refresh_at(step(start, 20), None, None);

        // The next valid sample derives over the full forty seconds since the last valid sample,
        // not the twenty since the failed one: two CPU-seconds over forty wall-seconds is 5%.
        info.refresh_at(step(start, 40), Some(300), Some(Duration::from_secs(3)));
        assert!((info.cpu_percent - 5.0).abs() < f64::EPSILON);
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
