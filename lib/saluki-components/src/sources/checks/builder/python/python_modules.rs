use std::sync::OnceLock;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use saluki_config::GenericConfiguration;
use saluki_core::data_model::event::{service_check::ServiceCheck, Event};
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, trace};

use crate::sources::checks::check_metric::{CheckMetric, MetricType};

// Global state to store the sender
static METRIC_SENDER: OnceLock<Sender<Event>> = OnceLock::new();
// Global state to store the configuration
static GLOBAL_CONFIGURATION: OnceLock<GenericConfiguration> = OnceLock::new();
// Global state to store the configuration
static GLOBAL_HOST: OnceLock<String> = OnceLock::new();

/// Sets the metric sender to be used by the aggregator module.
pub fn set_metric_sender(check_metrics_tx: Sender<Event>) -> &'static Sender<Event> {
    METRIC_SENDER.get_or_init(|| check_metrics_tx)
}

/// Stores the configuratioon for integration to use in the Python module.
pub fn set_configuration(configuration: Option<GenericConfiguration>) -> &'static GenericConfiguration {
    GLOBAL_CONFIGURATION.get_or_init(move || configuration.unwrap())
}

/// Stores the configuratioon for integration to use in the Python module.
pub fn set_hostname(hostname: String) -> &'static String {
    GLOBAL_HOST.get_or_init(|| hostname)
}

fn try_send_metric(metric: CheckMetric) -> Result<(), GenericError> {
    match METRIC_SENDER.get() {
        Some(sender) => sender
            .try_send(metric.into())
            .map_err(|e| generic_error!("Failed to send metric: {}", e)),
        None => Err(generic_error!("Metric sender not initialized.")),
    }
}

fn try_send_service_check(service_check: ServiceCheck) -> Result<(), GenericError> {
    match METRIC_SENDER.get() {
        Some(sender) => sender
            .try_send(Event::ServiceCheck(service_check))
            .map_err(|e| generic_error!("Failed to send metric: {}", e)),
        None => Err(generic_error!("Metric sender not initialized.")),
    }
}

fn get_config_key(key: String) -> String {
    match GLOBAL_CONFIGURATION.get() {
        Some(configuration) => configuration.get_typed_or_default::<String>(&key),

        None => "".to_string(),
    }
}

fn fetch_hostname() -> String {
    match GLOBAL_HOST.get() {
        Some(host) => host.clone(),
        None => "".to_string(),
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
    fn submit_service_check(
        _class: PyObject, _check_id: String, name: String, status: i32, tags: Vec<String>, hostname: String,
        message: Option<String>,
    ) {
        trace!(
            "submit_service_check called with name: {}, status: {}, tags: {:?}, hostname: {}, message: {:?}",
            name,
            status,
            tags,
            hostname,
            message
        );

        if let Ok(service_check_status) = status.try_into() {
            let service_check = ServiceCheck::new(&name, service_check_status)
                .with_tags(Some(tags.into_iter().map(|tag| tag.into()).collect()))
                .with_hostname(if hostname.is_empty() {
                    None
                } else {
                    Some(hostname.into())
                })
                .with_message(Some(message.unwrap_or_default().into()));

            if let Err(e) = try_send_service_check(service_check) {
                error!("Failed to send service check: {}", e);
            }
        } else {
            error!("Invalid service check status: {}", status);
        }
    }

    #[pyfunction]
    fn submit_event(_class: PyObject, _check_id: String, event: PyObject) {
        // TODO:
        Python::with_gil(|py| {
            // Convert PyObject to PyDict
            if let Ok(event_dict) = event.downcast_bound::<PyDict>(py) {
                trace!("submit_event called with event dictionary {:?}", event_dict);
            }
        })
    }
}

#[pymodule]
pub mod datadog_agent {
    use super::*;

    #[pyfunction]
    fn get_hostname() -> String {
        trace!("Called get_hostname()");
        let hostname = fetch_hostname();
        trace!("Hostname fetched: {}", hostname);
        hostname
    }

    #[pyfunction]
    fn get_config(config_option: String) -> String {
        trace!("Called get_config({})", config_option);

        get_config_key(config_option)
    }

    #[pyfunction]
    fn get_version() -> &'static str {
        trace!("Called get_version()");
        "0.0.0"
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
