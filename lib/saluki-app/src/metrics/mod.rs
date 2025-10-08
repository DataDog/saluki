//! Metrics.

use std::{sync::Mutex, time::Duration};

use metrics::gauge;
use saluki_common::task::spawn_traced_named;
use saluki_error::GenericError;
use saluki_metrics::static_metrics;
use tokio::{runtime::Handle, time::sleep};

static API_HANDLER: Mutex<Option<MetricsAPIHandler>> = Mutex::new(None);

mod api;
pub use self::api::MetricsAPIHandler;

/// Initializes the metrics subsystem for `metrics`.
///
/// The given prefix is used to namespace all metrics that are emitted by the application, and is prepended to all
/// metrics, followed by a period (e.g. `<prefix>.<metric name>`).
///
/// # Errors
///
/// If the metrics subsystem was already initialized, an error will be returned.
pub(crate) async fn initialize_metrics(metrics_prefix: impl Into<String>) -> Result<(), GenericError> {
    // We forward to the implementation in `saluki_core` so that we can have this crate be the collection point of all
    // helpers/types that are specific to generic application setup/initialization.
    //
    // The implementation itself has to live in `saluki_core`, however, to have access to all of the underlying types
    // that are created and used to install the global recorder, such that they need not be exposed publicly.
    let filter_handle = saluki_core::observability::metrics::initialize_metrics(metrics_prefix.into()).await?;
    API_HANDLER
        .lock()
        .unwrap()
        .replace(MetricsAPIHandler::new(filter_handle));

    // We also spawn a background task that collects and emits the Tokio runtime metrics.
    spawn_traced_named(
        "tokio-runtime-metrics-collector-primary",
        collect_runtime_metrics("primary"),
    );

    Ok(())
}

/// Acquires the metrics API handler.
///
/// This function is mutable, and consumes the handler if it's present. This means it should only be called once, and
/// only after metrics have been initialized via `initialize_metrics`.
///
/// The metrics API handler can be used to install API routes which allow dynamically controlling the metrics level
/// filtering. See [`MetricsAPIHandler`] for more information.
pub fn acquire_metrics_api_handler() -> Option<MetricsAPIHandler> {
    API_HANDLER.lock().unwrap().take()
}

/// Emits the startup metrics for the application.
///
/// This is generally meant to be called after the application has been initialized, in order to indicate the
/// application has completed start-up and is now running.
///
/// Must be called after the metrics subsystem has been initialized.
pub fn emit_startup_metrics() {
    let app_details = saluki_metadata::get_app_details();
    let app_version = if app_details.is_dev_build() {
        format!("{}-dev-{}", app_details.version().raw(), app_details.git_hash(),)
    } else {
        app_details.version().raw().to_string()
    };

    // Emit a "running" metric to indicate that the application is running.
    gauge!("running", "version" => app_version).set(1.0);
}

/// Collects Tokio runtime metrics on the current Tokio runtime.
///
/// All metrics generate will include a `runtime_id` label which maps to the given runtime ID. This allows for
/// differentiating between multiple runtimes that may be running in the same process.
pub async fn collect_runtime_metrics(runtime_id: &str) {
    // Get the handle to the runtime we're executing in, and then grab the total number of runtime workers.
    //
    // We need the number of workers to properly initialize/register our metrics.
    let handle = Handle::current();
    let runtime_metrics = RuntimeMetrics::with_workers(runtime_id, handle.metrics().num_workers());

    // With our metrics registered, enter the main loop where we periodically scrape the metrics.
    loop {
        let latest_runtime_metrics = handle.metrics();
        runtime_metrics.update(&latest_runtime_metrics);

        sleep(Duration::from_secs(5)).await;
    }
}

static_metrics!(
    name => WorkerMetrics,
    prefix => runtime_worker,
    labels => [runtime_id: String, worker_idx: String],
    metrics => [
        trace_gauge(local_queue_depth),
        trace_gauge(local_schedule_count),
        trace_gauge(mean_poll_time),
        trace_gauge(noop_count),
        trace_gauge(overflow_count),
        trace_gauge(park_count),
        trace_gauge(park_unpark_count),
        trace_gauge(poll_count),
        trace_gauge(steal_count),
        trace_gauge(steal_operations),
        trace_gauge(total_busy_duration),
    ]
);

