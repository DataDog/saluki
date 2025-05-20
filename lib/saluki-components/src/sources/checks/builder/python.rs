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
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tracing::{debug, error, info, warn};

use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;

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

    fn interval(&self) -> &Duration {
        &self.interval
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

static PYTHON_CHECK_BUILDER: OnceLock<PythonEnvBuilder> = OnceLock::new();

struct PythonEnvBuilder {
    valid: bool,
    agent_check_class: Option<PyObject>,
}

impl PythonEnvBuilder {
    pub fn get_instance() -> &'static PythonEnvBuilder {
        PYTHON_CHECK_BUILDER.get_or_init(PythonEnvBuilder::initialize)
    }

    fn initialize() -> Self {
        pyo3::prepare_freethreaded_python();
        let result = Python::with_gil(|py| -> Result<PyObject, GenericError> {
            if let Ok(sys) = py.import("sys") {
                if let Ok(path_attr) = sys.getattr("path") {
                    match path_attr.downcast::<PyList>() {
                        Ok(p) => {
                            // Get the project root to use as a base for relative paths
                            let project_root = match std::env::current_exe() {
                                Ok(path) => {
                                    // Get the project root directory (two levels up from executable)
                                    let mut project_root = path.clone();
                                    // Go up from executable to debug dir
                                    if let Some(parent) = project_root.parent() {
                                        project_root = parent.to_path_buf();
                                        // Go up from debug dir to target dir
                                        if let Some(parent) = project_root.parent() {
                                            project_root = parent.to_path_buf();
                                            // Go up from target dir to project root
                                            if let Some(parent) = project_root.parent() {
                                                project_root = parent.to_path_buf();
                                            }
                                        }
                                    }
                                    project_root
                                }
                                Err(e) => {
                                    error!("Failed to get current executable path: {}", e);
                                    Path::new(".").to_path_buf()
                                }
                            };

                            // Add paths relative to executable location
                            let dist_path = project_root.join("dist");
                            let checks_d_path = dist_path.join("checks.d");

                            p.insert(0, dist_path.to_string_lossy().as_ref()).unwrap(); // common modules are shipped in the dist path directly or under the "checks/" sub-dir
                            p.insert(0, checks_d_path.to_string_lossy().as_ref()).unwrap(); // integrations-core legacy checks
                            p.insert(0, "/etc/datadog-agent/checks.d/").unwrap(); // Agent checks folder

                            info!("Python sys.path is: {:?}", p);
                        }
                        Err(e) => {
                            return Err(generic_error!("Could not get sys.path {:?}", e));
                        }
                    };
                }
            } else {
                return Err(generic_error!("Could not import sys module"));
            }

            // Validate that python env is correctly configured
            let modd = match py.import("datadog_checks.checks") {
                Ok(m) => m,
                Err(e) => {
                    if let Some(traceback) = e.traceback(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    return Err(generic_error!("Could not import datadog_checks module {:?}", e));
                }
            };
            match modd.getattr("AgentCheck") {
                Ok(c) => {
                    info!("All pre-requisites for running python checks.");
                    Ok(c.unbind())
                }
                Err(e) => {
                    if let Some(traceback) = e.traceback(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    Err(generic_error!("Could not get AgentCheck class {:?}", e))
                }
            }
        });

        match result {
            Ok(agent_check_class) => Self {
                valid: true,
                agent_check_class: Some(agent_check_class),
            },
            Err(e) => {
                warn!("Could not load Python runtime. Cannot build Python check. {:?}", e);
                Self {
                    valid: false,
                    agent_check_class: None,
                }
            }
        }
    }
}

pub struct PythonCheckBuilder;

impl CheckBuilder for PythonCheckBuilder {
    fn build_check(
        &self, name: &str, instance: &Instance, init_config: &Data, source: &MetaString,
    ) -> Option<Arc<dyn Check + Send + Sync>> {
        let env = PythonEnvBuilder::get_instance();
        if !env.valid {
            None
        } else {
            let mut load_errors = vec![];
            for import_path in [name.to_string(), format!("datadog_checks.{}", name)].iter() {
                match self.register_check_from_imports(
                    env.agent_check_class.as_ref().expect("agent check class"),
                    name,
                    import_path,
                    instance,
                    init_config,
                    source,
                ) {
                    Ok(handle) => return Some(handle),
                    Err(e) => {
                        load_errors.push(e.root_cause().to_string());
                    }
                }
            }
            error!("Could not load check {} errors: {:?}", name, load_errors);
            None
        }
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
                return Err(generic_error!("No checks class found in source"));
            }
            if checks.len() >= 2 {
                return Err(generic_error!("Multiple checks classes found in source"));
            }
            let (check_key, check_value) = &checks[0];
            debug!("Found base class {check_key} for check {import_path} {check_value:?}");
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

            let parse_instance =
                map_to_pydict(instance.get_value(), &py).expect("Could not convert instance to pydict");

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
