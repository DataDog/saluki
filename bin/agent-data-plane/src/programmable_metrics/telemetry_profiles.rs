//! Telemetry profiles: the native telemetry identity of each reserved node of the default program.
//!
//! Telemetry identity lives outside the IR. A reserved node, whose ID derives from a native component's, reports its
//! telemetry under that component's identity (its `component_id` and `component_type` labels), and keeps the native
//! metric names of its [profile](TelemetryProfile), as long as its operator is the one the profile is for, whatever its
//! provenance. A node in the policy namespace has no native identity. Memory accounting is not part of the identity:
//! a kernel's memory and CPU time are accounted under its region, in a subcomponent named after its node.
//!
//! The policy gateways are routing operators: a `split` whose `true` port is emitted to the gateway's output and whose
//! `false` port drops. A routing group's filter keeps the native allowlist metrics; the failover gateways report no
//! filter telemetry natively, and keep none.
//!
//! # Where values come from
//!
//! Each kept metric says where its value comes from in the plan-driven topology, and so who records it (see
//! [`MetricSource`]). A kernel records its own. A `split` has no kernel, so the region host running it counts what
//! leaves its ports, and the installer records what describes the configuration a generation runs with, when the
//! generation commits: the length of a bound list, the rules a tag filterlist puts in effect, and how many
//! configuration changes changed a binding. These are the values the native components report: the native listener
//! filterlist reports its configured list's length and counts its live updates, the native tag filterlist reports its
//! rule count, and a native filter counts the metrics it drops.

use saluki_core::components::ComponentType;
use saluki_metric_plan::{
    ir::{Namespace, NodeId},
    operators::names,
};

/// The kind of a native metric.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeMetricKind {
    /// A counter.
    Counter,

    /// A gauge.
    Gauge,
}

/// Where the value of a native metric comes from in the plan-driven topology, which says who records it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricSource {
    /// The node's kernel records it, through the telemetry its factory registers for the node's identity.
    Kernel,

    /// The number of events the node sends on the named output port, such as the port of a `split` that leads to its
    /// `drop`. The region host running the node counts them: a `split` has no kernel.
    PortEvents(&'static str),

    /// The number of values in the list bound to the named binding, as configured, repeats included. The installer
    /// sets it whenever it installs a generation.
    BindingLength(&'static str),

    /// The number of entries a name matcher keeps for the list bound to the named binding: its distinct names, or, for
    /// a list of prefixes, the prefixes that no other prefix in it covers. The installer sets it whenever it installs a
    /// generation.
    MatcherEntries {
        /// The binding, with `{key}` standing for the key of a keyed binding.
        binding: &'static str,

        /// Whether the list holds prefixes.
        prefixes: bool,
    },

    /// The number of configuration changes, after the first installation, that changed the value of any of the named
    /// bindings, all of them live. The installer counts them, as it observes configuration changes.
    BindingChanges(&'static [&'static str]),

    /// The number of rules a `tag_projection` node's resolved `rules` and `value_rules` put in effect: one per metric
    /// name with tag rules, plus one per metric prefix and tag with a value rule, as the native tag filterlist counts
    /// them. The installer sets it whenever it installs a generation, so that it describes the rules of the generation
    /// that is active, never those of one still being prepared.
    TagProjectionRules,
}

/// A native metric a profile keeps, reported with its node's `component_id` and `component_type` labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeMetric {
    /// The metric's name.
    pub name: &'static str,

    /// The metric's kind.
    pub kind: NativeMetricKind,

    /// Tags the metric carries beside the node's labels, as `key:value`.
    pub tags: &'static [&'static str],

    /// Where the metric's value comes from.
    pub source: MetricSource,
}

const fn counter(name: &'static str, source: MetricSource) -> NativeMetric {
    NativeMetric {
        name,
        kind: NativeMetricKind::Counter,
        tags: &[],
        source,
    }
}

