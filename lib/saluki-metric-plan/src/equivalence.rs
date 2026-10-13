//! The equivalence oracle: comparing what two executions deliver, point by point.
//!
//! Two executions of a trace are **equivalent** when, for every output, they deliver the same multiset of points. Each
//! delivered record is flattened into one [`Point`] per point it carries: its context identity, its kind (including a
//! rate's interval), its metadata, its timestamp, and its value. How points are grouped into records or batches, and
//! the order in which they arrive, are not observable, so they play no part in the comparison.
//!
//! | Kind | Value of one point |
//! |---|---|
//! | `counter`, `rate`, `gauge` | The scalar, bitwise: `0.0` and `-0.0` differ; a NaN equals only the same NaN. |
//! | `set` | The set of members. |
//! | `histogram` | The samples, each with its weight, as a multiset. |
//! | `distribution` | The sketch: its count, minimum, maximum, sum, and average (bitwise), and its bins. |
//!
//! [`compare`] is the exact oracle the reference interpreter is checked against. [`compare_under_capacity`] checks the
//! weaker rule rewrites must keep under capacity pressure. [`compare_reordered`] is the weaker
//! oracle for executions that may reorder inputs across admission contexts: there, floating-point sums may differ
//! within a [tolerance][Tolerance], and set points are compared by their union. [`compare_partitioned`] completes it
//! for partitioned execution, whose lanes interleave the inputs of several admission points: it checks every kind,
//! gauges against the [candidates][Candidates] the interpreter records. [`check_capacity`] and [`check_balance`] check
//! a partitioned execution under capacity pressure, from the budget operations and losses a harness records.
//!
//! # Context identity
//!
//! A context compares equal to another when their 64-bit identity hashes are equal. The oracle instead compares a
//! context's parts: its name, its host (an unset host differs from an empty one), and its tags and origin tags, each
//! deduplicated and in sorted order. The two agree except when distinct contexts collide, which the oracle reports as
//! a difference rather than hiding.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
    hash::{Hash, Hasher},
    num::NonZeroU64,
    time::Duration,
};

use saluki_core::{
    data_model::event::metric::{Metric, MetricMetadata, MetricOrigin, MetricValues, SketchPoints},
    execution::AdmissionPointId,
};

use crate::{interpret::RunOutput, ir::OutputId, limits};

/// A floating-point value that compares bitwise and orders by the IEEE 754 total order.
///
/// `0.0` and `-0.0` are different values, and a NaN equals only a NaN with the same bits.
#[derive(Clone, Copy, Debug)]
pub struct Scalar(f64);

impl Scalar {
    /// Wraps a value.
    pub const fn new(value: f64) -> Self {
        Self(value)
    }

    /// Returns the value.
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl PartialEq for Scalar {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Scalar {}

impl PartialOrd for Scalar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scalar {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Hash for Scalar {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_nan() {
            // NaNs differ by their bits, which their decimal form does not show.
            write!(f, "NaN(0x{:016x})", self.0.to_bits())
        } else {
            write!(f, "{:?}", self.0)
        }
    }
}

/// The identity of a point's context: its name, host, tags, and origin tags.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContextIdentity {
    /// The raw name.
    pub name: String,

    /// The host, or `None` if it is unset.
    pub host: Option<String>,

    /// The tags, deduplicated and sorted.
    pub tags: Vec<String>,

    /// The origin tags, deduplicated and sorted.
    pub origin_tags: Vec<String>,
}

impl ContextIdentity {
    /// Returns the identity of a metric's context.
    pub fn of(metric: &Metric) -> Self {
        let context = metric.context();
        let sorted = |tags: BTreeSet<String>| tags.into_iter().collect::<Vec<_>>();
        Self {
            name: context.name().to_string(),
            host: context.host().map(str::to_string),
            tags: sorted(context.tags().into_iter().map(|tag| tag.as_str().to_string()).collect()),
            origin_tags: sorted(
                context
                    .origin_tags()
                    .into_iter()
                    .map(|tag| tag.as_str().to_string())
                    .collect(),
            ),
        }
    }
}

impl fmt::Display for ContextIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.name)?;
        match &self.host {
            Some(host) => write!(f, " host={:?}", host)?,
            None => f.write_str(" host=<unset>")?,
        }
        write!(f, " tags=[{}]", self.tags.join(", "))?;
        if !self.origin_tags.is_empty() {
            write!(f, " origin_tags=[{}]", self.origin_tags.join(", "))?;
        }
        Ok(())
    }
}

/// The kind of a point, including a rate's interval.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PointKind {
    /// A counter.
    Counter,

    /// A rate over the given interval.
    Rate {
        /// The interval; zero means the value is already per second.
        interval: Duration,
    },

    /// A gauge.
    Gauge,

    /// A set.
    Set,

    /// A histogram.
    Histogram,

    /// A distribution.
    Distribution,
}

impl PointKind {
    /// Returns the kind of a metric's values.
    pub fn of(values: &MetricValues) -> Self {
        match values {
            MetricValues::Counter(_) => Self::Counter,
            MetricValues::Rate(_, interval) => Self::Rate { interval: *interval },
            MetricValues::Gauge(_) => Self::Gauge,
            MetricValues::Set(_) => Self::Set,
            MetricValues::Histogram(_) => Self::Histogram,
            MetricValues::Distribution(_) => Self::Distribution,
        }
    }
}

impl fmt::Display for PointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Counter => f.write_str("counter"),
            Self::Rate { interval } => write!(f, "rate({:?})", interval),
            Self::Gauge => f.write_str("gauge"),
            Self::Set => f.write_str("set"),
            Self::Histogram => f.write_str("histogram"),
            Self::Distribution => f.write_str("distribution"),
        }
    }
}

/// Where a metric came from, as its metadata records it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OriginIdentity {
    /// A generic source type.
    SourceType(String),

    /// A product, subproduct, and product detail.
    Product {
        /// The product.
        product: u32,

        /// The subproduct.
        subproduct: u32,

        /// The product detail.
        detail: u32,
    },
}

/// The identity of a point's metadata.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MetadataIdentity {
    /// The origin, if it is set.
    pub origin: Option<OriginIdentity>,

    /// The unit; empty when it is not set.
    pub unit: String,
}

impl MetadataIdentity {
    /// Returns the identity of a metric's metadata.
    pub fn of(metadata: &MetricMetadata) -> Self {
        let origin = metadata.origin().map(|origin| match origin {
            MetricOrigin::SourceType(source_type) => OriginIdentity::SourceType(source_type.to_string()),
            MetricOrigin::OriginMetadata {
                product,
                subproduct,
                product_detail,
            } => OriginIdentity::Product {
                product: *product,
                subproduct: *subproduct,
                detail: *product_detail,
            },
        });
        Self {
            origin,
            unit: metadata.unit().unwrap_or_default().to_string(),
        }
    }

    fn is_default(&self) -> bool {
        self.origin.is_none() && self.unit.is_empty()
    }
}

impl fmt::Display for MetadataIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        match &self.origin {
            Some(OriginIdentity::SourceType(source_type)) => parts.push(format!("origin={:?}", source_type)),
            Some(OriginIdentity::Product {
                product,
                subproduct,
                detail,
            }) => parts.push(format!("origin={}/{}/{}", product, subproduct, detail)),
            None => {}
        }
        if !self.unit.is_empty() {
            parts.push(format!("unit={:?}", self.unit));
        }
        f.write_str(&parts.join(" "))
    }
}

/// One histogram sample and its weight.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sample {
    /// The sample's value.
    pub value: Scalar,

    /// How many samples it stands for: the inverse of its sample rate.
    pub weight: Scalar,
}

/// The identity of a sketch: its summary statistics and its bins.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SketchIdentity {
    /// The number of observations.
    pub count: u64,

    /// The stored minimum.
    pub min: Scalar,

    /// The stored maximum.
    pub max: Scalar,

    /// The stored sum.
    pub sum: Scalar,

    /// The stored average.
    pub avg: Scalar,

    /// Each bin's key and count, in the sketch's order.
    pub bins: Vec<(i32, u32)>,
}

/// The value of one point.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PointValue {
    /// The value of a counter, rate, or gauge point.
    Scalar(Scalar),

    /// The members of a set point.
    Members(BTreeSet<String>),

    /// The samples of a histogram point, sorted.
    Samples(Vec<Sample>),

    /// The sketch of a distribution point.
    Sketch(SketchIdentity),
}

impl fmt::Display for PointValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scalar(value) => write!(f, "{}", value),
            Self::Members(members) => {
                let members = members.iter().map(|member| format!("{:?}", member)).collect::<Vec<_>>();
                write!(f, "{{{}}}", members.join(", "))
            }
            Self::Samples(samples) => {
                let samples = samples
                    .iter()
                    .map(|sample| format!("{} x {}", sample.value, sample.weight))
                    .collect::<Vec<_>>();
                write!(f, "[{}]", samples.join(", "))
            }
            Self::Sketch(sketch) => {
                let bins = sketch
                    .bins
                    .iter()
                    .map(|(key, count)| format!("{}:{}", key, count))
                    .collect::<Vec<_>>();
                write!(
                    f,
                    "sketch(count={} min={} max={} sum={} avg={} bins=[{}])",
                    sketch.count,
                    sketch.min,
                    sketch.max,
                    sketch.sum,
                    sketch.avg,
                    bins.join(", ")
                )
            }
        }
    }
}

/// One delivered point, flattened out of the record that carried it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Point {
    /// The context identity.
    pub context: ContextIdentity,

    /// The kind, including a rate's interval.
    pub kind: PointKind,

    /// The metadata.
    pub metadata: MetadataIdentity,

    /// The timestamp, in seconds since the Unix epoch, or `None` if the point has none.
    pub timestamp: Option<NonZeroU64>,

    /// The value.
    pub value: PointValue,
}

impl Point {
    /// Flattens a record into its points, in the record's point order.
    pub fn flatten(metric: &Metric) -> Vec<Point> {
        let context = ContextIdentity::of(metric);
        let kind = PointKind::of(metric.values());
        let metadata = MetadataIdentity::of(metric.metadata());
        let point = |timestamp, value| Point {
            context: context.clone(),
            kind,
            metadata: metadata.clone(),
            timestamp,
            value,
        };

        match metric.values() {
            MetricValues::Counter(points) | MetricValues::Rate(points, _) | MetricValues::Gauge(points) => points
                .into_iter()
                .map(|(timestamp, value)| point(timestamp, PointValue::Scalar(Scalar(value))))
                .collect(),
            MetricValues::Set(points) => points
                .members()
                .map(|(timestamp, members)| point(timestamp, PointValue::Members(members.iter().cloned().collect())))
                .collect(),
            MetricValues::Histogram(points) => points
                .into_iter()
                .map(|(timestamp, histogram)| {
                    let mut samples = histogram
                        .samples()
                        .iter()
                        .map(|sample| Sample {
                            value: Scalar(sample.value.0),
                            weight: Scalar(sample.weight.0),
                        })
                        .collect::<Vec<_>>();
                    samples.sort_unstable();
                    point(timestamp, PointValue::Samples(samples))
                })
                .collect(),
            MetricValues::Distribution(points) => sketches(points)
                .into_iter()
                .map(|(timestamp, sketch)| point(timestamp, PointValue::Sketch(sketch)))
                .collect(),
        }
    }
}

/// Returns the identity of each sketch point.
fn sketches(points: &SketchPoints) -> Vec<(Option<NonZeroU64>, SketchIdentity)> {
    points
        .into_iter()
        .map(|(timestamp, sketch)| {
            let identity = SketchIdentity {
                count: sketch.count(),
                min: Scalar(sketch.stored_min()),
                max: Scalar(sketch.stored_max()),
                sum: Scalar(sketch.stored_sum()),
                avg: Scalar(sketch.stored_avg()),
                bins: sketch.bins().iter().map(|bin| (bin.key(), bin.count())).collect(),
            };
            (timestamp, identity)
        })
        .collect()
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind, self.context)?;
        if !self.metadata.is_default() {
            write!(f, " {}", self.metadata)?;
        }
        match self.timestamp {
            Some(timestamp) => write!(f, " ts={}", timestamp)?,
            None => f.write_str(" ts=none")?,
        }
        write!(f, " value={}", self.value)
    }
}

/// The multiset of points each output received.
///
/// An output that received nothing is the same as an output that is not listed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutputPoints(BTreeMap<OutputId, BTreeMap<Point, usize>>);

impl OutputPoints {
    /// Returns an empty set of outputs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the points every output of a reference execution received.
    pub fn from_run(run: &RunOutput) -> Self {
        let mut points = Self::new();
        for (output, records) in run.outputs() {
            for record in records {
                points.insert(output, &record.metric);
            }
        }
        points
    }

    /// Adds the points of one record that `output` received.
    pub fn insert(&mut self, output: &OutputId, metric: &Metric) {
        let points = Point::flatten(metric);
        if points.is_empty() {
            return;
        }
        let received = self.0.entry(output.clone()).or_default();
        for point in points {
            *received.entry(point).or_default() += 1;
        }
    }

