//! Whole-program metric pipeline intermediate representation (IR).

use std::{fmt, num::NonZeroU64};

use saluki_metric_kernels::{
    filterlist::MetricNameFilter,
    histogram::HistogramConfiguration,
    mapper::DogStatsDMapperProfile,
    routing_filter::MetricMirroring,
    tag_filter::{MetricTagFilterEntry, MetricTagValueAllowlistEntry},
};
use serde::{Deserialize, Serialize};

/// Stable identifier for a node inside a [`MetricPlan`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Creates a new `NodeId`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for NodeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Default output port name for single-output operators.
pub const DEFAULT_OUT_PORT: &str = "out";

/// Output port carrying non-timestamped metrics out of [`Op::TimestampSplit`].
pub const SPLIT_AGGREGATE_PORT: &str = "aggregate";

/// Output port carrying timestamped metrics out of [`Op::TimestampSplit`].
pub const SPLIT_PASSTHROUGH_PORT: &str = "passthrough";

/// A directed connection from an operator output port to a downstream node input.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Edge {
    /// Upstream node ID.
    pub from: NodeId,

    /// Output port name on `from`.
    pub from_port: String,

    /// Downstream node ID.
    pub to: NodeId,
}

impl Edge {
    /// Creates a new edge from `from` on `from_port` to `to`.
    pub fn new(from: impl Into<NodeId>, from_port: impl Into<String>, to: impl Into<NodeId>) -> Self {
        Self {
            from: from.into(),
            from_port: from_port.into(),
            to: to.into(),
        }
    }

    /// Creates a new edge using the default `"out"` port.
    pub fn default_port(from: impl Into<NodeId>, to: impl Into<NodeId>) -> Self {
        Self::new(from, DEFAULT_OUT_PORT, to)
    }
}

/// Kind of routing filter predicate executed by [`Op::RouteFilter`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RouteFilterKind {
    /// Endpoint allowlist matching normalized exact names or literal prefixes.
    Allowlist {
        /// Exact metric names permitted through.
        names: Vec<String>,
        /// Literal prefixes permitted through.
        prefixes: Vec<String>,
    },

    /// Multi-region mirroring filter gated by a startup toggle, literal policy, and optional live signal.
    Mirroring {
        /// Startup-level enablement flag for the mirroring branch.
        enabled: bool,
        /// Literal mirroring policy (used when no runtime signal overrides it).
        policy: MetricMirroring,
        /// Optional live signal binding name that supplies updated [`MetricMirroring`] state at runtime.
        signal_binding: Option<String>,
    },

    /// Scalar-series allowlist (counter, rate, gauge, set) for autoscaling failover.
    SeriesAllowlist {
        /// Whether the failover branch is enabled.
        enabled: bool,
        /// Exact metric names permitted through.
        metrics: Vec<String>,
    },
}

/// A single composable operator in a [`MetricPlan`].
///
/// Policy settings are stored as literals. Only runtime host facts (`EnrichHostTags`) and live operational signals
/// (`RouteFilterKind::Mirroring::signal_binding`) are resolved from runtime bindings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op {
    /// Named input stream entering the metric plan (for example, `"dogstatsd"` or `"otlp"`).
    Source {
        /// Logical stream name.
        stream: String,
    },

    /// DogStatsD metric-name mapper rewriting matching metric names and extracting tags.
    Map {
        /// Mapping profiles evaluated in order.
        profiles: Vec<DogStatsDMapperProfile>,
    },

    /// Applies a namespace prefix (respecting `metric_prefix_blocklist`) and listener-side metric filterlist.
    PrefixFilter {
        /// Prefix prepended to matching metrics. Empty disables prefixing.
        metric_prefix: String,
        /// Metric prefixes exempted from receiving `metric_prefix`.
        metric_prefix_blocklist: Vec<String>,
        /// Blocklist filter policy.
        filter: MetricNameFilter,
    },

    /// Splits metrics by per-sample timestamp presence.
    ///
    /// Non-timestamped values leave on `"aggregate"`. Supported timestamped values (gauges, counters converted to 10s
    /// rates, rates) leave on `"passthrough"`. Unsupported timestamped types are dropped.
    TimestampSplit,

    /// Filters or rewrites tags on counter and sketch-backed metrics before aggregation.
    TagFilter {
        /// Whole-tag include/exclude rules.
        entries: Vec<MetricTagFilterEntry>,
        /// Per-prefix tag value allowlist rules.
        value_allowlists: Vec<MetricTagValueAllowlistEntry>,
    },

    /// Stateful time-windowed metric aggregation.
    ///
    /// May appear zero, one, or multiple times anywhere in a valid plan DAG.
    Aggregate {
        /// Bucket width in seconds.
        window_duration_secs: NonZeroU64,
        /// Maximum number of retained contexts.
        context_limit: usize,
        /// Idle counter keep-alive expiration in seconds (`0` disables keep-alive).
        counter_expiry_secs: u64,
        /// Whether open windows are flushed on shutdown.
        flush_open_windows: bool,
        /// Histogram expansion settings.
        histogram: HistogramConfiguration,
    },

    /// Drops post-aggregation scalar metrics whose generated histogram names match the filterlist.
    PostAggregateFilter {
        /// Blocklist filter policy.
        filter: MetricNameFilter,
        /// Configured histogram aggregate names (such as `"avg"`, `"max"`).
        histogram_aggregates: Vec<String>,
        /// Configured histogram percentiles (such as `"0.95"`).
        histogram_percentiles: Vec<String>,
    },

    /// Attaches runtime host tags from the named runtime binding while active.
    EnrichHostTags {
        /// Name of the runtime binding supplying host tags (for example, `"host_tags"`).
        binding: String,
    },

    /// Filters metrics for a specific downstream routing branch.
    RouteFilter {
        /// Routing predicate configuration.
        kind: RouteFilterKind,
    },

    /// Merges multiple upstream streams into one output stream.
    Merge,

    /// Duplicates each incoming metric onto every outgoing edge connected to `"out"`.
    Tee,

    /// Terminal sink delivering metrics to a named downstream output (encoder or destination).
    Emit {
        /// Logical output target name (for example, `"primary"`, `"routing_0"`, `"mrf"`).
        target: String,
    },

    /// Terminal sink discarding every metric it receives.
    Drop,
}

