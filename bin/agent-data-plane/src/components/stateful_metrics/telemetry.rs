//! Retained sender counters, including idle periods between metric batches.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use foldspace_core::MetricStreamFailureKind;
use saluki_common::collections::FastHashMap;
use saluki_error::GenericError;
use saluki_io::net::util::retry::PushResult;
use saluki_metrics::{Counter, Gauge, Histogram, MetricsBuilder};
use tracing::warn;

pub(super) struct Telemetry {
    pub batches_acked: Counter,
    pub batches_retried: Counter,
    pub batches_abandoned: Counter,
    pub points_dropped: Counter,
    pub profile: WorkerProfile,
    builder: MetricsBuilder,
    stream_failures: FastHashMap<&'static str, Counter>,
}

impl Telemetry {
    pub fn new(builder: MetricsBuilder, endpoints: usize) -> Self {
        Self {
            batches_acked: builder.register_counter("stateful_metrics_batches_acked_total"),
            batches_retried: builder.register_counter("stateful_metrics_batches_retried_total"),
            batches_abandoned: builder.register_counter("stateful_metrics_batches_abandoned_total"),
            points_dropped: builder.register_counter("stateful_metrics_points_dropped_total"),
            profile: WorkerProfile::new(&builder, endpoints),
            builder,
            stream_failures: FastHashMap::default(),
        }
    }

    pub fn stream_failed(&mut self, kind: MetricStreamFailureKind) {
        let kind = match kind {
            MetricStreamFailureKind::Unavailable => "Unavailable",
            MetricStreamFailureKind::DeadlineExceeded => "DeadlineExceeded",
            MetricStreamFailureKind::ResourceExhausted => "ResourceExhausted",
            MetricStreamFailureKind::InvalidArgument => "InvalidArgument",
            MetricStreamFailureKind::Unauthenticated => "Unauthenticated",
            MetricStreamFailureKind::FailedPrecondition => "FailedPrecondition",
            MetricStreamFailureKind::ProtocolViolation => "ProtocolViolation",
        };
        self.stream_failures
            .entry(kind)
            .or_insert_with(|| {
                self.builder
                    .register_counter_with_tags("stateful_metrics_stream_failures_total", [("kind", kind)])
            })
            .increment(1);
    }

    pub fn track_drops(&self, result: PushResult) {
        if result.had_drops() {
            warn!(
                batches = result.items_dropped,
                points = result.data_points_dropped,
                "Stateful metrics retry storage dropped queued data."
            );
            self.batches_abandoned.increment(result.items_dropped);
            self.points_dropped.increment(result.data_points_dropped);
        }
    }

    // A queue rejection consumes the entry; account for it without terminating the sender.
    pub fn track_enqueue(&self, result: Result<PushResult, GenericError>, points: u64) {
        match result {
            Ok(result) => self.track_drops(result),
            Err(error) => {
                warn!(%error, points, "Stateful metrics batch could not enter retry storage.");
                self.batches_abandoned.increment(1);
                self.points_dropped.increment(points);
            }
        }
    }
}

/// Load-test instrumentation for one sender worker: where its time goes and whether it keeps up.
pub(super) struct WorkerProfile {
    /// Time spent inside polls of the worker task; its rate is the share of one core the worker uses.
    pub busy_nanos: Counter,
    pub push_batch_nanos: Counter,
    pub flush_nanos: Counter,
    pub send_batch_to_nanos: Counter,
    /// Logical batches the core encoded, including re-encoded retries.
    pub encodings: Counter,
    /// From the first batch received while idle until everything is acknowledged again.
    pub burst_drain_seconds: Histogram,
    pub dictionary_entries: Gauge,
    pub dictionary_estimated_bytes: Gauge,
    pub inflight_payloads: Gauge,
    pub buffered_series: Gauge,
    /// Indexed by endpoint.
    pub endpoints: Vec<EndpointProfile>,
}

pub(super) struct EndpointProfile {
    pub payloads_sent: Counter,
    /// Compressed payload bytes handed to the transport.
    pub payload_bytes: Counter,
    pub ack_latency_seconds: Histogram,
}

impl WorkerProfile {
    fn new(builder: &MetricsBuilder, endpoints: usize) -> Self {
        let core_nanos = |op: &'static str| {
            builder.register_debug_counter_with_tags("stateful_metrics_core_nanos_total", [("op", op)])
        };
        Self {
            busy_nanos: builder
                .register_debug_counter_with_tags("stateful_metrics_task_busy_nanos_total", [("task", "worker")]),
            push_batch_nanos: core_nanos("push_batch"),
            flush_nanos: core_nanos("flush"),
            send_batch_to_nanos: core_nanos("send_batch_to"),
            encodings: builder.register_debug_counter("stateful_metrics_encodings_total"),
            burst_drain_seconds: builder.register_debug_histogram("stateful_metrics_burst_drain_seconds"),
            dictionary_entries: builder.register_debug_gauge("stateful_metrics_dictionary_entries"),
            dictionary_estimated_bytes: builder.register_debug_gauge("stateful_metrics_dictionary_estimated_bytes"),
            inflight_payloads: builder.register_debug_gauge("stateful_metrics_inflight_payloads"),
            buffered_series: builder.register_debug_gauge("stateful_metrics_buffered_series"),
            endpoints: (0..endpoints)
                .map(|endpoint| {
                    let tag = ("endpoint", endpoint.to_string());
                    EndpointProfile {
                        payloads_sent: builder
                            .register_debug_counter_with_tags("stateful_metrics_payloads_sent_total", [tag.clone()]),
                        payload_bytes: builder
                            .register_debug_counter_with_tags("stateful_metrics_payload_bytes_total", [tag.clone()]),
                        ack_latency_seconds: builder
                            .register_debug_histogram_with_tags("stateful_metrics_ack_latency_seconds", [tag]),
                    }
                })
                .collect(),
        }
    }
}

/// Load-test instrumentation for the dispatcher, which converts, hashes, and routes every series on one task.
pub(super) struct DispatchProfile {
    /// Conversion and sharding time; its rate is the share of one core the dispatcher spends on them.
    pub partition_nanos: Counter,
    /// Time waiting for full worker input queues to accept a buffer.
    pub blocked_nanos: Counter,
    pub series: Counter,
}

impl DispatchProfile {
    pub fn new(builder: &MetricsBuilder) -> Self {
        Self {
            partition_nanos: builder.register_debug_counter("stateful_metrics_dispatch_partition_nanos_total"),
            blocked_nanos: builder.register_debug_counter("stateful_metrics_dispatch_blocked_nanos_total"),
            series: builder.register_debug_counter("stateful_metrics_dispatch_series_total"),
        }
    }
}

/// Adds the time spent inside each poll of the wrapped future to a counter.
pub(super) struct PollTimer<F> {
    inner: Pin<Box<F>>,
    busy_nanos: Counter,
}

impl<F: Future> PollTimer<F> {
    pub fn new(inner: F, busy_nanos: Counter) -> Self {
        Self {
            inner: Box::pin(inner),
            busy_nanos,
        }
    }
}

impl<F: Future> Future for PollTimer<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let started = Instant::now();
        let output = self.inner.as_mut().poll(cx);
        self.busy_nanos.increment(elapsed_nanos(started));
        output
    }
}

pub(super) fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