    /// Returns these outputs with the points of `metrics` added to `output`.
    pub fn with<'a, I>(mut self, output: &OutputId, metrics: I) -> Self
    where
        I: IntoIterator<Item = &'a Metric>,
    {
        for metric in metrics {
            self.insert(output, metric);
        }
        self
    }

    /// Returns the points `output` received, each with how many times it was received, in point order.
    pub fn points(&self, output: &OutputId) -> impl Iterator<Item = (&Point, usize)> {
        self.0
            .get(output)
            .into_iter()
            .flat_map(|points| points.iter().map(|(point, count)| (point, *count)))
    }

    /// Returns how many points `output` received, counting repeats.
    pub fn count(&self, output: &OutputId) -> usize {
        self.points(output).map(|(_, count)| count).sum()
    }

    fn outputs<'a>(&'a self, other: &'a Self) -> BTreeSet<&'a OutputId> {
        self.0.keys().chain(other.0.keys()).collect()
    }
}

/// The difference between what one output was expected to receive and what it received.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputDiff {
    /// The output.
    pub output: OutputId,

    /// Points expected but not received, with how many times each is missing.
    pub missing: Vec<(Point, usize)>,

    /// Points received but not expected, with how many times each is extra.
    pub unexpected: Vec<(Point, usize)>,
}

/// How two executions' outputs differ: one entry per output that differs, in output order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mismatch {
    /// The outputs that differ.
    pub outputs: Vec<OutputDiff>,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diff in &self.outputs {
            let missing = diff.missing.iter().map(|(_, count)| count).sum::<usize>();
            let unexpected = diff.unexpected.iter().map(|(_, count)| count).sum::<usize>();
            writeln!(
                f,
                "output `{}`: {} expected point(s) missing, {} unexpected point(s)",
                diff.output, missing, unexpected
            )?;
            for (point, count) in &diff.missing {
                writeln!(f, "  missing    {}x {}", count, point)?;
            }
            for (point, count) in &diff.unexpected {
                writeln!(f, "  unexpected {}x {}", count, point)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Mismatch {}

/// Compares the multisets two executions delivered, output by output.
///
/// # Errors
///
/// Returns a [`Mismatch`] listing, for every output that differs, the points missing from `actual` and the points
/// `actual` has in excess.
pub fn compare(expected: &OutputPoints, actual: &OutputPoints) -> Result<(), Mismatch> {
    let empty = BTreeMap::new();
    let mut outputs = Vec::new();
    for output in expected.outputs(actual) {
        let left = expected.0.get(output).unwrap_or(&empty);
        let right = actual.0.get(output).unwrap_or(&empty);
        if left == right {
            continue;
        }
        let excess = |from: &BTreeMap<Point, usize>, other: &BTreeMap<Point, usize>| {
            from.iter()
                .filter_map(|(point, count)| {
                    let extra = count.saturating_sub(other.get(point).copied().unwrap_or(0));
                    (extra > 0).then(|| (point.clone(), extra))
                })
                .collect::<Vec<_>>()
        };
        outputs.push(OutputDiff {
            output: output.clone(),
            missing: excess(left, right),
            unexpected: excess(right, left),
        });
    }
    if outputs.is_empty() {
        Ok(())
    } else {
        Err(Mismatch { outputs })
    }
}

/// What one execution of a trace with capacity events delivered, and what its outputs refused.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapacityOutcome {
    /// The points each output received.
    pub delivered: OutputPoints,

    /// The points of the records each output refused: its capacity loss.
    pub refused: OutputPoints,
}

impl CapacityOutcome {
    /// Returns what an execution of the reference interpreter delivered and refused.
    pub fn from_run(run: &RunOutput) -> Self {
        let mut refused = OutputPoints::new();
        for (output, records) in run.refused_outputs() {
            for record in records {
                refused.insert(output, &record.metric);
            }
        }
        Self {
            delivered: OutputPoints::from_run(run),
            refused,
        }
    }
}

/// One way a rewritten execution breaks the specification's capacity rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapacityViolation {
    /// An output received a point more times than the policy delivers it: a contribution the policy excludes.
    Excluded {
        /// The output.
        output: OutputId,

        /// The point.
        point: Point,

        /// How many times the policy delivers it.
        policy: usize,

        /// How many times the rewritten execution delivered it.
        delivered: usize,
    },

    /// A point the policy delivers was lost more times than the original execution lost it.
    MoreLoss {
        /// The output.
        output: OutputId,

        /// The point.
        point: Point,

        /// How many times the original execution lost it.
        original: usize,

        /// How many times the rewritten execution lost it.
        rewritten: usize,
    },

    /// The rewritten execution's outputs refused more points in all than the original's.
    MoreRefused {
        /// The points the original execution's outputs refused.
        original: usize,

        /// The points the rewritten execution's outputs refused.
        rewritten: usize,
    },
}

impl fmt::Display for CapacityViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Excluded {
                output,
                point,
                policy,
                delivered,
            } => write!(
                f,
                "output `{}` received {}x, but the policy delivers {}x: {}",
                output, delivered, policy, point
            ),
            Self::MoreLoss {
                output,
                point,
                original,
                rewritten,
            } => write!(
                f,
                "output `{}` lost {}x, but the original lost {}x: {}",
                output, rewritten, original, point
            ),
            Self::MoreRefused { original, rewritten } => write!(
                f,
                "the outputs refused {} point(s), but the original's refused {}",
                rewritten, original
            ),
        }
    }
}

/// How a rewritten execution under capacity pressure breaks the capacity rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityMismatch {
    /// Every violation, by output and point, then the total.
    pub violations: Vec<CapacityViolation>,
}

impl fmt::Display for CapacityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for violation in &self.violations {
            writeln!(f, "{}", violation)?;
        }
        Ok(())
    }
}

impl std::error::Error for CapacityMismatch {}

/// Checks the specification's capacity rule: under capacity pressure, a rewrite may only reduce capacity loss.
///
/// `policy` is what the policy includes: what the original plan delivers on the same trace with every `capacity`
/// event removed ([`Trace::without_capacity`][crate::interpret::Trace::without_capacity]), so that no output bounds
/// it. `original` and `rewritten` are the executions of the original and the rewritten plan on the trace with its
/// capacity events. A point's **loss** in an execution is how many fewer times its output received it than the policy
/// delivers it. The rewritten execution must:
///
/// - never deliver a contribution the policy excludes: no output receives a point more times than the policy delivers
///   it;
/// - never increase capacity loss for any policy-included contribution: no point is lost more times than the original
///   execution lost it;
/// - only reduce capacity loss: its outputs refuse no more points, in all, than the original's.
///
/// On a trace without capacity pressure, this holds exactly when the two executions are [equivalent][compare] to the
/// policy.
///
/// # Errors
///
/// Returns a [`CapacityMismatch`] listing every violation.
pub fn compare_under_capacity(
    policy: &OutputPoints, original: &CapacityOutcome, rewritten: &CapacityOutcome,
) -> Result<(), CapacityMismatch> {
    let empty = BTreeMap::new();
    let mut violations = Vec::new();
    let outputs = policy
        .outputs(&rewritten.delivered)
        .into_iter()
        .chain(original.delivered.0.keys())
        .collect::<BTreeSet<_>>();
    for output in outputs {
        let included = policy.0.get(output).unwrap_or(&empty);
        let before = original.delivered.0.get(output).unwrap_or(&empty);
        let after = rewritten.delivered.0.get(output).unwrap_or(&empty);
        let points = included.keys().chain(after.keys()).collect::<BTreeSet<_>>();
        for point in points {
            let count = |points: &BTreeMap<Point, usize>| points.get(point).copied().unwrap_or(0);
            let (policy, original, delivered) = (count(included), count(before), count(after));
            if delivered > policy {
                violations.push(CapacityViolation::Excluded {
                    output: output.clone(),
                    point: point.clone(),
                    policy,
                    delivered,
                });
            } else if policy - delivered > policy.saturating_sub(original) {
                violations.push(CapacityViolation::MoreLoss {
                    output: output.clone(),
                    point: point.clone(),
                    original: policy.saturating_sub(original),
                    rewritten: policy - delivered,
                });
            }
        }
    }
    let total = |points: &OutputPoints| points.0.values().flat_map(BTreeMap::values).sum::<usize>();
    let (refused_before, refused_after) = (total(&original.refused), total(&rewritten.refused));
    if refused_after > refused_before {
        violations.push(CapacityViolation::MoreRefused {
            original: refused_before,
            rewritten: refused_after,
        });
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(CapacityMismatch { violations })
    }
}

/// The relative tolerance within which two floating-point sums are equal when inputs may be reordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerance {
    /// The largest allowed difference, relative to the larger magnitude of the two values.
    pub relative: f64,
}

impl Tolerance {
    /// Returns the tolerance from the [limits table][crate::limits::FLOAT_TOLERANCE].
    pub fn from_limits() -> Self {
        Self {
            relative: limits::values::FLOAT_TOLERANCE,
        }
    }

    /// Returns `true` if `a` and `b` are equal within the tolerance.
    ///
    /// Equal bits are always equal. Otherwise, a NaN or an infinity equals nothing, and two finite values are equal
    /// when their difference is at most the relative tolerance times the larger of their magnitudes.
    pub fn accepts(&self, a: f64, b: f64) -> bool {
        if a.to_bits() == b.to_bits() {
            return true;
        }
        // An infinity's relative difference from anything is not finite, so no tolerance covers it: an overflowed sum
        // never matches a finite one, nor the opposite infinity.
        if !a.is_finite() || !b.is_finite() {
            return false;
        }
        (a - b).abs() <= self.relative * a.abs().max(b.abs())
    }
}

/// The verdict of [`compare_reordered`].
#[derive(Clone, Debug, PartialEq)]
pub enum ReorderedVerdict {
    /// Every output is equivalent under reordering.
    Equivalent,

    /// Some output differs in a way no reordering explains.
    NotEquivalent(Mismatch),

    /// Every difference is in points whose reordering check is not implemented yet, so equivalence is undecided.
    Undecided {
        /// Why, naming the first undecided group of points.
        reason: String,
    },
}

/// The points of one output that share a context, kind, metadata, and timestamp: one bucket of one series.
type Group<'a> = (&'a ContextIdentity, PointKind, &'a MetadataIdentity, Option<NonZeroU64>);

fn groups(points: &BTreeMap<Point, usize>) -> BTreeMap<Group<'_>, Vec<&PointValue>> {
    let mut groups: BTreeMap<Group<'_>, Vec<&PointValue>> = BTreeMap::new();
    for (point, count) in points {
        let values = groups
            .entry((&point.context, point.kind, &point.metadata, point.timestamp))
            .or_default();
        values.extend(std::iter::repeat_n(&point.value, *count));
    }
    groups
}

/// Compares two executions that may reorder inputs across admission contexts, with the per-kind checks that hold under
/// any such reordering.
///
/// Points are grouped by output, context, kind, metadata, and timestamp. A group whose points are exactly equal is
/// equivalent. Reordering inputs across admission contexts never changes how many points a group has, so groups with
/// different numbers of points differ, whatever their kind. Otherwise:
///
/// - counter and rate values are compared as sums: sorted, then pairwise equal within `tolerance`;
/// - set points are equivalent when the union of their members is equal;
/// - gauges, histograms, and distributions have no reordering check yet. The gauge check, that each value is one of the
///   candidates admitted to its bucket, needs the admitted inputs and arrives with partitioned execution. A difference
///   in these kinds leaves the verdict [undecided][ReorderedVerdict::Undecided].
///
/// Any group that differs beyond these checks makes the executions [not equivalent][ReorderedVerdict::NotEquivalent],
/// whatever the undecided groups hold.
pub fn compare_reordered(expected: &OutputPoints, actual: &OutputPoints, tolerance: Tolerance) -> ReorderedVerdict {
    let empty = BTreeMap::new();
    let mut differing = BTreeSet::new();
    let mut undecided = None;
    for output in expected.outputs(actual) {
        let left = groups(expected.0.get(output).unwrap_or(&empty));
        let right = groups(actual.0.get(output).unwrap_or(&empty));
        let keys = left.keys().chain(right.keys()).copied().collect::<BTreeSet<_>>();
        for key in keys {
            let mut a = left.get(&key).cloned().unwrap_or_default();
            let mut b = right.get(&key).cloned().unwrap_or_default();
            a.sort_unstable();
            b.sort_unstable();
            if a == b {
                continue;
            }
            let (context, kind, _, timestamp) = key;
            if a.len() != b.len() {
                differing.insert((output.clone(), key));
                continue;
            }
            let equivalent = match kind {
                PointKind::Counter | PointKind::Rate { .. } => a.iter().zip(&b).all(|pair| match pair {
                    (PointValue::Scalar(x), PointValue::Scalar(y)) => tolerance.accepts(x.get(), y.get()),
                    _ => false,
                }),
                PointKind::Set => union(&a) == union(&b),
                PointKind::Gauge | PointKind::Histogram | PointKind::Distribution => {
                    if undecided.is_none() {
                        undecided = Some(format!(
                            "the {} points of {} at {} in output `{}` differ, and {} points have no reordering \
                             check until partitioned execution (stage 9)",
                            kind,
                            context,
                            timestamp.map_or_else(|| "no timestamp".to_string(), |ts| format!("ts={}", ts)),
                            output,
                            kind
                        ));
                    }
                    continue;
                }
            };
            if !equivalent {
                differing.insert((output.clone(), key));
            }
        }
    }

    if differing.is_empty() {
        return match undecided {
            Some(reason) => ReorderedVerdict::Undecided { reason },
            None => ReorderedVerdict::Equivalent,
        };
    }

    // Report the differing groups exactly, as `compare` would for those points alone.
    let restrict = |points: &OutputPoints| {
        let mut restricted = OutputPoints::new();
        for (output, received) in &points.0 {
            for (point, count) in received {
                let key = (&point.context, point.kind, &point.metadata, point.timestamp);
                if differing.contains(&(output.clone(), key)) {
                    *restricted
                        .0
                        .entry(output.clone())
                        .or_default()
                        .entry(point.clone())
                        .or_default() += count;
                }
            }
        }
        restricted
    };
    match compare(&restrict(expected), &restrict(actual)) {
        Err(mismatch) => ReorderedVerdict::NotEquivalent(mismatch),
        // The restricted points differ by construction; this arm only keeps the match total.
        Ok(()) => ReorderedVerdict::Equivalent,
    }
}

