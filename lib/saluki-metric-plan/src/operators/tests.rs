use std::sync::{Arc, Mutex};

use saluki_core::{
    accounting::ComponentRegistry,
    components::{ComponentContext, ComponentType},
    data_model::event::{
        metric::{context::Context, Metric},
        Event,
    },
    execution::{Budget, Kernel, PortIndex, PortOutputs, Progress, Turn, TurnCounters},
    support::SubsystemIdentifier,
    topology::{ComponentId, EventsBuffer},
};
use saluki_metric_kernels::params::{
    HistogramAggregate, HistogramParams, MapperProfile, MatchType, MetricMapping, NamespacePrefixParams,
    TagFilterAction, TagFilterRule, TagValueMismatch, TagValueRule,
};
use serde_json::{json, Value};

use super::*;
use crate::{
    model::{Field, FieldSet, Kind, KindSet, PortType, TimestampForm},
    operators::contract::LossClass,
};

/// An operator, parameters, and the path and message fragment of each error they must cause.
type ParamCase = (&'static str, Value, Vec<(&'static str, &'static str)>);

/// Every operator in the implementation design's operator table.
const DESIGN_OPERATORS: &[&str] = &[
    "source",
    "dogstatsd_mapper",
    "namespace_prefix",
    "timestamp_split",
    "tag_projection",
    "aggregate",
    "host_enrichment",
    "host_tags",
    "opaque_host",
    "split",
    "tee",
    "merge",
    "emit",
    "drop",
];

/// The operators whose kernels have been extracted from native components: every synchronous native operator except
/// the routing operators, which the host runs itself.
const OPERATORS_WITH_KERNELS: &[&str] = &[
    "dogstatsd_mapper",
    "namespace_prefix",
    "timestamp_split",
    "tag_projection",
    "aggregate",
    "host_enrichment",
    "host_tags",
];

fn params(value: Value) -> Params {
    serde_json::from_value(value).unwrap()
}

#[track_caller]
fn configure(op: &str, value: Value) -> ConfiguredOperator {
    match OperatorRegistry::native().configure(op, &params(value.clone())) {
        Ok(configured) => configured,
        Err(error) => panic!("{} {}: {} {:?}", op, value, error, error),
    }
}

/// The native aggregation defaults.
fn aggregate_params() -> Value {
    json!({
        "window": "10s",
        "flush_interval": "15s",
        "context_limit": 1000000,
        "counter_expiry": "5m",
        "flush_open_windows": false,
        "histogram_aggregates": ["max", "median", "avg", "count"],
        "histogram_percentiles": [95],
        "copy_to_distribution": false,
        "copy_to_distribution_prefix": "",
    })
}

fn mapper_params() -> Value {
    json!({
        "profiles": [{"name": "checkout", "prefix": "raw.", "mappings": [
            {"match": "raw.requests.*", "match_type": "wildcard", "name": "mapped.requests",
             "tags": [{"name": "route", "value": "$1"}]},
            {"match": "raw\\.(\\w+)\\.latency", "match_type": "regex", "name": "latency"}
        ]}],
        "cache_size": 1000,
        "string_interner_bytes": 65536,
    })
}

fn tag_projection_params() -> Value {
    json!({
        "rules": [{"metric_name": "checkout.latency", "action": "exclude", "tags": ["user_id"]}],
        "value_rules": [{"metric_prefix": "checkout.", "tag_name": "env", "values": ["prod"], "on_miss": "replace",
                         "replacement": "other"}],
        "cache_capacity": 1000,
    })
}

/// Valid parameters for every operator, covering each shape that changes its ports or contract.
fn samples() -> Vec<(&'static str, Value)> {
    vec![
        ("source", json!({"binding": "dogstatsd", "timestamps": "mixed"})),
        ("source", json!({"binding": "dogstatsd", "timestamps": "untimestamped"})),
        ("source", json!({"binding": "otlp"})),
        ("source", json!({"binding": "checks"})),
        ("source", json!({"binding": "liveness"})),
        ("source", json!({"binding": "trace_sampler"})),
        ("dogstatsd_mapper", mapper_params()),
        (
            "namespace_prefix",
            json!({"namespace": "tenant", "exclusions": ["internal."]}),
        ),
        ("timestamp_split", json!({"window": "10s"})),
        ("tag_projection", tag_projection_params()),
        ("aggregate", aggregate_params()),
        ("host_enrichment", json!({})),
        ("host_tags", json!({"tags": ["zone:a"], "deadline": "1700000300s"})),
        (
            "opaque_host",
            json!({"component": "mrf_metrics_gateway", "input": {"kinds": "all", "timestamps": "mixed"},
                   "outputs": {"out": {"kinds": "all", "timestamps": "mixed"}}}),
        ),
        (
            "opaque_host",
            json!({"component": "dsd_no_agg_split", "input": {"kinds": "all", "timestamps": "mixed"},
                   "outputs": {"out": {"kinds": "all", "timestamps": "untimestamped"},
                               "passthrough": {"kinds": "all", "timestamps": "timestamped"}}}),
        ),
        ("split", json!({"predicate": {"name": {"eq": "a"}}})),
        ("tee", json!({"ports": 1})),
        ("tee", json!({"ports": 3})),
        ("merge", json!({"inputs": 1})),
        ("merge", json!({"inputs": 4})),
        ("emit", json!({"output": "dd_metrics_encode"})),
        ("drop", json!({})),
    ]
}

fn port_names(ports: &[impl AsPortName]) -> Vec<&str> {
    ports.iter().map(AsPortName::port_name).collect()
}

trait AsPortName {
    fn port_name(&self) -> &str;
}

impl AsPortName for InputPort {
    fn port_name(&self) -> &str {
        self.name.as_str()
    }
}

impl AsPortName for OutputPort {
    fn port_name(&self) -> &str {
        self.name.as_str()
    }
}

fn port_type(kinds: &[Kind], timestamps: TimestampForm) -> PortType {
    PortType {
        kinds: KindSet::of(kinds),
        timestamps,
    }
}

fn fields(fields: &[Field]) -> FieldSet {
    FieldSet::of(fields)
}

#[test]
fn registry_contains_exactly_the_operators_of_the_design() {
    let registry = OperatorRegistry::native();
    let registered = registry.iter().map(|def| def.name()).collect::<Vec<_>>();
    assert_eq!(registered, DESIGN_OPERATORS);
    for name in DESIGN_OPERATORS {
        let def = registry.get(name).unwrap();
        assert_eq!(def.name(), *name);
        assert!(def.summary().ends_with('.'), "{} has a one-sentence summary", name);
        assert!(crate::ir::is_identifier(name));
    }
    assert!(registry.get("regex_filter").is_none());

    let error = registry.configure("regex_filter", &Params::new()).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("Unknown operator `regex_filter`; known operators are: source,"),
        "{}",
        error
    );
}

/// Checks one parameter type of an operator's schema, recursively.
fn check_param_type(op: &str, path: &str, ty: &crate::ir::ParamType, inside_bindable: bool) {
    use crate::{canonical::MAX_SAFE_INTEGER, ir::ParamType};

    match ty {
        ParamType::Integer { min, max } => {
            assert!(min <= max, "{} {}: empty range", op, path);
            assert!(
                *min >= -MAX_SAFE_INTEGER && *max <= MAX_SAFE_INTEGER,
                "{} {}: integer bounds must lie within the canonical integer range",
                op,
                path
            );
        }
        ParamType::Enum(choices) => assert!(!choices.is_empty(), "{} {}: an enumeration needs choices", op, path),
        ParamType::List(element) | ParamType::Set(element) | ParamType::StringMap(element) => {
            check_param_type(op, &format!("{}[]", path), element, inside_bindable)
        }
        ParamType::Bindable(inner) => {
            assert!(!inside_bindable, "{} {}: bindings do not nest", op, path);
            check_param_type(op, path, inner, true);
        }
        ParamType::Record(members) => check_param_specs(op, path, members, inside_bindable),
        _ => {}
    }
}

fn check_param_specs(op: &str, path: &str, specs: &[ParamSpec], inside_bindable: bool) {
    let names = specs
        .iter()
        .map(|spec| spec.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), specs.len(), "{} {}: names are unique", op, path);
    for spec in specs {
        assert!(crate::ir::is_identifier(spec.name), "{} {}.{}", op, path, spec.name);
        // `config` marks configuration bindings everywhere in the IR.
        assert_ne!(spec.name, "config", "{} {}", op, path);
        check_param_type(op, &format!("{}.{}", path, spec.name), &spec.ty, inside_bindable);
    }
}

#[test]
fn parameter_schemas_are_well_formed() {
    for def in OperatorRegistry::native().iter() {
        check_param_specs(def.name(), "params", def.params(), false);
    }
}

