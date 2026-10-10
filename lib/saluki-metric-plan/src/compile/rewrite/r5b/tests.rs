//! R5b's applicability, its pushed filter, and the evidence that the removal form it replaces is unsound.
//!
//! Every expectation is written by hand from the applicability conditions in the module documentation.

use std::sync::OnceLock;

use saluki_core::{
    data_model::event::metric::{context::Context, Metric, MetricMetadata, MetricValues},
    execution::AdmissionTime,
};
use serde_json::{json, Value};
use stringtheory::MetaString;

use super::*;
use crate::{
    compile::rewrite::harness::{self, drop, emit, node, plan, split, TEST_SIGNAL_MAP},
    interpret::{Interpreter, RunOutput, Trace},
    ir::OutputId,
    operators::{OperatorDef, OperatorRegistry},
    predicate::NoBindings,
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
    validate::Validator,
};

/// The registry R5b applies under: aggregation behind an isolation fence, as a kernel, plus the harness's dynamic map.
fn registry() -> &'static OperatorRegistry {
    static REGISTRY: OnceLock<OperatorRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let dynamic = harness::registry()
            .get(TEST_SIGNAL_MAP)
            .expect("the harness registers its dynamic map");
        let operators = OperatorRegistry::native()
            .iter()
            .chain([dynamic])
            .collect::<Vec<&'static dyn OperatorDef>>();
        OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()))
    })
}

/// Validates a plan and runs the analysis over it.
fn analyze_plan(plan: &Plan, registry: &OperatorRegistry, catalog: &dyn crate::validate::Catalog) -> Analysis {
    let report = Validator::new(registry, catalog).validate(plan);
    assert!(report.is_valid(), "the fixture plan is invalid:\n{}", report);
    let (sites, records) = analyze(plan, &report, registry);
    Analysis { sites, records }
}

struct Analysis {
    sites: Vec<PushedFilterSite>,
    records: Vec<RewriteRecord>,
}

impl Analysis {
    /// Returns each record as `(applied, nodes, reason)`.
    fn outcomes(&self) -> Vec<(bool, Vec<String>, String)> {
        self.records
            .iter()
            .map(|record| {
                let reason = match &record.outcome {
                    RewriteOutcome::Applied => String::new(),
                    RewriteOutcome::Skipped { reason } => reason.clone(),
                };
                (
                    record.is_applied(),
                    record.nodes.iter().map(ToString::to_string).collect(),
                    reason,
                )
            })
            .collect()
    }
}

/// A DogStatsD source without timestamped points, as aggregation receives them.
fn dogstatsd() -> Value {
    node(
        "src",
        names::SOURCE,
        json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
        true,
    )
}

fn aggregate_params(window: &str, context_limit: u64, expiry: &str) -> Value {
    json!({
        "window": window, "flush_interval": "15s", "context_limit": context_limit, "counter_expiry": expiry,
        "flush_open_windows": false, "histogram_aggregates": ["max", "count"], "histogram_percentiles": [],
        "copy_to_distribution": false, "copy_to_distribution_prefix": "",
    })
}

/// A native aggregation with a 10-second window.
fn aggregate() -> Value {
    node("agg", names::AGGREGATE, aggregate_params("10s", 1_000, "5m"), true)
}

/// `src -> agg -> <between...> -> f`, where `f` drops by its `true` port and keeps for `o0` by its `false` port.
fn below_aggregation(between: Vec<(Value, &str, &str)>, predicate: Value) -> Plan {
    let mut nodes = vec![dogstatsd(), aggregate()];
    let mut edges = vec!["src.metrics -> agg.in".to_string()];
    let mut previous = "agg.out".to_string();
    for (node, input, output) in between {
        let id = node["id"].as_str().unwrap().to_string();
        edges.push(format!("{} -> {}.{}", previous, id, input));
        previous = format!("{}.{}", id, output);
        nodes.push(node);
    }
    nodes.extend([split("f", predicate, true), drop("f_drop", true), emit("e", "o0")]);
    edges.extend([
        format!("{} -> f.in", previous),
        "f.true -> f_drop.in".to_string(),
        "f.false -> policy/e.in".to_string(),
    ]);
    plan(nodes, &edges.iter().map(String::as_str).collect::<Vec<_>>(), &["o0"])
}

fn kinds_of(kinds: &[Kind]) -> KindSet {
    KindSet::of(kinds)
}

/// What aggregation emits for the kinds DogStatsD carries, but histograms: rates for counters, and the rest as is.
const EMITTED_BUT_HISTOGRAMS: KindSet = KindSet::of(&[Kind::Rate, Kind::Gauge, Kind::Set, Kind::Distribution]);

