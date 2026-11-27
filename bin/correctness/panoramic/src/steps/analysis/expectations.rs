//! What each side of a comparison must hold, checked before the sides are compared.
//!
//! Two sides that hold nothing are equal, and so are two sides that hold the same unrelated records. A case lists what
//! each side must hold. The analysis checks each side against the list first, and names the side and the expectation
//! that the side leaves unmet.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::num::NonZeroUsize;

use saluki_error::{generic_error, GenericError};
use serde::{
    de::{self, value::MapAccessDeserializer},
    Deserialize, Deserializer,
};
use stele::Span;
use tracing::error;

use crate::steps::intake::metrics::NormalizedMetrics;
use crate::steps::intake::MetricCriteria;

/// What an unmet expectation calls the side that holds the baseline's data.
pub(super) const BASELINE: &str = "baseline";

/// What an unmet expectation calls the side that holds the comparison's data.
pub(super) const COMPARISON: &str = "comparison";

/// The maximum number of series with the expected name, or span names of the expected service, that an unmet
/// expectation lists.
const MAX_LISTED: usize = 3;

/// The maximum number of characters of a tag value that an unmet expectation shows.
const MAX_SHOWN_CHARS: usize = 160;

/// What each side must hold of metrics. The `metrics` section of a `compare` step sets these fields.
///
/// Every field is optional. A series count is the number of metric contexts, by type, that remain after the
/// internal-name filter, so internal telemetry never counts toward it.
#[derive(Clone, Debug, Default)]
pub struct MetricsExpectations {
    /// The minimum number of series that each side is expected to hold after the internal-name filter.
    pub min_series: Option<NonZeroUsize>,

    /// Criteria that one series of each side must meet, one entry for each. A criterion names a metric, and it can add
    /// tags, a type and a value, as `intake_has_metric` does.
    pub series: Vec<MetricCriteria>,
}

impl MetricsExpectations {
    /// Checks both sides, which the internal-name filter has already cleaned.
    ///
    /// # Errors
    ///
    /// If a side leaves an expectation unmet, an error is returned with one line of details for each unmet
    /// expectation. Each line names the side.
    pub fn check(
        &self, baseline: &NormalizedMetrics, comparison: &NormalizedMetrics,
    ) -> Result<(), (GenericError, Vec<String>)> {
        require_met(self.unmet(BASELINE, baseline), self.unmet(COMPARISON, comparison))
    }

    /// Returns a line for each expectation that the series of `side` leave unmet.
    fn unmet(&self, side: &str, series: &NormalizedMetrics) -> Vec<String> {
        let mut unmet = Vec::new();

        if let Some(min_series) = self.min_series.filter(|min| series.len() < min.get()) {
            unmet.push(format!(
                "The {} holds {} series after the internal-name filter, but `min_series` is {}.",
                side,
                series.len(),
                min_series
            ));
        }

        for (idx, criteria) in self.series.iter().enumerate() {
            if !criteria.is_met_by(series) {
                unmet.push(format!(
                    "The {} holds no series that meets `series[{}]` ({}): {}.",
                    side,
                    idx,
                    criteria.describe(),
                    describe_named(criteria, series)
                ));
            }
        }

        unmet
    }
}

/// What each side must hold of traces. The `traces` section of a `compare` step sets these fields.
///
/// Every field is optional. A trace is a distinct trace ID among the spans of a side.
#[derive(Clone, Debug, Default)]
pub struct TracesExpectations {
    /// The minimum number of traces that each side is expected to hold.
    pub min_traces: Option<NonZeroUsize>,

    /// The maximum number of traces that each side is expected to hold.
    pub max_traces: Option<NonZeroUsize>,

    /// The minimum number of spans that each side is expected to hold.
    pub min_spans: Option<NonZeroUsize>,

    /// The maximum number of spans that each side is expected to hold.
    pub max_spans: Option<NonZeroUsize>,

