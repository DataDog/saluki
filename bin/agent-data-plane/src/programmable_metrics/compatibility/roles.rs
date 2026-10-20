//! Roles: the series a node reports because of its name, beside what its operator reports.
//!
//! An operator describes the series its kernel records ([`OperatorDef::reports`][reports]), whatever the node's name.
//! A few nodes of the default program stand for a native component whose series no operator can describe generically,
//! because their name depends on the component: a `split` is the listener filterlist, the post-aggregation filter, or
//! an endpoint routing group's allowlist, and each reports that filter's native series. The remapper reads those
//! series by component, and it is shared with native mode, so they cannot become generic. A node with a role's name,
//! running its operator, reports the role's series under its name; any other node reports only what its operator does.
//!
//! [reports]: saluki_metric_plan::operators::OperatorDef::reports

use saluki_metric_plan::{
    ir::NodeId,
    operators::{names, PlanValue, Recorder, ReportedSeries},
};

/// The series a node with a given name and operator reports because of its role.
#[derive(Debug, Eq, PartialEq)]
pub struct Role {
    /// The node's name. `{i}` stands for the index of an endpoint routing group.
    pub name: &'static str,

    /// The operator the node runs.
    pub operator: &'static str,

    /// The series the node reports because of its role.
    pub series: &'static [ReportedSeries],

    /// The identifier the node's kernel cache reports under, in place of one scoped to its component, if the role
    /// gives one: the native cache's, which the remapper selects on and native mode shares.
    pub cache_id: Option<&'static str>,
}

const INDEX: &str = "{i}";

/// The listener filterlist of the DogStatsD prefix filter.
const LISTENER_FILTERLIST: &[ReportedSeries] = &[
    ReportedSeries::gauge("metric_filterlist_size", Recorder::Plan(PlanValue::ListLength)),
    ReportedSeries::counter(
        "metric_filterlist_updates_total",
        Recorder::Plan(PlanValue::ParamChanges),
    ),
    // One per metric the filterlist drops, whatever its number of points. In normal form, the split keeps what the
    // filterlist does not match on its `true` port, so what it drops leaves on `false`.
    ReportedSeries::counter("dogstatsd_listener_filtered_points_total", Recorder::Port("false")),
];

/// The post-aggregation filter. In normal form, the split keeps what the filter does not match on its `true` port, so
/// what it drops leaves on `false`.
const POST_AGGREGATE_FILTER: &[ReportedSeries] = &[ReportedSeries::counter(
    "dogstatsd_post_aggregate_filtered_metrics_total",
    Recorder::Port("false"),
)];

/// The filter of an endpoint routing group. The split's `true` port leads to the group's output, so what the native
/// filter drops leaves on `false`.
const ENDPOINT_ALLOWLIST: &[ReportedSeries] = &[
    ReportedSeries::counter(
        "metrics_endpoint_allowlist_metrics_filtered_total",
        Recorder::Port("false"),
    ),
    ReportedSeries::gauge(
        "metrics_endpoint_allowlist_size",
        Recorder::Plan(PlanValue::MatcherEntries { prefixes: false }),
    )
    .with_tags(&["match_type:exact"]),
    ReportedSeries::gauge(
        "metrics_endpoint_allowlist_size",
        Recorder::Plan(PlanValue::MatcherEntries { prefixes: true }),
    )
    .with_tags(&["match_type:prefix"]),
];

/// The tag filterlist's rule count and updates. Its kernel's counters are the operator's.
const TAG_FILTERLIST: &[ReportedSeries] = &[
    // A kernel is built while its generation is prepared, which may never commit.
    ReportedSeries::gauge("tag_filterlist_size", Recorder::Plan(PlanValue::TagProjectionRules)),
    // A change of rules builds a new kernel, which cannot know it replaces one.
    ReportedSeries::counter("tag_filterlist_updates_total", Recorder::Plan(PlanValue::ParamChanges)),
];

/// The roles table.
pub static ROLES: &[Role] = &[
    Role {
        name: "dsd_prefix_filter",
        operator: names::SPLIT,
        series: LISTENER_FILTERLIST,
        cache_id: None,
    },
    Role {
        name: "dsd_post_agg_filter",
        operator: names::SPLIT,
        series: POST_AGGREGATE_FILTER,
        cache_id: None,
    },
    Role {
        name: "metrics_routing_filter_{i}",
        operator: names::SPLIT,
        series: ENDPOINT_ALLOWLIST,
        cache_id: None,
    },
    Role {
        name: "dsd_tag_filterlist",
        operator: names::TAG_PROJECTION,
        series: TAG_FILTERLIST,
        cache_id: Some("tag_filterlist/context_cache"),
    },
];

/// Matches `name` against a role's name, returning the routing group index `{i}` stands for, if any.
fn match_pattern(pattern: &str, name: &str) -> Option<Option<String>> {
    match pattern.split_once(INDEX) {
        None => (pattern == name).then_some(None),
        Some((before, after)) => {
            let index = name.strip_prefix(before)?.strip_suffix(after)?;
            let canonical = !index.is_empty()
                && index.bytes().all(|b| b.is_ascii_digit())
                && (index == "0" || !index.starts_with('0'));
            canonical.then(|| Some(index.to_string()))
        }
    }
}

/// Returns the role of the node `node` running `operator`, if it has one.
///
/// A role is keyed by the node's name as the plan writes it: a node in the policy namespace, written `policy/<name>`,
/// has none.
pub fn of(node: &NodeId, operator: &str) -> Option<&'static Role> {
    let name = node.to_string();
    ROLES
        .iter()
        .find(|role| role.operator == operator && match_pattern(role.name, &name).is_some())
}