#[test]
fn the_default_programs_post_aggregation_filter_is_pushed_into_aggregation_for_every_kind_but_histograms() {
    // The default program's post-aggregation filter drops by kind and intake name, behind the merge that joins the
    // timestamped points' passthrough (or directly, without the no-aggregation pipeline). Histograms' statistics are
    // renamed and re-kinded, so they stay with the filter: the specification's partial push-down.
    let registry = OperatorRegistry::native();
    let catalog = default_program_catalog();
    for (what, json) in [
        ("default program", DEFAULT_PROGRAM_JSON),
        ("no-aggregation pipeline off", NO_AGGREGATION_PIPELINE_OFF_JSON),
    ] {
        let plan = Plan::from_json(json).unwrap();
        let analysis = analyze_plan(&plan, &registry, &catalog);
        assert_eq!(analysis.sites.len(), 1, "{}", what);
        let site = &analysis.sites[0];
        assert_eq!(site.owner.to_string(), "dsd_agg", "{}", what);
        assert_eq!(site.filter.to_string(), "dsd_post_agg_filter", "{}", what);
        assert_eq!(site.drop_port.as_str(), "true", "{}", what);
        assert!(site.drops_matching, "{}", what);
        assert_eq!(site.emitted, EMITTED_BUT_HISTOGRAMS, "{}", what);
        assert!(!site.histograms, "{}", what);
        let filter = plan.nodes.iter().find(|node| node.id == site.filter).unwrap();
        assert_eq!(
            Some(&site.predicate),
            registry
                .configure(filter.op.as_str(), &filter.params)
                .unwrap()
                .params()
                .predicate("predicate"),
            "{}",
            what
        );

        assert_eq!(
            analysis.outcomes(),
            [(
                true,
                vec!["dsd_agg".to_string(), "dsd_post_agg_filter".to_string()],
                String::new()
            )],
            "{}",
            what
        );
        let record = &analysis.records[0];
        assert_eq!(record.id, RewriteId::R5b);
        assert!(!record.capacity_rule, "R5b is exact under capacity too");
        assert!(
            record.justification.contains("of {rate, gauge, set, distribution}"),
            "{}",
            record.justification
        );
        // The filter's own drop series keep counting what it drops; what reaches it, and the owner's sends, fall.
        let changed = record
            .telemetry
            .iter()
            .map(|effect| (effect.node.to_string(), effect.change))
            .collect::<Vec<_>>();
        let mut expected = vec![("dsd_agg".to_string(), TelemetryChange::Changed)];
        if what == "default program" {
            expected.push(("dsd_post_agg_filter_in".to_string(), TelemetryChange::Changed));
        }
        expected.push(("dsd_post_agg_filter".to_string(), TelemetryChange::Changed));
        assert_eq!(changed, expected, "{}", what);
    }
}