#[test]
fn every_operator_contract_satisfies_the_contract_rules() {
    let samples = samples();
    for name in DESIGN_OPERATORS {
        assert!(samples.iter().any(|(op, _)| op == name), "{} has a sample", name);
    }
    for (op, value) in samples {
        let configured = configure(op, value.clone());
        let contract = configured.contract();
        let ports = configured.ports();
        assert_eq!(
            contract.violations(ports.inputs.len(), ports.outputs.len()),
            Vec::<&str>::new(),
            "{} {}",
            op,
            value
        );

        // The registry invariant the implementation design names: a filter writes nothing.
        if contract.cardinality == Cardinality::Filter {
            assert!(contract.writes.any_kind().is_empty(), "{} is a filter that writes", op);
        }
        // Undeclared expansion is only for opaque hosts, and only opaque hosts keep undescribed state.
        if contract.state == StateContract::Opaque || contract.cardinality == (Cardinality::Expand { max: None }) {
            assert_eq!(contract.boundary, Boundary::OpaqueHost, "{}", op);
        }
        // No native operator reads signals yet.
        assert!(!contract.is_dynamic(), "{}", op);
        // Port names are unique per direction.
        let inputs = port_names(&ports.inputs);
        let outputs = port_names(&ports.outputs);
        assert_eq!(
            inputs.iter().collect::<std::collections::BTreeSet<_>>().len(),
            inputs.len(),
            "{}",
            op
        );
        assert_eq!(
            outputs.iter().collect::<std::collections::BTreeSet<_>>().len(),
            outputs.len(),
            "{}",
            op
        );
        // Only operators whose kernels have been extracted have one.
        assert_eq!(
            configured.kernel().is_some(),
            OPERATORS_WITH_KERNELS.contains(&op),
            "{}",
            op
        );
    }
}

#[test]
fn stateless_operators_always_retain_their_state_contract() {
    let samples = samples();
    for (op, old) in &samples {
        for (other, new) in &samples {
            if op != other {
                continue;
            }
            let old = configure(op, old.clone());
            let new = configure(op, new.clone());
            if old.contract().state == StateContract::None {
                assert_eq!(old.transition_to(&new), TransitionClass::Retain, "{}", op);
            }
        }
    }
}

#[test]
fn stateful_transitions_depend_on_the_state_contract() {
    let aggregate = configure("aggregate", aggregate_params());
    let with = |member: &str, value: Value| {
        let mut changed = aggregate_params();
        changed[member] = value;
        configure("aggregate", changed)
    };

    // (changed parameter, new value, expected class)
    let cases = [
        ("window", json!("30s"), TransitionClass::Cut),
        (
            "window",
            json!({"config": "dogstatsd.aggregation.window_duration_seconds"}),
            TransitionClass::Cut,
        ),
        ("window", json!("10s"), TransitionClass::Retain),
        ("flush_interval", json!("5s"), TransitionClass::Retain),
        ("context_limit", json!(10), TransitionClass::Retain),
        ("histogram_aggregates", json!(["max"]), TransitionClass::Retain),
    ];
    for (member, value, expected) in cases {
        assert_eq!(
            aggregate.transition_to(&with(member, value.clone())),
            expected,
            "{} {}",
            member,
            value
        );
    }

    // A node whose operator changes keeps no state: replacing a state owner is refused, replacing anything stateless is
    // not.
    let split = configure("split", json!({"predicate": {"const": true}}));
    let tee = configure("tee", json!({"ports": 2}));
    assert_eq!(aggregate.transition_to(&split), TransitionClass::Reject);
    assert_eq!(split.transition_to(&aggregate), TransitionClass::Reject);
    assert_eq!(split.transition_to(&tee), TransitionClass::Retain);

    // An opaque host's state is its own: any change to it is refused.
    let opaque = |component: &str| {
        configure(
            "opaque_host",
            json!({"component": component, "input": {"kinds": "all", "timestamps": "mixed"},
                   "outputs": {"out": {"kinds": "all", "timestamps": "mixed"}}}),
        )
    };
    assert_eq!(opaque("a").transition_to(&opaque("a")), TransitionClass::Retain);
    assert_eq!(opaque("a").transition_to(&opaque("b")), TransitionClass::Reject);
}

/// A stateful test operator whose state contract buckets by its `width` parameter, and whose own transition refuses a
/// change of its `strict` parameter.
struct Bucketed;

const BUCKETED_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("width", crate::ir::ParamType::Duration),
    ParamSpec::required("strict", crate::ir::ParamType::Bool),
];

impl OperatorDef for Bucketed {
    fn name(&self) -> &'static str {
        "test_bucketed"
    }

    fn summary(&self) -> &'static str {
        "Keeps keyed state in buckets of its width."
    }

    fn params(&self) -> &'static [ParamSpec] {
        BUCKETED_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        EffectContract {
            state: StateContract::Keyed(KeyedState {
                key: ReducerKey::Context,
                reducer: Reducer::NativeAggregation,
                time: StateTime::Buckets {
                    width: params.duration("width"),
                },
                admission: crate::extension::AdmissionPolicy::FirstCome,
                emission_filter: false,
            }),
            ..EffectContract::passthrough()
        }
    }

    fn transition(&self, old: &DecodedParams, new: &DecodedParams) -> TransitionClass {
        if old.bool("strict") == new.bool("strict") {
            TransitionClass::Retain
        } else {
            TransitionClass::Reject
        }
    }
}

#[test]
fn transitions_follow_the_state_contract_and_operators_only_make_them_stricter() {
    let registry = OperatorRegistry::with_operators(&[&Bucketed]);
    let bucketed = |width: &str, strict: bool| {
        registry
            .configure("test_bucketed", &params(json!({"width": width, "strict": strict})))
            .unwrap()
    };

    // (old, new, expected class)
    let cases = [
        (bucketed("10s", false), bucketed("10s", false), TransitionClass::Retain),
        // The operator's own transition allows a width change, but the time contract differs, so state is cut.
        (bucketed("10s", false), bucketed("20s", false), TransitionClass::Cut),
        // The contract allows sharing the state, but the operator refuses.
        (bucketed("10s", false), bucketed("10s", true), TransitionClass::Reject),
        // The stricter of the two decides.
        (bucketed("10s", false), bucketed("20s", true), TransitionClass::Reject),
    ];
    for (old, new, expected) in cases {
        assert_eq!(
            old.transition_to(&new),
            expected,
            "{:?} -> {:?}",
            old.params(),
            new.params()
        );
    }
}

#[test]
fn ports_follow_from_parameters() {
    // (operator, parameters, input ports, output ports)
    let cases: Vec<(&str, Value, Vec<&str>, Vec<&str>)> = vec![
        ("source", json!({"binding": "otlp"}), vec![], vec!["metrics"]),
        ("dogstatsd_mapper", mapper_params(), vec!["in"], vec!["out"]),
        (
            "timestamp_split",
            json!({"window": "10s"}),
            vec!["in"],
            vec!["default", "passthrough"],
        ),
        (
            "split",
            json!({"predicate": {"const": true}}),
            vec!["in"],
            vec!["true", "false"],
        ),
        ("tee", json!({"ports": 3}), vec!["in"], vec!["0", "1", "2"]),
        ("merge", json!({"inputs": 4}), vec!["0", "1", "2", "3"], vec!["out"]),
        ("emit", json!({"output": "a"}), vec!["in"], vec![]),
        ("drop", json!({}), vec!["in"], vec![]),
        (
            "opaque_host",
            json!({"component": "dsd_no_agg_split", "input": {"kinds": "all", "timestamps": "mixed"},
                   "outputs": {"out": {"kinds": "all", "timestamps": "untimestamped"},
                               "passthrough": {"kinds": "all", "timestamps": "timestamped"}}}),
            vec!["in"],
            vec!["out", "passthrough"],
        ),
    ];
    for (op, value, inputs, outputs) in cases {
        let configured = configure(op, value);
        assert_eq!(port_names(&configured.ports().inputs), inputs, "{}", op);
        assert_eq!(port_names(&configured.ports().outputs), outputs, "{}", op);
    }

    let tee = configure("tee", json!({"ports": 10000}));
    assert_eq!(tee.ports().outputs.len(), 10000);
    assert_eq!(tee.ports().outputs[9999].name.as_str(), "9999");
}

