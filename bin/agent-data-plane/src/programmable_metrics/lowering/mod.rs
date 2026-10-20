//! Lowering: native configuration into the default program.
//!
//! [`Lowering::lower`] is a pure function of the startup configuration, the current configuration, and the [host
//! facts](HostFacts). It produces the default program: the logical plan of the
//! native metrics path that `run.rs` builds for the same configuration, node for node, with every output's binding
//! and every resource its outputs deliver through ([`resources`](super::resources)). [`Lowering::new`] prepares it once
//! per process, from the startup configuration and the host facts: it checks what the native topology build checks,
//! fixes the delivery hosts, and resolves the outputs the process boots with. Re-lowering on a configuration change
//! reuses all three.
//!
//! # Shape
//!
//! The program is the one the implementation design draws in "Default program shape"
//! (`docs/design/programmable-metrics-implementation.md`). The DogStatsD source feeds the mapper, the namespace prefix,
//! and the listener filterlist (a `split` whose `false` port drops); with the no-aggregation pipeline, a timestamp
//! split sends timestamped points past the tag filterlist and aggregation to a merge in front of the post-aggregation
//! filter (another `split` and `drop`), whose output a `tee` sends to the client telemetry output and to metrics
//! enrichment.
//! Metrics enrichment merges that with the OTLP, checks, liveness, and trace sampler sources, runs host enrichment and
//! host tags, and feeds the policy region: a `tee` with one branch per output. The main output
//! is emitted directly; every other output sits behind its gateway, a `split` whose `true` port is emitted to the
//! output and whose `false` port drops:
//!
//! - an endpoint routing group's filter, `metrics_routing_filter_{i}`, selects the metrics whose intake name is one of
//!   the group's names or starts with one of its prefixes;
//! - the multi-region failover gateway selects every metric while failover is enabled and metric mirroring is on (both
//!   live flags), and only the metrics its allowlist names when that allowlist is not empty;
//! - the autoscaling failover gateway selects, while autoscaling failover is enabled (a live flag), the scalar series
//!   (counters, rates, gauges, and sets) whose name is one of the configured list of metrics.
//!
//! Each predicate matches as the gateway's native component does: routing groups compare the intake name (normalized
//! as the intake stores it), the failover gateways the raw name, and nothing but a metric is selected.
//!
//! Every branch exists only when `run.rs` builds it for the same configuration, decided by the same predicates and
//! helpers. The DogStatsD pipeline and the sources follow the startup configuration; the output branches (endpoint
//! routing groups, the failover branches) follow the current one, so a routing group or a failover branch appears and
//! disappears live; a published program replaces every branch with its own. Outputs a resources artifact declares
//! have no branch: only artifacts emit to them. The metrics enrichment merge (`metrics_in`)
//! and the tees (`metrics_fanout`, `dsd_post_agg_filter_fanout`) are present even with one input or output, so that
//! their IDs never depend on which other branches exist. The merge in front of the post-aggregation filter
//! (`dsd_post_agg_filter_in`) exists only with the no-aggregation pipeline, whose passthrough is its second input, as
//! natively that filter has a second input only then.
//!
//! The `source` nodes come first, DogStatsD's and then the OTLP, checks, liveness, and trace sampler sources, ahead of
//! every other node, as composition writes them ahead of a published program's nodes: so the default program's export,
//! composed unchanged, is the default program itself.
//!
//! # Parameters
//!
//! Every native policy setting is written as its value, a literal, as a published program writes its policy. A setting
//! that a native component reads through a live view comes from the current configuration (the metric filterlist and
//! the histogram statistics it names, the tag filterlist, and the lists of the output branches, which follow the
//! current configuration since outputs became live); every other setting comes from the startup configuration, so a
//! change to it takes effect only at restart, as natively. So re-lowering with a changed live setting changes only the
//! parameters it writes and the structure that follows it, which the installer installs as a generation that rebuilds
//! only what changed, and re-lowering with a changed startup-only setting changes nothing.
//!
//! Only facts and signals are bound to [bindings](super::bindings), with their resolved values: the host tags and their
//! deadline (`host.tags`, `host.tags_deadline`), whether failover is configured, and whether it mirrors metrics now. A
//! flag or an emptiness test over a bound value stays in the program as written; normalization folds those over
//! literals.
//!
//! # Normal form
//!
//! The program is written in [normal form](saluki_metric_plan::normalize), as composition reads every artifact.
//! Lowering builds each filter as its native component decides, a `split` on what the filterlist matches whose `true`
//! port drops, and normalization turns it into a `split` on the negation whose `false` port drops, renaming the drop
//! from `<split>_true` to `<split>_false`: the listener filterlist's drop is `dsd_prefix_filter_false`, and
//! the post-aggregation filter's `dsd_post_agg_filter_false`. Every parameter is written, defaults included, and every
//! literal set match as the string match it is: a filterlist with nothing in it is a `split` on `true`, which keeps
//! every metric. The node stays: the program's structure never depends on a setting's value.
//!
//! # Errors
//!
//! [`Lowering::new`] fails where `run.rs` would fail to build the native metrics path for the same configuration,
//! with the native error, and in the native order: first what assembling the blueprint checks (an endpoint routing
//! policy naming an unknown endpoint, a Cluster Agent token that cannot be sent, then the DogStatsD pipeline's tag
//! value rules and histogram statistics), then what building its components checks (an invalid mapper profile, since
//! transforms build before forwarders, then an endpoint a forwarder cannot resolve). So with several problems, the
//! first one native construction reports is the one reported, except between two forwarders' endpoints, which native
//! construction builds in no fixed order. Problems of other pipelines are left to their own construction.
//!
//! A configuration that runs natively lowers, with these known exceptions. [`Lowering::new`] fails for an aggregation
//! duration that no plan duration can represent (more than 2^53 − 1 of the largest unit dividing it), naming the
//! setting, its value, and the largest value a plan holds; and [`Lowering::lower`] fails for host tags enrichment
//! without host tags in the host facts. If the plan of a configuration that lowers does not validate, the default
//! program still runs and is reported degraded (see the implementation design's amendments).
//!
//! Where lowering departs from the configuration as written, because a plan cannot express it, it records a
//! [`LoweringNote`]: today, only histogram statistics listed more than once, which the plan computes once while native
//! aggregation emits them once per listing.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use agent_data_plane_config::SalukiConfiguration;
use saluki_components::transforms::HistogramConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_kernels::{
    mapper::validate_profiles, params::HistogramParams, selection::histogram_statistic_entries,
};
use saluki_metric_plan::{
    canonical::MAX_SAFE_INTEGER,
    compose::Demand,
    ir::{
        derived_terminal_id, format_duration, BindingName, Edge, Namespace, NativeComponentId, Node, NodeId,
        OperatorName, ParamName, ParamType, ParamValue, Params, Plan, PlanProvenance, PortName, PortRef, IR_VERSION,
    },
    normalize::normalize_plan,
    operators::{
        encode_mapper_profiles, encode_tag_filter_rules, encode_tag_value_rules, names, SourceBinding, MAPPER_PROFILES,
        METRICS_PORT, TAG_FILTER_RULES, TAG_VALUE_RULES,
    },
};
use serde_json::{json, Value};