#[test]
fn a_filter_is_pushed_for_the_kinds_whose_emission_it_judges_as_it_would_downstream() {
    let host_enrichment = || (node("hosts", "host_enrichment", json!({}), true), "in", "out");
    let merge = || (node("m", names::MERGE, json!({"inputs": 1}), true), "0", "out");
    let host_tags = || {
        (
            node(
                "host_tags",
                "host_tags",
                json!({"tags": ["env:prod"], "deadline": "1700000300s"}),
                true,
            ),
            "in",
            "out",
        )
    };
    let tag_projection = || {
        (
            node(
                "tags",
                "tag_projection",
                json!({"rules": [{"metric_name": "a", "action": "exclude", "tags": ["env"]}], "value_rules": [],
                       "cache_capacity": 1000}),
                true,
            ),
            "in",
            "out",
        )
    };
    // (what, plan, expected site as (emitted kinds, histograms, drops matching), or the skip reason's start)
    type Expected = Result<(KindSet, bool), &'static str>;
    let cases: Vec<(&str, Plan, Expected)> = vec![
        (
            "a name filter leaves histograms' statistics to the filter",
            below_aggregation(vec![], json!({"name": {"prefix": "drop."}})),
            Ok((EMITTED_BUT_HISTOGRAMS, false)),
        ),
        (
            "a tag filter judges histograms' statistics too, which keep the context's tags",
            below_aggregation(vec![], json!({"has_tag": "debug"})),
            Ok((EMITTED_BUT_HISTOGRAMS, true)),
        ),
        (
            "a host filter judges them too",
            below_aggregation(vec![], json!({"host": {"prefix": "test-"}})),
            Ok((EMITTED_BUT_HISTOGRAMS, true)),
        ),
        (
            "a kind filter judges the kind a record leaves as",
            below_aggregation(vec![], json!({"kind_in": ["rate"]})),
            Ok((EMITTED_BUT_HISTOGRAMS, false)),
        ),
        (
            "through a merge",
            below_aggregation(vec![merge()], json!({"name": {"prefix": "drop."}})),
            Ok((EMITTED_BUT_HISTOGRAMS, false)),
        ),
        (
            "through a map that writes nothing the filter reads",
            below_aggregation(vec![host_enrichment()], json!({"has_tag": "debug"})),
            Ok((EMITTED_BUT_HISTOGRAMS, true)),
        ),
        (
            "through a map that writes nothing a host filter reads either",
            below_aggregation(vec![host_enrichment()], json!({"host": {"prefix": "test-"}})),
            Ok((EMITTED_BUT_HISTOGRAMS, true)),
        ),
        (
            "a map that merges contexts, as appending host tags does",
            below_aggregation(vec![host_tags()], json!({"has_tag": "env"})),
            Err("`host_tags`, below it before any filter, does not preserve the identity of what it maps"),
        ),
        (
            "a projection of the tags a tag filter reads",
            below_aggregation(vec![tag_projection()], json!({"has_tag": "env"})),
            Err("`tags`, below it before any filter, does not preserve the identity of what it maps"),
        ),
        (
            "a dynamic node below the state owner",
            below_aggregation(
                vec![(node("dyn", TEST_SIGNAL_MAP, json!({}), true), "in", "out")],
                json!({"has_tag": "debug"}),
            ),
            Err("`dyn` reads a host signal"),
        ),
    ];
    for (what, plan, expected) in cases {
        let analysis = analyze_plan(&plan, registry(), &harness::catalog());
        match expected {
            Ok((emitted, histograms)) => {
                assert_eq!(analysis.sites.len(), 1, "{}: {:?}", what, analysis.outcomes());
                let site = &analysis.sites[0];
                assert_eq!((site.emitted, site.histograms), (emitted, histograms), "{}", what);
                assert_eq!(
                    (site.owner.to_string(), site.filter.to_string(), site.drop_port.as_str()),
                    ("agg".to_string(), "f".to_string(), "true"),
                    "{}",
                    what
                );
                assert!(site.drops_matching, "{}", what);
            }
            Err(reason) => {
                assert!(analysis.sites.is_empty(), "{}", what);
                let outcomes = analysis.outcomes();
                assert_eq!(outcomes.len(), 1, "{}", what);
                assert!(!outcomes[0].0, "{}", what);
                assert!(outcomes[0].2.starts_with(reason), "{}: {}", what, outcomes[0].2);
            }
        }
    }

    // A filter that drops by its `false` port drops what its predicate does not match.
    let mut keeps = below_aggregation(vec![], json!({"name": {"prefix": "keep."}}));
    for edge in &mut keeps.edges {
        if edge.from.to_string() == "f.true" {
            edge.from = "f.false".parse().unwrap();
        } else if edge.from.to_string() == "f.false" {
            edge.from = "f.true".parse().unwrap();
        }
    }
    let analysis = analyze_plan(&keeps, registry(), &harness::catalog());
    assert_eq!(analysis.sites.len(), 1);
    assert_eq!(analysis.sites[0].drop_port.as_str(), "false");
    assert!(!analysis.sites[0].drops_matching);
}