#[test]
fn output_types_follow_from_input_types() {
    use Kind::{Counter, Distribution, Gauge, Histogram, Rate, Set};
    use TimestampForm::{Mixed, Timestamped, Untimestamped};

    let dogstatsd = [Counter, Gauge, Set, Histogram, Distribution];
    let gateway = json!({"predicate": {"all": [
        {"kind_in": ["counter", "rate", "gauge", "set"]},
        {"name": {"in": ["requests"]}}
    ]}});
    let opaque = json!({"component": "dsd_no_agg_split", "input": {"kinds": "all", "timestamps": "mixed"},
                        "outputs": {"out": {"kinds": ["gauge"], "timestamps": "untimestamped"}}});

    // (operator, parameters, input types, expected output types)
    let cases: Vec<(&str, Value, Vec<PortType>, Vec<PortType>)> = vec![
        (
            "source",
            json!({"binding": "dogstatsd", "timestamps": "mixed"}),
            vec![],
            vec![port_type(&dogstatsd, Mixed)],
        ),
        (
            "source",
            json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
            vec![],
            vec![port_type(&dogstatsd, Untimestamped)],
        ),
        (
            "source",
            json!({"binding": "otlp"}),
            vec![],
            vec![port_type(&[Counter, Rate, Gauge, Distribution], Mixed)],
        ),
        (
            "source",
            json!({"binding": "checks"}),
            vec![],
            vec![port_type(&[Counter, Rate, Gauge, Histogram], Mixed)],
        ),
        (
            "source",
            json!({"binding": "liveness"}),
            vec![],
            vec![port_type(&[Gauge], Timestamped)],
        ),
        (
            "source",
            json!({"binding": "trace_sampler"}),
            vec![],
            vec![port_type(&[Counter, Gauge], Untimestamped)],
        ),
        // Untimestamped points take the default port and timestamped ones the passthrough port, where counters become
        // rates.
        (
            "timestamp_split",
            json!({"window": "10s"}),
            vec![port_type(&dogstatsd, Mixed)],
            vec![
                port_type(&dogstatsd, Untimestamped),
                port_type(&[Rate, Gauge, Set, Histogram, Distribution], Timestamped),
            ],
        ),
        (
            "timestamp_split",
            json!({"window": "10s"}),
            vec![port_type(&dogstatsd, Untimestamped)],
            vec![port_type(&dogstatsd, Untimestamped), port_type(&[], Timestamped)],
        ),
        // Aggregation emits rates for counters, series for histograms, and timestamps every point.
        (
            "aggregate",
            aggregate_params(),
            vec![port_type(&dogstatsd, Untimestamped)],
            vec![port_type(&[Rate, Gauge, Set, Distribution], Timestamped)],
        ),
        (
            "aggregate",
            aggregate_params(),
            vec![port_type(&[Counter], Untimestamped)],
            vec![port_type(&[Rate], Timestamped)],
        ),
        (
            "aggregate",
            aggregate_params(),
            vec![port_type(&[Histogram], Mixed)],
            vec![port_type(&[Rate, Gauge, Distribution], Timestamped)],
        ),
        // A split narrows each port to the kinds for which its predicate can take that port's value.
        (
            "split",
            gateway.clone(),
            vec![PortType::ANY],
            vec![port_type(&[Counter, Rate, Gauge, Set], Mixed), PortType::ANY],
        ),
        (
            "split",
            json!({"predicate": {"kind_in": ["counter"]}}),
            vec![port_type(&[Rate, Gauge], Timestamped)],
            vec![port_type(&[], Timestamped), port_type(&[Rate, Gauge], Timestamped)],
        ),
        (
            "merge",
            json!({"inputs": 2}),
            vec![port_type(&[Rate], Timestamped), port_type(&[Gauge], Untimestamped)],
            vec![port_type(&[Rate, Gauge], Mixed)],
        ),
        (
            "tee",
            json!({"ports": 2}),
            vec![port_type(&[Gauge], Mixed)],
            vec![port_type(&[Gauge], Mixed), port_type(&[Gauge], Mixed)],
        ),
        (
            "tag_projection",
            tag_projection_params(),
            vec![port_type(&dogstatsd, Untimestamped)],
            vec![port_type(&dogstatsd, Untimestamped)],
        ),
        // An opaque host's outputs have the declared types, whatever it receives.
        (
            "opaque_host",
            opaque,
            vec![port_type(&[Counter], Mixed)],
            vec![port_type(&[Gauge], Untimestamped)],
        ),
        ("emit", json!({"output": "a"}), vec![PortType::ANY], vec![]),
    ];
    for (op, value, inputs, expected) in cases {
        let configured = configure(op, value.clone());
        assert_eq!(
            configured.output_types(&inputs),
            expected,
            "{} {} {:?}",
            op,
            value,
            inputs
        );
    }

    // Accepted inputs: everything, except where an opaque host declares otherwise.
    let opaque_gauges = configure(
        "opaque_host",
        json!({"component": "x", "input": {"kinds": ["gauge"], "timestamps": "mixed"},
               "outputs": {"out": {"kinds": ["gauge"], "timestamps": "mixed"}}}),
    );
    assert_eq!(opaque_gauges.ports().inputs[0].accepts, port_type(&[Gauge], Mixed));
    for (op, value) in samples() {
        let configured = configure(op, value);
        if op != "opaque_host" {
            assert!(
                configured
                    .ports()
                    .inputs
                    .iter()
                    .all(|port| port.accepts == PortType::ANY),
                "{}",
                op
            );
        }
    }
}

