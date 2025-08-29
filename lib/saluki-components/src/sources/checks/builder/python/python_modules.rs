use std::sync::{LazyLock, OnceLock};
use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::PyDict;
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
use saluki_core::data_model::event::{
    eventd::{AlertType, EventD, Priority},
    service_check::ServiceCheck,
    Event,
};
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, trace};

use crate::sources::checks::check_metric::{CheckMetric, MetricType};

use super::execution_context::ExecutionContext;

// Global state to store the sender
static METRIC_SENDER: OnceLock<Sender<Event>> = OnceLock::new();
// Global state to store the configuration
static GLOBAL_CONFIGURATION: OnceLock<GenericConfiguration> = OnceLock::new();

// Global state to store the execution context
static GLOBAL_EXECUTION_CONTEXT: OnceLock<ExecutionContext> = OnceLock::new();

/// Sets the event sender to be used by the aggregator module.
pub fn set_event_sender(check_metrics_tx: Sender<Event>) -> &'static Sender<Event> {
    METRIC_SENDER.get_or_init(|| check_metrics_tx)
}

/// Sets the configuration to be used by the datadog_agent module.
pub fn set_configuration(configuration: GenericConfiguration) -> &'static GenericConfiguration {
    GLOBAL_CONFIGURATION.get_or_init(move || configuration)
}

/// Sets the hostname to be used by the datadog_agent module.
pub fn set_execution_context(execution_context: ExecutionContext) -> &'static ExecutionContext {
    GLOBAL_EXECUTION_CONTEXT.get_or_init(|| execution_context)
}

fn try_send_metric(metric: CheckMetric) -> Result<(), GenericError> {
    match METRIC_SENDER.get() {
        Some(sender) => sender
            .try_send(metric.into())
            .map_err(|e| generic_error!("Failed to send metric: {}", e)),
        None => Err(generic_error!("Event sender not initialized.")),
    }
}

fn try_send_service_check(service_check: ServiceCheck) -> Result<(), GenericError> {
    match METRIC_SENDER.get() {
        Some(sender) => sender
            .try_send(Event::ServiceCheck(service_check))
            .map_err(|e| generic_error!("Failed to send service check: {}", e)),
        None => Err(generic_error!("Event sender not initialized.")),
    }
}

fn try_send_event(event: EventD) -> Result<(), GenericError> {
    match METRIC_SENDER.get() {
        Some(sender) => sender
            .try_send(Event::EventD(event))
            .map_err(|e| generic_error!("Failed to send event: {}", e)),
        None => Err(generic_error!("Event sender not initialized.")),
    }
}

fn get_config_key(key: String) -> String {
    match GLOBAL_CONFIGURATION.get() {
        Some(configuration) => configuration.get_typed_or_default::<String>(&key),

        None => "".to_string(),
    }
}

fn fetch_hostname() -> &'static str {
    match GLOBAL_EXECUTION_CONTEXT.get() {
        Some(ExecutionContext { hostname, ..}) => hostname.as_str(),
        None => "",
    }
}

fn fetch_tracemalloc_enabled() -> bool {
    match GLOBAL_EXECUTION_CONTEXT.get() {
        Some(ExecutionContext { tracemalloc_enabled, ..}) => *tracemalloc_enabled,
        None => false,
    }
}

