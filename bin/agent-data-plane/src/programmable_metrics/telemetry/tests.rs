//! The telemetry of the plan-driven topology against the legacy one's: the native metrics each fused component
//! reported, and what the Datadog Agent and its compatibility endpoint see of them through ADP's remappings.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric},
        Event,
    },
    data_model::tags::{Tag, TagSet},
    observability::metrics::{
        AggregatedMetricsProcessor, MetricsSnapshot, Processor as _, RemapperRule, TelemetryProcessor,
    },
};
use saluki_metrics::test::TestRecorder;

use crate::{
    programmable_metrics::{
        test_support::layered_configuration_system,
        test_topology::{Aggregation, MetricsPath, Options, TestTopology},
    },
    state::metrics::{get_compat_remappings, get_datadog_agent_remappings},
};

/// A metric series: its name, and its labels as sorted `key:value` tags.
type Series = (String, Vec<String>);

/// The configuration both topologies run: every DogStatsD transform the regions fuse has work to do.
const CONFIG: &str = r#"
aggregate_passthrough_idle_flush_timeout: 0s
dogstatsd_mapper_profiles:
  - name: checkout
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: mapped.requests
metric_filterlist: [raw.blocked, request.duration.max]
metric_tag_filterlist:
  - metric_name: checkout.latency
    action: exclude
    tags: [user_id]
"#;

/// The DogStatsD batches: a mapped name, a blocked one, a tag the tag filterlist removes, a histogram statistic the
/// post-aggregation filter blocks, and timestamped counters in two of the three batches.
fn batches() -> Vec<Vec<Metric>> {
    let context = Context::from_static_parts;
    vec![
        vec![
            Metric::counter(context("raw.requests.checkout", &[]), 1.0),
            Metric::counter(context("raw.blocked", &[]), 1.0),
            Metric::distribution(context("checkout.latency", &["user_id:1", "env:dev"]), 2.0),
            Metric::histogram(context("request.duration", &[]), [3.0, 4.0]),
        ],
        vec![
            Metric::counter(context("orders.total", &[]), (1_700_000_000, 1.0)),
            Metric::counter(context("orders.count", &[]), (1_700_000_000, 2.0)),
        ],
        vec![
            Metric::counter(context("orders.total", &[]), (1_700_000_010, 1.0)),
            Metric::gauge(context("checkout.cart", &[]), 3.0),
        ],
    ]
}

/// Runs `path` over the batches with `recorder` as the metrics recorder, and returns every series it registered, with
/// its value.
async fn telemetry(path: MetricsPath) -> BTreeMap<Series, String> {
    let recorder = TestRecorder::default();
    {
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let system = layered_configuration_system(&[CONFIG]).await;
        let topology = TestTopology::start(&system, Options::new(path, Aggregation::Clocked), |_| {}).await;
        for batch in batches() {
            topology.feed("dsd_in.metrics", batch);
        }
        topology.stop().await;
    }
    let series = |key: &metrics::Key| {
        let mut labels = key
            .labels()
            .map(|label| format!("{}:{}", label.key(), label.value()))
            .collect::<Vec<_>>();
        labels.sort();
        (key.name().to_string(), labels)
    };
    let counters = recorder
        .counters()
        .into_iter()
        .map(|(key, value)| (series(&key), format!("counter {}", value)));
    let gauges = recorder
        .gauges()
        .into_iter()
        .map(|(key, value)| (series(&key), format!("gauge {}", value)));
    let histograms = recorder
        .histogram_keys()
        .into_iter()
        .map(|key| (series(&key), "histogram".to_string()));
    counters.chain(gauges).chain(histograms).collect()
}

/// Returns the `component_id` label of a series.
fn component_id(series: &Series) -> Option<&str> {
    series.1.iter().find_map(|tag| tag.strip_prefix("component_id:"))
}

/// Renders series through remapping rules, as ADP serves its telemetry to the Datadog Agent and on its compatibility
/// endpoint: every counter and gauge, named `adp.<name>` and tagged with its labels.
fn render(rules: Vec<RemapperRule>, series: &BTreeMap<Series, String>) -> String {
    let upserts = series
        .iter()
        .filter_map(|((name, tags), value)| {
            let context = Context::from_parts(
                format!("adp.{}", name),
                tags.iter().map(|tag| Tag::from(tag.clone())).collect::<TagSet>(),
            );
            let metric = match value.split_once(' ') {
                Some(("counter", value)) => Metric::counter(context, value.parse::<f64>().ok()?),
                Some(("gauge", value)) => Metric::gauge(context, value.parse::<f64>().ok()?),
                _ => return None,
            };
            Some(Event::Metric(metric))
        })
        .collect();
    let processor = AggregatedMetricsProcessor;
    let state = processor.build_initial_state();
    processor.process(
        MetricsSnapshot {
            upserts,
            evictions: Vec::new(),
        },
        &state,
    );
    TelemetryProcessor::new().with_remapper_rules(rules).process(&state)
}