/// The contracts reflect native behavior, as documented for each native component.
#[test]
fn contracts_reflect_native_behavior() {
    use Kind::{Counter, Distribution, Gauge, Histogram, Rate, Set};

    // The mapper matches the raw name and rewrites the name and tags; distinct names can map to one.
    let mapper = configure("dogstatsd_mapper", mapper_params());
    let contract = mapper.contract();
    assert_eq!(contract.reads.any_kind(), fields(&[Field::Name]));
    for kind in Kind::ALL {
        assert_eq!(*contract.writes.get(kind), fields(&[Field::Name, Field::Tags]));
        assert_eq!(*contract.identity.get(kind), Identity::Quotient);
    }
    assert_eq!(contract.cardinality, Cardinality::Map);
    assert_eq!(contract.loss, Loss::Lossless);
    assert_eq!(contract.boundary, Boundary::Synchronous);

    // Namespace prefixing reads and writes only the name, and is not injective.
    let prefix = configure("namespace_prefix", json!({"namespace": "tenant", "exclusions": []}));
    assert_eq!(prefix.contract().writes.any_kind(), fields(&[Field::Name]));
    assert_eq!(*prefix.contract().identity.get(Gauge), Identity::Quotient);
    assert_eq!(prefix.contract().loss, Loss::Lossless);

    // The timestamp split's ports are point-disjoint; it changes the kind of counters only, and a mixed metric becomes
    // at most two records.
    let split = configure("timestamp_split", json!({"window": "10s"}));
    let contract = split.contract();
    assert_eq!(contract.ports, PortRelation::PointDisjoint);
    assert_eq!(contract.cardinality, Cardinality::Expand { max: Some(2) });
    assert_eq!(*contract.writes.get(Counter), fields(&[Field::Kind]));
    for kind in [Rate, Gauge, Set, Histogram, Distribution] {
        assert_eq!(*contract.writes.get(kind), FieldSet::EMPTY, "{}", kind);
    }
    let passthrough = &split.ports().outputs[1];
    assert_eq!(passthrough.name.as_str(), "passthrough");
    assert_eq!(passthrough.mapping.kinds.get(Counter), KindSet::of(&[Rate]));
    assert_eq!(passthrough.mapping.kinds.get(Rate), KindSet::of(&[Rate]));
    assert_eq!(
        split.ports().outputs[0].mapping.kinds.get(Counter),
        KindSet::of(&[Counter])
    );
    assert_eq!(contract.identity.get(Counter), &Identity::Preserving);

    // Tag projection rewrites tags and origin tags of counters and distributions only, and is policy-lossy.
    let tags = configure("tag_projection", tag_projection_params());
    let contract = tags.contract();
    for kind in [Counter, Distribution] {
        assert_eq!(
            *contract.writes.get(kind),
            fields(&[Field::Tags, Field::OriginTags]),
            "{}",
            kind
        );
        assert_eq!(*contract.identity.get(kind), Identity::Quotient, "{}", kind);
        assert!(contract.reads.get(kind).contains(Field::Name));
    }
    for kind in [Rate, Gauge, Set, Histogram] {
        assert_eq!(*contract.writes.get(kind), FieldSet::EMPTY, "{}", kind);
        assert_eq!(*contract.identity.get(kind), Identity::Preserving, "{}", kind);
        assert_eq!(*contract.reads.get(kind), fields(&[Field::Kind]), "{}", kind);
    }
    assert!(contract.loss.has_class(LossClass::Policy));

    // Aggregation writes the kind of counters (they leave as rates), and the name, kind, and metadata of histograms
    // (they expand into series). Every kind's points are rewritten into buckets.
    let aggregate = configure("aggregate", aggregate_params());
    let contract = aggregate.contract();
    assert_eq!(*contract.writes.get(Counter), fields(&[Field::Kind, Field::Points]));
    assert_eq!(
        *contract.writes.get(Histogram),
        fields(&[Field::Name, Field::Kind, Field::Metadata, Field::Points])
    );
    for kind in [Rate, Gauge, Set, Distribution] {
        assert_eq!(*contract.writes.get(kind), fields(&[Field::Points]), "{}", kind);
        assert_eq!(*contract.identity.get(kind), Identity::Preserving, "{}", kind);
    }
    for kind in Kind::ALL {
        assert!(!contract.writes.get(kind).contains(Field::Host), "{}", kind);
        assert!(!contract.writes.get(kind).contains(Field::Tags), "{}", kind);
        assert!(!contract.writes.get(kind).contains(Field::OriginTags), "{}", kind);
    }
    assert_eq!(*contract.identity.get(Histogram), Identity::Opaque);
    // Four statistics by default (max, median, avg, count) plus the 95th percentile.
    assert_eq!(contract.cardinality, Cardinality::Expand { max: Some(5) });
    assert_eq!(contract.boundary, Boundary::Isolated);
    assert_eq!(contract.order, OrderContract::Dependent);
    assert_eq!(
        contract.state,
        StateContract::Keyed(KeyedState {
            key: ReducerKey::Context,
            reducer: Reducer::NativeAggregation,
            time: StateTime::Buckets {
                width: Some(std::time::Duration::from_secs(10)),
            },
            admission: crate::extension::AdmissionPolicy::FirstCome,
            emission_filter: true,
        })
    );
    // The context limit loses by capacity, open buckets are discarded at shutdown, and the native merge loses by
    // policy: a bucket keeps only the latest untimestamped gauge, and a kind or rate-interval conflict replaces the
    // context's values.
    assert!(contract.loss.has_class(LossClass::Capacity));
    assert!(contract.loss.has_class(LossClass::Shutdown));
    assert!(contract.loss.has_class(LossClass::Policy));
    assert!(!contract.loss.has_class(LossClass::Invalid));
    assert!(!contract.time.reads_admission_time);

    // Copying histograms to distributions adds one series.
    let mut copying = aggregate_params();
    copying["copy_to_distribution"] = json!(true);
    assert_eq!(
        configure("aggregate", copying).contract().cardinality,
        Cardinality::Expand { max: Some(6) }
    );

    // Host enrichment leaves metrics unchanged; host tags add tags until a deadline compared with the admission time.
    let enrichment = configure("host_enrichment", json!({}));
    assert_eq!(enrichment.contract(), &EffectContract::passthrough());
    let host_tags = configure("host_tags", json!({"tags": ["zone:a"], "deadline": "1700000300s"}));
    let contract = host_tags.contract();
    assert_eq!(contract.writes.any_kind(), fields(&[Field::Tags]));
    assert_eq!(contract.reads.any_kind(), FieldSet::EMPTY);
    assert!(contract.time.reads_admission_time);
    assert_eq!(*contract.identity.get(Gauge), Identity::Quotient);
    assert_eq!(contract.loss, Loss::Lossless);

    // Sources are I/O hosts; opaque hosts read and write everything and may copy.
    let source = configure("source", json!({"binding": "otlp"}));
    assert_eq!(source.contract().boundary, Boundary::IoHost);
    assert_eq!(source.contract().cardinality, Cardinality::Source);
    let opaque = configure(
        "opaque_host",
        json!({"component": "af_metrics_gateway", "input": {"kinds": "all", "timestamps": "mixed"},
               "outputs": {"out": {"kinds": "all", "timestamps": "mixed"}}}),
    );
    let contract = opaque.contract();
    assert_eq!(contract.reads.any_kind(), FieldSet::ALL);
    assert_eq!(contract.writes.any_kind(), FieldSet::ALL);
    assert_eq!(contract.ports, PortRelation::Copies);
    assert_eq!(contract.state, StateContract::Opaque);
    assert!(contract.boundary.is_fence());

    // Routing: a split reads what its predicate reads and sends each record to exactly one port; a tee copies; drop
    // is policy loss; emit is a lossless sink.
    let routing = configure(
        "split",
        json!({"predicate": {"any": [{"intake_name": {"eq": "a"}}, {"has_tag": "b"}]}}),
    );
    assert_eq!(routing.contract().reads.any_kind(), fields(&[Field::Name, Field::Tags]));
    assert_eq!(routing.contract().writes.any_kind(), FieldSet::EMPTY);
    assert_eq!(routing.contract().ports, PortRelation::PointDisjoint);
    assert_eq!(routing.contract().cardinality, Cardinality::Map);
    assert_eq!(
        configure("tee", json!({"ports": 2})).contract().ports,
        PortRelation::Copies
    );
    let drop = configure("drop", json!({}));
    assert_eq!(drop.contract().cardinality, Cardinality::Sink);
    assert!(drop.contract().loss.has_class(LossClass::Policy));
    let emit = configure("emit", json!({"output": "a"}));
    assert_eq!(emit.contract().cardinality, Cardinality::Sink);
    assert_eq!(emit.contract().loss, Loss::Lossless);
}

#[test]
fn invalid_parameters_are_rejected_with_every_problem() {
    // (operator, parameters, expected (path, message fragment) for each error)
    let cases: Vec<ParamCase> = vec![
        ("tee", json!({}), vec![("ports", "missing required parameter")]),
        (
            "tee",
            json!({"ports": 0}),
            vec![("ports", "outside the allowed range 1..=10000")],
        ),
        (
            "merge",
            json!({"inputs": 10001}),
            vec![("inputs", "outside the allowed range")],
        ),
        (
            "split",
            json!({"predicate": {"name": {"equals": "a"}}, "extra": 1}),
            vec![("extra", "unknown parameter"), ("predicate", "invalid predicate")],
        ),
        ("emit", json!({"output": "Bad"}), vec![("output", "Invalid output ID")]),
        (
            "drop",
            json!({"reason": "x"}),
            vec![("reason", "this operator takes no parameters")],
        ),
        (
            "source",
            json!({"binding": "dogstatsd"}),
            vec![("timestamps", "the `dogstatsd` source requires `timestamps`")],
        ),
        (
            "source",
            json!({"binding": "dogstatsd", "timestamps": "timestamped"}),
            vec![("timestamps", "never all timestamped")],
        ),
        (
            "source",
            json!({"binding": "otlp", "timestamps": "mixed"}),
            vec![("timestamps", "the `otlp` source's timestamp form is fixed")],
        ),
        (
            "source",
            json!({"binding": "statsd"}),
            vec![("binding", "is not one of")],
        ),
        (
            "timestamp_split",
            json!({"window": "1500ms"}),
            vec![("window", "must be a whole number of seconds")],
        ),
        (
            "timestamp_split",
            json!({"window": "0s"}),
            vec![("window", "must be at least one second")],
        ),
        (
            "timestamp_split",
            json!({"window": "7s"}),
            vec![(
                "window",
                "must be one of 1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30, 60, 120, 180, or 300 seconds, but is 7 seconds",
            )],
        ),
        (
            "aggregate",
            {
                let mut value = aggregate_params();
                value["window"] = json!("4m");
                value
            },
            vec![(
                "window",
                "but is 240 seconds: aggregation buckets must line up at a common boundary",
            )],
        ),
        (
            "aggregate",
            {
                let mut value = aggregate_params();
                value["flush_interval"] = json!("0s");
                value["counter_expiry"] = json!("90500ms");
                value["histogram_aggregates"] = json!(["max", "count", "max"]);
                value["histogram_percentiles"] = json!([95, 99, 95]);
                value
            },
            vec![
                ("flush_interval", "must be at least one second"),
                ("counter_expiry", "must be a whole number of seconds"),
                ("histogram_aggregates[2]", "\"max\" is listed more than once"),
                ("histogram_percentiles[2]", "95 is listed more than once"),
            ],
        ),
        (
            "aggregate",
            {
                let mut value = aggregate_params();
                value["histogram_percentiles"] = json!([101]);
                value["histogram_aggregates"] = json!(["p50"]);
                value
            },
            vec![
                ("histogram_aggregates[0]", "is not one of"),
                ("histogram_percentiles[0]", "outside the allowed range 0..=100"),
            ],
        ),
        (
            "tag_projection",
            json!({"rules": [], "cache_capacity": 1, "value_rules": [
                {"metric_prefix": "a.", "tag_name": "env", "values": [], "on_miss": "replace"},
                {"metric_prefix": "b.", "tag_name": "env", "values": [], "on_miss": "remove", "replacement": "x"}
            ]}),
            vec![
                ("value_rules[0].replacement", "needs a replacement"),
                ("value_rules[1].replacement", "takes no replacement"),
            ],
        ),
        (
            "tag_projection",
            json!({"rules": [], "cache_capacity": 1, "value_rules": [
                {"metric_prefix": "checkout.", "tag_name": "env", "values": [], "on_miss": "remove"},
                {"metric_prefix": "checkout.api.", "tag_name": "env", "values": [], "on_miss": "remove"}
            ]}),
            vec![("value_rules", "prefixes 'checkout.' and 'checkout.api.' overlap")],
        ),
        (
            "tag_projection",
            json!({"rules": [{"metric_name": "a", "action": "keep", "tags": []}], "value_rules": [],
                   "cache_capacity": 1}),
            vec![("rules[0].action", "is not one of: include, exclude")],
        ),
        (
            "dogstatsd_mapper",
            json!({"profiles": [{"name": "a", "prefix": "a.", "mappings": [{"match": "a.*", "name": "b"}]}],
                   "cache_size": 0, "string_interner_bytes": 0}),
            vec![
                ("profiles[0].mappings[0].match_type", "missing required member"),
                ("string_interner_bytes", "outside the allowed range"),
            ],
        ),
        (
            "dogstatsd_mapper",
            json!({"profiles": [{"name": "a", "prefix": "a.", "mappings": [
                       {"match": "a.**", "match_type": "wildcard", "name": "b"}]}],
                   "cache_size": 0, "string_interner_bytes": 1}),
            vec![(
                "profiles",
                "invalid wildcard match pattern `a.**`, it should not contain consecutive `*`",
            )],
        ),
        (
            "dogstatsd_mapper",
            json!({"profiles": [{"name": "a", "prefix": "a.", "mappings": [
                       {"match": "a.(", "match_type": "regex", "name": "b"}]}],
                   "cache_size": 0, "string_interner_bytes": 1}),
            vec![(
                "profiles",
                "Failed to compile regular expression `^a.($` for `regex` match type: regex parse error",
            )],
        ),
        (
            "dogstatsd_mapper",
            json!({"profiles": [{"name": "", "prefix": "a.", "mappings": []}],
                   "cache_size": 0, "string_interner_bytes": 1}),
            vec![("profiles", "missing profile name")],
        ),
        (
            "opaque_host",
            json!({"component": "_bad", "input": {"kinds": "all", "timestamps": "mixed"}, "outputs": {}}),
            vec![
                ("component", "Invalid native component ID"),
                ("outputs", "at least one output port"),
            ],
        ),
        (
            "opaque_host",
            json!({"component": "x", "input": {"kinds": "all", "timestamps": "mixed"},
                   "outputs": {"Out": {"kinds": "all", "timestamps": "mixed"}}}),
            vec![("outputs.Out", "Invalid port name")],
        ),
        (
            "host_tags",
            json!({"tags": ["a"], "deadline": 1700000300}),
            vec![("deadline", "expected a duration")],
        ),
    ];
    for (op, value, expected) in cases {
        let error = OperatorRegistry::native()
            .configure(op, &params(value.clone()))
            .unwrap_err();
        let ConfigureError::InvalidParams { operator, errors } = &error else {
            panic!("{} {}: {}", op, value, error);
        };
        assert_eq!(*operator, op);
        assert_eq!(errors.len(), expected.len(), "{} {}: {:?}", op, value, errors);
        for (error, (path, fragment)) in errors.iter().zip(&expected) {
            assert_eq!(error.path, *path, "{} {}: {}", op, value, error);
            assert!(error.message.contains(fragment), "{} {}: {}", op, value, error);
        }
    }

    // Values bound to configuration and not yet resolved are not checked.
    let unresolved = json!({"window": {"config": "dogstatsd.aggregation.window_duration_seconds"}});
    assert!(OperatorRegistry::native()
        .configure("timestamp_split", &params(unresolved))
        .is_ok());
}