use super::{
    bindings::{self, Snapshots},
    catalog::AdpCatalog,
    compatibility,
    host_facts::{host_tags_enabled, HostFacts},
    resources::{
        cluster_agent_forwarder, routing_groups, spelled_endpoint, Demanded, FixedHosts, NativeOutputs,
        NativeResources, OutputKind, RoutingGroup, DSD_CLIENT_TELEMETRY, DSD_CLIENT_TELEMETRY_OUT,
    },
};
use crate::{
    cli::run::dogstatsd_mapper_configuration,
    components::tag_filterlist::{tag_filter_rules, tag_value_rules, value_rules},
    config::DataPlaneConfiguration,
};

/// The types policy settings are written as, so that every lowering of one configuration writes, and hashes, the same
/// plan: each value is decoded and encoded again as its type, as the parameter that reads it will read it.
mod types {
    use saluki_metric_plan::{canonical::MAX_SAFE_INTEGER, ir::ParamType, operators::HISTOGRAM_AGGREGATES};

    pub const STRINGS: ParamType = ParamType::List(&ParamType::String);
    pub const COUNT: ParamType = ParamType::Integer {
        min: 0,
        max: MAX_SAFE_INTEGER,
    };
    pub const POSITIVE_COUNT: ParamType = ParamType::Integer {
        min: 1,
        max: MAX_SAFE_INTEGER,
    };
    pub const HISTOGRAM_AGGREGATE_NAMES: ParamType = ParamType::List(&ParamType::Enum(HISTOGRAM_AGGREGATES));
    pub const HISTOGRAM_PERCENTILES: ParamType = ParamType::List(&ParamType::Integer { min: 0, max: 100 });
}

/// The reserved node IDs of the default program.
///
/// A node reports under its name, so a node whose series the remapper reads keeps its native component's name, such as
/// `dsd_prefix_filter` and `dsd_agg` (the host's telemetry contract, `compatibility`, lists them). The others have the
/// IDs the pipeline syntax derives for their place in the program (`let dsd_prefix_filter = dsd_enrich |
/// namespace_prefix(...) | where ...`, and `let metrics = (...) | host_enrichment | host_tags`, used once per output
/// branch), so that the program prints without naming them; so which enrichment node is `metrics` follows the startup
/// configuration. The plan's provenance maps each node to the native component it stands for, for the boot layout
/// ([`components`](super::components) where no node has that component's ID).
pub mod ids {
    /// The DogStatsD source.
    pub const DSD_IN: &str = "dsd_in";
    /// The DogStatsD mapper.
    pub const DSD_ENRICH: &str = "dsd_enrich";
    /// The namespace prefix of the DogStatsD prefix filter, the first stage of the listener filterlist's pipeline.
    pub const DSD_PREFIX_FILTER_NAMESPACE_PREFIX: &str = "dsd_prefix_filter_namespace_prefix";
    /// The listener filterlist of the DogStatsD prefix filter, which the namespace prefix and its `drop` are named
    /// after, as the native prefix filter they stand for is.
    pub const DSD_PREFIX_FILTER: &str = "dsd_prefix_filter";
    /// The no-aggregation timestamp split.
    pub const DSD_NO_AGG_SPLIT: &str = "dsd_no_agg_split";
    /// The tag filterlist.
    pub const DSD_TAG_FILTERLIST: &str = "dsd_tag_filterlist";
    /// DogStatsD aggregation.
    pub const DSD_AGG: &str = "dsd_agg";
    /// The fan-in of the post-aggregation filter, from aggregation and the passthrough.
    pub const DSD_POST_AGG_FILTER_IN: &str = "dsd_post_agg_filter_in";
    /// The post-aggregation filter.
    pub const DSD_POST_AGG_FILTER: &str = "dsd_post_agg_filter";
    /// The fan-out of the post-aggregation filter, to the client telemetry tap and to metrics enrichment.
    pub const DSD_POST_AGG_FILTER_FANOUT: &str = "dsd_post_agg_filter_fanout";
    /// The OTLP source.
    pub const OTLP_IN: &str = "otlp_in";
    /// The checks IPC source.
    pub const CHECKS_IPC_IN: &str = "checks_ipc_in";
    /// The liveness source.
    pub const LIVENESS_IN: &str = "liveness_in";
    /// The trace sampler's metrics.
    pub const TRACE_SAMPLER: &str = "trace_sampler";
    /// The fan-in of metrics enrichment.
    pub const METRICS_IN: &str = "metrics_in";
    /// Host enrichment, when host tags follow it.
    pub const METRICS_HOST_ENRICHMENT: &str = "metrics_host_enrichment";
    /// The enriched metrics: the last node of the enrichment chain, host tags when enabled and host enrichment
    /// otherwise.
    pub const METRICS: &str = "metrics";
    /// The fan-out of the enriched metrics to every output branch.
    pub const METRICS_FANOUT: &str = "metrics_fanout";
    /// The multi-region failover gateway.
    pub const MRF_METRICS_GATEWAY: &str = "mrf_metrics_gateway";
    /// The autoscaling failover gateway.
    pub const AF_METRICS_GATEWAY: &str = "af_metrics_gateway";

    /// Returns the ID of the filter of the endpoint routing group at `index`, which is also its component ID.
    pub fn routing_filter(index: usize) -> String {
        format!("metrics_routing_filter_{}", index)
    }
}

/// The native components that nodes of the default program stand for, where no node has the component's ID.
pub mod components {
    /// Metrics enrichment: its fan-in, host enrichment, host tags, and the fan-out to the outputs.
    pub const METRICS_ENRICH: &str = "metrics_enrich";
}

/// A place where lowering departs from the configuration as written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoweringNote {
    /// The node whose parameters depart from the configuration.
    pub node: NodeId,

    /// What lowering did, and why.
    pub message: String,
}

