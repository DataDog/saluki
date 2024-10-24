use pyo3::prelude::PyAnyMethods;
use pyo3::types::PyDict;
use pyo3::types::PyList;
use pyo3::types::PyType;
use saluki_error::{generic_error, GenericError};
use serde_json::json;
use tracing::trace;

use super::python_exposed_modules::aggregator as pyagg;
use super::python_exposed_modules::datadog_agent;
use super::*;

struct PyCheckClassHandle(Py<PyAny>);

impl Clone for PyCheckClassHandle {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

pub enum RunnableDecision {
    CanRun,
    CannotRun(String),
}

#[pyclass]
pub struct PythonSenderHolder {
    pub sender: mpsc::Sender<CheckMetric>,
}

struct RunningPythonInstance {
    _check_handle: Py<PyAny>,
    join_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Serialize, Default)]
struct StatusDetail {
    // This maps from a given check request (TODO: Conflicts between check names are probable, so this should key off of check_id)
    check_request_failures: HashMap<String, serde_json::Value>,

    running_checks: HashMap<String, Vec<String>>,
}

pub struct PythonCheckScheduler {
    tlm: ChecksTelemetry,
    health: Health,
    // Contains human-readable debugging information
    status_detail: StatusDetail,
    // A CheckSource will either appear in 'running'
    running: HashMap<CheckRequest, (PyCheckClassHandle, Vec<RunningPythonInstance>)>,
    agent_check_base_class: Py<PyAny>,
    manually_registered_checks: HashMap<String, PyCheckClassHandle>,
}

