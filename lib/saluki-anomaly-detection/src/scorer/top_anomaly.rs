//! Bounded retention of the strongest recent anomalies for episode contributor attribution.
//!
//! This is a port of the Go `topAnomalyBuffer` from `observer/impl/anomaly_scorer.go`. The buffer keeps a
//! bounded approximation of the strongest anomalies over the previous five minutes so that a scorer
//! episode's start event can report which metrics contributed. Entries deliberately store no descriptor,
//! tags, or detector payload: those are resolved only for final reporting.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use super::anomaly_scorer::{contributor_weight, ScorerContributor};
use crate::config::AnomalyScorerConfig;
use crate::identity::QueryHandle;
use crate::model::Anomaly;

/// How long a retained anomaly occurrence stays eligible for contributor attribution, in seconds.
pub const TOP_ANOMALY_WINDOW_SECS: i64 = 5 * 60;

/// The retained-occurrence capacity multiplier over the displayed contributor count.
pub const CONTRIBUTOR_BUFFER_MULTIPLIER: usize = 10;

/// One storage-backed anomaly occurrence retained for scorer episode attribution.
///
/// The timestamp is the **effective** scorer second of the anomaly (late, clamped anomalies use
/// `lastAdvancedSecond + 1`), which is what the five-minute expiration compares against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TopAnomaly {
    handle: QueryHandle,
    timestamp_sec: i64,
    weight: f64,
}

// Order by weakness so the `BinaryHeap` root is the weakest retained occurrence, mirroring the Go min-heap
// rooted at the weakest entry. Go's `topAnomalyBefore` defines strength: higher weight first, then newer
// timestamp, then smaller ref, then smaller aggregate; equal elements compare `Equal`, so a candidate must be
// strictly stronger than the root to replace it. The comparison is a total order — `total_cmp` orders NaN
// consistently — so `PartialEq`/`Eq` are defined through it rather than derived from the raw `f64` equality.
impl PartialEq for TopAnomaly {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for TopAnomaly {}

impl PartialOrd for TopAnomaly {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopAnomaly {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .weight
            .total_cmp(&self.weight)
            .then_with(|| other.timestamp_sec.cmp(&self.timestamp_sec))
            .then_with(|| self.handle.series.cmp(&other.handle.series))
            .then_with(|| self.handle.aggregate.cmp(&other.handle.aggregate))
    }
}

/// A bounded approximation of the strongest anomalies over the previous five minutes.
///
/// Entries form a min-heap with the weakest retained anomaly at the root. When the buffer is full, a new
/// occurrence is admitted only if it is strictly stronger than the weakest retained one.
#[derive(Debug)]
pub(crate) struct TopAnomalyBuffer {
    entries: BinaryHeap<TopAnomaly>,
    capacity: usize,
}

impl TopAnomalyBuffer {
    /// Creates a buffer that retains `display_count * 10` occurrences.
    pub(crate) fn new(display_count: usize) -> Self {
        Self {
            entries: BinaryHeap::with_capacity(display_count * CONTRIBUTOR_BUFFER_MULTIPLIER),
            capacity: display_count * CONTRIBUTOR_BUFFER_MULTIPLIER,
        }
    }

    /// Expires stale entries, then considers each anomaly from a finalized scorer second.
    ///
    /// `sec` is data time, never wall-clock time. Anomalies without a storage handle are skipped: episode
    /// attribution is keyed by series handle.
    pub(crate) fn update(&mut self, sec: i64, anomalies: &[Anomaly], config: &AnomalyScorerConfig) {
        self.expire(sec);
        for anomaly in anomalies {
            if let Some(handle) = anomaly.series_ref {
                self.insert(TopAnomaly {
                    handle,
                    timestamp_sec: sec,
                    weight: contributor_weight(anomaly, config),
                });
            }
        }
    }

    /// Drops entries whose effective second is at or before `sec - 5 minutes`.
    fn expire(&mut self, sec: i64) {
        let cutoff = sec - TOP_ANOMALY_WINDOW_SECS;
        let retained: Vec<TopAnomaly> = self
            .entries
            .iter()
            .filter(|entry| entry.timestamp_sec > cutoff)
            .copied()
            .collect();
        self.entries = BinaryHeap::from(retained);
    }

