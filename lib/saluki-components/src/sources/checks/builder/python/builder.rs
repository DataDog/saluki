//! A Python implementation of CheckBuilder

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use pyo3::types::{PyDict, PyList, PyNone, PyTuple, PyType};
use pyo3::PyObject;
use pyo3::{prelude::*, IntoPyObjectExt};
use saluki_env::autodiscovery::{Data, Instance, RawData};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use stringtheory::MetaString;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, warn};

use super::python_modules::aggregator as pyagg;
use super::python_modules::datadog_agent;
use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;
use crate::sources::checks::check_metric::CheckMetric;

struct PythonCheck {
    version: String,
    interval: Duration,
    instance: PyObject,
    source: String,
    id: String,
}

impl Check for PythonCheck {
    fn run(&self) -> Result<(), GenericError> {
        let result = pyo3::Python::with_gil(|py| self.instance.call_method(py, "run", (), None));
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(generic_error!(e)),
        }
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn source(&self) -> &str {
        &self.source
    }
}

static PYTHON_ENV_BUILDER: OnceLock<PythonEnvBuilder> = OnceLock::new();

struct PythonEnvBuilder {
    check_events_tx: Sender<CheckMetric>,
    agent_check_class: Option<PyObject>,
    custom_checks_folders: Option<Vec<String>>,
}
impl PythonEnvBuilder {
    pub fn new(check_events_tx: Sender<CheckMetric>, custom_checks_folders: Option<Vec<String>>) -> Self {
        Self {
            check_events_tx,
            custom_checks_folders,
            agent_check_class: None,
        }
    }