/// One node in a [`MetricPlan`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Unique identifier within the plan.
    pub id: NodeId,

    /// Operator executed by this node.
    pub op: Op,
}

impl Node {
    /// Creates a new `Node`.
    pub fn new(id: impl Into<NodeId>, op: Op) -> Self {
        Self { id: id.into(), op }
    }
}

/// A whole-program metric processing plan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricPlan {
    /// Schema version of the plan IR.
    pub version: u32,

    /// Optional human-readable description (such as `"native-default"`).
    pub description: Option<String>,

    /// Nodes in the plan.
    pub nodes: Vec<Node>,

    /// Directed edges connecting node output ports to downstream nodes.
    pub edges: Vec<Edge>,
}

impl MetricPlan {
    /// Current IR schema version.
    pub const CURRENT_VERSION: u32 = 1;

    /// Returns a builder for constructing a [`MetricPlan`].
    pub fn builder() -> MetricPlanBuilder {
        MetricPlanBuilder::default()
    }

    /// Looks up a node by ID.
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    /// Serializes this plan to pretty-printed JSON.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserializes a plan from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Fluent builder for constructing a [`MetricPlan`].
#[derive(Default)]
pub struct MetricPlanBuilder {
    description: Option<String>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl MetricPlanBuilder {
    /// Sets a human-readable description on the plan.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Adds a node and returns `self`.
    pub fn node(mut self, id: impl Into<NodeId>, op: Op) -> Self {
        self.nodes.push(Node::new(id, op));
        self
    }

    /// Adds a node in place.
    pub fn add_node(&mut self, id: impl Into<NodeId>, op: Op) -> NodeId {
        let id = id.into();
        self.nodes.push(Node::new(id.clone(), op));
        id
    }

    /// Connects `from` (default `"out"` port) to `to`.
    pub fn edge(mut self, from: impl Into<NodeId>, to: impl Into<NodeId>) -> Self {
        self.edges.push(Edge::default_port(from, to));
        self
    }

    /// Connects `from` on `from_port` to `to`.
    pub fn edge_port(mut self, from: impl Into<NodeId>, from_port: impl Into<String>, to: impl Into<NodeId>) -> Self {
        self.edges.push(Edge::new(from, from_port, to));
        self
    }

    /// Connects `from` (default `"out"` port) to `to` in place.
    pub fn add_edge(&mut self, from: impl Into<NodeId>, to: impl Into<NodeId>) {
        self.edges.push(Edge::default_port(from, to));
    }

    /// Connects `from` on `from_port` to `to` in place.
    pub fn add_edge_port(&mut self, from: impl Into<NodeId>, from_port: impl Into<String>, to: impl Into<NodeId>) {
        self.edges.push(Edge::new(from, from_port, to));
    }

    /// Connects a chain of node IDs pairwise along their default `"out"` ports.
    pub fn add_chain(&mut self, ids: &[NodeId]) {
        for pair in ids.windows(2) {
            self.add_edge(pair[0].clone(), pair[1].clone());
        }
    }

    /// Finalizes and returns the [`MetricPlan`].
    pub fn build(self) -> MetricPlan {
        MetricPlan {
            version: MetricPlan::CURRENT_VERSION,
            description: self.description,
            nodes: self.nodes,
            edges: self.edges,
        }
    }
}
