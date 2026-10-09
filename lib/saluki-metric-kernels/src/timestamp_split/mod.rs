//! Timestamp splitting, which separates the timestamped points of metrics from their untimestamped points.
//!
//! DogStatsD clients can attach an explicit timestamp to a point. Timestamped points are taken to be aggregated
//! already, so they bypass aggregation: [`split_metric`] separates them, and turns timestamped counters into rates over
//! the aggregation window, as aggregation would have. Untimestamped points continue to aggregation unchanged.
//!
//! [`TimestampSplit`] is the kernel, with two ports: [`DEFAULT_PORT`] for untimestamped points and
//! [`PASSTHROUGH_PORT`] for timestamped ones. When no metric of a batch has a timestamped point, which is by far the
//! common case, the whole batch goes to the default port untouched.
//!
//! Batching passthrough metrics across input batches, which the native transform does to send larger buffers on, is
//! not part of the kernel: a host decides how its output is buffered.

use std::{num::NonZeroU64, time::Duration};

use saluki_core::{
    data_model::event::{
        metric::{Metric, MetricValues, ScalarPoints},
        Event,
    },
    execution::{Kernel, LossClass, PortIndex, Progress, Turn},
    topology::EventsBuffer,
};

mod telemetry;
pub use self::telemetry::TimestampSplitTelemetry;

/// The port for untimestamped points, which continue to aggregation. It is the primary port.
pub const DEFAULT_PORT: PortIndex = PortIndex::PRIMARY;

/// The port for timestamped points, which bypass aggregation.
pub const PASSTHROUGH_PORT: PortIndex = PortIndex::new(1);

/// Returns `true` if any metric in `events` has a timestamped point.
///
/// When none has, a batch needs no splitting: every metric in it goes to the default port as it is.
pub fn has_timestamped_metrics(events: &EventsBuffer) -> bool {
    events
        .into_iter()
        .filter_map(|event| event.try_as_metric())
        .any(|metric| metric.values().any_timestamped())
}

/// Splits a metric into its timestamped and its untimestamped points, as `(timestamped, untimestamped)`.
///
/// A metric whose points are all timestamped, or none, is returned whole on its side. A metric with both kinds of
/// point keeps its untimestamped points, and its timestamped points move to a new metric with the same context and
/// metadata. Values are not converted; see [`split_metric`].
pub fn split_timestamped(mut metric: Metric) -> (Option<Metric>, Option<Metric>) {
    if metric.values().all_timestamped() {
        (Some(metric), None)
    } else if metric.values().any_timestamped() {
        // Only _some_ of the values are timestamped, so we'll split the timestamped values into a new metric.
        let new_metric_values = metric.values_mut().split_timestamped();
        let new_metric = Metric::from_parts(metric.context().clone(), new_metric_values, metric.metadata().clone());

        (Some(new_metric), Some(metric))
    } else {
        // No timestamped values, so this metric isn't part of the no-aggregation pipeline.
        (None, Some(metric))
    }
}

/// Turns counter values into rates over `interval_secs` seconds, and returns every other kind of value unchanged,
/// including rates, which keep their own interval.
pub fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

/// Prepares a metric of timestamped points to bypass aggregation: a counter becomes a rate over the aggregation
/// window, `window_secs` seconds, as aggregation emits counters.
pub fn to_passthrough(metric: Metric, window_secs: NonZeroU64) -> Metric {
    // This involves specifying the rate interval as the bucket width of the aggregate transform itself, which when
    // you say it out loud is sort of confusing and nonsensical since the whole point is that these are
    // _pre-aggregated_ metrics but we have to match the native aggregation's behavior.
    let (context, values, metadata) = metric.into_parts();
    let adjusted_values = counter_values_to_rate(values, window_secs);
    Metric::from_parts(context, adjusted_values, metadata)
}

/// Prepares a metric of timestamped points to bypass aggregation in place, as [`to_passthrough`] does: a counter
/// becomes a rate over the aggregation window, `window_secs` seconds. Nothing is allocated.
pub fn prepare_passthrough(metric: &mut Metric, window_secs: NonZeroU64) {
    let values = metric.values_mut();
    if let MetricValues::Counter(points) = values {
        let points = std::mem::replace(
            points,
            ScalarPoints::from_iter(std::iter::empty::<(Option<NonZeroU64>, f64)>()),
        );
        *values = MetricValues::rate(points, Duration::from_secs(window_secs.get()));
    }
}

