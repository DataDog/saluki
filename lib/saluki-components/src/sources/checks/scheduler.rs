use super::*;
use pyo3::prelude::PyAnyMethods;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::types::PyList;
use pyo3::types::PyType;
use saluki_error::{generic_error, GenericError};

struct CheckHandle {
    id: Py<PyAny>,
}

impl Clone for CheckHandle {
    fn clone(&self) -> Self {
        Self { id: self.id.clone() }
    }
}

pub struct CheckScheduler {
    running: HashMap<CheckSource, (CheckHandle, Vec<tokio::task::JoinHandle<()>>)>,
}

impl CheckScheduler {
    pub fn new(//schedule_check_rx: mpsc::Receiver<RunnableCheckRequest>, unschedule_check_rx: mpsc::Receiver<CheckRequest>,
    ) -> Self {
        // todo, add in apis that python checks expect
        //pyo3::append_to_inittab!(pylib_module);

        pyo3::prepare_freethreaded_python();

        // Sanity test for python environment before executing
        pyo3::Python::with_gil(|py| {
            let modd = match py.import_bound("datadog_checks.checks") {
                Ok(m) => m,
                Err(e) => {
                    let traceback = e
                        .traceback_bound(py)
                        .expect("Traceback should be present on this error");
                    error!(%e, "Could not import datadog_checks module. traceback: {}", traceback.format().expect("Could format traceback"));
                    return;
                }
            };
            let class = match modd.getattr("AgentCheck") {
                Ok(c) => c,
                Err(e) => {
                    let traceback = e
                        .traceback_bound(py)
                        .expect("Traceback should be present on this error");
                    error!(%e, "Could not get AgentCheck class. traceback: {}", traceback.format().expect("Could format traceback"));
                    return;
                }
            };
            info!(%class, %modd, "Was able to import AgentCheck!");
        });

        Self {
            running: HashMap::new(),
        }
    }

    // compiles the python source code and instantiates it (??) into the VM
    // returns an opaque handle to the check
    fn register_check(&mut self, check_source_path: PathBuf) -> Result<CheckHandle, GenericError> {
        let py_source = std::fs::read_to_string(&check_source_path).unwrap();

        pyo3::Python::with_gil(|py| {
            let locals = pyo3::types::PyDict::new_bound(py);

            match py.run_bound(&py_source, None, Some(&locals)) {
                Ok(c) => {}
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
                .filter(|(key, value)| {
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
            Ok(CheckHandle {
                id: check_value.as_unbound().clone(),
            })
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
            let handle = tokio::task::spawn_local(async move {
                let mut interval =
                    tokio::time::interval(Duration::from_millis(instance.min_collection_interval_ms.into()));
                loop {
                    interval.tick().await;
                    // run check
                    info!("Running check instance {idx}");
                    pyo3::Python::with_gil(|py| {
                        let instance_as_pydict = PyDict::new_bound(py);
                        instance_as_pydict.set_item("min_collection_interval_ms", instance.min_collection_interval_ms);

                        let instance_list = PyList::new_bound(py, &[instance_as_pydict]);
                        let kwargs = PyDict::new_bound(py);
                        kwargs.set_item("name", "placeholder_check_name");
                        kwargs.set_item("init_config", PyDict::new_bound(py)); // todo this is in the check request maybe
                        kwargs.set_item("instances", instance_list);

                        let check_ref = &check_handle.id;
                        let pycheck = match check_ref.call_bound(py, (), Some(&kwargs)) {
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
                        info!("Result: {:?}", result);
                    })
                }
            });
            running_entry.1.push(handle);
        }

        Ok(())
    }

    pub fn stop_check(&self, check: CheckRequest) {
        info!("Deleting check request {check}");
        if let Some((check_handle, running)) = self.running.get(&check.source) {
            for handle in running.iter() {
                handle.abort();
            }
        }
        // todo delete stuff out of the containers
        // and stop/release the class if its not being used
    }
}