#[test]
fn memory_bounds_mirror_native_accounting() {
    let bounds = |configured: &ConfiguredOperator| {
        let registry = ComponentRegistry::default();
        configured.specify_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("node")));
        let bounds = registry.as_bounds();
        (bounds.total_minimum_required_bytes(), bounds.total_firm_limit_bytes())
    };

    // The tag projection's cache is accounted as 64 bytes per entry, as the native component does.
    let (minimum, firm) = bounds(&configure("tag_projection", tag_projection_params()));
    assert_eq!(minimum, 0);
    assert_eq!(firm, 1000 * 64);

    // The mapper's interner storage is allocated up front, as is its result cache of 1,000 entries.
    let (minimum, _) = bounds(&configure("dogstatsd_mapper", mapper_params()));
    assert_eq!(
        minimum,
        65536 + 1000 * saluki_metric_kernels::mapper::RESULT_CACHE_ENTRY_BYTES
    );
    let mut uncached = mapper_params();
    uncached["cache_size"] = json!(0);
    let (minimum, _) = bounds(&configure("dogstatsd_mapper", uncached));
    assert_eq!(minimum, 65536);

    // Aggregation's kernel declares what native aggregation does: the kernel itself up front, and, as the state it
    // draws from its host's state budget, its state map and a snapshot of it, per context of its limit, firmly.
    let state_bounds = |configured: &ConfiguredOperator| {
        let registry = ComponentRegistry::default();
        configured.specify_state_bounds(&mut registry.bounds_builder(&SubsystemIdentifier::from_dotted("node")));
        let bounds = registry.as_bounds();
        (bounds.total_minimum_required_bytes(), bounds.total_firm_limit_bytes())
    };
    let aggregate = configure("aggregate", aggregate_params());
    let kernel = size_of::<saluki_metric_kernels::aggregate::AggregateKernel>();
    // The firm limit includes the minimum.
    assert_eq!(bounds(&aggregate), (kernel, kernel));
    assert_eq!(
        state_bounds(&aggregate),
        (
            0,
            1_000_000
                * (saluki_metric_kernels::aggregate::STATE_ENTRY_BYTES
                    + size_of::<saluki_metric_kernels::aggregate::AggregateContextSnapshotEntry>())
        )
    );
    // Stateless operators draw nothing from a state budget.
    assert_eq!(
        state_bounds(&configure("tag_projection", tag_projection_params())),
        (0, 0)
    );
    let mut bound = aggregate_params();
    bound["context_limit"] = json!({"config": "aggregator.context_limit"});
    let bound = configure("aggregate", bound);
    assert_eq!(
        (bounds(&bound), state_bounds(&bound)),
        ((0, 0), (0, 0)),
        "a context limit without a resolved value has no size"
    );
}

#[test]
fn kernel_parameters_round_trip_through_plan_parameters() {
    let profiles = vec![MapperProfile {
        name: "checkout".to_string(),
        prefix: "raw.".to_string(),
        mappings: vec![
            MetricMapping {
                metric_match: "raw.requests.*".to_string(),
                match_type: MatchType::Wildcard,
                name: "mapped.requests".to_string(),
                tags: vec![("route".to_string(), "$1".to_string())],
            },
            MetricMapping {
                metric_match: "raw\\.(\\w+)\\.latency".to_string(),
                match_type: MatchType::Regex,
                name: "latency".to_string(),
                tags: vec![],
            },
        ],
    }];
    let encoded = encode_mapper_profiles(&profiles);
    assert_eq!(encoded, mapper_params()["profiles"]);
    let mapper = configure(
        "dogstatsd_mapper",
        json!({"profiles": {"config": "dogstatsd.mapper.profiles", "resolved": encoded},
               "cache_size": 0, "string_interner_bytes": 1}),
    );
    assert_eq!(
        decode_mapper_profiles(mapper.params().get("profiles").unwrap()),
        Some(profiles)
    );

    let rules = vec![TagFilterRule {
        metric_name: "checkout.latency".to_string(),
        action: TagFilterAction::Exclude,
        tags: vec!["user_id".to_string()],
    }];
    let value_rules = vec![
        TagValueRule {
            metric_prefix: "checkout.".to_string(),
            tag_name: "env".to_string(),
            values: vec!["prod".to_string()],
            on_miss: TagValueMismatch::Replace("other".to_string()),
        },
        TagValueRule {
            metric_prefix: "catalog.".to_string(),
            tag_name: "env".to_string(),
            values: vec![],
            on_miss: TagValueMismatch::Remove,
        },
    ];
    let tags = configure(
        "tag_projection",
        json!({"rules": encode_tag_filter_rules(&rules), "value_rules": encode_tag_value_rules(&value_rules),
               "cache_capacity": 1}),
    );
    assert_eq!(
        decode_tag_filter_rules(tags.params().get("rules").unwrap()),
        Some(rules)
    );
    assert_eq!(
        decode_tag_value_rules(tags.params().get("value_rules").unwrap()),
        Some(value_rules)
    );

    let aggregate = configure("aggregate", aggregate_params());
    assert_eq!(
        histogram_params(aggregate.params()),
        Some(HistogramParams {
            aggregates: vec![
                HistogramAggregate::Max,
                HistogramAggregate::Median,
                HistogramAggregate::Avg,
                HistogramAggregate::Count
            ],
            percentiles: vec![95],
            copy_to_distribution: false,
            copy_to_distribution_prefix: String::new(),
        })
    );

    let prefix = configure(
        "namespace_prefix",
        json!({"namespace": "tenant", "exclusions": ["internal."]}),
    );
    assert_eq!(
        namespace_prefix_params(prefix.params()),
        Some(NamespacePrefixParams {
            namespace: "tenant".to_string(),
            exclusions: vec!["internal.".to_string()],
        })
    );

    // Unresolved bindings have no known value.
    let unresolved = configure(
        "namespace_prefix",
        json!({"namespace": {"config": "dogstatsd.prefix_filter.metric_namespace"}, "exclusions": []}),
    );
    assert_eq!(namespace_prefix_params(unresolved.params()), None);
    assert_eq!(decode_mapper_profiles(&crate::ir::ParamValue::Bool(true)), None);
}

