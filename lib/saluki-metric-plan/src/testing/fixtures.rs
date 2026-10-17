//! Hand-written default-program fixtures and the catalog they validate against.

use serde_json::{json, Value};

use crate::{
    canonical::{PlanHash, MAX_SAFE_INTEGER},
    compile::FrameHosts,
    ir::{
        BindingName, HostId, InputName, NativeComponentId, NodeId, OutputDecl, OutputId, ParamSpec, ParamType,
        RecipientId,
    },
    model::{Kind, KindSet, PortType, TimestampForm},
    operators::SourceBinding,
    validate::{BindingClass, CatalogOutput, Liveness, MemoryCatalog},
};

/// The default program with every optional branch wired (host tags, two endpoint routing groups, multi-region
/// failover, and autoscaling failover) and the no-aggregation pipeline on, as it is by default.
///
/// This is `fixtures/default-program.json`, written by hand from the implementation design. It validates with no
/// findings against [`default_program_catalog`].
pub const DEFAULT_PROGRAM_JSON: &str = include_str!("../../fixtures/default-program.json");

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
                fingerprint: output.fingerprint,
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
const MAPPING_TAG: &[ParamSpec] = &[
    ParamSpec::required("name", ParamType::String),
    ParamSpec::required("value", ParamType::String),
];
const MAPPING: &[ParamSpec] = &[
    ParamSpec::required("match", ParamType::String),
    ParamSpec::required("match_type", ParamType::Enum(&["wildcard", "regex"])),
    ParamSpec::required("name", ParamType::String),
    ParamSpec::optional("tags", ParamType::Set(&ParamType::Record(MAPPING_TAG))),
];
const PROFILE: &[ParamSpec] = &[
    ParamSpec::required("name", ParamType::String),
    ParamSpec::required("prefix", ParamType::String),
    ParamSpec::required("mappings", ParamType::List(&ParamType::Record(MAPPING))),
];
const TAG_RULE: &[ParamSpec] = &[
    ParamSpec::required("metric_name", ParamType::String),
    ParamSpec::required("action", ParamType::Enum(&["include", "exclude"])),
    ParamSpec::required("tags", STRINGS),
];
const TAG_VALUE_RULE: &[ParamSpec] = &[
    ParamSpec::required("metric_prefix", ParamType::String),
    ParamSpec::required("tag_name", ParamType::String),
    ParamSpec::required("values", STRINGS),
    ParamSpec::required("on_miss", ParamType::Enum(&["remove", "replace"])),
    ParamSpec::optional("replacement", ParamType::String),
];

/// Returns the hash whose 64 hexadecimal digits are all `digit`.
fn hash(digit: char) -> PlanHash {
    format!("sha256:{}", digit.to_string().repeat(64))
        .parse()
        .expect("64 hexadecimal digits form a hash")
}

/// The one scope of the default-program fixtures, which an artifact replaces: the whole metrics path after the sources.
pub const DEFAULT_PROGRAM_SCOPE: &str = "metrics";

/// The inputs of the default-program fixtures' scope ([`DEFAULT_PROGRAM_SCOPE`]): for each, its name, the port it names
/// (the `metrics` port of one source), and the source's binding.
pub const DEFAULT_PROGRAM_SCOPE_INPUTS: [(&str, &str, SourceBinding); 5] = [
    ("dogstatsd", "dsd_in.metrics", SourceBinding::Dogstatsd),
    ("otlp", "otlp_in.metrics", SourceBinding::Otlp),
    ("checks", "checks_ipc_in.metrics", SourceBinding::Checks),
    ("liveness", "liveness_in.metrics", SourceBinding::Liveness),
    ("trace_sampler", "trace_sampler.metrics", SourceBinding::TraceSampler),
];

/// Returns the catalog the host would declare for the default program: its outputs, the configuration bindings that
/// lowering uses with their types and liveness, the inputs of the `metrics` scope, each carrying what its source
/// produces (DogStatsD with client timestamps, as the no-aggregation pipeline parses them), and that the host runs at
/// most one aggregation per program.
///
/// Binding names, types, and liveness are those of the host's binding registry, whose tests check that they agree.
/// Endpoint routing group keys are placeholders.
///
/// Output fingerprints are placeholder hashes whose digits are all the same: `1` for `dsd_client_telemetry` through
/// `6` for `af_metrics_encode`, in the order the fixtures declare the outputs.
pub fn default_program_catalog() -> MemoryCatalog {
    DEFAULT_PROGRAM_SCOPE_INPUTS
        .iter()
        .fold(catalog_without_scopes(), |catalog, (input, _, binding)| {
            catalog.with_scope_input(
                DEFAULT_PROGRAM_SCOPE.parse().unwrap(),
                InputName::new(*input).unwrap(),
                binding.port_type(TimestampForm::Mixed),
            )
        })
        .with_single_aggregation()
}