fn fetch_http_headers() -> &'static HashMap<String, String> {
    static EMPTY : LazyLock<HashMap<String, String>> = LazyLock::new(|| HashMap::new()); // FIXME: yuk!

    match GLOBAL_EXECUTION_CONTEXT.get() {
        Some(ExecutionContext { http_headers, ..}) => http_headers,
        None => &EMPTY,
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
        _class: PyObject, check_id: String, name: String, status: u8, tags: Vec<String>, hostname: String,
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

        let check_status = match status.try_into() {
            Ok(check_status) => check_status,
            _ => {
                error!(
                    check_id,
                    check_status = status,
                    "Check returned invalid/unknown status value."
                );
                return;
            }
        };

        let mut tag_set = TagSet::default();
        for tag in tags {
            tag_set.insert_tag(tag);
        }

        let service_check = ServiceCheck::new(name, check_status)
            .with_tags(tag_set.into_shared())
            .with_hostname(if hostname.is_empty() {
                None
            } else {
                Some(hostname.into())
            })
            .with_message(message.map(Into::into));

        if let Err(e) = try_send_service_check(service_check) {
            error!("Failed to send service check: {}", e);
        }
    }

    #[derive(FromPyObject)]
    struct PythonEvent {
        #[pyo3(item("msg_title"))]
        msg_title: String,
        #[pyo3(item("msg_text"))]
        msg_text: String,
        #[pyo3(item("event_type"), default)]
        #[allow(dead_code)]
        event_type: Option<String>,
        #[pyo3(item("timestamp"), default)]
        timestamp: Option<u64>,
        #[pyo3(item("api_key"), default)]
        #[allow(dead_code)]
        api_key: Option<String>,
        #[pyo3(item("aggregation_key"), default)]
        aggregation_key: Option<String>,
        #[pyo3(item("alert_type"), default)]
        alert_type: Option<String>,
        #[pyo3(item("source_type_name"), default)]
        source_type_name: Option<String>,
        #[pyo3(item("host"), default)]
        host: Option<String>,
        #[pyo3(item("tags"), default)]
        tags: Option<Vec<String>>,
        #[pyo3(item("priority"), default)]
        priority: Option<String>,
    }

    #[pyfunction]
    fn submit_event(_class: PyObject, _check_id: String, event: PyObject) {
        let extracted_event = Python::with_gil(|py| -> PyResult<PythonEvent> {
            let python_event = event.downcast_bound::<PyDict>(py)?;
            let extracted_event: PythonEvent = python_event.extract()?;
            Ok(extracted_event)
        });

        match extracted_event {
            Ok(python_event) => {
                trace!(
                    "submit_event called with title: {}, text: {}, event_type: {:?}, host: {:?}",
                    python_event.msg_title,
                    python_event.msg_text,
                    python_event.event_type,
                    python_event.host
                );

                let mut tag_set = TagSet::default();
                if let Some(tags) = python_event.tags {
                    for tag in tags {
                        tag_set.insert_tag(tag);
                    }
                }

                let mut event_d = EventD::new(python_event.msg_title, python_event.msg_text)
                    .with_timestamp(python_event.timestamp)
                    .with_tags(tag_set.into_shared())
                    .with_hostname(python_event.host.map(Into::into))
                    .with_source_type_name(python_event.source_type_name.map(Into::into))
                    .with_aggregation_key(python_event.aggregation_key.map(Into::into));

                if let Some(alert_type_str) = python_event.alert_type {
                    event_d = event_d.with_alert_type(AlertType::try_from_string(&alert_type_str));
                }

                if let Some(priority_str) = python_event.priority {
                    event_d = event_d.with_priority(Priority::try_from_string(&priority_str));
                }

                if let Err(e) = try_send_event(event_d) {
                    error!("Failed to send event: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to extract event from Python object: {}", e);
            }
        }
    }
}

#[pymodule]
pub mod datadog_agent {
    use super::*;

    #[pyfunction]
    fn get_hostname() -> &'static str {
        trace!("Called get_hostname()");
        fetch_hostname()
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
    fn headers() -> &'static HashMap<String, String> {
        trace!("Called headers()");
        fetch_http_headers()
    }

    #[pyfunction]
    fn tracemalloc_enabled() -> bool {
        trace!("Called tracemalloc_enabled()");
        fetch_tracemalloc_enabled()
    }
}

