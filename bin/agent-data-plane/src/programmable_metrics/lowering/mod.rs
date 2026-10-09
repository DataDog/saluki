//! Lowering: native configuration into the default program.
//!
//! [`Lowering::lower`] is a pure function of the startup configuration, the current configuration, and the [host
//! facts](HostFacts). It produces the default program: the logical plan of the native metrics path that `run.rs`
//! builds for the same configuration, node for node. [`Lowering::new`] prepares it once per process, from the startup
//! configuration and the host facts: it checks what the native topology build checks, and resolves the native
//! outputs, which depend on nothing else. Re-lowering on a configuration change reuses both.
//!
//! # Shape
//!
//! The program is the one the implementation design draws in "Default program shape"
//! (`docs/design/programmable-metrics-implementation.md`), in its stage 2 form. The DogStatsD source feeds the mapper,
//! the namespace prefix, and the listener filterlist (a `split` whose `true` port drops); with the no-aggregation
//! pipeline, a timestamp split sends timestamped points past the tag filterlist and aggregation to a merge in front of
//! the post-aggregation filter (another `split` and `drop`), whose output a `tee` sends to the client telemetry output
//! and to metrics enrichment. Metrics enrichment merges that with the OTLP, checks, liveness, and trace sampler
//! sources, runs host enrichment and host tags, and feeds the `post_enrich` scope: a `tee` with one branch per output,
//! the main output emitted directly and each gateway an opaque host whose `out` port is emitted to its output.
//!
//! Every branch exists only when `run.rs` builds it, decided from the startup configuration by the same predicates and
//! helpers. Until stage 4, the policy gateways are opaque hosts backed by today's components, with their parameters in
//! those components. The metrics enrichment merge (`metrics_enrich_in`) and the tees (`metrics_policy`,
//! `dsd_post_agg_filter_fanout`) are present even with one input or output, so that their IDs never depend on which
//! other branches exist. The merge in front of the post-aggregation filter (`dsd_post_agg_filter_in`) exists only with
//! the no-aggregation pipeline, whose passthrough is its second input, as natively that filter has a second input
//! only then.
//!
//! # Parameters
//!
//! Every configuration-derived parameter is bound to a [binding](super::bindings) and carries its resolved value:
//! `Startup` bindings from the startup configuration, `Live` ones from the current configuration. Structure always
//! comes from the startup configuration. So re-lowering with a changed live input changes only the parameters bound to
//! it, and re-lowering with a changed startup-only input changes nothing, as natively. Host facts (the host tags and
//! their deadline) and structural parameters (port counts, source bindings, opaque-host port types) are literals.
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

use std::{sync::Arc, time::Duration};

use agent_data_plane_config::SalukiConfiguration;
use saluki_components::transforms::HistogramConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_kernels::mapper::validate_profiles;
use saluki_metric_plan::{
    canonical::MAX_SAFE_INTEGER,
    ir::{
        derived_terminal_id, format_duration, BindingName, Edge, NativeComponentId, Node, NodeId, OperatorName,
        ParamName, Params, Plan, PortName, PortRef, Provenance, ScopeDecl, IR_VERSION,
    },
    operators::{names, METRICS_PORT},
};
use serde_json::{json, Value};

use super::{
    bindings::{self, Snapshots},
    catalog::{AdpCatalog, POST_ENRICH_INPUT, POST_ENRICH_SCOPE},
    host_facts::{host_tags_enabled, HostFacts},
    outputs::{cluster_agent_forwarder, NativeOutputs, OutputKind, DSD_CLIENT_TELEMETRY, DSD_CLIENT_TELEMETRY_OUT},
};
use crate::{
    cli::run::dogstatsd_mapper_configuration, components::tag_filterlist::value_rules, config::DataPlaneConfiguration,
};

/// The reserved node IDs of the default program, which are, or derive from, native component IDs.
pub mod ids {
    /// The DogStatsD source.
    pub const DSD_IN: &str = "dsd_in";
    /// The DogStatsD mapper.
    pub const DSD_ENRICH: &str = "dsd_enrich";
    /// The namespace prefix of the DogStatsD prefix filter.
    pub const DSD_PREFIX_FILTER: &str = "dsd_prefix_filter";
    /// The listener filterlist of the DogStatsD prefix filter.
    pub const DSD_PREFIX_FILTER_BLOCKLIST: &str = "dsd_prefix_filter_blocklist";
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
    pub const METRICS_ENRICH_IN: &str = "metrics_enrich_in";
    /// Host enrichment.
    pub const METRICS_ENRICH: &str = "metrics_enrich";
    /// Host tags.
    pub const METRICS_ENRICH_HOST_TAGS: &str = "metrics_enrich_host_tags";
    /// The fan-out of metrics enrichment to every output branch.
    pub const METRICS_POLICY: &str = "metrics_policy";
    /// The multi-region failover gateway.
    pub const MRF_METRICS_GATEWAY: &str = "mrf_metrics_gateway";
    /// The autoscaling failover gateway.
    pub const AF_METRICS_GATEWAY: &str = "af_metrics_gateway";

