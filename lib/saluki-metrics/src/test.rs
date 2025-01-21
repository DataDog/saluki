//! Testing-related helpers.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Arc, Mutex,
    },
};

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};

struct CounterStorage {
    total: AtomicU64,
}

impl CounterStorage {
    fn total(&self) -> u64 {
        self.total.load(SeqCst)
    }
}

impl CounterFn for CounterStorage {
    fn increment(&self, value: u64) {
        self.total.fetch_add(value, SeqCst);
    }

    fn absolute(&self, value: u64) {
        self.total.store(value, SeqCst);
    }
}

struct GaugeStorage {
    current: AtomicU64,
}

impl GaugeStorage {
    fn current(&self) -> f64 {
        f64::from_bits(self.current.load(SeqCst))
    }
}

impl GaugeFn for GaugeStorage {
    fn increment(&self, value: f64) {
        self.current
            .fetch_update(SeqCst, SeqCst, |v| {
                let new = f64::from_bits(v) + value;
                Some(new.to_bits())
            })
            .unwrap();
    }

    fn decrement(&self, value: f64) {
        self.current
            .fetch_update(SeqCst, SeqCst, |v| {
                let new = f64::from_bits(v) - value;
                Some(new.to_bits())
            })
            .unwrap();
    }

    fn set(&self, value: f64) {
        self.current.store(value.to_bits(), SeqCst);
    }
}

struct HistogramStorage {
    samples: Mutex<Vec<f64>>,
}

impl HistogramStorage {
    fn samples(&self) -> Vec<f64> {
        let samples = self.samples.lock().unwrap();
        samples.to_vec()
    }
}

impl HistogramFn for HistogramStorage {
    fn record(&self, value: f64) {
        let mut samples = self.samples.lock().unwrap();
        samples.push(value);
    }
}

#[derive(Default)]
struct RecorderState {
    counters: HashMap<Key, Arc<CounterStorage>>,
    gauges: HashMap<Key, Arc<GaugeStorage>>,
    histograms: HashMap<Key, Arc<HistogramStorage>>,
}

/// A recorder implementation that stores metrics in memory for testing purposes.
///
/// This recorder is exposes a simplistic API for querying the current values of specific counters, gauges, or
/// histograms. It is intended for use in unit tests to verify that metrics are being recorded correctly.
#[derive(Default)]
pub struct TestRecorder {
    state: Arc<Mutex<RecorderState>>,
}

impl TestRecorder {
    /// Returns the current value of the counter with the given key, or `None` if no such counter exists.
    pub fn counter<K>(&self, key: K) -> Option<u64>
    where
        K: Into<Key>,
    {
        let state = self.state.lock().unwrap();
        let counter = state.counters.get(&key.into())?;
        Some(counter.total())
    }

    /// Returns the current value of the gauge with the given key, or `None` if no such gauge exists.
    pub fn gauge<K>(&self, key: K) -> Option<f64>
    where
        K: Into<Key>,
    {
        let state = self.state.lock().unwrap();
        let gauge = state.gauges.get(&key.into())?;
        Some(gauge.current())
    }

    /// Returns the current samples of the histogram with the given key, or `None` if no such histogram exists.
    pub fn histogram<K>(&self, key: K) -> Option<Vec<f64>>
    where
        K: Into<Key>,
    {
        let state = self.state.lock().unwrap();
        let histogram = state.histograms.get(&key.into())?;
        Some(histogram.samples())
    }
}

impl Recorder for TestRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let mut state = self.state.lock().unwrap();
        let counter = state.counters.entry(key.clone()).or_insert_with(|| {
            Arc::new(CounterStorage {
                total: AtomicU64::new(0),
            })
        });

        Counter::from_arc(Arc::clone(counter))
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        let mut state = self.state.lock().unwrap();
        let gauge = state.gauges.entry(key.clone()).or_insert_with(|| {
            Arc::new(GaugeStorage {
                current: AtomicU64::new(0.0f64.to_bits()),
            })
        });

        Gauge::from_arc(Arc::clone(gauge))
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let mut state = self.state.lock().unwrap();
        let histogram = state.histograms.entry(key.clone()).or_insert_with(|| {
            Arc::new(HistogramStorage {
                samples: Mutex::new(Vec::new()),
            })
        });

        Histogram::from_arc(Arc::clone(histogram))
    }
}
