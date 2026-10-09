//! Selecting metrics by name: the name sets that gateways and post-aggregation filtering match metrics against.
//!
//! Each selection here is one comparison a native component makes, built on [`SortedSetMatcher`] so that every list
//! of names is matched by the same implementation:
//!
//! - [`RawNameSet`]: an exact set of raw names, as the multi-region failover allowlist and the autoscaling failover
//!   allowlist compare them.
//! - [`IntakeNameAllowlist`]: exact intake names plus literal intake-name prefixes, as endpoint allowlists compare
//!   them.
//! - [`histogram_statistic_entries`]: the entries of a metric filterlist that name a series emitted for a histogram
//!   statistic, which post-aggregation filtering drops.
//! - [`is_scalar_series`]: the kinds whose series those selections apply to where they are restricted by kind.
//!
//! In a plan, each of them is a predicate of a `split`; the gateways and the filter lower to routing operators.

use saluki_core::data_model::event::metric::MetricValues;

use crate::{
    intake_name::{intake_name, NameBuf},
    matcher::SortedSetMatcher,
    params::HistogramParams,
};

/// Returns `true` for metrics whose values are scalar series: counters, rates, gauges, and sets.
///
/// Histograms and distributions are not: they are sketched or expanded downstream.
pub fn is_scalar_series(values: &MetricValues) -> bool {
    matches!(
        values,
        MetricValues::Counter(_) | MetricValues::Rate(_, _) | MetricValues::Gauge(_) | MetricValues::Set(_)
    )
}

/// An exact set of raw metric names.
///
/// A name matches when it equals an entry, byte for byte: neither names nor entries are normalized. An empty set
/// matches nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RawNameSet {
    names: SortedSetMatcher,
}

impl RawNameSet {
    /// Creates a set of the given names.
    pub fn new<T, I>(names: I) -> Self
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        Self {
            names: SortedSetMatcher::new(names, false),
        }
    }

    /// Returns `true` if the set has no names.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Returns the number of distinct names in the set.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns `true` if `name` is in the set.
    pub fn contains(&self, name: &str) -> bool {
        self.names.matches(name.as_bytes())
    }
}

/// An allowlist of intake names: exact names and literal prefixes.
///
/// A metric matches when its intake name equals one of the names or starts with one of the prefixes. Entries are taken
/// verbatim, never normalized, so an entry that is not a normalized name matches nothing exactly. A metric whose name
/// the intake would reject has no intake name and never matches. The empty prefix matches every metric that has an
/// intake name; an allowlist with neither names nor prefixes matches nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntakeNameAllowlist {
    names: SortedSetMatcher,
    prefixes: SortedSetMatcher,
}

impl IntakeNameAllowlist {
    /// Creates an allowlist of exact intake names and literal intake-name prefixes.
    ///
    /// Duplicate names are kept once, and prefixes are compacted: duplicates and prefixes that start with another
    /// prefix are dropped, since they match nothing more (see
    /// [`compact_prefixes`][crate::matcher::compact_prefixes]).
    pub fn new<N, P>(names: &[N], prefixes: &[P]) -> Self
    where
        N: AsRef<str>,
        P: AsRef<str>,
    {
        Self {
            names: SortedSetMatcher::new(names, false),
            prefixes: SortedSetMatcher::new(prefixes, true),
        }
    }

    /// Returns the number of distinct exact names kept.
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// Returns the number of prefixes kept after compaction.
    pub fn prefix_count(&self) -> usize {
        self.prefixes.len()
    }

    /// Returns `true` if the intake name of the metric named `name` is allowed.
    ///
    /// The name is normalized at most once, into a stack buffer, and only when it is not normalized already.
    pub fn matches(&self, name: &str) -> bool {
        if self.names.is_empty() && self.prefixes.is_empty() {
            return false;
        }
        let mut scratch = NameBuf::new();
        match intake_name(name, &mut scratch) {
            Some(intake_name) => self.names.matches(intake_name) || self.prefixes.matches(intake_name),
            None => false,
        }
    }
}

/// Returns `true` if a metric filterlist entry names the series of a histogram statistic: it ends with
/// `.<suffix>` for one of `suffixes`.
pub fn is_histogram_statistic_entry<S>(entry: &str, suffixes: &[S]) -> bool
where
    S: AsRef<str>,
{
    suffixes.iter().any(|suffix| {
        entry
            .strip_suffix(suffix.as_ref())
            .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Returns the entries of a metric filterlist that name the series of a histogram statistic, in order.
///
/// When aggregation flushes a histogram `<name>`, it emits a series `<name>.<suffix>` per statistic (see
/// [`HistogramParams::suffixes`]). Post-aggregation filtering drops those of the series a filterlist names, so it
/// matches only these entries; the rest of the filterlist applies before aggregation. Entries that merely look like a
/// statistic's series are included too, so a scalar series submitted under such a name is dropped after aggregation as
/// well.
pub fn histogram_statistic_entries<'a, S>(values: &'a [S], histogram: &HistogramParams) -> Vec<&'a str>
where
    S: AsRef<str>,
{
    let suffixes = histogram.suffixes().collect::<Vec<_>>();
    values
        .iter()
        .map(AsRef::as_ref)
        .filter(|value| is_histogram_statistic_entry(value, &suffixes))
        .collect()
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
