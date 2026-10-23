//! Stable routing of logical series, independent of point values and timestamps.

use std::{
    cell::RefCell,
    hash::{Hash, Hasher},
};

use foldspace_core::{LogicalMetricBatch, LogicalMetricSeries, MetricTags};
use saluki_core::data_model::event::Event;
use smallvec::SmallVec;
use twox_hash::XxHash64;

use super::conversion;

pub(super) fn series_hash(series: &LogicalMetricSeries) -> u64 {
    let tags = series.tags();
    // Conversion produces sorted, unique standalone tags; hash them without allocating another vector.
    if tags.prefix.is_empty() && tags.values.iter().is_sorted_by(|a, b| a < b) {
        return hash_with_tags(series, SortedTags(&tags.values));
    }

    let mut tags: Vec<_> = tags.prefix.iter().chain(tags.values.iter()).collect();
    tags.sort_unstable();
    tags.dedup();
    hash_with_tags(series, tags)
}

/// Hashes like `[String]`, so routing matches series hashed before tags could be shared.
struct SortedTags<'a>(&'a MetricTags);

impl Hash for SortedTags<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.0.len());
        for tag in self.0.iter() {
            tag.hash(state);
        }
    }
}

fn hash_with_tags(series: &LogicalMetricSeries, tags: impl Hash) -> u64 {
    let mut resources: SmallVec<[_; 8]> = series.resources().iter().map(|r| (&r.kind, &r.name)).collect();
    resources.sort_unstable();
    resources.dedup();
    // Routing must not change across restarts or releases: the worker that persisted a retry replays it.
    IDENTITY_BYTES.with_borrow_mut(|bytes| {
        bytes.clear();
        (
            series.name(),
            series.metric_type() as u8,
            tags,
            resources.as_slice(),
            series.interval(),
            series.unit(),
            series.source_type_name(),
            series.origin(),
            series.no_index(),
        )
            .hash(&mut ByteSink(bytes));
        XxHash64::oneshot(0, bytes)
    })
}

thread_local! {
    // Streaming XXH64 is slow for the identity's many small writes, so hash its bytes in one pass.
    static IDENTITY_BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct ByteSink<'a>(&'a mut Vec<u8>);

impl Hasher for ByteSink<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> u64 {
        XxHash64::oneshot(0, self.0)
    }
}

pub(super) fn partition(events: impl IntoIterator<Item = Event>, workers: usize) -> Vec<LogicalMetricBatch> {
    let events = events.into_iter();
    // Hashing spreads series about evenly; the slack keeps a shard slightly over its share from doubling.
    let share = events.size_hint().0.div_ceil(workers);
    let mut shards: Vec<Vec<LogicalMetricSeries>> =
        (0..workers).map(|_| Vec::with_capacity(share + share / 4)).collect();
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
