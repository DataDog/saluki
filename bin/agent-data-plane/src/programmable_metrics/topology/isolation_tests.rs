//! The admission points of the plan-driven topology as cuts and state owners need them, and DogStatsD aggregation
//! running as a stateful kernel in its own region.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use saluki_common::resource_tracking::{ResourceGroupRegistry, ResourceStatsSnapshot};
use saluki_core::{
    accounting::{ComponentBounds, ComponentRegistry},
    components::ComponentType,
    data_model::event::metric::{context::Context, Metric},
    execution::{AdmissionPointId, AdmissionRegistry},
    health::HealthRegistry,
    topology::BlueprintSnapshot,
};
use saluki_metric_plan::{
    compile::Frame,
    limits,
    operators::{names, OperatorRegistry},
};
use serde_json::Value;

use super::aggregation_slot;
use crate::{
    cli::run::{create_legacy_topology, create_topology},
    config::remote_agent_client_configuration,
    programmable_metrics::{
        test_support::{
            configuration, configuration_system, environment_provider, programmable_metrics_with_operators, ConfigCell,
        },
        test_topology::{Aggregation, MetricsPath, Options, TestTopology, DSD_AGG, PATIENCE},
    },
};

/// The resource group, and the health registration, of DogStatsD aggregation's native component.
const DSD_AGG_COMPONENT: &str = "topology.primary.transforms.dsd_agg";

/// The inputs the sizing guide's page sets from the cardinality it asks for (see `sizing_guide_template.html`).
const CARDINALITY_INPUTS: [&str; 2] = [
    "dsd_agg.aggregate_context_limit",
    "dsd_in.dogstatsd_string_interner_size_bytes",
];

/// What a test keeps of the plan-driven metrics path it starts: the frame, and the registry its points register in.
type Captured = Arc<Mutex<Option<(Arc<Frame>, AdmissionRegistry)>>>;