    fn get_instance(&self) -> &'static PythonEnvBuilder {
        PYTHON_ENV_BUILDER.get_or_init(|| self.initialize())
    }

    fn initialized(&self) -> bool {
        self.agent_check_class.is_some()
    }

    fn initialize(&self) -> Self {
        pyo3::append_to_inittab!(datadog_agent);
        pyo3::append_to_inittab!(pyagg);
        pyo3::prepare_freethreaded_python();

        let result = Python::with_gil(|py| -> Result<PyObject, GenericError> {
            let sys_path_attr = py
                .import("sys")
                .error_context("Could not import sys module.")
                .and_then(|sys| sys.getattr("path").error_context("Could not get 'sys.path' attribute."))?;
            let sys_path = sys_path_attr
                .downcast::<PyList>()
                .map_err(|_| GenericError::msg("Could not downcast 'sys.path' to list."))?;
            // Add additional paths to sys.path to support loading checks.
            let dist_path = Path::new("./dist");
            let checks_d_path = dist_path.join("checks.d");
            sys_path.insert(0, dist_path.to_string_lossy().as_ref()).unwrap(); // common modules are shipped in the dist path directly or under the "checks/" sub-dir
            sys_path.insert(0, checks_d_path.to_string_lossy().as_ref()).unwrap(); // integrations-core legacy checks
            if let Some(custom_checks_folders) = &self.custom_checks_folders {
                for folder in custom_checks_folders {
                    let path = Path::new(&folder);
                    sys_path.insert(0, path.to_string_lossy().as_ref()).unwrap();
                }
            }
            sys_path.insert(0, "/etc/datadog-agent/checks.d/").unwrap(); // Agent checks folder

            debug!("Updated Python system path (sys.path) to {:?}.", sys_path);
            // Import the Datadog Checks module, ensuring it loads correctly, and grab a reference to the base AgentCheck class.
            let dd_checks_module = match py.import("datadog_checks.checks") {
                Ok(m) => m,
                Err(e) => {
                    if let Some(traceback) = e.traceback(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return Err(generic_error!("Could not import datadog_checks module {:?}", e));
                }
            };
            dd_checks_module
                .getattr("AgentCheck")
                .map(|c| c.unbind())
                .error_context("Could not get AgentCheck class from datadog_checks module.")
        });
        match result {
            Ok(agent_check_class) => {
                // Initialize global state for our Python modules.
                super::python_modules::set_metric_sender(self.check_events_tx.clone());

                info!("Python runtime loaded successfully and initialized for checks.");
                Self {
                    agent_check_class: Some(agent_check_class),
                    check_events_tx: self.check_events_tx.clone(),
                    custom_checks_folders: self.custom_checks_folders.clone(),
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to load/initialize Python runtime for checks. Python checks will be unavailable.");
                Self {
                    agent_check_class: None,
                    check_events_tx: self.check_events_tx.clone(),
                    custom_checks_folders: self.custom_checks_folders.clone(),
                }
            }
        }
    }
}

pub struct PythonCheckBuilder {
    env_builder: PythonEnvBuilder,
}

impl PythonCheckBuilder {
    pub fn new(check_events_tx: Sender<CheckMetric>, custom_checks_folders: Option<Vec<String>>) -> Self {
        let env_builder = PythonEnvBuilder::new(check_events_tx, custom_checks_folders);
        Self { env_builder }
    }
}

impl CheckBuilder for PythonCheckBuilder {
    fn build_check(
        &self, name: &str, instance: &Instance, init_config: &Data, source: &MetaString,
    ) -> Option<Arc<dyn Check + Send + Sync>> {
        let env = self.env_builder.get_instance();
        if !env.initialized() {
            return None;
        }
        let agent_check_class = env
            .agent_check_class
            .as_ref()
            .expect("agent check class should have value.");
        let mut load_errors = vec![];
        for import_path in [name.to_string(), format!("datadog_checks.{}", name)].iter() {
            match self.register_check_from_imports(agent_check_class, name, import_path, instance, init_config, source)
            {
                Ok(handle) => return Some(handle),
                Err(e) => {
                    load_errors.push(e.root_cause().to_string());
                }
            }
        }
        error!(check_name = name, "Could not load check: {:?}", load_errors);
        None
    }
}

impl PythonCheckBuilder {
    fn register_check_from_imports(
        &self, agent_check_class: &PyObject, name: &str, import_path: &str, instance: &Instance, init_config: &Data,
        source: &MetaString,
    ) -> Result<Arc<dyn Check + Send + Sync>, GenericError> {
        pyo3::Python::with_gil(|py| {
            let module = py.import(import_path)?;

            let check_class = agent_check_class.bind(py);

            let checks = module
                .dict()
                .iter()
                .filter(|(_name, value)| {
                    let class_bound: &Bound<PyType> = match value.downcast() {
                        Ok(c) => c,
                        Err(_) => return false,
                    };
                    if class_bound.is(check_class) {
                        // skip the base class
                        return false;
                    }
                    if let Ok(true) = class_bound.is_subclass(check_class) {
                        return true;
                    }
                    false
                })
                .collect::<Vec<_>>();
            if checks.is_empty() {
                return Err(generic_error!("No checks class found in source."));
            }
            if checks.len() >= 2 {
                return Err(generic_error!("Multiple checks classes found in source."));
            }
            let (_check_key, check_value) = &checks[0];
            debug!(path = import_path, "Found base class for check.");
            let mut version = "unversioned".to_string();
            if let Ok(check_version) = check_value.getattr("__version__") {
                if let Ok(v) = check_version.extract::<String>() {
                    version = v;
                }
            }

            let min_interval = if let Some(value) = instance.get("min_collection_interval") {
                value.as_u64().unwrap_or_default()
            } else {
                0
            };

            let kwargs = PyDict::new(py);
            let parsed_config = map_to_pydict(init_config.get_value(), &py)?;

            let parse_instance = map_to_pydict(instance.get_value(), &py)?;

            let py_vec = PyTuple::new(py, vec![parse_instance])?;

            kwargs.set_item("name", name)?;
            kwargs.set_item("init_config", parsed_config)?;
            kwargs.set_item("instances", py_vec)?;

            let check_instance = check_value.call((), Some(&kwargs))?;

            check_instance.setattr("check_id", instance.id())?;

            let check = Arc::new(PythonCheck {
                version,
                interval: Duration::from_secs(min_interval),
                instance: check_instance.clone().unbind(),
                source: source.to_string(),
                id: instance.id().clone(),
            }) as Arc<dyn Check + Send + Sync>;
            Ok(check)
        })
    }
}

fn map_to_pydict<'py>(
    map: &HashMap<MetaString, serde_yaml::Value>, p: &'py pyo3::Python,
) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
    let dict = PyDict::new(*p);
    for (key, value) in map {
        let value = serde_value_to_pytype(value, p)?;
        dict.set_item(key.to_string(), value)?;
    }
    Ok(dict)
}

fn serde_value_to_pytype<'py>(value: &serde_yaml::Value, p: &'py pyo3::Python) -> PyResult<Bound<'py, pyo3::PyAny>> {
    match value {
        serde_yaml::Value::String(s) => Ok(s.into_bound_py_any(*p)?),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_bound_py_any(*p)?)
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_bound_py_any(*p)?)
            } else {
                unreachable!("Number is neither i64 nor f64")
            }
        }
        serde_yaml::Value::Bool(b) => Ok(b.into_bound_py_any(*p)?),
        serde_yaml::Value::Sequence(s) => {
            let list = PyList::empty(*p);
            for item in s {
                let item = serde_value_to_pytype(item, p)?;
                list.append(item)?;
            }
            Ok(list.into_any())
        }
        serde_yaml::Value::Mapping(m) => {
            let dict = PyDict::new(*p);
            for (key, value) in m {
                let value = serde_value_to_pytype(value, p)?;
                let key = serde_value_to_pytype(key, p)?;
                dict.set_item(key, value)?;
            }
            Ok(dict.into_any())
        }
        serde_yaml::Value::Null => Ok(PyNone::get(*p).to_owned().into_any()),
        serde_yaml::Value::Tagged(_) => Err(pyo3::exceptions::PyValueError::new_err(
            "Tagged values are not supported",
        )),
    }
}
