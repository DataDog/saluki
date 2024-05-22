use tracing::{event, trace};

use super::*;

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
    trace!(
        "submit_metric called with name: {}, value: {}, tags: {:?}, hostname: {}",
        name,
        value,
        tags,
        hostname
    );

    let check_metric = CheckMetric {
        name,
        metric_type: mtype.into(),
        value,
        tags,
    };

    match module.getattr("SUBMISSION_QUEUE") {
        Ok(py_item) => match py_item.extract::<Py<python_scheduler::PythonSenderHolder>>() {
            Ok(q) => {
                let res = pyo3::Python::with_gil(|py| q.bind_borrowed(py).borrow_mut().sender.clone());

                match res.try_send(check_metric) {
                    Ok(_) => { /* nothing to do, success! */ }
                    Err(e) => error!("Failed to send metric: {}", e),
                }
            }
            Err(e) => unreachable!("Failed to extract SUBMISSION_QUEUE: {}", e),
        },
        Err(e) => {
            // This is a fatal error and should be impossible to hit this
            unreachable!("SUBMISSION_QUEUE not found: {}", e);
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