/// What each node of the default program stands for, as a note tools print above it: lowering, the compatibility
/// layer, knows which native component and setting each node comes from.
///
/// Endpoint routing groups' filters are noted with their endpoints when lowered ([`Program::policy`]); a node not
/// listed has no note. Each is keyed by node and operator: the enrichment chain's last node, `metrics`, is host tags or
/// host enrichment.
const ANNOTATIONS: &[(&str, &str, &str)] = &[
    (
        ids::DSD_ENRICH,
        names::DOGSTATSD_MAPPER,
        "DogStatsD mapper: renames and tags by `dogstatsd_mapper_profiles`",
    ),
    (
        ids::DSD_PREFIX_FILTER_NAMESPACE_PREFIX,
        names::NAMESPACE_PREFIX,
        "Namespace prefix, from `statsd_metric_namespace`, except blocklisted names",
    ),
    (
        ids::DSD_PREFIX_FILTER,
        names::SPLIT,
        "Metric filterlist, from `metric_filterlist`: matches drop",
    ),
    (
        ids::DSD_NO_AGG_SPLIT,
        names::TIMESTAMP_SPLIT,
        "Timestamped points skip aggregation (no-aggregation pipeline)",
    ),
    (
        ids::DSD_TAG_FILTERLIST,
        names::TAG_PROJECTION,
        "Tag filterlist and value rules: `metric_tag_filterlist`, `metric_tag_value_allowlist`",
    ),
    (
        ids::DSD_AGG,
        names::AGGREGATE,
        "Aggregation; edits keep its state unless they change its window or lineage",
    ),
    (
        ids::DSD_POST_AGG_FILTER_IN,
        names::MERGE,
        "Aggregated and timestamped points rejoin",
    ),
    (
        ids::DSD_POST_AGG_FILTER,
        names::SPLIT,
        "Drops filterlisted histogram statistics, after aggregation",
    ),
    (
        ids::DSD_POST_AGG_FILTER_FANOUT,
        names::TEE,
        "Copies every metric to the client telemetry tap and to enrichment",
    ),
    (ids::METRICS_IN, names::MERGE, "Every source's metrics, to enrichment"),
    (
        ids::METRICS_HOST_ENRICHMENT,
        names::HOST_ENRICHMENT,
        HOST_ENRICHMENT_NOTE,
    ),
    (ids::METRICS, names::HOST_ENRICHMENT, HOST_ENRICHMENT_NOTE),
    (
        ids::METRICS,
        names::HOST_TAGS,
        "Adds the host's tags (a fact) until `expected_tags_duration` passes",
    ),
    (
        ids::METRICS_FANOUT,
        names::TEE,
        "Policy: one branch per output; route here",
    ),
    (
        ids::MRF_METRICS_GATEWAY,
        names::SPLIT,
        "Multi-Region Failover: mirrors listed, or all, metrics while failing over",
    ),
    (
        ids::AF_METRICS_GATEWAY,
        names::SPLIT,
        "Autoscaling failover: listed scalar series to the Cluster Agent",
    ),
];

/// The note of host enrichment, whichever node of the enrichment chain it is.
const HOST_ENRICHMENT_NOTE: &str = "Host enrichment: leaves metrics unchanged";

/// Returns the note that lowering gives the node `local` running `operator`, if it has one.
fn annotation(local: &str, operator: &str) -> Option<&'static str> {
    ANNOTATIONS
        .iter()
        .find(|(id, op, _)| *id == local && *op == operator)
        .map(|(_, _, note)| *note)
}

/// How a reader knows each node of the default program that the detached export can leave out because it does nothing
/// as configured, with why it does nothing then, in program order: the note the export carries
/// ([`Annotations::omitted`](saluki_metric_plan::export::Annotations::omitted)).
///
/// An endpoint routing group's filter is named from its note ([`omitted_name`]). The nodes a left-out `split` alone
/// reached, such as its `drop`, go with it and are not named. The failover gateways' flags are a fact and signals, which
/// a published program keeps bound, so the export never leaves those gateways out for their values. Each is keyed by
/// node and operator, as [`ANNOTATIONS`] is.
const OMITTED: &[(&str, &str, &str)] = &[
    (ids::DSD_ENRICH, names::DOGSTATSD_MAPPER, "mapper (no profiles)"),
    (
        ids::DSD_PREFIX_FILTER_NAMESPACE_PREFIX,
        names::NAMESPACE_PREFIX,
        "namespace prefix (none)",
    ),
    (ids::DSD_PREFIX_FILTER, names::SPLIT, "metric filterlist (empty)"),
    (ids::DSD_TAG_FILTERLIST, names::TAG_PROJECTION, "tag filterlist (empty)"),
    (
        ids::DSD_POST_AGG_FILTER_IN,
        names::MERGE,
        "post-aggregation merge (one input)",
    ),
    (
        ids::DSD_POST_AGG_FILTER,
        names::SPLIT,
        "post-aggregation filter (empty)",
    ),
    (ids::METRICS_IN, names::MERGE, "enrichment merge (one input)"),
    (
        ids::METRICS_HOST_ENRICHMENT,
        names::HOST_ENRICHMENT,
        "host enrichment (changes no metric)",
    ),
    (
        ids::METRICS,
        names::HOST_ENRICHMENT,
        "host enrichment (changes no metric)",
    ),
    (ids::METRICS, names::HOST_TAGS, "host tags (none)"),
    (
        ids::AF_METRICS_GATEWAY,
        names::SPLIT,
        "autoscaling failover (no metrics listed)",
    ),
];

/// Returns how a reader knows the node `local` of the default program, left out of the detached export because it does
/// nothing as configured, and its rank in program order; `note` is the note from lowering about it, if it has one.
/// `None` for a node no reader needs named: one a left-out `split` alone reached.
///
/// Where several operators can hold the ID, as `metrics` can, the note tells them apart.
pub fn omitted_name(local: &str, note: Option<&str>) -> Option<(usize, String)> {
    let rows = OMITTED
        .iter()
        .enumerate()
        .filter(|(_, (id, _, _))| *id == local)
        .collect::<Vec<_>>();
    let row = match rows.as_slice() {
        [row] => Some(*row),
        rows => rows
            .iter()
            .find(|(_, (id, op, _))| note.is_some() && annotation(id, op) == note)
            .copied(),
    };
    if let Some((rank, (_, _, name))) = row {
        return Some((rank, name.to_string()));
    }
    // Endpoint routing groups come after enrichment, in the policy, with neither names nor prefixes.
    let routed = note.and_then(|note| note.strip_prefix("Endpoint routing to "))?;
    local.starts_with("metrics_routing_filter_").then(|| {
        (
            OMITTED.len(),
            format!("endpoint routing to {} (no names or prefixes)", routed),
        )
    })
}

/// The default program, and what the host declares for it.
#[derive(Clone, Debug)]
pub struct Lowered {
    /// The default program.
    pub plan: Plan,

    /// The catalog the program validates against.
    pub catalog: AdpCatalog,

    /// Where lowering departs from the configuration as written.
    pub notes: Vec<LoweringNote>,

    /// What each node stands for, in one line, as tools print it above the node: the native component and setting
    /// it comes from. Not part of the program.
    pub annotations: BTreeMap<NodeId, String>,

    /// The nodes whose parameters lowering writes from native policy settings, which a published program owns: what
    /// status compares a published program with when it says which native settings that program does not follow.
    pub policy_nodes: BTreeSet<NodeId>,