/// The two scopes the default program declared through stage 9, before its one `metrics` scope: `post_enrich`, the
/// policy region after host tags, and `dogstatsd_pre_aggregation`, DogStatsD's processing up to and including
/// aggregation, which ends at exits.
///
/// The host no longer declares them. Tests of what the plan crate supports in general, but the default program no
/// longer uses, keep them: a scope that covers part of a plan, a scope's exits, and artifacts of several scopes composed
/// together.
pub const TWO_SCOPES: [&str; 2] = ["dogstatsd_pre_aggregation", "post_enrich"];

/// Returns a default-program fixture with its scope replaced by the [two scopes it declared through stage
/// 9](TWO_SCOPES): it validates with no findings against [`two_scope_catalog`].
///
/// `dogstatsd_pre_aggregation`'s exits are the inputs of the post-aggregation filter: of the merge in front of it with
/// the no-aggregation pipeline (`aggregated` and `passthrough`), of the filter itself without (`aggregated`).
pub fn with_two_scopes(mut value: Value) -> Value {
    let has_passthrough = value["nodes"]
        .as_array()
        .expect("plans have nodes")
        .iter()
        .any(|node| node["id"] == "dsd_post_agg_filter_in");
    let exits = if has_passthrough {
        json!({"aggregated": "dsd_post_agg_filter_in.0", "passthrough": "dsd_post_agg_filter_in.1"})
    } else {
        json!({"aggregated": "dsd_post_agg_filter.in"})
    };
    value["scopes"] = json!({
        "dogstatsd_pre_aggregation": {"inputs": {"metrics": "dsd_in.metrics"}, "exits": exits},
        "post_enrich": {"inputs": {"metrics": "metrics.out"}}
    });
    value
}

/// Returns the catalog of a default-program fixture with [two scopes](with_two_scopes): [`default_program_catalog`]
/// as [`two_scopes_of`] declares it.
pub fn two_scope_catalog() -> MemoryCatalog {
    two_scopes_of(default_program_catalog())
}