#[test]
fn kernel_specs_build_kernels() {
    struct Passthrough;

    impl Kernel for Passthrough {
        fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
            Progress::Done
        }
    }

    // The factory receives the context the host builds the kernel in: here, the node it reports telemetry for.
    let built_for = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&built_for);
    let spec = KernelSpec::new(move |context| {
        recorded
            .lock()
            .unwrap()
            .push(context.component().component_id().to_string());
        Ok(Box::new(Passthrough) as Box<dyn Kernel>)
    });
    let reserved = KernelBuildContext::detached(&"dsd_tag_filterlist".parse().unwrap());
    let policy = KernelBuildContext::detached(&"policy/my_filter".parse().unwrap());
    let in_topology = KernelBuildContext::new(
        "policy/my_filter".parse().unwrap(),
        ComponentContext::transform(
            &SubsystemIdentifier::from_segments(["topology", "primary"]),
            ComponentId::try_from("metrics_enrich").unwrap(),
        ),
    );
    assert!(spec.build(&reserved).is_ok());
    assert!(spec.clone().build(&policy).is_ok());
    assert!(spec.build(&in_topology).is_ok());
    assert_eq!(
        *built_for.lock().unwrap(),
        vec!["dsd_tag_filterlist", "policy_my_filter", "metrics_enrich"]
    );
    assert_eq!(policy.node().to_string(), "policy/my_filter");
    assert_eq!(policy.component().component_type(), ComponentType::Transform);
    assert_eq!(
        policy.component().identity().to_string(),
        "plan.transforms.policy_my_filter"
    );
    assert_eq!(in_topology.node(), policy.node());

    let failing = KernelSpec::new(|_| {
        Err(KernelBuildError {
            message: "the pattern `(` does not compile".to_string(),
        })
    });
    let error = failing.build(&reserved).err().unwrap();
    assert_eq!(
        error.to_string(),
        "Failed to build the kernel: the pattern `(` does not compile"
    );
    assert_eq!(format!("{:?}", spec), "KernelSpec { .. }");
}

#[test]
fn a_spec_with_a_shared_part_computes_it_once_and_builds_every_instance_around_it() {
    struct Holding;

    impl Kernel for Holding {
        fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
            Progress::Done
        }
    }

    let prepared = Arc::new(Mutex::new(0usize));
    let parts = Arc::new(Mutex::new(Vec::<Arc<String>>::new()));
    let (counted, recorded) = (Arc::clone(&prepared), Arc::clone(&parts));
    let spec = KernelSpec::with_shared(
        move || {
            *counted.lock().unwrap() += 1;
            Ok("compiled".to_string())
        },
        move |part, _context| {
            recorded.lock().unwrap().push(Arc::clone(part));
            Ok(Box::new(Holding) as Box<dyn Kernel>)
        },
    );
    let context = KernelBuildContext::detached(&"dsd_node".parse().unwrap());
    let threads = std::thread::scope(|scope| {
        let builds = (0..4)
            .map(|_| {
                let spec = spec.clone();
                let context = &context;
                scope.spawn(move || spec.build(context).is_ok())
            })
            .collect::<Vec<_>>();
        builds
            .into_iter()
            .map(|build| build.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(threads, [true; 4]);
    assert!(spec.build(&context).is_ok());

    // One computation for five instances, built on several threads at once and from clones of the spec, which all
    // hold the same part.
    assert_eq!(*prepared.lock().unwrap(), 1);
    let parts = parts.lock().unwrap();
    assert_eq!(parts.len(), 5);
    assert!(parts.iter().all(|part| Arc::ptr_eq(part, &parts[0])));

    // A failed computation is not remembered: the next build computes the part again.
    let attempts = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&attempts);
    let flaky = KernelSpec::with_shared(
        move || {
            let mut attempts = counted.lock().unwrap();
            *attempts += 1;
            if *attempts == 1 {
                Err(KernelBuildError {
                    message: "not yet".to_string(),
                })
            } else {
                Ok("compiled".to_string())
            }
        },
        |_part, _context| Ok(Box::new(Holding) as Box<dyn Kernel>),
    );
    assert_eq!(
        flaky.build(&context).err().unwrap().to_string(),
        "Failed to build the kernel: not yet"
    );
    assert!(flaky.build(&context).is_ok());
    assert!(flaky.build(&context).is_ok());
    assert_eq!(*attempts.lock().unwrap(), 2);
}

/// Consumes one batch of `metrics` with `kernel`, which has `ports` output ports, and returns the metrics on each.
fn consume_with(kernel: &mut Box<dyn Kernel>, ports: usize, metrics: Vec<Metric>) -> Vec<Vec<Metric>> {
    let header = saluki_core::execution::BatchHeader::default();
    let mut input = EventsBuffer::default();
    for metric in metrics {
        assert!(input.try_push(Event::Metric(metric)).is_none());
    }
    let mut budget = Budget::unlimited();
    let mut outputs = PortOutputs::new(ports);
    let mut counters = TurnCounters::default();
    {
        let mut turn = Turn::new(
            &header,
            std::time::Instant::now(),
            &mut budget,
            &mut outputs,
            &mut counters,
        );
        assert!(matches!(kernel.consume(&mut turn, &mut input), Progress::Done));
    }
    outputs.complete(input);
    (0..ports)
        .map(|port| {
            outputs
                .drain(PortIndex::new(port as u16))
                .flat_map(|buffer| buffer.into_iter().filter_map(Event::try_into_metric))
                .collect()
        })
        .collect()
}

#[test]
fn instances_of_one_mapper_or_tag_projection_spec_behave_as_instances_of_separate_specs() {
    // Instances built from one spec share its compiled profiles or rules, and keep their caches and memos to
    // themselves: each maps and projects as an instance built from a spec of its own, a second batch, served from
    // its cache or memo, included.
    let runtime = runtime();
    let _context = runtime.enter();
    let projected = || Context::from_static_parts("checkout.latency", &["user_id:1", "env:dev"]);
    let cases = [
        (
            "dogstatsd_mapper",
            mapper_params(),
            vec![
                Metric::counter("raw.requests.checkout", 1.0),
                Metric::counter("raw.other", 1.0),
                Metric::counter("raw.cart.latency", 1.0),
            ],
        ),
        (
            "tag_projection",
            tag_projection_params(),
            vec![Metric::counter(projected(), 1.0), Metric::gauge(projected(), 1.0)],
        ),
    ];
    for (op, params, batch) in cases {
        let configured = configure(op, params);
        let ports = configured.ports().outputs.len();
        let context = KernelBuildContext::detached(&"dsd_node".parse().unwrap());
        let spec = configured.kernel().expect("the operator has a kernel");
        let mut shared = [spec.build(&context).unwrap(), spec.clone().build(&context).unwrap()];
        let mut separate = configured.kernel().unwrap().build(&context).unwrap();
        for round in 0..2 {
            let expected = consume_with(&mut separate, ports, batch.clone());
            for kernel in &mut shared {
                assert_eq!(
                    consume_with(kernel, ports, batch.clone()),
                    expected,
                    "{} round {}",
                    op,
                    round
                );
            }
        }
    }
}

#[test]
fn instances_of_one_mapper_or_tag_projection_spec_share_the_profiles_or_rules_it_compiled_once() {
    // Instances of one spec, built from it or from its clones, are built around the profiles or rules the spec
    // compiled for the first of them: building more compiles nothing more, while a spec of its own compiles its own.
    let runtime = runtime();
    let _context = runtime.enter();
    let context = KernelBuildContext::detached(&"dsd_node".parse().unwrap());
    let cases = [
        (
            "dogstatsd_mapper",
            mapper_params(),
            saluki_metric_kernels::mapper::CompiledProfiles::compilations_on_this_thread as fn() -> u64,
        ),
        (
            "tag_projection",
            tag_projection_params(),
            saluki_metric_kernels::tag_projection::TagProjectionRules::compilations_on_this_thread,
        ),
    ];
    for (op, params, compilations) in cases {
        let spec = configure(op, params.clone())
            .kernel()
            .expect("the operator has a kernel");
        let separate = configure(op, params).kernel().unwrap();
        let before = compilations();
        let instances = [
            spec.build(&context).unwrap(),
            spec.clone().build(&context).unwrap(),
            spec.build(&context).unwrap(),
        ];
        assert_eq!(compilations() - before, 1, "{}: three instances, one compilation", op);
        let other = separate.build(&context).unwrap();
        assert_eq!(compilations() - before, 2, "{}: a spec of its own compiles its own", op);
        drop((instances, other));
    }
}

/// Returns a current-thread Tokio runtime with timers, as hosts build kernels in.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the test runtime builds")
}