#[test]
fn nothing_is_pushed_past_a_copy_or_a_split_that_is_not_a_filter() {
    // A tee copies every record, so a filter on one copy does not judge the others.
    let teed = plan(
        vec![
            dogstatsd(),
            aggregate(),
            node("t", names::TEE, json!({"ports": 2}), true),
            split("f", json!({"name": {"prefix": "drop."}}), true),
            drop("f_drop", true),
            emit("e0", "o0"),
            emit("e1", "o1"),
        ],
        &[
            "src.metrics -> agg.in",
            "agg.out -> t.in",
            "t.0 -> f.in",
            "f.true -> f_drop.in",
            "f.false -> policy/e0.in",
            "t.1 -> policy/e1.in",
        ],
        &["o0", "o1"],
    );
    // A split whose ports both deliver is routing, not a filter.
    let routed = plan(
        vec![
            dogstatsd(),
            aggregate(),
            split("r", json!({"name": {"prefix": "a."}}), true),
            emit("e0", "o0"),
            emit("e1", "o1"),
        ],
        &[
            "src.metrics -> agg.in",
            "agg.out -> r.in",
            "r.true -> policy/e0.in",
            "r.false -> policy/e1.in",
        ],
        &["o0", "o1"],
    );
    // Straight to an output.
    let direct = plan(
        vec![dogstatsd(), aggregate(), emit("e0", "o0")],
        &["src.metrics -> agg.in", "agg.out -> policy/e0.in"],
        &["o0"],
    );
    for (what, plan, reason) in [
        (
            "a tee",
            teed,
            "`t`, below it before any filter, copies every record to each of its ports",
        ),
        (
            "a routing split",
            routed,
            "`r`, below it before any filter, routes records between its ports",
        ),
        (
            "an emit",
            direct,
            "`policy/e0`, below it before any filter, ends the path",
        ),
    ] {
        let analysis = analyze_plan(&plan, registry(), &harness::catalog());
        assert!(analysis.sites.is_empty(), "{}", what);
        let outcomes = analysis.outcomes();
        assert_eq!(outcomes.len(), 1, "{}", what);
        assert_eq!(outcomes[0].1, ["agg"], "{}", what);
        assert!(outcomes[0].2.starts_with(reason), "{}: {}", what, outcomes[0].2);
    }
}

#[test]
fn a_pushed_filter_judges_what_aggregation_emits_by_its_parts_and_only_for_its_kinds() {
    let site = PushedFilterSite {
        owner: "agg".parse().unwrap(),
        filter: "f".parse().unwrap(),
        drop_port: PortName::from_static("true"),
        drop: "f_true".parse().unwrap(),
        predicate: serde_json::from_value(json!({"any": [{"has_tag": "debug"}, {"kind_in": ["set"]}]})).unwrap(),
        drops_matching: true,
        emitted: kinds_of(&[Kind::Rate, Kind::Set]),
        histograms: true,
    };
    let filter = site.compile(SiteIndex::new(3), &NoBindings).unwrap();
    assert_eq!(filter.site(), SiteIndex::new(3));

    let metadata = MetricMetadata::default().with_unit(MetaString::from_static("second"));
    let debug = Context::from_static_parts("requests", &["debug"]);
    let plain = Context::from_static_parts("requests", &[]);
    let cases = [
        // (context, kind it leaves as, dropped)
        (&debug, OutputKind::Rate, true),
        (&plain, OutputKind::Rate, false),
        // A set matches by its kind.
        (&plain, OutputKind::Set, true),
        // Gauges are not among the kinds it judges: the filter downstream does.
        (&debug, OutputKind::Gauge, false),
        (&debug, OutputKind::Distribution, false),
    ];
    for (context, kind, dropped) in cases {
        assert_eq!(
            filter.drops(context, kind, &metadata),
            dropped,
            "{:?} {:?}",
            context,
            kind
        );
    }
    assert!(filter.drops_histogram(&debug, &metadata));
    assert!(!filter.drops_histogram(&plain, &metadata));

    // Dropping by the `false` port drops what does not match; without histograms, none is judged.
    let keeps = PushedFilterSite {
        drops_matching: false,
        histograms: false,
        ..site
    };
    let filter = keeps.compile(SiteIndex::new(0), &NoBindings).unwrap();
    assert!(!filter.drops(&debug, OutputKind::Rate, &metadata));
    assert!(filter.drops(&plain, OutputKind::Rate, &metadata));
    assert!(!filter.drops_histogram(&plain, &metadata));

    // It judges exactly what the filter would of the record built from those parts.
    let predicate = keeps.predicate.compile(&NoBindings).unwrap();
    for (context, kind) in [(&debug, Kind::Rate), (&plain, Kind::Set)] {
        let record = Metric::from_parts(
            context.clone(),
            match kind {
                Kind::Rate => MetricValues::rate(1.0, std::time::Duration::from_secs(10)),
                _ => MetricValues::set("member"),
            },
            metadata.clone(),
        );
        let output = OutputKind::of(record.values()).unwrap();
        assert_eq!(
            filter.drops(context, output, &metadata),
            !predicate.evaluate(&mut EventView::new(&record))
        );
    }
}

// The removal form: the evidence that moving a filter above aggregation is unsound, which is why R5b suppresses at
// emission instead (amendment 76). Each pair of plans differs only in where the filter is, and the interpreter's
// outputs differ.

const B: u64 = 1_700_000_000;

fn at(relative: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(B + relative)
}

fn src() -> NodeId {
    "src".parse().unwrap()
}