    /// The outputs, recipients, and host settings the program delivers through, as resolved for it.
    pub resources: NativeResources,
}

/// Lowering for one process: its startup configuration and host facts, checked as native construction checks them,
/// its delivery hosts, and the outputs it boots with.
///
/// Build it once at boot, and [lower](Self::lower_with) whenever the configuration or the accepted resources change.
#[derive(Clone, Debug)]
pub struct Lowering {
    startup: Arc<SalukiConfiguration>,
    facts: HostFacts,
    hosts: FixedHosts,
    outputs: NativeOutputs,
}

impl Lowering {
    /// Prepares lowering for a process that started with the configuration `startup` and the host facts `facts`.
    ///
    /// Returns `None` when the configuration requires no metrics pipeline, so there is no metrics path to describe.
    /// Otherwise it checks the configuration as native construction of the metrics path does, fixes the delivery hosts
    /// from the host facts, and resolves the outputs the process boots with.
    ///
    /// # Errors
    ///
    /// Returns the native error if the native metrics path would fail to build for `startup`; see the [module
    /// documentation](self#errors).
    pub fn new(startup: Arc<SalukiConfiguration>, facts: HostFacts) -> Result<Option<Self>, GenericError> {
        let dp = DataPlaneConfiguration::from_configuration(&startup);
        if !dp.metrics_pipeline_required() {
            return Ok(None);
        }

        check_native_construction(&startup, &facts)?;
        if dp.dogstatsd_enabled() {
            check_plan_durations(&startup)?;
        }
        let hosts = FixedHosts::at_boot(&facts);
        let outputs = NativeResources::resolve(&startup, &startup, &facts, &hosts)?.outputs;
        Ok(Some(Self {
            startup,
            facts,
            hosts,
            outputs,
        }))
    }

    /// Returns the outputs the process boots with, which the metrics path's frame is built for.
    pub fn outputs(&self) -> &NativeOutputs {
        &self.outputs
    }

    /// Returns the delivery hosts, fixed at boot.
    pub fn hosts(&self) -> &FixedHosts {
        &self.hosts
    }

    /// Lowers native configuration into the default program.
    ///
    /// The DogStatsD pipeline, the sources, and the settings read only at startup come from the startup configuration;
    /// the outputs, their bindings, the recipients, the host settings, the settings native components read live, and
    /// `Live` bindings from `current`, the configuration now.
    ///
    /// # Errors
    ///
    /// Returns an error for the configurations that run natively but cannot be lowered; see the [module
    /// documentation](self#errors).
    pub fn lower(&self, current: &SalukiConfiguration) -> Result<Lowered, GenericError> {
        self.lower_after(current, &[], &NativeResources::default())
    }

    /// Lowers native configuration, and an implied output for each set of recipients of `demand` that no output
    /// delivers to, into the default program that follows the one lowered before it, whose resources were `previous`
    /// (see [`Demanded`]).
    ///
    /// The program is the one [`lower`][Self::lower] lowers, except that a native recipient `previous`
    /// declares stays while its endpoint and key position stay configured, whatever its key now holds: a key is a
    /// credential, which its recipient follows (see [`NativeResources::resolve_after`]).
    ///
    /// # Errors
    ///
    /// As for [`lower`][Self::lower].
    pub fn lower_after(
        &self, current: &SalukiConfiguration, demand: &[Demand], previous: &NativeResources,
    ) -> Result<Lowered, GenericError> {
        let startup = &*self.startup;
        let resolved = NativeResources::resolve_after(
            startup,
            current,
            &self.facts,
            &self.hosts,
            Demanded {
                demand,
                previous: &previous.outputs,
            },
            &previous.resources,
        )?;
        let dp = DataPlaneConfiguration::from_configuration(startup);
        let snapshots = Snapshots {
            startup,
            current,
            facts: &self.facts,
        };
        let mut program = Program::new(snapshots);
        let mut notes = Vec::new();

        // The sources come first, in a fixed order, as composition writes them ahead of a program's nodes: so a
        // program exported from the default program and composed unchanged is the default program, node for node.
        let dogstatsd = dp.dogstatsd_enabled().then(|| program.dogstatsd_source(startup));
        let sources = [
            (
                ids::OTLP_IN,
                SourceBinding::Otlp,
                dp.otlp_enabled() && !dp.otlp_proxy_enabled(),
            ),
            (ids::CHECKS_IPC_IN, SourceBinding::Checks, dp.checks_enabled()),
            (ids::LIVENESS_IN, SourceBinding::Liveness, !dp.standalone_mode()),
            (
                ids::TRACE_SAMPLER,
                SourceBinding::TraceSampler,
                dp.traces_pipeline_required(),
            ),
        ];
        let sources = sources
            .into_iter()
            .filter(|(_, _, present)| *present)
            .map(|(id, binding, _)| {
                let source = program.node(id, names::SOURCE, json!({"binding": binding.as_str()}), id);
                PortRef::new(source, port(METRICS_PORT))
            })
            .collect::<Vec<_>>();

        // Everything feeding metrics enrichment, in a fixed order.
        let mut enrichment_inputs = Vec::new();
        if let Some(source) = dogstatsd {
            enrichment_inputs.push(program.dogstatsd(startup, source, &mut notes)?);
        }
        enrichment_inputs.extend(sources);
        if enrichment_inputs.is_empty() {
            return Err(generic_error!(
                "The metrics pipeline is required, but no source feeds it; this is a lowering bug."
            ));
        }

        let enriched = program.enrichment(startup, &self.facts, enrichment_inputs)?;
        program.policy(enriched, &resolved.outputs)?;

        let native_components = std::mem::take(&mut program.native_components);
        let plan = Plan {
            ir_version: IR_VERSION,
            provenance: (!native_components.is_empty()).then(|| PlanProvenance {
                native_components,
                ..PlanProvenance::default()
            }),
            nodes: std::mem::take(&mut program.nodes),
            edges: std::mem::take(&mut program.edges),
            outputs: resolved.outputs.decls(),
            resources: resolved.resources.clone(),
        };
        // The default program is written in normal form, as composition reads every artifact, so that its export, and
        // a program parsed back from that export unchanged, are the same document.
        let plan = normalize_plan(&plan);

        // The remapper reads native series from the nodes of the default program by name: every program is held to it.
        let catalog =
            AdpCatalog::new(&resolved, snapshots, self.hosts).with_telemetry_contract(compatibility::of_default(&plan));
        let mut annotations = program.annotations;
        for node in &plan.nodes {
            if let Some(note) = annotation(node.id.local().as_str(), node.op.as_str()) {
                if node.id.namespace() == Namespace::Reserved {
                    annotations.insert(node.id.clone(), note.to_string());
                }
            }
        }
        Ok(Lowered {
            plan,
            catalog,
            notes,
            annotations,
            policy_nodes: program.policy_nodes,
            resources: resolved,
        })
    }
}

