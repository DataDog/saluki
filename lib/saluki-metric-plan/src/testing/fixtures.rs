//! Hand-written default-program fixtures and the catalog they validate against.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::{
    canonical::{BINDING_MEMBER, RESOLVED_MEMBER},
    compile::FrameHosts,
    ir::{BindingName, HostId, NativeComponentId, NodeId, OutputDecl, OutputId, ParamType, Plan, RecipientId},
    model::{Kind, KindSet, PortType, TimestampForm},
    operators::SourceBinding,
    validate::{BindingClass, Catalog, CatalogBinding, CatalogOutput, Liveness, MemoryCatalog},
};

/// The default program with every optional branch wired (host tags, two endpoint routing groups, multi-region
/// failover, and autoscaling failover) and the no-aggregation pipeline on, as it is by default.
///
/// This is `fixtures/default-program.json`, written by hand from the implementation design. It validates with no
/// findings against [`default_program_catalog`].
pub const DEFAULT_PROGRAM_JSON: &str = include_str!("../../fixtures/default-program.json");

/// Returns the predicate of the default-program fixtures' listener filterlist (`dsd_prefix_filter_blocklist`) for the
/// filterlist `values`, a JSON list of names, in normal form, as lowering writes it: a filterlist with nothing in it
/// keeps everything.
pub fn listener_filterlist(values: Value) -> Value {
    match names_match(values) {
        Some(matched) => json!({"not": {"intake_name": matched}}),
        None => json!({"const": true}),
    }
}

/// Returns the predicate of the default-program fixtures' post-aggregation filter (`dsd_post_agg_filter`) for the
/// filterlist entries `values`, a JSON list of names, in normal form, as lowering writes it.
pub fn post_aggregate_filter(values: Value) -> Value {
    match names_match(values) {
        Some(matched) => json!({"not": {"all": [
            {"kind_in": ["counter", "rate", "gauge", "set"]},
            {"intake_name": matched},
        ]}}),
        None => json!({"const": true}),
    }
}

/// Returns the normal form of an exact match of the names `values`, each kept once: `eq` of one, `in` of several, or
/// `None` for none, which matches nothing.
fn names_match(values: Value) -> Option<Value> {
    let mut names = Vec::new();
    for value in values.as_array().expect("a list of names") {
        if !names.contains(value) {
            names.push(value.clone());
        }
    }
    match names.len() {
        0 => None,
        1 => Some(json!({"eq": names[0]})),
        _ => Some(json!({"in": names})),
    }
}

/// The default program of [`DEFAULT_PROGRAM_JSON`] with the no-aggregation pipeline off: no timestamp split, and no
/// merge before the post-aggregation filter.
pub const NO_AGGREGATION_PIPELINE_OFF_JSON: &str =
    include_str!("../../fixtures/default-program-no-aggregation-pipeline-off.json");

/// The outputs and resources of the default program as stage 7 lowering resolves them, written by hand: every output
/// but the client telemetry tap bound to a delivery host (the main output and the two routing groups to `dd_out`, the
/// failover outputs each to its own), with fingerprints of version 2 derived by hand from each binding, every
/// delivery host's settings, and every recipient; and the outputs `resources_team_d` and `resources_team_e`, each with
/// its recipient, which a resources artifact declares and nothing in the default program emits to.
///
/// Encoder parameters and protocols are illustrative, since the fixtures' catalogs declare no host schema: an encoder's
/// `series` is `v2` or `v3`, and a recipient whose protocol says `v3.series` takes V3 series and so no V2 series. Each
/// output encodes the series variant its recipients take: team E's recipient takes V3 series, and its output encodes
/// them so; every other recipient takes V2 series.
///
/// This is `fixtures/default-program-resources.json`: an object with the plan members `outputs` and `resources`, which
/// [`with_resources`] puts in a default-program fixture.
pub const DEFAULT_PROGRAM_RESOURCES_JSON: &str = include_str!("../../fixtures/default-program-resources.json");