    /// Inserts one occurrence, replacing the weakest retained entry when the buffer is full.
    fn insert(&mut self, candidate: TopAnomaly) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() < self.capacity {
            self.entries.push(candidate);
            return;
        }
        if let Some(&root) = self.entries.peek() {
            // Reject unless strictly stronger than the weakest retained entry; equal-weight tail
            // candidates lose, matching Go's `!topAnomalyBefore` rejection.
            if candidate.cmp(&root) != Ordering::Less {
                return;
            }
        }
        self.entries.pop();
        self.entries.push(candidate);
    }

    /// Returns the number of retained occurrences.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether no occurrences are retained.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clears all retained occurrences.
    pub(crate) fn reset(&mut self) {
        self.entries.clear();
    }

    /// Returns the highest-weight metrics represented in the retained occurrences.
    ///
    /// Shares are normalized across the returned top `max_items` entries (not the global weight total).
    /// Returns an empty vector when `max_items` is zero, nothing is retained, or all weights sum to zero.
    pub(crate) fn contributors(&self, max_items: usize) -> Vec<ScorerContributor> {
        if max_items == 0 || self.entries.is_empty() {
            return Vec::new();
        }

        let mut totals: HashMap<QueryHandle, f64> = HashMap::with_capacity(self.entries.len());
        let mut total = 0.0;
        for entry in &self.entries {
            *totals.entry(entry.handle).or_insert(0.0) += entry.weight;
            total += entry.weight;
        }
        if total == 0.0 {
            return Vec::new();
        }

        let mut contributors: Vec<ScorerContributor> = totals
            .into_iter()
            .map(|(handle, weight)| ScorerContributor {
                handle,
                weight,
                share: 0.0,
            })
            .collect();
        // Weight descending, then ref ascending, then aggregate ascending — Go's stable rank order.
        contributors.sort_by(|a, b| {
            b.weight
                .total_cmp(&a.weight)
                .then_with(|| a.handle.series.cmp(&b.handle.series))
                .then_with(|| a.handle.aggregate.cmp(&b.handle.aggregate))
        });
        contributors.truncate(max_items);
        let selected_total: f64 = contributors.iter().map(|contributor| contributor.weight).sum();
        for contributor in &mut contributors {
            contributor.share = contributor.weight / selected_total;
        }
        contributors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Aggregate, SeriesDescriptor, SeriesRef};
    use crate::scorer::anomaly_scorer::LEVEL_WEIGHTS;

    fn handle(ref_id: u64) -> Option<QueryHandle> {
        Some(QueryHandle::new(SeriesRef::new(ref_id), Aggregate::Average))
    }

    fn anomaly(detector: &str, timestamp_sec: i64, score: Option<f64>, series_ref: Option<QueryHandle>) -> Anomaly {
        Anomaly {
            anomaly_type: crate::model::AnomalyType::Metric,
            series: SeriesDescriptor::new("test", "series", None, Vec::new(), Aggregate::None),
            series_ref,
            detector_name: detector.to_string(),
            context: None,
            timestamp_sec,
            score,
            sampling_interval_sec: 0,
            evidence: None,
        }
    }

    fn test_config() -> AnomalyScorerConfig {
        AnomalyScorerConfig::production()
    }

