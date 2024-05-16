use super::*;
use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;
use pyo3::types::PyType;
use saluki_error::{generic_error, GenericError};

struct CheckHandle<'py> {
    id: Bound<'py, PyAny>,
}

pub struct CheckScheduler<'py> {
    //schedule_check_rx: mpsc::Receiver<RunnableCheckRequest>,
    //unschedule_check_rx: mpsc::Receiver<CheckRequest>,
    running: HashMap<isize, (CheckHandle<'py>, Vec<tokio::task::JoinHandle<()>>)>,
    source_to_handle: HashMap<CheckSource, CheckHandle<'py>>,
}
impl<'py> CheckScheduler<'py> {
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
            //schedule_check_rx,
            //unschedule_check_rx,
            running: HashMap::new(),
            source_to_handle: HashMap::new(),
        }
    }

    /*
    // consumes self
    pub async fn run(mut self) {
        let CheckScheduler {
            mut schedule_check_rx,
            mut unschedule_check_rx,
            ..
        } = self;

        loop {
            select! {
                Some(check) = schedule_check_rx.recv() => {
                    self.run_check(check).await;
                }
                Some(check) = unschedule_check_rx.recv() => {
                    self.stop_check(check).await;
                }
            }
        }
    }
    */

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
            // the locals should now contain the class that this check defines
            // lets make the simplifying assumption that there is only one
            let base_class = locals
                .get_item("AgentCheck")
                .expect("Could not get 'AgentCheck' class")
                .unwrap();
            /*
            for (key, value) in locals.iter() {
                if let Ok(class_obj) = value.downcast::<PyType>() {
                    match class_obj.is_subclass(&base_class) {
                        Ok(true) => {
                            info!(%key, "Found class that is a subclass of AgentCheck");
                        }
                        _ => {}
                    }
                }
            } */
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

            if checks.len() == 0 {
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
            //let class_ref = check_value.unbind();
            return Ok(CheckHandle { id: *check_value });
        })
    }

    // This function does 3 things
    // 1. Registers the check source code and gets a handle
    // 2. Starts a local task for each instance that
    //    queues a run of the check every min_collection_interval_ms
    // 3. Stores the handles in the running hashmap
    pub fn run_check(&'py mut self, check: RunnableCheckRequest) -> Result<(), GenericError> {
        // registry should probably queue off of checkhandle
        //let current = self.running.entry(check.check_request.source.clone()).or_default();

        let check_handle = self.register_check(check.check_source_code.clone())?;
        let check_handle_hash = check_handle.id.hash()?;
        self.running
            .entry(check_handle_hash)
            .or_insert((check_handle, Vec::new()));

        for (idx, instance) in check.check_request.instances.iter().enumerate() {
            let instance = instance.clone();
            let handle = tokio::task::spawn_local(async move {
                let mut interval =
                    tokio::time::interval(Duration::from_millis(instance.min_collection_interval_ms.into()));
                loop {
                    interval.tick().await;
                    // run check
                    info!("Running check instance {idx}");
                    /*
                    self.queue_run_tx
                        .send(check_handle, instance.clone())
                        .await
                        .expect("Could send");
                    */
                }
            });
        }

        Ok(())
    }

    pub fn stop_check(&self, check: CheckRequest) {
        info!("Deleting check request {check}");
        if let Some(check_handle) = self.source_to_handle.get(&check.source) {
            let check_handle_hash = check_handle.id.hash().expect("Could hash");
            if let Some(running) = self.running.get(&check_handle_hash) {
                for handle in running.1.iter() {
                    handle.abort();
                }
            }
        }
        // todo delete stuff out of the containers
        // and stop/release the class if its not being used
    }
}
