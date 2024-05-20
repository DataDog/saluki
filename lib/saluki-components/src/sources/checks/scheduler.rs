use super::python_exposed_modules::aggregator as pyagg;
use super::python_exposed_modules::datadog_agent;
use super::*;
use pyo3::prelude::PyAnyMethods;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::types::PyList;
use pyo3::types::PyType;
use saluki_error::{generic_error, GenericError};

struct CheckHandle(Py<PyAny>);

impl Clone for CheckHandle {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[pyclass]
pub struct SenderHolder {
    pub sender: mpsc::Sender<CheckMetric>,
}

pub struct CheckScheduler {
    running: HashMap<CheckSource, (CheckHandle, Vec<tokio::task::JoinHandle<()>>)>,
}

impl CheckScheduler {
    pub fn new(send_check_metrics: mpsc::Sender<CheckMetric>) -> Self {
        pyo3::append_to_inittab!(datadog_agent);
        pyo3::append_to_inittab!(pyagg);

        pyo3::prepare_freethreaded_python();

        pyo3::Python::with_gil(|py| {
            // Initialize the aggregator module with the submission queue
            match py.import_bound("aggregator") {
                Ok(m) => {
                    let sender_holder = Bound::new(
                        py,
                        SenderHolder {
                            sender: send_check_metrics.clone(),
                        },
                    )
                    .expect("Could not create sender holder");
                    m.setattr("SUBMISSION_QUEUE", sender_holder)
                        .expect("Could not set sender_holder on module attribute")
                }
                Err(e) => {
                    error!(%e, "Could not import aggregator module.");
                    if let Some(traceback) = e.traceback_bound(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return; // fatal
                }
            };

            // Test to ensure expected module is available
            // fail early, it is fatal if these are not present
            let modd = match py.import_bound("datadog_checks.checks") {
                Ok(m) => m,
                Err(e) => {
                    let traceback = e
                        .traceback_bound(py)
                        .expect("Traceback should be present on this error");
                    error!(%e, "Could not import datadog_checks module. traceback: {}", traceback.format().expect("Could format traceback"));
                    return; // fatal
                }
            };
            let class = match modd.getattr("AgentCheck") {
                Ok(c) => c,
                Err(e) => {
                    let traceback = e
                        .traceback_bound(py)
                        .expect("Traceback should be present on this error");
                    error!(%e, "Could not get AgentCheck class. traceback: {}", traceback.format().expect("Could format traceback"));
                    return; // fatal
                }
            };
            info!(%class, %modd, "Was able to import AgentCheck!");
        });

        Self {
            running: HashMap::new(),
        }
    }

    // See `CheckRequest::to_runnable_request` for more TODO items here
    // Returns an opaque handle to the python class implementing the class
    fn register_check(&mut self, check_source_path: PathBuf) -> Result<CheckHandle, GenericError> {
        let py_source = std::fs::read_to_string(&check_source_path).unwrap();

        pyo3::Python::with_gil(|py| {
            let locals = pyo3::types::PyDict::new_bound(py);

            match py.run_bound(&py_source, None, Some(&locals)) {
                Ok(_) => {}
                Err(e) => {
                    let traceback = e
                        .traceback_bound(py)
                        .expect("Traceback should be present on this error");
                    error!(%e, "Could not compile check source. traceback: {}", traceback.format().expect("Could format traceback"));
                    return Err(generic_error!("Could not compile check source"));
                }
            };
            let base_class = locals
                .get_item("AgentCheck")
                .expect("Could not get 'AgentCheck' class")
                .unwrap();
            let checks = locals
                .iter()
                .filter(|(_, value)| {
                    if let Ok(class_obj) = value.downcast::<PyType>() {
                        if class_obj.is(&base_class) {
                            // skip the base class
                            return false;
                        }
                        if let Ok(true) = class_obj.is_subclass(&base_class) {
                            return true;
                        }
                        false
                    } else {
                        false
                    }
                })
                .collect::<Vec<_>>();

            if checks.is_empty() {
                return Err(generic_error!("No checks found in source"));
            }
            if checks.len() >= 2 {
                return Err(generic_error!("Multiple checks found in source"));
            }
            let (check_key, check_value) = &checks[0];
            info!(
                "For check source {}, found check: {}",
                check_source_path.display(),
                check_key
            );
            Ok(CheckHandle(check_value.as_unbound().clone()))
        })
    }

    // This function does 3 things
    // 1. Registers the check source code and gets a handle
    // 2. Starts a local task for each instance that
    //    queues a run of the check every min_collection_interval_ms
    // 3. Stores the handles in the running hashmap
    pub fn run_check(&mut self, check: RunnableCheckRequest) -> Result<(), GenericError> {
        // registry should probably queue off of checkhandle
        //let current = self.running.entry(check.check_request.source.clone()).or_default();

        let check_handle = self.register_check(check.check_source_code.clone())?;
        let running_entry = self
            .running
            .entry(check.check_request.source)
            .or_insert((check_handle.clone(), Vec::new()));

        for (idx, instance) in check.check_request.instances.iter().enumerate() {
            let instance = instance.clone();

            let check_handle = check_handle.clone();
            let handle = tokio::task::spawn(async move {
                let mut interval =
                    tokio::time::interval(Duration::from_millis(instance.min_collection_interval_ms.into()));
                loop {
                    interval.tick().await;
                    // run check
                    info!("Running check instance {idx}");
                    pyo3::Python::with_gil(|py| {
                        let instance_as_pydict = PyDict::new_bound(py);
                        instance_as_pydict
                            .set_item("min_collection_interval_ms", instance.min_collection_interval_ms)
                            .expect("could not set min-collection-interval");

                        let instance_list = PyList::new_bound(py, &[instance_as_pydict]);
                        let kwargs = PyDict::new_bound(py);
                        kwargs
                            .set_item("name", "placeholder_check_name")
                            .expect("Could not set name");
                        kwargs
                            .set_item("init_config", PyDict::new_bound(py))
                            .expect("could not set init_config"); // todo this is in the check request maybe
                        kwargs
                            .set_item("instances", instance_list)
                            .expect("could not set instance list");

                        let pycheck = match check_handle.0.call_bound(py, (), Some(&kwargs)) {
                            Ok(c) => c,
                            Err(e) => {
                                let traceback = e
                                    .traceback_bound(py)
                                    .expect("Traceback should be present on this error");
                                error!(%e, "Could not instantiate check. traceback: {}", traceback.format().expect("Could format traceback"));
                                return;
                            }
                        };
                        // 'run' method invokes 'check' with the instance we initialized with
                        // ref https://github.com/DataDog/integrations-core/blob/bc3b1c3496e79aa1b75ebcc9ef1c2a2b26487ebd/datadog_checks_base/datadog_checks/base/checks/base.py#L1197
                        let result = pycheck.call_method0(py, "run").unwrap();

                        let s: String = result
                            .extract(py)
                            .expect("Can't read the string result from the check execution");
                        // TODO(remy): turn this into debug log level later on
                        debug!("Check execution returned {:?}", s);
                    })
                }
            });
            running_entry.1.push(handle);
        }

        Ok(())
    }

    pub fn stop_check(&mut self, check: CheckRequest) {
        info!("Deleting check request {check}");
        if let Some((check_handle, running)) = self.running.remove(&check.source) {
            for handle in running.iter() {
                handle.abort();
            }
            drop(check_handle); // release the reference to this check
        }
    }
}