#[test]
fn every_kernel_factory_builds_inside_a_current_thread_tokio_context() {
    let runtime = runtime();
    let _context = runtime.enter();
    let mut built = Vec::new();
    for (op, value) in samples() {
        let configured = configure(op, value.clone());
        let Some(spec) = configured.kernel() else { continue };
        for node in ["dsd_node", "policy/node"] {
            let context = KernelBuildContext::detached(&node.parse().unwrap());
            if let Err(error) = spec.build(&context) {
                panic!("{} {} built for {}: {}", op, value, node, error);
            }
        }
        built.push(op);
    }
    built.dedup();
    assert_eq!(built, OPERATORS_WITH_KERNELS);
}

#[test]
fn kernel_factories_need_resolved_parameters() {
    // (operator, parameters with one binding left unresolved, the parameter named)
    let cases = [
        (
            "namespace_prefix",
            json!({"namespace": {"config": "test.namespace"}, "exclusions": ["internal."]}),
            "namespace",
        ),
        (
            "namespace_prefix",
            json!({"namespace": "tenant", "exclusions": {"config": "test.exclusions"}}),
            "exclusions",
        ),
        (
            "timestamp_split",
            json!({"window": {"config": "test.window"}}),
            "window",
        ),
        (
            "tag_projection",
            json!({"rules": {"config": "test.rules"}, "value_rules": [], "cache_capacity": 10}),
            "rules",
        ),
        (
            "tag_projection",
            json!({"rules": [], "value_rules": {"config": "test.value_rules"}, "cache_capacity": 10}),
            "value_rules",
        ),
        (
            "tag_projection",
            json!({"rules": [], "value_rules": [], "cache_capacity": {"config": "test.capacity"}}),
            "cache_capacity",
        ),
    ];
    let runtime = runtime();
    let _context = runtime.enter();
    for (op, value, param) in cases {
        let spec = configure(op, value.clone())
            .kernel()
            .expect("the operator has a kernel");
        let error = spec
            .build(&KernelBuildContext::detached(&"node".parse().unwrap()))
            .err()
            .unwrap_or_else(|| panic!("{} {} built", op, value));
        assert!(
            error
                .message
                .contains(&format!("the `{}` parameter is bound to configuration", param)),
            "{} {}: {}",
            op,
            value,
            error
        );
    }
}

/// Runs one batch through a kernel built from a node's operator, as a host does, and returns the metrics on each
/// port.
fn run_kernel(
    op: &str, params: Value, node: &str, admitted_at: Option<saluki_core::execution::AdmissionTime>,
    metrics: Vec<Metric>,
) -> Vec<Vec<Metric>> {
    let configured = configure(op, params);
    let ports = configured.ports().outputs.len();
    let mut kernel = configured
        .kernel()
        .expect("the operator has a kernel")
        .build(&KernelBuildContext::detached(&node.parse().unwrap()))
        .expect("the kernel builds");
    let header = saluki_core::execution::BatchHeader {
        admitted_at,
        ..Default::default()
    };
    let mut input = EventsBuffer::default();
    for metric in metrics {
        assert!(input.try_push(Event::Metric(metric)).is_none());
    }
    let mut budget = Budget::unlimited();
    let mut outputs = PortOutputs::new(ports);
    let mut counters = TurnCounters::default();
    {
        let mut turn = Turn::new(
            &header,
            std::time::Instant::now(),
            &mut budget,
            &mut outputs,
            &mut counters,
        );
        assert!(matches!(kernel.consume(&mut turn, &mut input), Progress::Done));
    }
    outputs.complete(input);
    (0..ports)
        .map(|port| {
            outputs
                .drain(PortIndex::new(port as u16))
                .flat_map(|buffer| buffer.into_iter().filter_map(Event::try_into_metric))
                .collect()
        })
        .collect()
}

fn names_of(metrics: &[Metric]) -> Vec<&str> {
    metrics.iter().map(|metric| metric.context().name().as_ref()).collect()
}

#[test]
fn kernels_are_built_from_their_nodes_parameters() {
    let runtime = runtime();
    let _context = runtime.enter();

    // `namespace_prefix` prefixes everything but the exclusions.
    let ports = run_kernel(
        "namespace_prefix",
        json!({"namespace": "tenant", "exclusions": ["internal."]}),
        "dsd_prefix_filter",
        None,
        vec![Metric::gauge("app.requests", 1.0), Metric::gauge("internal.x", 1.0)],
    );
    assert_eq!(names_of(&ports[0]), ["tenant.app.requests", "internal.x"]);

    // `timestamp_split` sends timestamped points to `passthrough`, counters there becoming rates over the window.
    let ports = run_kernel(
        "timestamp_split",
        json!({"window": "15s"}),
        "dsd_no_agg_split",
        None,
        vec![Metric::counter("a", 1.0), Metric::counter("b", (100, 2.0))],
    );
    assert_eq!(ports[0], [Metric::counter("a", 1.0)]);
    assert_eq!(
        ports[1],
        [Metric::rate("b", (100, 2.0), std::time::Duration::from_secs(15))]
    );

    // `tag_projection` applies its rules to counters and distributions only.
    let context = || Context::from_static_parts("checkout.latency", &["user_id:1", "env:dev"]);
    let ports = run_kernel(
        "tag_projection",
        tag_projection_params(),
        "dsd_tag_filterlist",
        None,
        vec![Metric::counter(context(), 1.0), Metric::gauge(context(), 1.0)],
    );
    let tags = |metric: &Metric| {
        let mut tags = metric
            .context()
            .tags()
            .into_iter()
            .map(|tag| tag.as_str().to_string())
            .collect::<Vec<_>>();
        tags.sort();
        tags
    };
    assert_eq!(tags(&ports[0][0]), ["env:other"]);
    assert_eq!(tags(&ports[0][1]), ["env:dev", "user_id:1"]);

    // `host_tags` adds its tags to batches admitted before the deadline, and `host_enrichment` changes nothing.
    let host_tags = json!({"tags": ["zone:a"], "deadline": "1700000300s"});
    for (secs, expected) in [(1_700_000_299, vec!["zone:a"]), (1_700_000_300, vec![])] {
        let ports = run_kernel(
            "host_tags",
            host_tags.clone(),
            "metrics_enrich_host_tags",
            Some(saluki_core::execution::AdmissionTime::from_unix_secs(secs)),
            vec![Metric::gauge("a", 1.0)],
        );
        assert_eq!(tags(&ports[0][0]), expected, "{}", secs);
    }
    let ports = run_kernel(
        "host_enrichment",
        json!({}),
        "metrics_enrich",
        None,
        vec![Metric::gauge("a", 1.0)],
    );
    assert_eq!(ports[0], [Metric::gauge("a", 1.0)]);
}

#[test]
fn kernels_report_native_telemetry_under_the_component_they_are_built_for() {
    let runtime = runtime();
    let _context = runtime.enter();
    let recorder = saluki_metrics::test::TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);
    let labels = |component: &'static str| [("component_id", component), ("component_type", "transform")];

    // The timestamp split's passthrough telemetry.
    run_kernel(
        "timestamp_split",
        json!({"window": "10s"}),
        "dsd_no_agg_split",
        None,
        vec![Metric::counter("a", (100, 1.0))],
    );
    assert_eq!(
        recorder.counter(("aggregate_passthrough_metrics_total", &labels("dsd_no_agg_split")[..])),
        Some(1)
    );

    // Tag projection's rule telemetry, and its memo: a reserved node's memo keeps the native identifier, and a policy
    // node's is named after its component.
    for (node, component, memo) in [
        (
            "dsd_tag_filterlist",
            "dsd_tag_filterlist",
            "tag_filterlist/context_cache",
        ),
        (
            "policy/projection",
            "policy_projection",
            "policy_projection/tag_filterlist/context_cache",
        ),
    ] {
        run_kernel(
            "tag_projection",
            tag_projection_params(),
            node,
            None,
            vec![Metric::counter(
                Context::from_static_parts("checkout.latency", &["user_id:1"]),
                1.0,
            )],
        );
        assert_eq!(
            recorder.counter(("tag_filterlist_metrics_modified_total", &labels(component)[..])),
            Some(1),
            "{}",
            node
        );
        // The rule count describes the rules in effect, which the installer records once a generation commits: a
        // kernel built for a preparation that never commits must not change it.
        assert_eq!(
            recorder.gauge(("tag_filterlist_size", &labels(component)[..])),
            None,
            "{}",
            node
        );
        assert_eq!(
            recorder.gauge(("cache_weight_limit", &[("cache_id", memo)][..])),
            Some(1000.0),
            "{}",
            node
        );
    }
}