/// The legacy transforms whose work the regions do, besides `metrics_enrich`, whose ID the enrichment region keeps.
const FUSED: [&str; 5] = [
    "dsd_enrich",
    "dsd_prefix_filter",
    "dsd_no_agg_split",
    "dsd_tag_filterlist",
    "dsd_post_agg_filter",
];

fn series(name: &str, tags: &[&str]) -> Series {
    let mut tags = tags.iter().map(ToString::to_string).collect::<Vec<_>>();
    tags.sort();
    (name.to_string(), tags)
}

/// The series of the legacy topology that the plan-driven one does not report, written by hand: everything the
/// topology reports about a fused transform as a component of its own (its interconnects, its health, its task), the
/// one output `metrics_enrich` had before it had one per exit, and the no-aggregation split's passthrough batcher,
/// which the plan does not have.
fn removed_series() -> BTreeSet<Series> {
    let mut removed = BTreeSet::new();
    for id in FUSED {
        let component = format!("component_id:{}", id);
        let labels = [component.as_str(), "component_type:transform"];
        removed.insert(series("component_events_received_total", &labels));
        removed.insert(series("component_events_received_size", &labels));
        let outputs: &[&str] = match id {
            "dsd_no_agg_split" => &["output:_default", "output:passthrough"],
            _ => &["output:_default"],
        };
        for output in outputs {
            for name in [
                "component_events_sent_total",
                "component_send_latency_seconds",
                "component_events_discarded_total",
            ] {
                removed.insert(series(name, &[labels[0], labels[1], output]));
            }
        }
        let identity = format!("topology.primary.transforms.{}", id);
        for name in [
            "health_component_live",
            "health_component_ready",
            "health_component_liveness_latency_seconds",
        ] {
            removed.insert(series(name, &[format!("component_id:{}", identity).as_str()]));
        }
        for task in [identity.clone(), format!("{}.transform", identity)] {
            for name in ["runtime_task_poll_count", "runtime_task_poll_duration_seconds"] {
                removed.insert(series(name, &[format!("task_name:{}", task).as_str()]));
            }
        }
    }
    for name in [
        "component_events_sent_total",
        "component_send_latency_seconds",
        "component_events_discarded_total",
    ] {
        removed.insert(series(
            name,
            &[
                "component_id:metrics_enrich",
                "component_type:transform",
                "output:_default",
            ],
        ));
    }
    removed.insert(series(
        "aggregate_passthrough_batch_duration_secs",
        &["component_id:dsd_no_agg_split", "component_type:transform"],
    ));
    removed.insert(series(
        "component_events_dropped_total",
        &[
            "component_id:dsd_no_agg_split",
            "component_type:transform",
            "drop_reason:buffer_full",
            "intentional:false",
        ],
    ));
    removed
}

/// The regions the plan-driven topology runs for [`CONFIG`]: its mapper profile makes the pre-aggregation chain costly
/// enough that R1's cost model pipelines it over two regions, the mapper's and the rest's, and the enrichment region
/// keeps the native `metrics_enrich` ID.
const NEW_REGIONS: [&str; 2] = ["dsd_pre_aggregation", "dsd_prefix_filter_region"];

/// Returns `true` if a series the plan-driven topology adds is one it is expected to add: what the topology reports
/// about each pre-aggregation region as a component, the enrichment region's outputs (one per exit) and the region
/// hosts' failure counters, the plan-level metrics, and the installer's task.
fn expected_addition((name, tags): &Series) -> bool {
    let has = |tag: &str| tags.iter().any(|candidate| candidate == tag);
    let region = NEW_REGIONS
        .iter()
        .chain(&["metrics_enrich"])
        .any(|region| has(&format!("component_id:{}", region)));
    let new_component = NEW_REGIONS.iter().any(|region| {
        let identity = format!("topology.primary.transforms.{}", region);
        has(&format!("component_id:{}", identity))
            || has(&format!("task_name:{}", identity))
            || has(&format!("task_name:{}.transform", identity))
    });
    name.starts_with("plan_")
        || (region && name.starts_with("component_"))
        || new_component
        || has("task_name:test_topology.programmable_metrics_installer")
}

/// Returns the value of a series, which must exist.
#[track_caller]
fn value<'a>(telemetry: &'a BTreeMap<Series, String>, name: &str, tags: &[&str]) -> &'a str {
    let series = series(name, tags);
    telemetry
        .get(&series)
        .unwrap_or_else(|| panic!("no series {:?}", series))
        .as_str()
}