    /// Spans that each side must hold, one entry for each. A span of the side must meet every field of an entry.
    pub spans: Vec<ExpectedSpan>,

    /// Spans that neither side may hold, one entry for each. No span of a side may meet every field of an entry.
    pub absent: Vec<AbsentSpan>,

    /// The minimum number of APM stats groups that each side is expected to hold. A group is one aggregation key,
    /// whatever its buckets.
    ///
    /// It applies only when the step compares stats.
    pub min_stats_groups: Option<NonZeroUsize>,
}

/// What one side of a trace comparison holds.
pub struct HeldTraces<'a> {
    /// The spans of the side, by span ID.
    pub spans: &'a HashMap<u64, Span>,

    /// The number of distinct trace IDs among the spans.
    pub traces: usize,

    /// The number of groups of APM stats.
    pub stats_groups: usize,
}

impl TracesExpectations {
    /// Checks both sides.
    ///
    /// # Errors
    ///
    /// If a side leaves an expectation unmet, an error is returned with one line of details for each unmet
    /// expectation. Each line names the side.
    pub fn check(
        &self, baseline: &HeldTraces<'_>, comparison: &HeldTraces<'_>,
    ) -> Result<(), (GenericError, Vec<String>)> {
        require_met(self.unmet(BASELINE, baseline), self.unmet(COMPARISON, comparison))
    }

    /// Returns a line for each expectation that `held` leaves unmet.
    fn unmet(&self, side: &str, held: &HeldTraces<'_>) -> Vec<String> {
        let mut unmet = Vec::new();

        let counts = [
            ("traces", "min_traces", held.traces, self.min_traces),
            ("spans", "min_spans", held.spans.len(), self.min_spans),
            (
                "stats groups",
                "min_stats_groups",
                held.stats_groups,
                self.min_stats_groups,
            ),
        ];
        for (what, key, found, min) in counts {
            if let Some(min) = min.filter(|min| found < min.get()) {
                unmet.push(format!(
                    "The {} holds {} {}, but `{}` is {}.",
                    side, found, what, key, min
                ));
            }
        }

        let limits = [
            ("traces", "max_traces", held.traces, self.max_traces),
            ("spans", "max_spans", held.spans.len(), self.max_spans),
        ];
        for (what, key, found, max) in limits {
            if let Some(max) = max.filter(|max| found > max.get()) {
                unmet.push(format!(
                    "The {} holds {} {}, but `{}` is {}.",
                    side, found, what, key, max
                ));
            }
        }

        for (idx, expected) in self.spans.iter().enumerate() {
            let met = expected.is_met_by_side(held.spans);
            if expected.applies_to(side) {
                if !met {
                    unmet.push(format!(
                        "The {} holds no span that meets `spans[{}]` ({}): {}.",
                        side,
                        idx,
                        expected.describe(),
                        expected.describe_held(held.spans)
                    ));
                }
            } else if met {
                // An entry for one side also holds on the other, so `side` no longer separates them. A known
                // difference goes stale when the gap closes, and this makes the entry go stale with it.
                unmet.push(format!(
                    "The {} meets `spans[{}]` ({}) too, but the entry is `side: {}`. Drop `side`, so that the entry \
                     holds both sides.",
                    side,
                    idx,
                    expected.describe(),
                    expected.side_name()
                ));
            }
        }

        for (idx, unexpected) in self.absent.iter().enumerate() {
            let held = held.spans.values().filter(|span| unexpected.is_met_by(span)).count();
            if held > 0 {
                unmet.push(format!(
                    "The {} holds {} span(s) that meet `absent[{}]` ({}), but it must hold none.",
                    side,
                    held,
                    idx,
                    unexpected.describe()
                ));
            }
        }

        unmet
    }
}