#[test]
fn a_split_costs_its_predicate_and_a_decision_at_most_a_memo_lookup() {
    // (predicates, a `split` of the first alone, a decision of them all), estimated per event by hand: a constant,
    // flag, emptiness, or kind test costs 1 ns, a name match 5 ns, a tag test 10 ns, an intake-name match 5 ns plus
    // 10 ns to normalize the name, once per event, and routing 5 ns. A decision whose predicates read only context
    // fields and the kind, and whose `split`s cost at least `MEMO_COST_FLOOR` (25 ns) alone, is memoized: a lookup
    // (10 ns) and routing.
    let intake = |values: &[&str]| json!({"intake_name": {"matches": {"values": values, "prefix": false}}});
    let routing = |names: &[&str], prefixes: &[&str]| {
        json!({"any": [
            intake(names),
            {"intake_name": {"matches": {"values": prefixes, "prefix": true}}},
        ]})
    };
    let mirroring = json!({"all": [
        {"flag": true},
        {"flag": true},
        {"any": [{"is_empty": ["a"]}, {"name": {"matches": {"values": ["a"], "prefix": false}}}]},
    ]});
    let cases: [(Vec<Value>, u32, u32); 8] = [
        (vec![json!({"kind_in": ["counter"]})], 6, 6),
        (vec![json!({"const": false})], 6, 6),
        // Below the memo's floor, evaluated, though a hit would cost less.
        (
            vec![json!({"all": [{"kind_in": ["gauge"]}, {"has_tag": "env"}]})],
            16,
            16,
        ),
        (vec![json!({"kind_in": ["gauge"]}), json!({"name": {"eq": "a"}})], 6, 11),
        // One intake-name leaf: below the floor, evaluated.
        (vec![intake(&["a"])], 20, 20),
        // Both leaves read the one intake name.
        (vec![routing(&["a"], &["b."])], 25, 15),
        (vec![mirroring.clone()], 13, 13),
        // Two endpoint routing groups and Multi-Region Failover: three predicates, one lookup.
        (vec![routing(&["a"], &[]), routing(&[], &["b."]), mirroring], 25, 15),
    ];
    for (predicates, split, decision) in cases {
        let decoded = predicates
            .iter()
            .map(|predicate| serde_json::from_value::<crate::predicate::Predicate>(predicate.clone()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            configure("split", json!({"predicate": predicates[0]})).cost(),
            split,
            "{:?}",
            predicates
        );
        assert_eq!(
            decision_cost(&decoded.iter().collect::<Vec<_>>()),
            decision,
            "{:?}",
            predicates
        );
    }
    assert_eq!(decision_cost(&[]), 0);
    assert_eq!(configure("tee", json!({"ports": 5})).cost(), 60);
}

mod properties {
    use proptest::prelude::*;

    use super::configure;
    use crate::{
        model::{EventView, Kind, PortType},
        predicate::NoBindings,
        testing::{arb_metric, arb_predicate},
    };

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// The kinds a `split` declares for each port include the kind of every metric the predicate sends there,
        /// which is what lets validation reject an `emit` that would receive a kind its output does not accept.
        #[test]
        fn property_test_split_port_types_include_every_routed_kind(
            predicate in arb_predicate(),
            metrics in proptest::collection::vec(arb_metric(), 1..8),
        ) {
            let split = configure("split", serde_json::json!({"predicate": predicate}));
            let types = split.output_types(&[PortType::ANY]);
            let compiled = predicate.compile(&NoBindings).expect("generated predicates compile");
            for metric in &metrics {
                let kind = Kind::of(metric.values());
                let matched = compiled.evaluate(&mut EventView::new(metric));
                let port = if matched { 0 } else { 1 };
                prop_assert!(
                    types[port].kinds.contains(kind),
                    "{:?} sends a {} to port {}, typed {:?}",
                    predicate,
                    kind,
                    port,
                    types[port]
                );
                // When the kind alone decides the predicate, evaluation agrees with it, in canonical form too.
                if let Some(decided) = predicate.truth_for_kind(kind) {
                    prop_assert_eq!(decided, matched);
                }
                prop_assert_eq!(predicate.canonical().truth_for_kind(kind), predicate.truth_for_kind(kind));
            }
        }
    }
}

#[test]
fn aggregations_lineage_is_its_window_and_its_other_parameters_reach_its_kernel_per_call() {
    use saluki_metric_kernels::aggregate::AggregateStepParams;

    let configured = configure("aggregate", aggregate_params());
    // The lineage is the window alone, resolved, so a change of any other parameter keeps the state.
    assert_eq!(
        serde_json::to_value(configured.lineage_params()).unwrap(),
        json!({"window": "10s"})
    );
    let bound = {
        let mut value = aggregate_params();
        value["window"] = json!({"config": "aggregator.window", "resolved": "10s"});
        configure("aggregate", value)
    };
    assert_eq!(
        bound.lineage_params(),
        configured.lineage_params(),
        "a bound window is keyed by its resolved value"
    );
    assert_eq!(configured.flush_interval(), Some(std::time::Duration::from_secs(15)));

    let params = configured
        .step_params(None)
        .expect("aggregation takes per-call parameters");
    let step = params
        .downcast_ref::<AggregateStepParams>()
        .expect("aggregation's per-call parameters");
    assert_eq!(step.admit.context_limit, 1_000_000);
    assert_eq!(step.flush.counter_expiry.map(|expiry| expiry.get()), Some(300));
    assert_eq!(step.flush_interval, std::time::Duration::from_secs(15));
    assert!(step.flush.filter.is_none(), "no filter is pushed");

    // A stateless operator keeps the defaults: every parameter is its lineage, and it takes none per call.
    let prefix = configure("namespace_prefix", json!({"namespace": "ns", "exclusions": []}));
    assert!(prefix.step_params(None).is_none());
    assert_eq!(prefix.flush_interval(), None);
    assert_eq!(
        serde_json::to_value(prefix.lineage_params()).unwrap(),
        json!({"namespace": "ns", "exclusions": []})
    );

    // A kernel built with a host's state budgets draws its contexts from them; one built without owns them alone.
    let runtime = runtime();
    let _context = runtime.enter();
    let budgets = crate::runtime::StateBudgets::new(1);
    let node: crate::ir::NodeId = "dsd_agg".parse().unwrap();
    let mut shared = configured
        .kernel()
        .unwrap()
        .build(&KernelBuildContext::detached(&node).with_state_budget(budgets.context_budget(&node)))
        .unwrap();
    let mut alone = configured
        .kernel()
        .unwrap()
        .build(&KernelBuildContext::detached(&node))
        .unwrap();
    let admit = |kernel: &mut Box<dyn saluki_core::execution::Kernel>, names: &[&str]| {
        let header = saluki_core::execution::BatchHeader {
            admitted_at: Some(saluki_core::execution::AdmissionTime::from_unix_secs(1_700_000_003)),
            ..Default::default()
        };
        let mut input = EventsBuffer::default();
        for name in names {
            assert!(input
                .try_push(Event::Metric(Metric::gauge(
                    saluki_core::data_model::event::metric::context::Context::from_parts(
                        name.to_string(),
                        saluki_core::data_model::tags::TagSet::default()
                    ),
                    1.0
                )))
                .is_none());
        }
        let mut budget = Budget::unlimited();
        let mut outputs = PortOutputs::new(1);
        let mut counters = TurnCounters::default();
        let mut turn = saluki_core::execution::Turn::new(
            &header,
            std::time::Instant::now(),
            &mut budget,
            &mut outputs,
            &mut counters,
        );
        kernel.consume(&mut turn, &mut input);
        counters.loss(saluki_core::execution::LossClass::Capacity).events
    };
    assert_eq!(
        admit(&mut shared, &["a", "b"]),
        1,
        "the host's budget holds one context"
    );
    assert_eq!(budgets.global().used(), 1);
    assert_eq!(admit(&mut alone, &["a", "b"]), 0, "its own limit holds a million");
    drop(shared);
    assert_eq!(budgets.global().used(), 0, "a dropped kernel gives its contexts back");
}

#[test]
fn every_lanes_instance_of_a_lineage_built_against_one_cell_shares_one_record() {
    use saluki_metric_kernels::aggregate::LaneShared;

    let node: NodeId = "dsd_agg".parse().unwrap();
    let detached = KernelBuildContext::detached(&node);
    assert!(detached.lanes().is_none());
    assert!(
        detached.lane_share(LaneShared::new).is_none(),
        "a kernel without lanes shares nothing"
    );

    let cell = LaneCell::new();
    assert!(cell.share().is_none());
    let lane = |lane| {
        KernelBuildContext::detached(&node).with_lanes(LaneBinding {
            cell: Arc::clone(&cell),
            lane,
            lanes: 3,
        })
    };
    let first = lane(0).lane_share(LaneShared::new).unwrap();
    assert_eq!(first.lanes(), 3, "the first instance creates the record for every lane");
    let second = lane(2).lane_share(|_| panic!("the record exists")).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(lane(1).lanes().map(|binding| binding.lane), Some(1));
    assert!(cell.share().is_some(), "the host reads the record the kernels share");

    // Aggregation builds its kernels against the cell, and the aggregation operator partitions by context.
    let aggregate = OperatorRegistry::native().get("aggregate").unwrap();
    assert_eq!(aggregate.partition_key(), Some(PartitionKey::Context));
    assert_eq!(
        OperatorRegistry::native()
            .get("dogstatsd_mapper")
            .unwrap()
            .partition_key(),
        None
    );
}
