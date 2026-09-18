//! Stable routing of logical series, independent of point values and timestamps.

use foldspace_core::{LogicalMetricBatch, LogicalMetricSeries};
use saluki_common::hash::hash_single_stable;
use saluki_core::data_model::event::Event;

use super::conversion;

pub(super) fn series_hash(series: &LogicalMetricSeries) -> u64 {
    let mut tags: Vec<_> = series.tags().prefix.iter().chain(&series.tags().values).collect();
    tags.sort_unstable();
    tags.dedup();
    let mut resources: Vec<_> = series.resources().iter().map(|r| (&r.kind, &r.name)).collect();
    resources.sort_unstable();
    resources.dedup();
    hash_single_stable((
        series.name(),
        series.metric_type() as u8,
        tags,
        resources,
        series.interval(),
        series.unit(),
        series.source_type_name(),
        series.origin(),
        series.no_index(),
    ))
}

pub(super) fn partition(events: impl IntoIterator<Item = Event>, workers: usize) -> Vec<LogicalMetricBatch> {
    let mut shards: Vec<Vec<LogicalMetricSeries>> = (0..workers).map(|_| Vec::new()).collect();
    for event in events {
        let Event::Metric(metric) = event else { continue };
        let Some(series) = conversion::convert(&metric) else {
            continue;
        };
        if series.name().is_empty() || series.points().is_empty() {
            continue;
        }
        let worker = if workers == 1 {
            0
        } else {
            (series_hash(&series) % workers as u64) as usize
        };
        shards[worker].push(series);
    }
    shards.into_iter().map(LogicalMetricBatch::new).collect()
}
