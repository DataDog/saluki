use std::{
    future::Future,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
};

use pin_project::{pin_project, pinned_drop};
use saluki_metrics::static_metrics;

use crate::collections::FastConcurrentHashMap;

static TASK_INSTRUMENTATION_REGISTRY: OnceLock<InstrumentationRegistry> = OnceLock::new();

static_metrics!(
   name => Telemetry,
   prefix => runtime_task,
   labels => [task_name: String],
   metrics => [
       counter(poll_count),
       histogram(poll_duration_seconds),
   ],
);

/// Helper trait for instrumenting futures that are run as asynchronous tasks.
pub trait TaskInstrument {
    /// Instruments the future, tracking task-specific metrics about its execution.
    fn with_task_instrumentation(self, task_name: String) -> InstrumentedTask<Self>
    where
        Self: Sized;
}

impl<F> TaskInstrument for F
where
    F: Future + Send + 'static,
{
    fn with_task_instrumentation(self, task_name: String) -> InstrumentedTask<Self> {
        InstrumentationRegistry::global().instrument_task(task_name, self)
    }
}

#[pin_project(PinnedDrop)]
pub struct InstrumentedTask<F> {
    task_name: String,
    telemetry: Telemetry,

    #[pin]
    inner: F,
}

impl<F> Future for InstrumentedTask<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let poll_start = std::time::Instant::now();
        let result = this.inner.poll(cx);
        let poll_duration = poll_start.elapsed();

        this.telemetry.poll_count.increment(1);
        this.telemetry.poll_duration_seconds.record(poll_duration.as_secs_f64());

        result
    }
}

#[pinned_drop]
impl<F> PinnedDrop for InstrumentedTask<F> {
    fn drop(self: Pin<&mut Self>) {
        InstrumentationRegistry::global().remove_task(&self.task_name);
    }
}

// TODO: Use `Arc<str>` instead of `String` for the task name so we can amortize allocations between having to allocate
// for fixed, static strings as well as the multiple clones we need between the task map, telemetry struct, and the
// instrumented wrapper struct.
struct InstrumentationRegistry {
    tasks: FastConcurrentHashMap<String, Telemetry>,
}

impl InstrumentationRegistry {
    fn new() -> Self {
        Self {
            tasks: FastConcurrentHashMap::default(),
        }
    }

    /// Returns a reference to the global instrumentation registry.
    pub fn global() -> &'static Self {
        TASK_INSTRUMENTATION_REGISTRY.get_or_init(InstrumentationRegistry::new)
    }

    fn instrument_task<F>(&self, task_name: String, f: F) -> InstrumentedTask<F>
    where
        F: Future + Send + 'static,
    {
        let tasks_guard = self.tasks.pin();
        let telemetry = tasks_guard
            .get_or_insert_with(task_name.clone(), || Telemetry::new(task_name.clone()))
            .clone();

        InstrumentedTask {
            task_name,
            telemetry,
            inner: f,
        }
    }

    fn remove_task(&self, task_name: &str) {
        self.tasks.pin().remove(task_name);
    }
}
