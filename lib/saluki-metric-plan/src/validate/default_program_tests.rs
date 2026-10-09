//! The default program's shape, as the implementation design draws it, written by hand as JSON fixtures and validated
//! against a hand-written catalog.
//!
//! `fixtures/default-program.json` has every optional branch wired (host tags, two endpoint routing groups,
//! multi-region failover, and autoscaling failover) with the no-aggregation pipeline on, as it is by default.
//! `fixtures/default-program-no-aggregation-pipeline-off.json` is the same program with the pipeline off: no timestamp
//! split, and no merge before the post-aggregation filter.

use saluki_core::data_model::event::metric::{Metric, SetPoints};
use serde_json::{json, Value};

use super::*;
use crate::{
    ir::{derived_terminal_id, Node},
    model::{EventView, Field, Kind, KindSet, PortType, TimestampForm},
    operators::{names, Boundary, ConfiguredOperator, PortRelation, SourceBinding},
    predicate::{CompiledPredicate, NoBindings, Predicate},
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
};

const SPECIFICATION: &str = include_str!("../../../../docs/design/programmable-metrics.md");

/// A validated fixture.
struct Fixture {
    plan: Plan,
    report: ValidationReport,
}

impl Fixture {
    fn load(json: &str) -> Self {
        Self::from_plan(Plan::from_json(json).unwrap())
    }

    fn from_value(value: Value) -> Self {
        Self::from_plan(Plan::from_value(value).unwrap())
    }

    fn from_plan(plan: Plan) -> Self {
        let registry = OperatorRegistry::native();
        let catalog = default_program_catalog();
        let report = Validator::new(&registry, &catalog).validate(&plan);
        Self { plan, report }
    }

    #[track_caller]
    fn node(&self, id: &str) -> &Node {
        let id = id.parse().unwrap();
        self.plan.node(&id).unwrap_or_else(|| panic!("no node `{}`", id))
    }

    #[track_caller]
    fn op(&self, id: &str) -> ConfiguredOperator {
        let node = self.node(id);
        OperatorRegistry::native()
            .configure(node.op.as_str(), &node.params)
            .unwrap()
    }

    #[track_caller]
    fn predicate(&self, id: &str) -> Predicate {
        self.op(id).params().predicate("predicate").unwrap().canonical()
    }

    /// Returns the input port the given output port's edge enters, as `node.port`.
    #[track_caller]
    fn target(&self, from: &str) -> String {
        let edges = self
            .plan
            .edges
            .iter()
            .filter(|edge| edge.from.to_string() == from)
            .collect::<Vec<_>>();
        assert_eq!(edges.len(), 1, "{} has one edge", from);
        edges[0].to.to_string()
    }

    #[track_caller]
    fn port_type(&self, port: &str) -> PortType {
        self.report
            .port_type(&port.parse().unwrap())
            .unwrap_or_else(|| panic!("no type for `{}`", port))
    }
}

fn port_type(kinds: &[Kind], timestamps: TimestampForm) -> PortType {
    PortType {
        kinds: KindSet::of(kinds),
        timestamps,
    }
}

#[test]
fn the_default_program_validates_with_no_findings() {
    for (name, json) in [
        ("default program", DEFAULT_PROGRAM_JSON),
        ("no-aggregation pipeline off", NO_AGGREGATION_PIPELINE_OFF_JSON),
    ] {
        let fixture = Fixture::load(json);
        assert!(
            fixture.report.findings().is_empty(),
            "{} has findings:\n{}",
            name,
            fixture.report
        );
    }
}