/// A span that a side must hold, by its service, its operation name, its resource, its `meta` tags and its chunk.
///
/// The service, the name and the resource select the spans of a side, and the other fields say what every selected
/// span must hold: the `meta` tags of the span, and the `priority`, the `dropped_trace` mark and the `chunk_tags` of the
/// trace chunk that holds it. A side meets the entry when it selects at least one span and every selected span meets
/// every one of them, so a second span that leaks what the first one scrubbed fails it. A failure names the fields
/// that a selected span holds wrong, so a field that a side should have scrubbed shows its value.
///
/// An entry applies to both sides, unless it sets `side`. A side-specific entry holds one side to a value that the
/// other side does not yet produce, while a known difference covers the gap between them. When the other side meets the
/// entry too, the entry is stale, and the step fails until the entry drops `side`.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "ExpectedSpanFields")]
pub struct ExpectedSpan {
    service: String,
    name: Option<String>,
    resource: Option<String>,
    meta: BTreeMap<String, MetaExpectation>,
    priority: Option<i32>,
    dropped_trace: Option<bool>,
    chunk_tags: BTreeMap<String, MetaExpectation>,
    side: Option<Side>,
}

/// A span that neither side may hold, by its service, its operation name and its resource.
///
/// A span meets the entry when it has the service, and the name and the resource if the entry sets them. The entry
/// takes no `meta`, no chunk field and no `side`: `not_contains` would turn around, since it holds a tag that is
/// present and lacks the value, and a span that one side drops is one that both sides drop.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "ExpectedSpanFields")]
pub struct AbsentSpan(ExpectedSpan);

impl TryFrom<ExpectedSpanFields> for AbsentSpan {
    type Error = String;

    fn try_from(fields: ExpectedSpanFields) -> Result<Self, Self::Error> {
        if !fields.meta.is_empty() || fields.has_chunk_checks() || fields.side.is_some() {
            return Err(format!(
                "an `absent` span of service '{}' takes a `name` and a `resource`, but no `meta`, no chunk check \
                 (`priority`, `dropped_trace` or `chunk_tags`) and no `side`: a span that must not exist has no tags \
                 to check, and both sides must drop it",
                fields.service
            ));
        }
        if fields.name.is_none() && fields.resource.is_none() {
            return Err(format!(
                "an `absent` span of service '{}' needs a `name` or a `resource`: any span of the service would \
                 meet it",
                fields.service
            ));
        }

        ExpectedSpan::try_from(fields).map(Self)
    }
}

impl AbsentSpan {
    /// Returns the service of the absent span.
    pub fn service(&self) -> &str {
        self.0.service()
    }

    /// Returns whether `span` has the service, the name and the resource that the entry sets.
    fn is_met_by(&self, span: &Span) -> bool {
        self.0.selects(span)
    }

    /// Writes the entry as `service=.., name=.., resource=..`, leaving out what it doesn't set.
    fn describe(&self) -> String {
        self.0.describe()
    }
}

/// The side that an entry applies to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Side {
    Baseline,
    Comparison,
}

impl Side {
    /// Returns the name that an unmet expectation gives the side.
    fn name(self) -> &'static str {
        match self {
            Self::Baseline => BASELINE,
            Self::Comparison => COMPARISON,
        }
    }
}

/// The fields of [`ExpectedSpan`], as a case writes them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedSpanFields {
    service: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    meta: BTreeMap<String, MetaExpectation>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    dropped_trace: Option<bool>,
    #[serde(default)]
    chunk_tags: BTreeMap<String, MetaExpectation>,
    #[serde(default)]
    side: Option<Side>,
}

impl ExpectedSpanFields {
    /// Returns whether the entry checks the trace chunk of the span.
    fn has_chunk_checks(&self) -> bool {
        self.priority.is_some() || self.dropped_trace.is_some() || !self.chunk_tags.is_empty()
    }
}

impl TryFrom<ExpectedSpanFields> for ExpectedSpan {
    type Error = String;