/// Returns a default-program fixture with its outputs bound and its resources declared as
/// [`DEFAULT_PROGRAM_RESOURCES_JSON`] has them. It validates with no findings against
/// [`default_program_resources_catalog`], and its frame has the delivery hosts of [`default_program_hosts`].
pub fn with_resources(plan_json: &str) -> Value {
    let mut value = serde_json::from_str::<Value>(plan_json).expect("default-program fixtures are JSON");
    let resources =
        serde_json::from_str::<Value>(DEFAULT_PROGRAM_RESOURCES_JSON).expect("the resources fixture is JSON");
    value["outputs"] = resources["outputs"].clone();
    value["resources"] = resources["resources"].clone();
    value
}

/// Returns the delivery hosts of a default-program fixture with resources ([`with_resources`]), each with its encode
/// host, as the host builds them: `dd_out` with `dd_metrics_encode`, `mrf_dd_out` with `mrf_metrics_encode`, and
/// `cluster_agent_out` with `af_metrics_encode`.
pub fn default_program_hosts() -> FrameHosts {
    [
        ("dd_out", "dd_metrics_encode"),
        ("mrf_dd_out", "mrf_metrics_encode"),
        ("cluster_agent_out", "af_metrics_encode"),
    ]
    .into_iter()
    .fold(FrameHosts::new(), |hosts, (host, encoder)| {
        hosts.with_host(HostId::new(host).unwrap(), NativeComponentId::new(encoder).unwrap())
    })
}

/// Returns the catalog a default-program fixture with resources ([`with_resources`]) validates against:
/// [`default_program_catalog`], with each output declared as the resources fixture binds it.
pub fn default_program_resources_catalog() -> MemoryCatalog {
    let resources =
        serde_json::from_str::<Value>(DEFAULT_PROGRAM_RESOURCES_JSON).expect("the resources fixture is JSON");
    let outputs = serde_json::from_value::<Vec<OutputDecl>>(resources["outputs"].clone())
        .expect("the resources fixture declares outputs");
    outputs.into_iter().fold(default_program_catalog(), |catalog, output| {
        catalog.with_output(
            output.id,
            CatalogOutput {
                accepts: output.accepts,
                recipients: output.recipients,
            },
        )
    })
}

/// Names the region host of a slot of a default-program fixture's frame, as the host names them.
///
/// The slot holding the DogStatsD mapper is `dsd_pre_aggregation`, the slot holding the enriched metrics (the reserved
/// node `metrics`, the last node of the enrichment chain, whichever operator it is) is `metrics_enrich`, and the isolation slot holding DogStatsD aggregation is `dsd_agg`. Any other slot, one that R1's
/// cost model cut from a pipeline, is named after its first node, without the `_in` of a merge that starts it, as
/// `<node>_region`, except that a slot the namespace prefix or the enrichment merge starts is named for the native
/// component that node stands for: `dsd_prefix_filter_region` or `metrics_enrich_region`.
///
/// # Errors
///
/// Never fails: every slot gets a name.
pub fn default_program_slot_name(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    let has = |id: &str| nodes.iter().any(|node| node.local().as_str() == id);
    let name = if has("dsd_agg") {
        "dsd_agg".to_string()
    } else if has("dsd_enrich") {
        "dsd_pre_aggregation".to_string()
    } else if nodes.iter().any(|node| node.to_string() == "metrics") {
        "metrics_enrich".to_string()
    } else {
        let first = match nodes.first().map_or("slot", |node| node.local().as_str()) {
            "dsd_prefix_filter_blocklist_namespace_prefix" => "dsd_prefix_filter",
            "metrics_in" => "metrics_enrich",
            first => first.strip_suffix("_in").unwrap_or(first),
        };
        format!("{}_region", first)
    };
    NativeComponentId::new(name).map_err(|error| error.to_string())
}

