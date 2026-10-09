//! Resource and timing limits.
//!
//! [`LIMITS`] lists every limit that plans are validated or run against, with its default and where its value comes
//! from. Limits on what an artifact may contain, such as its size or node count, apply only to nodes with artifact
//! provenance, so that large native configurations stay valid.
//!
//! Configured limits, such as the aggregation context limit, are global: they mean the same at any degree of
//! parallelism. Scheduling limits, such as the batch size, are local to one execution unit.
//!
//! Code that enforces a limit reads its typed value from [`values`]; the table describes the same values.

use std::time::Duration;

/// The value of every limit, typed for the code that enforces it.
///
/// The [table][crate::limits::LIMITS] is built from these, so the two cannot disagree.
pub mod values {
    use std::time::Duration;

    /// The default number of contexts aggregation holds.
    pub const AGGREGATION_CONTEXT_LIMIT: u64 = 1_000_000;

    /// The minimum delay between preparing a cut and the cut taking effect.
    pub const CUT_MARGIN: Duration = Duration::from_secs(1);

    /// The maximum serialized size of an artifact, in bytes.
    pub const ARTIFACT_SIZE: u64 = 256 * 1024;

    /// The maximum number of nodes in an artifact.
    pub const ARTIFACT_NODES: u64 = 256;

    /// The maximum number of predicate leaves in an artifact.
    pub const PREDICATE_LEAVES: u64 = 1024;

    /// The maximum number of output ports of one copying node in an artifact.
    pub const TEE_FAN_OUT: u64 = 32;

    /// The maximum number of node visits one input record can cause.
    pub const WORK_PER_INPUT: u64 = 4096;

    /// The time allowed for every region to prepare a generation.
    pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);

    /// The time a retiring generation's outputs have to carry out their closure behavior.
    pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

    /// The maximum number of generations alive at once.
    pub const OVERLAP_BUDGET: u64 = 4;

    /// The longest an idle admission point waits before sending a heartbeat.
    pub const HEARTBEAT_PERIOD: Duration = Duration::from_secs(5);

    /// The backward wall-clock jump that resets an admission clock.
    pub const CLOCK_RESET_THRESHOLD: Duration = Duration::from_secs(600);

    /// The maximum serialized size of one blob, in bytes.
    pub const BLOB_SIZE: u64 = 64 * 1024;

    /// The number of entries in each decision's memo.
    pub const DECISION_MEMO_ENTRIES: u64 = 4096;

    /// The maximum number of events in one batch: the capacity of an events buffer.
    pub const BATCH_SIZE: usize = 1024;

    /// The maximum number of batches a region processes between checks for control messages and timers.
    pub const WORK_PER_TURN: u64 = 8;

    /// The relative tolerance for comparing floating-point sums under reordering.
    pub const FLOAT_TOLERANCE: f64 = 1e-9;
}

/// The value of a limit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LimitValue {
    /// A number of items, such as nodes or entries.
    Count(u64),

    /// A number of bytes.
    Bytes(u64),

    /// A length of time.
    Duration(Duration),

    /// A set of allowed lengths of time.
    Durations(&'static [Duration]),

    /// A relative tolerance.
    Ratio(f64),
}

/// Where a limit's value comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitSource {
    /// The limit is fixed at its default.
    Fixed,

    /// The limit is a parameter of an operator, so each node sets it, and lowering binds it to native configuration in
    /// the default program. The table gives the native default.
    Parameter {
        /// The operator.
        operator: &'static str,

        /// The parameter.
        parameter: &'static str,
    },
}

/// One limit: its name, its default, and where its value comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimitSpec {
    /// The limit's name.
    pub name: &'static str,

    /// The default value.
    pub default: LimitValue,

    /// Where the limit's value comes from.
    pub source: LimitSource,

    /// What the limit bounds, and what happens at the limit.
    pub description: &'static str,
}

/// The bucket widths that aggregation may use: every divisor of 60 seconds, and 120, 180, and 300 seconds.
///
/// One plan uses at most one width above 60 seconds, so the least common multiple of a plan's widths is the larger of
/// 60 seconds and its largest width.
pub const ALLOWED_BUCKET_WIDTHS: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(3),
    Duration::from_secs(4),
    Duration::from_secs(5),
    Duration::from_secs(6),
    Duration::from_secs(10),
    Duration::from_secs(12),
    Duration::from_secs(15),
    Duration::from_secs(20),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(180),
    Duration::from_secs(300),
];

/// Returns `true` if `width` is an allowed bucket width.
pub fn is_allowed_bucket_width(width: Duration) -> bool {
    ALLOWED_BUCKET_WIDTHS.contains(&width)
}

