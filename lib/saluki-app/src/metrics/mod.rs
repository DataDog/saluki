//! Metrics.

use std::{sync::Mutex, time::Duration};

use metrics::{gauge, Gauge};
use saluki_common::task::spawn_traced_named;
use tokio::{runtime::Handle, time::sleep};

static API_HANDLER: Mutex<Option<MetricsAPIHandler>> = Mutex::new(None);

mod api;
pub use self::api::MetricsAPIHandler;

/// Initializes the metrics subsystem for `metrics`.
///
/// The given prefix is used to namespace all metrics that are emitted by the application, and is prepended to all
/// metrics, followed by a period (e.g. `<prefix>.<metric name>`).
///
/// ## Errors
///
/// If the metrics subsystem was already initialized, an error will be returned.
pub async fn initialize_metrics(
    metrics_prefix: impl Into<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

struct WorkerMetrics {
    local_queue_depth: Gauge,
    local_schedule_count: Gauge,
    mean_poll_time: Gauge,
    noop_count: Gauge,
    overflow_count: Gauge,
    park_count: Gauge,
    park_unpark_count: Gauge,
    poll_count: Gauge,
    steal_count: Gauge,
    steal_operations: Gauge,
    total_busy_duration: Gauge,
}

impl WorkerMetrics {
    fn with_worker_idx(runtime_id: &str, worker_idx: usize) -> Self {
        let labels = [
            ("runtime_id", runtime_id.to_string()),
            ("worker_idx", worker_idx.to_string()),
        ];

        Self {
            local_queue_depth: gauge!("runtime.worker.local_queue_depth", &labels),
            local_schedule_count: gauge!("runtime.worker.local_schedule_count", &labels),
            mean_poll_time: gauge!("runtime.worker.mean_poll_time_ns", &labels),
            noop_count: gauge!("runtime.worker.noop_count", &labels),
            overflow_count: gauge!("runtime.worker.overflow_count", &labels),
            park_count: gauge!("runtime.worker.park_count", &labels),
            park_unpark_count: gauge!("runtime.worker.park_unpark_count", &labels),
            poll_count: gauge!("runtime.worker.poll_count", &labels),
            steal_count: gauge!("runtime.worker.steal_count", &labels),
            steal_operations: gauge!("runtime.worker.steal_operations", &labels),
            total_busy_duration: gauge!("runtime.worker.total_busy_duration_ns", &labels),
        }
    }

    fn update(&self, worker_idx: usize, metrics: &tokio::runtime::RuntimeMetrics) {
        self.local_queue_depth
            .set(metrics.worker_local_queue_depth(worker_idx) as f64);
        self.local_schedule_count
            .set(metrics.worker_local_schedule_count(worker_idx) as f64);
        self.mean_poll_time
            .set(metrics.worker_mean_poll_time(worker_idx).as_nanos() as f64);
        self.noop_count.set(metrics.worker_noop_count(worker_idx) as f64);
        self.overflow_count
            .set(metrics.worker_overflow_count(worker_idx) as f64);
        self.park_count.set(metrics.worker_park_count(worker_idx) as f64);
        self.park_unpark_count
            .set(metrics.worker_park_unpark_count(worker_idx) as f64);
        self.poll_count.set(metrics.worker_poll_count(worker_idx) as f64);
        self.steal_count.set(metrics.worker_steal_count(worker_idx) as f64);
        self.steal_operations
            .set(metrics.worker_steal_operations(worker_idx) as f64);
        self.total_busy_duration
            .set(metrics.worker_total_busy_duration(worker_idx).as_nanos() as f64);
    }
}

struct RuntimeMetrics {
    num_alive_tasks: Gauge,
    blocking_queue_depth: Gauge,
    budget_forced_yield_count: Gauge,
    global_queue_depth: Gauge,
    io_driver_fd_deregistered_count: Gauge,
    io_driver_fd_registered_count: Gauge,
    io_driver_ready_count: Gauge,
    num_blocking_threads: Gauge,
    num_idle_blocking_threads: Gauge,
    num_workers: Gauge,
    remote_schedule_count: Gauge,
    spawned_tasks_count: Gauge,
    worker_metrics: Vec<WorkerMetrics>,
}

impl RuntimeMetrics {
    fn with_workers(runtime_id: &str, workers_len: usize) -> Self {
        let labels = [("runtime_id", runtime_id.to_string())];
        let mut worker_metrics = Vec::with_capacity(workers_len);
        for i in 0..workers_len {
            worker_metrics.push(WorkerMetrics::with_worker_idx(runtime_id, i));
        }

        Self {
            num_alive_tasks: gauge!("runtime.num_alive_tasks", &labels),
            blocking_queue_depth: gauge!("runtime.blocking_queue_depth", &labels),
            budget_forced_yield_count: gauge!("runtime.budget_forced_yield_count", &labels),
            global_queue_depth: gauge!("runtime.global_queue_depth", &labels),
            io_driver_fd_deregistered_count: gauge!("runtime.io_driver_fd_deregistered_count", &labels),
            io_driver_fd_registered_count: gauge!("runtime.io_driver_fd_registered_count", &labels),
            io_driver_ready_count: gauge!("runtime.io_driver_ready_count", &labels),
            num_blocking_threads: gauge!("runtime.num_blocking_threads", &labels),
            num_idle_blocking_threads: gauge!("runtime.num_idle_blocking_threads", &labels),
            num_workers: gauge!("runtime.num_workers", &labels),
            remote_schedule_count: gauge!("runtime.remote_schedule_count", &labels),
            spawned_tasks_count: gauge!("runtime.spawned_tasks_count", &labels),
            worker_metrics,
        }
    }

    fn update(&self, metrics: &tokio::runtime::RuntimeMetrics) {
        self.num_alive_tasks.set(metrics.num_alive_tasks() as f64);
        self.blocking_queue_depth.set(metrics.blocking_queue_depth() as f64);
        self.budget_forced_yield_count
            .set(metrics.budget_forced_yield_count() as f64);
        self.global_queue_depth.set(metrics.global_queue_depth() as f64);
        self.io_driver_fd_deregistered_count
            .set(metrics.io_driver_fd_deregistered_count() as f64);
        self.io_driver_fd_registered_count
            .set(metrics.io_driver_fd_registered_count() as f64);
        self.io_driver_ready_count.set(metrics.io_driver_ready_count() as f64);
        self.num_blocking_threads.set(metrics.num_blocking_threads() as f64);
        self.num_idle_blocking_threads
            .set(metrics.num_idle_blocking_threads() as f64);
        self.num_workers.set(metrics.num_workers() as f64);
        self.remote_schedule_count.set(metrics.remote_schedule_count() as f64);
        self.spawned_tasks_count.set(metrics.spawned_tasks_count() as f64);

        for (worker_idx, worker_metrics) in self.worker_metrics.iter().enumerate() {
            worker_metrics.update(worker_idx, metrics);
        }
    }
}
