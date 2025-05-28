use std::sync::{Arc, OnceLock};

use pyo3::prelude::*;
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, trace};

use crate::sources::checks::check_metric::{CheckMetric, MetricType};

// Global state to store the sender
static METRIC_SENDER: OnceLock<Arc<Sender<CheckMetric>>> = OnceLock::new();

/// Sets the metric sender to be used by the aggregator module.
pub fn set_metric_sender(check_metrics_tx: Sender<CheckMetric>) -> &'static Arc<Sender<CheckMetric>> {
    METRIC_SENDER.get_or_init(|| Arc::new(check_metrics_tx))
}

fn try_send_metric(metric: CheckMetric) -> Result<(), GenericError> {
    match METRIC_SENDER.get() {
        Some(sender) => sender
            .try_send(metric)
            .map_err(|e| generic_error!("Failed to send metric: {}", e)),
        None => Err(generic_error!("Metric sender not initialized.")),
    }
}

#[pymodule]
pub mod aggregator {
    use super::*;

    #[pymodule_export]
    const GAUGE: i32 = MetricType::Gauge as i32;
    #[pymodule_export]
    const RATE: i32 = MetricType::Rate as i32;
    #[pymodule_export]
    const COUNT: i32 = MetricType::Count as i32;
    #[pymodule_export]
    const MONOTONIC_COUNT: i32 = MetricType::MonotonicCount as i32;
    #[pymodule_export]
    const COUNTER: i32 = MetricType::Counter as i32;
    #[pymodule_export]
    const HISTOGRAM: i32 = MetricType::Histogram as i32;
    #[pymodule_export]
    const HISTORATE: i32 = MetricType::Historate as i32;

    #[allow(clippy::too_many_arguments)]
    #[pyfunction]
    fn submit_metric(
        _class: PyObject, _check_id: String, mtype: i32, name: String, value: f64, tags: Vec<String>, hostname: String,
        _flush_first_value: bool,
    ) {
        trace!(
            "submit_metric called with name: {}, value: {}, tags: {:?}, hostname: {}",
            name,
            value,
            tags,
            hostname
        );

        let check_metric = CheckMetric::new(name.clone(), mtype.into(), value, tags.clone());

        if let Err(e) = try_send_metric(check_metric) {
            error!("Failed to send metric: {}", e);
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
}

#[pymodule]
pub mod datadog_agent {
    use super::*;

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
}
