//! Support for gauges that track the lowest or highest value recorded to a histogram.

#[cfg(not(all(test, feature = "loom")))]
use std::sync::{atomic::AtomicU64, Mutex};
use std::sync::{atomic::Ordering::Relaxed, Arc, PoisonError};

#[cfg(all(test, feature = "loom"))]
use loom::sync::{atomic::AtomicU64, Mutex};
use metrics::{Gauge, Histogram, IntoF64};

/// The bits of the starting value. `NaN` means that the tracker holds no value yet.
const UNSET: u64 = f64::NAN.to_bits();

#[derive(Clone, Copy, Debug)]
enum Kind {
    Min,
    Max,
}

impl Kind {
    /// Returns `true` if `value` is lower (`Min`) or higher (`Max`) than `current`.
    ///
    /// A `NaN` value never improves. Any other value improves on an unset (`NaN`) current value.
    #[inline]
    fn improves(self, value: f64, current: f64) -> bool {
        !value.is_nan()
            && (current.is_nan()
                || match self {
                    Kind::Min => value < current,
                    Kind::Max => value > current,
                })
    }
}

#[derive(Debug)]
struct State {
    current: AtomicU64,
    gauge: Mutex<Gauge>,
}

/// A gauge that holds the lowest or highest value observed so far.
///
/// The gauge only changes when a value is lower (for `min`) or higher (for `max`) than all values observed before it,
/// so it never moves back. The gauge ignores `NaN` values. The tracked value never resets.
///
/// Clones share the tracking state, so every clone of the containing metrics struct updates the same value. Separate
/// instances do not share state: if two instances write to the same gauge, the gauge can move back.
///
/// This type is an implementation detail of the `static_metrics` attribute macro. Don't construct or use it directly.
#[derive(Clone, Debug)]
pub struct DerivedGauge {
    kind: Kind,
    state: Arc<State>,
}

impl DerivedGauge {
    fn new(kind: Kind, gauge: Gauge) -> Self {
        Self {
            kind,
            state: Arc::new(State {
                current: AtomicU64::new(UNSET),
                gauge: Mutex::new(gauge),
            }),
        }
    }

    /// Creates a `DerivedGauge` that sets `gauge` to the lowest value observed.
    pub fn min(gauge: Gauge) -> Self {
        Self::new(Kind::Min, gauge)
    }

    /// Creates a `DerivedGauge` that sets `gauge` to the highest value observed.
    pub fn max(gauge: Gauge) -> Self {
        Self::new(Kind::Max, gauge)
    }

    /// Observes a value, and updates the gauge if the value is a new lowest or highest value.
    #[inline]
    pub fn observe(&self, value: f64) {
        // `current` only moves toward the extreme, so a stale value here can only send us to `publish` without need.
        if self
            .kind
            .improves(value, f64::from_bits(self.state.current.load(Relaxed)))
        {
            self.publish(value);
        }
    }

    #[cold]
    #[inline(never)]
    fn publish(&self, value: f64) {
        // All writes to `current` and to the gauge happen under this lock, so the gauge never moves back. We store
        // `current` before we set the gauge, so a panic in `set` can't let a later, worse value overwrite this one.
        let gauge = self.state.gauge.lock().unwrap_or_else(PoisonError::into_inner);
        if self
            .kind
            .improves(value, f64::from_bits(self.state.current.load(Relaxed)))
        {
            self.state.current.store(value.to_bits(), Relaxed);
            gauge.set(value);
        }
    }
}

/// A histogram that also updates one or more [`DerivedGauge`]s.
///
/// The accessor that `static_metrics` generates for a histogram field with `#[metric(derived(...))]` returns this type.
/// Each recorded value goes to the histogram, then to each derived gauge.
#[derive(Clone, Copy, Debug)]
#[must_use = "histograms do nothing unless you use them"]
pub struct DerivedHistogram<'a> {
    histogram: &'a Histogram,
    derived: &'a [DerivedGauge],
}

impl<'a> DerivedHistogram<'a> {
    /// Creates a `DerivedHistogram` from a histogram and its derived gauges.
    ///
    /// This is an implementation detail of the `static_metrics` attribute macro.
    pub fn new(histogram: &'a Histogram, derived: &'a [DerivedGauge]) -> Self {
        Self { histogram, derived }
    }

