use super::*;
use pyo3::prelude::*;
use saluki_env::time::get_unix_timestamp;
use saluki_event::{metric::*, Event};
use tracing::warn;

#[derive(Clone, Copy)]
pub enum PyMetricType {
    Gauge = 0,
    Rate,
    Count,
    MonotonicCount,
    Counter,
    Histogram,
    Historate,
}

impl From<i32> for PyMetricType {
    fn from(v: i32) -> Self {
        match v {
            0 => PyMetricType::Gauge,
            1 => PyMetricType::Rate,
            2 => PyMetricType::Count,
            3 => PyMetricType::MonotonicCount,
            4 => PyMetricType::Counter,
            5 => PyMetricType::Histogram,
            6 => PyMetricType::Historate,
            _ => {
                warn!("Unknown metric type: {}, considering it as a gauge", v);
                PyMetricType::Gauge
            }
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum AggregatorError {
    UnsupportedType {},
}

/// CheckMetric are used to transmit metrics from python check execution results
/// to forward in the saluki's pipeline.
pub struct CheckMetric {
    name: String,
    metric_type: PyMetricType,
    value: f64,
    tags: Vec<String>,
}

impl TryInto<Event> for CheckMetric {
    type Error = AggregatorError;

    fn try_into(self) -> Result<Event, Self::Error> {
        let tags: MetricTags = self.tags.into();

        let context = MetricContext { name: self.name, tags };
        let metadata = MetricMetadata::from_timestamp(get_unix_timestamp());

        match self.metric_type {
            PyMetricType::Gauge => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                MetricValue::Gauge { value: self.value },
                metadata,
            ))),
            PyMetricType::Counter => Ok(saluki_event::Event::Metric(Metric::from_parts(
                context,
                MetricValue::Counter { value: self.value },
                metadata,
            ))),
            // TODO(remy): rest of the types
            _ => Err(AggregatorError::UnsupportedType {}),
        }
    }
}

impl Clone for CheckMetric {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            metric_type: self.metric_type,
            value: self.value,
            tags: self.tags.clone(),
        }
    }
}

/// submit_metric is called from the AgentCheck implementation when a check submits a metric.
/// Python signature:
///     aggregator.submit_metric(self, self.check_id, mtype, name, value, tags, hostname, flush_first_value)
///
/// TODO(remy): should mtype be a PyMetricType?
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(pass_module)]
pub(crate) fn submit_metric(
    module: &Bound<PyModule>, _class: PyObject, _check_id: String, mtype: i32, name: String, value: f64,
    tags: Vec<String>, hostname: String, _flush_first_value: bool,
) {
    debug!(
        "submit_metric called with name: {}, value: {}, tags: {:?}, hostname: {}",
        name, value, tags, hostname
    );

    let check_metric = CheckMetric {
        name,
        metric_type: mtype.into(),
        value,
        tags,
    };

    match module.getattr("SUBMISSION_QUEUE") {
        Ok(py_item) => match py_item.extract::<Py<scheduler::SenderHolder>>() {
            Ok(q) => {
                let res = pyo3::Python::with_gil(|py| q.bind_borrowed(py).borrow_mut().sender.clone());

                match res.try_send(check_metric) {
                    Ok(_) => debug!("Successfully sent metric"),
                    Err(e) => error!("Failed to send metric: {}", e),
                }
            }
            Err(e) => error!("Failed to extract SUBMISSION_QUEUE: {}", e),
        },
        Err(e) => {
            // Theoretically possible early in the init, but not should be basically impossible
            error!("SUBMISSION_QUEUE not found: {}", e);
        }
    };
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
    m.add_function(wrap_pyfunction_bound!(submit_metric, m)?)?;
    m.add_function(wrap_pyfunction_bound!(submit_service_check, m)?)?;
    m.add_function(wrap_pyfunction_bound!(self::metrics, m)?)?;
    m.add_function(wrap_pyfunction_bound!(reset, m)?)?;

    m.add("GAUGE", PyMetricType::Gauge as i32)?;
    m.add("RATE", PyMetricType::Rate as i32)?;
    m.add("COUNT", PyMetricType::Count as i32)?;
    m.add("MONOTONIC_COUNT", PyMetricType::MonotonicCount as i32)?;
    m.add("COUNTER", PyMetricType::Counter as i32)?;
    m.add("HISTOGRAM", PyMetricType::Histogram as i32)?;
    m.add("HISTORATE", PyMetricType::Historate as i32)?;

    Ok(())
}

#[pyfunction]
fn get_hostname() -> &'static str {
    debug!("Called get_hostname()");
    "stubbed.hostname"
}

#[pyfunction]
fn set_hostname(hostname: String) {
    debug!("Called set_hostname({})", hostname);
    // In a function context without a struct, we cannot actually "set" the hostname persistently.
}

#[pyfunction]
fn reset_hostname() {
    debug!("Called reset_hostname()");
    // Similar to `set_hostname`, we cannot reset without a persistent structure.
}

#[pyfunction]
fn get_config(config_option: String) -> bool {
    debug!("Called get_config({})", config_option);

    false
}

#[pyfunction]
fn get_version() -> &'static str {
    debug!("Called get_version()");
    "0.0.0"
}

#[pyfunction]
fn log(message: String, level: u32) {
    debug!("{level} Log: {}", message);
}

#[pyfunction]
fn set_check_metadata(check_id: String, name: String, value: String) {
    debug!("Called set_check_metadata({}, {}, {})", check_id, name, value);
    // Again, we can only log this because there's no structure to store it.
}

#[pyfunction]
fn tracemalloc_enabled() -> bool {
    // tracemalloc unsupported for now
    false
}

#[pymodule]
pub fn datadog_agent(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(get_hostname, m)?)?;
    m.add_function(wrap_pyfunction!(set_hostname, m)?)?;
    m.add_function(wrap_pyfunction!(reset_hostname, m)?)?;
    m.add_function(wrap_pyfunction!(get_config, m)?)?;
    m.add_function(wrap_pyfunction!(get_version, m)?)?;
    m.add_function(wrap_pyfunction!(log, m)?)?;
    m.add_function(wrap_pyfunction!(set_check_metadata, m)?)?;
    m.add_function(wrap_pyfunction!(tracemalloc_enabled, m)?)?;

    Ok(())
}