    #[test]
    fn expires_at_five_minutes() {
        let config = test_config();
        let mut buffer = TopAnomalyBuffer::new(10);
        buffer.update(1000, &[anomaly("holt_residual", 1000, Some(40.0), handle(42))], &config);

        // One catch-up advance reaches the five-minute boundary and expires the entry.
        buffer.update(1300, &[], &config);
        assert!(buffer.is_empty(), "expected contributor to expire after five minutes");

        // An entry one second inside the boundary survives.
        buffer.update(1000, &[anomaly("holt_residual", 1000, Some(40.0), handle(42))], &config);
        buffer.update(1299, &[], &config);
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn uses_scorer_weights() {
        let config = test_config();
        let mut buffer = TopAnomalyBuffer::new(10);
        buffer.update(
            1000,
            &[
                anomaly("holt_residual", 1000, Some(20.0), handle(42)),
                anomaly("holt_residual", 1000, Some(40.0), handle(7)),
                anomaly("holt_residual", 1000, Some(40.0), handle(3)),
            ],
            &config,
        );

        assert_eq!(buffer.len(), 3);
        let mut weights = HashMap::new();
        for entry in buffer.entries.iter() {
            weights.insert(entry.handle.series.raw(), entry.weight);
        }
        // Score 40 is at holt_residual's xhigh boundary (35): weight 3.0. Score 20 is exactly at the
        // high boundary: interpolated weight lands on 2.0.
        assert_eq!(weights[&3], LEVEL_WEIGHTS[4]);
        assert_eq!(weights[&7], LEVEL_WEIGHTS[4]);
        let expected = contributor_weight(&anomaly("holt_residual", 1000, Some(20.0), None), &config);
        assert!((weights[&42] - expected).abs() < 1e-12);
        assert!((expected - 2.0).abs() < 1e-12);
    }

    #[test]
    fn rejects_weak_candidates_and_replaces_weakest() {
        let config = test_config();
        let mut buffer = TopAnomalyBuffer::new(2);
        for ref_id in 1..=buffer.capacity as u64 {
            buffer.update(
                1000,
                &[anomaly("holt_residual", 1000, Some(8.0), handle(ref_id))],
                &config,
            );
        }
        // Equal-weight, equal-timestamp tail candidate: rejected (not strictly stronger than the root).
        buffer.update(1000, &[anomaly("holt_residual", 1000, Some(8.0), handle(99))], &config);
        // Stronger candidate replaces the weakest retained entry.
        buffer.update(
            1001,
            &[anomaly("holt_residual", 1001, Some(40.0), handle(100))],
            &config,
        );

        assert_eq!(buffer.len(), buffer.capacity);
        let mut saw_strong = false;
        for entry in buffer.entries.iter() {
            assert_ne!(entry.handle.series.raw(), 99, "weak tail candidate must be rejected");
            if entry.handle.series.raw() == 100 {
                saw_strong = true;
                assert_eq!(entry.weight, LEVEL_WEIGHTS[4]);
            }
        }
        assert!(saw_strong, "expected stronger anomaly to be retained");
    }

    #[test]
    fn capacity_tracks_display_count() {
        let buffer = TopAnomalyBuffer::new(7);
        assert_eq!(buffer.capacity, 7 * CONTRIBUTOR_BUFFER_MULTIPLIER);
    }

    #[test]
    fn contributors_aggregate_shares_and_limit_results() {
        // Ports the Go test, which seeds entries directly with synthetic weights (3 and 2 for the
        // average handle, 3 for count, 2 for sum) and expects shares 5/8 and 3/8 after truncation.
        let average = QueryHandle::new(SeriesRef::new(10), Aggregate::Average);
        let count = QueryHandle::new(SeriesRef::new(20), Aggregate::Count);
        let total = QueryHandle::new(SeriesRef::new(30), Aggregate::Sum);

        let mut buffer = TopAnomalyBuffer::new(10);
        buffer.entries = BinaryHeap::from([
            TopAnomaly {
                handle: average,
                timestamp_sec: 1000,
                weight: 3.0,
            },
            TopAnomaly {
                handle: average,
                timestamp_sec: 1000,
                weight: 2.0,
            },
            TopAnomaly {
                handle: count,
                timestamp_sec: 1000,
                weight: 3.0,
            },
            TopAnomaly {
                handle: total,
                timestamp_sec: 1000,
                weight: 2.0,
            },
        ]);

        let contributors = buffer.contributors(2);
        assert_eq!(contributors.len(), 2);
        assert_eq!(contributors[0].handle, average);
        assert_eq!(contributors[0].weight, 5.0);
        assert!((contributors[0].share - 0.625).abs() < 1e-9, "share 5/8");
        assert_eq!(contributors[1].handle, count);
        assert_eq!(contributors[1].weight, 3.0);
        assert!((contributors[1].share - 0.375).abs() < 1e-9, "share 3/8");
    }

    #[test]
    fn contributors_return_empty_when_no_entries_or_zero_weight() {
        let buffer = TopAnomalyBuffer::new(10);
        assert!(buffer.contributors(2).is_empty());

        let mut buffer = TopAnomalyBuffer::new(10);
        buffer.entries = BinaryHeap::from([TopAnomaly {
            handle: QueryHandle::new(SeriesRef::new(1), Aggregate::Average),
            timestamp_sec: 1000,
            weight: 0.0,
        }]);
        assert!(buffer.contributors(2).is_empty());
    }
}
