use super::*;
use pyo3::prelude::*;

#[pyfunction]
fn submit_metric(name: String, value: f64, tags: Vec<String>, hostname: String) {
    println!(
        "submit_metric called with name: {}, value: {}, tags: {:?}, hostname: {}",
        name, value, tags, hostname
    );
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

    Ok(())
}
