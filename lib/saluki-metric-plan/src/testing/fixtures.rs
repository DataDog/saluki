//! Hand-written default-program fixtures and the catalog they validate against.

use crate::{
    canonical::{PlanHash, MAX_SAFE_INTEGER},
    ir::{BindingName, InputName, OutputId, ParamSpec, ParamType, RecipientId},
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
/// lowering uses with their types and liveness, and the `post_enrich` scope.
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
            Liveness::Startup,
        ),
        (
            "metrics_endpoint_routing.group.0123456789abcdef.prefixes",
            STRINGS,
            Liveness::Startup,
        ),
        (
            "metrics_endpoint_routing.group.fedcba9876543210.names",
            STRINGS,
            Liveness::Startup,
        ),
        (
            "metrics_endpoint_routing.group.fedcba9876543210.prefixes",
            STRINGS,
            Liveness::Startup,
        ),
        ("multi_region_failover.enabled", ParamType::Bool, Liveness::Startup),
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
        ("autoscaling_failover.metrics", STRINGS, Liveness::Startup),
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
        catalog = catalog.with_binding(BindingName::new(name).unwrap(), ty, liveness);
    }
    catalog.with_scope_input(
        "post_enrich".parse().unwrap(),
        InputName::new("metrics").unwrap(),
        PortType::ANY,
    )
}
