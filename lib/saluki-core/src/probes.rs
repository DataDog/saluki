//! Measurement probes at the serial points of a topology.
//!
//! This module exists only with the `measurement` feature, which no production build enables. It holds named
//! [`Histogram`]s in a process-wide registry: the dispatcher records into them how long a sender waits for an output's
//! admission point lock and how long it holds it, how long it waits on the output's channels, how long it spends
//! cloning a buffer for every sender but the last, and how many items each buffer it sends holds; sources and other
//! components record waits of their own. A measurement reads the histograms with [`snapshot`] and clears them with
//! [`reset`]. (`Consumer::observe`, a `test-util` helper, lets a measurement see what a component receives.)
//!
//! Item counts are always recorded, at a few relaxed atomic additions per buffer. Durations need two clock reads per
//! event and are recorded only while [`set_timing`] has turned timing on, so that a measurement of throughput is not
//! perturbed by the probes of another.
//!
//! Probe names have the form `<probe>/<subject>`, where the subject is `<component>.<output>` for dispatcher probes
//! (`_default` names the default output). The dispatcher's probes are:
//!
//! | Probe | Records |
//! |---|---|
//! | `admission_lock_wait` | nanoseconds from asking for an admission point's lock to holding it |
//! | `admission_lock_hold` | nanoseconds the lock is held: the stamp and the whole fan-out, waits for room included |
//! | `dispatch_send_wait` | nanoseconds one send awaits room in one of the output's channels: one record per sender of each buffer |
//! | `dispatch_clone` | nanoseconds cloning one buffer: one record per sender of each buffer but the last |
//! | `dispatch_items` | items per buffer sent |

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

/// The number of exact buckets, for values below it, and of sub-buckets per power of two above it.
const SUB_BUCKETS: usize = 8;

/// The base-two logarithm of [`SUB_BUCKETS`].
const SUB_BUCKET_BITS: u32 = 3;

/// The number of buckets: the exact ones, then [`SUB_BUCKETS`] for each power of two from `2^3` to `2^63`.
const BUCKETS: usize = SUB_BUCKETS + (64 - SUB_BUCKET_BITS as usize) * SUB_BUCKETS;

static TIMING: AtomicBool = AtomicBool::new(false);

/// Turns the recording of durations on or off, for every probe in the process.
///
/// Timing is off until turned on. Item counts are recorded either way.
pub fn set_timing(enabled: bool) {
    TIMING.store(enabled, Relaxed);
}

/// Returns `true` if probes record durations.
#[inline]
pub fn timing() -> bool {
    TIMING.load(Relaxed)
}

/// A histogram of `u64` values, such as nanoseconds or item counts, that any number of threads record into.
///
/// Values below 8 have exact buckets; larger values fall into one of eight buckets per power of two, so a quantile is
/// reported within 12.5% of the value it estimates. The count, the sum, and the largest value are exact.
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}

impl Histogram {
    /// Creates an empty histogram.
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }

    /// Records `value`.
    #[inline]
    pub fn record(&self, value: u64) {
        self.buckets[bucket_of(value)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.sum.fetch_add(value, Relaxed);
        self.max.fetch_max(value, Relaxed);
    }

    /// Records `duration` in nanoseconds, saturating at `u64::MAX`.
    #[inline]
    pub fn record_duration(&self, duration: Duration) {
        self.record(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Returns what the histogram holds now.
    ///
    /// Recording continues meanwhile, so the snapshot of a histogram that is being recorded into is not one instant:
    /// its count can differ from the sum of its buckets by the records in flight.
    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: self.buckets.iter().map(|bucket| bucket.load(Relaxed)).collect(),
            count: self.count.load(Relaxed),
            sum: self.sum.load(Relaxed),
            max: self.max.load(Relaxed),
        }
    }

    /// Clears the histogram.
    pub fn reset(&self) {
        for bucket in &self.buckets {
            bucket.store(0, Relaxed);
        }
        self.count.store(0, Relaxed);
        self.sum.store(0, Relaxed);
        self.max.store(0, Relaxed);
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Histogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.snapshot().fmt(f)
    }
}

/// Returns the bucket `value` falls into.
#[inline]
fn bucket_of(value: u64) -> usize {
    if value < SUB_BUCKETS as u64 {
        return value as usize;
    }
    let exponent = 63 - value.leading_zeros();
    let mantissa = ((value >> (exponent - SUB_BUCKET_BITS)) as usize) & (SUB_BUCKETS - 1);
    SUB_BUCKETS + (exponent - SUB_BUCKET_BITS) as usize * SUB_BUCKETS + mantissa
}

