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
       debug_counter(poll_count),
       trace_histogram(poll_duration_seconds),
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

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
    };

    use saluki_metrics::test::TestRecorder;

    use super::*;

    /// A future that returns `Pending` a fixed number of times before completing, letting a test
    /// drive a known number of polls.
    struct PendsThenReady {
        pending_polls: usize,
    }

    impl Future for PendsThenReady {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            if self.pending_polls == 0 {
                Poll::Ready(())
            } else {
                self.pending_polls -= 1;
                Poll::Pending
            }
        }
    }

    struct NoopWaker;

    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn poll_records_one_duration_sample_per_poll() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // Two `Pending` polls followed by one `Ready` poll: three polls total.
        let task = PendsThenReady { pending_polls: 2 }.with_task_instrumentation("poll_duration_test".to_string());
        let mut task = Box::pin(task);

        let waker = Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let mut polls = 0;
        loop {
            polls += 1;
            if task.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
        assert_eq!(polls, 3);

        // The documented behavior is that every poll records one `poll_duration_seconds` sample,
        // tagged with the task name provided to `with_task_instrumentation`.
        let samples = recorder
            .histogram((
                Telemetry::poll_duration_seconds_name(),
                &[("task_name", "poll_duration_test")],
            ))
            .expect("poll-duration histogram should be registered");
        assert_eq!(samples.len(), 3, "each poll must record one duration sample");
        assert!(
            samples.iter().all(|&sample| sample >= 0.0),
            "recorded poll durations must be non-negative"
        );

        // NOTE: `poll_count` is declared in the telemetry block but is never incremented by
        // `InstrumentedTask::poll` (only the histogram is recorded). This assertion pins that
        // current gap; if poll counting is ever wired up, this test should be updated to match.
        assert_eq!(
            recorder.counter((Telemetry::poll_count_name(), &[("task_name", "poll_duration_test")])),
            Some(0)
        );
    }
}