/// Splits a metric by timestamp, as `(passthrough, default)`: its timestamped points, ready to bypass aggregation (see
/// [`to_passthrough`]), and its untimestamped points, unchanged.
pub fn split_metric(metric: Metric, window_secs: NonZeroU64) -> (Option<Metric>, Option<Metric>) {
    let (timestamped, untimestamped) = split_timestamped(metric);
    (
        timestamped.map(|metric| to_passthrough(metric, window_secs)),
        untimestamped,
    )
}

/// The timestamp split kernel: routes untimestamped points to [`DEFAULT_PORT`] and timestamped points, prepared to
/// bypass aggregation, to [`PASSTHROUGH_PORT`].
///
/// A batch without timestamped points goes to the default port whole, without touching its events. Otherwise, each
/// metric is split with [`split_metric`], in order, so a metric with both kinds of point leaves on both ports. An event
/// that is not a metric cannot be split, and is counted as invalid loss.
///
/// For every batch that sends metrics to the passthrough port, the kernel records the metrics and one passthrough
/// flush in its [telemetry][TimestampSplitTelemetry].
pub struct TimestampSplit {
    window_secs: NonZeroU64,
    telemetry: TimestampSplitTelemetry,
}

impl TimestampSplit {
    /// Creates a timestamp split whose passthrough counters become rates over `window_secs` seconds, the aggregation
    /// window.
    pub fn new(window_secs: NonZeroU64, telemetry: TimestampSplitTelemetry) -> Self {
        Self { window_secs, telemetry }
    }
}

impl Kernel for TimestampSplit {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        // Count what leaves on each port, and whether any metric has points of both kinds.
        let mut timestamped = 0;
        let mut invalid = 0;
        let mut mixed = false;
        for event in &*input {
            match event.try_as_metric() {
                Some(metric) if metric.values().any_timestamped() => {
                    timestamped += 1;
                    mixed |= !metric.values().all_timestamped();
                }
                Some(_) => {}
                None => invalid += 1,
            }
        }

        // Fast path: when nothing in this buffer is timestamped -- overwhelmingly the common case, since only clients
        // using the timestamp extension produce them -- there's nothing to split out, so the buffer leaves as it is,
        // events other than metrics included.
        if timestamped == 0 {
            turn.route_all(DEFAULT_PORT);
            return Progress::Done;
        }

        if !mixed {
            // Every metric leaves whole on one port, prepared in place, in order. When none stays, the input itself
            // leaves on the passthrough port; otherwise the untimestamped metrics stay in the input and leave on the
            // default port without being touched, and the rest move to a buffer of exactly their number.
            let window_secs = self.window_secs;
            let mut invalid_loss = 0;
            let mut prepare = |event: &mut Event| match event.try_as_metric_mut() {
                Some(metric) => {
                    prepare_passthrough(metric, window_secs);
                    true
                }
                None => {
                    invalid_loss += 1;
                    false
                }
            };
            if timestamped + invalid == input.len() {
                input.retain(&mut prepare);
                turn.route_all(PASSTHROUGH_PORT);
            } else {
                let mut moved = EventsBuffer::with_capacity(timestamped + invalid);
                input.move_into(&mut moved, |event| {
                    event
                        .try_as_metric()
                        .is_none_or(|metric| metric.values().any_timestamped())
                });
                moved.retain(&mut prepare);
                turn.push_buffer(PASSTHROUGH_PORT, moved);
            }
            if invalid_loss > 0 {
                turn.counters().record_loss(LossClass::Invalid, invalid_loss, 0);
            }
            self.telemetry.increment_passthrough_metrics(timestamped as u64);
            self.telemetry.increment_passthrough_flushes();
            return Progress::Done;
        }

        // A metric with both kinds of point leaves on both ports, so every metric is split in turn.
        let events = std::mem::replace(input, EventsBuffer::with_capacity(0));
        turn.reserve(DEFAULT_PORT, events.len());
        turn.reserve(PASSTHROUGH_PORT, timestamped);
        let mut passthrough = 0u64;
        for event in events {
            let Some(metric) = event.try_into_metric() else {
                turn.counters().record_loss(LossClass::Invalid, 1, 0);
                continue;
            };
            let (timestamped, untimestamped) = split_metric(metric, self.window_secs);
            if let Some(timestamped) = timestamped {
                turn.push(PASSTHROUGH_PORT, Event::Metric(timestamped));
                passthrough += 1;
            }
            if let Some(untimestamped) = untimestamped {
                turn.push(DEFAULT_PORT, Event::Metric(untimestamped));
            }
        }

        if passthrough > 0 {
            self.telemetry.increment_passthrough_metrics(passthrough);
            self.telemetry.increment_passthrough_flushes();
        }
        Progress::Done
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