#[tokio::test]
async fn the_fused_components_keep_their_native_metrics_and_lose_only_their_component_series() {
    let legacy = telemetry(MetricsPath::Legacy).await;
    let plan = telemetry(MetricsPath::Plan).await;

    let removed = legacy
        .keys()
        .filter(|series| !plan.contains_key(*series))
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(removed, removed_series());
    let unexpected = plan
        .keys()
        .filter(|series| !legacy.contains_key(*series) && !expected_addition(series))
        .collect::<Vec<_>>();
    assert!(unexpected.is_empty(), "unexpected series: {:?}", unexpected);

    // Every native metric the fused components report keeps its name, its `component_id`, and its value, with two
    // exceptions, written by hand: the timestamp split counts one passthrough flush per batch that has timestamped
    // metrics (the second and third), where the native batcher flushed once, or twice in a slow run; and the
    // enrichment region, which runs the post-aggregation filter, receives the statistic the filter blocks (10 events
    // reach the filter, 11 with it).
    for (series, legacy_value) in &legacy {
        let fused = series.1.iter().any(|tag| {
            tag.strip_prefix("component_id:")
                .is_some_and(|id| FUSED.contains(&id) || id == "metrics_enrich")
                || tag.starts_with("cache_id:")
        });
        if !fused || removed.contains(series) {
            continue;
        }
        let expected = match (series.0.as_str(), component_id(series)) {
            ("aggregate_passthrough_flushes_total", _) => {
                // The native batcher flushes when it has been idle at one of its checks, every two seconds, so a slow
                // run can see it flush the two batches apart.
                assert!(
                    ["counter 1", "counter 2"].contains(&legacy_value.as_str()),
                    "{}",
                    legacy_value
                );
                "counter 2"
            }
            ("component_events_received_total", Some("metrics_enrich")) => {
                assert_eq!(legacy_value, "counter 10");
                "counter 11"
            }
            _ => legacy_value.as_str(),
        };
        assert_eq!(plan[series], expected, "{:?}", series);
    }

    // The native metrics' values, written by hand from the input.
    let prefix_filter = ["component_id:dsd_prefix_filter", "component_type:transform"];
    let split = ["component_id:dsd_no_agg_split", "component_type:transform"];
    let tag_filterlist = ["component_id:dsd_tag_filterlist", "component_type:transform"];
    for (name, tags, expected) in [
        ("dogstatsd_listener_filtered_points_total", &prefix_filter, "counter 1"),
        ("metric_filterlist_size", &prefix_filter, "gauge 2"),
        ("metric_filterlist_updates_total", &prefix_filter, "counter 0"),
        ("aggregate_passthrough_metrics_total", &split, "counter 3"),
        ("aggregate_passthrough_flushes_total", &split, "counter 2"),
        ("tag_filterlist_tags_filtered_total", &tag_filterlist, "counter 1"),
        ("tag_filterlist_size", &tag_filterlist, "gauge 1"),
        ("tag_filterlist_updates_total", &tag_filterlist, "counter 0"),
        (
            "dogstatsd_post_aggregate_filtered_metrics_total",
            &["component_id:dsd_post_agg_filter", "component_type:transform"],
            "counter 1",
        ),
    ] {
        assert_eq!(value(&plan, name, tags), expected, "{}", name);
    }
}