    fn try_from(fields: ExpectedSpanFields) -> Result<Self, Self::Error> {
        if fields.name.is_none() && fields.resource.is_none() && fields.meta.is_empty() {
            return Err(format!(
                "a span of service '{}' needs a `name`, a `resource` or a `meta` tag: any span of the service would \
                 meet it",
                fields.service
            ));
        }

        Ok(Self {
            service: fields.service,
            name: fields.name,
            resource: fields.resource,
            meta: fields.meta,
            priority: fields.priority,
            dropped_trace: fields.dropped_trace,
            chunk_tags: fields.chunk_tags,
            side: fields.side,
        })
    }
}

impl ExpectedSpan {
    /// Returns the service of the expected span.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Returns whether the entry applies to both sides.
    pub fn applies_to_both(&self) -> bool {
        self.side.is_none()
    }

    /// Returns the name of the side that the entry is for, or `both` if it sets none.
    fn side_name(&self) -> &'static str {
        self.side.map_or("both", Side::name)
    }

    /// Returns whether the entry applies to the side named `side`.
    fn applies_to(&self, side: &str) -> bool {
        self.side.is_none_or(|only| only.name() == side)
    }

    /// Returns whether `span` has the service, the name and the resource that the expectation sets.
    fn selects(&self, span: &Span) -> bool {
        span.service() == self.service
            && self.name.as_deref().is_none_or(|name| span.name() == name)
            && self
                .resource
                .as_deref()
                .is_none_or(|resource| span.resource() == resource)
    }

    /// Returns whether the entry checks more of a selected span than its service, name and resource.
    fn checks_selected_spans(&self) -> bool {
        !self.meta.is_empty() || self.priority.is_some() || self.dropped_trace.is_some() || !self.chunk_tags.is_empty()
    }

    /// Returns a line for each `meta` tag, and each field of the chunk, of `span` that the expectation leaves unmet.
    fn mismatches(&self, span: &Span) -> Vec<String> {
        let mut mismatches = tag_mismatches("meta", &self.meta, |key| span.get_meta_field(key));
        if let Some(expected) = self.priority.filter(|expected| *expected != span.priority()) {
            mismatches.push(format!("`priority` is `{}`, not `{}`", span.priority(), expected));
        }
        if let Some(expected) = self.dropped_trace.filter(|expected| *expected != span.dropped_trace()) {
            mismatches.push(format!(
                "`dropped_trace` is `{}`, not `{}`",
                span.dropped_trace(),
                expected
            ));
        }
        mismatches.extend(tag_mismatches("chunk_tags", &self.chunk_tags, |key| {
            span.chunk_tag(key)
        }));
        mismatches
    }

    /// Returns whether the spans of one side meet the expectation: it selects a span, and every selected span meets
    /// the `meta` and chunk expectations.
    fn is_met_by_side(&self, spans: &HashMap<u64, Span>) -> bool {
        let mut selected = spans.values().filter(|span| self.selects(span)).peekable();
        selected.peek().is_some() && selected.all(|span| self.mismatches(span).is_empty())
    }

    /// Writes the expectation as `service=.., name=.., resource=.., meta.<key> ..`, followed by the chunk fields as
    /// `priority=..`, `dropped_trace=..` and `chunk_tags.<key> ..`, leaving out what it doesn't set.
    fn describe(&self) -> String {
        let mut parts = vec![format!("service={}", self.service)];
        if let Some(name) = &self.name {
            parts.push(format!("name={}", name));
        }
        if let Some(resource) = &self.resource {
            parts.push(format!("resource={}", resource));
        }
        for (key, expected) in &self.meta {
            parts.push(format!("meta.{} {}", key, expected.describe()));
        }
        if let Some(priority) = self.priority {
            parts.push(format!("priority={}", priority));
        }
        if let Some(dropped_trace) = self.dropped_trace {
            parts.push(format!("dropped_trace={}", dropped_trace));
        }
        for (key, expected) in &self.chunk_tags {
            parts.push(format!("chunk_tags.{} {}", key, expected.describe()));
        }
        parts.join(", ")
    }

    /// Says what the spans that the expectation selects hold, to show how close a side came.
    ///
    /// If a span has the service, the name and the resource, it says which `meta` tags and chunk fields are wrong, for
    /// each such span that holds one wrong. Otherwise it lists the fields that the expectation sets: the names, the
    /// resources, or both as `name/resource` pairs.
    fn describe_held(&self, spans: &HashMap<u64, Span>) -> String {
        let selected = spans.iter().filter(|(_, span)| self.selects(span)).collect::<Vec<_>>();
        if !self.checks_selected_spans() || selected.is_empty() {
            return self.describe_service(spans);
        }

        // List only the spans that fail: a span that meets the entry has nothing to show, and listing it could push the
        // span that fails past `MAX_LISTED`.
        let mut failing = selected
            .iter()
            .map(|(span_id, span)| (**span_id, self.mismatches(span)))
            .filter(|(_, mismatches)| !mismatches.is_empty())
            .collect::<Vec<_>>();
        failing.sort_by_key(|(span_id, _)| *span_id);
        let listed = failing
            .iter()
            .take(MAX_LISTED)
            .map(|(span_id, mismatches)| format!("span {} has {}", span_id, mismatches.join(" and ")))
            .collect::<Vec<_>>();
        format!(
            "{} span(s) have that service, name and resource, and {} hold a field wrong: {}{}",
            selected.len(),
            failing.len(),
            listed.join("; "),
            if failing.len() > MAX_LISTED { "; and more" } else { "" }
        )
    }

    /// Says what the spans of the expected service hold, to show how close a side came.
    ///
    /// It lists the fields that the expectation sets: the names, the resources, or both as `name/resource` pairs.
    fn describe_service(&self, spans: &HashMap<u64, Span>) -> String {
        let (what, label): (&str, fn(&Span) -> String) = match (&self.name, &self.resource) {
            (Some(_), Some(_)) => ("name/resource pairs", |span| {
                format!("{}/{}", span.name(), span.resource())
            }),
            (None, Some(_)) => ("resources", |span| span.resource().to_string()),
            _ => ("names", |span| span.name().to_string()),
        };
        let labels = spans
            .values()
            .filter(|span| span.service() == self.service)
            .map(label)
            .collect::<Vec<_>>();
        if labels.is_empty() {
            return "no span has that service".to_string();
        }

        let distinct = labels.iter().collect::<BTreeSet<_>>();
        let listed = distinct
            .iter()
            .take(MAX_LISTED)
            .map(|label| label.as_str())
            .collect::<Vec<_>>();
        format!(
            "{} span(s) have that service, with the {} {}{}",
            labels.len(),
            what,
            listed.join(", "),
            if distinct.len() > MAX_LISTED { ", and more" } else { "" }
        )
    }
}

