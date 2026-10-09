//! A frozen copy of the DogStatsD no-aggregation-pipeline split as it was before its splitting moved into this crate.
//!
//! The functions below are the original `agent-data-plane` transform (`dogstatsd_no_agg_split`), copied verbatim with
//! only their imports and visibility adjusted, and one comment reworded to name no product. The passthrough batcher,
//! the component builder, and the memory bounds are left out, since batching is not kernel behavior; what the batcher
//! did to each metric before batching it is kept as [`passthrough_conversion`], and the run loop's routing of one
//! input buffer as [`process_buffer`]. The property tests compare
//! [`TimestampSplit`][crate::timestamp_split::TimestampSplit] and
//! [`split_metric`][crate::timestamp_split::split_metric] with this copy, so that the kernel keeps the native behavior
//! while the legacy component calls the kernel. Do not change it.

use std::{num::NonZeroU64, time::Duration};

use saluki_core::{
    data_model::event::metric::{Metric, MetricValues},
    topology::EventsBuffer,
};

pub(super) fn has_timestamped_metrics(events: &EventsBuffer) -> bool {
    events
        .into_iter()
        .filter_map(|event| event.try_as_metric())
        .any(|metric| metric.values().any_timestamped())
}

pub(super) fn try_split_timestamped_values(mut metric: Metric) -> (Option<Metric>, Option<Metric>) {
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

fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

/// The start of `PassthroughBatcher::push_metric`: what it did to a timestamped metric before batching it.
pub(super) fn passthrough_conversion(metric: Metric, bucket_width_secs: NonZeroU64) -> Metric {
    // Convert counters to rates before we batch them up.
    //
    // This involves specifying the rate interval as the bucket width of the aggregate transform itself, which when
    // you say it out loud is sort of confusing and nonsensical since the whole point is that these are
    // _pre-aggregated_ metrics but we have to match native aggregation's behavior.
    let (context, values, metadata) = metric.into_parts();
    let adjusted_values = counter_values_to_rate(values, bucket_width_secs);
    Metric::from_parts(context, adjusted_values, metadata)
}

/// Where the run loop sent the contents of one input buffer.
pub(super) enum Routed {
    /// The fast path: the whole buffer went to the default output as it was.
    Whole(EventsBuffer),

    /// The slow path: these remainders went to the default output, and these timestamped metrics, converted, to the
    /// passthrough batcher.
    Split {
        default: Vec<Metric>,
        passthrough: Vec<Metric>,
    },
}

/// The run loop's processing of one input buffer, with its awaited sends replaced by collecting what was sent.
pub(super) fn process_buffer(events: EventsBuffer, bucket_width_secs: NonZeroU64) -> Routed {
    // Fast path: when nothing in this buffer is timestamped -- overwhelmingly the common case,
    // since only clients using the timestamp extension produce them -- there's nothing to split
    // out, so forward the buffer as-is instead of moving every event into a new one.
    if !has_timestamped_metrics(&events) {
        return Routed::Whole(events);
    }

    let mut default = Vec::new();
    let mut passthrough = Vec::new();
    for event in events {
        if let Some(metric) = event.try_into_metric() {
            let (maybe_timestamped, maybe_remainder) = try_split_timestamped_values(metric);

            if let Some(timestamped_metric) = maybe_timestamped {
                passthrough.push(passthrough_conversion(timestamped_metric, bucket_width_secs));
            }

            if let Some(remainder) = maybe_remainder {
                default.push(remainder);
            }
        }
    }
    Routed::Split { default, passthrough }
}