/// Returns the remapped metric names in rendered telemetry.
fn rendered_names(rendered: &str) -> BTreeSet<String> {
    rendered
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split(['{', ' ']).next())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn the_datadog_agent_and_the_compatibility_endpoint_see_the_fused_components_as_before() {
    let legacy = telemetry(MetricsPath::Legacy).await;
    let plan = telemetry(MetricsPath::Plan).await;

    // Through the remappings for the Datadog Agent, the plan-driven topology reports every remapped metric the fused
    // components report natively, with the same values but the one the passthrough batching changes.
    let rendered = render(get_datadog_agent_remappings(), &plan);
    let names = rendered_names(&rendered);
    for name in [
        "filterlist__size",
        "filterlist__updates",
        "dogstatsd__listener_filtered_points",
        "aggregator__dogstatsd_filtered_metrics",
        "tag_filterlist__size",
        "tag_filterlist__updates",
        "aggregator__filtered_tags",
        "aggregator__filtered_tags_cache_hit",
        "aggregator__filtered_tags_cache_miss",
        "aggregator__filtered_tags_cache_evict",
        "no_aggregation__processed",
        "no_aggregation__flush",
    ] {
        assert!(names.contains(name), "`{}` is not reported:\n{}", name, rendered);
    }
    for line in [
        "filterlist__size 2",
        "dogstatsd__listener_filtered_points 1",
        "aggregator__dogstatsd_filtered_metrics 1",
        "aggregator__filtered_tags 1",
        "no_aggregation__processed{state=\"ok\"} 3",
        "no_aggregation__flush 2",
    ] {
        assert!(
            rendered.lines().any(|rendered| rendered == line),
            "`{}` is not reported:\n{}",
            line,
            rendered
        );
    }
    let native = render(get_datadog_agent_remappings(), &legacy);
    assert_eq!(
        rendered.replace("no_aggregation__flush 2", "no_aggregation__flush 1"),
        native,
        "the remapped telemetry differs"
    );

    // The compatibility endpoint renders the same, and no series the plan-driven topology no longer reports is
    // remapped by either rule set: nothing either consumer reads is lost.
    assert_eq!(
        render(get_compat_remappings(), &plan),
        render(get_compat_remappings(), &legacy)
    );
    let removed = legacy
        .iter()
        .filter(|(series, _)| !plan.contains_key(*series))
        .map(|(series, value)| (series.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    assert!(!removed.is_empty());
    for rules in [get_datadog_agent_remappings(), get_compat_remappings()] {
        let rendered = render(rules, &removed);
        assert!(
            rendered_names(&rendered).is_empty(),
            "removed series are remapped:\n{}",
            rendered
        );
    }
}

/// Returns the bounds and live bytes the registry reports for each resource group, as `/memory/status` does.
fn memory_status(registry: &saluki_core::accounting::ComponentRegistry) -> BTreeMap<String, (u64, u64, u64)> {
    let snapshot = serde_json::from_str::<serde_json::Value>(&registry.root().memory_snapshot_json())
        .expect("the memory snapshot is JSON");
    snapshot
        .as_object()
        .expect("the memory snapshot is an object")
        .iter()
        .map(|(group, usage)| {
            let bytes = |field: &str| usage[field].as_u64().expect("usage fields are byte counts");
            (
                group.clone(),
                (
                    bytes("minimum_required_bytes"),
                    bytes("firm_limit_bytes"),
                    bytes("actual_live_bytes"),
                ),
            )
        })
        .collect()
}

#[tokio::test]
async fn each_fused_kernel_is_accounted_in_a_resource_group_of_its_own_under_its_region() {
    let registry = saluki_core::accounting::ComponentRegistry::default();
    let system = layered_configuration_system(&[CONFIG]).await;
    let mut options = Options::new(MetricsPath::Plan, Aggregation::Clocked);
    options.registry = registry.clone();
    let mut topology = TestTopology::start(&system, options, |_| {}).await;
    for batch in batches() {
        topology.feed("dsd_in.metrics", batch);
    }
    // The timestamped counters pass aggregation, so they arrive once every batch went through the regions.
    topology
        .wait_for("the passthrough of every batch", |deliveries| {
            deliveries
                .iter()
                .filter(|delivery| delivery.output == "dd_metrics_encode")
                .flat_map(|delivery| &delivery.metrics)
                .filter(|metric| metric.context().name() == "orders.total")
                .count()
                == 2
        })
        .await;
    let status = memory_status(&registry);
    topology.stop().await;

    // Every kernel of a region has a group named after its node, under its region's, as the region's bounds declare
    // the kernel's bounds; what the kernel holds is accounted there.
    let group = |region: &str, node: &str| format!("topology.primary.transforms.{}.{}", region, node);
    let kernels = [
        ("dsd_pre_aggregation", "dsd_enrich"),
        ("dsd_prefix_filter_region", "dsd_prefix_filter"),
        ("dsd_prefix_filter_region", "dsd_no_agg_split"),
        ("dsd_prefix_filter_region", "dsd_tag_filterlist"),
        ("metrics_enrich", "metrics_enrich"),
    ];
    for (region, node) in kernels {
        assert!(
            status.contains_key(&group(region, node)),
            "no group for `{}`: {:?}",
            node,
            status.keys().collect::<Vec<_>>()
        );
    }
    // The tag filterlist's group compares with its own bounds, as the native component's did: a firm limit of 64
    // bytes for each of the 100,000 contexts its memo holds by default; and its memo and rules are accounted to it.
    let (minimum, firm, live) = status[&group("dsd_prefix_filter_region", "dsd_tag_filterlist")];
    assert_eq!((minimum, firm), (0, 100_000 * 64));
    assert!(live > 0, "the tag filterlist's kernel holds nothing");
    let (_, _, mapper_live) = status[&group("dsd_pre_aggregation", "dsd_enrich")];
    assert!(mapper_live > 0, "the mapper's kernel holds nothing");
    // Each region's own group is accounted separately, against the region's own bounds.
    for region in NEW_REGIONS {
        assert!(status.contains_key(&format!("topology.primary.transforms.{}", region)));
    }
}