/// Checks the startup configuration as native construction of the metrics path checks it, in the same order and with
/// the same errors, except for forwarder endpoints, which resolving the native outputs checks last.
///
/// `run.rs` assembles the metrics output pipelines (endpoint routing, then the failover branches) before the DogStatsD
/// pipeline, and then builds every component, transforms before forwarders.
fn check_native_construction(startup: &SalukiConfiguration, facts: &HostFacts) -> Result<(), GenericError> {
    // Assembling the blueprint.
    routing_groups(startup)?;
    cluster_agent_forwarder(&startup.shared, facts)?;
    let dp = DataPlaneConfiguration::from_configuration(startup);
    if dp.dogstatsd_enabled() {
        let dogstatsd = &startup.domains.dogstatsd;
        value_rules(&dogstatsd.tag_value_allowlist)
            .error_context("Failed to configure metric tag filterlist transform.")?;
        let histogram = &startup.shared.metrics_encoding.histogram;
        HistogramConfiguration::try_new(
            &histogram.aggregates,
            &histogram.percentiles,
            histogram.copy_to_distribution,
            histogram.copy_to_distribution_prefix.clone(),
        )
        .error_context("Failed to configure histogram aggregation.")?;

        // Building the components: the mapper is a transform.
        let profiles = dogstatsd_mapper_configuration(&dogstatsd.mapper).profiles()?;
        validate_profiles(&profiles)?;
    }
    Ok(())
}

/// Fails for an aggregation setting whose duration runs natively but that no plan duration can represent: a plan writes
/// a duration as a whole number, at most 2^53 − 1, of the largest unit that divides it exactly.
fn check_plan_durations(startup: &SalukiConfiguration) -> Result<(), GenericError> {
    let aggregation = &startup.domains.dogstatsd.aggregation;
    let durations = [
        (
            "aggregate_window_duration_seconds",
            Duration::from_secs(aggregation.window_duration_seconds.get()),
        ),
        ("aggregate_flush_interval", aggregation.flush_interval),
        (
            "dogstatsd_expiry_seconds",
            Duration::from_secs(aggregation.counter_expiry_seconds.unwrap_or_default()),
        ),
    ];
    for (setting, duration) in durations {
        if format_duration(duration).is_none() {
            return Err(generic_error!(
                "`{}` is {} seconds, which the metrics plan cannot represent: a plan duration is a whole number of at \
                 most {} of the largest unit that divides it exactly. Set `{}` to at most {} seconds.",
                setting,
                format_seconds(duration),
                MAX_SAFE_INTEGER,
                setting,
                MAX_SAFE_INTEGER
            ));
        }
    }
    Ok(())
}

/// Returns a duration in seconds, with as many decimals as it needs.
fn format_seconds(duration: Duration) -> String {
    match duration.subsec_nanos() {
        0 => duration.as_secs().to_string(),
        nanos => format!("{}.{:09}", duration.as_secs(), nanos)
            .trim_end_matches('0')
            .to_string(),
    }
}

/// Returns a policy setting's value as a literal of type `ty`, in its canonical form: decoded and encoded again as the
/// parameter that reads it will, so that every lowering of the same configuration writes, and hashes, the same plan. A
/// value that is not of the type, such as an integer beyond the plan's integer range, is written as it is, so that
/// validating the plan reports the problem; `setting` names the setting, for that report.
///
/// Returns an error if the configuration has no value for it, such as a duration no plan duration represents.
fn literal(ty: &ParamType, value: Option<Value>, setting: &str) -> Result<Value, GenericError> {
    let value = value.ok_or_else(|| generic_error!("The configuration has no valid value for `{}`.", setting))?;
    Ok(match ParamValue::decode(ty, &value, setting) {
        Ok(decoded) => decoded.encode(),
        Err(_) => value,
    })
}

/// Returns the histogram statistics aggregation computes, as native configuration parses them, each listed once, and
/// the statistics that configuration repeats, in order.
///
/// Native aggregation computes a repeated statistic as many times as it is listed, emitting the same series more than
/// once per flush, and a plan lists each statistic once, so the first of each is kept. Percentiles that round to the
/// same whole percent are repeats too.
///
/// # Errors
///
/// Returns the native error for a statistic that aggregation cannot compute.
pub fn histogram_params(
    config: &SalukiConfiguration,
) -> Result<(HistogramParams, Vec<String>), saluki_metric_kernels::params::InvalidHistogramStatistic> {
    let histogram = &config.shared.metrics_encoding.histogram;
    let mut params = HistogramParams::from_names(
        &histogram.aggregates,
        &histogram.percentiles,
        histogram.copy_to_distribution,
        histogram.copy_to_distribution_prefix.clone(),
    )?;
    let mut repeated = Vec::new();
    let mut seen = Vec::new();
    params.aggregates.retain(|aggregate| {
        let first = !seen.contains(aggregate);
        if first {
            seen.push(*aggregate);
        } else {
            repeated.push(aggregate.as_str().to_string());
        }
        first
    });
    let mut seen = Vec::new();
    params.percentiles.retain(|percentile| {
        let first = !seen.contains(percentile);
        if first {
            seen.push(*percentile);
        } else {
            repeated.push(format!("{}percentile", percentile));
        }
        first
    });
    Ok((params, repeated))
}

fn seconds(seconds: u64) -> Option<Value> {
    format_duration(Duration::from_secs(seconds)).map(Value::String)
}

/// Returns the metric filterlist's entries that name a histogram statistic's series, which the post-aggregation filter
/// drops: the filterlist as it is now, and the statistics as configured at startup, as natively.
fn post_aggregate_filter_values(live: &SalukiConfiguration, startup: &SalukiConfiguration) -> Option<Value> {
    let (histogram, _) = histogram_params(startup).ok()?;
    Some(json!(histogram_statistic_entries(
        &live.domains.dogstatsd.metric_filter.values,
        &histogram
    )))
}

/// Returns the port name `name`, which is a constant of the default program.
fn port(name: &str) -> PortName {
    PortName::new(name).expect("default-program port names are valid")
}

/// Returns the reserved node ID `local`.
fn node_id(local: &str) -> NodeId {
    local.parse().expect("default-program node IDs are identifiers")
}

/// The default program under construction.
struct Program<'a> {
    snapshots: Snapshots<'a>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    /// The native component each node stands for, where it is not the node's own ID: the plan's
    /// [`native_components`][PlanProvenance::native_components], for the boot layout.
    native_components: BTreeMap<NodeId, NativeComponentId>,
    /// Notes of nodes whose note depends on the configuration, beside [`ANNOTATIONS`].
    annotations: BTreeMap<NodeId, String>,
    /// The nodes whose parameters come from native policy settings.
    policy_nodes: BTreeSet<NodeId>,
}

