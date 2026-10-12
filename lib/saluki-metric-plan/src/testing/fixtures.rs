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
    validate::{CatalogOutput, Liveness, MemoryCatalog},
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
                legacy_fingerprint: None,
            },
        )
    })
}

/// Names the region host of a slot of a default-program fixture's frame, as the host names them.
///
/// The slot holding the DogStatsD mapper is `dsd_pre_aggregation`, the slot holding host enrichment is
/// `metrics_enrich`, and the isolation slot holding DogStatsD aggregation is `dsd_agg`. Any other slot, one that R1's
/// cost model cut from a pipeline, is named after its first node, without the `_in` of a merge that starts it, as
/// `<node>_region`.
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
    } else if has("metrics_enrich") {
        "metrics_enrich".to_string()
    } else {
        let first = nodes.first().map_or("slot", |node| node.local().as_str());
        format!("{}_region", first.strip_suffix("_in").unwrap_or(first))
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

/// Returns the catalog the host would declare for the default program: its outputs, the configuration bindings that
/// lowering uses with their types and liveness, and the `dogstatsd_pre_aggregation` and `post_enrich` scopes.
///
/// Binding names, types, and liveness are those of the host's binding registry, whose tests check that they agree.
/// Endpoint routing group keys are placeholders.
///
/// Output fingerprints are placeholder hashes whose digits are all the same: `1` for `dsd_client_telemetry` through
/// `6` for `af_metrics_encode`, in the order the fixtures declare the outputs.
pub fn default_program_catalog() -> MemoryCatalog {
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
    ];

    let mut catalog = MemoryCatalog::new();
    for (id, accepts, digit, recipients) in outputs {
        catalog = catalog.with_output(
            OutputId::new(id).unwrap(),
            CatalogOutput {
                accepts,
                fingerprint: hash(digit),
                recipients: recipients.iter().map(|r| RecipientId::new(*r).unwrap()).collect(),
                legacy_fingerprint: None,
            },
        );
    }
    for (name, ty, liveness) in bindings {
        catalog = catalog.with_binding(BindingName::new(name).unwrap(), ty, liveness);
    }
    catalog
        .with_scope_input(
            "dogstatsd_pre_aggregation".parse().unwrap(),
            InputName::new("metrics").unwrap(),
            PortType::ANY,
        )
        .with_scope_input(
            "post_enrich".parse().unwrap(),
            InputName::new("metrics").unwrap(),
            PortType::ANY,
        )
}