    /// Returns the ID of the filter of the endpoint routing group at `index`, which is also its component ID.
    pub fn routing_filter(index: usize) -> String {
        format!("metrics_routing_filter_{}", index)
    }
}

/// The output port of an opaque host in the default program.
pub const OPAQUE_OUTPUT_PORT: &str = "out";

/// A place where lowering departs from the configuration as written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoweringNote {
    /// The node whose parameters depart from the configuration.
    pub node: NodeId,

    /// What lowering did, and why.
    pub message: String,
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
}

/// Lowering for one process: its startup configuration and host facts, checked as native construction checks them,
/// and its native outputs.
///
/// Build it once at boot, and [lower](Self::lower) whenever the configuration changes.
#[derive(Clone, Debug)]
pub struct Lowering {
    startup: Arc<SalukiConfiguration>,
    facts: HostFacts,
    outputs: NativeOutputs,
}

impl Lowering {
    /// Prepares lowering for a process that started with the configuration `startup` and the host facts `facts`.
    ///
    /// Returns `None` when the configuration requires no metrics pipeline, so there is no metrics path to describe.
    /// Otherwise it checks the configuration as native construction of the metrics path does, and resolves the native
    /// outputs, which depend only on the startup configuration and the host facts.
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
        let outputs = NativeOutputs::from_configuration(&startup, &facts)?;
        Ok(Some(Self {
            startup,
            facts,
            outputs,
        }))
    }

    /// Returns the native outputs the program delivers to.
    pub fn outputs(&self) -> &NativeOutputs {
        &self.outputs
    }

    /// Lowers native configuration into the default program.
    ///
    /// Structure and `Startup` bindings come from the startup configuration; `Live` bindings from `current`, the
    /// configuration now.
    ///
    /// # Errors
    ///
    /// Returns an error for the configurations that run natively but cannot be lowered; see the [module
    /// documentation](self#errors).
    pub fn lower(&self, current: &SalukiConfiguration) -> Result<Lowered, GenericError> {
        let startup = &*self.startup;
        let dp = DataPlaneConfiguration::from_configuration(startup);
        let snapshots = Snapshots { startup, current };
        let mut program = Program::new(snapshots);
        let mut notes = Vec::new();

        // Everything feeding metrics enrichment, in a fixed order.
        let mut enrichment_inputs = Vec::new();
        if dp.dogstatsd_enabled() {
            enrichment_inputs.push(program.dogstatsd(startup, &mut notes)?);
        }
        let sources = [
            (ids::OTLP_IN, "otlp", dp.otlp_enabled() && !dp.otlp_proxy_enabled()),
            (ids::CHECKS_IPC_IN, "checks", dp.checks_enabled()),
            (ids::LIVENESS_IN, "liveness", !dp.standalone_mode()),
            (ids::TRACE_SAMPLER, "trace_sampler", dp.traces_pipeline_required()),
        ];
        for (id, binding, present) in sources {
            if present {
                let source = program.node(id, names::SOURCE, json!({"binding": binding}), id);
                enrichment_inputs.push(PortRef::new(source, port(METRICS_PORT)));
            }
        }
        if enrichment_inputs.is_empty() {
            return Err(generic_error!(
                "The metrics pipeline is required, but no source feeds it; this is a lowering bug."
            ));
        }

        let enriched = program.enrichment(startup, &self.facts, enrichment_inputs)?;
        program.policy(enriched.clone(), &self.outputs);

        let mut plan = Plan {
            ir_version: IR_VERSION,
            provenance: None,
            nodes: program.nodes,
            edges: program.edges,
            outputs: self.outputs.decls(),
            scopes: Default::default(),
        };
        plan.scopes.insert(
            POST_ENRICH_SCOPE.parse().expect("scope names are identifiers"),
            ScopeDecl {
                inputs: [(
                    POST_ENRICH_INPUT.parse().expect("input names are identifiers"),
                    enriched,
                )]
                .into(),
                exits: Default::default(),
            },
        );

        let catalog = AdpCatalog::new(&self.outputs, snapshots);
        Ok(Lowered { plan, catalog, notes })
    }
}

/// Checks the startup configuration as native construction of the metrics path checks it, in the same order and with
/// the same errors, except for forwarder endpoints, which resolving the native outputs checks last.
///
/// `run.rs` assembles the metrics output pipelines (endpoint routing, then the failover branches) before the DogStatsD
/// pipeline, and then builds every component, transforms before forwarders.
fn check_native_construction(startup: &SalukiConfiguration, facts: &HostFacts) -> Result<(), GenericError> {
    // Assembling the blueprint.
    bindings::routing_groups(startup)?;
    cluster_agent_forwarder(startup, facts)?;
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
}