impl<'a> Program<'a> {
    fn new(snapshots: Snapshots<'a>) -> Self {
        Self {
            snapshots,
            nodes: Vec::new(),
            edges: Vec::new(),
            native_components: BTreeMap::new(),
            annotations: BTreeMap::new(),
            policy_nodes: BTreeSet::new(),
        }
    }

    /// Adds a node standing for the native component `native`, and returns its ID.
    fn node(&mut self, id: &str, op: &str, params: Value, native: &str) -> NodeId {
        let id = node_id(id);
        let params = match params {
            Value::Object(members) => members
                .into_iter()
                .map(|(name, value)| (ParamName::new(name).expect("parameter names are identifiers"), value))
                .collect(),
            _ => Params::new(),
        };
        self.nodes.push(Node {
            id: id.clone(),
            op: OperatorName::new(op).expect("operator names are identifiers"),
            params,
        });
        if id.local().as_str() != native {
            let native = NativeComponentId::new(native).expect("native component IDs are valid");
            self.native_components.insert(id.clone(), native);
        }
        id
    }

    /// Adds a node whose parameters come from native policy settings, as [`node`][Self::node] does.
    fn policy_node(&mut self, id: &str, op: &str, params: Value, native: &str) -> NodeId {
        let id = self.node(id, op, params, native);
        self.policy_nodes.insert(id.clone());
        id
    }

    /// Adds an edge from `from` to the input port `to_port` of `to`.
    fn edge(&mut self, from: PortRef, to: &NodeId, to_port: &str) {
        self.edges.push(Edge {
            from,
            to: PortRef::new(to.clone(), port(to_port)),
        });
    }

    /// Adds a `drop` or `emit` terminal on `parent`'s output port `parent_port`, named by `derived_terminal_id`.
    fn terminal(&mut self, parent: &NodeId, parent_port: &str, op: &str, params: Value, native: &str) {
        let terminal = derived_terminal_id(parent, &port(parent_port));
        let terminal = self.node(terminal.local().as_ref(), op, params, native);
        self.edge(PortRef::new(parent.clone(), port(parent_port)), &terminal, "in");
    }