/// The endpoint routing filters and failover gateways of the default-program fixtures, with the type each one's output
/// accepts.
const GATEWAYS: [(&str, bool); 4] = [
    ("metrics_routing_filter_0", false),
    ("metrics_routing_filter_1", false),
    ("mrf_metrics_gateway", false),
    ("af_metrics_gateway", true),
];

/// Returns a default-program fixture with its endpoint routing filters and failover gateways as opaque hosts, as the
/// default program has them until they lower to routing operators (stage 4).
///
/// Each gateway `split` becomes an `opaque_host` backed by the native component of the same ID, with one output port
/// `out` of the type its output accepts; its `true` terminal becomes the `emit` named `<gateway>_out` that the port
/// feeds, and its `false` terminal, a `drop`, is removed, since the native component drops what it does not forward.
/// This is the shape native lowering produces for the same configuration.
pub fn with_opaque_gateways(plan_json: &str) -> Value {
    let mut value = serde_json::from_str::<Value>(plan_json).expect("default-program fixtures are JSON");
    let all = json!({"kinds": "all", "timestamps": "mixed"});
    let counters = json!({"kinds": ["counter", "rate", "gauge", "set"], "timestamps": "mixed"});

    let nodes = value["nodes"].as_array_mut().expect("plans have nodes");
    nodes.retain(|node| !GATEWAYS.iter().any(|(id, _)| node["id"] == format!("{}_false", id)));
    for node in nodes.iter_mut() {
        if let Some((id, only_counters)) = GATEWAYS.iter().find(|(id, _)| node["id"] == *id) {
            let output = if *only_counters { counters.clone() } else { all.clone() };
            node["op"] = json!("opaque_host");
            node["params"] = json!({"component": id, "input": all, "outputs": {"out": output}});
        }
        if let Some((id, _)) = GATEWAYS.iter().find(|(id, _)| node["id"] == format!("{}_true", id)) {
            node["id"] = json!(format!("{}_out", id));
        }
    }
    let edges = value["edges"].as_array_mut().expect("plans have edges");
    edges.retain(|edge| !GATEWAYS.iter().any(|(id, _)| edge["from"] == format!("{}.false", id)));
    for edge in edges.iter_mut() {
        if let Some((id, _)) = GATEWAYS.iter().find(|(id, _)| edge["from"] == format!("{}.true", id)) {
            edge["from"] = json!(format!("{}.out", id));
            edge["to"] = json!(format!("{}_out.in", id));
        }
    }
    value
}

const STRINGS: ParamType = ParamType::List(&ParamType::String);

/// The sources of the default-program fixtures: for each, the name an artifact edge reads it by (`input.<name>`), the
/// port it names (the `metrics` port of its `source` node), and the source's binding.
pub const DEFAULT_PROGRAM_SOURCES: [(&str, &str, SourceBinding); 5] = [
    ("dogstatsd", "dsd_in.metrics", SourceBinding::Dogstatsd),
    ("otlp", "otlp_in.metrics", SourceBinding::Otlp),
    ("checks", "checks_ipc_in.metrics", SourceBinding::Checks),
    ("liveness", "liveness_in.metrics", SourceBinding::Liveness),
    ("trace_sampler", "trace_sampler.metrics", SourceBinding::TraceSampler),
];

/// Returns the catalog the host would declare for the default program: its outputs, the configuration bindings that
/// lowering uses with their types and liveness, and that the host runs at most one aggregation per program.
///
/// Binding names, types, and liveness are those of the host's binding registry, whose tests check that they agree.
pub fn default_program_catalog() -> MemoryCatalog {
    catalog_outputs_and_bindings().with_single_aggregation()
}