/// The maximum number of contexts held in aggregation state.
pub const AGGREGATION_CONTEXT_LIMIT: LimitSpec = LimitSpec {
    name: "aggregation_context_limit",
    default: LimitValue::Count(values::AGGREGATION_CONTEXT_LIMIT),
    source: LimitSource::Parameter {
        operator: "aggregate",
        parameter: "context_limit",
    },
    description: "Maximum number of contexts held in aggregation state, including idle counters. When full, new \
                  contexts are dropped and counted as capacity loss until a flush brings the count below the limit. \
                  During a cut, both generations count against this one budget.",
};

/// The bucket widths aggregation may use.
///
/// # Missing
///
/// Nothing enforces the restriction yet. The specification introduces it with aggregation as a kernel: from then on,
/// validation rejects an `aggregate` or `timestamp_split` window that is not an allowed width, a plan with more than
/// one width above 60 seconds, and a `timestamp_split` whose window differs from its aggregation's, and native
/// configuration rejects other widths at startup. Until then aggregation accepts any whole number of seconds, as
/// native aggregation does.
pub const BUCKET_WIDTHS: LimitSpec = LimitSpec {
    name: "bucket_widths",
    default: LimitValue::Durations(ALLOWED_BUCKET_WIDTHS),
    source: LimitSource::Fixed,
    description:
        "Allowed aggregation bucket widths: every divisor of 60 seconds, and 120, 180, and 300 seconds. A plan \
         uses at most one width above 60 seconds. Enforced once aggregation is a kernel; until then any whole \
         number of seconds is accepted, as natively.",
};

/// The minimum delay between preparing a cut and the cut taking effect.
pub const CUT_MARGIN: LimitSpec = LimitSpec {
    name: "cut_margin",
    default: LimitValue::Duration(values::CUT_MARGIN),
    source: LimitSource::Fixed,
    description: "Minimum time between the end of preparation and the cut time, which is the first common bucket \
                  boundary at least this far in the future.",
};

/// The maximum serialized size of an artifact, including its blobs.
pub const ARTIFACT_SIZE: LimitSpec = LimitSpec {
    name: "artifact_size",
    default: LimitValue::Bytes(values::ARTIFACT_SIZE),
    source: LimitSource::Fixed,
    description: "Maximum size of an artifact's JSON, including its blobs. Larger artifacts are rejected.",
};

/// The maximum number of nodes in an artifact.
pub const ARTIFACT_NODES: LimitSpec = LimitSpec {
    name: "artifact_nodes",
    default: LimitValue::Count(values::ARTIFACT_NODES),
    source: LimitSource::Fixed,
    description: "Maximum number of nodes in an artifact. Telemetry is reported per node, so this also bounds the \
                  telemetry cardinality an artifact author controls.",
};

/// The maximum number of predicate leaves in an artifact.
pub const PREDICATE_LEAVES: LimitSpec = LimitSpec {
    name: "predicate_leaves",
    default: LimitValue::Count(values::PREDICATE_LEAVES),
    source: LimitSource::Fixed,
    description: "Maximum number of predicate leaves, across all predicates of an artifact.",
};

/// The maximum number of output ports of one `tee`.
pub const TEE_FAN_OUT: LimitSpec = LimitSpec {
    name: "tee_fan_out",
    default: LimitValue::Count(values::TEE_FAN_OUT),
    source: LimitSource::Fixed,
    description: "Maximum number of output ports of one tee in an artifact.",
};

/// The maximum number of node visits one input can cause in the worst case.
pub const WORK_PER_INPUT: LimitSpec = LimitSpec {
    name: "work_per_input",
    default: LimitValue::Count(values::WORK_PER_INPUT),
    source: LimitSource::Fixed,
    description: "Maximum number of node visits that one input record can cause in the worst case, counting every \
                  path it can take and the declared expansion of every operator on them.",
};

/// The time allowed for every region to prepare a new generation.
pub const PREPARE_TIMEOUT: LimitSpec = LimitSpec {
    name: "prepare_timeout",
    default: LimitValue::Duration(values::PREPARE_TIMEOUT),
    source: LimitSource::Fixed,
    description: "Time allowed for every region to acknowledge a prepared generation. On timeout the installation is \
                  aborted and the active generation keeps running.",
};

/// The time a retiring generation's outputs have to carry out their closure behavior.
pub const DRAIN_DEADLINE: LimitSpec = LimitSpec {
    name: "drain_deadline",
    default: LimitValue::Duration(values::DRAIN_DEADLINE),
    source: LimitSource::Fixed,
    description: "Time a retiring generation's outputs and recipients have to attempt their queued work before it is \
                  handled as at process shutdown. This changes behavior, not only tuning.",
};

