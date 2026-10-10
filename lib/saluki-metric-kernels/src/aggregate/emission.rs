//! Emission filters: records a flush skips because a filter downstream would drop them.

use std::fmt;

use saluki_core::data_model::event::metric::{context::Context, MetricMetadata, MetricValues};
use smallvec::SmallVec;

/// The kind of record a flush emits for a context whose values are not histograms.
///
/// A flush emits one record per such context: counters leave as rates, and every other kind leaves as it is.
/// Histograms expand into several records of different kinds, so they have no single output kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OutputKind {
    /// A rate: a counter's values over the bucket width, or a rate's own values.
    Rate,

    /// A gauge.
    Gauge,

    /// A set.
    Set,

    /// A distribution.
    Distribution,
}

impl OutputKind {
    /// Returns the kind of record a flush emits for a context holding `values`, or `None` for histograms, which expand
    /// into several records.
    pub fn of(values: &MetricValues) -> Option<Self> {
        match values {
            MetricValues::Counter(_) | MetricValues::Rate(..) => Some(Self::Rate),
            MetricValues::Gauge(_) => Some(Self::Gauge),
            MetricValues::Set(_) => Some(Self::Set),
            MetricValues::Distribution(_) => Some(Self::Distribution),
            MetricValues::Histogram(_) => None,
        }
    }
}

/// The index of a filter site: one filter downstream of aggregation whose decisions a flush applies at emission.
///
/// Indices are assigned by the host that builds the filters, which maps each back to the filter it stands for, to
/// attribute the records a flush skipped to that filter.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SiteIndex(u32);

impl SiteIndex {
    /// Creates a site index.
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Returns the numeric value of this index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A filter downstream of aggregation that a flush applies when it emits, skipping the records the filter would drop.
///
/// Skipping a record at emission changes nothing about what aggregation holds: the context is admitted, merged,
/// counted against the context limit, flushed, and counted in `aggregate_flushed_total` exactly as without the filter;
/// only the record itself is not built. The filter must therefore decide by what a record carries when it is emitted,
/// and only by its context, kind, and metadata: never by its values or timestamps.
///
/// The filter itself stays downstream and keeps filtering every record the flush emits. A flush skips a record only
/// when the filter says it drops it, so the filter decides which kinds it is consulted for: [`drops`][Self::drops]
/// returns `false` for a kind whose records it cannot judge at emission. A flush counts what it skipped per
/// [`site`][Self::site] as [`Suppressed`] records, which a host attributes to the filter: each is a record the filter
/// would have received and dropped.
pub trait EmissionFilter: Send + Sync {
    /// Returns `true` if the filter drops a record of `kind` for `context`, with `metadata`.
    ///
    /// This decides every context whose values are not histograms: a flush emits one record for it, of `kind`, with
    /// the context's own name, tags, and metadata.
    fn drops(&self, context: &Context, kind: OutputKind, metadata: &MetricMetadata) -> bool;

    /// Returns `true` if the filter drops every record a histogram held for `context`, with `metadata`, expands into.
    ///
    /// A histogram expands into several records, a distribution copy and one record per statistic, each with a name and
    /// kind of its own but the context's host and tags, and the context's metadata (the count's without its unit). A
    /// filter that decides by none of name, kind, or metadata decides them all alike, and may return that decision
    /// here; the flush then skips the whole expansion. The default returns `false`: every histogram expands, and the
    /// filter downstream judges each of its records.
    fn drops_histogram(&self, context: &Context, metadata: &MetricMetadata) -> bool {
        let _ = (context, metadata);
        false
    }

    /// Returns the site this filter stands for, which the records it skips are attributed to.
    fn site(&self) -> SiteIndex;
}

/// The records a flush skipped because an [`EmissionFilter`] drops them, and the points those records carried.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Suppressed {
    /// The records skipped: one per skipped context that is not a histogram, and one per record of each skipped
    /// histogram's expansion.
    pub records: u64,

    /// The points the skipped records would have carried.
    pub points: u64,
}

impl Suppressed {
    /// Returns `true` if nothing was skipped.
    pub fn is_empty(&self) -> bool {
        self.records == 0 && self.points == 0
    }

    /// Adds `other` to these counts.
    pub fn add(&mut self, other: Suppressed) {
        self.records += other.records;
        self.points += other.points;
    }
}

/// [`Suppressed`] counts per site, each site at most once, in the order the sites were first counted.
pub type SuppressedBySite = SmallVec<[(SiteIndex, Suppressed); 1]>;

/// Adds `suppressed` to the counts of `site` in `counts`; nothing, if `suppressed` is empty.
pub(super) fn count_suppressed(counts: &mut SuppressedBySite, site: SiteIndex, suppressed: Suppressed) {
    if suppressed.is_empty() {
        return;
    }
    match counts.iter_mut().find(|(counted, _)| *counted == site) {
        Some((_, total)) => total.add(suppressed),
        None => counts.push((site, suppressed)),
    }
}

impl fmt::Debug for dyn EmissionFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmissionFilter").field("site", &self.site()).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn every_kind_but_histograms_leaves_as_one_record_of_one_kind() {
        let cases = [
            (MetricValues::counter(1.0), Some(OutputKind::Rate)),
            (MetricValues::rate(1.0, Duration::from_secs(10)), Some(OutputKind::Rate)),
            (MetricValues::gauge(1.0), Some(OutputKind::Gauge)),
            (MetricValues::set("member"), Some(OutputKind::Set)),
            (MetricValues::distribution(&[1.0][..]), Some(OutputKind::Distribution)),
            (MetricValues::histogram([1.0]), None),
        ];
        for (values, expected) in cases {
            assert_eq!(OutputKind::of(&values), expected, "{}", values.as_str());
        }
    }
}