fn union(values: &[&PointValue]) -> BTreeSet<String> {
    values
        .iter()
        .flat_map(|value| match value {
            PointValue::Members(members) => members.iter().cloned().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// The key candidates are recorded and looked up under: one bucket of one series of one output, whatever its kind and
/// metadata, which are what may differ when inputs from several admission points interleave.
pub type CandidateKey = (OutputId, ContextIdentity, Option<NonZeroU64>);

/// One admission into a bucket of a series, as candidate mode records it: its kind as the state owner emits it, its
/// value for a gauge, the instance of the state owner that admitted it, and its place among the run's admissions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Admission {
    /// The kind, a rate's interval included, as the state owner emits it.
    pub kind: PointKind,

    /// For a gauge, the value admitted.
    pub gauge: Option<Scalar>,

    /// The instance of the state owner that admitted it: a series has one entry per instance, which a kind replaces
    /// whole.
    pub instance: u64,

    /// The admission's place among the run's admissions, which orders one point's admissions.
    pub order: u64,
}

/// What one bucket of one series could hold when inputs from several admission points interleave in another order:
/// each point's last admission into the bucket, and the kinds and metadata the bucket's entry was admitted with.
///
/// Natively several producers race to aggregation, so which of two points' gauge values is last in a bucket, which
/// metadata is first, and which kind replaces the other are arrival-order outcomes. A partitioned execution keeps the
/// order within each point, and so delivers one of these candidates; the reference interpreter, in
/// [candidate mode][crate::interpret::Interpreter::with_candidates], records them.
///
/// Every admission into a bucket arrives before the flush that closes it, since a state owner closes a bucket only once
/// every upstream point's progress passed it, and each point's path keeps its order. So whichever admission into the
/// bucket arrives last is some point's last admission into it, and decides the kind the bucket holds, if it holds any,
/// and a gauge's value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BucketCandidates {
    /// Per admission point that admitted to the bucket, its last admission into it.
    pub last: BTreeMap<AdmissionPointId, Admission>,

    /// The kinds, a rate's interval included, that the bucket's entry was admitted with while its instance lived, in
    /// any of its buckets.
    pub kinds: BTreeSet<PointKind>,

    /// The metadata the bucket's entry was admitted with while it lived.
    pub metadata: BTreeSet<MetadataIdentity>,
}

impl BucketCandidates {
    /// Returns `true` if the bucket's entry was admitted with more than one kind, as recorded here: the kind it holds,
    /// and so its values and whether it holds points at all, depend on which input arrived last.
    pub fn has_kind_conflict(&self) -> bool {
        self.kinds.len() > 1
    }

    /// Returns the kinds the bucket may hold: the kind of each point's last admission into it.
    pub fn emitted_kinds(&self) -> BTreeSet<PointKind> {
        self.last.values().map(|admission| admission.kind).collect()
    }

    /// Returns the gauge values the bucket may hold: the value of each point's last admission into it that is a gauge.
    pub fn gauges(&self) -> BTreeMap<AdmissionPointId, Scalar> {
        self.last
            .iter()
            .filter_map(|(point, admission)| admission.gauge.map(|value| (*point, value)))
            .collect()
    }

    /// Adds the candidates of another record of the same bucket: of a later emission of its entry, or a copy of the
    /// same series delivered twice. A point's later admission replaces an earlier one.
    fn merge(&mut self, other: &Self) {
        for (point, admission) in &other.last {
            let kept = self.last.entry(*point).or_insert(*admission);
            if admission.order > kept.order {
                *kept = *admission;
            }
        }
        self.kinds.extend(other.kinds.iter().copied());
        self.metadata.extend(other.metadata.iter().cloned());
    }
}

/// What the interpreter's candidate mode recorded for one run
/// ([`Interpreter::with_candidates`][crate::interpret::Interpreter::with_candidates]): for every bucket of every series
/// an output received from an isolated state owner, its [candidates][BucketCandidates], keyed by the output's own view
/// of the series, after everything downstream of the state owner. Every bucket the series' entry was admitted to is
/// recorded, including one whose points a kind replaced before they were emitted.
///
/// A harness that knows what each point admitted can also build candidates by hand, with
/// [`admit`][Self::admit] and [`add_metadata`][Self::add_metadata].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Candidates {
    buckets: BTreeMap<CandidateKey, BucketCandidates>,
    /// The order the next admission recorded by hand gets.
    next: u64,
}

impl Candidates {
    /// Returns no candidates.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if no bucket has candidates.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Returns every bucket's candidates, in key order.
    pub fn buckets(&self) -> impl Iterator<Item = (&CandidateKey, &BucketCandidates)> {
        self.buckets.iter()
    }

    /// Returns the candidates of the bucket at `timestamp` of the series `context` that `output` received, if any were
    /// recorded.
    pub fn get(
        &self, output: &OutputId, context: &ContextIdentity, timestamp: Option<NonZeroU64>,
    ) -> Option<&BucketCandidates> {
        self.buckets.get(&(output.clone(), context.clone(), timestamp))
    }

    fn bucket(&mut self, key: CandidateKey) -> &mut BucketCandidates {
        self.buckets.entry(key).or_default()
    }

    /// Records an admission by `point` into the bucket `key`, of `kind` with a gauge's `value`, by the state owner's
    /// instance `instance`, after every admission recorded before: the point's last admission into the bucket so far,
    /// whose kind the bucket's entry was admitted with.
    pub fn admit(
        &mut self, key: CandidateKey, point: AdmissionPointId, instance: u64, kind: PointKind, value: Option<f64>,
    ) {
        let order = self.next;
        self.next += 1;
        let bucket = self.bucket(key);
        bucket.last.insert(
            point,
            Admission {
                kind,
                gauge: value.filter(|_| kind == PointKind::Gauge).map(Scalar::new),
                instance,
                order,
            },
        );
        bucket.kinds.insert(kind);
    }

    /// Records that the entry of the series of `key` was admitted with `metadata`.
    pub fn add_metadata(&mut self, key: CandidateKey, metadata: MetadataIdentity) {
        self.bucket(key).metadata.insert(metadata);
    }

    /// Adds `candidates` to the bucket `key`, merging them with any recorded already.
    pub fn add(&mut self, key: CandidateKey, candidates: &BucketCandidates) {
        let after = candidates.last.values().map(|admission| admission.order + 1).max();
        self.next = self.next.max(after.unwrap_or(0));
        self.bucket(key).merge(candidates);
    }
}

/// The points of one output in one bucket of one series, whatever their kind and metadata.
type SeriesBucket<'a> = (&'a ContextIdentity, Option<NonZeroU64>);

fn series_buckets(points: &BTreeMap<Point, usize>) -> BTreeMap<SeriesBucket<'_>, Vec<&Point>> {
    let mut buckets: BTreeMap<SeriesBucket<'_>, Vec<&Point>> = BTreeMap::new();
    for (point, count) in points {
        buckets
            .entry((&point.context, point.timestamp))
            .or_default()
            .extend(std::iter::repeat_n(point, *count));
    }
    buckets
}

/// One series of one output fed by one instance of a state owner: what a kind replaces whole.
type SeriesInstance<'a> = (&'a OutputId, &'a ContextIdentity, u64);

/// One recorded admission of a series instance: the bucket it went to, the admission point, and the admission.
type RecordedAdmission = (Option<NonZeroU64>, AdmissionPointId, Admission);

/// Compares a partitioned execution against the reference interpreter's run of the original trace in candidate mode,
/// with the per-kind checks that hold however the lanes' input streams interleave at the state lanes.
///
/// Points are grouped by output, context, and timestamp: one bucket of one series. Metadata and kind leave the group's
/// key, since the order of arrival decides them. Interleaving never changes how many points a bucket has, so buckets
/// whose point counts differ differ, with one exception: an **idle-counter zero**. Which ticks fire while a counter is
/// idle, and so from which flush its idle buckets get zeros, depends on when the heartbeats that release a tick arrive,
/// since a tick with a point not heard from yet is skipped, so a counter's or rate's point of value zero counts as no
/// point: it adds nothing to a sum. Otherwise a bucket is equivalent when:
///
/// | What | Check |
/// |---|---|
/// | Metadata | each point's is one of the bucket's candidates; without candidates, the metadata are equal |
/// | Kind | each kind has as many points on both sides |
/// | Counter, rate | sorted, the sums are pairwise equal within `tolerance` (amendment 31) |
/// | Set | the unions of members are equal |
/// | Histogram | the sample multisets with their weights are equal |
/// | Distribution | sorted, bins, count, minimum, and maximum are equal; sum and average within `tolerance` |
/// | Gauge | the **maximal candidate**, below; a gauge without candidates is compared as a sum |
///
/// A gauge's value must be the value of one of the contributing points' last admissions into the bucket, which with one
/// contributing point is exactly that point's last: "any admitted value" would accept a point's first value when it
/// sent two, which no interleaving delivers. A gauge without candidates is one the state owner derived, such as a
/// histogram's aggregate.
///
/// A **kind conflict** is a series whose entry in one instance of the state owner was admitted with several kinds. The
/// kind that arrives replaces the value set the entry holds, every bucket of it (amendment 82), so which kind a bucket
/// holds, and whether it holds points at all, depend on the order, and its values are not compared. Its buckets are
/// checked against what no order changes:
///
/// - a bucket holds no more points than it does in the reference or, absent there, than one, and only at a timestamp
///   some admission was bucketed at: a bucket no admission reached has no candidates, so it is compared exactly;
/// - each point's kind is the kind of some point's last admission into the bucket, which arrives after every other
///   admission of that point into it; a gauge's value is that admission's;
/// - **presence**: some point's last admission into the entry, which no later one replaces once it arrived last of
///   all, is delivered with its kind in its bucket, whenever the reference delivered the entry's last admission. That
///   holds unless the bucket of the last admission to arrive is discarded at shutdown, which then discards the
///   reference's last admission too, since every later admission falls in the bucket open at the close.
///
/// The loss balance of a kind replacement, which the state owner counts as policy loss, is checked separately, by
/// [`check_balance`]. The checks assume buckets by admission time, as aggregation of untimestamped values has them, and
/// that what reaches an output downstream of the state owner does not depend on a record's kind or value.
///
/// Any bucket that differs makes the executions [not equivalent][ReorderedVerdict::NotEquivalent], reported exactly
/// as [`compare`] reports those buckets' points; the verdict is never undecided.
pub fn compare_partitioned(
    expected: &OutputPoints, actual: &OutputPoints, candidates: &Candidates, tolerance: Tolerance,
) -> ReorderedVerdict {
    let empty = BTreeMap::new();
    // The kinds each series' entry was admitted with, per instance: more than one is a kind conflict.
    let mut instance_kinds: BTreeMap<SeriesInstance<'_>, BTreeSet<PointKind>> = BTreeMap::new();
    for ((output, context, _), bucket) in candidates.buckets() {
        for admission in bucket.last.values() {
            instance_kinds
                .entry((output, context, admission.instance))
                .or_default()
                .extend(bucket.kinds.iter().copied());
        }
    }
    let conflicted = |output, context, instance| {
        instance_kinds
            .get(&(output, context, instance))
            .is_some_and(|kinds| kinds.len() > 1)
    };

    let mut differing = BTreeSet::new();
    for output in expected.outputs(actual) {
        let left = series_buckets(expected.0.get(output).unwrap_or(&empty));
        let right = series_buckets(actual.0.get(output).unwrap_or(&empty));
        let keys = left.keys().chain(right.keys()).copied().collect::<BTreeSet<_>>();
        for key in keys {
            let (context, timestamp) = key;
            let a = left.get(&key).map_or(&[][..], Vec::as_slice);
            let b = right.get(&key).map_or(&[][..], Vec::as_slice);
            let bucket = candidates.get(output, context, timestamp);
            let equivalent = match bucket {
                Some(bucket)
                    if bucket
                        .last
                        .values()
                        .any(|admission| conflicted(output, context, admission.instance)) =>
                {
                    conflict_equivalent(a, b, bucket)
                }
                bucket => bucket_equivalent(a, b, bucket, tolerance),
            };
            if !equivalent {
                differing.insert((output.clone(), context.clone(), timestamp));
            }
        }

        // Presence: in each conflicted series instance, some point's last admission into the entry is delivered.
        let mut admissions = BTreeMap::<(&ContextIdentity, u64), Vec<RecordedAdmission>>::new();
        for ((at, context, timestamp), bucket) in candidates.buckets() {
            if at != output {
                continue;
            }
            for (point, admission) in &bucket.last {
                admissions
                    .entry((context, admission.instance))
                    .or_default()
                    .push((*timestamp, *point, *admission));
            }
        }
        let holds = |points: &BTreeMap<SeriesBucket<'_>, Vec<&Point>>, context, timestamp, kind| {
            points
                .get(&(context, timestamp))
                .is_some_and(|points| points.iter().any(|point| point.kind == kind))
        };
        for ((context, instance), admissions) in admissions {
            if !conflicted(output, context, instance) {
                continue;
            }
            let mut finals = BTreeMap::<AdmissionPointId, (Option<NonZeroU64>, Admission)>::new();
            for (timestamp, point, admission) in &admissions {
                let kept = finals.entry(*point).or_insert((*timestamp, *admission));
                if admission.order > kept.1.order {
                    *kept = (*timestamp, *admission);
                }
            }
            let Some((last_at, last)) = finals.values().max_by_key(|(_, admission)| admission.order) else {
                continue;
            };
            if !holds(&left, context, *last_at, last.kind) {
                continue;
            }
            if !finals
                .values()
                .any(|(timestamp, admission)| holds(&right, context, *timestamp, admission.kind))
            {
                for (timestamp, _) in finals.values() {
                    differing.insert((output.clone(), context.clone(), *timestamp));
                }
            }
        }
    }
    if differing.is_empty() {
        return ReorderedVerdict::Equivalent;
    }
    let restrict = |points: &OutputPoints| {
        let mut restricted = OutputPoints::new();
        for (output, received) in &points.0 {
            for (point, count) in received {
                if differing.contains(&(output.clone(), point.context.clone(), point.timestamp)) {
                    *restricted
                        .0
                        .entry(output.clone())
                        .or_default()
                        .entry(point.clone())
                        .or_default() += count;
                }
            }
        }
        restricted
    };
    match compare(&restrict(expected), &restrict(actual)) {
        Err(mismatch) => ReorderedVerdict::NotEquivalent(mismatch),
        // The restricted points differ by construction: a bucket that differs holds different points, and a missing
        // final's buckets hold the reference's last admission, which the execution lacks.
        Ok(()) => ReorderedVerdict::Equivalent,
    }
}