#[cfg(test)]
mod tests {
    use pyo3::Python;
    use pyo3_ffi::c_str;
    use saluki_config::ConfigurationLoader;
    use saluki_core::data_model::event::service_check::CheckStatus;
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn test_python_aggregator_integration() {
        let (tx, mut rx) = mpsc::channel(100);
        set_event_sender(tx);

        pyo3::append_to_inittab!(aggregator);
        pyo3::prepare_freethreaded_python();

        let code = c_str!(include_str!("tests/test_aggregator.py"));

        let result = Python::with_gil(|py| -> PyResult<()> {
            PyModule::from_code(py, code, c_str!("aggregator.py"), c_str!("aggregator.py"))?;
            Ok(())
        });

        assert!(result.is_ok(), "Python script execution failed: {:?}", result.err());

        let mut metric_count = 0;
        let mut service_check_count = 0;
        let mut event_count = 0;

        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Metric(_) => metric_count += 1,
                Event::ServiceCheck(sc) => {
                    service_check_count += 1;
                    assert_eq!(sc.name(), "test.service_check");
                    assert_eq!(sc.status(), CheckStatus::Ok);
                    assert_eq!(sc.message().unwrap(), "All systems operational");
                }
                Event::EventD(ev) => {
                    event_count += 1;

                    if ev.title() == "Test Event Title" {
                        // Full event test
                        assert_eq!(ev.text(), "This is a test event message from Python");
                        assert_eq!(ev.hostname().unwrap(), "test-event-hostname");
                        assert_eq!(ev.alert_type().unwrap(), AlertType::Warning);
                        assert_eq!(ev.priority().unwrap(), Priority::Low);
                        assert_eq!(ev.aggregation_key().unwrap(), "test-aggregation-key");
                        assert_eq!(ev.source_type_name().unwrap(), "python_test");
                        assert_eq!(ev.timestamp().unwrap(), 1234567890);

                        let tags = ev.tags();
                        assert!(tags.into_iter().any(|tag| tag.as_str() == "event:test"));
                        assert!(tags.into_iter().any(|tag| tag.as_str() == "source:python"));
                        assert!(tags.into_iter().any(|tag| tag.as_str() == "priority:high"));
                    } else if ev.title() == "Minimal Event" {
                        // Minimal event test
                        assert_eq!(ev.text(), "This event has only required fields");
                        assert!(ev.hostname().is_none());
                        assert!(ev.aggregation_key().is_none());
                        assert!(ev.source_type_name().is_none());
                        assert!(ev.timestamp().is_none());
                        assert!(ev.tags().into_iter().count() == 0);
                        // Default values should be set for priority and alert_type
                        assert_eq!(ev.priority().unwrap(), Priority::Normal);
                        assert_eq!(ev.alert_type().unwrap(), AlertType::Info);
                    } else {
                        panic!("Unexpected event title: {}", ev.title());
                    }
                }
            }
        }

        assert!(metric_count >= 2, "Expected at least 2 metrics, got {}", metric_count);
        assert_eq!(
            service_check_count, 1,
            "Expected 1 service check, got {}",
            service_check_count
        );
        assert_eq!(event_count, 2, "Expected 2 events, got {}", event_count);
    }

    struct TestResults {
        hostname: String,
        config_value: String,
    }

    #[tokio::test]
    async fn test_python_datadog_agent_integration() {
        std::env::set_var("DD_TEST_FOO", "bar");

        let config = ConfigurationLoader::default()
            .from_environment("DD_TEST")
            .expect("configuration should be loaded")
            .into_generic()
            .await
            .expect("convert to generic configuration");
        set_configuration(config);

        let execution_context = ExecutionContext {
            hostname: "agent-test-host".to_string(),
            http_headers: HashMap::from([
                ("Sample-Header-1".to_string(), "value1".to_string()),
                ("Sample-Header-2".to_string(), "value2".to_string()),
            ]),
            tracemalloc_enabled: true,
        };
        set_execution_context(execution_context);

        pyo3::append_to_inittab!(datadog_agent);
        pyo3::prepare_freethreaded_python();

        let results = Python::with_gil(|py| -> Result<TestResults, Box<dyn std::error::Error>> {
            let datadog_agent = PyModule::import(py, "datadog_agent")?;

            let hostname: String = datadog_agent.getattr("get_hostname")?.call0()?.extract()?;

            let config_value: String = datadog_agent.getattr("get_config")?.call1(("foo",))?.extract()?;

            Ok(TestResults { hostname, config_value })
        });

        assert!(results.is_ok(), "Python datadog_agent test failed: {:?}", results.err());

        let results = results.unwrap();

        assert!(
            results.hostname == "agent-test-host",
            "Hostname mismatch: {}",
            results.hostname
        );
        assert!(
            results.config_value == "bar",
            "Config value mismatch: {}",
            results.config_value
        );
        std::env::remove_var("DD_TEST_FOO");
    }
}
