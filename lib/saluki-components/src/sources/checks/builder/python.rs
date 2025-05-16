//! A Python implementation of CheckBuilder

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pyo3::types::{PyDict, PyList, PyNone, PyTuple, PyType};
use pyo3::PyObject;
use pyo3::{prelude::*, IntoPyObjectExt};
use saluki_env::autodiscovery::{Data, Instance, RawData};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tracing::{debug, error, info, warn};

use crate::sources::checks::builder::CheckBuilder;
use crate::sources::checks::check::Check;

#[allow(dead_code)]
struct PythonCheck {
    check_class: PyObject,
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
    /// Get the interval of the check
    fn interval(&self) -> &Duration {
        &self.interval
    }
    /// Get the id of the check
    fn id(&self) -> &str {
        &self.id
    }
}

pub struct PythonCheckBuilder {
    valid: bool,
    agent_check_class: Option<PyObject>,
}

impl PythonCheckBuilder {
    pub fn new() -> Self {
        pyo3::prepare_freethreaded_python();
        let result = Python::with_gil(|py| -> Option<PyObject> {
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

                            p.insert(0, &dist_path).unwrap(); // path.GetDistPath()
                            p.insert(0, &checks_d_path).unwrap(); // custom checks in checks.d subdir
                            p.insert(0, Path::new("/etc/datadog-agent/checks.d/")).unwrap(); // custom checks in checks.d subdir

                            info!("Python sys.path is: {:?}", p);
                        }
                        Err(e) => {
                            error!(%e, "Could not get sys.path");
                            return None;
                        }
                    };
                }
            } else {
                error!("Could not import sys module");
                return None;
            }

            // Validate that python env is correctly configured
            let modd = match py.import("datadog_checks.checks") {
                Ok(m) => m,
                Err(e) => {
                    if let Some(traceback) = e.traceback(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    error!(%e, "Could not import datadog_checks module");
                    return None;
                }
            };
            match modd.getattr("AgentCheck") {
                Ok(c) => {
                    info!("All pre-requisites for running python checks.");
                    Some(c.unbind())
                }
                Err(e) => {
                    if let Some(traceback) = e.traceback(py) {
                        error!("Traceback: {}", traceback.format().expect("Could format traceback"));
                    }
                    error!(%e, "Could not get AgentCheck class.");
                    None
                }
            }
        });

        match result {
            Some(agent_check_class) => Self {
                valid: true,
                agent_check_class: Some(agent_check_class),
            },
            None => Self {
                valid: false,
                agent_check_class: None,
            },
        }
    }
}

#[async_trait]
impl CheckBuilder for PythonCheckBuilder {
    async fn build_check(
        &self, name: &str, instance: &Instance, init_config: &Data, source: &MetaString,
    ) -> Option<Arc<dyn Check + Send + Sync>> {
        if !self.valid {
            warn!("Python check builder is not configured. Cannot build check.");
            return None;
        } else {
            let mut load_errors = vec![];
            for import_str in [name.to_string(), format!("datadog_checks.{}", name)].iter() {
                match self.register_check_from_imports(name, import_str, instance, init_config, source) {
                    Ok(handle) => return Some(handle),
                    Err(e) => {
                        load_errors.push(e.root_cause().to_string());
                    }
                }
            }
            error!("Could not load check {} errors: {:?}", name, load_errors);
            return None;
        }
    }
}

impl PythonCheckBuilder {
    fn register_check_from_imports(
        &self, name: &str, import_path: &str, instance: &Instance, init_config: &Data, source: &MetaString,
    ) -> Result<Arc<dyn Check + Send + Sync>, GenericError> {
        pyo3::Python::with_gil(|py| {
            let module = py.import(import_path)?;

            let base_class = self
                .agent_check_class
                .as_ref()
                .ok_or_else(|| generic_error!("PythonCheckBuilder is not configured. Cannot build check."))?;

            let check_class = base_class.bind(py);

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

            let kwargs = PyDict::new(py);
            let parsed_config =
                map_to_pydict(init_config.get_value(), &py).expect("Could not convert init_config to pydict");

            let parse_instance =
                map_to_pydict(instance.get_value(), &py).expect("Could not convert instance to pydict");

            let py_vec = PyTuple::new(py, vec![parse_instance]).expect("Could not create tuple.");

            kwargs.set_item("name", name).expect("Could not set name");
            kwargs
                .set_item("init_config", parsed_config)
                .expect("could not set init_config");
            kwargs
                .set_item("instances", py_vec)
                .expect("could not set instance list");

            let check_instance = check_value
                .call((), Some(&kwargs))
                .expect("Could not create check instance");

            check_instance
                .setattr("check_id", instance.id())
                .expect("Could not set check_id");

            let check = Arc::new(PythonCheck {
                check_class: check_value.clone().unbind(),
                version,
                // TODO: extract interval from instance configuration
                interval: Duration::from_secs(5),
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