/// Returns the points of a bucket without its idle-counter zeros: a counter's or rate's point of value zero.
fn without_zeros<'p>(points: &[&'p Point]) -> Vec<&'p Point> {
    let zero = |point: &Point| {
        matches!(point.kind, PointKind::Counter | PointKind::Rate { .. })
            && matches!(point.value, PointValue::Scalar(value) if value.get() == 0.0)
    };
    points.iter().copied().filter(|point| !zero(point)).collect()
}

/// Returns `true` if the points one bucket of a kind-conflicted series received in a partitioned execution are among
/// those some interleaving delivers, by [`compare_partitioned`]'s rules for a kind conflict.
fn conflict_equivalent(expected: &[&Point], actual: &[&Point], candidates: &BucketCandidates) -> bool {
    let (expected, actual) = (without_zeros(expected), without_zeros(actual));
    let kinds = candidates.emitted_kinds();
    let gauges = candidates.gauges();
    let metadata_ok = |point: &Point| candidates.metadata.is_empty() || candidates.metadata.contains(&point.metadata);
    actual.len() <= expected.len().max(1)
        && actual.iter().all(|point| {
            kinds.contains(&point.kind)
                && metadata_ok(point)
                && match (&point.kind, &point.value) {
                    (PointKind::Gauge, PointValue::Scalar(value)) => {
                        gauges.values().any(|candidate| candidate == value)
                    }
                    (PointKind::Gauge, _) => false,
                    _ => true,
                }
        })
}

/// Returns `true` if the points one bucket of one series received in two executions are equivalent under
/// interleaving, by [`compare_partitioned`]'s table.
fn bucket_equivalent(
    expected: &[&Point], actual: &[&Point], candidates: Option<&BucketCandidates>, tolerance: Tolerance,
) -> bool {
    let (expected, actual) = (&without_zeros(expected)[..], &without_zeros(actual)[..]);
    if expected.len() != actual.len() {
        return false;
    }
    fn sorted<'p>(points: &[&'p Point]) -> Vec<&'p Point> {
        let mut points = points.to_vec();
        points.sort_unstable();
        points
    }
    if sorted(expected) == sorted(actual) {
        return true;
    }

    let metadata_candidates = candidates
        .map(|candidates| &candidates.metadata)
        .filter(|metadata| !metadata.is_empty());
    let metadata_ok = match metadata_candidates {
        Some(allowed) => actual.iter().all(|point| allowed.contains(&point.metadata)),
        None => {
            fn metadata<'p>(points: &[&'p Point]) -> Vec<&'p MetadataIdentity> {
                let mut metadata = points.iter().map(|point| &point.metadata).collect::<Vec<_>>();
                metadata.sort_unstable();
                metadata
            }
            metadata(expected) == metadata(actual)
        }
    };
    if !metadata_ok {
        return false;
    }

    fn by_kind<'p>(points: &[&'p Point]) -> BTreeMap<PointKind, Vec<&'p PointValue>> {
        let mut kinds: BTreeMap<PointKind, Vec<&PointValue>> = BTreeMap::new();
        for point in points {
            kinds.entry(point.kind).or_default().push(&point.value);
        }
        kinds
    }
    let (left, right) = (by_kind(expected), by_kind(actual));
    let kinds = left.keys().chain(right.keys()).copied().collect::<BTreeSet<_>>();
    kinds.into_iter().all(|kind| {
        let mut a = left.get(&kind).cloned().unwrap_or_default();
        let mut b = right.get(&kind).cloned().unwrap_or_default();
        if a.len() != b.len() {
            return false;
        }
        a.sort_unstable();
        b.sort_unstable();
        match kind {
            PointKind::Counter | PointKind::Rate { .. } => sums_within(&a, &b, tolerance),
            PointKind::Set => union(&a) == union(&b),
            PointKind::Histogram => a == b,
            PointKind::Distribution => sketches_within(&a, &b, tolerance),
            PointKind::Gauge => match candidates
                .map(BucketCandidates::gauges)
                .filter(|gauges| !gauges.is_empty())
            {
                Some(gauges) => b.iter().all(|value| match value {
                    PointValue::Scalar(value) => gauges.values().any(|candidate| candidate == value),
                    _ => false,
                }),
                None => sums_within(&a, &b, tolerance),
            },
        }
    })
}

/// Returns `true` if two sorted lists of scalar values are pairwise equal within `tolerance`.
fn sums_within(a: &[&PointValue], b: &[&PointValue], tolerance: Tolerance) -> bool {
    a.iter().zip(b).all(|pair| match pair {
        (PointValue::Scalar(x), PointValue::Scalar(y)) => tolerance.accepts(x.get(), y.get()),
        _ => false,
    })
}

/// Returns `true` if two lists of sketches pair up, sorted by their exact parts, with equal bins, counts, minimums, and
/// maximums, and sums and averages within `tolerance`.
fn sketches_within(a: &[&PointValue], b: &[&PointValue], tolerance: Tolerance) -> bool {
    fn exact<'p>(values: &[&'p PointValue]) -> Vec<&'p SketchIdentity> {
        let mut sketches = values
            .iter()
            .filter_map(|value| match value {
                PointValue::Sketch(sketch) => Some(sketch),
                _ => None,
            })
            .collect::<Vec<_>>();
        sketches.sort_by(|x, y| (&x.bins, x.count, x.min, x.max).cmp(&(&y.bins, y.count, y.min, y.max)));
        sketches
    }
    let (left, right) = (exact(a), exact(b));
    left.len() == a.len()
        && right.len() == b.len()
        && left.iter().zip(&right).all(|(x, y)| {
            x.bins == y.bins
                && x.count == y.count
                && x.min == y.min
                && x.max == y.max
                && tolerance.accepts(x.sum.get(), y.sum.get())
                && tolerance.accepts(x.avg.get(), y.avg.get())
        })
}

/// One operation on a state budget, as a harness records it from a budget's recording, with the `used` value the
/// operation's own compare-and-swap observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetEvent {
    /// An acquisition of `units`, bounded by `bound`, that observed `observed` units in use and was granted or not.
    Acquire {
        /// The units the acquisition took, if it was granted, or asked for, if it was refused: an acquisition of as
        /// many units as fit, such as a lane batch's block of context units, records the units it took, and one unit
        /// when not one fit.
        ///
        /// The units in use count units a block reserved and has not handed to a context yet, which a refusal's own
        /// observation includes; they are outstanding from the block's acquisition until they are handed out, given
        /// back at once (a release right after the acquisition, when another budget had less room), or returned when
        /// the batch ends. Once every batch ended, what was granted less what was released is what contexts hold.
        units: u64,

        /// The bound the acquisition was checked against: the smaller of its limit and the budget's capacity.
        bound: u64,

        /// The units in use the acquisition observed.
        observed: u64,

        /// Whether the units were granted.
        granted: bool,
    },

    /// A release of `units`, which observed `observed` units in use.
    Release {
        /// The units released.
        units: u64,

        /// The units in use the release observed.
        observed: u64,
    },
}

#[cfg(any(test, feature = "test-util"))]
impl From<saluki_core::execution::BudgetOperation> for BudgetEvent {
    fn from(operation: saluki_core::execution::BudgetOperation) -> Self {
        use saluki_core::execution::BudgetOperation;
        match operation {
            BudgetOperation::Acquire {
                units,
                bound,
                observed,
                granted,
            } => Self::Acquire {
                units,
                bound,
                observed,
                granted,
            },
            BudgetOperation::Release { units, observed } => Self::Release { units, observed },
        }
    }
}

/// What a harness recorded of one budget of a partitioned execution under capacity pressure, for [`check_capacity`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapacityTrace {
    /// The capacity of the budget the execution drew on.
    pub capacity: u64,

    /// The most units every acquisition must keep in use, when the harness knows a limit below the capacity, such as
    /// the context limit of a node's budget, whose capacity is unlimited; `None` checks the capacity alone.
    pub limit: Option<u64>,

    /// Every operation on the budget, in recording order, which under races can differ from the order the operations
    /// took effect in.
    pub operations: Vec<BudgetEvent>,
}

impl CapacityTrace {
    /// Returns the trace of a budget of `capacity` units from what its recording holds so far.
    #[cfg(any(test, feature = "test-util"))]
    pub fn recorded(capacity: u64, recording: &saluki_core::execution::BudgetRecording) -> Self {
        Self {
            capacity,
            limit: None,
            operations: recording.operations().into_iter().map(BudgetEvent::from).collect(),
        }
    }

    /// Returns this trace with every acquisition checked against `limit` as well as the capacity.
    pub fn with_limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Returns the units in use once every recorded operation took effect: what was granted, less what was released.
    pub fn held(&self) -> i128 {
        self.operations
            .iter()
            .map(|operation| match *operation {
                BudgetEvent::Acquire {
                    units, granted: true, ..
                } => i128::from(units),
                BudgetEvent::Acquire { granted: false, .. } => 0,
                BudgetEvent::Release { units, .. } => -i128::from(units),
            })
            .sum()
    }
}

/// The verdict of [`check_capacity`] and [`check_balance`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapacityVerdict {
    /// Held units never exceeded the limit, every refusal was justified, and the loss balance holds.
    Holds,

    /// The execution broke the capacity rules, as each message says.
    Violated(Vec<String>),

    /// The check could not decide, and why.
    Undecided {
        /// Why.
        reason: String,
    },
}

impl CapacityVerdict {
    /// Returns the verdict of both checks: violated if either is, with every message, then undecided if either is.
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Violated(mut first), Self::Violated(second)) => {
                first.extend(second);
                Self::Violated(first)
            }
            (violated @ Self::Violated(_), _) | (_, violated @ Self::Violated(_)) => violated,
            (undecided @ Self::Undecided { .. }, _) | (_, undecided @ Self::Undecided { .. }) => undecided,
            (Self::Holds, Self::Holds) => Self::Holds,
        }
    }
}

