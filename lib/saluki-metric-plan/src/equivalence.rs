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
//! oracle for executions that may reorder inputs across admission contexts, as partitioned execution does: there,
//! floating-point sums may differ within a [tolerance][Tolerance], and set points are compared by their union.
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

use saluki_core::data_model::event::metric::{Metric, MetricMetadata, MetricOrigin, MetricValues, SketchPoints};

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
}
