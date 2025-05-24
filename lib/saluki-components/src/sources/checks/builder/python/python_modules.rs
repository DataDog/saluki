use pyo3::prelude::*;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, trace};

use crate::sources::checks::check_metric::{CheckMetric, MetricType};

// Thsi custom class is used to hold the sender for the submission queue
#[pyclass]
pub struct SubmissionQueue {
    pub sender: Sender<CheckMetric>,
}

/// submit_metric is called from the AgentCheck implementation when a check submits a metric.
/// Python signature:
///     aggregator.submit_metric(self, self.check_id, mtype, name, value, tags, hostname, flush_first_value)
#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(pass_module)]
pub(crate) fn submit_metric(
    module: &Bound<PyModule>, _class: PyObject, _check_id: String, mtype: i32, name: String, value: f64,
    tags: Vec<String>, hostname: String, _flush_first_value: bool,
) {
    trace!(
        "submit_metric called with name: {}, value: {}, tags: {:?}, hostname: {}",
        name,
        value,
        tags,
        hostname
    );

    let check_metric = CheckMetric::new(name.clone(), mtype.into(), value, tags.clone());

    match module.getattr("_submission_queue") {
        Ok(py_item) => match py_item.extract::<Py<SubmissionQueue>>() {
            Ok(q) => {
                let py = py_item.py();
                let sender = &q.bind_borrowed(py).borrow_mut().sender;

                match sender.try_send(check_metric) {
                    Ok(_) => { /* nothing to do, success! */ }
                    Err(e) => error!("Failed to send metric: {}", e),
                }
            }
            Err(e) => unreachable!("Failed to extract _submission_queue: {}", e),
        },
        Err(e) => {
            // This is a fatal error and should be impossible to hit this
            unreachable!("_submission_queue not found: {}", e);
        }
    };
}

#[pyfunction]
#[pyo3(signature = (name, status, tags, hostname, message=None))]
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

    m.add("GAUGE", MetricType::Gauge as i32)?;
    m.add("RATE", MetricType::Rate as i32)?;
    m.add("COUNT", MetricType::Count as i32)?;
    m.add("MONOTONIC_COUNT", MetricType::MonotonicCount as i32)?;
    m.add("COUNTER", MetricType::Counter as i32)?;
    m.add("HISTOGRAM", MetricType::Histogram as i32)?;
    m.add("HISTORATE", MetricType::Historate as i32)?;

    Ok(())
}

#[pyfunction]
fn get_hostname() -> &'static str {
    trace!("Called get_hostname()");
    "stubbed.hostname"
}

#[pyfunction]
fn set_hostname(hostname: String) {
    trace!("Called set_hostname({})", hostname);
    // In a function context without a struct, we cannot actually "set" the hostname persistently.
}

#[pyfunction]
fn reset_hostname() {
    trace!("Called reset_hostname()");
    // Similar to `set_hostname`, we cannot reset without a persistent structure.
}

#[pyfunction]
fn get_config(config_option: String) -> bool {
    trace!("Called get_config({})", config_option);

    false
}

#[pyfunction]
fn get_version() -> &'static str {
    trace!("Called get_version()");
    "0.0.0"
}

#[pyfunction]
fn log(message: String, level: u32) {
    match level {
        // All except trace are from python3 logging levels
        // https://docs.python.org/3/library/logging.html#levels
        // Trace is manually specified in datadog_checks_base
        // https://github.com/DataDog/integrations-core/blob/458274dfd867b40e368c795574b6d97a9b7e471d/datadog_checks_base/datadog_checks/base/log.py#L20-L21
        // Currently the '_check_id' / 'check_id' field is unset inside python-world
        // so these logs say "Unknown" instead of the check-name
        40 => tracing::event!(tracing::Level::ERROR, "Python Log: {}", message),
        30 => tracing::event!(tracing::Level::WARN, "Python Log: {}", message),
        20 => tracing::event!(tracing::Level::INFO, "Python Log: {}", message),
        10 => tracing::event!(tracing::Level::DEBUG, "Python Log: {}", message),
        7 => tracing::event!(tracing::Level::TRACE, "Python Log: {}", message),
        _ => tracing::event!(tracing::Level::TRACE, "Python Log: {}", message),
    };
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