/// Returns the largest value that falls into `bucket`.
fn bucket_upper_bound(bucket: usize) -> u64 {
    if bucket < SUB_BUCKETS {
        return bucket as u64;
    }
    let exponent = (bucket - SUB_BUCKETS) / SUB_BUCKETS + SUB_BUCKET_BITS as usize;
    let mantissa = ((bucket - SUB_BUCKETS) % SUB_BUCKETS) as u64;
    let lower = (1u64 << exponent) | (mantissa << (exponent - SUB_BUCKET_BITS as usize));
    lower.saturating_add((1u64 << (exponent - SUB_BUCKET_BITS as usize)) - 1)
}

/// What a [`Histogram`] held when it was read.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct HistogramSnapshot {
    buckets: Vec<u64>,

    /// The number of values recorded.
    pub count: u64,

    /// The sum of the values recorded, wrapping on overflow.
    pub sum: u64,

    /// The largest value recorded, or `0` if none was.
    pub max: u64,
}

impl HistogramSnapshot {
    /// Returns the mean of the values recorded, or `0` if none was.
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum as f64 / self.count as f64
        }
    }

    /// Returns an estimate of the `quantile` (from 0 to 1) of the values recorded: the largest value of the bucket the
    /// quantile falls into, capped at the largest value recorded. Returns `0` if nothing was recorded.
    pub fn quantile(&self, quantile: f64) -> u64 {
        let total = self.buckets.iter().sum::<u64>();
        if total == 0 {
            return 0;
        }
        let rank = ((total as f64 * quantile.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut seen = 0;
        for (bucket, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return bucket_upper_bound(bucket).min(self.max);
            }
        }
        self.max
    }

    /// Returns what was recorded after `earlier`, a snapshot of the same histogram taken before this one and after its
    /// last reset. The largest value is this snapshot's, which may predate `earlier`.
    pub fn since(&self, earlier: &Self) -> Self {
        let buckets = self
            .buckets
            .iter()
            .enumerate()
            .map(|(index, count)| count.saturating_sub(earlier.buckets.get(index).copied().unwrap_or(0)))
            .collect();
        Self {
            buckets,
            count: self.count.saturating_sub(earlier.count),
            sum: self.sum.wrapping_sub(earlier.sum),
            max: self.max,
        }
    }
}

impl fmt::Debug for HistogramSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistogramSnapshot")
            .field("count", &self.count)
            .field("sum", &self.sum)
            .field("mean", &self.mean())
            .field("p50", &self.quantile(0.5))
            .field("p99", &self.quantile(0.99))
            .field("max", &self.max)
            .finish()
    }
}

/// A span of time recorded into a histogram in nanoseconds when it ends: when it is dropped, or when
/// [`end`][Self::end] is called.
///
/// A span started while timing is off records nothing and reads no clock. Dropping a span records it, so the wait of a
/// future that is cancelled while waiting is recorded too.
#[must_use = "a span records when it is dropped"]
pub struct Span<'a> {
    histogram: &'a Histogram,
    started: Option<Instant>,
}

impl<'a> Span<'a> {
    /// Starts a span that records into `histogram`, if timing is on.
    #[inline]
    pub fn start(histogram: &'a Histogram) -> Self {
        Self {
            histogram,
            started: timing().then(Instant::now),
        }
    }

    /// Ends the span, recording it, and returns when it ended, if it was timed.
    #[inline]
    pub fn end(mut self) -> Option<Instant> {
        self.finish()
    }

    #[inline]
    fn finish(&mut self) -> Option<Instant> {
        let started = self.started.take()?;
        let now = Instant::now();
        self.histogram.record_duration(now.saturating_duration_since(started));
        Some(now)
    }
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

fn registry() -> &'static Mutex<BTreeMap<String, Arc<Histogram>>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, Arc<Histogram>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Returns the histogram registered as `name`, registering an empty one first if there is none.
///
/// Call this when a probe site is created, not on every record: it takes the registry's lock.
pub fn histogram(name: &str) -> Arc<Histogram> {
    let mut registry = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        registry
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Histogram::new())),
    )
}

/// Returns a snapshot of every registered histogram, by name.
pub fn snapshot() -> BTreeMap<String, HistogramSnapshot> {
    let registry = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .iter()
        .map(|(name, histogram)| (name.clone(), histogram.snapshot()))
        .collect()
}

