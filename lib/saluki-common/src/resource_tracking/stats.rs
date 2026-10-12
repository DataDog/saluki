use std::{
    cell::Cell,
    num::NonZeroU32,
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed},
};

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
    ///
    /// Saturates at zero rather than underflowing. The allocation and deallocation counters this is derived from are
    /// read independently, so a deallocation landing between the two reads can momentarily make the difference
    /// negative.
    pub fn live_bytes(&self) -> usize {
        self.allocated_bytes.saturating_sub(self.deallocated_bytes)
    }

    /// Returns the number of live allocated objects.
    ///
    /// Saturates at zero, for the same reason as [`live_bytes`][Self::live_bytes].
    pub fn live_objects(&self) -> usize {
        self.allocated_objects.saturating_sub(self.deallocated_objects)
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

/// The process-wide sampling rate of CPU-time reads: an entry into a resource group reads the thread's CPU time with
/// probability `1 / N`.
static CPU_TIME_SAMPLING: AtomicU32 = AtomicU32::new(1);

/// Sets how often entering a resource group reads the thread's CPU time, process-wide: one entry in `n`, chosen at
/// random, with the CPU time it measures counted `n` times.
///
/// Reading a thread's CPU time is a system call on Linux, of about 850 nanoseconds, and every entry into a resource
/// group, such as each poll of a tracked future, reads it twice. Sampling makes each group's CPU time an unbiased
/// estimate at a fraction of that cost: over many entries, its expected value is the CPU time the group used, while a
/// short interval of few entries may show none, or `n` times what it used. Entries are chosen at random rather than one
/// in `n` in turn, so that a pattern of entries, such as a task poll that nests a call in another group, cannot line
/// up with the samples. Entries made with [`enter_measured`][super::ResourceGroupToken::enter_measured] always read.
///
/// Sampling never changes which group allocations are attributed to: every entry switches the current group.
///
/// Defaults to `1`, which reads the CPU time on every entry. A new rate applies from the next entry.
pub fn set_cpu_time_sampling(n: NonZeroU32) {
    CPU_TIME_SAMPLING.store(n.get(), Relaxed);
}

/// Returns how often entering a resource group reads the thread's CPU time: one entry in the returned number. See
/// [`set_cpu_time_sampling`].
pub fn cpu_time_sampling() -> NonZeroU32 {
    NonZeroU32::new(CPU_TIME_SAMPLING.load(Relaxed)).unwrap_or(NonZeroU32::MIN)
}

/// Returns the sampling rate an entry on this thread applies: the process-wide rate, or the one a test installed with
/// a [`FakeCpuClock`][testing::FakeCpuClock].
#[inline]
fn effective_sampling() -> u32 {
    #[cfg(any(test, feature = "test-util"))]
    if let Some(n) = testing::fake_sampling() {
        return n.get();
    }
    CPU_TIME_SAMPLING.load(Relaxed)
}

/// The first CPU-time reading of an entry into a resource group, which its exit completes.
#[derive(Clone, Copy, Debug)]
pub(super) struct CpuTimeStart {
    nanos: u64,
    scale: u64,
}

impl CpuTimeStart {
    /// Returns the CPU time to attribute to an entry that started here and ends now: the thread's CPU time since, times
    /// the sampling rate the entry was drawn at.
    #[inline]
    pub(super) fn attributable_since(self) -> u64 {
        let end = thread_cpu_time_nanos().unwrap_or(0);
        end.saturating_sub(self.nanos).saturating_mul(self.scale)
    }
}

/// Reads the thread's CPU time at the start of an entry into a resource group, if the entry is sampled: always at a
/// sampling rate of `1`, and otherwise with probability `1 / N`.
///
/// Returns `None` for an entry that is not sampled, and when the platform has no thread CPU time.
#[inline]
pub(super) fn sampled_cpu_time_start() -> Option<CpuTimeStart> {
    let n = effective_sampling().max(1);
    if n > 1 && !sampled(n) {
        return None;
    }
    measured_cpu_time_start(u64::from(n))
}

/// Reads the thread's CPU time at the start of an entry into a resource group whose CPU time counts `scale` times, or
/// returns `None` when the platform has no thread CPU time.
#[inline]
pub(super) fn measured_cpu_time_start(scale: u64) -> Option<CpuTimeStart> {
    thread_cpu_time_nanos().map(|nanos| CpuTimeStart { nanos, scale })
}

thread_local! {
    /// The state of this thread's `xorshift64*` sampler, never zero.
    static SAMPLER: Cell<u64> = Cell::new(sampler_seed());
}

/// Returns a seed for a new thread's sampler: distinct per thread, and never zero.
fn sampler_seed() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    splitmix64(NEXT.fetch_add(1, Relaxed)).max(1)
}