/// Returns the host's catalog of a default program: the fixtures' catalog, with every binding the program resolves
/// carrying the value it resolves to, as a host's catalog carries the values lowering writes.
pub fn catalog_for(plan: &Plan) -> MemoryCatalog {
    fn collect(value: &Value, found: &mut BTreeMap<BindingName, Value>) {
        match value {
            Value::Object(members) => {
                if let (Some(Value::String(name)), Some(resolved)) =
                    (members.get(BINDING_MEMBER), members.get(RESOLVED_MEMBER))
                {
                    found.insert(name.parse().expect("bindings are named validly"), resolved.clone());
                }
                members.values().for_each(|member| collect(member, found));
            }
            Value::Array(values) => values.iter().for_each(|value| collect(value, found)),
            _ => {}
        }
    }

    let mut found = BTreeMap::new();
    for node in &plan.nodes {
        node.params.iter().for_each(|(_, value)| collect(value, &mut found));
    }
    let mut catalog = default_program_catalog();
    for (name, value) in found {
        let declared = catalog
            .binding(&name)
            .expect("the catalog declares every binding")
            .clone();
        catalog = catalog.with_catalog_binding(
            name,
            CatalogBinding {
                value: Some(value),
                ..declared
            },
        );
    }
    catalog
}

/// Returns `catalog` without the restriction to one aggregation per program, for tests of what placement and the
/// runtime do with several state owners, which the plan crate supports.
pub fn without_single_aggregation(catalog: MemoryCatalog) -> MemoryCatalog {
    let mut document = serde_json::to_value(catalog).expect("catalogs are written as documents");
    if let Some(members) = document.as_object_mut() {
        members.remove("single_aggregation");
    }
    serde_json::from_value(document).expect("the catalog document reads back")
}

/// The outputs and bindings of [`default_program_catalog`].
fn catalog_outputs_and_bindings() -> MemoryCatalog {
    let all = PortType::ANY;
    let counters = PortType {
        kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
        timestamps: TimestampForm::Mixed,
    };
    let outputs: [(&str, PortType, &[&str]); 6] = [
        ("dsd_client_telemetry", all, &[]),
        ("dd_metrics_encode", all, &["dd_out/primary/0"]),
        ("metrics_routing_encode_0", all, &["dd_out/additional_0/0"]),
        (
            "metrics_routing_encode_1",
            all,
            &["dd_out/additional_1/0", "dd_out/additional_1/1"],
        ),
        ("mrf_metrics_encode", all, &["mrf_dd_out/primary/0"]),
        ("af_metrics_encode", counters, &["cluster_agent_out/primary/0"]),
    ];

    // The facts and signals the default program binds, as the host classifies them: the host's tags and whether
    // failover is configured are facts, and the two failover switches signals. Every policy setting is a literal.
    let bindings: [(&str, ParamType, Liveness, BindingClass); 5] = [
        (
            "multi_region_failover.enabled",
            ParamType::Bool,
            Liveness::Live,
            BindingClass::Fact,
        ),
        (
            "multi_region_failover.metric_mirroring.enabled",
            ParamType::Bool,
            Liveness::Live,
            BindingClass::Signal,
        ),
        (
            "autoscaling_failover.enabled",
            ParamType::Bool,
            Liveness::Live,
            BindingClass::Signal,
        ),
        ("host.tags", STRINGS, Liveness::Startup, BindingClass::Fact),
        (
            "host.tags_deadline",
            ParamType::Duration,
            Liveness::Startup,
            BindingClass::Fact,
        ),
    ];

    let mut catalog = MemoryCatalog::new();
    for (id, accepts, recipients) in outputs {
        catalog = catalog.with_output(
            OutputId::new(id).unwrap(),
            CatalogOutput {
                accepts,
                recipients: recipients.iter().map(|r| RecipientId::new(*r).unwrap()).collect(),
            },
        );
    }
    for (name, ty, liveness, class) in bindings {
        catalog = catalog.with_classified_binding(BindingName::new(name).unwrap(), ty, liveness, class);
    }
    catalog
}
