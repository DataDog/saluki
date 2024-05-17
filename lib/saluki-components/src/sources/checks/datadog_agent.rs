use super::*;
use pyo3::prelude::*;

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
