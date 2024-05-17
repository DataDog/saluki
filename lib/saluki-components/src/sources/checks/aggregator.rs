use super::*;
use pyo3::prelude::*;
use saluki_event::{metric::*, Event};
use saluki_env::time::get_unix_timestamp;

#[derive(Clone, Copy)]
enum PyMetricType {
    Gauge = 0,
    Rate,
    Count,
    MonotonicCount,
    Counter,
    Histogram,
    Historate,
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum AggregatorError {
    UnsupportedType{},
}

impl TryFrom<i32> for PyMetricType {
    type Error = ();

    fn try_from(v: i32) -> Result<Self, Self::Error> {
        match v {
            x if x == PyMetricType::Gauge as i32 => Ok(PyMetricType::Gauge),
            x if x == PyMetricType::Rate as i32 => Ok(PyMetricType::Rate),
            x if x == PyMetricType::Count as i32 => Ok(PyMetricType::Count),
            x if x == PyMetricType::MonotonicCount as i32 => Ok(PyMetricType::MonotonicCount),
            x if x == PyMetricType::Counter as i32 => Ok(PyMetricType::Counter),
            x if x == PyMetricType::Histogram as i32 => Ok(PyMetricType::Histogram),
            x if x == PyMetricType::Historate as i32 => Ok(PyMetricType::Historate),
            _ => Err(()),
        }
    }
}

/// CheckMetric are used to transmit metrics from python check execution results
/// to forward in the saluki's pipeline.
pub struct CheckMetric {
    name: String,
    metric_type: PyMetricType,
    value: f64,
    tags: Vec<String>,
}

// TODO(remy): use TryFrom instead
pub fn check_metric_as_event(metric: CheckMetric) -> Result<Event, AggregatorError> {
    let mut tags = MetricTags::default();
    // TODO(remy): do this more idiomatically
    for tag in metric.tags.iter() {
        tags.insert_tag(tag.clone());
    }

    let context = MetricContext{
                    name: metric.name,
                    tags,
    };
    let metadata = MetricMetadata::from_timestamp(get_unix_timestamp());

    match metric.metric_type {
        PyMetricType::Gauge => {
            Ok(saluki_event::Event::Metric(Metric::from_parts(
                context, MetricValue::Gauge { value: metric.value, }, metadata,
            )))
        },
        PyMetricType::Counter => {
            Ok(saluki_event::Event::Metric(Metric::from_parts(
                context, MetricValue::Counter { value: metric.value, }, metadata,
            )))
        },
        // TODO(remy): rest of the types
        _ => Err(AggregatorError::UnsupportedType{}),
    }
}

impl Clone for CheckMetric {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            metric_type: self.metric_type,
            value: self.value.clone(),
            tags: self.tags.clone(),
        }
    }
}


/// Global for Python checks execution to report the data
pub static SUBMISSION_QUEUE: Lazy<Mutex<Queue<CheckMetric>>> = Lazy::new(|| {
    Mutex::new(queue![])
});

/// submit_metric is called from the AgentCheck implementation when a check submits a metric.
/// Python signature:
///     aggregator.submit_metric(self, self.check_id, mtype, name, value, tags, hostname, flush_first_value)
///
/// TODO(remy): should mtype be a PyMetricType?
#[pyfunction]
fn submit_metric(_class: PyObject, _check_id: String, mtype: i32, name: String, value: f64, tags: Vec<String>, hostname: String, _flush_first_value: bool) {
    println!(
        "submit_metric called with name: {}, value: {}, tags: {:?}, hostname: {}",
        name, value, tags, hostname
    );

    let metric_type = match PyMetricType::try_from(mtype) {
        Ok(mt) => mt,
        Err(e) => {
            error!("can't convert metric type: {}", mtype);
            PyMetricType::Gauge
        }
    };

    let mut q = SUBMISSION_QUEUE.lock().unwrap();
    match q.add(CheckMetric{
        name,
        metric_type,
        value,
        tags,
    }) {
        Ok(_) => {},
        Err(e) => {
            error!("can't push into the submission queue: {}", e);
        }
    }
    drop(q);
}

#[pyfunction]
fn submit_service_check(name: String, status: i32, tags: Vec<String>, hostname: String, message: Option<String>) {
    println!(
        "submit_service_check called with name: {}, status: {}, tags: {:?}, hostname: {}, message: {:?}",
        name, status, tags, hostname, message
    );
}

#[pyfunction]
fn metrics(name: String) -> Vec<String> {
    println!("metrics called for: {}", name);
    vec![] // Dummy return
}

#[pyfunction]
fn reset() {
    println!("reset called");
}

#[pymodule]
pub fn aggregator(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(submit_metric, m)?)?;
    m.add_function(wrap_pyfunction!(submit_service_check, m)?)?;
    m.add_function(wrap_pyfunction!(self::metrics, m)?)?;
    m.add_function(wrap_pyfunction!(reset, m)?)?;

    m.add("GAUGE", PyMetricType::Gauge as i32)?;
    m.add("RATE", PyMetricType::Rate as i32)?;
    m.add("COUNT", PyMetricType::Count as i32)?;
    m.add("MONOTONIC_COUNT", PyMetricType::MonotonicCount as i32)?;
    m.add("COUNTER", PyMetricType::Counter as i32)?;
    m.add("HISTOGRAM", PyMetricType::Histogram as i32)?;
    m.add("HISTORATE", PyMetricType::Historate as i32)?;

    Ok(())
}
