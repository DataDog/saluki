//! Report shapes the two differential oracles hand to the Antithesis SDK.
//!
//! Failures only. A context that agrees contributes nothing, so a healthy run carries no per-context
//! data. The counts alongside are what separate a healthy run from one that compared nothing.

use serde::Serialize;

use crate::capture::{Context, Target};
use crate::series::Skip;

/// Most failures any report lists. `found` against `listed` keeps the truncation visible.
pub(crate) const SAMPLE_LIMIT: usize = 10;

/// A context on one lane and not the other, flattened for the assertion details.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DivergingOut {
    pub(crate) lane: Target,
    pub(crate) name: String,
    pub(crate) tagset: Vec<String>,
    pub(crate) kind: String,
    pub(crate) age_secs: i64,
}

/// What the contexts oracle asserts on.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ContextsReport {
    /// Contexts present on at least one lane. Zero means the run compared nothing.
    pub(crate) compared: usize,
    /// Members of the symmetric difference.
    pub(crate) diverged: usize,
    /// Members observed on the ADP lane only. Counted over the whole difference, not the sample.
    pub(crate) adp_only: usize,
    /// Members observed on the Datadog Agent lane only. Counted over the whole difference.
    pub(crate) agent_only: usize,
    /// Distinct metric names among `adp_only`. Separates one name diverging many times from many
    /// names diverging once, which have different causes.
    pub(crate) adp_only_names: usize,
    /// Distinct metric names among `agent_only`.
    pub(crate) agent_only_names: usize,
    /// Members that have sat in the difference longer than the flush budget.
    pub(crate) overdue: usize,
    pub(crate) acceptable_flush_delay_secs: i64,
    pub(crate) listed: usize,
    pub(crate) sample: Vec<DivergingOut>,
}

/// Skipped contexts by reason. Summed with `compared` this reconciles to the whole population, so no
/// context is lost between the store and the verdict.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct SkipCounts {
    pub(crate) no_overlap: usize,
    pub(crate) short_series: usize,
    pub(crate) kind_other: usize,
    pub(crate) span_too_wide: usize,
}

impl SkipCounts {
    pub(crate) fn record(&mut self, skip: Skip) {
        match skip {
            Skip::NoOverlap => self.no_overlap += 1,
            Skip::ShortSeries => self.short_series += 1,
            Skip::KindOther => self.kind_other += 1,
            Skip::SpanTooWide => self.span_too_wide += 1,
        }
    }

    pub(crate) fn total(self) -> usize {
        self.no_overlap + self.short_series + self.kind_other + self.span_too_wide
    }
}

/// One context whose lanes diverged, with the distance that decided it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct FailureOut {
    pub(crate) name: String,
    pub(crate) tagset: Vec<String>,
    pub(crate) kind: String,
    /// Absent for a grid mismatch, which is a failure without a meaningful distance.
    pub(crate) distance: Option<f64>,
    pub(crate) quantization_mismatch: bool,
}

/// What the series oracle asserts on.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SeriesReport {
    /// Contexts that yielded a distance.
    pub(crate) compared: usize,
    /// Contexts at or above the threshold, plus grid mismatches.
    pub(crate) failed: usize,
    /// Failures carried in `failures`, below `failed` when truncated.
    pub(crate) listed: usize,
    pub(crate) skipped: SkipCounts,
    /// `compared` plus every skip reason. Reconciles to the whole population, so no context is lost
    /// between the store and the verdict.
    pub(crate) population: usize,
    pub(crate) bucket_width: i64,
    pub(crate) leash_width: usize,
    pub(crate) equivalence_threshold: f64,
    pub(crate) failures: Vec<FailureOut>,
}

impl SeriesReport {
    /// Whether the run compared nothing. An empty failure list means agreement only when something
    /// was compared.
    pub(crate) fn vacuous(&self) -> bool {
        self.compared == 0
    }
}

/// Flatten a context for the wire. `tagset` is already sorted, so the output is replay-stable.
pub(crate) fn flatten(context: &Context) -> (String, Vec<String>, String) {
    (
        context.name.clone(),
        context.tagset.iter().cloned().collect(),
        format!("{:?}", context.kind).to_lowercase(),
    )
}