/// The maximum number of generations alive at once.
pub const OVERLAP_BUDGET: LimitSpec = LimitSpec {
    name: "overlap_budget",
    default: LimitValue::Count(values::OVERLAP_BUDGET),
    source: LimitSource::Fixed,
    description: "Maximum number of generations alive at once: the active one, the last-known-good one, and two \
                  draining. A candidate beyond the budget waits, as pending, until a generation retires. This changes \
                  behavior, not only tuning.",
};

/// The longest an idle admission point waits before sending a heartbeat.
pub const HEARTBEAT_PERIOD: LimitSpec = LimitSpec {
    name: "heartbeat_period",
    default: LimitValue::Duration(values::HEARTBEAT_PERIOD),
    source: LimitSource::Fixed,
    description: "Longest an idle admission point waits before sending a heartbeat. The effective period is the \
                  smaller of this and the smallest flush interval of the state owners downstream, so idle sources \
                  never delay a flush.",
};

/// The backward wall-clock jump that resets an admission clock.
pub const CLOCK_RESET_THRESHOLD: LimitSpec = LimitSpec {
    name: "clock_reset_threshold",
    default: LimitValue::Duration(values::CLOCK_RESET_THRESHOLD),
    source: LimitSource::Fixed,
    description: "Backward wall-clock jump beyond which an admission clock resets: every state owner it feeds closes \
                  its open buckets, then the clock restarts from the wall clock. Smaller jumps are absorbed. At least \
                  the largest bucket width.",
};

/// The maximum size of one blob.
pub const BLOB_SIZE: LimitSpec = LimitSpec {
    name: "blob_size",
    default: LimitValue::Bytes(values::BLOB_SIZE),
    source: LimitSource::Fixed,
    description: "Maximum size of one blob's JSON. Blobs also count toward the artifact size limit.",
};

/// The number of entries in each decision's memo.
pub const DECISION_MEMO_ENTRIES: LimitSpec = LimitSpec {
    name: "decision_memo_entries",
    default: LimitValue::Count(values::DECISION_MEMO_ENTRIES),
    source: LimitSource::Fixed,
    description: "Number of entries in the memo of each routing decision, keyed by context and kind. The memo has a \
                  fixed capacity and evicts without allocating.",
};

/// The maximum number of events in one batch.
pub const BATCH_SIZE: LimitSpec = LimitSpec {
    name: "batch_size",
    default: LimitValue::Count(values::BATCH_SIZE as u64),
    source: LimitSource::Fixed,
    description: "Maximum number of events in one batch, which is the unit of stateless work.",
};

/// The maximum number of batches a region processes between checks for control messages and timers.
pub const WORK_PER_TURN: LimitSpec = LimitSpec {
    name: "work_per_turn",
    default: LimitValue::Count(values::WORK_PER_TURN),
    source: LimitSource::Fixed,
    description: "Maximum number of batches a region processes before checking for control messages and timers, so \
                  that heavy input cannot starve them.",
};

/// The relative tolerance for comparing floating-point sums under reordering.
pub const FLOAT_TOLERANCE: LimitSpec = LimitSpec {
    name: "float_tolerance",
    default: LimitValue::Ratio(values::FLOAT_TOLERANCE),
    source: LimitSource::Fixed,
    description: "Relative tolerance when checking equivalence of floating-point sums whose inputs may be reordered, \
                  as under partitioned execution.",
};

/// Every limit.
pub const LIMITS: &[LimitSpec] = &[
    AGGREGATION_CONTEXT_LIMIT,
    BUCKET_WIDTHS,
    CUT_MARGIN,
    ARTIFACT_SIZE,
    ARTIFACT_NODES,
    PREDICATE_LEAVES,
    TEE_FAN_OUT,
    WORK_PER_INPUT,
    PREPARE_TIMEOUT,
    DRAIN_DEADLINE,
    OVERLAP_BUDGET,
    HEARTBEAT_PERIOD,
    CLOCK_RESET_THRESHOLD,
    BLOB_SIZE,
    DECISION_MEMO_ENTRIES,
    BATCH_SIZE,
    WORK_PER_TURN,
    FLOAT_TOLERANCE,
];

