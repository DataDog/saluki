//! Operator registry and declarative effect contracts.
//!
//! Every [`Op`] declares what fields of a metric it reads and writes, which runtime signals it depends on, whether it
//! can drop metrics, and whether it holds state across batches. Validation, static analysis, optimization, and
//! human-readable `explain` output all read from this single source of truth.

use std::fmt;

use crate::ir::{Op, DEFAULT_OUT_PORT, SPLIT_AGGREGATE_PORT, SPLIT_PASSTHROUGH_PORT};

/// Input arity accepted by an operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortArity {
    /// Accepts no incoming edges (sources).
    Zero,

    /// Accepts exactly one incoming edge.
    One,

    /// Accepts one or more incoming edges (`Merge`).
    OneOrMore,
}

/// Bitflag set describing metric fields and runtime bindings touched by an operator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EffectFlags(u16);

impl EffectFlags {
    /// No effects.
    pub const NONE: Self = Self(0);
    /// Metric name.
    pub const NAME: Self = Self(1 << 0);
    /// Metric tags (instrumented or origin).
    pub const TAGS: Self = Self(1 << 1);
    /// Metric values or value kind.
    pub const VALUES: Self = Self(1 << 2);
    /// Per-sample timestamps.
    pub const TIMESTAMPS: Self = Self(1 << 3);
    /// Metric metadata (such as unit).
    pub const METADATA: Self = Self(1 << 4);
    /// Runtime host-tags binding.
    pub const HOST_TAGS_BINDING: Self = Self(1 << 5);
    /// Runtime mirroring signal binding.
    pub const MIRRORING_SIGNAL: Self = Self(1 << 6);

    /// Combines two flag sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns `true` if all flags in `other` are set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Returns `true` when no flags are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for EffectFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }

        let mut parts = Vec::new();
        if self.contains(Self::NAME) {
            parts.push("name");
        }
        if self.contains(Self::TAGS) {
            parts.push("tags");
        }
        if self.contains(Self::VALUES) {
            parts.push("values");
        }
        if self.contains(Self::TIMESTAMPS) {
            parts.push("timestamps");
        }
        if self.contains(Self::METADATA) {
            parts.push("metadata");
        }
        if self.contains(Self::HOST_TAGS_BINDING) {
            parts.push("binding:host_tags");
        }
        if self.contains(Self::MIRRORING_SIGNAL) {
            parts.push("binding:mirroring");
        }
        f.write_str(&parts.join(","))
    }
}

/// Static specification and effect contract for an [`Op`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorSpec {
    /// Canonical operator name.
    pub name: &'static str,

    /// Allowed incoming edge arity.
    pub input_arity: PortArity,

    /// Valid outgoing port names.
    pub output_ports: &'static [&'static str],

    /// Whether a single output port may fan out to multiple downstream edges (`Tee`).
    pub allows_fanout: bool,

    /// Metric fields and runtime bindings read by the operator.
    pub reads: EffectFlags,

    /// Metric fields mutated by the operator.
    pub writes: EffectFlags,

    /// Whether the operator can drop incoming metrics.
    pub may_drop: bool,

    /// Whether the operator maintains state across batches/time windows.
    pub stateful: bool,
}

static OUT_ONLY: &[&str] = &[DEFAULT_OUT_PORT];
static SPLIT_PORTS: &[&str] = &[SPLIT_AGGREGATE_PORT, SPLIT_PASSTHROUGH_PORT];
static NO_PORTS: &[&str] = &[];