impl<'a> Program<'a> {
    fn new(snapshots: Snapshots<'a>) -> Self {
        Self {
            snapshots,
            nodes: Vec::new(),
            edges: Vec::new(),
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
            provenance: Some(Provenance::Native(
                NativeComponentId::new(native).expect("native component IDs are valid"),
            )),
        });
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

    /// Returns the parameter value binding `name`, with its resolved value.
    fn bound(&self, name: &str) -> Result<Value, GenericError> {
        let binding = BindingName::new(name).expect("registry binding names are valid");
        let resolved = bindings::resolve(&binding, self.snapshots)
            .ok_or_else(|| generic_error!("The configuration has no value for the binding `{}`.", name))?;
        Ok(json!({"config": name, "resolved": resolved}))
    }

    /// Lowers the DogStatsD pre-aggregation path, aggregation, and the post-aggregation filter, and returns the port
    /// that feeds metrics enrichment.
    fn dogstatsd(
        &mut self, startup: &SalukiConfiguration, notes: &mut Vec<LoweringNote>,
    ) -> Result<PortRef, GenericError> {
        // `Lowering::new` checked what native construction rejects.
        let dogstatsd = &startup.domains.dogstatsd;
        let no_aggregation_pipeline = dogstatsd.aggregation.no_aggregation_pipeline;
        let timestamps = if no_aggregation_pipeline {
            "mixed"
        } else {
            "untimestamped"
        };
        let dsd_in = self.node(
            ids::DSD_IN,
            names::SOURCE,
            json!({"binding": "dogstatsd", "timestamps": timestamps}),
            ids::DSD_IN,
        );

        let params = json!({
            "profiles": self.bound("dogstatsd.mapper.profiles")?,
            "cache_size": self.bound("dogstatsd.mapper.cache_size")?,
            "string_interner_bytes": self.bound("dogstatsd.mapper.string_interner_size_bytes")?,
        });
        let mapper = self.node(ids::DSD_ENRICH, names::DOGSTATSD_MAPPER, params, ids::DSD_ENRICH);
        self.edge(PortRef::new(dsd_in, port(METRICS_PORT)), &mapper, "in");

        let params = json!({
            "namespace": self.bound("dogstatsd.prefix_filter.metric_namespace")?,
            "exclusions": self.bound("dogstatsd.prefix_filter.metric_namespace_blocklist")?,
        });
        let prefix = self.node(
            ids::DSD_PREFIX_FILTER,
            names::NAMESPACE_PREFIX,
            params,
            ids::DSD_PREFIX_FILTER,
        );
        self.edge(PortRef::new(mapper, port("out")), &prefix, "in");

        // The listener filterlist matches the intake name after prefixing, as the native prefix filter does.
        let predicate = json!({"intake_name": {"matches": {
            "values": self.bound("dogstatsd.metric_filter.values")?,
            "prefix": self.bound("dogstatsd.metric_filter.match_prefix")?,
        }}});
        let blocklist = self.node(
            ids::DSD_PREFIX_FILTER_BLOCKLIST,
            names::SPLIT,
            json!({"predicate": predicate}),
            ids::DSD_PREFIX_FILTER,
        );
        self.edge(PortRef::new(prefix, port("out")), &blocklist, "in");
        self.terminal(&blocklist, "true", names::DROP, Value::Null, ids::DSD_PREFIX_FILTER);
        let kept = PortRef::new(blocklist, port("false"));

        let (to_projection, passthrough) = if no_aggregation_pipeline {
            let params = json!({"window": self.bound("dogstatsd.aggregation.window_duration_seconds")?});
            let split = self.node(
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

        let params = json!({
            "rules": self.bound("dogstatsd.tag_filterlist")?,
            "value_rules": self.bound("dogstatsd.tag_value_allowlist")?,
            "cache_capacity": self.bound("dogstatsd.aggregation.aggregator_tag_filter_cache_capacity")?,
        });
        let projection = self.node(
            ids::DSD_TAG_FILTERLIST,
            names::TAG_PROJECTION,
            params,
            ids::DSD_TAG_FILTERLIST,
        );
        self.edge(to_projection, &projection, "in");

        // `Lowering::new` parsed the statistics, so this cannot fail.
        let (_, repeated) =
            bindings::histogram_params(startup).error_context("Failed to configure histogram aggregation.")?;
        let params = json!({
            "window": self.bound("dogstatsd.aggregation.window_duration_seconds")?,
            "flush_interval": self.bound("dogstatsd.aggregation.flush_interval")?,
            "context_limit": self.bound("dogstatsd.aggregation.context_limit")?,
            "counter_expiry": self.bound("dogstatsd.aggregation.counter_expiry_seconds")?,
            "flush_open_windows": self.bound("dogstatsd.aggregation.flush_open_windows")?,
            "histogram_aggregates": self.bound("metrics_encoding.histogram.aggregates")?,
            "histogram_percentiles": self.bound("metrics_encoding.histogram.percentiles")?,
            "copy_to_distribution": self.bound("metrics_encoding.histogram.copy_to_distribution")?,
            "copy_to_distribution_prefix": self.bound("metrics_encoding.histogram.copy_to_distribution_prefix")?,
        });
        let aggregate = self.node(ids::DSD_AGG, names::AGGREGATE, params, ids::DSD_AGG);
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

        let predicate = json!({"all": [
            {"kind_in": ["counter", "rate", "gauge", "set"]},
            {"intake_name": {"matches": {
                "values": self.bound("dogstatsd.post_aggregate_filter.values")?,
                "prefix": self.bound("dogstatsd.metric_filter.match_prefix")?,
            }}},
        ]});
        let post_aggregation = self.node(
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
        let merge = self.node(
            ids::METRICS_ENRICH_IN,
            names::MERGE,
            json!({"inputs": inputs.len()}),
            ids::METRICS_ENRICH,
        );
        for (index, input) in inputs.into_iter().enumerate() {
            self.edge(input, &merge, &index.to_string());
        }
        let enrichment = self.node(
            ids::METRICS_ENRICH,
            names::HOST_ENRICHMENT,
            Value::Null,
            ids::METRICS_ENRICH,
        );
        self.edge(PortRef::new(merge, port("out")), &enrichment, "in");
        let mut enriched = PortRef::new(enrichment, port("out"));

        if host_tags_enabled(startup) {
            let tags = facts.host_tags.as_ref().ok_or_else(|| {
                generic_error!("Host tags enrichment is enabled, but the host facts carry no host tags.")
            })?;
            let tags = tags.into_iter().map(|tag| tag.as_str().to_string()).collect::<Vec<_>>();
            // A whole number of seconds within the plan's integers, which a plan duration always represents.
            let deadline = facts.host_tags_deadline(startup.shared.tags.expected_tags_duration);
            let deadline = format_duration(deadline).ok_or_else(|| {
                generic_error!("The host tags deadline cannot be written as a duration; this is a lowering bug.")
            })?;
            let host_tags = self.node(
                ids::METRICS_ENRICH_HOST_TAGS,
                names::HOST_TAGS,
                json!({"tags": tags, "deadline": deadline}),
                ids::METRICS_ENRICH,
            );
            self.edge(enriched, &host_tags, "in");
            enriched = PortRef::new(host_tags, port("out"));
        }
        Ok(enriched)
    }

    /// Lowers the policy region fed by `enriched`: one branch per output other than the client telemetry tap.
    fn policy(&mut self, enriched: PortRef, outputs: &NativeOutputs) {
        // Each branch, with the gateway that selects what reaches its output, if any.
        let branches = outputs
            .iter()
            .filter_map(|output| {
                let gateway = match &output.kind {
                    OutputKind::ClientTelemetry => return None,
                    OutputKind::Main => None,
                    OutputKind::RoutingGroup(group) => Some(ids::routing_filter(group.index)),
                    OutputKind::MultiRegionFailover => Some(ids::MRF_METRICS_GATEWAY.to_string()),
                    OutputKind::AutoscalingFailover => Some(ids::AF_METRICS_GATEWAY.to_string()),
                };
                Some((output, gateway))
            })
            .collect::<Vec<_>>();
        let policy = self.node(
            ids::METRICS_POLICY,
            names::TEE,
            json!({"ports": branches.len()}),
            ids::METRICS_ENRICH,
        );
        self.edge(enriched, &policy, "in");

        for (index, (output, gateway)) in branches.into_iter().enumerate() {
            let emit = json!({"output": output.decl.id.as_str()});
            let branch = index.to_string();
            match gateway {
                None => self.terminal(&policy, &branch, names::EMIT, emit, &output.component),
                Some(gateway) => {
                    // Until stage 4, the gateway is its native component, which forwards what it selects.
                    let selected = serde_json::to_value(output.decl.accepts).expect("port types serialize");
                    let params = json!({
                        "component": gateway,
                        "input": {"kinds": "all", "timestamps": "mixed"},
                        "outputs": {OPAQUE_OUTPUT_PORT: selected},
                    });
                    let gateway = self.node(&gateway, names::OPAQUE_HOST, params, &gateway);
                    self.edge(PortRef::new(policy.clone(), port(&branch)), &gateway, "in");
                    self.terminal(&gateway, OPAQUE_OUTPUT_PORT, names::EMIT, emit, &output.component);
                }
            }
        }
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