impl WorkerMetrics {
    fn with_worker_idx(runtime_id: &str, worker_idx: usize) -> Self {
        Self::new(runtime_id.to_string(), worker_idx.to_string())
    }

    fn update(&self, worker_idx: usize, metrics: &tokio::runtime::RuntimeMetrics) {
        self.local_queue_depth()
            .set(metrics.worker_local_queue_depth(worker_idx) as f64);
        self.local_schedule_count()
            .set(metrics.worker_local_schedule_count(worker_idx) as f64);
        self.mean_poll_time()
            .set(metrics.worker_mean_poll_time(worker_idx).as_nanos() as f64);
        self.noop_count().set(metrics.worker_noop_count(worker_idx) as f64);
        self.overflow_count()
            .set(metrics.worker_overflow_count(worker_idx) as f64);
        self.park_count().set(metrics.worker_park_count(worker_idx) as f64);
        self.park_unpark_count()
            .set(metrics.worker_park_unpark_count(worker_idx) as f64);
        self.poll_count().set(metrics.worker_poll_count(worker_idx) as f64);
        self.steal_count().set(metrics.worker_steal_count(worker_idx) as f64);
        self.steal_operations()
            .set(metrics.worker_steal_operations(worker_idx) as f64);
        self.total_busy_duration()
            .set(metrics.worker_total_busy_duration(worker_idx).as_nanos() as f64);
    }
}

static_metrics!(
    name => GlobalRuntimeMetrics,
    prefix => runtime,
    labels => [runtime_id: String],
    metrics => [
        gauge(num_alive_tasks),
        gauge(blocking_queue_depth),
        gauge(budget_forced_yield_count),
        gauge(global_queue_depth),
        gauge(io_driver_fd_deregistered_count),
        gauge(io_driver_fd_registered_count),
        gauge(io_driver_ready_count),
        gauge(num_blocking_threads),
        gauge(num_idle_blocking_threads),
        gauge(num_workers),
        gauge(remote_schedule_count),
        gauge(spawned_tasks_count),
    ],
);

struct RuntimeMetrics {
    global: GlobalRuntimeMetrics,
    workers: Vec<WorkerMetrics>,
}

impl RuntimeMetrics {
    fn with_workers(runtime_id: &str, workers_len: usize) -> Self {
        let mut workers = Vec::with_capacity(workers_len);
        for i in 0..workers_len {
            workers.push(WorkerMetrics::with_worker_idx(runtime_id, i));
        }

        Self {
            global: GlobalRuntimeMetrics::new(runtime_id.to_string()),
            workers,
        }
    }

    fn update(&self, metrics: &tokio::runtime::RuntimeMetrics) {
        self.global.num_alive_tasks().set(metrics.num_alive_tasks() as f64);
        self.global
            .blocking_queue_depth()
            .set(metrics.blocking_queue_depth() as f64);
        self.global
            .budget_forced_yield_count()
            .set(metrics.budget_forced_yield_count() as f64);
        self.global
            .global_queue_depth()
            .set(metrics.global_queue_depth() as f64);
        self.global
            .io_driver_fd_deregistered_count()
            .set(metrics.io_driver_fd_deregistered_count() as f64);
        self.global
            .io_driver_fd_registered_count()
            .set(metrics.io_driver_fd_registered_count() as f64);
        self.global
            .io_driver_ready_count()
            .set(metrics.io_driver_ready_count() as f64);
        self.global
            .num_blocking_threads()
            .set(metrics.num_blocking_threads() as f64);
        self.global
            .num_idle_blocking_threads()
            .set(metrics.num_idle_blocking_threads() as f64);
        self.global.num_workers().set(metrics.num_workers() as f64);
        self.global
            .remote_schedule_count()
            .set(metrics.remote_schedule_count() as f64);
        self.global
            .spawned_tasks_count()
            .set(metrics.spawned_tasks_count() as f64);

        for (worker_idx, worker) in self.workers.iter().enumerate() {
            worker.update(worker_idx, metrics);
        }
    }
}
