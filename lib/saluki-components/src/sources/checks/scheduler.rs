use std::sync::Arc;

use rustpython_vm::{compiler::Mode, PyObjectRef};

use super::*;

struct CheckHandle {
    id: PyObjectRef,
}

pub struct CheckScheduler {
    schedule_check_rx: mpsc::Receiver<RunnableCheckRequest>,
    unschedule_check_rx: mpsc::Receiver<CheckRequest>,
    interpreter: Arc<Interpreter>,
    running: HashMap<CheckSource, Vec<tokio::task::JoinHandle<()>>>,
}
impl CheckScheduler {
    pub fn new(
        schedule_check_rx: mpsc::Receiver<RunnableCheckRequest>, unschedule_check_rx: mpsc::Receiver<CheckRequest>,
    ) -> Self {
        let interpreter = Interpreter::with_init(Default::default(), |vm| {
            vm.add_native_modules(rustpython_stdlib::get_module_inits());
        });

        Self {
            schedule_check_rx,
            unschedule_check_rx,
            interpreter: Arc::new(interpreter),
            running: HashMap::new(),
        }
    }

    // consumes self
    pub async fn run(mut self) {
        loop {
            select! {
                Some(check) = self.schedule_check_rx.recv() => {
                    self.run_check(check).await;
                }
                Some(check) = self.unschedule_check_rx.recv() => {
                    self.stop_check(check).await;
                }
            }
        }
    }

    // compiles the python source code and instantiates it (??) into the VM
    // returns an opaque handle to the check
    fn register_check(&mut self, check_source_path: PathBuf) -> Result<CheckHandle, Error> {
        let py_source = std::fs::read_to_string(&check_source_path).unwrap();
        info!(?py_source, "Trying to run the following code");
        //let py_source = r#"print("hello world")"#;
        let path_to_source = check_source_path
            .as_os_str()
            .to_str()
            .unwrap_or("<embedded>")
            .to_string();

        self.interpreter
            .enter(|vm| {
                let scope = vm.new_scope_with_builtins();
                let code_obj = vm
                    .compile(&py_source, Mode::Exec, path_to_source)
                    .map_err(|err| vm.new_syntax_error(&err, Some(&py_source)))
                    .unwrap();
                let run_res = vm.run_code_obj(code_obj, scope);
                match run_res {
                    Ok(t) => {
                        info!(?t, "ran compiled pycheck");
                        Ok(CheckHandle { id: t })
                    }
                    Err(e) => {
                        error!(?e, "failed to run compiled pycheck");
                        Err(e)
                    }
                }
            })
            .map_err(Error::from)
    }

    // This function does 3 things
    // 1. Registers the check source code and gets a handle
    // 2. Starts a local task for each instance that
    //    queues a run of the check every min_collection_interval_ms
    // 3. Stores the handles in the running hashmap
    async fn run_check(&mut self, check: RunnableCheckRequest) {
        // registry should probably queue off of checkhandle
        //let current = self.running.entry(check.check_request.source.clone()).or_default();

        let check_handle = self.register_check(check.check_source_code.clone());

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
    }

    async fn stop_check(&self, check: CheckRequest) {
        info!("Deleting check request {check}");
        if let Some(running) = self.running.get(&check.source) {
            for handle in running {
                handle.abort();
            }
        }
    }
}