/// Returns `splitmix64`'s output for `x`, which spreads consecutive inputs over the whole range.
fn splitmix64(x: u64) -> u64 {
    let mut x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Draws from this thread's sampler, returning `true` with probability `1 / n`.
#[inline]
fn sampled(n: u32) -> bool {
    SAMPLER.with(|state| {
        // xorshift64*: a full-period generator over the nonzero 64-bit states, whose high bits are well mixed.
        let mut x = state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        let draw = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32;
        // Lemire's multiply-shift maps the 32-bit draw onto `0..n`; `0` samples. The probability is `1 / n` exactly
        // when `n` is a power of two, and within `n / 2^32` of it otherwise.
        (draw * u64::from(n)) >> 32 == 0
    })
}

/// Returns the current thread's CPU time in nanoseconds, or `None` if unavailable.
///
/// A test that installed a [`FakeCpuClock`][testing::FakeCpuClock] on the thread reads that clock instead.
#[inline]
fn thread_cpu_time_nanos() -> Option<u64> {
    #[cfg(any(test, feature = "test-util"))]
    if let Some(nanos) = testing::read_fake_cpu_clock() {
        return Some(nanos);
    }
    os_thread_cpu_time_nanos()
}

/// Returns the current thread's CPU time in nanoseconds, as the operating system reports it, or `None` if unavailable.
#[cfg(target_os = "linux")]
#[inline]
fn os_thread_cpu_time_nanos() -> Option<u64> {
    // NOTE: `CLOCK_THREAD_CPUTIME_ID` is not vDSO accelerated and degrades to a full syscall.
    //
    // Practically speaking, this ends up being roughly ~40-50x slower than a vDSO call at around 850ns or so. Entries
    // into resource groups happen on every poll of a tracked future and around every kernel call, so the reads can be
    // sampled; see `set_cpu_time_sampling`.

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

/// Returns the current thread's CPU time in nanoseconds, as the operating system reports it, or `None` if unavailable.
#[cfg(not(target_os = "linux"))]
#[inline]
fn os_thread_cpu_time_nanos() -> Option<u64> {
    None
}

/// Test support for CPU-time accounting.
#[cfg(any(test, feature = "test-util"))]
pub mod testing {
    use std::{cell::Cell, marker::PhantomData, num::NonZeroU32};

    /// What a [`FakeCpuClock`] installed on a thread holds.
    #[derive(Clone, Copy)]
    struct FakeState {
        now: u64,
        reads: u64,
        sampling: NonZeroU32,
    }

    thread_local! {
        static FAKE: Cell<Option<FakeState>> = const { Cell::new(None) };
    }

    /// A thread CPU clock that a test controls, which replaces the current thread's real one while it is installed.
    ///
    /// The clock starts at zero and moves only when the test [advances][Self::advance] it, so that CPU-time accounting
    /// can be checked exactly, on every platform, including those without thread CPU time. While it is installed,
    /// entries into resource groups on the thread are sampled at the rate it was installed with, instead of the
    /// process-wide rate, by a sampler seeded for the test: tests in parallel threads do not affect one another, and
    /// every run of a test samples the same entries.
    ///
    /// Dropping the clock restores the thread's real clock and the process-wide rate. It cannot leave its thread.
    pub struct FakeCpuClock {
        _not_send: PhantomData<*const ()>,
    }

    impl FakeCpuClock {
        /// Installs a fake clock on the current thread, which samples entries with probability `1 / sampling`, drawn
        /// by a sampler seeded with `seed`.
        ///
        /// # Panics
        ///
        /// Panics if a fake clock is already installed on the thread.
        pub fn install(sampling: NonZeroU32, seed: u64) -> Self {
            FAKE.with(|fake| {
                assert!(
                    fake.get().is_none(),
                    "a fake CPU clock is already installed on this thread"
                );
                fake.set(Some(FakeState {
                    now: 0,
                    reads: 0,
                    sampling,
                }));
            });
            super::SAMPLER.with(|state| state.set(super::splitmix64(seed).max(1)));
            Self { _not_send: PhantomData }
        }

        /// Moves the clock forward by `nanos`, as if the thread had used that much CPU time.
        pub fn advance(&self, nanos: u64) {
            FAKE.with(|fake| {
                if let Some(mut state) = fake.get() {
                    state.now = state.now.saturating_add(nanos);
                    fake.set(Some(state));
                }
            });
        }

        /// Returns how many times the clock was read.
        pub fn reads(&self) -> u64 {
            FAKE.with(|fake| fake.get().map_or(0, |state| state.reads))
        }
    }

    impl Drop for FakeCpuClock {
        fn drop(&mut self) {
            FAKE.with(|fake| fake.set(None));
        }
    }

    /// Reads the fake clock installed on the current thread, if there is one.
    pub(super) fn read_fake_cpu_clock() -> Option<u64> {
        FAKE.with(|fake| {
            let mut state = fake.get()?;
            state.reads += 1;
            fake.set(Some(state));
            Some(state.now)
        })
    }

    /// Returns the sampling rate of the fake clock installed on the current thread, if there is one.
    pub(super) fn fake_sampling() -> Option<NonZeroU32> {
        FAKE.with(|fake| fake.get().map(|state| state.sampling))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn thread_cpu_time_returns_some() {
        let t = os_thread_cpu_time_nanos();
        assert!(t.is_some());
        assert!(t.unwrap() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn thread_cpu_time_is_monotonic() {
        let t1 = os_thread_cpu_time_nanos().unwrap();
        // Do some work to consume CPU.
        let mut sum = 0u64;
        for i in 0..100_000 {
            sum = sum.wrapping_add(i);
        }
        std::hint::black_box(sum);
        let t2 = os_thread_cpu_time_nanos().unwrap();
        assert!(t2 > t1);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn thread_cpu_time_returns_none_on_non_linux() {
        assert!(os_thread_cpu_time_nanos().is_none());
    }

    #[test]
    fn a_fake_clock_replaces_the_threads_clock_until_dropped() {
        let clock = testing::FakeCpuClock::install(NonZeroU32::MIN, 1);
        assert_eq!(thread_cpu_time_nanos(), Some(0));
        clock.advance(250);
        assert_eq!(thread_cpu_time_nanos(), Some(250));
        assert_eq!(clock.reads(), 2);
        drop(clock);
        assert_eq!(testing::read_fake_cpu_clock(), None);
        assert_eq!(testing::fake_sampling(), None);
    }

    #[test]
    fn the_sampler_draws_one_entry_in_n_on_average() {
        let _clock = testing::FakeCpuClock::install(NonZeroU32::new(16).unwrap(), 7);
        const DRAWS: u32 = 160_000;
        let hits = (0..DRAWS).filter(|_| sampled(16)).count() as f64;
        let expected = f64::from(DRAWS) / 16.0;
        // The count is binomial, with a standard deviation of about 97: 5% is more than 10 deviations.
        assert!(
            (hits - expected).abs() < expected * 0.05,
            "{} hits, {} expected",
            hits,
            expected
        );
    }

    #[test]
    fn the_sampling_rate_is_every_entry_by_default() {
        assert_eq!(cpu_time_sampling(), NonZeroU32::MIN);
        assert_eq!(effective_sampling(), 1);
        assert!(sampled_cpu_time_start().is_some() || os_thread_cpu_time_nanos().is_none());
    }
}