#[test]
fn the_default_program_has_the_native_port_types() {
    use Kind::{Counter, Distribution, Gauge, Histogram, Rate, Set};
    use TimestampForm::{Mixed, Timestamped, Untimestamped};

    let dogstatsd = [Counter, Gauge, Set, Histogram, Distribution];
    let aggregated = [Rate, Gauge, Set, Distribution];

    // (port, expected type), for the default program.
    let fixture = Fixture::load(DEFAULT_PROGRAM_JSON);
    let expected = [
        ("dsd_in.metrics", port_type(&dogstatsd, Mixed)),
        ("dsd_prefix_filter_blocklist.false", port_type(&dogstatsd, Mixed)),
        ("dsd_no_agg_split.default", port_type(&dogstatsd, Untimestamped)),
        // Timestamped counters bypass aggregation as rates.
        (
            "dsd_no_agg_split.passthrough",
            port_type(&[Rate, Gauge, Set, Histogram, Distribution], Timestamped),
        ),
        ("dsd_tag_filterlist.out", port_type(&dogstatsd, Untimestamped)),
        // Aggregation emits no counters and no histograms, and timestamps every point.
        ("dsd_agg.out", port_type(&aggregated, Timestamped)),
        (
            "dsd_post_agg_filter_in.out",
            port_type(&[Rate, Gauge, Set, Histogram, Distribution], Timestamped),
        ),
        // The post-aggregation filter drops only scalar series.
        ("dsd_post_agg_filter.true", port_type(&[Rate, Gauge, Set], Timestamped)),
        (
            "otlp_in.metrics",
            port_type(&[Counter, Rate, Gauge, Distribution], Mixed),
        ),
        (
            "checks_ipc_in.metrics",
            port_type(&[Counter, Rate, Gauge, Histogram], Mixed),
        ),
        ("liveness_in.metrics", port_type(&[Gauge], Timestamped)),
        ("trace_sampler.metrics", port_type(&[Counter, Gauge], Untimestamped)),
        ("metrics_enrich_host_tags.out", PortType::ANY),
        // The autoscaling gateway forwards only scalar series, which its output requires.
        (
            "af_metrics_gateway.true",
            port_type(&[Counter, Rate, Gauge, Set], Mixed),
        ),
        ("mrf_metrics_gateway.true", PortType::ANY),
    ];
    for (port, expected) in expected {
        assert_eq!(fixture.port_type(port), expected, "{}", port);
    }

    // With the no-aggregation pipeline off, DogStatsD timestamps are discarded at parse time.
    let fixture = Fixture::load(NO_AGGREGATION_PIPELINE_OFF_JSON);
    let expected = [
        ("dsd_in.metrics", port_type(&dogstatsd, Untimestamped)),
        ("dsd_agg.out", port_type(&aggregated, Timestamped)),
        ("dsd_post_agg_filter.false", port_type(&aggregated, Timestamped)),
        ("metrics_enrich_host_tags.out", PortType::ANY),
    ];
    for (port, expected) in expected {
        assert_eq!(fixture.port_type(port), expected, "{}", port);
    }
}

#[test]
fn the_default_program_names_inline_terminals_by_their_parent_port() {
    for json in [DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON] {
        let plan = Plan::from_json(json).unwrap();
        let terminals = plan
            .nodes
            .iter()
            .filter(|node| [names::EMIT, names::DROP].contains(&node.op.as_str()))
            .collect::<Vec<_>>();
        // Six drops and six emits.
        assert_eq!(terminals.len(), 12);
        for terminal in terminals {
            let edge = plan.edges.iter().find(|edge| edge.to.node == terminal.id).unwrap();
            assert_eq!(terminal.id, derived_terminal_id(&edge.from.node, &edge.from.port));
        }
    }
}