/// Returns the limit with the given name, if there is one.
pub fn limit(name: &str) -> Option<&'static LimitSpec> {
    LIMITS.iter().find(|spec| spec.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use saluki_core::topology::EventsBuffer;

    use super::*;

    fn duration(spec: &LimitSpec) -> Duration {
        match spec.default {
            LimitValue::Duration(duration) => duration,
            other => panic!("{} is not a duration: {:?}", spec.name, other),
        }
    }

    fn count(spec: &LimitSpec) -> u64 {
        match spec.default {
            LimitValue::Count(count) | LimitValue::Bytes(count) => count,
            other => panic!("{} is not a count: {:?}", spec.name, other),
        }
    }

    #[test]
    fn limits_are_named_uniquely_and_described() {
        let names = LIMITS.iter().map(|spec| spec.name).collect::<HashSet<_>>();
        assert_eq!(names.len(), LIMITS.len(), "limit names are unique");
        for spec in LIMITS {
            assert!(crate::ir::is_identifier(spec.name), "{} is an identifier", spec.name);
            assert!(
                spec.description.ends_with('.'),
                "{} has a complete description",
                spec.name
            );
            assert_eq!(limit(spec.name), Some(spec));
        }
        assert_eq!(limit("no_such_limit"), None);
    }

    #[test]
    fn known_values_match_the_specification() {
        assert_eq!(count(&AGGREGATION_CONTEXT_LIMIT), 1_000_000);
        assert_eq!(duration(&CUT_MARGIN), Duration::from_secs(1));
        assert_eq!(count(&ARTIFACT_SIZE), 262_144);
        assert_eq!(count(&ARTIFACT_NODES), 256);
        assert_eq!(count(&PREDICATE_LEAVES), 1024);
        assert_eq!(count(&TEE_FAN_OUT), 32);
        assert_eq!(count(&WORK_PER_INPUT), 4096);
        assert_eq!(duration(&PREPARE_TIMEOUT), Duration::from_secs(10));
        assert_eq!(duration(&DRAIN_DEADLINE), Duration::from_secs(30));
        assert_eq!(count(&OVERLAP_BUDGET), 4);
        assert_eq!(count(&BLOB_SIZE), 65_536);
        assert_eq!(count(&DECISION_MEMO_ENTRIES), 4096);
        assert_eq!(count(&WORK_PER_TURN), 8);
        assert_eq!(FLOAT_TOLERANCE.default, LimitValue::Ratio(1e-9));
    }

    #[test]
    fn only_the_context_limit_comes_from_an_operator_parameter() {
        for spec in LIMITS {
            let expected = if spec.name == AGGREGATION_CONTEXT_LIMIT.name {
                LimitSource::Parameter {
                    operator: "aggregate",
                    parameter: "context_limit",
                }
            } else {
                LimitSource::Fixed
            };
            assert_eq!(spec.source, expected, "{}", spec.name);
        }
        // The parameter exists.
        let aggregate = crate::operators::OperatorRegistry::native().get("aggregate").unwrap();
        assert!(aggregate.params().iter().any(|param| param.name == "context_limit"));
    }

    #[test]
    fn typed_values_are_the_tables_defaults() {
        assert_eq!(count(&BATCH_SIZE), values::BATCH_SIZE as u64);
        assert_eq!(values::BATCH_SIZE, EventsBuffer::default().capacity());
        assert_eq!(count(&WORK_PER_INPUT), values::WORK_PER_INPUT);
        assert_eq!(duration(&PREPARE_TIMEOUT), values::PREPARE_TIMEOUT);
        assert_eq!(FLOAT_TOLERANCE.default, LimitValue::Ratio(values::FLOAT_TOLERANCE));
    }

    #[test]
    fn limits_respect_the_relations_the_specification_requires() {
        // The clock-reset threshold is at least the largest bucket width.
        let largest_width = ALLOWED_BUCKET_WIDTHS.iter().max().copied().unwrap();
        assert!(duration(&CLOCK_RESET_THRESHOLD) >= largest_width);

        // The heartbeat period is at most the native flush interval of 15 seconds.
        assert!(duration(&HEARTBEAT_PERIOD) <= Duration::from_secs(15));

        // A blob fits in an artifact.
        assert!(count(&BLOB_SIZE) <= count(&ARTIFACT_SIZE));

        // The batch size is the capacity of the native event buffer.
        assert_eq!(count(&BATCH_SIZE), EventsBuffer::default().capacity() as u64);
    }

    #[test]
    fn bucket_widths_are_divisors_of_a_minute_or_a_few_longer_widths() {
        for secs in 1..=600 {
            let expected = (60 % secs == 0) || [120, 180, 300].contains(&secs);
            assert_eq!(
                is_allowed_bucket_width(Duration::from_secs(secs)),
                expected,
                "{} seconds",
                secs
            );
        }
        assert!(!is_allowed_bucket_width(Duration::from_millis(1500)));
        assert!(!is_allowed_bucket_width(Duration::ZERO));
        assert_eq!(BUCKET_WIDTHS.default, LimitValue::Durations(ALLOWED_BUCKET_WIDTHS));
    }
}