/// The filter below aggregation, as written, or moved above it.
fn filtered(above: bool, context_limit: u64) -> Plan {
    let nodes = vec![
        dogstatsd(),
        node(
            "agg",
            names::AGGREGATE,
            aggregate_params("10s", context_limit, "5m"),
            true,
        ),
        split("f", json!({"name": {"eq": "x"}}), true),
        drop("f_drop", true),
        emit("e", "o0"),
    ];
    let edges: &[&str] = if above {
        &[
            "src.metrics -> f.in",
            "f.true -> f_drop.in",
            "f.false -> agg.in",
            "agg.out -> policy/e.in",
        ]
    } else {
        &[
            "src.metrics -> agg.in",
            "agg.out -> f.in",
            "f.true -> f_drop.in",
            "f.false -> policy/e.in",
        ]
    };
    plan(nodes, edges, &["o0"])
}

/// Runs a trace after installing `plan`, with a heartbeat from every source at each tick, and returns what `o0`
/// received, each record as `name kind [bucket:value, ...]` with buckets relative to `B`, sorted.
fn delivered(plan: Plan, trace: Trace) -> Vec<String> {
    let catalog = harness::catalog();
    let mut events = Trace::new().install(plan);
    events.extend(trace.events().iter().cloned());
    let run: RunOutput = Interpreter::new(registry(), &catalog)
        .with_heartbeats_at_ticks()
        .run(&events)
        .unwrap();
    let mut records = run
        .output(&OutputId::new("o0").unwrap())
        .iter()
        .map(|record| {
            let (kind, points) = match record.metric.values() {
                MetricValues::Rate(points, _) => ("rate", points),
                MetricValues::Gauge(points) => ("gauge", points),
                other => panic!("unexpected values {:?}", other),
            };
            let mut points = points
                .into_iter()
                .map(|(ts, value)| format!("{}:{}", ts.map_or(0, |ts| ts.get() - B), value))
                .collect::<Vec<_>>();
            points.sort();
            format!("{} {} [{}]", record.metric.context().name(), kind, points.join(", "))
        })
        .collect::<Vec<_>>();
    records.sort();
    records
}

fn gauge(name: &'static str) -> Metric {
    Metric::gauge(Context::from_static_parts(name, &[]), 1.0)
}

fn counter(name: &'static str) -> Metric {
    Metric::counter(Context::from_static_parts(name, &[]), 1.0)
}

#[test]
fn moving_a_filter_above_first_come_aggregation_changes_what_capacity_admits() {
    // A limit of one context. As written, the gauge `x` takes the context at 1, so the counter `b` is dropped at 2;
    // the flush at 15 removes `x`, which the filter then drops, so `d` is admitted at 16 and delivered. With the filter
    // above, `b` takes the context instead and holds it as an idle counter for its 5-minute expiry, so `d` is dropped:
    // a policy-included contribution loses more to capacity, which the capacity rule forbids.
    let trace = Trace::new()
        .admit(src(), at(1), vec![gauge("x")])
        .admit(src(), at(2), vec![counter("b")])
        .advance(at(15))
        .admit(src(), at(16), vec![gauge("d")])
        .advance(at(30));
    assert_eq!(delivered(filtered(false, 1), trace.clone()), ["d gauge [10:1]"]);
    assert_eq!(
        delivered(filtered(true, 1), trace),
        ["b rate [0:1]", "b rate [10:0, 20:0]"]
    );
}

#[test]
fn moving_a_filter_above_aggregation_changes_the_zeros_idle_counters_back_fill() {
    // Below capacity. As written, the filtered counter `x` keeps aggregation's state non-empty, so every tick is a
    // flush, and the counter `c`, first seen at 63, back-fills from the flush at 60: no leading zeros. With the filter
    // above, nothing is held after the flush at 15, the ticks at 30, 45, and 60 do nothing, and `c` back-fills zeros
    // from the flush at 15.
    let trace = Trace::new()
        .admit(src(), at(5), vec![counter("x"), gauge("g")])
        .advance(at(15))
        .advance(at(30))
        .advance(at(45))
        .advance(at(60))
        .admit(src(), at(63), vec![counter("c")])
        .advance(at(75));
    assert_eq!(
        delivered(filtered(false, 1_000), trace.clone()),
        ["c rate [60:1]", "g gauge [0:1]"]
    );
    assert_eq!(
        delivered(filtered(true, 1_000), trace),
        ["c rate [10:0, 20:0, 30:0, 40:0, 50:0, 60:1]", "g gauge [0:1]"]
    );
}