/// Starts the plan-driven topology of `yaml` with `options`, returning it with its frame and admission registry, once
/// every admission point of the frame is registered.
async fn start(yaml: &str, mut options: Options) -> (TestTopology, Arc<Frame>, AdmissionRegistry) {
    let system = configuration_system(yaml).await;
    let captured: Captured = Arc::default();
    let capture = Arc::clone(&captured);
    options.before_topology = Some(Box::new(move |boot| {
        *capture.lock().unwrap() = Some((Arc::clone(&boot.frame), boot.admission.clone()));
    }));
    let topology = TestTopology::start(&system, options, |_| {}).await;
    let (frame, registry) = captured.lock().unwrap().take().expect("the topology was built");
    let points = frame
        .admission_points()
        .iter()
        .map(|point| point.id)
        .collect::<Vec<_>>();
    tokio::time::timeout(PATIENCE, async {
        while registry.ids() != points {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the points {:?} register; registered: {:?}", points, registry.ids()));
    (topology, frame, registry)
}

/// Returns the heartbeat period and recipients of every registered admission point.
fn heartbeats(registry: &AdmissionRegistry) -> Vec<(AdmissionPointId, Option<Duration>, Vec<String>)> {
    registry
        .ids()
        .into_iter()
        .map(|id| {
            let point = registry.get(id).expect("the point is registered");
            let recipients = point.heartbeat_recipients().iter().map(ToString::to_string).collect();
            (id, point.heartbeat_period(), recipients)
        })
        .collect()
}

fn edges(snapshot: &BlueprintSnapshot) -> BTreeSet<(String, String)> {
    snapshot
        .edges()
        .iter()
        .map(|edge| (edge.from().to_string(), edge.to().to_string()))
        .collect()
}

#[tokio::test]
async fn every_admission_point_registers_for_cuts_and_none_sends_heartbeats_without_a_state_owner() {
    // Every source of metrics but DogStatsD, whose aggregation is the one state owner.
    let yaml = "data_plane: {dogstatsd: {enabled: false}, otlp: {enabled: true}, checks: {enabled: true}}";
    let (topology, frame, registry) = start(yaml, Options::new(MetricsPath::Plan, Aggregation::Native)).await;
    assert!(frame.admission_points().len() >= 3, "{:?}", frame.admission_points());
    assert!(aggregation_slot(&frame).is_none());
    for (id, period, recipients) in heartbeats(&registry) {
        assert_eq!((period, recipients.len()), (None, 0), "point {}", id.get());
    }
    topology.stop().await;

    // The registry does not keep the points of a stopped topology.
    assert!(registry.ids().is_empty());
}

#[tokio::test]
async fn aggregation_runs_as_a_kernel_in_the_dsd_agg_region_whose_upstream_point_heartbeats_only_toward_it() {
    let options = Options::new(MetricsPath::Plan, Aggregation::Native);
    let (topology, frame, registry) = start("data_plane: {otlp: {enabled: true}}", options).await;

    // Aggregation is the `dsd_agg` region, a transform with the native component's ID; no fixed component runs it.
    let slot = aggregation_slot(&frame).expect("aggregation runs in an isolation slot");
    assert_eq!(slot.name.as_str(), "dsd_agg");
    assert!(frame
        .fixed()
        .iter()
        .all(|fixed| fixed.operator.as_str() == names::SOURCE));
    let component = topology
        .snapshot()
        .components()
        .iter()
        .find(|component| &**component.id() == "dsd_agg")
        .expect("the topology has `dsd_agg`");
    assert_eq!(component.component_type(), ComponentType::Transform);
    let edges = edges(topology.snapshot());
    assert!(
        edges.contains(&("dsd_pre_aggregation.dsd_agg".to_string(), "dsd_agg".to_string())),
        "{:?}",
        edges
    );
    assert!(
        edges.iter().any(|(from, _)| from.starts_with("dsd_agg.")),
        "{:?}",
        edges
    );

    // Only the DogStatsD source's point feeds a state owner: it heartbeats every second (the default flush interval is
    // longer), toward the region that leads to aggregation, and not toward the DogStatsD taps.
    let dogstatsd = frame
        .admission_points()
        .iter()
        .find(|point| point.node.to_string() == "dsd_in")
        .expect("the frame admits DogStatsD metrics");
    for (id, period, recipients) in heartbeats(&registry) {
        if id == dogstatsd.id {
            assert_eq!(period, Some(limits::values::HEARTBEAT_PERIOD));
            assert_eq!(recipients, ["dsd_pre_aggregation"]);
        } else {
            assert_eq!((period, recipients.len()), (None, 0), "point {}", id.get());
        }
    }
    topology.stop().await;
}

#[tokio::test]
async fn aggregations_kernel_draws_on_the_shared_budget_of_state_sized_by_the_native_context_limit() {
    let config = configuration("aggregate_context_limit: 1234").await;
    let cell = ConfigCell::new(config.clone());
    let mut path = programmable_metrics_with_operators(
        &config,
        cell.live(),
        &ComponentRegistry::default(),
        OperatorRegistry::native(),
    )
    .expect("a metrics pipeline is required");
    let boot = path.boot_topology();
    assert_eq!(boot.state_budgets.global().capacity(), 1_234);
    let slot = aggregation_slot(&boot.frame)
        .expect("aggregation runs in an isolation slot")
        .id;
    let program = &boot.programs[slot.get()];
    let node = "dsd_agg".parse().unwrap();
    let context = boot.telemetry.kernel_context(program, &node);
    let budget = context
        .state_budget()
        .expect("a state owner's kernel draws on the budgets of state");
    assert!(budget.global.shares_with(boot.state_budgets.global()));
    assert!(budget.node.shares_with(&boot.state_budgets.node(&node)));

    // No kernel of a fused region draws on the budgets: only isolation slots run state owners.
    let mut kernels = 0;
    for fused in boot.programs.iter().filter(|program| program.slot() != slot) {
        for kernel in fused.kernels() {
            let context = boot.telemetry.kernel_context(fused, &kernel.key.node);
            assert!(context.state_budget().is_none(), "`{}`", kernel.key.node);
            kernels += 1;
        }
    }
    assert!(kernels > 0, "fused regions have kernels");
}

/// Builds the legacy topology of `yaml` and the plan-driven one, whose metrics path runs `operators`, and returns the
/// memory bounds each declares: the legacy one's first.
async fn bounds(yaml: &str, operators: OperatorRegistry) -> (ComponentBounds, ComponentBounds) {
    let system = configuration_system(yaml).await;
    let config = system.config();
    let env = environment_provider(&config).await;
    let client = (!config.control.standalone_mode).then(|| remote_agent_client_configuration(&config).unwrap());
    let legacy = ComponentRegistry::default();
    let _legacy_topology = create_legacy_topology(&system, client.as_ref(), &env, &legacy)
        .await
        .expect("the legacy topology builds");
    let plan = ComponentRegistry::default();
    let cell = ConfigCell::new(config.as_ref().clone());
    let mut metrics = programmable_metrics_with_operators(&config, cell.live(), &plan, operators)
        .expect("a metrics pipeline is required");
    let _plan_topology = create_topology(&system, &env, &plan, Some(&mut metrics))
        .await
        .expect("the plan-driven topology builds");
    (legacy.as_bounds(), plan.as_bounds())
}

/// Returns the configuration inputs the sizing guide offers for `bounds`, derived as its page derives them: for every
/// configuration value a bound expression reads, `<component>.<value>`, where the component is the path of the
/// expression's component below `root.topology.<topology>.<kind>`.
fn sizing_guide_inputs(bounds: &ComponentBounds) -> BTreeSet<String> {
    fn values(expr: &Value, into: &mut Vec<String>) {
        match expr["type"].as_str() {
            Some("Config") => into.push(expr["name"].as_str().expect("a value has a name").to_string()),
            Some("Sum" | "Product") => {
                for expr in expr["values"].as_array().expect("an operation has operands") {
                    values(expr, into);
                }
            }
            _ => {}
        }
    }
    let mut inputs = BTreeSet::new();
    for entry in bounds.to_exprs() {
        let path = entry["name"]
            .as_str()
            .expect("a bound has a name")
            .split('.')
            .collect::<Vec<_>>();
        // `root.topology.<topology>.<kind>.<component path>.<min or firm>`
        let Some(component) = path.get(4..path.len() - 1) else {
            continue;
        };
        let mut read = Vec::new();
        values(&entry["expr"], &mut read);
        let component = component.join(".");
        inputs.extend(read.into_iter().map(|value| format!("{}.{}", component, value)));
    }
    inputs
}

/// Returns every bound expression `bounds` declares for DogStatsD aggregation's native component itself that reads its
/// context limit, as `min` or `firm` with the expression, sorted.
fn aggregation_terms(bounds: &ComponentBounds) -> Vec<String> {
    let own = format!("root.{}.", DSD_AGG_COMPONENT);
    let mut terms = bounds
        .to_exprs()
        .into_iter()
        .filter(|entry| entry["expr"].to_string().contains("aggregate_context_limit"))
        .filter_map(|entry| {
            let name = entry["name"].as_str()?;
            let bound = name.strip_prefix(&own).filter(|bound| !bound.contains('.'))?;
            Some(format!("{} {}", bound, entry["expr"]))
        })
        .collect::<Vec<_>>();
    terms.sort();
    terms
}

/// Checks that the inputs the sizing guide's page sets from the cardinality resolve on the plan-driven topology whose
/// metrics path runs `operators`, and that aggregation's and the DogStatsD source's inputs are those of the legacy
/// topology.
async fn check_sizing_guide_inputs(operators: OperatorRegistry) {
    let (legacy, plan) = bounds("", operators).await;
    let (legacy, plan) = (sizing_guide_inputs(&legacy), sizing_guide_inputs(&plan));
    for input in CARDINALITY_INPUTS {
        assert!(
            legacy.contains(input),
            "`{}` on the legacy topology: {:?}",
            input,
            legacy
        );
        assert!(
            plan.contains(input),
            "`{}` on the plan-driven topology: {:?}",
            input,
            plan
        );
    }
    let dogstatsd = |inputs: &BTreeSet<String>| {
        inputs
            .iter()
            .filter(|input| input.starts_with("dsd_agg.") || input.starts_with("dsd_in."))
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(dogstatsd(&plan), dogstatsd(&legacy));
}

#[tokio::test]
async fn the_sizing_guides_cardinality_inputs_resolve_with_aggregation_in_its_region_as_on_the_legacy_topology() {
    check_sizing_guide_inputs(OperatorRegistry::native()).await;
}

/// Returns whether each component registered with `health` is ready, by name.
fn readiness(health: &HealthRegistry) -> BTreeMap<String, bool> {
    let snapshot = serde_json::from_str::<Value>(&health.snapshot_json()).expect("the snapshot is JSON");
    snapshot
        .as_object()
        .expect("the snapshot is an object")
        .iter()
        .map(|(name, state)| (name.clone(), state["ready"].as_bool().unwrap_or(false)))
        .collect()
}

/// Returns the bytes live in every resource group registered in the process, by name.
fn live_bytes() -> BTreeMap<String, usize> {
    let empty = ResourceStatsSnapshot::empty();
    let mut groups = BTreeMap::new();
    ResourceGroupRegistry::global().visit_resource_groups(|name, stats| {
        groups.insert(name.to_string(), stats.snapshot_delta(&empty).live_bytes());
    });
    groups
}

#[tokio::test]
async fn aggregation_in_its_region_keeps_the_native_components_health_resource_group_and_bounds() {
    // The memory bounds: the region declares aggregation's state and snapshot terms at its own top level, as the native
    // component does, and nothing below it for the kernel, so that the sizing guide's input stays the native one.
    let (legacy, plan) = bounds("", OperatorRegistry::native()).await;
    let native = aggregation_terms(&legacy);
    assert!(
        !native.is_empty(),
        "the native component's bounds read its context limit"
    );
    assert_eq!(aggregation_terms(&plan), native);
    let kernel = format!("root.{}.{}.", DSD_AGG_COMPONENT, DSD_AGG);
    let below = plan
        .to_exprs()
        .into_iter()
        .filter_map(|entry| entry["name"].as_str().map(String::from))
        .filter(|name| name.starts_with(&kernel))
        .collect::<Vec<_>>();
    assert!(below.is_empty(), "bounds below the region: {:?}", below);

    // Health: the region registers as the native component, and is ready, as on the legacy topology.
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let options = Options::new(path, Aggregation::Native);
        let health = options.health.clone();
        let system = configuration_system("").await;
        let topology = TestTopology::start(&system, options, |_| {}).await;
        let metric = Metric::counter(Context::from_static_parts("requests", &[]), 1.0);
        topology.feed("dsd_in.metrics", vec![metric]);
        tokio::time::timeout(PATIENCE, async {
            while readiness(&health).get(DSD_AGG_COMPONENT) != Some(&true) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{:?}: `dsd_agg` is never ready: {:?}", path, readiness(&health)));

        // The resource group: the kernel's contexts are accounted in the region's own group, the native component's,
        // and in no group of their own below it.
        if path == MetricsPath::Plan {
            tokio::time::timeout(PATIENCE, async {
                while live_bytes().get(DSD_AGG_COMPONENT).copied().unwrap_or_default() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("nothing is accounted to `{}`: {:?}", DSD_AGG_COMPONENT, live_bytes()));
            let kernel_group = format!("{}.{}", DSD_AGG_COMPONENT, DSD_AGG);
            assert!(!live_bytes().contains_key(&kernel_group), "{:?}", live_bytes());
        }
        topology.stop().await;
    }
}

#[tokio::test]
async fn the_context_dump_api_lists_what_aggregation_holds_in_its_region_as_it_does_natively() {
    use saluki_components::transforms::AggregateMetricType;

    // On the plan-driven path the request travels as `run.rs` wires it: from the context dump API's handle, which the
    // topology created when it added the DogStatsD pipeline, through the snapshot bridge to the `dsd_agg` region, whose
    // aggregation kernel answers it.
    for path in [MetricsPath::Legacy, MetricsPath::Plan] {
        let system = configuration_system("").await;
        let topology = TestTopology::start(&system, Options::new(path, Aggregation::Native), |_| {}).await;
        topology.feed(
            "dsd_in.metrics",
            vec![
                Metric::gauge(Context::from_static_name("dump.gauge"), 1.0),
                Metric::counter(Context::from_static_name("dump.counter"), 1.0),
                Metric::histogram(Context::from_static_name("dump.histogram"), [1.0, 2.0]),
            ],
        );
        let patience = tokio::time::Instant::now() + PATIENCE;
        let contexts = loop {
            let snapshot = topology.context_dump().await.expect("aggregation answers the dump");
            let mut contexts = snapshot
                .iter()
                .map(|entry| (entry.context().name().to_string(), entry.metric_type()))
                .collect::<Vec<_>>();
            contexts.sort_by(|a, b| a.0.cmp(&b.0));
            if contexts.len() >= 3 || tokio::time::Instant::now() > patience {
                break contexts;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(
            contexts,
            [
                ("dump.counter".to_string(), AggregateMetricType::Counter),
                ("dump.gauge".to_string(), AggregateMetricType::Gauge),
                ("dump.histogram".to_string(), AggregateMetricType::Histogram),
            ],
            "{:?}",
            path
        );
        topology.stop().await;
    }
}
