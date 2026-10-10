//! The telemetry of the plan-driven topology against the legacy one's: the native metrics each fused component
//! reported, and what the Datadog Agent and its compatibility endpoint see of them through ADP's remappings.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric},
        Event,
    },
    data_model::tags::{Tag, TagSet},
    observability::metrics::{
        AggregatedMetricsProcessor, MetricsSnapshot, Processor as _, RemapperRule, TelemetryProcessor,
    },
    support::SubsystemIdentifier,
};
use saluki_metric_plan::{
    compile::{RewriteId, RewriteOutcome, RewriteRecord},
    ir::NodeId,
};
use saluki_metrics::test::TestRecorder;

use super::{
    CutOutcome, InstallerTelemetry, PLAN_CUTS_TOTAL, PLAN_CUT_CANCELLATIONS_REFUSED_TOTAL, PLAN_REWRITES_APPLIED,
};
use crate::{
    programmable_metrics::{
        telemetry_profiles::TelemetryProfile,
        test_support::{configuration, layered_configuration_system, programmable_metrics, ConfigCell},
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

/// Runs `path` with the configuration `config` over the batches with `recorder` as the metrics recorder, and returns
/// every series it registered, with its value.
async fn telemetry(path: MetricsPath, config: &str) -> BTreeMap<Series, String> {
    telemetry_with(Options::new(path, Aggregation::Clocked), config).await
}

/// Runs a topology started with `options` as [`telemetry`] does.
async fn telemetry_with(options: Options, config: &str) -> BTreeMap<Series, String> {
    let recorder = TestRecorder::default();
    {
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let system = layered_configuration_system(&[config]).await;
        let topology = TestTopology::start(&system, options, |_| {}).await;
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

/// Returns everything the topology reports about the transform `id` as a component of its own, with the given outputs:
/// its interconnects, its health, and its task.
fn component_series(id: &str, outputs: &[&str]) -> BTreeSet<Series> {
    let mut found = BTreeSet::new();
    let component = format!("component_id:{}", id);
    let labels = [component.as_str(), "component_type:transform"];
    found.insert(series("component_events_received_total", &labels));
    found.insert(series("component_events_received_size", &labels));
    for output in outputs {
        for name in [
            "component_events_sent_total",
            "component_send_latency_seconds",
            "component_events_discarded_total",
        ] {
            found.insert(series(name, &[labels[0], labels[1], output]));
        }
    }
    let identity = format!("topology.primary.transforms.{}", id);
    for name in [
        "health_component_live",
        "health_component_ready",
        "health_component_liveness_latency_seconds",
    ] {
        found.insert(series(name, &[format!("component_id:{}", identity).as_str()]));
    }
    for task in [identity.clone(), format!("{}.transform", identity)] {
        for name in ["runtime_task_poll_count", "runtime_task_poll_duration_seconds"] {
            found.insert(series(name, &[format!("task_name:{}", task).as_str()]));
        }
    }
    found
}

/// The series of the legacy topology that the plan-driven one does not report, written by hand: everything the
/// topology reports about a fused transform as a component of its own (its interconnects, its health, its task), the
/// one output `metrics_enrich` and `dsd_agg` had before each had one per exit, and the no-aggregation split's
/// passthrough batcher, which the plan does not have.
fn removed_series() -> BTreeSet<Series> {
    let mut removed = BTreeSet::new();
    for id in FUSED {
        let outputs: &[&str] = match id {
            "dsd_no_agg_split" => &["output:_default", "output:passthrough"],
            _ => &["output:_default"],
        };
        removed.extend(component_series(id, outputs));
    }
    for name in [
        "component_events_sent_total",
        "component_send_latency_seconds",
        "component_events_discarded_total",
    ] {
        for component in ["component_id:metrics_enrich", "component_id:dsd_agg"] {
            removed.insert(series(
                name,
                &[component, "component_type:transform", "output:_default"],
            ));
        }
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
/// about each of the regions `new_regions` as a component, the outputs (one per exit) of the regions that keep a
/// native ID, enrichment's and aggregation's, and the region hosts' failure counters, the plan-level metrics, the
/// installer's task, the snapshot bridge's task, and the heartbeat task of the admission point that feeds aggregation's
/// state.
fn expected_addition(new_regions: &[&str], (name, tags): &Series) -> bool {
    let has = |tag: &str| tags.iter().any(|candidate| candidate == tag);
    let region = new_regions
        .iter()
        .chain(&["metrics_enrich", "dsd_agg"])
        .any(|region| has(&format!("component_id:{}", region)));
    let new_component = new_regions.iter().any(|region| {
        let identity = format!("topology.primary.transforms.{}", region);
        has(&format!("component_id:{}", identity))
            || has(&format!("task_name:{}", identity))
            || has(&format!("task_name:{}.transform", identity))
    });
    name.starts_with("plan_")
        || (region && name.starts_with("component_"))
        || new_component
        || has("task_name:test_topology.programmable_metrics_installer")
        || has("task_name:test_topology.programmable_metrics_snapshot_bridge")
        || has("task_name:topology.primary.sources.dsd_in.admission_heartbeat")
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
    let legacy = telemetry(MetricsPath::Legacy, CONFIG).await;
    let plan = telemetry(MetricsPath::Plan, CONFIG).await;

    let removed = legacy
        .keys()
        .filter(|series| !plan.contains_key(*series))
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(removed, removed_series());
    let unexpected = plan
        .keys()
        .filter(|series| !legacy.contains_key(*series) && !expected_addition(&NEW_REGIONS, series))
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

/// The configuration of the gateway telemetry test: two endpoint routing groups (one of prefixes, which compact, and
/// one of names, which repeat) and both failover gateways, with nothing for the DogStatsD transforms to do.
const GATEWAYS: &str = r#"
aggregate_passthrough_idle_flush_timeout: 0s
dd_url: https://primary.example.com
additional_endpoints:
  https://b.example.com: [key-b]
  https://c.example.com: [key-c]
experimental:
  metrics_endpoint_routing:
    metric_allowlist:
      https://b.example.com: [orders.total, checkout.cart, orders.total]
    metric_prefix_allowlist:
      https://c.example.com: [raw., raw.requests.]
multi_region_failover:
  enabled: true
  api_key: mrf-key
  dd_url: https://mrf.example.com
  failover_metrics: true
  metric_allowlist: [orders.total]
autoscaling:
  failover:
    enabled: true
    metrics: [orders.total]
cluster_agent:
  enabled: true
  url: https://cluster-agent.example.com:5005
  auth_token: cluster-agent-token
"#;

/// The gateways of [`GATEWAYS`]: the routing groups are ordered by their lists, so the one of prefixes is the first.
const GATEWAY_COMPONENTS: [&str; 4] = [
    "metrics_routing_filter_0",
    "metrics_routing_filter_1",
    "mrf_metrics_gateway",
    "af_metrics_gateway",
];

/// The regions the plan-driven topology runs for [`GATEWAYS`], besides `metrics_enrich`: the policy fan-out and the
/// four gateways are one decision, which runs in the enrichment region, and whose five-way fan-out keeps enrichment
/// from the post-aggregation filter.
const GATEWAY_REGIONS: [&str; 2] = ["dsd_pre_aggregation", "dsd_post_agg_filter_region"];

#[tokio::test]
async fn the_gateways_keep_their_native_metrics_and_lose_only_their_component_series() {
    let legacy = telemetry(MetricsPath::Legacy, GATEWAYS).await;
    let plan = telemetry(MetricsPath::Plan, GATEWAYS).await;

    // Removed: the DogStatsD transforms' component series, as above, and the gateways', each a transform with one
    // output.
    let removed = legacy
        .keys()
        .filter(|series| !plan.contains_key(*series))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut expected = removed_series();
    for gateway in GATEWAY_COMPONENTS {
        expected.extend(component_series(gateway, &["output:_default"]));
    }
    assert_eq!(removed, expected);
    let unexpected = plan
        .keys()
        .filter(|series| !legacy.contains_key(*series) && !expected_addition(&GATEWAY_REGIONS, series))
        .collect::<Vec<_>>();
    assert!(unexpected.is_empty(), "unexpected series: {:?}", unexpected);
    // Each of those regions runs, and none other: no gateway has a region of its own.
    for region in GATEWAY_REGIONS {
        let health = format!("component_id:topology.primary.transforms.{}", region);
        assert!(
            plan.contains_key(&series("health_component_live", &[health.as_str()])),
            "{}",
            region
        );
    }

    // Every other series of a gateway keeps its name, labels, and value.
    let mut kept = 0;
    for (series, legacy_value) in &legacy {
        let gateway = component_id(series).is_some_and(|id| GATEWAY_COMPONENTS.contains(&id));
        if gateway && !removed.contains(series) {
            assert_eq!(plan[series], *legacy_value, "{:?}", series);
            kept += 1;
        }
    }
    // The routing filters' counts of what they drop and sizes of their lists; the failover gateways report none.
    assert_eq!(kept, 6);

    // The values, written by hand: the 12 metrics reaching the gateways are the 9 that aggregation flushes and the 3
    // timestamped ones. The prefix group keeps the 2 `raw.` counters (`raw.requests.` is covered by `raw.`); the name
    // group keeps `checkout.cart` and both `orders.total` points (its names are kept once each).
    for (gateway, filtered, exact, prefix) in [
        ("metrics_routing_filter_0", 10, 0, 1),
        ("metrics_routing_filter_1", 9, 2, 0),
    ] {
        let labels = [
            format!("component_id:{}", gateway),
            "component_type:transform".to_string(),
        ];
        let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            value(&plan, "metrics_endpoint_allowlist_metrics_filtered_total", &labels),
            format!("counter {}", filtered),
            "{}",
            gateway
        );
        for (match_type, expected) in [("match_type:exact", exact), ("match_type:prefix", prefix)] {
            let tags = [labels[0], labels[1], match_type];
            assert_eq!(
                value(&plan, "metrics_endpoint_allowlist_size", &tags),
                format!("gauge {}", expected),
                "{}: {}",
                gateway,
                match_type
            );
        }
    }

    // Either consumer of remapped telemetry sees what it saw, but the passthrough flushes, whose native count depends
    // on timing (see above).
    let without_flushes = |rendered: String| {
        rendered
            .lines()
            .filter(|line| !line.starts_with("no_aggregation__flush "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        without_flushes(render(get_datadog_agent_remappings(), &plan)),
        without_flushes(render(get_datadog_agent_remappings(), &legacy))
    );
    assert_eq!(
        render(get_compat_remappings(), &plan),
        render(get_compat_remappings(), &legacy)
    );
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
    let legacy = telemetry(MetricsPath::Legacy, CONFIG).await;
    let plan = telemetry(MetricsPath::Plan, CONFIG).await;

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

#[tokio::test]
async fn a_kernel_of_an_isolation_slot_named_after_its_node_is_accounted_in_its_regions_own_group_and_draws_on_state() {
    // How much a kernel's build allocates, at least, in the group its context names.
    const HELD: usize = 8 << 20;
    let registry = saluki_core::accounting::ComponentRegistry::default();
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let mut path = programmable_metrics(&config, cell.live(), &registry).expect("a metrics pipeline is required");
    let boot = path.boot_topology();
    // The `metrics_enrich` slot is named after the node of its one kernel, as the `dsd_agg` isolation slot is after
    // aggregation's: run as one, the kernel stands for the native component, as aggregation's does in `dsd_agg`.
    let slot = boot
        .frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == "metrics_enrich")
        .expect("the frame has the `metrics_enrich` slot")
        .id;
    let program = Arc::clone(&boot.programs[slot.get()]);
    let node = "metrics_enrich".parse::<NodeId>().unwrap();
    let region = "topology.primary.transforms.metrics_enrich".to_string();
    let own = format!("{}.metrics_enrich", region);

    // (the name of the isolation slot the program runs in, if it runs in one; the group its kernel is accounted in;
    // whether the kernel draws on the budgets of state)
    let cases = [
        (None, &own, false),
        (Some("metrics_enrich"), &region, true),
        (Some("metrics_enrich_region"), &own, true),
    ];
    for (isolation, group, stateful) in cases {
        let what = format!("isolation slot {:?}", isolation);
        let mut telemetry = boot.telemetry.clone();
        telemetry.isolation = isolation.map(|name| (slot, name.to_string())).into_iter().collect();
        let context = telemetry.kernel_context(&program, &node);
        match context.state_budget() {
            Some(budget) => {
                assert!(stateful, "{}", what);
                assert!(budget.global.shares_with(boot.state_budgets.global()), "{}", what);
                assert!(budget.node.shares_with(&boot.state_budgets.node(&node)), "{}", what);
            }
            None => assert!(!stateful, "{}", what),
        }
        let token = context.resource_group().expect("kernels are accounted");
        let before = memory_status(&registry).get(group).map_or(0, |(_, _, live)| *live);
        let held = {
            let _entered = token.enter();
            vec![1u8; HELD]
        };
        let after = memory_status(&registry).get(group).map_or(0, |(_, _, live)| *live);
        assert!(
            after >= before + HELD as u64,
            "{}: `{}` holds {} bytes, {} before",
            what,
            group,
            after,
            before
        );
        drop(held);
    }
}

#[test]
fn the_installer_counts_the_rewrites_applied_to_the_active_generation_by_rewrite() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let applied = |rewrite: &'static str| {
        recorder.gauge(metrics::Key::from_parts(PLAN_REWRITES_APPLIED, &[("rewrite", rewrite)]))
    };
    let record = |id, outcome| RewriteRecord {
        id,
        outcome,
        nodes: Vec::new(),
        justification: String::new(),
        telemetry: Vec::new(),
        capacity_rule: false,
    };
    let rewrites = ["R2", "R3", "R4", "R5a", "R6", "R7"];

    // Nothing is reported until the installer reports the rewrites of its first generation, as it does at boot; then
    // every rewrite is, at zero when none applies.
    let mut telemetry = InstallerTelemetry::new(SubsystemIdentifier::from_dotted("primary"));
    for rewrite in rewrites {
        assert_eq!(applied(rewrite), None, "{}", rewrite);
    }
    telemetry.rewrites(Some(&[]));
    for rewrite in rewrites {
        assert_eq!(applied(rewrite), Some(0.0), "{}", rewrite);
    }

    // Applications are counted per rewrite; skipped ones are not.
    let skipped = || RewriteOutcome::Skipped {
        reason: "the rewritten plan places work outside the frame".to_string(),
    };
    telemetry.rewrites(Some(&[
        record(RewriteId::R2, RewriteOutcome::Applied),
        record(RewriteId::R3, RewriteOutcome::Applied),
        record(RewriteId::R2, RewriteOutcome::Applied),
        record(RewriteId::R6, skipped()),
    ]));
    for (rewrite, expected) in rewrites.into_iter().zip([2.0, 1.0, 0.0, 0.0, 0.0, 0.0]) {
        assert_eq!(applied(rewrite), Some(expected), "{}", rewrite);
    }

    // A generation compiled with fewer rewrites, or none, resets the others.
    telemetry.rewrites(Some(&[record(RewriteId::R3, RewriteOutcome::Applied)]));
    assert_eq!((applied("R2"), applied("R3")), (Some(0.0), Some(1.0)));
    telemetry.rewrites(None);
    for rewrite in rewrites {
        assert_eq!(applied(rewrite), Some(0.0), "{}", rewrite);
    }
}

#[test]
fn the_installer_reports_every_cut_outcome_from_creation() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let telemetry = InstallerTelemetry::new(SubsystemIdentifier::from_segments(["primary"]));
    let cuts =
        |outcome: &'static str| recorder.counter(metrics::Key::from_parts(PLAN_CUTS_TOTAL, &[("outcome", outcome)]));
    let refused = || recorder.counter(metrics::Key::from_name(PLAN_CUT_CANCELLATIONS_REFUSED_TOTAL));
    for outcome in ["completed", "cancelled", "not_armed"] {
        assert_eq!(cuts(outcome), Some(0), "{}", outcome);
    }
    assert_eq!(refused(), Some(0));

    // A refused cancellation ends no cut: the cut it was refused for completes, and only that is counted as its end.
    telemetry.cut_cancellation_refused();
    telemetry.cut_cancellation_refused();
    telemetry.cut(CutOutcome::Completed);
    telemetry.cut(CutOutcome::NotArmed);
    assert_eq!(
        CutOutcome::ALL.map(|outcome| cuts(outcome.as_str())),
        [Some(1), Some(0), Some(1)]
    );
    assert_eq!(refused(), Some(2));
    assert_eq!(cuts("cancel_refused"), None, "a refusal is no outcome of a cut");
}

#[tokio::test]
async fn aggregation_in_its_region_keeps_the_native_aggregation_components_series_and_what_they_remap_to() {
    let legacy = telemetry(MetricsPath::Legacy, CONFIG).await;
    let options = Options::new(MetricsPath::Plan, Aggregation::Clocked);
    let plan = telemetry_with(options, CONFIG).await;

    // The aggregation kernel reports every series of the native component, with the native values.
    for metric in TelemetryProfile::Aggregation.metrics() {
        let mut labels = vec!["component_id:dsd_agg", "component_type:transform"];
        labels.extend(metric.tags);
        let series = series(metric.name, &labels);
        assert!(legacy.contains_key(&series), "native aggregation reports {:?}", series);
        assert_eq!(plan.get(&series), legacy.get(&series), "{:?}", series);
    }
    // So does every metric the Datadog Agent's `aggregator.*` metrics are remapped from, the post-aggregation filter's
    // among them: aggregation emits nothing the filter would drop (R5b), and counts what it suppressed as the filter's.
    let aggregator = |rendered: String| {
        rendered
            .lines()
            .filter(|line| line.starts_with("aggregator__"))
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let remapped = aggregator(render(get_datadog_agent_remappings(), &plan));
    assert!(!remapped.is_empty());
    assert_eq!(remapped, aggregator(render(get_datadog_agent_remappings(), &legacy)));

    // The region's one output is named after the component it feeds, where the native transform had a default one.
    let dsd_agg_outputs = |telemetry: &BTreeMap<Series, String>| {
        telemetry
            .keys()
            .filter(|(name, tags)| {
                name == "component_events_sent_total" && tags.iter().any(|tag| tag == "component_id:dsd_agg")
            })
            .flat_map(|(_, tags)| tags.iter().filter_map(|tag| tag.strip_prefix("output:")))
            .map(String::from)
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(dsd_agg_outputs(&legacy), ["_default".to_string()].into_iter().collect());
    assert_eq!(dsd_agg_outputs(&plan).len(), 1);
    assert!(!dsd_agg_outputs(&plan).contains("_default"));
}

/// Runs `path` on the connected configuration path with clocked aggregation: the DogStatsD source receives the gauge
/// `foo.max`, the gauge `kept`, and the counter `bar.avg`; then a live update sets the metric filterlist to `foo.max`
/// and `bar.avg`, and input ends, which flushes aggregation. Returns every counter, and the names the main output
/// received.
async fn filtered_live(path: MetricsPath) -> (BTreeMap<Series, u64>, Vec<String>) {
    use std::time::Duration;

    use serde_json::json;

    use crate::{
        internal::ConfigUpdatesWorker,
        programmable_metrics::test_support::{connected_configuration_system, update},
    };

    let recorder = TestRecorder::default();
    let deliveries = {
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let (system, updates, stream) = connected_configuration_system("").await;
        let topology = TestTopology::start(&system, Options::new(path, Aggregation::Clocked), |supervisor| {
            supervisor.add_worker(ConfigUpdatesWorker::new(updates))
        })
        .await;
        topology.feed(
            "dsd_in.metrics",
            vec![
                Metric::gauge(Context::from_static_name("foo.max"), 1.0),
                Metric::gauge(Context::from_static_name("kept"), 2.0),
                Metric::counter(Context::from_static_name("bar.avg"), 3.0),
            ],
        );
        let patience = tokio::time::Instant::now() + Duration::from_secs(30);
        while topology.sent("dsd_in.metrics") < 1 {
            assert!(
                tokio::time::Instant::now() < patience,
                "the source never sent its batch"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stream
            .send(update("metric_filterlist", json!(["foo.max", "bar.avg"])))
            .await
            .unwrap();
        // The configuration changes; the plan's installer then commits a generation that filters by it, and the
        // legacy filter reads it from its watch.
        while system.config().domains.dogstatsd.metric_filter.values.is_empty() {
            assert!(tokio::time::Instant::now() < patience, "the update never applied");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        match topology.status() {
            Some(status) => loop {
                let status = status.load();
                let committed = status
                    .installer
                    .active
                    .as_ref()
                    .is_some_and(|active| active.id.get() > 1);
                if committed && status.installer.preparing.is_none() && status.installer.pending.is_none() {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < patience,
                    "the change never committed: {}",
                    status
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            },
            None => tokio::time::sleep(Duration::from_millis(500)).await,
        }
        topology.stop().await
    };
    let names = deliveries
        .iter()
        .filter(|delivery| delivery.output == "dd_metrics_encode")
        .flat_map(|delivery| {
            delivery
                .metrics
                .iter()
                .map(|metric| metric.context().name().to_string())
        })
        .collect();
    let counters = recorder
        .counters()
        .into_iter()
        .map(|(key, value)| {
            let mut labels = key
                .labels()
                .map(|label| format!("{}:{}", label.key(), label.value()))
                .collect::<Vec<_>>();
            labels.sort();
            ((key.name().to_string(), labels), value)
        })
        .collect();
    (counters, names)
}

#[tokio::test]
async fn aggregated_metrics_a_live_filterlist_update_filters_count_in_the_post_aggregation_filters_series() {
    let (legacy, legacy_names) = filtered_live(MetricsPath::Legacy).await;
    let (plan, plan_names) = filtered_live(MetricsPath::Plan).await;
    let counter = |counters: &BTreeMap<Series, u64>, name: &str, tags: &[&str]| {
        counters
            .get(&series(name, tags))
            .copied()
            .unwrap_or_else(|| panic!("no series {} {:?}", name, tags))
    };

    // The filterlist names the gauge `foo.max` and the counter `bar.avg`, both kinds the post-aggregation filter judges
    // whatever their name says: only `kept` reaches the main output.
    assert_eq!(legacy_names, ["kept"]);
    assert_eq!(plan_names, ["kept"]);

    // The filter's native counter counts both, as natively.
    let filtered = |counters: &BTreeMap<Series, u64>| {
        counter(
            counters,
            "dogstatsd_post_aggregate_filtered_metrics_total",
            &["component_id:dsd_post_agg_filter", "component_type:transform"],
        )
    };
    assert_eq!(filtered(&legacy), 2);
    assert_eq!(filtered(&plan), 2);

    // Aggregation emitted only `kept`: it suppressed the two records the filter drops (R5b), under the live binding's
    // new value, and counted them as the filter's: what its `true` port sent, and its drop's policy loss.
    assert_eq!(
        counter(&plan, "plan_node_events_total", &["component_id:dsd_agg", "port:out"]),
        1
    );
    assert_eq!(
        counter(
            &plan,
            "plan_node_events_total",
            &["component_id:dsd_post_agg_filter", "port:true"]
        ),
        2
    );
    assert_eq!(
        counter(
            &plan,
            "plan_loss_total",
            &["class:policy", "component_id:dsd_post_agg_filter_true"]
        ),
        2
    );
}