/// Returns the static [`OperatorSpec`] for `op`.
pub fn operator_spec(op: &Op) -> OperatorSpec {
    match op {
        Op::Source { .. } => OperatorSpec {
            name: "source",
            input_arity: PortArity::Zero,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NONE,
            writes: EffectFlags::NONE,
            may_drop: false,
            stateful: false,
        },
        Op::Map { .. } => OperatorSpec {
            name: "map",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NAME,
            writes: EffectFlags::NAME.union(EffectFlags::TAGS),
            may_drop: false,
            stateful: false,
        },
        Op::PrefixFilter { .. } => OperatorSpec {
            name: "prefix_filter",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NAME.union(EffectFlags::VALUES),
            writes: EffectFlags::NAME,
            may_drop: true,
            stateful: false,
        },
        Op::TimestampSplit => OperatorSpec {
            name: "timestamp_split",
            input_arity: PortArity::One,
            output_ports: SPLIT_PORTS,
            allows_fanout: false,
            reads: EffectFlags::VALUES.union(EffectFlags::TIMESTAMPS),
            writes: EffectFlags::VALUES,
            may_drop: true,
            stateful: false,
        },
        Op::TagFilter { .. } => OperatorSpec {
            name: "tag_filter",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NAME.union(EffectFlags::TAGS).union(EffectFlags::VALUES),
            writes: EffectFlags::TAGS,
            may_drop: false,
            stateful: false,
        },
        Op::Aggregate { .. } => OperatorSpec {
            name: "aggregate",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NAME
                .union(EffectFlags::TAGS)
                .union(EffectFlags::VALUES)
                .union(EffectFlags::TIMESTAMPS)
                .union(EffectFlags::METADATA),
            writes: EffectFlags::NAME
                .union(EffectFlags::VALUES)
                .union(EffectFlags::TIMESTAMPS)
                .union(EffectFlags::METADATA),
            may_drop: true,
            stateful: true,
        },
        Op::PostAggregateFilter { .. } => OperatorSpec {
            name: "post_aggregate_filter",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NAME.union(EffectFlags::VALUES),
            writes: EffectFlags::NONE,
            may_drop: true,
            stateful: false,
        },
        Op::EnrichHostTags { .. } => OperatorSpec {
            name: "enrich_host_tags",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::HOST_TAGS_BINDING,
            writes: EffectFlags::TAGS,
            may_drop: false,
            stateful: false,
        },
        Op::RouteFilter { kind } => {
            let reads = match kind {
                crate::ir::RouteFilterKind::Mirroring {
                    signal_binding: Some(_),
                    ..
                } => EffectFlags::NAME.union(EffectFlags::MIRRORING_SIGNAL),
                crate::ir::RouteFilterKind::SeriesAllowlist { .. } => EffectFlags::NAME.union(EffectFlags::VALUES),
                _ => EffectFlags::NAME,
            };
            OperatorSpec {
                name: "route_filter",
                input_arity: PortArity::One,
                output_ports: OUT_ONLY,
                allows_fanout: false,
                reads,
                writes: EffectFlags::NONE,
                may_drop: true,
                stateful: false,
            }
        }
        Op::Merge => OperatorSpec {
            name: "merge",
            input_arity: PortArity::OneOrMore,
            output_ports: OUT_ONLY,
            allows_fanout: false,
            reads: EffectFlags::NONE,
            writes: EffectFlags::NONE,
            may_drop: false,
            stateful: false,
        },
        Op::Tee => OperatorSpec {
            name: "tee",
            input_arity: PortArity::One,
            output_ports: OUT_ONLY,
            allows_fanout: true,
            reads: EffectFlags::NONE,
            writes: EffectFlags::NONE,
            may_drop: false,
            stateful: false,
        },
        Op::Emit { .. } => OperatorSpec {
            name: "emit",
            input_arity: PortArity::One,
            output_ports: NO_PORTS,
            allows_fanout: false,
            reads: EffectFlags::NONE,
            writes: EffectFlags::NONE,
            may_drop: false,
            stateful: false,
        },
        Op::Drop => OperatorSpec {
            name: "drop",
            input_arity: PortArity::One,
            output_ports: NO_PORTS,
            allows_fanout: false,
            reads: EffectFlags::NONE,
            writes: EffectFlags::NONE,
            may_drop: true,
            stateful: false,
        },
    }
}