    /// Returns the configuration the settings a native component reads through a live view come from: the current
    /// one. Every other setting comes from the startup configuration.
    fn live(&self) -> &'a SalukiConfiguration {
        self.snapshots.current
    }

    /// Returns the parameter value binding the fact or signal `name`, with its resolved value.
    fn bound(&self, name: &str) -> Result<Value, GenericError> {
        let binding = BindingName::new(name).expect("registry binding names are valid");
        let resolved = bindings::resolve(&binding, self.snapshots)
            .ok_or_else(|| generic_error!("The configuration has no value for the binding `{}`.", name))?;
        Ok(json!({"config": name, "resolved": resolved}))
    }

    /// Lowers the DogStatsD source and returns its metrics port. Its metrics are mixed with the no-aggregation
    /// pipeline, which parses client timestamps, and untimestamped without it.
    fn dogstatsd_source(&mut self, startup: &SalukiConfiguration) -> PortRef {
        let timestamps = if startup.domains.dogstatsd.aggregation.no_aggregation_pipeline {
            "mixed"
        } else {
            "untimestamped"
        };
        let dsd_in = self.node(
            ids::DSD_IN,
            names::SOURCE,
            json!({"binding": SourceBinding::Dogstatsd.as_str(), "timestamps": timestamps}),
            ids::DSD_IN,
        );
        PortRef::new(dsd_in, port(METRICS_PORT))
    }

    /// Lowers DogStatsD's pre-aggregation path from its `source`, aggregation, and the post-aggregation filter, and
    /// returns the port that feeds metrics enrichment.
    fn dogstatsd(
        &mut self, startup: &SalukiConfiguration, source: PortRef, notes: &mut Vec<LoweringNote>,
    ) -> Result<PortRef, GenericError> {
        // `Lowering::new` checked what native construction rejects.
        let dogstatsd = &startup.domains.dogstatsd;
        let no_aggregation_pipeline = dogstatsd.aggregation.no_aggregation_pipeline;

        // Every setting of DogStatsD's processing is read at startup, but the filterlists.
        let mapper = &dogstatsd.mapper;
        let profiles = dogstatsd_mapper_configuration(mapper)
            .profiles()
            .ok()
            .map(|profiles| encode_mapper_profiles(&profiles));
        let params = json!({
            "profiles": literal(&MAPPER_PROFILES, profiles, "dogstatsd_mapper_profiles")?,
            "cache_size": literal(&types::COUNT, Some(json!(mapper.cache_size)), "dogstatsd_mapper_cache_size")?,
            "string_interner_bytes": literal(
                &types::POSITIVE_COUNT,
                Some(json!(mapper.string_interner_size_bytes.get())),
                "dogstatsd_mapper_string_interner_size",
            )?,
        });
        let mapper = self.policy_node(ids::DSD_ENRICH, names::DOGSTATSD_MAPPER, params, ids::DSD_ENRICH);
        self.edge(source, &mapper, "in");

        let prefix_filter = &dogstatsd.prefix_filter;
        let params = json!({
            "namespace": literal(
                &ParamType::String,
                Some(json!(prefix_filter.metric_namespace)),
                "statsd_metric_namespace",
            )?,
            "exclusions": literal(
                &types::STRINGS,
                Some(json!(prefix_filter.metric_namespace_blocklist)),
                "statsd_metric_namespace_blocklist",
            )?,
        });
        let prefix = self.policy_node(
            ids::DSD_PREFIX_FILTER_NAMESPACE_PREFIX,
            names::NAMESPACE_PREFIX,
            params,
            ids::DSD_PREFIX_FILTER,
        );
        self.edge(PortRef::new(mapper, port("out")), &prefix, "in");

        // The listener filterlist matches the intake name after prefixing, as the native prefix filter does. It follows
        // the current configuration, as the native prefix filter's live view does.
        let filter = &self.live().domains.dogstatsd.metric_filter;
        let match_prefix = literal(
            &ParamType::Bool,
            Some(json!(filter.match_prefix)),
            "metric_filterlist_match_prefix",
        )?;
        let predicate = json!({"intake_name": {"matches": {
            "values": literal(&types::STRINGS, Some(json!(filter.values)), "metric_filterlist")?,
            "prefix": match_prefix,
        }}});
        let blocklist = self.policy_node(
            ids::DSD_PREFIX_FILTER,
            names::SPLIT,
            json!({"predicate": predicate}),
            ids::DSD_PREFIX_FILTER,
        );
        self.edge(PortRef::new(prefix, port("out")), &blocklist, "in");
        self.terminal(&blocklist, "true", names::DROP, Value::Null, ids::DSD_PREFIX_FILTER);
        let kept = PortRef::new(blocklist, port("false"));

        let aggregation = &dogstatsd.aggregation;
        let window = literal(
            &ParamType::Duration,
            seconds(aggregation.window_duration_seconds.get()),
            "aggregate_window_duration_seconds",
        )?;
        let (to_projection, passthrough) = if no_aggregation_pipeline {
            let params = json!({"window": window});
            let split = self.policy_node(
                ids::DSD_NO_AGG_SPLIT,
                names::TIMESTAMP_SPLIT,
                params,
                ids::DSD_NO_AGG_SPLIT,
            );
            self.edge(kept, &split, "in");
            (
                PortRef::new(split.clone(), port("default")),
                Some(PortRef::new(split, port("passthrough"))),
            )
        } else {
            (kept, None)
        };

        // The tag filterlist follows the current configuration, as the native one's live view does.
        let rules = tag_filter_rules(&self.live().domains.dogstatsd.tag_filterlist);
        let params = json!({
            "rules": literal(&TAG_FILTER_RULES, Some(encode_tag_filter_rules(&rules)), "metric_tag_filterlist")?,
            "value_rules": literal(
                &TAG_VALUE_RULES,
                Some(encode_tag_value_rules(&tag_value_rules(&dogstatsd.tag_value_allowlist))),
                "metric_tag_value_allowlist",
            )?,
            "cache_capacity": literal(
                &types::COUNT,
                Some(json!(aggregation.aggregator_tag_filter_cache_capacity)),
                "aggregator_tag_filter_cache_capacity",
            )?,
        });
        let projection = self.policy_node(
            ids::DSD_TAG_FILTERLIST,
            names::TAG_PROJECTION,
            params,
            ids::DSD_TAG_FILTERLIST,
        );
        self.edge(to_projection, &projection, "in");

        // `Lowering::new` parsed the statistics, so this cannot fail. Each is listed once, in its configured order.
        let (histogram, repeated) =
            histogram_params(startup).error_context("Failed to configure histogram aggregation.")?;
        let aggregates = histogram
            .aggregates
            .iter()
            .map(|aggregate| aggregate.as_str())
            .collect::<Vec<_>>();
        // An unset or zero expiry never expires counters, which the parameter writes as `0s`.
        let counter_expiry = seconds(aggregation.counter_expiry_seconds.unwrap_or_default());
        let params = json!({
            "window": window,
            "flush_interval": literal(
                &ParamType::Duration,
                format_duration(aggregation.flush_interval).map(Value::String),
                "aggregate_flush_interval",
            )?,
            "context_limit": literal(&types::COUNT, Some(json!(aggregation.context_limit)), "aggregate_context_limit")?,
            "counter_expiry": literal(&ParamType::Duration, counter_expiry, "dogstatsd_expiry_seconds")?,
            "flush_open_windows": literal(
                &ParamType::Bool,
                Some(json!(aggregation.flush_open_windows)),
                "aggregate_flush_open_windows",
            )?,
            "histogram_aggregates": literal(
                &types::HISTOGRAM_AGGREGATE_NAMES,
                Some(json!(aggregates)),
                "histogram_aggregates",
            )?,
            "histogram_percentiles": literal(
                &types::HISTOGRAM_PERCENTILES,
                Some(json!(histogram.percentiles)),
                "histogram_percentiles",
            )?,
            "copy_to_distribution": literal(
                &ParamType::Bool,
                Some(json!(histogram.copy_to_distribution)),
                "histogram_copy_to_distribution",
            )?,
            "copy_to_distribution_prefix": literal(
                &ParamType::String,
                Some(json!(histogram.copy_to_distribution_prefix)),
                "histogram_copy_to_distribution_prefix",
            )?,
        });
        let aggregate = self.policy_node(ids::DSD_AGG, names::AGGREGATE, params, ids::DSD_AGG);
        self.edge(PortRef::new(projection, port("out")), &aggregate, "in");
        if !repeated.is_empty() {
            notes.push(LoweringNote {
                node: aggregate.clone(),
                message: format!(
                    "The histogram statistics {} are listed more than once; each is computed once. Native \
                     aggregation emits a repeated statistic's series once per listing.",
                    repeated
                        .iter()
                        .map(|statistic| format!("`{}`", statistic))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        let aggregated = PortRef::new(aggregate, port("out"));

        // Timestamped points rejoin the aggregated stream at the post-aggregation filter.
        let to_post_aggregation = match passthrough {
            Some(passthrough) => {
                let merge = self.node(
                    ids::DSD_POST_AGG_FILTER_IN,
                    names::MERGE,
                    json!({"inputs": 2}),
                    ids::DSD_POST_AGG_FILTER,
                );
                self.edge(aggregated, &merge, "0");
                self.edge(passthrough, &merge, "1");
                PortRef::new(merge, port("out"))
            }
            None => aggregated,
        };

        // The filterlist's entries that name a histogram statistic's series, live with the filterlist.
        let statistics = post_aggregate_filter_values(self.live(), startup);
        let predicate = json!({"all": [
            {"kind_in": ["counter", "rate", "gauge", "set"]},
            {"intake_name": {"matches": {
                "values": literal(&types::STRINGS, statistics, "metric_filterlist")?,
                "prefix": match_prefix,
            }}},
        ]});
        let post_aggregation = self.policy_node(
            ids::DSD_POST_AGG_FILTER,
            names::SPLIT,
            json!({"predicate": predicate}),
            ids::DSD_POST_AGG_FILTER,
        );
        self.edge(to_post_aggregation, &post_aggregation, "in");
        self.terminal(
            &post_aggregation,
            "true",
            names::DROP,
            Value::Null,
            ids::DSD_POST_AGG_FILTER,
        );

        let fanout = self.node(
            ids::DSD_POST_AGG_FILTER_FANOUT,
            names::TEE,
            json!({"ports": 2}),
            DSD_CLIENT_TELEMETRY_OUT,
        );
        self.edge(PortRef::new(post_aggregation, port("false")), &fanout, "in");
        self.terminal(
            &fanout,
            "0",
            names::EMIT,
            json!({"output": DSD_CLIENT_TELEMETRY}),
            DSD_CLIENT_TELEMETRY_OUT,
        );
        Ok(PortRef::new(fanout, port("1")))
    }

    /// Lowers metrics enrichment fed by `inputs`, and returns the port of the enriched metrics.
    fn enrichment(
        &mut self, startup: &SalukiConfiguration, facts: &HostFacts, inputs: Vec<PortRef>,
    ) -> Result<PortRef, GenericError> {
        // The enrichment stages after the merge, in order: host enrichment, then host tags when enabled.
        let mut stages = vec![(names::HOST_ENRICHMENT, Value::Null)];
        if host_tags_enabled(startup) {
            if facts.host_tags.is_none() {
                return Err(generic_error!(
                    "Host tags enrichment is enabled, but the host facts carry no host tags."
                ));
            }
            // Both are facts of the host. The deadline is a whole number of seconds within the plan's integers, which a
            // plan duration always represents.
            let params = json!({
                "tags": self.bound("host.tags")?,
                "deadline": self.bound("host.tags_deadline")?,
            });
            stages.push((names::HOST_TAGS, params));
        }

        // Named as the pipeline syntax derives `let metrics = (<inputs>) | <stages>`: the last node is `metrics`, an
        // earlier stage `metrics_<operator>`, and the merge `metrics_in`. Host enrichment is always a stage, so the chain
        // is never empty and no merge is named `metrics`.
        debug_assert!(!stages.is_empty(), "host enrichment is always a stage of the chain");
        let merge = self.node(
            ids::METRICS_IN,
            names::MERGE,
            json!({"inputs": inputs.len()}),
            components::METRICS_ENRICH,
        );
        for (index, input) in inputs.into_iter().enumerate() {
            self.edge(input, &merge, &index.to_string());
        }
        let mut enriched = PortRef::new(merge, port("out"));
        let last = stages.len().saturating_sub(1);
        for (index, (op, params)) in stages.into_iter().enumerate() {
            let id = if index == last {
                ids::METRICS.to_string()
            } else {
                format!("{}_{}", ids::METRICS, op)
            };
            let stage = self.node(&id, op, params, components::METRICS_ENRICH);
            self.edge(enriched, &stage, "in");
            enriched = PortRef::new(stage, port("out"));
        }
        Ok(enriched)
    }

    /// Lowers the policy region fed by `enriched`: one branch per output other than the client telemetry tap.
    ///
    /// Outputs programs imply get no branch: only programs emit to them.
    fn policy(&mut self, enriched: PortRef, outputs: &NativeOutputs) -> Result<(), GenericError> {
        let branches = outputs
            .iter()
            .filter(|output| !matches!(output.kind, OutputKind::ClientTelemetry | OutputKind::Implied { .. }))
            .collect::<Vec<_>>();
        let policy = self.node(
            ids::METRICS_FANOUT,
            names::TEE,
            json!({"ports": branches.len()}),
            components::METRICS_ENRICH,
        );
        self.edge(enriched, &policy, "in");

        for (index, output) in branches.into_iter().enumerate() {
            let emit = json!({"output": output.decl.id.as_str()});
            let branch = index.to_string();
            // The gateway that selects what reaches the output, as a `split` whose `true` port leads to the output and
            // whose `false` port drops, and the predicate its native component applies.
            let (gateway, predicate) = match &output.kind {
                OutputKind::ClientTelemetry | OutputKind::Implied { .. } => continue,
                OutputKind::Main => {
                    self.terminal(&policy, &branch, names::EMIT, emit, &output.component);
                    continue;
                }
                OutputKind::RoutingGroup(group) => {
                    let filter = ids::routing_filter(group.index);
                    // As status spells endpoints: never with what userinfo may hold.
                    let endpoints = group
                        .endpoints
                        .iter()
                        .map(|endpoint| spelled_endpoint(endpoint))
                        .collect::<Vec<_>>();
                    let note = format!("Endpoint routing to {}", endpoints.join(", "));
                    self.annotations.insert(node_id(&filter), note);
                    (filter, self.routing_group(group)?)
                }
                OutputKind::MultiRegionFailover => {
                    (ids::MRF_METRICS_GATEWAY.to_string(), self.multi_region_failover()?)
                }
                OutputKind::AutoscalingFailover => (ids::AF_METRICS_GATEWAY.to_string(), self.autoscaling_failover()?),
            };
            let split = self.policy_node(&gateway, names::SPLIT, json!({"predicate": predicate}), &gateway);
            self.edge(PortRef::new(policy.clone(), port(&branch)), &split, "in");
            self.terminal(&split, "true", names::EMIT, emit, &output.component);
            self.terminal(&split, "false", names::DROP, Value::Null, &gateway);
        }
        Ok(())
    }

    /// Returns the predicate of an endpoint routing group's filter: the intake name is one of the group's names, or
    /// starts with one of its prefixes, as the current configuration lists them. Two empty lists match nothing, as
    /// natively.
    fn routing_group(&self, group: &RoutingGroup) -> Result<Value, GenericError> {
        let routing = routing_groups(self.live())?;
        let configured = routing
            .policy_groups()
            .iter()
            .find(|configured| configured.endpoints == group.endpoints)
            .ok_or_else(|| {
                generic_error!(
                    "The configuration has no endpoint routing group for {:?}.",
                    group.endpoints
                )
            })?;
        let names = literal(
            &types::STRINGS,
            Some(json!(configured.metric_allowlist)),
            "metrics_endpoint_routing.metric_allowlist",
        )?;
        let prefixes = literal(
            &types::STRINGS,
            Some(json!(configured.metric_prefix_allowlist)),
            "metrics_endpoint_routing.metric_prefix_allowlist",
        )?;
        Ok(json!({"any": [
            {"intake_name": {"matches": {"values": names, "prefix": false}}},
            {"intake_name": {"matches": {"values": prefixes, "prefix": true}}},
        ]}))
    }

    /// Returns the predicate of the multi-region failover gateway: failover is enabled (live, as the branch is, which
    /// exists only then), metric mirroring is on, and the raw name is in the mirroring allowlist, unless the allowlist
    /// is empty, which mirrors everything. Mirroring and its allowlist are live, as natively.
    fn multi_region_failover(&self) -> Result<Value, GenericError> {
        let mirroring = &self.live().domains.multi_region_failover.metric_mirroring;
        let allowlist = literal(
            &types::STRINGS,
            Some(json!(mirroring.allowlist)),
            "multi_region_failover.metric_allowlist",
        )?;
        Ok(json!({"all": [
            {"flag": self.bound("multi_region_failover.enabled")?},
            {"flag": self.bound("multi_region_failover.metric_mirroring.enabled")?},
            {"any": [
                {"is_empty": allowlist},
                {"name": {"matches": {"values": allowlist, "prefix": false}}},
            ]},
        ]}))
    }

    /// Returns the predicate of the autoscaling failover gateway: a scalar series (a counter, rate, gauge, or set),
    /// while autoscaling failover is enabled (a live signal, always true in the default program, whose branch exists
    /// only then), whose raw name is one of the configured metrics.
    fn autoscaling_failover(&self) -> Result<Value, GenericError> {
        let metrics = literal(
            &types::STRINGS,
            Some(json!(self.live().shared.autoscaling_failover.metrics)),
            "autoscaling.failover.metrics",
        )?;
        Ok(json!({"all": [
            {"kind_in": ["counter", "rate", "gauge", "set"]},
            {"flag": self.bound("autoscaling_failover.enabled")?},
            {"name": {"matches": {"values": metrics, "prefix": false}}},
        ]}))
    }
}

#[cfg(test)]
pub(super) mod matrix;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod legacy_tests;

#[cfg(test)]
mod interpreter_tests;

#[cfg(test)]
mod export_tests;