impl PythonCheckScheduler {
    pub fn new(
        send_check_metrics: mpsc::Sender<CheckMetric>, tlm: ChecksTelemetry, health: Health,
    ) -> Result<Self, GenericError> {
        // TODO future improvement to hook into py allocators to track memory usage
        // ref https://docs.rs/pyembed/latest/src/pyembed/pyalloc.rs.html#605-609
        pyo3::append_to_inittab!(datadog_agent);
        pyo3::append_to_inittab!(pyagg);

        pyo3::prepare_freethreaded_python();

        let mut agent_check_base_class = None;

        pyo3::Python::with_gil(|py| -> Result<(), GenericError> {
            let syspath: &PyList = py.import_bound("sys")?.getattr("path")?.extract()?;
            // Mimicing the python path setup from the Agent
            // https://github.com/DataDog/datadog-agent/blob/b039ea43d3168f521e8ea3e8356a0e84eec170d1/cmd/agent/common/common.go#L24-L33
            syspath.insert(0, Path::new("./dist/"))?; // path.GetDistPath()
            syspath.insert(0, Path::new("./dist/checks.d/"))?; // custom checks in checks.d subdir
                                                               // syspath.insert(0, config.get('additional_checksd'))?; // config option not supported yet
            info!("Python sys.path is: {:?}", syspath);

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
                        .expect("Could not set sender_holder on module attribute")
                }
                Err(e) => {
                    error!(%e, "Could not import aggregator module.");
                    if let Some(traceback) = e.traceback_bound(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return Err(generic_error!("Could not import 'aggregator' module"));
                }
            };

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
            health,
            status_detail: StatusDetail::default(),
            running: HashMap::new(),
            manually_registered_checks: HashMap::new(),
            agent_check_base_class: agent_check_base_class.expect("AgentCheck class should be present"),
        })
    }

    // Returns an opaque handle to the python class implementing the check
    fn find_agentcheck(&mut self, check: &CheckRequest) -> Result<PyCheckClassHandle, GenericError> {
        // Search manually registered checks first
        if let Some(h) = self.manually_registered_checks.get(&check.name) {
            return Ok(h.clone());
        }
        // Then attempt to find from imports
        let check_handle = match self.find_agentcheck_import(check) {
            Ok(h) => h,
            Err(e) => {
                error!(%e, "Could not register check {}", check.name);
                return Err(e);
            }
        };

        Ok(check_handle)
    }

    fn possible_import_paths_for_module(&self, module_name: &str) -> Vec<String> {
        vec![module_name.to_string(), format!("datadog_checks.{}", module_name)]
    }

    /// Given a check request, find a corresponding AgentCheck subclass from imports and return it
    fn find_agentcheck_import(&self, check: &CheckRequest) -> Result<PyCheckClassHandle, GenericError> {
        let check_module_name = &check.name;
        let mut load_errors = vec![];
        for import_str in self.possible_import_paths_for_module(check_module_name).iter() {
            match self.find_agentcheck_from_import_path(import_str) {
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

    /// Attempt to import the module and find a subclass of `AgentCheck`
    /// If the module is found, and a subclass of `AgentCheck` is found, return a handle to the class
    ///
    /// Returns: A handle to the class that is a subclass of `AgentCheck`
    fn find_agentcheck_from_import_path(&self, import_path: &str) -> Result<PyCheckClassHandle, GenericError> {
        pyo3::Python::with_gil(|py| {
            let module = py.import_bound(import_path)?;
            let base_class = self.agent_check_base_class.bind(py);

            let checks = module
                .dict()
                .iter()
                .filter(|(name, value)| {
                    trace!(
                        "Found {name}: {value}, with type {t}",
                        t = value.get_type().name().unwrap_or(std::borrow::Cow::Borrowed("unknown"))
                    );
                    let class_bound: &Bound<PyType> = match value.downcast() {
                        Ok(c) => c,
                        Err(_) => return false,
                    };
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
            debug!("Found base class {check_key} for check {import_path} {check_value:?}");
            Ok(PyCheckClassHandle(check_value.as_unbound().clone()))
        })
    }

    /// Local Check Source Handling
    /// The following two functions are used to handle checks that are provided as source code
    /// This is currently only used for testing (hence the '_' prefix), but could be used to support custom checks

    /// Given a check source code, find a subclass of `AgentCheck` in the local scope
    /// Currently only used in tests
    fn _find_agentcheck_local(&mut self, py_source: String) -> Result<PyCheckClassHandle, GenericError> {
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
            Ok(PyCheckClassHandle(check_value.as_unbound().clone()))
        })
    }

    /// When a check source code is found, it can be registered with the scheduler
    /// to ensure check requests for the given 'module_name' will use the provided 'source'
    /// Currently only used in tests, but is useful for supporting custom checks
    /// TODO: Support loading check sources out of `checks.d` directory
    fn _register_check_module(&mut self, module_name: &str, source: &str) -> Result<(), GenericError> {
        let check_handle = self._find_agentcheck_local(source.to_string())?;

        match self
            .manually_registered_checks
            .insert(module_name.to_string(), check_handle)
        {
            Some(_) => Err(generic_error!("Check already registered")),
            None => Ok(()),
        }
    }
}

impl CheckScheduler for PythonCheckScheduler {
    fn can_run_check(&mut self, check: &CheckRequest) -> RunnableDecision {
        let ret = match self.find_agentcheck(check) {
            Ok(_) => RunnableDecision::CanRun,
            Err(e) => {
                self.status_detail.check_request_failures.insert(
                    check.name.clone(),
                    json!({
                        "error": e.to_string(),
                    }),
                );
                RunnableDecision::CannotRun(format!("Could not find base class for check '{}'", check.name))
            }
        };

        self.health
            .set_status_detail(serde_json::to_value(&self.status_detail).expect("Can serialize"));
        ret
    }

    /// This function does 3 things
    /// 1. Registers the check source code and gets a handle
    /// 2. Starts a local task for each instance that
    ///    queues a run of the check every min_collection_interval_ms
    /// 3. Stores the handles in the running hashmap
    ///
    /// Known Gaps:
    /// - `__version__` not set correctyl. ref PythonCheckLoader::Load in Agent
    fn run_check(&mut self, check: &CheckRequest) -> Result<(), GenericError> {
        let check_py_class = self.find_agentcheck(check)?;
        let running_entry = self
            .running
            .entry(check.clone())
            .or_insert((check_py_class.clone(), Vec::new()));

        info!(
            "Starting up check '{name}' with {num_instances} instances",
            name = check.name,
            num_instances = check.instances.len()
        );
        for instance in check.instances.iter() {
            let instance = instance.clone();
            let init_config = check.init_config.clone();

            let check_py_class = check_py_class.clone();
            // create check
            let pycheck_instance = pyo3::Python::with_gil(|py| -> Result<pyo3::Py<pyo3::PyAny>, GenericError> {
                // In rtloader, 'instance_as_pydict' is created by invoking
                //  `AgentCheck.load_config(instance)`
                // However, this is un-necessary as pyo3 provides an api to directly convert
                // 'instance' (yaml) to a pydict.
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

                let check_id = build_check_id(&check.name, 0, &instance, &init_config);

                match check_py_class.0.call_bound(py, (), Some(&kwargs)) {
                    Ok(c) => {
                        // If check is successfully created, then set check_id
                        if let Err(e) = c.setattr(py, "check_id", &check_id) {
                            error!(%e, "Could not set check_id on instance of check '{name}'", name = check.name);
                        } else {
                            info!(
                                "Set check_id to '{check_id}' on instance of check '{name}'",
                                check_id = check_id,
                                name = check.name
                            );
                        }

                        Ok(c)
                    }
                    Err(e) => {
                        // TODO: This represents an initialization error for one of the instances
                        // requested. It should be tracked and made visible to the user.
                        error!(%e, "Instantiating '{name}' failed.", name = check.name);
                        if let Some(traceback) = e.traceback_bound(py) {
                            error!("{}", traceback.format().expect("Could format traceback"));
                        }
                        Err(e.into())
                    }
                }
            })?;

            self.tlm.check_instances_started.increment(1);
            let cloned_pycheck_instance = pycheck_instance.clone();
            let join_handle = tokio::task::spawn(async move {
                let mut interval =
                    tokio::time::interval(Duration::from_millis(instance.min_collection_interval_ms().into()));
                loop {
                    interval.tick().await;
                    // create check
                    pyo3::Python::with_gil(|py| {
                        // 'run' method invokes 'check' with the instance we initialized with
                        // ref https://github.com/DataDog/integrations-core/blob/bc3b1c3496e79aa1b75ebcc9ef1c2a2b26487ebd/datadog_checks_base/datadog_checks/base/checks/base.py#L1197
                        let result = cloned_pycheck_instance
                            .call_method_bound(py, "run", (), None)
                            .expect("Can run check");

                        let s: String = result
                            .extract(py)
                            .expect("Can't read the string result from the check execution");

                        trace!("Check execution returned {:?}", s);
                    })
                }
            });
            let running_instance = RunningPythonInstance {
                _check_handle: pycheck_instance,
                join_handle,
            };
            running_entry.1.push(running_instance);
        }

        Ok(())
    }

    fn stop_check(&mut self, check: CheckRequest) {
        info!("Deleting check request {check}");
        if let Some((check_handle, running)) = self.running.remove(&check) {
            for current in running.iter() {
                // does the running check need to be torn down?
                // maybe current.check_handle.call_method("stop", (), None).expect("Can stop check");
                current.join_handle.abort();
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
        let scheduler =
            PythonCheckScheduler::new(sender, ChecksTelemetry::noop(), saluki_health::Health::noop()).unwrap();
        assert!(scheduler.running.is_empty());
    }

    #[tokio::test]
    async fn test_register_check_with_source() {
        let (sender, _) = mpsc::channel(10);
        let mut scheduler =
            PythonCheckScheduler::new(sender, ChecksTelemetry::noop(), saluki_health::Health::noop()).unwrap();
        let py_source = r#"
from datadog_checks.checks import AgentCheck

class MyCheck(AgentCheck):
    def check(self, instance):
        pass
        "#;
        let check_handle = scheduler._find_agentcheck_local(py_source.to_string()).unwrap();
        pyo3::Python::with_gil(|py| {
            let check_class_ref = check_handle.0.bind(py);
            assert!(check_class_ref.is_callable());
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_check() {
        let (sender, mut receiver) = mpsc::channel(10);
        let mut scheduler =
            PythonCheckScheduler::new(sender, ChecksTelemetry::noop(), saluki_health::Health::noop()).unwrap();
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

        scheduler._register_check_module("my_check", py_source).unwrap();

        scheduler.run_check(&check_request).unwrap();
        assert_eq!(scheduler.running.len(), 1);
        assert_eq!(scheduler.running.keys().next(), Some(&check_request));

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
        let mut scheduler =
            PythonCheckScheduler::new(sender, ChecksTelemetry::noop(), saluki_health::Health::noop()).unwrap();
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
        scheduler._register_check_module("my_check", py_source).unwrap();

        scheduler.run_check(&check_request).unwrap();
        assert_eq!(scheduler.running.len(), 1);
        assert_eq!(scheduler.running.keys().next(), Some(&check_request));

        scheduler.stop_check(check_request);
        assert!(scheduler.running.is_empty());

        receiver
            .try_recv()
            .expect_err("No check metrics should be received after stopping the check");
    }
}