/// Returns a line for each tag of `expected` whose value in `actual` leaves it unmet. `kind` names the map of tags in
/// the line.
fn tag_mismatches<'a>(
    kind: &str, expected: &BTreeMap<String, MetaExpectation>, actual: impl Fn(&str) -> Option<&'a str>,
) -> Vec<String> {
    expected
        .iter()
        .filter_map(|(key, expected)| {
            expected
                .mismatch(actual(key))
                .map(|mismatch| format!("`{}.{}` {}", kind, key, mismatch))
        })
        .collect()
}

/// What a span must hold in one `meta` tag, or in one tag of its trace chunk: the whole value, or a part that the value
/// must not hold.
///
/// A case writes a string for a value that the tag must equal, or a map of `equals` and `not_contains`, which can set
/// both. The tag must be present. Use `not_contains` for a secret that a side must scrub, together with `equals` for the
/// scrubbed value, so that a tag that a side drops fails the expectation instead of passing it.
#[derive(Clone, Debug)]
pub struct MetaExpectation {
    equals: Option<String>,
    not_contains: Option<String>,
}

/// The map form of [`MetaExpectation`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaChecks {
    #[serde(default)]
    equals: Option<String>,
    #[serde(default)]
    not_contains: Option<String>,
}

impl<'de> Deserialize<'de> for MetaExpectation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = MetaExpectation;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string, or a map of `equals` and `not_contains`")
            }

            fn visit_str<E: de::Error>(self, equals: &str) -> Result<Self::Value, E> {
                Ok(MetaExpectation {
                    equals: Some(equals.to_string()),
                    not_contains: None,
                })
            }

            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let MetaChecks { equals, not_contains } = MetaChecks::deserialize(MapAccessDeserializer::new(map))?;
                if equals.is_none() && not_contains.is_none() {
                    return Err(de::Error::custom(
                        "a `meta` expectation needs `equals` or `not_contains`",
                    ));
                }
                Ok(MetaExpectation { equals, not_contains })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl MetaExpectation {
    /// Says how `actual` fails the expectation, or returns `None` when it meets it.
    fn mismatch(&self, actual: Option<&str>) -> Option<String> {
        let Some(actual) = actual else {
            return Some("is missing".to_string());
        };
        if let Some(equals) = self.equals.as_deref().filter(|equals| *equals != actual) {
            return Some(format!("is `{}`, not `{}`", abbreviate(actual), abbreviate(equals)));
        }
        if let Some(part) = self.not_contains.as_deref().filter(|part| actual.contains(part)) {
            return Some(format!("is `{}`, and holds `{}`", abbreviate(actual), abbreviate(part)));
        }
        None
    }

    /// Writes the expectation as `= <value>` or `without <part>`, or both.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(equals) = &self.equals {
            parts.push(format!("= {}", abbreviate(equals)));
        }
        if let Some(part) = &self.not_contains {
            parts.push(format!("without {}", abbreviate(part)));
        }
        parts.join(" ")
    }
}

