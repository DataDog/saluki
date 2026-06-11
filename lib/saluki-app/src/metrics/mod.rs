//! Metrics.

use std::time::Duration;

use async_trait::async_trait;
use metrics::{gauge, Level};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::{
    observability::metrics::MetricsFlusherWorker,
    runtime::{InitializationError, Supervisable, SupervisorFuture},
};
use saluki_error::GenericError;
use saluki_metrics::static_metrics;
use tokio::{runtime::Handle, select, time::sleep};

mod api;
pub use self::api::{MetricsAPIHandler, MetricsOverrideWorker};

/// The set of workers spawned by [`initialize_metrics`].
///
/// Each worker must be added to a [`Supervisor`][saluki_core::runtime::Supervisor] for the metrics subsystem to
/// fully function: the flusher worker propagates internal metrics to subscribers, the runtime worker emits Tokio
/// runtime gauges, and the override processor asserts the privileged API routes and handles dynamic filter
/// overrides driven through them.
pub(crate) struct MetricsWorkers {
    pub runtime: RuntimeMetricsWorker,
    pub flusher: MetricsFlusherWorker,
    pub override_processor: MetricsOverrideWorker,
}

/// Initializes the metrics subsystem for `metrics`.
///
/// The given prefix is used to namespace all metrics that are emitted by the application, and is prepended to all
/// metrics, followed by a period (for example, `<prefix>.<metric name>`). The given default level seeds the runtime
/// filter and is what the filter is restored to when [`MetricsAPIHandler`]'s reset route is invoked.
///
/// Returns a [`MetricsWorkers`] bundle containing the supervisable workers needed to drive the metrics subsystem
/// at runtime.
///
/// # Errors
///
/// If the metrics subsystem was already initialized, an error will be returned.
pub(crate) async fn initialize_metrics(
    metrics_prefix: impl Into<String>, default_level: Level,
) -> Result<MetricsWorkers, GenericError> {
    // We forward to the implementation in `saluki_core` so that we can have this crate be the collection point of all
    // helpers/types that are specific to generic application setup/initialization.
    //
    // The implementation itself has to live in `saluki_core`, however, to have access to all of the underlying types
    // that are created and used to install the global recorder, such that they need not be exposed publicly.
    let (filter_handle, flusher) =
        saluki_core::observability::metrics::initialize_metrics(metrics_prefix.into(), default_level).await?;

    let override_processor = MetricsOverrideWorker::new(filter_handle);

    // Capture the current runtime handle eagerly so the runtime metrics worker measures the runtime that owns
    // bootstrap, regardless of where the worker future eventually executes under the supervisor.
    let runtime = RuntimeMetricsWorker::new("primary", Handle::current());

    Ok(MetricsWorkers {
        runtime,
        flusher,
        override_processor,
    })
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

/// Collects Tokio runtime metrics from the given runtime handle.
///
/// All metrics generated will include a `runtime_id` label which maps to the given runtime ID. This allows for
/// differentiating between multiple runtimes that may be running in the same process.
pub async fn collect_runtime_metrics(runtime_id: &str, handle: Handle) {
    // Grab the total number of runtime workers to properly initialize/register our metrics.
    let runtime_metrics = RuntimeMetrics::with_workers(runtime_id, handle.metrics().num_workers());

    // With our metrics registered, enter the main loop where we periodically scrape the metrics.
    loop {
        let latest_runtime_metrics = handle.metrics();
        runtime_metrics.update(&latest_runtime_metrics);

        sleep(Duration::from_secs(5)).await;
    }
}

/// A worker that periodically collects Tokio runtime metrics.
///
/// The runtime is captured at construction time so that the metrics measured always describe the runtime that
/// owned the bootstrap, regardless of where this worker eventually executes under a supervisor.
pub struct RuntimeMetricsWorker {
    runtime_id: String,
    handle: Handle,
}

impl RuntimeMetricsWorker {
    /// Creates a new `RuntimeMetricsWorker` that collects metrics from the given runtime.
    pub fn new<S: Into<String>>(runtime_id: S, handle: Handle) -> Self {
        Self {
            runtime_id: runtime_id.into(),
            handle,
        }
    }
}

#[async_trait]
impl Supervisable for RuntimeMetricsWorker {
    fn name(&self) -> &str {
        "tokio-runtime-metrics-collector"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let runtime_id = self.runtime_id.clone();
        let handle = self.handle.clone();

        Ok(Box::pin(async move {
            select! {
                _ = process_shutdown => {},
                _ = collect_runtime_metrics(&runtime_id, handle) => {},
            }

            Ok(())
        }))
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
        debug_gauge(num_alive_tasks),
        debug_gauge(blocking_queue_depth),
        debug_gauge(budget_forced_yield_count),
        debug_gauge(global_queue_depth),
        debug_gauge(io_driver_fd_deregistered_count),
        debug_gauge(io_driver_fd_registered_count),
        debug_gauge(io_driver_ready_count),
        debug_gauge(num_blocking_threads),
        debug_gauge(num_idle_blocking_threads),
        debug_gauge(num_workers),
        debug_gauge(remote_schedule_count),
        debug_gauge(spawned_tasks_count),
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