#[test]
fn changes_that_break_the_default_program_are_rejected() {
    let base = || serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    let edge = |value: &mut Value, from: &str, to: &str| {
        let edges = value["edges"].as_array_mut().unwrap();
        let edge = edges.iter_mut().find(|edge| edge["from"] == from).unwrap();
        edge["to"] = json!(to);
    };

    // Routing a policy branch's matches to the main output delivers them twice.
    let mut value = base();
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "metrics_routing_filter_0_true")
        .unwrap()["params"]["output"] = json!("dd_metrics_encode");
    let fixture = Fixture::from_value(value);
    let rules = fixture
        .report
        .findings()
        .iter()
        .map(|f| (f.rule, f.location.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        rules,
        vec![
            (Rule::UnusedOutput, "output `metrics_routing_encode_0`".to_string()),
            (Rule::DuplicateContribution, "node `metrics_policy`".to_string()),
        ],
        "{}",
        fixture.report
    );

    // The autoscaling output cannot receive what the gateway does not forward.
    let mut value = base();
    edge(&mut value, "af_metrics_gateway.true", "af_metrics_gateway_false.in");
    edge(&mut value, "af_metrics_gateway.false", "af_metrics_gateway_true.in");
    let fixture = Fixture::from_value(value);
    let rules = fixture
        .report
        .findings()
        .iter()
        .map(|f| (f.rule, f.location.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        rules,
        vec![(Rule::OutputAccepts, "node `af_metrics_gateway_true`".to_string())],
        "{}",
        fixture.report
    );
}

/// Before stage 4, the policy gateways lower to opaque hosts backed by the native components, with direct edges from
/// their `out` ports to `emit` nodes named `<gateway>_out` by `derived_terminal_id`, as the host's lowering writes
/// them.
fn with_opaque_gateways() -> Value {
    crate::testing::with_opaque_gateways(DEFAULT_PROGRAM_JSON)
}

#[test]
fn the_default_program_with_opaque_gateways_validates() {
    let fixture = Fixture::from_value(with_opaque_gateways());
    assert!(fixture.report.findings().is_empty(), "{}", fixture.report);
    assert_eq!(
        fixture.op("mrf_metrics_gateway").contract().boundary,
        Boundary::OpaqueHost
    );
}

/// A row of the coverage table, and the check that the IR represents it.
type CoverageCheck = (&'static str, fn(&Fixture));

/// Each row of the specification's default lowering coverage table, with a check that the IR represents it as the
/// table says.
const COVERAGE: &[CoverageCheck] = &[
    (
        "DogStatsD source: decoding, payload-type filtering, origin enrichment, tag cardinality, sample-rate scaling",
        |f| {
            // A source I/O host: everything inside decoding is host behavior.
            let op = f.op("dsd_in");
            assert_eq!(op.name(), names::SOURCE);
            assert_eq!(op.params().str("binding"), Some("dogstatsd"));
            assert_eq!(op.contract().boundary, Boundary::IoHost);
        },
    ),
    (
        "DogStatsD mapping, prefix and listener filtering, tag filterlist",
        |f| {
            assert_eq!(f.op("dsd_enrich").name(), names::DOGSTATSD_MAPPER);
            assert_eq!(f.target("dsd_enrich.out"), "dsd_prefix_filter.in");
            assert_eq!(f.op("dsd_prefix_filter").name(), names::NAMESPACE_PREFIX);
            // The listener filterlist matches the intake name after prefixing, and drops what matches.
            assert_eq!(f.target("dsd_prefix_filter.out"), "dsd_prefix_filter_blocklist.in");
            assert!(matches!(
                f.predicate("dsd_prefix_filter_blocklist"),
                Predicate::IntakeName(_)
            ));
            assert_eq!(f.op("dsd_prefix_filter_blocklist_true").name(), names::DROP);
            let tags = f.op("dsd_tag_filterlist");
            assert_eq!(tags.name(), names::TAG_PROJECTION);
            assert!(tags.contract().writes.get(Kind::Gauge).is_empty());
            for operator in [
                "dsd_enrich",
                "dsd_prefix_filter",
                "dsd_prefix_filter_blocklist",
                "dsd_tag_filterlist",
            ] {
                assert_eq!(
                    f.op(operator).contract().boundary,
                    Boundary::Synchronous,
                    "{}",
                    operator
                );
            }
        },
    ),
    ("Timestamp split (no-aggregation pipeline enabled)", |f| {
        let op = f.op("dsd_no_agg_split");
        assert_eq!(op.name(), names::TIMESTAMP_SPLIT);
        assert_eq!(op.contract().ports, PortRelation::PointDisjoint);
        assert_eq!(
            op.ports().outputs[1].mapping.kinds.get(Kind::Counter),
            KindSet::of(&[Kind::Rate])
        );
        // The passthrough port skips the tag filterlist and aggregation and rejoins at the post-aggregation filter.
        assert_eq!(f.target("dsd_no_agg_split.default"), "dsd_tag_filterlist.in");
        assert_eq!(f.target("dsd_no_agg_split.passthrough"), "dsd_post_agg_filter_in.1");
        assert_eq!(f.target("dsd_post_agg_filter_in.out"), "dsd_post_agg_filter.in");
        // Its window and aggregation's are one binding.
        assert_eq!(op.params().get("window"), f.op("dsd_agg").params().get("window"));
    }),
    ("DogStatsD aggregation, including histogram expansion", |f| {
        let op = f.op("dsd_agg");
        assert_eq!(op.name(), names::AGGREGATE);
        assert_eq!(op.contract().boundary, Boundary::OpaqueHost);
        assert!(op.contract().writes.get(Kind::Histogram).contains(Field::Name));
        assert_eq!(
            f.port_type("dsd_agg.out"),
            port_type(
                &[Kind::Rate, Kind::Gauge, Kind::Set, Kind::Distribution],
                TimestampForm::Timestamped
            )
        );
    }),
    ("Post-aggregation filter", |f| {
        let Predicate::All(operands) = f.predicate("dsd_post_agg_filter") else {
            panic!("the post-aggregation filter is a conjunction");
        };
        assert!(operands.contains(&Predicate::KindIn(KindSet::of(&[
            Kind::Counter,
            Kind::Rate,
            Kind::Gauge,
            Kind::Set
        ]))));
        assert!(operands
            .iter()
            .any(|operand| matches!(operand, Predicate::IntakeName(_))));
        assert_eq!(f.op("dsd_post_agg_filter_true").name(), names::DROP);
    }),
    (
        "OTLP metrics receiver and translation; checks IPC; `liveness_in`; `trace_sampler.metrics`",
        |f| {
            for (id, binding, port) in [
                ("otlp_in", "otlp", "metrics_enrich_in.1"),
                ("checks_ipc_in", "checks", "metrics_enrich_in.2"),
                ("liveness_in", "liveness", "metrics_enrich_in.3"),
                ("trace_sampler", "trace_sampler", "metrics_enrich_in.4"),
            ] {
                let op = f.op(id);
                assert_eq!(op.params().str("binding"), Some(binding));
                assert_eq!(op.contract().boundary, Boundary::IoHost);
                assert_eq!(f.target(&format!("{}.metrics", id)), port);
            }
        },
    ),
    ("`metrics_enrich`: host enrichment, then host tags", |f| {
        assert_eq!(f.target("metrics_enrich_in.out"), "metrics_enrich.in");
        assert_eq!(f.op("metrics_enrich").name(), names::HOST_ENRICHMENT);
        assert_eq!(f.target("metrics_enrich.out"), "metrics_enrich_host_tags.in");
        let host_tags = f.op("metrics_enrich_host_tags");
        assert_eq!(host_tags.name(), names::HOST_TAGS);
        assert!(host_tags.contract().time.reads_admission_time);
        assert!(host_tags.params().duration("deadline").is_some());
    }),
    ("Endpoint allowlists and additional endpoints", |f| {
        // One output per encoder: the main encoder, and one per routing group, each behind a split on the intake name.
        assert_eq!(f.target("metrics_policy.0"), "metrics_policy_0.in");
        for group in 0..2 {
            let split = format!("metrics_routing_filter_{}", group);
            let Predicate::Any(operands) = f.predicate(&split) else {
                panic!("{} matches names or prefixes", split);
            };
            assert!(operands
                .iter()
                .all(|operand| matches!(operand, Predicate::IntakeName(_))));
            let emit = f.op(&format!("{}_true", split));
            assert_eq!(
                emit.params().get("output"),
                Some(&crate::ir::ParamValue::Output(
                    OutputId::new(format!("metrics_routing_encode_{}", group)).unwrap()
                ))
            );
        }
        // The outputs' recipients are endpoints of one shared forwarder.
        for output in &f.plan.outputs[1..4] {
            assert!(output.recipients.iter().all(|r| r.as_str().starts_with("dd_out/")));
        }
        // Before stage 4 the same branches are opaque filters.
        assert!(Fixture::from_value(with_opaque_gateways()).report.is_valid());
    }),
    ("Multi-region failover gateway", |_| {
        // Bound to the static enable flag, the live mirroring flag, and a live allowlist of raw names; an empty list
        // forwards everything (see `the_gateways_forward_what_native_configuration_selects`).
        let forwards = |enabled: bool, mirroring: bool, allowlist: Value| {
            let gateway = gateway(
                "mrf_metrics_gateway",
                &[
                    ("multi_region_failover.enabled", json!(enabled)),
                    ("multi_region_failover.metric_mirroring.enabled", json!(mirroring)),
                    ("multi_region_failover.metric_mirroring.allowlist", allowlist),
                ],
            );
            [counter("requests.count"), counter("other")].map(|metric| gateway.forwards(&metric))
        };
        assert_eq!(forwards(true, true, json!([])), [true, true]);
        assert_eq!(forwards(true, true, json!(["requests.count"])), [true, false]);
        assert_eq!(forwards(false, true, json!([])), [false, false]);
        assert_eq!(forwards(true, false, json!(["requests.count"])), [false, false]);
    }),
    ("Autoscaling failover gateway", |f| {
        // Bound to a startup list of raw names; an empty list forwards nothing, and only scalar series pass.
        let empty = gateway("af_metrics_gateway", &[("autoscaling_failover.metrics", json!([]))]);
        assert!(!empty.forwards(&counter("requests.count")));
        let listed = gateway(
            "af_metrics_gateway",
            &[("autoscaling_failover.metrics", json!(["requests.count"]))],
        );
        for metric in [
            counter("requests.count"),
            Metric::rate("requests.count", 1.0, std::time::Duration::from_secs(10)),
            Metric::gauge("requests.count", 1.0),
            Metric::set("requests.count", SetPoints::from("member")),
        ] {
            assert!(listed.forwards(&metric), "{:?}", metric.values());
        }
        assert!(!listed.forwards(&Metric::distribution("requests.count", 1.0)));
        assert!(!listed.forwards(&counter("other")));
        let output = f.plan.output(&OutputId::new("af_metrics_encode").unwrap()).unwrap();
        assert!(!output.accepts.kinds.contains(Kind::Distribution));
    }),
    ("Encoders and forwarders", |f| {
        // One output per native encoder, each reached by exactly one `emit`: one encoder stream per output.
        let ids = f
            .plan
            .outputs
            .iter()
            .map(|output| output.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "dsd_client_telemetry",
                "dd_metrics_encode",
                "metrics_routing_encode_0",
                "metrics_routing_encode_1",
                "mrf_metrics_encode",
                "af_metrics_encode"
            ]
        );
        for output in &f.plan.outputs {
            let emits = f
                .plan
                .nodes
                .iter()
                .filter(|node| node.params.get("output") == Some(&json!(output.id.as_str())))
                .count();
            assert_eq!(emits, 1, "{}", output.id);
        }
    }),
    ("Internal telemetry", |f| {
        // Not a pipeline source: no binding exists for it, and no node represents it.
        assert!(SourceBinding::from_name("internal_telemetry").is_none());
        assert_eq!(
            f.plan
                .nodes
                .iter()
                .filter(|node| node.op.as_str() == names::SOURCE)
                .count(),
            SourceBinding::ALL.len()
        );
    }),
    (
        "`dsd_stats_out`, `dsd_debug_log_out` (taps on the DogStatsD source's metrics), `dsd_client_telemetry_out`",
        |f| {
            // The client telemetry tap is a tee after the post-aggregation filter to an internal output without
            // recipients. The source taps are host edges, outside the plan: the source's only edge enters the mapper.
            assert_eq!(f.target("dsd_post_agg_filter.false"), "dsd_post_agg_filter_fanout.in");
            assert_eq!(
                f.target("dsd_post_agg_filter_fanout.0"),
                "dsd_post_agg_filter_fanout_0.in"
            );
            let output = f.plan.output(&OutputId::new("dsd_client_telemetry").unwrap()).unwrap();
            assert!(output.recipients.is_empty());
            assert_eq!(f.target("dsd_in.metrics"), "dsd_enrich.in");
            assert!(f
                .plan
                .nodes
                .iter()
                .all(|node| !node.id.to_string().starts_with("dsd_stats")));
        },
    ),
    ("OTLP relay and proxy mode", |_| {
        // Unsupported: a payload path, not metric events. The source bindings are exactly the five metric sources, and
        // a source cannot bind anything else.
        let bindings = SourceBinding::ALL.map(SourceBinding::as_str);
        assert_eq!(bindings, ["dogstatsd", "otlp", "checks", "liveness", "trace_sampler"]);
        assert!(!binds("otlp_relay") && !binds("otlp_proxy"));
    }),
    ("Logs, events, service checks, traces", |_| {
        // Outside the plan: no source binds them.
        for unsupported in ["logs", "events", "service_checks", "traces"] {
            assert!(!binds(unsupported), "{}", unsupported);
        }
    }),
];

/// Returns `true` if a `source` node can bind the host source `binding`.
fn binds(binding: &str) -> bool {
    let params = serde_json::from_value(json!({"binding": binding})).unwrap();
    OperatorRegistry::native().configure(names::SOURCE, &params).is_ok()
}

/// A predicate from the default-program fixture, compiled for evaluation.
struct Gateway(CompiledPredicate);

impl Gateway {
    /// Returns `true` if the predicate sends `metric` to its `true` port, which delivers to the gateway's output.
    fn forwards(&self, metric: &Metric) -> bool {
        self.0.evaluate(&mut EventView::new(metric))
    }
}

/// Returns the predicate of the fixture's node `id`, with the resolved value of each given binding replaced, as if
/// native configuration had those values.
#[track_caller]
fn gateway(id: &str, values: &[(&str, Value)]) -> Gateway {
    fn rebind(value: &mut Value, binding: &str, resolved: &Value, found: &mut usize) {
        match value {
            Value::Object(members) => {
                if members.get("config").and_then(Value::as_str) == Some(binding) {
                    members.insert("resolved".to_string(), resolved.clone());
                    *found += 1;
                    return;
                }
                members
                    .values_mut()
                    .for_each(|member| rebind(member, binding, resolved, found));
            }
            Value::Array(elements) => elements
                .iter_mut()
                .for_each(|element| rebind(element, binding, resolved, found)),
            _ => {}
        }
    }

    let fixture = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    let node = fixture["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id)
        .unwrap_or_else(|| panic!("no node `{}`", id));
    let mut predicate = node["params"]["predicate"].clone();
    for (binding, resolved) in values {
        let mut found = 0;
        rebind(&mut predicate, binding, resolved, &mut found);
        assert!(found > 0, "`{}` does not use the binding `{}`", id, binding);
    }
    let predicate = serde_json::from_value::<Predicate>(predicate).unwrap();
    Gateway(predicate.compile(&NoBindings).unwrap())
}

fn counter(name: &'static str) -> Metric {
    Metric::counter(name, 1.0)
}

#[test]
fn the_gateways_forward_what_native_configuration_selects() {
    // Endpoint routing: the intake name is one of the group's names, or starts with one of its prefixes; an empty
    // group forwards nothing.
    let routing = |names: Value, prefixes: Value| {
        gateway(
            "metrics_routing_filter_0",
            &[
                ("metrics_endpoint_routing.group.0123456789abcdef.names", names),
                ("metrics_endpoint_routing.group.0123456789abcdef.prefixes", prefixes),
            ],
        )
    };
    let metrics = || {
        [
            counter("checkout.requests"),
            counter("checkout.latency"),
            // Normalizing the name gives the intake name `checkout.requests`.
            counter("1checkout.requests"),
            counter("catalog.requests"),
        ]
    };
    // (names, prefixes, expected for each metric)
    let cases = [
        (json!([]), json!([]), [false, false, false, false]),
        (json!(["checkout.requests"]), json!([]), [true, false, true, false]),
        (json!([]), json!(["checkout."]), [true, true, true, false]),
        (
            json!(["catalog.requests"]),
            json!(["checkout."]),
            [true, true, true, true],
        ),
    ];
    for (names, prefixes, expected) in cases {
        let gateway = routing(names.clone(), prefixes.clone());
        assert_eq!(
            metrics().map(|metric| gateway.forwards(&metric)),
            expected,
            "names {} prefixes {}",
            names,
            prefixes
        );
    }

    // The fixture's own values: the first group lists names and prefixes, the second only prefixes.
    let fixture = Fixture::load(DEFAULT_PROGRAM_JSON);
    let compiled = |id: &str| Gateway(fixture.predicate(id).compile(&NoBindings).unwrap());
    assert_eq!(
        metrics().map(|metric| compiled("metrics_routing_filter_0").forwards(&metric)),
        [true, true, true, false]
    );
    assert_eq!(
        metrics().map(|metric| compiled("metrics_routing_filter_1").forwards(&metric)),
        [false, false, false, true]
    );
}

/// Returns the first cell of every row of the specification's default lowering coverage table.
fn specification_coverage_rows() -> Vec<&'static str> {
    let section = SPECIFICATION
        .split("\n## Default lowering coverage\n")
        .nth(1)
        .expect("the specification has a default lowering coverage section");
    section
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        // The header row and the separator row.
        .skip(2)
        .filter_map(|line| line.strip_prefix("| ")?.split(" | ").next())
        .collect()
}

#[test]
fn the_ir_represents_every_component_of_the_default_lowering_coverage_table() {
    let rows = specification_coverage_rows();
    assert_eq!(rows.len(), 15, "{:?}", rows);
    let checked = COVERAGE.iter().map(|(row, _)| *row).collect::<Vec<_>>();
    assert_eq!(checked, rows, "every row of the table has a check, in order");

    let fixture = Fixture::load(DEFAULT_PROGRAM_JSON);
    assert!(fixture.report.is_valid(), "{}", fixture.report);
    for (row, check) in COVERAGE {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&fixture)));
        assert!(outcome.is_ok(), "coverage row failed: {}", row);
    }
}