/// Cuts a tag value to the length that a line of an unmet expectation shows, and writes its line breaks as `\n`, so that
/// the value stays on one line.
fn abbreviate(value: &str) -> String {
    let value = value.replace('\n', "\\n");
    match value.char_indices().nth(MAX_SHOWN_CHARS) {
        Some((end, _)) => format!("{}...", &value[..end]),
        None => value,
    }
}

/// Says why no series meets `criteria`: no series has its name, or the series with its name fail.
fn describe_named(criteria: &MetricCriteria, series: &NormalizedMetrics) -> String {
    let named = series
        .metrics()
        .iter()
        .filter(|metric| metric.context().name() == criteria.name())
        .collect::<Vec<_>>();
    if named.is_empty() {
        return "no series has that name".to_string();
    }

    let failures = named
        .iter()
        .take(MAX_LISTED)
        .map(|metric| {
            let failure = criteria
                .mismatch(metric)
                .map(|mismatch| criteria.describe_mismatch(mismatch, metric))
                .unwrap_or_default();
            format!("{} ({}) fails {}", metric.context(), metric.value_type(), failure)
        })
        .collect::<Vec<_>>();
    format!(
        "{} series have that name: {}{}",
        named.len(),
        failures.join("; "),
        if named.len() > MAX_LISTED { "; and more" } else { "" }
    )
}

/// Passes when neither side leaves an expectation unmet.
///
/// # Errors
///
/// If a side leaves an expectation unmet, an error is returned with the lines of both sides as its details.
pub(super) fn require_met(baseline: Vec<String>, comparison: Vec<String>) -> Result<(), (GenericError, Vec<String>)> {
    if baseline.is_empty() && comparison.is_empty() {
        return Ok(());
    }

    let summary = format!(
        "Expectations are unmet: {} on the baseline and {} on the comparison.",
        baseline.len(),
        comparison.len()
    );
    error!("{}", summary);
    let details = baseline.into_iter().chain(comparison).collect::<Vec<_>>();
    for line in &details {
        error!("{}", line);
    }

    Err((generic_error!("{}", summary), details))
}