/// Returns `catalog` with the input `metrics` of each of the [two scopes](TWO_SCOPES), carrying anything, instead of
/// the scopes it declares, and without the restriction to one aggregation, as the host declared it through stage 9.
pub fn two_scopes_of(catalog: MemoryCatalog) -> MemoryCatalog {
    let mut document = serde_json::to_value(without_single_aggregation(catalog)).expect("catalogs are written");
    let members = document.as_object_mut().expect("a catalog document is an object");
    let inputs = json!({"metrics": {"kinds": "all", "timestamps": "mixed"}});
    members.insert(
        "scopes".to_string(),
        TWO_SCOPES
            .iter()
            .map(|scope| (scope.to_string(), inputs.clone()))
            .collect(),
    );
    serde_json::from_value(document).expect("the catalog document reads back")
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

/// The outputs and bindings of [`default_program_catalog`], without scopes.
fn catalog_without_scopes() -> MemoryCatalog {
    let all = PortType::ANY;
    let counters = PortType {
        kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
        timestamps: TimestampForm::Mixed,
    };
    let outputs: [(&str, PortType, char, &[&str]); 6] = [
        ("dsd_client_telemetry", all, '1', &[]),
        ("dd_metrics_encode", all, '2', &["dd_out/primary/0"]),
        ("metrics_routing_encode_0", all, '3', &["dd_out/additional_0/0"]),
        (
            "metrics_routing_encode_1",
            all,
            '4',
            &["dd_out/additional_1/0", "dd_out/additional_1/1"],
        ),
        ("mrf_metrics_encode", all, '5', &["mrf_dd_out/primary/0"]),
        ("af_metrics_encode", counters, '6', &["cluster_agent_out/primary/0"]),
    ];

    // Counts are integers within the plan's integer range, as the host declares them.
    let count = |max| ParamType::Integer { min: 0, max };
    let bindings: Vec<(&str, ParamType, Liveness)> = vec![
        (
            "dogstatsd.mapper.profiles",
            ParamType::List(&ParamType::Record(PROFILE)),
            Liveness::Startup,
        ),
        (
            "dogstatsd.mapper.cache_size",
            count(MAX_SAFE_INTEGER),
            Liveness::Startup,
        ),
        (
            "dogstatsd.mapper.string_interner_size_bytes",
            ParamType::Integer {
                min: 1,
                max: MAX_SAFE_INTEGER,
            },
            Liveness::Startup,
        ),
        (
            "dogstatsd.prefix_filter.metric_namespace",
            ParamType::String,
            Liveness::Startup,
        ),
        (
            "dogstatsd.prefix_filter.metric_namespace_blocklist",
            STRINGS,
            Liveness::Startup,
        ),
        ("dogstatsd.metric_filter.values", STRINGS, Liveness::Live),
        ("dogstatsd.metric_filter.match_prefix", ParamType::Bool, Liveness::Live),
        ("dogstatsd.post_aggregate_filter.values", STRINGS, Liveness::Live),
        (
            "dogstatsd.tag_filterlist",
            ParamType::List(&ParamType::Record(TAG_RULE)),
            Liveness::Live,
        ),
        (
            "dogstatsd.tag_value_allowlist",
            ParamType::List(&ParamType::Record(TAG_VALUE_RULE)),
            Liveness::Startup,
        ),
        (
            "dogstatsd.aggregation.window_duration_seconds",
            ParamType::Duration,
            Liveness::Startup,
        ),
        (
            "dogstatsd.aggregation.flush_interval",
            ParamType::Duration,
            Liveness::Startup,
        ),
        (
            "dogstatsd.aggregation.context_limit",
            count(MAX_SAFE_INTEGER),
            Liveness::Startup,
        ),
        (
            "dogstatsd.aggregation.counter_expiry_seconds",
            ParamType::Duration,
            Liveness::Startup,
        ),
        (
            "dogstatsd.aggregation.flush_open_windows",
            ParamType::Bool,
            Liveness::Startup,
        ),
        (
            "dogstatsd.aggregation.aggregator_tag_filter_cache_capacity",
            count(MAX_SAFE_INTEGER),
            Liveness::Startup,
        ),
        (
            "metrics_encoding.histogram.aggregates",
            ParamType::List(&ParamType::Enum(&["count", "sum", "min", "max", "avg", "median"])),
            Liveness::Startup,
        ),
        (
            "metrics_encoding.histogram.percentiles",
            ParamType::List(&ParamType::Integer { min: 0, max: 100 }),
            Liveness::Startup,
        ),
        (
            "metrics_encoding.histogram.copy_to_distribution",
            ParamType::Bool,
            Liveness::Startup,
        ),
        (
            "metrics_encoding.histogram.copy_to_distribution_prefix",
            ParamType::String,
            Liveness::Startup,
        ),
        (
            "metrics_endpoint_routing.group.0123456789abcdef.names",
            STRINGS,
            Liveness::Live,
        ),
        (
            "metrics_endpoint_routing.group.0123456789abcdef.prefixes",
            STRINGS,
            Liveness::Live,
        ),
        (
            "metrics_endpoint_routing.group.fedcba9876543210.names",
            STRINGS,
            Liveness::Live,
        ),
        (
            "metrics_endpoint_routing.group.fedcba9876543210.prefixes",
            STRINGS,
            Liveness::Live,
        ),
        ("multi_region_failover.enabled", ParamType::Bool, Liveness::Live),
        (
            "multi_region_failover.metric_mirroring.enabled",
            ParamType::Bool,
            Liveness::Live,
        ),
        (
            "multi_region_failover.metric_mirroring.allowlist",
            STRINGS,
            Liveness::Live,
        ),
        ("autoscaling_failover.metrics", STRINGS, Liveness::Live),
        ("autoscaling_failover.enabled", ParamType::Bool, Liveness::Live),
        ("host.tags", STRINGS, Liveness::Startup),
        ("host.tags_deadline", ParamType::Duration, Liveness::Startup),
    ];

    let mut catalog = MemoryCatalog::new();
    for (id, accepts, digit, recipients) in outputs {
        catalog = catalog.with_output(
            OutputId::new(id).unwrap(),
            CatalogOutput {
                accepts,
                fingerprint: hash(digit),
                recipients: recipients.iter().map(|r| RecipientId::new(*r).unwrap()).collect(),
            },
        );
    }
    for (name, ty, liveness) in bindings {
        // As the host classifies them: the host's tags and whether failover is configured are facts, and the two
        // failover switches signals; everything else is policy.
        let class = match name {
            "host.tags" | "host.tags_deadline" | "multi_region_failover.enabled" => BindingClass::Fact,
            "multi_region_failover.metric_mirroring.enabled" | "autoscaling_failover.enabled" => BindingClass::Signal,
            _ => BindingClass::Policy,
        };
        catalog = catalog.with_classified_binding(BindingName::new(name).unwrap(), ty, liveness, class);
    }
    catalog
}