const fn gauge(name: &'static str, source: MetricSource) -> NativeMetric {
    NativeMetric {
        name,
        kind: NativeMetricKind::Gauge,
        tags: &[],
        source,
    }
}

/// The native telemetry a reserved node keeps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryProfile {
    /// The node reports nothing of its own, such as a routing operator inside a native component, or one that stands
    /// for a component's output fan-out.
    Silent,

    /// The node is a host component (a source or aggregation), which reports its own telemetry as it does natively.
    Host,

    /// The DogStatsD mapper, whose context resolver and result cache are named `<component>/dsd_mapper/primary` and
    /// `<component>/dsd_mapper/result_cache`.
    Mapper,

    /// The listener filterlist of the DogStatsD prefix filter.
    ListenerFilterlist,

    /// The no-aggregation timestamp split. The native batch duration histogram and buffer-full drop counter belong to
    /// the native passthrough batcher, which the plan does not have.
    TimestampSplit,

    /// The tag filterlist, whose context cache is named `tag_filterlist/context_cache`.
    TagFilterlist,

    /// The post-aggregation filter.
    PostAggregateFilter,

    /// The filter of an endpoint routing group.
    EndpointAllowlist,
}

const LISTENER_FILTERLIST_METRICS: &[NativeMetric] = {
    use MetricSource::*;
    &[
        gauge(
            "metric_filterlist_size",
            BindingLength("dogstatsd.metric_filter.values"),
        ),
        counter(
            "metric_filterlist_updates_total",
            BindingChanges(&["dogstatsd.metric_filter.values", "dogstatsd.metric_filter.match_prefix"]),
        ),
        // One per metric the filterlist drops, whatever its number of points.
        counter("dogstatsd_listener_filtered_points_total", PortEvents("true")),
    ]
};

const TIMESTAMP_SPLIT_METRICS: &[NativeMetric] = &[
    counter("aggregate_passthrough_metrics_total", MetricSource::Kernel),
    counter("aggregate_passthrough_flushes_total", MetricSource::Kernel),
];

const TAG_FILTERLIST_METRICS: &[NativeMetric] = {
    use MetricSource::*;
    &[
        counter("tag_filterlist_rule_hits_total", Kernel),
        counter("tag_filterlist_rule_misses_total", Kernel),
        counter("tag_filterlist_noop_hits_total", Kernel),
        counter("tag_filterlist_metrics_modified_total", Kernel),
        counter("tag_filterlist_tags_filtered_total", Kernel),
        // A kernel is built while its generation is prepared, which may never commit.
        gauge("tag_filterlist_size", TagProjectionRules),
        // A change of rules builds a new kernel, which cannot know it replaces one.
        counter(
            "tag_filterlist_updates_total",
            BindingChanges(&["dogstatsd.tag_filterlist"]),
        ),
    ]
};

const POST_AGGREGATE_FILTER_METRICS: &[NativeMetric] = &[counter(
    "dogstatsd_post_aggregate_filtered_metrics_total",
    MetricSource::PortEvents("true"),
)];

const ENDPOINT_ALLOWLIST_METRICS: &[NativeMetric] = {
    use MetricSource::*;
    &[
        // The split's `true` port leads to the group's output, so what the native filter drops leaves on `false`.
        counter("metrics_endpoint_allowlist_metrics_filtered_total", PortEvents("false")),
        NativeMetric {
            tags: &["match_type:exact"],
            ..gauge(
                "metrics_endpoint_allowlist_size",
                MatcherEntries {
                    binding: "metrics_endpoint_routing.group.{key}.names",
                    prefixes: false,
                },
            )
        },
        NativeMetric {
            tags: &["match_type:prefix"],
            ..gauge(
                "metrics_endpoint_allowlist_size",
                MatcherEntries {
                    binding: "metrics_endpoint_routing.group.{key}.prefixes",
                    prefixes: true,
                },
            )
        },
    ]
};