/// Checks one budget of a partitioned execution under capacity pressure, from the operations a harness recorded on
/// it, as each operation's own compare-and-swap observed the units in use.
///
/// Recordings from several threads are listed in recording order, not necessarily in the order their operations took
/// effect, so no operation is judged by what the recording lists before it. Instead:
///
/// 1. **Each operation is justified by itself.** A granted acquisition fits: the units it observed in use plus its own
///    are at most its bound, which is at most the capacity (and the trace's limit, if it has one), so the units held
///    never exceed the limit after any admission. A refused one does not fit: the units it observed, which count every
///    unit acquired and not yet returned, such as a node's unit held while a global acquisition is refused, leave no
///    room for its own. A release returns units that are in use.
/// 2. **Some order explains every observation.** The units in use start at 0; a granted acquisition moves them from
///    what it observed up by its units, and a release down by its units. Some order of all of these moves must start
///    each one where the previous one ended and pass every count a refusal observed: the moves form an Euler path
///    from 0 over the counts, which degrees and connectivity decide. A check followed by an acquisition, which two
///    holders can each pass with room for one unit, shows up here as two moves from the same count with no move back
///    between them.
///
/// The recording of one thread, in which operations are recorded in the order they took effect, is replayed in that
/// order first.
pub fn check_capacity(trace: &CapacityTrace) -> CapacityVerdict {
    let mut violations = Vec::new();
    for (index, operation) in trace.operations.iter().enumerate() {
        match *operation {
            BudgetEvent::Acquire {
                units,
                bound,
                observed,
                granted,
            } => {
                let most = trace.limit.map_or(trace.capacity, |limit| limit.min(trace.capacity));
                if bound > most {
                    violations.push(format!(
                        "operation {}: an acquisition was bounded by {} units, above the {} of {}",
                        index,
                        bound,
                        if trace.limit.is_some_and(|limit| limit < trace.capacity) {
                            "limit"
                        } else {
                            "budget's capacity"
                        },
                        most
                    ));
                }
                let fits = observed.checked_add(units).is_some_and(|total| total <= bound);
                if granted && !fits {
                    violations.push(format!(
                        "operation {}: {} unit(s) were granted with {} in use, beyond the bound of {}",
                        index, units, observed, bound
                    ));
                }
                if !granted && fits {
                    violations.push(format!(
                        "operation {}: {} unit(s) were refused with {} in use, although they fit below the bound of \
                         {}: the refusal is not justified",
                        index, units, observed, bound
                    ));
                }
            }
            BudgetEvent::Release { units, observed } => {
                if observed < units {
                    violations.push(format!(
                        "operation {}: {} unit(s) were released with only {} in use",
                        index, units, observed
                    ));
                }
            }
        }
    }
    if violations.is_empty() {
        if let Err(reason) = explained(&trace.operations) {
            violations.push(reason);
        }
    }
    if violations.is_empty() {
        CapacityVerdict::Holds
    } else {
        CapacityVerdict::Violated(violations)
    }
}

/// The count of units in use an operation observed.
fn observed_by(operation: &BudgetEvent) -> u64 {
    match *operation {
        BudgetEvent::Acquire { observed, .. } | BudgetEvent::Release { observed, .. } => observed,
    }
}

/// The count of units in use an operation left, if it moved the count: a granted acquisition or a release, each of
/// which [`check_capacity`] checked fits.
fn left_by(operation: &BudgetEvent) -> Option<u64> {
    match *operation {
        BudgetEvent::Acquire {
            units,
            observed,
            granted: true,
            ..
        } => Some(observed + units),
        BudgetEvent::Acquire { granted: false, .. } => None,
        BudgetEvent::Release { units, observed } => Some(observed - units),
    }
}

/// Returns the representative of `count`'s connected set of counts, compressing the path to it.
fn representative(parents: &mut BTreeMap<u64, u64>, count: u64) -> u64 {
    let mut root = count;
    while let Some(&parent) = parents.get(&root).filter(|parent| **parent != root) {
        root = parent;
    }
    let mut current = count;
    while let Some(&parent) = parents.get(&current).filter(|parent| **parent != current) {
        parents.insert(current, root);
        current = parent;
    }
    root
}

/// Returns whether some order of the operations takes effect with the units in use each one observed, starting from
/// none; see [`check_capacity`].
fn explained(operations: &[BudgetEvent]) -> Result<(), String> {
    // In recording order first: one thread's recording, or a race that happened to be recorded in order.
    let mut used = 0;
    let mut in_order = true;
    for operation in operations {
        in_order &= observed_by(operation) == used;
        if let Some(next) = left_by(operation) {
            used = next;
        }
    }
    if in_order {
        return Ok(());
    }

    let inconsistent = |detail: String| {
        Err(format!(
            "no order of the recorded operations takes effect with the units in use each one observed ({}); units \
             were granted that were not free, as a check followed by a separate acquisition grants them",
            detail
        ))
    };
    // Otherwise, an Eulerian path from 0 through every move that passes every refusal's count.
    let mut balance = BTreeMap::<u64, i64>::new();
    let mut parents = BTreeMap::<u64, u64>::new();
    parents.insert(0, 0);
    for operation in operations {
        let Some(to) = left_by(operation) else { continue };
        let from = observed_by(operation);
        *balance.entry(from).or_default() += 1;
        *balance.entry(to).or_default() -= 1;
        parents.entry(from).or_insert(from);
        parents.entry(to).or_insert(to);
        let (a, b) = (representative(&mut parents, from), representative(&mut parents, to));
        if a != b {
            parents.insert(a, b);
        }
    }
    let unbalanced = balance
        .iter()
        .filter(|(_, balance)| **balance != 0)
        .map(|(count, balance)| (*count, *balance))
        .collect::<Vec<_>>();
    let starts_at_zero = matches!(unbalanced.as_slice(), [] | [(0, 1), (_, -1)]);
    if !starts_at_zero {
        let detail = unbalanced
            .iter()
            .filter(|(_, balance)| *balance > 0)
            .map(|(count, balance)| format!("{} move(s) from {} unit(s) in use more than reach it", balance, count))
            .collect::<Vec<_>>();
        return inconsistent(detail.join("; "));
    }
    let start = representative(&mut parents, 0);
    let counts = parents.keys().copied().collect::<Vec<_>>();
    if let Some(stray) = counts
        .into_iter()
        .find(|count| representative(&mut parents, *count) != start)
    {
        return inconsistent(format!("no move from 0 unit(s) in use reaches {}", stray));
    }
    if let Some(refusal) = operations
        .iter()
        .find(|operation| left_by(operation).is_none() && !parents.contains_key(&observed_by(operation)))
    {
        return inconsistent(format!(
            "a refusal observed {} unit(s) in use, which no move reaches",
            observed_by(refusal)
        ));
    }
    Ok(())
}

/// What one partitioned execution admitted and delivered, for [`check_balance`]: counts of the points of the series a
/// harness follows, such as canaries whose every admitted point is delivered once natively, or every series of an
/// aggregation, whose points are its buckets.
///
/// A state owner that merges what it admits holds one point per bucket of a series. A kind replacing another drops
/// the points the series held, counted as policy loss, and the bucket the replacing admission fills holds a new
/// point, so with kind conflicts the points held over a run depend on the order of arrival: at least one per bucket
/// that admissions reached, and at most one per admission.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapacityBalance {
    /// The points admitted to the state owners: for a state owner that merges, one per bucket of a series that
    /// admissions reached.
    pub admitted: u64,

    /// The most points the state owners can have held: one per admission, for a state owner that merges and whose
    /// input has kind conflicts; `admitted` otherwise.
    pub admitted_at_most: u64,

    /// The points the outputs received, without idle-counter zeros, which no admission filled.
    pub delivered: u64,

    /// The points counted as capacity loss: refused at a context limit, or by an output.
    pub capacity_loss: u64,

    /// The points counted as policy loss, such as a kind replacing another.
    pub policy_loss: u64,

    /// The points discarded at shutdown: those of the bucket open at the close time.
    pub shutdown_loss: u64,

    /// The contexts the state owners admitted.
    pub admitted_contexts: BTreeSet<ContextIdentity>,

    /// The contexts the state owners emitted.
    pub emitted_contexts: BTreeSet<ContextIdentity>,
}