    /// Records a value into the histogram, and updates the derived gauges.
    pub fn record<T: IntoF64>(&self, value: T) {
        let value = value.into_f64();
        self.histogram.record(value);
        self.observe(value);
    }

    /// Records a value into the histogram multiple times, and updates the derived gauges.
    ///
    /// If `count` is zero, this method records nothing and leaves the derived gauges unchanged.
    pub fn record_many<T: IntoF64>(&self, value: T, count: usize) {
        let value = value.into_f64();
        self.histogram.record_many(value, count);
        if count > 0 {
            self.observe(value);
        }
    }

    fn observe(&self, value: f64) {
        for derived in self.derived {
            derived.observe(value);
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use std::{sync::Mutex as StdMutex, thread};

    use metrics::{GaugeFn, HistogramFn, Key, Label};
    use proptest::{collection::vec, prelude::*};

    use super::*;

    /// A gauge that records every value passed to `set`, so that tests can tell a missing write from a repeated one.
    #[derive(Default)]
    struct RecordingGauge {
        writes: StdMutex<Vec<f64>>,
    }

    impl RecordingGauge {
        fn writes(&self) -> Vec<f64> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl GaugeFn for RecordingGauge {
        fn increment(&self, _: f64) {
            unreachable!("derived gauges only call `set`");
        }

        fn decrement(&self, _: f64) {
            unreachable!("derived gauges only call `set`");
        }

        fn set(&self, value: f64) {
            self.writes.lock().unwrap().push(value);
        }
    }

    #[derive(Default)]
    struct RecordingHistogram {
        samples: StdMutex<Vec<f64>>,
    }

    impl HistogramFn for RecordingHistogram {
        fn record(&self, value: f64) {
            self.samples.lock().unwrap().push(value);
        }
    }

    fn recording_tracker(kind: Kind) -> (DerivedGauge, Arc<RecordingGauge>) {
        let recording = Arc::new(RecordingGauge::default());
        let derived = DerivedGauge::new(kind, Gauge::from_arc(Arc::clone(&recording)));
        (derived, recording)
    }

    fn observe_all(derived: &DerivedGauge, values: &[f64]) {
        for value in values {
            derived.observe(*value);
        }
    }

    #[test]
    fn max_writes_only_new_highest_values() {
        let (derived, recording) = recording_tracker(Kind::Max);
        observe_all(&derived, &[5.0, 3.0, 5.0, 7.0, 6.0]);
        assert_eq!(recording.writes(), vec![5.0, 7.0]);
    }

    #[test]
    fn min_writes_only_new_lowest_values() {
        let (derived, recording) = recording_tracker(Kind::Min);
        observe_all(&derived, &[5.0, 7.0, 5.0, 3.0, 4.0]);
        assert_eq!(recording.writes(), vec![5.0, 3.0]);
    }

    #[test]
    fn nan_never_changes_the_gauge() {
        let (derived, recording) = recording_tracker(Kind::Max);
        observe_all(&derived, &[f64::NAN, f64::NAN]);
        assert!(recording.writes().is_empty());

        observe_all(&derived, &[1.0, f64::NAN, 2.0]);
        assert_eq!(recording.writes(), vec![1.0, 2.0]);
    }

    #[test]
    fn infinities_are_published() {
        // An infinite starting value would hide these, so the tracker starts unset instead.
        let (derived, recording) = recording_tracker(Kind::Max);
        observe_all(&derived, &[f64::NEG_INFINITY, 1.0, f64::INFINITY]);
        assert_eq!(recording.writes(), vec![f64::NEG_INFINITY, 1.0, f64::INFINITY]);

        let (derived, recording) = recording_tracker(Kind::Min);
        observe_all(&derived, &[f64::INFINITY, -1.0, f64::NEG_INFINITY]);
        assert_eq!(recording.writes(), vec![f64::INFINITY, -1.0, f64::NEG_INFINITY]);
    }

    #[test]
    fn clones_share_tracked_value() {
        let (derived, recording) = recording_tracker(Kind::Max);
        let clone = derived.clone();
        derived.observe(10.0);
        clone.observe(5.0);
        assert_eq!(recording.writes(), vec![10.0]);

        let threads: Vec<_> = (0..4)
            .map(|offset| {
                let derived = derived.clone();
                thread::spawn(move || {
                    for i in 0..1_000 {
                        derived.observe(f64::from(i * 4 + offset));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let writes = recording.writes();
        assert!(
            writes.windows(2).all(|pair| pair[1] > pair[0]),
            "gauge moved backwards: {writes:?}"
        );
        assert_eq!(writes.last(), Some(&3_999.0));
    }

    #[test]
    fn derived_histogram_records_then_updates_every_gauge() {
        let samples = Arc::new(RecordingHistogram::default());
        let histogram = Histogram::from_arc(Arc::clone(&samples));
        let (min, min_recording) = recording_tracker(Kind::Min);
        let (max, max_recording) = recording_tracker(Kind::Max);
        let derived = [min, max];
        let wrapped = DerivedHistogram::new(&histogram, &derived);

        wrapped.record(4u32);
        wrapped.record(2.0);
        wrapped.record(8.0);

        assert_eq!(*samples.samples.lock().unwrap(), vec![4.0, 2.0, 8.0]);
        assert_eq!(min_recording.writes(), vec![4.0, 2.0]);
        assert_eq!(max_recording.writes(), vec![4.0, 8.0]);
    }

    #[test]
    fn record_many_observes_once_and_not_at_all_for_zero() {
        let samples = Arc::new(RecordingHistogram::default());
        let histogram = Histogram::from_arc(Arc::clone(&samples));
        let (max, recording) = recording_tracker(Kind::Max);
        let derived = [max];
        let wrapped = DerivedHistogram::new(&histogram, &derived);

        wrapped.record_many(9.0, 0);
        assert!(samples.samples.lock().unwrap().is_empty());
        assert!(recording.writes().is_empty());

        wrapped.record_many(3.0, 2);
        assert_eq!(*samples.samples.lock().unwrap(), vec![3.0, 3.0]);
        assert_eq!(recording.writes(), vec![3.0]);
    }

    #[crate::static_metrics(prefix = derived_test, labels(id))]
    #[derive(Clone)]
    struct Telemetry {
        #[metric(mapped(reason))]
        dropped_total: crate::Counter,
        #[metric(derived(min, max))]
        latency_seconds: crate::Histogram,
    }

    #[test]
    fn static_metrics_registers_and_updates_derived_gauges() {
        let recorder = crate::test::TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let key = |name: &'static str| Key::from_parts(name, vec![Label::new("id", "a")]);

        assert_eq!(Telemetry::latency_seconds_name(), "derived_test_latency_seconds");
        assert_eq!(
            Telemetry::latency_seconds_min_name(),
            "derived_test_latency_seconds_min"
        );
        assert_eq!(
            Telemetry::latency_seconds_max_name(),
            "derived_test_latency_seconds_max"
        );
        assert_eq!(Telemetry::dropped_total_name(), "derived_test_dropped_total");

        let telemetry = Telemetry::new("a");

        // The gauges register in `new()`, with the histogram's labels, so they read 0 before the first value.
        assert_eq!(recorder.gauge(key("derived_test_latency_seconds_min")), Some(0.0));
        assert_eq!(recorder.gauge(key("derived_test_latency_seconds_max")), Some(0.0));

        let clone = telemetry.clone();
        telemetry.latency_seconds().record(0.5);
        clone.latency_seconds().record(2.0);
        telemetry.latency_seconds().record(0.25);
        clone.latency_seconds().record(1.0);

        assert_eq!(
            recorder.histogram(key("derived_test_latency_seconds")),
            Some(vec![0.5, 2.0, 0.25, 1.0])
        );
        assert_eq!(recorder.gauge(key("derived_test_latency_seconds_min")), Some(0.25));
        assert_eq!(recorder.gauge(key("derived_test_latency_seconds_max")), Some(2.0));

        telemetry.dropped_total("full").increment(1);
        let dropped_key = Key::from_parts(
            "derived_test_dropped_total",
            vec![Label::new("id", "a"), Label::new("reason", "full")],
        );
        assert_eq!(recorder.counter(dropped_key), Some(1));
    }

    proptest! {
        #[test]
        fn property_test_derived_gauge_matches_running_extreme(
            // `any::<f64>()` never yields `NaN` or infinities, and small integers make repeated values likely.
            values in vec(prop_oneof![(-4i8..4).prop_map(f64::from), proptest::num::f64::ANY], 0..64),
            track_max in any::<bool>(),
        ) {
            let (derived, recording) = recording_tracker(if track_max { Kind::Max } else { Kind::Min });

            // The model publishes each non-`NaN` value that is strictly lower or higher than every value before it.
            let mut expected = Vec::new();
            let mut extreme: Option<f64> = None;
            for value in &values {
                derived.observe(*value);
                let improves = !value.is_nan()
                    && extreme.is_none_or(|extreme| if track_max { *value > extreme } else { *value < extreme });
                if improves {
                    extreme = Some(*value);
                    expected.push(*value);
                }
            }

            prop_assert_eq!(recording.writes(), expected);
        }
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use loom::sync::{atomic::AtomicU64, Mutex};
    use metrics::GaugeFn;

    use super::*;

    /// A gauge that records every value passed to `set`.
    ///
    /// It uses a loom mutex, so loom can switch threads between a tracker update and the gauge write.
    #[derive(Default)]
    struct RecordingGauge {
        writes: Mutex<Vec<f64>>,
    }

    impl GaugeFn for RecordingGauge {
        fn increment(&self, _: f64) {
            unreachable!("derived gauges only call `set`");
        }

        fn decrement(&self, _: f64) {
            unreachable!("derived gauges only call `set`");
        }

        fn set(&self, value: f64) {
            self.writes.lock().unwrap().push(value);
        }
    }

    /// A tracker that updates its value with a CAS and then sets the gauge outside any lock.
    ///
    /// Two callers can both win a CAS and then set the gauge in the opposite order. This is the race that
    /// `DerivedGauge::publish` prevents with its lock.
    struct UnlockedTracker {
        current: AtomicU64,
        gauge: Gauge,
    }

    impl UnlockedTracker {
        fn observe(&self, value: f64) {
            let updated = self.current.fetch_update(Relaxed, Relaxed, |current| {
                Kind::Max
                    .improves(value, f64::from_bits(current))
                    .then_some(value.to_bits())
            });
            if updated.is_ok() {
                self.gauge.set(value);
            }
        }
    }

    fn check_writes(writes: &[f64], inputs: &[f64], kind: Kind, expected: f64) {
        for pair in writes.windows(2) {
            assert!(
                kind.improves(pair[1], pair[0]),
                "gauge moved backwards or repeated a value: {writes:?}"
            );
        }
        assert_eq!(writes.last(), Some(&expected), "final gauge value is wrong: {writes:?}");
        assert!(
            writes.iter().all(|write| inputs.contains(write)),
            "unexpected write: {writes:?}"
        );
    }

    /// Observes `inputs[0]` on the model's main thread and each other input on its own thread.
    fn model_derived_gauge(kind: Kind, inputs: [f64; 3], expected: f64) {
        loom::model(move || {
            let recording = Arc::new(RecordingGauge::default());
            let derived = DerivedGauge::new(kind, Gauge::from_arc(Arc::clone(&recording)));

            let threads: Vec<_> = inputs[1..]
                .iter()
                .map(|&value| {
                    let derived = derived.clone();
                    loom::thread::spawn(move || derived.observe(value))
                })
                .collect();
            derived.observe(inputs[0]);
            for thread in threads {
                thread.join().unwrap();
            }

            check_writes(&recording.writes.lock().unwrap(), &inputs, kind, expected);
        });
    }

    #[test]
    fn max_never_moves_back_and_ends_at_highest() {
        model_derived_gauge(Kind::Max, [2.0, 3.0, 1.0], 3.0);
    }

    #[test]
    fn min_never_moves_back_and_ends_at_lowest() {
        model_derived_gauge(Kind::Min, [2.0, 1.0, 3.0], 1.0);
    }

    #[test]
    fn equal_values_write_once() {
        model_derived_gauge(Kind::Max, [2.0, 2.0, 1.0], 2.0);
    }

    // Shows that the model can catch the race that the lock in `DerivedGauge::publish` prevents.
    #[test]
    #[should_panic(expected = "gauge moved backwards")]
    fn unlocked_publish_can_move_the_gauge_back() {
        loom::model(|| {
            let recording = Arc::new(RecordingGauge::default());
            let tracker = Arc::new(UnlockedTracker {
                current: AtomicU64::new(UNSET),
                gauge: Gauge::from_arc(Arc::clone(&recording)),
            });

            let other = Arc::clone(&tracker);
            let thread = loom::thread::spawn(move || other.observe(2.0));
            tracker.observe(1.0);
            thread.join().unwrap();

            check_writes(&recording.writes.lock().unwrap(), &[1.0, 2.0], Kind::Max, 2.0);
        });
    }
}