/// Clears every registered histogram. Histograms stay registered, so probe sites keep recording into them.
pub fn reset() {
    let registry = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for histogram in registry.values() {
        histogram.reset();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Held by every test that changes process-wide probe state, turning timing on or off or resetting the registry,
    /// so that tests running at once in one process do not change it under each other.
    pub(crate) static PROBES_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn small_values_have_exact_buckets_and_larger_ones_eight_per_power_of_two() {
        for value in 0..8 {
            assert_eq!(bucket_of(value), value as usize);
            assert_eq!(bucket_upper_bound(value as usize), value);
        }
        // 8 to 15 are still exact: eight buckets for the power of two 2^3.
        assert_eq!(bucket_of(8), 8);
        assert_eq!(bucket_of(15), 15);
        // 16 to 31 take two values per bucket.
        assert_eq!(bucket_of(16), 16);
        assert_eq!(bucket_of(17), 16);
        assert_eq!(bucket_upper_bound(16), 17);
        assert_eq!(bucket_of(31), 23);
        assert_eq!(bucket_upper_bound(23), 31);
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_upper_bound(BUCKETS - 1), u64::MAX);
    }

    #[test]
    fn a_snapshot_reports_the_count_sum_mean_quantiles_and_maximum() {
        let histogram = Histogram::new();
        for value in 1..=100 {
            histogram.record(value);
        }
        let snapshot = histogram.snapshot();
        assert_eq!((snapshot.count, snapshot.sum, snapshot.max), (100, 5_050, 100));
        assert_eq!(snapshot.mean(), 50.5);
        // The median, 50, falls into the bucket of 48 to 51; the 99th percentile, 99, into 96 to 103, capped at the
        // largest value recorded.
        assert_eq!(snapshot.quantile(0.5), 51);
        assert_eq!(snapshot.quantile(0.99), 100);
        assert_eq!(snapshot.quantile(0.0), 1);

        histogram.reset();
        assert_eq!(histogram.snapshot(), HistogramSnapshot::default().with_buckets());
        assert_eq!(histogram.snapshot().quantile(0.5), 0);
    }

    #[test]
    fn since_subtracts_an_earlier_snapshot() {
        let histogram = Histogram::new();
        histogram.record(10);
        let earlier = histogram.snapshot();
        histogram.record(1_000);
        histogram.record(1_000);
        let since = histogram.snapshot().since(&earlier);
        assert_eq!((since.count, since.sum), (2, 2_000));
        assert_eq!(since.quantile(0.0), since.quantile(1.0));
        assert!((896..=1_000).contains(&since.quantile(0.5)));
    }

    #[test]
    fn a_span_records_only_while_timing_is_on_and_when_dropped() {
        let _probes = PROBES_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let histogram = Histogram::new();
        set_timing(false);
        assert!(Span::start(&histogram).end().is_none());
        drop(Span::start(&histogram));
        assert_eq!(histogram.snapshot().count, 0);

        set_timing(true);
        assert!(Span::start(&histogram).end().is_some());
        drop(Span::start(&histogram));
        set_timing(false);
        assert_eq!(histogram.snapshot().count, 2);
    }

    #[test]
    fn the_registry_returns_one_histogram_per_name() {
        let _probes = PROBES_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let first = histogram("registry_test/one");
        let again = histogram("registry_test/one");
        assert!(Arc::ptr_eq(&first, &again));
        first.record(3);
        assert_eq!(snapshot()["registry_test/one"].sum, 3);
        reset();
        assert_eq!(snapshot()["registry_test/one"].count, 0);
    }

    impl HistogramSnapshot {
        fn with_buckets(mut self) -> Self {
            self.buckets = vec![0; BUCKETS];
            self
        }
    }

    proptest! {
        #[test]
        fn property_test_every_value_lies_within_its_bucket_and_quantiles_within_an_eighth(
            values in proptest::collection::vec(any::<u64>(), 1..64),
            quantile in 0.0f64..=1.0,
        ) {
            for value in &values {
                let bucket = bucket_of(*value);
                prop_assert!(*value <= bucket_upper_bound(bucket));
                if bucket > 0 {
                    prop_assert!(*value > bucket_upper_bound(bucket - 1));
                }
            }

            let histogram = Histogram::new();
            for value in &values {
                histogram.record(*value);
            }
            let mut sorted = values.clone();
            sorted.sort_unstable();
            let rank = ((sorted.len() as f64 * quantile).ceil() as usize).max(1);
            let exact = sorted[rank - 1];
            let estimate = histogram.snapshot().quantile(quantile);
            prop_assert!(estimate >= exact);
            prop_assert!(estimate as f64 <= exact as f64 * 1.125 + 1.0);
        }
    }
}