/// Checks the loss balance of a partitioned execution: every admitted point is delivered or counted as capacity,
/// policy, or shutdown loss, exactly once when the input has no kind conflict, and the state owners emit no context
/// they did not admit.
///
/// With kind conflicts the points accounted for lie between [`admitted`][CapacityBalance::admitted] and
/// [`admitted_at_most`][CapacityBalance::admitted_at_most]: fewer means a bucket was lost uncounted, and more means a
/// point was delivered or counted twice.
pub fn check_balance(balance: &CapacityBalance) -> CapacityVerdict {
    let mut violations = Vec::new();
    let accounted = u128::from(balance.delivered)
        + u128::from(balance.capacity_loss)
        + u128::from(balance.policy_loss)
        + u128::from(balance.shutdown_loss);
    let at_most = balance.admitted_at_most.max(balance.admitted);
    if accounted < u128::from(balance.admitted) || accounted > u128::from(at_most) {
        let admitted = if at_most == balance.admitted {
            balance.admitted.to_string()
        } else {
            format!("{} to {}", balance.admitted, at_most)
        };
        violations.push(format!(
            "{} point(s) were admitted, but {} were delivered, {} lost to capacity, {} lost to policy, and {} lost at \
             shutdown: {} in all",
            admitted, balance.delivered, balance.capacity_loss, balance.policy_loss, balance.shutdown_loss, accounted
        ));
    }
    for context in balance.emitted_contexts.difference(&balance.admitted_contexts) {
        violations.push(format!("{} was emitted but never admitted", context));
    }
    if violations.is_empty() {
        CapacityVerdict::Holds
    } else {
        CapacityVerdict::Violated(violations)
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::{
        context::Context, Histogram, HistogramPoints, MetricOrigin, SampleRate, ScalarPoints, SetPoints, SketchPoints,
    };
    use stringtheory::MetaString;

    use super::*;

    fn output(id: &str) -> OutputId {
        OutputId::new(id).unwrap()
    }

    fn context(name: &'static str, tags: &[&'static str]) -> Context {
        Context::from_static_parts(name, tags)
    }

    fn points_of(metrics: &[Metric]) -> OutputPoints {
        OutputPoints::new().with(&output("a"), metrics)
    }

    fn histogram(samples: &[(f64, f64)], timestamp: u64) -> Metric {
        let mut histogram = Histogram::default();
        for (value, rate) in samples {
            histogram.insert(*value, SampleRate::try_from(*rate).unwrap());
        }
        let mut values = MetricValues::histogram(HistogramPoints::from(histogram));
        values.set_timestamp(timestamp);
        Metric::from_parts(context("h", &[]), values, MetricMetadata::default())
    }

    #[test]
    fn records_flatten_into_one_point_per_point() {
        let metric = Metric::counter(context("requests", &["b:2", "a:1", "a:1"]), [(10, 1.0), (20, 2.0)]);
        let points = Point::flatten(&metric);
        assert_eq!(points.len(), 2);
        assert_eq!(
            points[0].context,
            ContextIdentity {
                name: "requests".into(),
                host: None,
                tags: vec!["a:1".into(), "b:2".into()],
                origin_tags: vec![],
            }
        );
        assert_eq!(points[0].kind, PointKind::Counter);
        assert_eq!(points[0].timestamp, NonZeroU64::new(10));
        assert_eq!(points[0].value, PointValue::Scalar(Scalar::new(1.0)));
        assert_eq!(points[1].timestamp, NonZeroU64::new(20));

        let set = Metric::set(context("users", &[]), SetPoints::from((5, ["x", "y"])));
        assert_eq!(
            Point::flatten(&set)[0].value,
            PointValue::Members(["x".to_string(), "y".to_string()].into_iter().collect())
        );

        // Histogram samples compare as a multiset of (value, weight).
        let histogram = histogram(&[(3.0, 1.0), (1.0, 0.5)], 7);
        assert_eq!(
            Point::flatten(&histogram)[0].value,
            PointValue::Samples(vec![
                Sample {
                    value: Scalar::new(1.0),
                    weight: Scalar::new(2.0)
                },
                Sample {
                    value: Scalar::new(3.0),
                    weight: Scalar::new(1.0)
                },
            ])
        );

        let distribution = Metric::distribution(context("latency", &[]), SketchPoints::from((9, [1.0, 2.0])));
        match &Point::flatten(&distribution)[0].value {
            PointValue::Sketch(sketch) => {
                assert_eq!(sketch.count, 2);
                assert_eq!(sketch.min, Scalar::new(1.0));
                assert_eq!(sketch.max, Scalar::new(2.0));
                assert_eq!(sketch.sum, Scalar::new(3.0));
                assert_eq!(sketch.bins.iter().map(|(_, count)| count).sum::<u32>(), 2);
            }
            other => panic!("expected a sketch, got {:?}", other),
        }
    }

    #[test]
    fn equal_multisets_in_different_orders_and_groupings_are_equivalent() {
        let a = context("a", &["env:prod"]);
        let b = context("b", &[]);
        let grouped = vec![
            Metric::gauge(a.clone(), [(10, 1.0), (20, 2.0)]),
            Metric::gauge(b.clone(), 5.0),
            Metric::gauge(b.clone(), 5.0),
        ];
        let regrouped = vec![
            Metric::gauge(b.clone(), 5.0),
            Metric::gauge(a.clone(), (20, 2.0)),
            Metric::gauge(b, 5.0),
            Metric::gauge(a, (10, 1.0)),
        ];
        assert_eq!(compare(&points_of(&grouped), &points_of(&regrouped)), Ok(()));

        // Tag order and repeated tags are not part of identity.
        let reordered_tags = Metric::counter(context("c", &["y:1", "x:1", "x:1"]), 1.0);
        let sorted_tags = Metric::counter(context("c", &["x:1", "y:1"]), 1.0);
        assert_eq!(
            compare(&points_of(&[reordered_tags]), &points_of(&[sorted_tags])),
            Ok(())
        );

        // An output that received nothing equals one that is not listed.
        let empty = OutputPoints::new().with(&output("b"), &[]);
        assert_eq!(compare(&empty, &OutputPoints::new()), Ok(()));
    }

    #[test]
    fn any_value_difference_is_not_equivalent() {
        let base = || Metric::gauge(context("m", &["env:prod"]), (100, 1.0));
        let with_values =
            |values: MetricValues| Metric::from_parts(context("m", &["env:prod"]), values, Default::default());
        let with_metadata = |metadata: MetricMetadata| {
            let (context, values, _) = base().into_parts();
            Metric::from_parts(context, values, metadata)
        };

        // (what differs, the differing metric)
        let cases: Vec<(&str, Metric)> = vec![
            ("value", Metric::gauge(context("m", &["env:prod"]), (100, 2.0))),
            ("sign of zero", Metric::gauge(context("m", &["env:prod"]), (100, -0.0))),
            ("timestamp", Metric::gauge(context("m", &["env:prod"]), (101, 1.0))),
            ("no timestamp", Metric::gauge(context("m", &["env:prod"]), 1.0)),
            ("name", Metric::gauge(context("n", &["env:prod"]), (100, 1.0))),
            ("tag", Metric::gauge(context("m", &["env:dev"]), (100, 1.0))),
            ("unset versus empty host", {
                let (context, values, metadata) = base().into_parts();
                Metric::from_parts(context.with_host(MetaString::empty()), values, metadata)
            }),
            ("origin tag", {
                let (context, values, metadata) = base().into_parts();
                let origin_tags = saluki_core::data_model::tags::TagSet::from_iter(["o:1".into()]);
                Metric::from_parts(context.with_origin_tags(origin_tags), values, metadata)
            }),
            ("kind", with_values(MetricValues::counter((100, 1.0)))),
            (
                "rate interval",
                with_values(MetricValues::rate((100, 1.0), Duration::from_secs(10))),
            ),
            ("unit", with_metadata(MetricMetadata::default().with_unit("ms"))),
            (
                "origin",
                with_metadata(MetricMetadata::default().with_origin(MetricOrigin::dogstatsd())),
            ),
        ];
        let expected = points_of(&[base()]);
        for (what, metric) in cases {
            let mismatch = compare(&expected, &points_of(&[metric])).expect_err(what);
            assert_eq!(mismatch.outputs.len(), 1, "{}", what);
            assert_eq!(mismatch.outputs[0].missing.len(), 1, "{}", what);
            assert_eq!(mismatch.outputs[0].unexpected.len(), 1, "{}", what);
        }

        // Rates with different intervals differ, and so do NaNs with different bits.
        let rate = |interval| Metric::rate(context("r", &[]), 1.0, Duration::from_secs(interval));
        assert!(compare(&points_of(&[rate(10)]), &points_of(&[rate(15)])).is_err());
        let nan = |bits| Metric::gauge(context("nan", &[]), f64::from_bits(bits));
        assert_eq!(
            compare(
                &points_of(&[nan(0x7ff8_0000_0000_0001)]),
                &points_of(&[nan(0x7ff8_0000_0000_0001)])
            ),
            Ok(())
        );
        assert!(compare(
            &points_of(&[nan(0x7ff8_0000_0000_0001)]),
            &points_of(&[nan(0x7ff8_0000_0000_0002)])
        )
        .is_err());

        // A repeated point is not the same as a single one.
        assert!(compare(&points_of(&[base(), base()]), &points_of(&[base()])).is_err());

        // Set members, histogram weights, and sketches are compared by value.
        let set = |members: [&'static str; 2]| Metric::set(context("s", &[]), SetPoints::from((5, members)));
        assert!(compare(&points_of(&[set(["a", "b"])]), &points_of(&[set(["a", "c"])])).is_err());
        assert!(compare(
            &points_of(&[histogram(&[(1.0, 1.0)], 5)]),
            &points_of(&[histogram(&[(1.0, 0.5)], 5)])
        )
        .is_err());
        let sketch = |values: [f64; 2]| Metric::distribution(context("d", &[]), SketchPoints::from((5, values)));
        assert!(compare(&points_of(&[sketch([1.0, 2.0])]), &points_of(&[sketch([1.0, 3.0])])).is_err());
        assert_eq!(
            compare(&points_of(&[sketch([1.0, 2.0])]), &points_of(&[sketch([2.0, 1.0])])),
            Ok(())
        );
    }

    #[test]
    fn mismatches_read_as_a_diff() {
        let expected = points_of(&[
            Metric::counter(context("requests", &["env:prod"]), (100, 1.0)),
            Metric::counter(context("errors", &[]), 2.0),
        ]);
        let actual = points_of(&[
            Metric::counter(context("requests", &["env:prod"]), (100, 3.0)),
            Metric::counter(context("errors", &[]), 2.0),
        ]);
        let mismatch = compare(&expected, &actual).unwrap_err();
        assert_eq!(
            mismatch.to_string(),
            "output `a`: 1 expected point(s) missing, 1 unexpected point(s)\n  missing    1x counter \"requests\" \
             host=<unset> tags=[env:prod] ts=100 value=1.0\n  unexpected 1x counter \"requests\" host=<unset> \
             tags=[env:prod] ts=100 value=3.0\n"
        );

        let set = Metric::set(context("users", &[]), SetPoints::from((5, ["b", "a"])));
        let unit = Metric::from_parts(
            context("latency", &[]),
            MetricValues::gauge(ScalarPoints::from(f64::NAN)),
            MetricMetadata::default().with_unit("ms"),
        );
        let mismatch = compare(&OutputPoints::new(), &points_of(&[set, unit])).unwrap_err();
        let text = mismatch.to_string();
        assert!(
            text.contains("unexpected 1x set \"users\" host=<unset> tags=[] ts=5 value={\"a\", \"b\"}"),
            "{}",
            text
        );
        assert!(
            text.contains("gauge \"latency\" host=<unset> tags=[] unit=\"ms\" ts=none value=NaN(0x7ff8000000000000)"),
            "{}",
            text
        );
    }

    #[test]
    fn reordered_comparison_accepts_sums_within_the_tolerance() {
        let tolerance = Tolerance::from_limits();
        assert_eq!(tolerance.relative, 1e-9);

        let counter = |value| Metric::counter(context("c", &[]), (100, value));
        let rate = |value| Metric::rate(context("r", &[]), (100, value), Duration::from_secs(10));
        // (expected value, actual value, equivalent)
        let cases = [
            (1.0, 1.0, true),
            (0.1 + 0.2 + 0.3, 0.3 + 0.2 + 0.1, true),
            (1e12, 1e12 + 1e2, true),
            (1e12, 1e12 + 1e4, false),
            (1.0, 1.0 + 1e-6, false),
            (0.0, -0.0, true),
            (f64::NAN, f64::NAN, true),
            (f64::NAN, 1.0, false),
            // An infinity matches only itself: never a finite value, whatever its size, nor the other infinity.
            (f64::INFINITY, f64::INFINITY, true),
            (5.0, f64::INFINITY, false),
            (f64::INFINITY, 5.0, false),
            (f64::MAX, f64::INFINITY, false),
            (f64::INFINITY, f64::NEG_INFINITY, false),
        ];
        for (expected, actual, equivalent) in cases {
            for make in [&counter as &dyn Fn(f64) -> Metric, &rate] {
                let verdict = compare_reordered(&points_of(&[make(expected)]), &points_of(&[make(actual)]), tolerance);
                assert_eq!(
                    verdict == ReorderedVerdict::Equivalent,
                    equivalent,
                    "{} vs {}: {:?}",
                    expected,
                    actual,
                    verdict
                );
            }
        }

        // Two points per bucket are paired after sorting; a missing point is never within tolerance.
        assert_eq!(
            compare_reordered(
                &points_of(&[counter(1.0), counter(2.0)]),
                &points_of(&[counter(2.0 + 1e-12), counter(1.0)]),
                tolerance
            ),
            ReorderedVerdict::Equivalent
        );
        assert!(matches!(
            compare_reordered(
                &points_of(&[counter(1.0), counter(2.0)]),
                &points_of(&[counter(3.0)]),
                tolerance
            ),
            ReorderedVerdict::NotEquivalent(_)
        ));
    }

    #[test]
    fn reordered_comparison_checks_set_unions_and_leaves_other_kinds_undecided() {
        let tolerance = Tolerance::from_limits();
        let set = |members: &[&'static str]| {
            let mut points = SetPoints::from((5, members[0]));
            for member in &members[1..] {
                points.merge(SetPoints::from((5, *member)));
            }
            Metric::set(context("s", &[]), points)
        };
        // The same members spread differently over the same number of points have the same union.
        assert_eq!(
            compare_reordered(
                &points_of(&[set(&["a", "b"]), set(&["c"])]),
                &points_of(&[set(&["a"]), set(&["b", "c"])]),
                tolerance
            ),
            ReorderedVerdict::Equivalent
        );
        assert!(matches!(
            compare_reordered(&points_of(&[set(&["a", "b"])]), &points_of(&[set(&["a"])]), tolerance),
            ReorderedVerdict::NotEquivalent(_)
        ));
        // Regrouping changes the number of points, which reordering never does: one point is not two.
        assert!(matches!(
            compare_reordered(
                &points_of(&[set(&["a", "b"])]),
                &points_of(&[set(&["a"]), set(&["b"])]),
                tolerance
            ),
            ReorderedVerdict::NotEquivalent(_)
        ));

        let gauge = |value| Metric::gauge(context("g", &[]), (100, value));
        match compare_reordered(&points_of(&[gauge(1.0)]), &points_of(&[gauge(2.0)]), tolerance) {
            ReorderedVerdict::Undecided { reason } => {
                assert!(reason.contains("gauge points of \"g\""), "{}", reason);
                assert!(reason.contains("stage 9"), "{}", reason);
            }
            other => panic!("expected an undecided verdict, got {:?}", other),
        }
        // Equal gauges need no check, and a definite difference elsewhere decides the verdict.
        assert_eq!(
            compare_reordered(&points_of(&[gauge(1.0)]), &points_of(&[gauge(1.0)]), tolerance),
            ReorderedVerdict::Equivalent
        );
        // A missing or extra point is a definite difference, whatever the kind.
        let histogram = histogram(&[(1.0, 1.0)], 100);
        for (expected, actual) in [
            (vec![gauge(1.0)], vec![]),
            (vec![gauge(1.0)], vec![gauge(1.0), gauge(2.0)]),
            (vec![histogram.clone()], vec![]),
            (vec![], vec![Metric::distribution(context("d", &[]), (100, 1.0))]),
        ] {
            match compare_reordered(&points_of(&expected), &points_of(&actual), tolerance) {
                ReorderedVerdict::NotEquivalent(_) => {}
                other => panic!("{:?} vs {:?}: expected a difference, got {:?}", expected, actual, other),
            }
        }
        let counter = |value| Metric::counter(context("c", &[]), (100, value));
        match compare_reordered(
            &points_of(&[gauge(1.0), counter(1.0)]),
            &points_of(&[gauge(2.0), counter(5.0)]),
            tolerance,
        ) {
            ReorderedVerdict::NotEquivalent(mismatch) => {
                assert_eq!(
                    mismatch.outputs[0].missing,
                    vec![(Point::flatten(&counter(1.0))[0].clone(), 1)]
                );
            }
            other => panic!("expected a difference, got {:?}", other),
        }
    }

    #[test]
    fn under_capacity_a_rewrite_may_only_reduce_capacity_loss() {
        let p = Metric::gauge(context("p", &[]), 1.0);
        let q = Metric::gauge(context("q", &[]), 1.0);
        let r = Metric::gauge(context("r", &[]), 1.0);
        let outcome = |delivered: &[&Metric], refused: &[&Metric]| CapacityOutcome {
            delivered: OutputPoints::new().with(&output("a"), delivered.iter().copied()),
            refused: OutputPoints::new().with(&output("a"), refused.iter().copied()),
        };
        // The policy delivers p twice and q once; the original, under pressure, lost one p to capacity.
        let policy = points_of(&[p.clone(), p.clone(), q.clone()]);
        let original = outcome(&[&p, &q], &[&p]);

        let cases = [
            ("the same delivery", outcome(&[&p, &q], &[&p]), None),
            ("less loss", outcome(&[&p, &p, &q], &[]), None),
            (
                "an excluded contribution",
                outcome(&[&p, &q, &r], &[&p]),
                Some("received 1x, but the policy delivers 0x"),
            ),
            (
                "loss moved to another contribution",
                outcome(&[&p, &p], &[&q]),
                Some("lost 1x, but the original lost 0x"),
            ),
            (
                "more refused in all",
                outcome(&[&p, &q], &[&p, &r]),
                Some("refused 2 point(s), but the original's refused 1"),
            ),
        ];
        for (case, rewritten, expected) in cases {
            let result = compare_under_capacity(&policy, &original, &rewritten);
            match expected {
                None => assert!(result.is_ok(), "{}: {}", case, result.unwrap_err()),
                Some(message) => {
                    let mismatch = result.expect_err(case);
                    assert_eq!(mismatch.violations.len(), 1, "{}: {}", case, mismatch);
                    assert!(mismatch.to_string().contains(message), "{}: {}", case, mismatch);
                }
            }
        }
    }

    fn key(name: &'static str, timestamp: u64) -> CandidateKey {
        (
            output("a"),
            ContextIdentity::of(&Metric::gauge(context(name, &[]), 0.0)),
            NonZeroU64::new(timestamp),
        )
    }

    fn point(id: u32) -> AdmissionPointId {
        AdmissionPointId::new(id)
    }

    fn partitioned(expected: &[Metric], actual: &[Metric], candidates: &Candidates) -> ReorderedVerdict {
        compare_partitioned(
            &points_of(expected),
            &points_of(actual),
            candidates,
            Tolerance::from_limits(),
        )
    }

    #[track_caller]
    fn assert_equivalent(verdict: ReorderedVerdict) {
        assert_eq!(verdict, ReorderedVerdict::Equivalent);
    }

    #[track_caller]
    fn assert_differs(verdict: ReorderedVerdict) {
        assert!(matches!(verdict, ReorderedVerdict::NotEquivalent(_)), "{:?}", verdict);
    }

    #[test]
    fn a_partitioned_gauge_is_the_last_value_of_one_of_the_points_that_contributed_to_its_bucket() {
        // Fixture X4: gauge `g`, bucket 100, from point 0 (values 1 then 2) and point 1 (value 3). Either point's last
        // value is a candidate, whichever lane interleaving delivered it; point 0's first value never is.
        let gauge = |value| Metric::gauge(context("g", &[]), (100, value));
        let mut candidates = Candidates::new();
        candidates.admit(key("g", 100), point(0), 0, PointKind::Gauge, Some(1.0));
        candidates.admit(key("g", 100), point(0), 0, PointKind::Gauge, Some(2.0));
        candidates.admit(key("g", 100), point(1), 0, PointKind::Gauge, Some(3.0));
        let expected = [gauge(3.0)];
        assert_equivalent(partitioned(&expected, &[gauge(3.0)], &candidates));
        assert_equivalent(partitioned(&expected, &[gauge(2.0)], &candidates));
        assert_differs(partitioned(&expected, &[gauge(1.0)], &candidates));
        assert_differs(partitioned(&expected, &[gauge(4.0)], &candidates));
        // Interleaving never changes how many points a bucket has.
        assert_differs(partitioned(&expected, &[gauge(2.0), gauge(3.0)], &candidates));
        assert_differs(partitioned(&expected, &[], &candidates));

        // With one contributing point, its last value is the only one.
        let mut one = Candidates::new();
        one.admit(key("g", 100), point(0), 0, PointKind::Gauge, Some(2.0));
        assert_equivalent(partitioned(&[gauge(2.0)], &[gauge(2.0)], &one));
        assert_differs(partitioned(&[gauge(2.0)], &[gauge(3.0)], &one));

        // A gauge without candidates, such as a histogram's aggregate, is a sum of what the bucket holds.
        let none = Candidates::new();
        assert_equivalent(partitioned(&[gauge(0.1 + 0.2)], &[gauge(0.2 + 0.1)], &none));
        assert_differs(partitioned(&[gauge(2.0)], &[gauge(3.0)], &none));
    }

    #[test]
    fn partitioned_counters_rates_and_sets_compare_as_reordered_ones_do() {
        let none = Candidates::new();
        let counter = |value| Metric::counter(context("c", &[]), (100, value));
        assert_equivalent(partitioned(
            &[counter(0.1 + 0.2 + 0.3)],
            &[counter(0.3 + 0.2 + 0.1)],
            &none,
        ));
        assert_differs(partitioned(&[counter(1.0)], &[counter(1.0 + 1e-6)], &none));
        let rate = |value| Metric::rate(context("r", &[]), (100, value), Duration::from_secs(10));
        assert_equivalent(partitioned(&[rate(1.0), rate(2.0)], &[rate(2.0), rate(1.0)], &none));
        assert_differs(partitioned(&[rate(1.0)], &[rate(2.0)], &none));

        let set = |members: &[&'static str]| {
            let mut points = SetPoints::from((5, members[0]));
            for member in &members[1..] {
                points.merge(SetPoints::from((5, *member)));
            }
            Metric::set(context("s", &[]), points)
        };
        assert_equivalent(partitioned(
            &[set(&["a", "b"]), set(&["c"])],
            &[set(&["c", "a"]), set(&["b"])],
            &none,
        ));
        assert_differs(partitioned(&[set(&["a", "b"])], &[set(&["a"])], &none));
    }

    #[test]
    fn partitioned_histograms_keep_their_samples_and_distributions_their_bins() {
        let none = Candidates::new();
        // A histogram's samples are a multiset: any order of insertion gives the same point.
        let forward = histogram(&[(1.0, 1.0), (2.0, 0.5), (3.0, 1.0)], 100);
        let backward = histogram(&[(3.0, 1.0), (2.0, 0.5), (1.0, 1.0)], 100);
        assert_equivalent(partitioned(std::slice::from_ref(&forward), &[backward], &none));
        assert_differs(partitioned(
            std::slice::from_ref(&forward),
            &[histogram(&[(1.0, 1.0), (2.0, 1.0), (3.0, 1.0)], 100)],
            &none,
        ));
        assert_differs(partitioned(
            &[forward],
            &[histogram(&[(1.0, 1.0), (2.0, 0.5)], 100)],
            &none,
        ));

        // Distributions: bins, count, minimum, and maximum exact; the sum and average within the tolerance.
        let sketch = |values: &[f64]| Metric::distribution(context("d", &[]), (100, values));
        let values = [0.1, 0.2, 0.3, 1e9];
        let reversed = [1e9, 0.3, 0.2, 0.1];
        assert_equivalent(partitioned(&[sketch(&values)], &[sketch(&reversed)], &none));
        assert_differs(partitioned(&[sketch(&values)], &[sketch(&[0.1, 0.2, 0.3])], &none));
        assert_differs(partitioned(&[sketch(&values)], &[sketch(&[0.1, 0.2, 0.3, 2e9])], &none));

        // A sum that differs beyond the tolerance, with the same bins, differs.
        let summed = |sum: f64| {
            let mut distribution = Point::flatten(&sketch(&values));
            if let PointValue::Sketch(sketch) = &mut distribution[0].value {
                sketch.sum = Scalar::new(sum);
            }
            let mut points = OutputPoints::new();
            points
                .0
                .entry(output("a"))
                .or_default()
                .insert(distribution.remove(0), 1);
            points
        };
        let exact = Point::flatten(&sketch(&values));
        let PointValue::Sketch(identity) = &exact[0].value else {
            panic!("a distribution flattens into a sketch");
        };
        let sum = identity.sum.get();
        let tolerance = Tolerance::from_limits();
        assert_eq!(
            compare_partitioned(&summed(sum), &summed(sum * (1.0 + 1e-12)), &none, tolerance),
            ReorderedVerdict::Equivalent
        );
        assert!(matches!(
            compare_partitioned(&summed(sum), &summed(sum * (1.0 + 1e-6)), &none, tolerance),
            ReorderedVerdict::NotEquivalent(_)
        ));
    }

    #[test]
    fn partitioned_metadata_and_kinds_are_checked_against_what_was_admitted() {
        let gauge = |unit: &'static str, value| {
            Metric::from_parts(
                context("g", &[]),
                MetricValues::gauge((100, value)),
                MetricMetadata::default().with_unit(MetaString::from_static(unit)),
            )
        };
        let unit = |unit: &'static str| MetadataIdentity::of(gauge(unit, 0.0).metadata());

        // First-seen metadata is one of what the entry was admitted with.
        let mut candidates = Candidates::new();
        candidates.admit(key("g", 100), point(0), 0, PointKind::Gauge, Some(1.0));
        candidates.add_metadata(key("g", 100), unit("second"));
        candidates.add_metadata(key("g", 100), unit("millisecond"));
        assert_equivalent(partitioned(
            &[gauge("second", 1.0)],
            &[gauge("millisecond", 1.0)],
            &candidates,
        ));
        assert_differs(partitioned(&[gauge("second", 1.0)], &[gauge("byte", 1.0)], &candidates));
        // Without metadata candidates, metadata must be equal.
        let mut values_only = Candidates::new();
        values_only.admit(key("g", 100), point(0), 0, PointKind::Gauge, Some(1.0));
        assert_differs(partitioned(
            &[gauge("second", 1.0)],
            &[gauge("millisecond", 1.0)],
            &values_only,
        ));

        // Without candidates, a kind is part of what must match.
        let counter = Metric::counter(context("g", &[]), (100, 7.0));
        let plain = Metric::gauge(context("g", &[]), (100, 1.0));
        assert_differs(partitioned(&[plain], &[counter], &Candidates::new()));
    }

    #[test]
    fn a_kind_conflict_within_one_points_bucket_keeps_that_points_order() {
        // Only point 0 sends `c`, a counter and then a gauge of 4, into one bucket: the gauge replaced the counter, and
        // every order of the points keeps point 0's, so the bucket holds the gauge. No interleaving delivers the
        // counter's value instead, a point at a timestamp nothing was admitted at, or nothing at all.
        let gauge = |value| Metric::gauge(context("c", &[]), (100, value));
        let counter = |timestamp, value| Metric::counter(context("c", &[]), (timestamp, value));
        let mut candidates = Candidates::new();
        candidates.admit(key("c", 100), point(0), 0, PointKind::Counter, None);
        candidates.admit(key("c", 100), point(0), 0, PointKind::Gauge, Some(4.0));
        assert!(candidates
            .get(&output("a"), &key("c", 100).1, NonZeroU64::new(100))
            .unwrap()
            .has_kind_conflict());
        let expected = [gauge(4.0)];
        assert_equivalent(partitioned(&expected, &[gauge(4.0)], &candidates));
        assert_differs(partitioned(&expected, &[counter(100, 1_000.0)], &candidates));
        assert_differs(partitioned(&expected, &[gauge(3.0)], &candidates));
        assert_differs(partitioned(&expected, &[counter(5, 1_000.0)], &candidates));
        assert_differs(partitioned(&expected, &[], &candidates));

        // With point 1's gauge in the next bucket delivered, which some order leaves the last to arrive, the first
        // bucket still holds point 0's last kind, if it holds anything.
        let mut two = candidates.clone();
        two.admit(key("c", 110), point(1), 0, PointKind::Gauge, Some(3.0));
        let later = Metric::gauge(context("c", &[]), (110, 3.0));
        let expected = [gauge(4.0), later.clone()];
        assert_equivalent(partitioned(&expected, &[gauge(4.0), later.clone()], &two));
        assert_equivalent(partitioned(&expected, &[gauge(4.0)], &two));
        assert_differs(partitioned(&expected, &[counter(100, 1_000.0), later], &two));
    }

    #[test]
    fn a_kind_conflict_across_points_delivers_one_of_each_buckets_last_kinds_and_some_points_last_admission() {
        // Point 0 admits `m` as a counter into [100, 110), then point 1 as a gauge of 3 into [110, 120). Whichever
        // arrives last replaced what the entry held, so an interleaving delivers both buckets, or only the bucket of
        // the last to arrive: each bucket holds its own point's kind, a gauge its value, and values of other kinds are
        // not compared.
        let counter = |timestamp, value| Metric::counter(context("m", &[]), (timestamp, value));
        let gauge = |timestamp, value| Metric::gauge(context("m", &[]), (timestamp, value));
        let mut candidates = Candidates::new();
        candidates.admit(key("m", 100), point(0), 0, PointKind::Counter, None);
        candidates.admit(key("m", 110), point(1), 0, PointKind::Gauge, Some(3.0));
        let expected = [counter(100, 7.0), gauge(110, 3.0)];
        for actual in [
            vec![counter(100, 7.0), gauge(110, 3.0)],
            vec![gauge(110, 3.0)],
            vec![counter(100, 7.0)],
            vec![counter(100, 9.0)],
        ] {
            assert_equivalent(partitioned(&expected, &actual, &candidates));
        }
        for actual in [
            // Nothing: the point whose admission arrived last always delivers it.
            vec![],
            // A kind no point admitted last into the bucket.
            vec![gauge(100, 3.0), gauge(110, 3.0)],
            vec![counter(110, 7.0)],
            // A gauge value that is no point's last.
            vec![gauge(110, 4.0)],
            // More points than the bucket has.
            vec![counter(100, 7.0), counter(100, 7.0), gauge(110, 3.0)],
            // A bucket no admission reached.
            vec![counter(100, 7.0), gauge(110, 3.0), gauge(120, 3.0)],
            // A set, which no point admitted.
            vec![Metric::set(context("m", &[]), SetPoints::from((110, "x")))],
        ] {
            assert_differs(partitioned(&expected, &actual, &candidates));
        }

        // When the reference itself lost its last admission, at shutdown, so does an interleaving whose last arrival
        // falls in the bucket open at the close: nothing need be delivered.
        assert_equivalent(partitioned(&[counter(100, 7.0)], &[], &candidates));

        // Another instance of the state owner holds its own entry: its single kind is compared exactly.
        let mut instances = candidates.clone();
        instances.admit(key("m", 200), point(0), 1, PointKind::Counter, None);
        let expected = [counter(100, 7.0), gauge(110, 3.0), counter(200, 2.0)];
        assert_equivalent(partitioned(
            &expected,
            &[gauge(110, 3.0), counter(200, 2.0)],
            &instances,
        ));
        assert_differs(partitioned(
            &expected,
            &[gauge(110, 3.0), counter(200, 5.0)],
            &instances,
        ));
    }

    #[test]
    fn a_partitioned_counters_zero_counts_as_no_point() {
        // Which ticks fire while a counter is idle depends on when heartbeats arrive, so a zero may be there or not.
        let rate = |timestamp, value| {
            Metric::from_parts(
                context("c", &[]),
                MetricValues::rate(ScalarPoints::from((timestamp, value)), Duration::from_secs(10)),
                MetricMetadata::default(),
            )
        };
        let none = Candidates::new();
        assert_equivalent(partitioned(&[rate(100, 1.0), rate(110, 0.0)], &[rate(100, 1.0)], &none));
        assert_equivalent(partitioned(&[rate(100, 1.0)], &[rate(100, 1.0), rate(110, 0.0)], &none));
        // Zeros of two lineages' rates, of different intervals, in one bucket, are no points either.
        let five = Metric::from_parts(
            context("c", &[]),
            MetricValues::rate(ScalarPoints::from((110, 0.0)), Duration::from_secs(5)),
            MetricMetadata::default(),
        );
        assert_equivalent(partitioned(&[rate(110, 0.0)], &[five], &none));
        // A value is never optional, and a zero does not stand in for one.
        assert_differs(partitioned(&[rate(100, 1.0), rate(110, 2.0)], &[rate(100, 1.0)], &none));
        assert_differs(partitioned(&[rate(110, 2.0)], &[rate(110, 0.0)], &none));
        // A gauge's zero is a value.
        let gauge = Metric::gauge(context("g", &[]), (110, 0.0));
        assert_differs(partitioned(&[gauge], &[], &none));
    }

    #[test]
    fn a_partitioned_mismatch_reports_only_the_buckets_that_differ() {
        let gauge = |name, value| Metric::gauge(context(name, &[]), (100, value));
        let mut candidates = Candidates::new();
        candidates.admit(key("g", 100), point(0), 0, PointKind::Gauge, Some(1.0));
        candidates.admit(key("g", 100), point(1), 0, PointKind::Gauge, Some(2.0));
        match partitioned(
            &[gauge("g", 1.0), gauge("h", 1.0)],
            &[gauge("g", 2.0), gauge("h", 5.0)],
            &candidates,
        ) {
            ReorderedVerdict::NotEquivalent(mismatch) => {
                assert_eq!(mismatch.outputs.len(), 1);
                assert_eq!(
                    mismatch.outputs[0].missing,
                    vec![(Point::flatten(&gauge("h", 1.0))[0].clone(), 1)]
                );
                assert_eq!(
                    mismatch.outputs[0].unexpected,
                    vec![(Point::flatten(&gauge("h", 5.0))[0].clone(), 1)]
                );
            }
            other => panic!("expected a difference, got {:?}", other),
        }
    }

    fn acquire(units: u64, bound: u64, observed: u64, granted: bool) -> BudgetEvent {
        BudgetEvent::Acquire {
            units,
            bound,
            observed,
            granted,
        }
    }

    fn release(units: u64, observed: u64) -> BudgetEvent {
        BudgetEvent::Release { units, observed }
    }

    fn capacity_of(capacity: u64, operations: Vec<BudgetEvent>) -> CapacityVerdict {
        check_capacity(&CapacityTrace {
            capacity,
            limit: None,
            operations,
        })
    }

    #[track_caller]
    fn assert_violated(verdict: CapacityVerdict, message: &str) {
        match verdict {
            CapacityVerdict::Violated(messages) => assert!(
                messages.iter().any(|reported| reported.contains(message)),
                "expected {:?} among {:?}",
                message,
                messages
            ),
            other => panic!("expected a violation, got {:?}", other),
        }
    }

    #[test]
    fn a_budget_recorded_in_order_holds_when_every_grant_fits_and_every_refusal_is_justified() {
        let operations = vec![
            acquire(1, 2, 0, true),
            acquire(1, 2, 1, true),
            acquire(1, 2, 2, false),
            release(1, 2),
            acquire(1, 2, 1, true),
        ];
        assert_eq!(capacity_of(2, operations.clone()), CapacityVerdict::Holds);
        assert_eq!(
            CapacityTrace {
                capacity: 2,
                limit: None,
                operations,
            }
            .held(),
            2
        );
        assert_eq!(capacity_of(0, Vec::new()), CapacityVerdict::Holds);
        assert_eq!(capacity_of(0, vec![acquire(1, 0, 0, false)]), CapacityVerdict::Holds);
    }

    #[test]
    fn every_operation_is_judged_by_the_units_in_use_it_observed_itself() {
        assert_violated(
            capacity_of(
                2,
                vec![acquire(1, 2, 0, true), acquire(1, 2, 1, true), acquire(1, 2, 2, true)],
            ),
            "granted with 2 in use, beyond the bound of 2",
        );
        assert_violated(
            capacity_of(3, vec![acquire(1, 3, 0, true), acquire(1, 3, 1, false)]),
            "the refusal is not justified",
        );
        assert_violated(
            capacity_of(3, vec![acquire(1, 4, 0, true)]),
            "above the budget's capacity of 3",
        );
        assert_violated(
            capacity_of(3, vec![acquire(1, 3, 0, true), release(2, 1)]),
            "released with only 1",
        );
        // A node's budget is unlimited; the context limit its acquisitions are bounded by is the harness's to check.
        let node = CapacityTrace {
            capacity: u64::MAX,
            limit: None,
            operations: vec![acquire(1, 5, 0, true)],
        };
        assert_eq!(check_capacity(&node), CapacityVerdict::Holds);
        assert_violated(check_capacity(&node.with_limit(3)), "above the limit of 3");
    }

    #[test]
    fn racing_recordings_hold_when_some_order_explains_what_each_operation_observed() {
        // Two lanes, recorded in the opposite order to the one their compare-and-swaps took effect in.
        assert_eq!(
            capacity_of(2, vec![acquire(1, 2, 1, true), acquire(1, 2, 0, true)]),
            CapacityVerdict::Holds
        );
        // A node's unit held while its context's global acquisition is refused still occupies the node: another lane's
        // refusal that observed it is justified, and its later grant follows the release.
        let operations = vec![
            acquire(1, 2, 0, true),  // lane A takes the node's first unit
            acquire(1, 2, 2, false), // lane C, refused while lane B holds the second
            acquire(1, 2, 1, true),  // lane B takes the second; its global acquisition is refused
            acquire(1, 2, 1, true),  // lane C again, after lane B gave its unit back
            release(1, 2),           // lane B gives its unit back
        ];
        assert_eq!(capacity_of(2, operations), CapacityVerdict::Holds);
    }

    #[test]
    fn a_check_followed_by_an_acquisition_is_caught_by_the_order_its_observations_need() {
        // Two lanes both see one free unit of two and both take it, as a check of `used` followed by an unconditional
        // acquisition would record: each looks justified alone, but no order makes both start from one unit in use.
        let racy = vec![acquire(1, 2, 0, true), acquire(1, 2, 1, true), acquire(1, 2, 1, true)];
        assert_violated(capacity_of(2, racy), "no order of the recorded operations");
        // A refusal that observed a count no move reaches is not explained either.
        assert_violated(
            capacity_of(
                5,
                vec![acquire(1, 5, 1, true), acquire(1, 5, 0, true), acquire(4, 5, 4, false)],
            ),
            "a refusal observed 4 unit(s) in use",
        );
        // Moves that never start from 0.
        assert_violated(
            capacity_of(5, vec![acquire(1, 5, 2, true), acquire(1, 5, 3, true)]),
            "no order of the recorded operations",
        );
        assert_violated(
            capacity_of(5, vec![acquire(6, 5, 0, false), acquire(5, 5, 3, false)]),
            "observed 3 unit(s) in use, which no move reaches",
        );
    }

    #[test]
    fn a_real_budget_shared_by_racing_threads_records_what_the_capacity_check_accepts() {
        use saluki_core::execution::StateBudget;
        let (budget, recording) = StateBudget::recorded(8);
        std::thread::scope(|scope| {
            for thread in 0..4u64 {
                let budget = budget.clone();
                scope.spawn(move || {
                    let mut held = 0;
                    for step in 0..2_000u64 {
                        if (step + thread) % 3 == 2 && held > 0 {
                            budget.release(1);
                            held -= 1;
                        } else if budget.try_acquire_below(1, 6 + thread % 2) {
                            held += 1;
                        }
                    }
                    budget.release(held);
                });
            }
        });
        let trace = CapacityTrace::recorded(8, &recording);
        assert_eq!(trace.operations.len(), 4 * 2_000 + 4);
        assert_eq!(check_capacity(&trace), CapacityVerdict::Holds);
        assert_eq!(trace.held(), 0);
        assert_violated(check_capacity(&trace.with_limit(6)), "above the limit of 6");
    }

    #[test]
    fn the_loss_balance_accounts_for_every_admitted_point_once() {
        let identity = |name| ContextIdentity::of(&Metric::counter(context(name, &[]), 1.0));
        let balance = CapacityBalance {
            admitted: 10,
            admitted_at_most: 10,
            delivered: 6,
            capacity_loss: 2,
            policy_loss: 1,
            shutdown_loss: 1,
            admitted_contexts: [identity("a"), identity("b")].into_iter().collect(),
            emitted_contexts: [identity("a")].into_iter().collect(),
        };
        assert_eq!(check_balance(&balance), CapacityVerdict::Holds);
        assert_violated(
            check_balance(&CapacityBalance {
                delivered: 8,
                ..balance.clone()
            }),
            "10 point(s) were admitted, but 8 were delivered",
        );
        // With kind conflicts, a bucket refilled after a replacement holds a new point: between one per bucket and one
        // per admission are accounted for, never fewer, which would be a bucket lost uncounted.
        let conflicts = CapacityBalance {
            admitted_at_most: 14,
            policy_loss: 4,
            ..balance.clone()
        };
        assert_eq!(check_balance(&conflicts), CapacityVerdict::Holds);
        assert_violated(
            check_balance(&CapacityBalance {
                delivered: 2,
                ..conflicts.clone()
            }),
            "10 to 14 point(s) were admitted, but 2 were delivered",
        );
        assert_violated(
            check_balance(&CapacityBalance {
                policy_loss: 6,
                ..conflicts.clone()
            }),
            "6 lost to policy",
        );
        assert_violated(
            check_balance(&CapacityBalance {
                emitted_contexts: [identity("c")].into_iter().collect(),
                ..balance.clone()
            }),
            "was emitted but never admitted",
        );
        assert_eq!(
            check_balance(&balance).and(capacity_of(1, vec![acquire(1, 1, 0, true)])),
            CapacityVerdict::Holds
        );
        assert!(matches!(
            CapacityVerdict::Holds.and(CapacityVerdict::Undecided { reason: "?".into() }),
            CapacityVerdict::Undecided { .. }
        ));
        assert_eq!(
            CapacityVerdict::Violated(vec!["a".into()]).and(CapacityVerdict::Violated(vec!["b".into()])),
            CapacityVerdict::Violated(vec!["a".into(), "b".into()])
        );
    }
}
