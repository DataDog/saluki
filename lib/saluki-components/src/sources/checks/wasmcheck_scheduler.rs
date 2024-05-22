use super::*;

use wasmtime::component::ResourceTable;
use wasmtime::Store;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

struct MyState {
    ctx: WasiCtx,
    table: ResourceTable,
    check_sender: mpsc::Sender<CheckMetric>,
}

impl WasiView for MyState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

struct CheckHandle(u32);

pub struct WasmCheckScheduler {
    linker: wasmtime::component::Linker<MyState>,
    engine: wasmtime::Engine,
    store: Store<MyState>,
    running: HashMap<CheckSource, Vec<CheckHandle>>,
    available_components: HashMap<String, wasmtime::component::Instance>,
}

fn find_available_wasm_checks() -> Result<Vec<PathBuf>, GenericError> {
    let mut available_checks = vec![];
    let wasm_files = std::fs::read_dir("./dist/checks.d/")?;
    for entry in wasm_files {
        let entry = entry?;
        debug!("Considering file: {:?}", entry);
        let path = entry.path();
        if let Some(extension) = path.extension() {
            if extension == "wasm" {
                available_checks.push(path);
            }
        }
    }
    Ok(available_checks)
}

impl WasmCheckScheduler {
    pub fn new(send_check_metrics: mpsc::Sender<CheckMetric>) -> Result<Self, GenericError> {
        let engine = wasmtime::Engine::default();

        let mut linker = wasmtime::component::Linker::<MyState>::new(&engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;

        let mut wasi_builder = WasiCtxBuilder::new();

        let mut store = Store::new(
            &engine,
            MyState {
                ctx: wasi_builder.build(),
                table: ResourceTable::new(),
                check_sender: send_check_metrics,
            },
        );
        // TODO - once I have this version working, move to the `wasmtime::component::bindgen`
        // https://docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html
        // this will let me implement the imports as a trait.
        type Params = (String, f64, Vec<String>);
        linker.root().func_wrap("reportmetric", |store, params: Params| {
            let (metricname, value, tags) = params;
            println!("reportmetric called: {:?}, {:?}, {:?}", metricname, value, tags);
            match store.data().check_sender.try_send(CheckMetric {
                name: metricname,
                metric_type: PyMetricType::Gauge,
                value,
                tags,
            }) {
                Ok(_) => {}
                Err(e) => {
                    error!("Error sending check metric: {:?}", e);
                }
            }
            Ok(())
        })?;

        let mut available_components = HashMap::new();
        let components_on_disk = find_available_wasm_checks()?;
        for component_path in components_on_disk {
            let bytes = std::fs::read(&component_path)?;
            // TODO how to get a name for this component?
            // Could it be statically available on the component? idk enough about WIT / Components to say
            // For now, its the wasm file name
            let component_name = component_path.file_stem().unwrap().to_string_lossy().to_string();

            let component = wasmtime::component::Component::new(&engine, bytes)?;
            let instance = linker.instantiate(&mut store, &component)?;
            available_components.insert(component_name, instance);
        }
        info!(
            "Available components are: {:?}",
            available_components.keys().collect::<Vec<_>>()
        );

        Ok(WasmCheckScheduler {
            linker,
            engine,
            store,
            available_components,
            running: HashMap::new(),
        })
    }
}

impl CheckScheduler for WasmCheckScheduler {
    fn can_run_check(&self, check_request: &RunnableCheckRequest) -> bool {
        info!(
            "Considered whether to run check as wasmcheck: {:?}",
            check_request.check_request
        );
        self.available_components
            .contains_key(&check_request.check_request.name)
    }

    // Currently just runs the 'run' function once, no repeated execution, no instance config, etc.
    fn run_check(&mut self, _check_request: &RunnableCheckRequest) -> Result<(), GenericError> {
        let component = self
            .available_components
            .get(&_check_request.check_request.name)
            .ok_or(generic_error!("Component not found, can't load wasm check"))?;

        let func = component
            .get_func(&mut self.store, "run")
            .expect("run export not found");
        let mut result = [];
        func.call(&mut self.store, &[], &mut result)?;
        Ok(())
    }

    fn stop_check(&mut self, _check_request: CheckRequest) {
        // todo
    }
}