impl TelemetryProfile {
    /// Returns the native metrics the profile keeps, each with where its value comes from.
    ///
    /// A host component's metrics are its own and are not listed.
    pub const fn metrics(self) -> &'static [NativeMetric] {
        match self {
            Self::Silent | Self::Host | Self::Mapper => &[],
            Self::ListenerFilterlist => LISTENER_FILTERLIST_METRICS,
            Self::TimestampSplit => TIMESTAMP_SPLIT_METRICS,
            Self::TagFilterlist => TAG_FILTERLIST_METRICS,
            Self::PostAggregateFilter => POST_AGGREGATE_FILTER_METRICS,
            Self::EndpointAllowlist => ENDPOINT_ALLOWLIST_METRICS,
        }
    }
}

/// The native telemetry identity of a reserved node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTelemetry {
    /// The `component_id` label: the native component the node stands for.
    pub component_id: String,

    /// The `component_type` label.
    pub component_type: ComponentType,

    /// The native metrics the node keeps.
    pub profile: TelemetryProfile,
}

/// One row of the profile table.
///
/// In `node` and `component`, `{i}` stands for the index of an endpoint routing group.
struct ProfileEntry {
    node: &'static str,
    operator: &'static str,
    component: &'static str,
    component_type: ComponentType,
    profile: TelemetryProfile,
}

const fn entry(
    node: &'static str, operator: &'static str, component: &'static str, component_type: ComponentType,
    profile: TelemetryProfile,
) -> ProfileEntry {
    ProfileEntry {
        node,
        operator,
        component,
        component_type,
        profile,
    }
}

const INDEX: &str = "{i}";

