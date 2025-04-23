use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project::pin_project;
use saluki_metrics::static_metrics;

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
    ///
    /// Whenever the resulting future is polled, an internal metric (`runtime_task.poll_duration_seconds`) will be
    /// updated with the duration of the poll operation, in seconds. This metric will be tagged with the task name
    /// provided here (as `task_name:<task name>`).
    ///
    /// In general, a unique task name should be provided where possible. If multiple tasks share the same task name,
    /// they will all update the same metric, which will simply influence the resulting percentiles and make it more
    /// difficult to isolate outlier poll durations.
    fn with_task_instrumentation(self, task_name: String) -> InstrumentedTask<Self>
    where
        Self: Sized;
}

impl<F> TaskInstrument for F
where
    F: Future + Send + 'static,
{
    fn with_task_instrumentation(self, task_name: String) -> InstrumentedTask<Self> {
        InstrumentedTask {
            telemetry: Telemetry::new(task_name),
            inner: self,
        }
    }
}

/// An instrumented task future.
///
/// This wraps a `Future` and emits telemetry about the duration of each poll operation.
#[pin_project]
pub struct InstrumentedTask<F> {
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

        this.telemetry.poll_duration_seconds.record(poll_duration.as_secs_f64());

        result
    }
}
