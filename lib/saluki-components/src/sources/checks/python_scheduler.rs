use pyo3::prelude::PyAnyMethods;
use pyo3::types::PyDict;
use pyo3::types::PyList;
use pyo3::types::PyString;
use pyo3::types::PyType;
use saluki_error::{generic_error, GenericError};
use tracing::trace;

use super::python_exposed_modules::aggregator as pyagg;
use super::python_exposed_modules::datadog_agent;
use super::*;

struct CheckHandle(Py<PyAny>);

impl Clone for CheckHandle {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[pyclass]
pub struct PythonSenderHolder {
    pub sender: mpsc::Sender<CheckMetric>,
}

pub struct PythonCheckScheduler {
    tlm: ChecksTelemetry,
    running: HashMap<CheckSource, (CheckHandle, Vec<tokio::task::JoinHandle<()>>)>,
    agent_check_base_class: Py<PyAny>,
}

impl PythonCheckScheduler {
    pub fn new(send_check_metrics: mpsc::Sender<CheckMetric>, tlm: ChecksTelemetry) -> Result<Self, GenericError> {
        // TODO future improvement to hook into py allocators to track memory usage
        // ref https://docs.rs/pyembed/latest/src/pyembed/pyalloc.rs.html#605-609
        pyo3::append_to_inittab!(datadog_agent);
        pyo3::append_to_inittab!(pyagg);

        pyo3::prepare_freethreaded_python();

        let mut agent_check_base_class = None;

        pyo3::Python::with_gil(|py: Python<'_>| -> Result<(), GenericError> {
            let syspath: Bound<PyList> = py.import_bound("sys")?.getattr("path")?.extract()?;
            // Mimicing the python path setup from the Agent
            // https://github.com/DataDog/datadog-agent/blob/b039ea43d3168f521e8ea3e8356a0e84eec170d1/cmd/agent/common/common.go#L24-L33
            syspath.insert(0, Path::new("./dist/"))?; // path.GetDistPath()
            syspath.insert(0, Path::new("./dist/checks.d/"))?; // custom checks in checks.d subdir
                                                               // syspath.insert(0, config.get('additional_checksd'))?; // config option not supported yet
            debug!("Python sys.path is: {:?}", syspath.to_string());

            // Initialize the aggregator module with the submission queue
            match py.import_bound("aggregator") {
                Ok(m) => {
                    let sender_holder = Bound::new(
                        py,
                        PythonSenderHolder {
                            sender: send_check_metrics.clone(),
                        },
                    )
                    .expect("Could not create sender holder");
                    m.setattr("SUBMISSION_QUEUE", sender_holder)
                        .expect("Could not set sender_holder on module attribute");
                    Ok(())
                }
                Err(e) => {
                    error!(%e, "Could not import aggregator module.");
                    if let Some(traceback) = e.traceback_bound(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    Err(generic_error!("Could not import 'aggregator' module"))
                }
            }?;

            // Validate that python env is correctly configured
            let modd = match py.import_bound("datadog_checks.checks") {
                Ok(m) => m,
                Err(e) => {
                    error!(%e, "Could not import datadog_checks module");
                    if let Some(traceback) = e.traceback_bound(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return Err(generic_error!("Could not import 'datadog_checks' module"));
                }
            };
            match modd.getattr("AgentCheck") {
                Ok(c) => {
                    agent_check_base_class = Some(c.unbind());
                }
                Err(e) => {
                    error!(%e, "Could not get AgentCheck class.");
                    if let Some(traceback) = e.traceback_bound(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return Err(generic_error!("Could not find 'AgentCheck' class"));
                }
            };
            info!("Found all pre-requisites for Agent python check execution");

            Ok(())
        })?;

        Ok(Self {
            tlm,
            running: HashMap::new(),
            agent_check_base_class: agent_check_base_class.expect("AgentCheck class should be present"),
        })
    }

    // See `CheckRequest::to_runnable_request` for more TODO items here
    // Returns an opaque handle to the python class implementing the check
    fn register_check(&mut self, check: &RunnableCheckRequest) -> Result<CheckHandle, GenericError> {
        let check_handle = match self.register_check_impl(check) {
            Ok(h) => h,
            Err(e) => {
                error!(%e, "Could not register check {}", check.check_request.name);
                return Err(e);
            }
        };
        // TODO What 'init' is needed after grabbing a handle to the check class?
        // See `rtloader/three/three.cpp::getCheck` for calls:
        // - `AgentCheck.load_config(init_config)`
        // - `AgentCheck.load_config(instance)`

        // JK load_config is just yaml parsing -- str -> pyAny
        // I assume my impl of CheckRequest::to_pydict makes this un-neccessary
        // but TBD, could be some subtle differences between it and 'load_config'

        // - set attr 'check_id' equal to the check id
        // also parse out the attr `__version__` and record it somewhere
        // ref PythonCheckLoader::Load in Agent

        Ok(check_handle)
    }

    fn possible_import_paths_for_module(&self, module_name: &str) -> Vec<String> {
        vec![module_name.to_string(), format!("datadog_checks.{}", module_name)]
    }

    fn register_check_impl(&mut self, check: &RunnableCheckRequest) -> Result<CheckHandle, GenericError> {
        let check_module_name = &check.check_request.name;
        // if there is a specific source, then this will populate into locals and can be found
        if let Some(py_source) = &check.check_source_code {
            return self.register_check_with_source(py_source.clone());
        }
        let mut load_errors = vec![];
        for import_str in self.possible_import_paths_for_module(check_module_name).iter() {
            match self.register_check_from_imports(import_str) {
                Ok(handle) => return Ok(handle),
                Err(e) => {
                    // Not an error yet, wait until all imports have been tried
                    // Grab the underlying error, not the anyhow wrapper
                    load_errors.push(e.root_cause().to_string());
                }
            }
        }

        Err(generic_error!(
            "Could not find class for check {check_module_name}. Errors: {load_errors:?}"
        ))
    }

    fn register_check_from_imports(&self, import_path: &str) -> Result<CheckHandle, GenericError> {
        pyo3::Python::with_gil(|py| {
            let module = py.import_bound(import_path)?;
            let base_class = self.agent_check_base_class.bind(py);

            let checks = module
                .dict()
                .iter()
                .filter(|(name, value)| {
                    trace!(
                        "Found {name}: {value}, with type {t}",
                        t = value.get_type().name().unwrap_or(PyString::new_bound(py, "unknown"))
                    );
                    let class_bound: &Bound<PyType> = match value.downcast() {
                        Ok(c) => c,
                        Err(_) => return false,
                    };
                    trace!("Found a class: {class_bound}");
                    if class_bound.is(base_class) {
                        // skip the base class
                        return false;
                    }
                    if let Ok(true) = class_bound.is_subclass(base_class) {
                        return true;
                    }
                    false
                })
                .collect::<Vec<_>>();
            if checks.is_empty() {
                return Err(generic_error!("No checks found in source"));
            }
            if checks.len() >= 2 {
                return Err(generic_error!("Multiple checks found in source"));
            }
            let (check_key, check_value) = &checks[0];
            info!("Found base class {check_key} for check {import_path} {check_value:?}");
            Ok(CheckHandle(check_value.as_unbound().clone()))
        })
    }

    fn register_check_with_source(&mut self, py_source: String) -> Result<CheckHandle, GenericError> {
        pyo3::Python::with_gil(|py| {
            debug!("Running provided check source and checking locals for subclasses of 'AgentCheck'");
            let locals = pyo3::types::PyDict::new_bound(py);

            match py.run_bound(&py_source, None, Some(&locals)) {
                Ok(_) => {}
                Err(e) => {
                    error!(%e, "Could not compile check source");
                    if let Some(traceback) = e.traceback_bound(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return Err(generic_error!("Could not compile check source"));
                }
            };
            let base_class = self.agent_check_base_class.bind(py);
            let checks = locals
                .iter()
                .filter(|(_, value)| {
                    if let Ok(class_obj) = value.downcast::<PyType>() {
                        if class_obj.is(base_class) {
                            // skip the base class
                            return false;
                        }
                        if let Ok(true) = class_obj.is_subclass(base_class) {
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
            info!("Found check {check_key} from source: {py_source}");
            Ok(CheckHandle(check_value.as_unbound().clone()))
        })
    }
}
impl CheckScheduler for PythonCheckScheduler {
    fn can_run_check(&self, check: &RunnableCheckRequest) -> bool {
        // If we have explicit source code provided, we can definitely run it
        if check.check_source_code.is_some() {
            debug!("Check has explicit source code, can definitely run it");
            return true;
        }

        // Try to find the check in possible import paths
        let module_paths = self.possible_import_paths_for_module(check.check_request.name.as_str());
        debug!(
            "Checking if module '{}' can be run, found {} possible paths",
            check.check_request.name,
            module_paths.len()
        );

        if module_paths.is_empty() {
            warn!("No import paths found for check: {}", check.check_request.name);
            return false;
        }

        for module_name in module_paths {
            debug!("Attempting to register check from import path: {}", module_name);
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.register_check_from_imports(&module_name)
            })) {
                Ok(handle_result) => {
                    let can_run = handle_result.is_ok();
                    info!(
                        "Checking if py module '{name}' can be run. Result: {}",
                        can_run,
                        name = module_name,
                    );
                    if can_run {
                        return true;
                    }
                }
                Err(e) => {
                    error!("Python panic during module check for {}: {:?}", module_name, e);
                    // Continue trying other modules
                }
            }
        }

        false
    }

    // This function does 3 things
    // 1. Registers the check source code and gets a handle
    // 2. Starts a local task for each instance that
    //    queues a run of the check every min_collection_interval_ms
    // 3. Stores the handles in the running hashmap
    fn run_check(&mut self, check: &RunnableCheckRequest) -> Result<(), GenericError> {
        let check_handle = self.register_check(check)?;
        let running_entry = self
            .running
            .entry(check.check_request.source.clone())
            .or_insert((check_handle.clone(), Vec::new()));

        info!(
            "Running check {name} with {num_instances} instances",
            name = check.check_request.name,
            num_instances = check.check_request.instances.len()
        );
        for (idx, instance) in check.check_request.instances.iter().enumerate() {
            let instance = instance.clone();
            let init_config = check.check_request.init_config.clone();

            let check_handle = check_handle.clone();
            trace!("Spawning task for check instance {idx}");
            self.tlm.check_instances_started.increment(1);
            let handle = tokio::task::spawn(async move {
                let mut interval =
                    tokio::time::interval(Duration::from_millis(instance.min_collection_interval_ms().into()));
                loop {
                    interval.tick().await;
                    // run check
                    info!("Running check instance {idx}");
                    pyo3::Python::with_gil(|py| {
                        let instance_as_pydict = instance.to_pydict(&py);

                        let instance_list = PyList::new_bound(py, &[instance_as_pydict]);
                        let kwargs = PyDict::new_bound(py);
                        kwargs
                            .set_item("name", "placeholder_check_name")
                            .expect("Could not set name");
                        kwargs
                            .set_item("init_config", init_config.to_pydict(&py))
                            .expect("could not set init_config");
                        kwargs
                            .set_item("instances", instance_list)
                            .expect("could not set instance list");

                        // TODO should this be moved up outside of the loop?
                        // I kind of think so, this makes a new check instance every tick
                        let pycheck = match check_handle.0.call_bound(py, (), Some(&kwargs)) {
                            Ok(c) => c,
                            Err(e) => {
                                error!(%e, "Could not instantiate check.");
                                if let Some(traceback) = e.traceback_bound(py) {
                                    error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                                }
                                return;
                            }
                        };
                        // 'run' method invokes 'check' with the instance we initialized with
                        // ref https://github.com/DataDog/integrations-core/blob/bc3b1c3496e79aa1b75ebcc9ef1c2a2b26487ebd/datadog_checks_base/datadog_checks/base/checks/base.py#L1197
                        let result = pycheck.call_method0(py, "run").unwrap();

                        let s: String = result
                            .extract(py)
                            .expect("Can't read the string result from the check execution");

                        trace!("Check execution returned {:?}", s);
                    })
                }
            });
            running_entry.1.push(handle);
        }

        Ok(())
    }

    fn stop_check(&mut self, check: CheckRequest) {
        info!("Deleting check request {check}");
        if let Some((check_handle, running)) = self.running.remove(&check.source) {
            for handle in running.iter() {
                handle.abort();
            }
            drop(check_handle); // release the reference to this check
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::super::*;
    use super::*;

    #[tokio::test]
    async fn test_new_check_scheduler() {
        let (sender, _) = mpsc::channel(10);
        let scheduler = PythonCheckScheduler::new(sender, ChecksTelemetry::noop()).unwrap();
        assert!(scheduler.running.is_empty());
    }

    #[tokio::test]
    async fn test_register_check_with_source() {
        let (sender, _) = mpsc::channel(10);
        let mut scheduler = PythonCheckScheduler::new(sender, ChecksTelemetry::noop()).unwrap();
        let py_source = r#"
from datadog_checks.checks import AgentCheck

class MyCheck(AgentCheck):
    def check(self, instance):
        pass
        "#;
        let check_handle = scheduler.register_check_with_source(py_source.to_string()).unwrap();
        pyo3::Python::with_gil(|py| {
            let check_class_ref = check_handle.0.bind(py);
            assert!(check_class_ref.is_callable());
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_check() {
        let (sender, mut receiver) = mpsc::channel(10);
        let mut scheduler = PythonCheckScheduler::new(sender, ChecksTelemetry::noop()).unwrap();
        let py_source = r#"
from datadog_checks.checks import AgentCheck

class MyCheck(AgentCheck):
    def check(self, instance):
        self.gauge('test-metric-name', 41, tags=['hello:world'])"#;

        let source = CheckSource::Yaml(YamlCheck::new(
            "my_check",
            "instances: [{}]",
            Some(PathBuf::from("/tmp/my_check.yaml")),
        ));
        let check_request = source.to_check_request().unwrap();

        let runnable_check_request = RunnableCheckRequest {
            check_source_code: Some(py_source.to_string()),
            check_request,
        };

        scheduler.run_check(&runnable_check_request).unwrap();
        assert_eq!(scheduler.running.len(), 1);
        assert_eq!(scheduler.running.keys().next(), Some(&source));

        let check_metric = CheckMetric {
            name: "test-metric-name".to_string(),
            metric_type: PyMetricType::Gauge,
            value: 41.0,
            tags: vec!["hello:world".to_string()],
        };
        let check_from_channel = receiver.recv().await.unwrap();
        assert_eq!(check_from_channel, check_metric);
    }

    #[tokio::test]
    async fn test_stop_check() {
        let (sender, mut receiver) = mpsc::channel(10);
        let mut scheduler = PythonCheckScheduler::new(sender, ChecksTelemetry::noop()).unwrap();
        let py_source = r#"
from datadog_checks.checks import AgentCheck

class MyCheck(AgentCheck):
    def check(self, instance):
        self.gauge('test-metric-name', 41, tags=['hello:world'])"#;

        let source = CheckSource::Yaml(YamlCheck::new(
            "my_check",
            "instances: [{}]",
            Some(PathBuf::from("/tmp/my_check.yaml")),
        ));
        let check_request = source.to_check_request().unwrap();

        let runnable_check_request = RunnableCheckRequest {
            check_source_code: Some(py_source.to_string()),
            check_request: check_request.clone(),
        };

        scheduler.run_check(&runnable_check_request).unwrap();
        assert_eq!(scheduler.running.len(), 1);
        assert_eq!(scheduler.running.keys().next(), Some(&source));

        scheduler.stop_check(check_request);
        assert!(scheduler.running.is_empty());

        receiver
            .try_recv()
            .expect_err("No check metrics should be received after stopping the check");
    }
}