static PROFILES: &[ProfileEntry] = {
    use ComponentType::{Destination, Encoder, Source, Transform};
    use TelemetryProfile::*;
    &[
        // Sources.
        entry("dsd_in", names::SOURCE, "dsd_in", Source, Host),
        entry("otlp_in", names::SOURCE, "otlp_in", Source, Host),
        entry("checks_ipc_in", names::SOURCE, "checks_ipc_in", Source, Host),
        entry("liveness_in", names::SOURCE, "liveness_in", Source, Host),
        // The trace sampler is a transform whose `metrics` output is a source of the plan.
        entry("trace_sampler", names::SOURCE, "trace_sampler", Transform, Host),
        // DogStatsD pre-aggregation, aggregation, and the post-aggregation filter.
        entry("dsd_enrich", names::DOGSTATSD_MAPPER, "dsd_enrich", Transform, Mapper),
        entry(
            "dsd_prefix_filter",
            names::NAMESPACE_PREFIX,
            "dsd_prefix_filter",
            Transform,
            Silent,
        ),
        entry(
            "dsd_prefix_filter_blocklist",
            names::SPLIT,
            "dsd_prefix_filter",
            Transform,
            ListenerFilterlist,
        ),
        entry(
            "dsd_prefix_filter_blocklist_true",
            names::DROP,
            "dsd_prefix_filter",
            Transform,
            Silent,
        ),
        entry(
            "dsd_no_agg_split",
            names::TIMESTAMP_SPLIT,
            "dsd_no_agg_split",
            Transform,
            TimestampSplit,
        ),
        entry(
            "dsd_tag_filterlist",
            names::TAG_PROJECTION,
            "dsd_tag_filterlist",
            Transform,
            TagFilterlist,
        ),
        entry("dsd_agg", names::AGGREGATE, "dsd_agg", Transform, Host),
        entry(
            "dsd_post_agg_filter_in",
            names::MERGE,
            "dsd_post_agg_filter",
            Transform,
            Silent,
        ),
        entry(
            "dsd_post_agg_filter",
            names::SPLIT,
            "dsd_post_agg_filter",
            Transform,
            PostAggregateFilter,
        ),
        entry(
            "dsd_post_agg_filter_true",
            names::DROP,
            "dsd_post_agg_filter",
            Transform,
            Silent,
        ),
        entry(
            "dsd_post_agg_filter_fanout",
            names::TEE,
            "dsd_client_telemetry_out",
            Destination,
            Silent,
        ),
        entry(
            "dsd_post_agg_filter_fanout_0",
            names::EMIT,
            "dsd_client_telemetry_out",
            Destination,
            Silent,
        ),
        // Metrics enrichment and its fan-out to the outputs.
        entry("metrics_enrich_in", names::MERGE, "metrics_enrich", Transform, Silent),
        entry(
            "metrics_enrich",
            names::HOST_ENRICHMENT,
            "metrics_enrich",
            Transform,
            Silent,
        ),
        entry(
            "metrics_enrich_host_tags",
            names::HOST_TAGS,
            "metrics_enrich",
            Transform,
            Silent,
        ),
        entry("metrics_policy", names::TEE, "metrics_enrich", Transform, Silent),
        entry("metrics_policy_0", names::EMIT, "dd_metrics_encode", Encoder, Silent),
        // Endpoint routing groups.
        entry(
            "metrics_routing_filter_{i}",
            names::SPLIT,
            "metrics_routing_filter_{i}",
            Transform,
            EndpointAllowlist,
        ),
        entry(
            "metrics_routing_filter_{i}_true",
            names::EMIT,
            "metrics_routing_encode_{i}",
            Encoder,
            Silent,
        ),
        entry(
            "metrics_routing_filter_{i}_false",
            names::DROP,
            "metrics_routing_filter_{i}",
            Transform,
            Silent,
        ),
        // The failover gateways, neither of which reports filter telemetry natively.
        entry(
            "mrf_metrics_gateway",
            names::SPLIT,
            "mrf_metrics_gateway",
            Transform,
            Silent,
        ),
        entry(
            "mrf_metrics_gateway_true",
            names::EMIT,
            "mrf_metrics_encode",
            Encoder,
            Silent,
        ),
        entry(
            "mrf_metrics_gateway_false",
            names::DROP,
            "mrf_metrics_gateway",
            Transform,
            Silent,
        ),
        entry(
            "af_metrics_gateway",
            names::SPLIT,
            "af_metrics_gateway",
            Transform,
            Silent,
        ),
        entry(
            "af_metrics_gateway_true",
            names::EMIT,
            "af_metrics_encode",
            Encoder,
            Silent,
        ),
        entry(
            "af_metrics_gateway_false",
            names::DROP,
            "af_metrics_gateway",
            Transform,
            Silent,
        ),
    ]
};

/// Matches `local` against a node pattern, returning the routing group index `{i}` stands for, if any.
fn match_pattern(pattern: &str, local: &str) -> Option<Option<String>> {
    match pattern.split_once(INDEX) {
        None => (pattern == local).then_some(None),
        Some((before, after)) => {
            let index = local.strip_prefix(before)?.strip_suffix(after)?;
            let canonical = !index.is_empty()
                && index.bytes().all(|b| b.is_ascii_digit())
                && (index == "0" || !index.starts_with('0'));
            canonical.then(|| Some(index.to_string()))
        }
    }
}

/// Returns the native telemetry identity of the node `node` running `operator`, if it has one.
///
/// Only reserved nodes have one, and only while their operator is the one their profile is for.
pub fn native_telemetry(node: &NodeId, operator: &str) -> Option<NativeTelemetry> {
    if node.namespace() != Namespace::Reserved {
        return None;
    }
    let local: &str = node.local().as_ref();
    PROFILES.iter().find_map(|entry| {
        if entry.operator != operator {
            return None;
        }
        let index = match_pattern(entry.node, local)?;
        let component_id = match index {
            Some(index) => entry.component.replace(INDEX, &index),
            None => entry.component.to_string(),
        };
        Some(NativeTelemetry {
            component_id,
            component_type: entry.component_type,
            profile: entry.profile,
        })
    })
}

#[cfg(test)]
mod tests;
