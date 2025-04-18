use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use pin_project::{pin_project, pinned_drop};
use saluki_metrics::static_metrics;

use crate::collections::FastConcurrentHashMap;

static TASK_INSTRUMENTATION_REGISTRY: OnceLock<InstrumentationRegistry> = OnceLock::new();

static_metrics!(
   name => Telemetry,
   prefix => runtime_task,
   labels => [task_name: Arc<str>],
   metrics => [
       counter(poll_count),
       histogram(poll_duration_seconds),
   ],
);

/// Helper trait for instrumenting futures that are run as asynchronous tasks.
pub trait TaskInstrument {
    /// Instruments the future, tracking task-specific metrics about its execution.
    fn with_task_instrumentation<S: AsRef<str>>(self, task_name: S) -> InstrumentedTask<Self>
    where
        Self: Sized;
}

impl<F> TaskInstrument for F
where
    F: Future + Send + 'static,
{
    fn with_task_instrumentation<S: AsRef<str>>(self, task_name: S) -> InstrumentedTask<Self> {
        InstrumentationRegistry::global().instrument_task(Arc::from(task_name.as_ref()), self)
    }
}

#[pin_project(PinnedDrop)]
pub struct InstrumentedTask<F> {
    task_name: Arc<str>,
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

struct InstrumentationRegistry {
    tasks: FastConcurrentHashMap<Arc<str>, Telemetry>,
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

    fn instrument_task<F>(&self, task_name: Arc<str>, f: F) -> InstrumentedTask<F>
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

    fn remove_task(&self, task_name: &Arc<str>) {
        self.tasks.pin().remove(task_name);
    }
}
