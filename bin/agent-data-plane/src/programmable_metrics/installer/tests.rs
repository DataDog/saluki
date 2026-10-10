//! The installer, driven by configuration changes: with the regions of a running topology, and with regions that
//! never answer or are gone; and the plan-driven metrics path running beside the legacy one on the same input.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use metrics::Key;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        sources::{Source, SourceBuilder, SourceContext},
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext,
    },
    data_model::event::{
        metric::{context::Context, Metric},
        Event, EventType,
    },
    execution::GenerationId,
    health::HealthRegistry,
    runtime::{state::ResourceRegistry, Supervisable, Supervisor},
    topology::{EventsBuffer, OutputDefinition, TopologyBlueprint},
};
use saluki_error::{generic_error, GenericError};
use saluki_metric_plan::{compile::RewriteId, install::GenerationState};
use saluki_metrics::test::TestRecorder;
use tokio::sync::{mpsc, oneshot};

use super::*;
use crate::{
    cli::run::{dogstatsd_aggregate_configuration, PRIMARY_TOPOLOGY},
    programmable_metrics::{
        telemetry::{PLAN_ACTIVE_GENERATION, PLAN_INSTALLS_TOTAL, PLAN_LIVE_GENERATIONS, PLAN_REWRITES_APPLIED},
        test_support::{configuration, programmable_metrics, ConfigCell},
        topology::{add_regions, connect_frame},
    },
};

/// The longest a test waits for the installer or the topology.
const PATIENCE: Duration = Duration::from_secs(30);

/// Waits until `status` satisfies `done`, returning it.
async fn status_where(status: &ArcSwap<PlanStatus>, done: impl Fn(&PlanStatus) -> bool) -> PlanStatus {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let current = status.load_full();
            if done(&current) {
                return PlanStatus::clone(&current);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the installer's status never changed as expected: {}", status.load()))
}

fn active(status: &PlanStatus) -> Option<u64> {
    status.installer.active.as_ref().map(|active| active.id.get())
}

fn live(status: &PlanStatus) -> Vec<u64> {
    status
        .installer
        .live
        .iter()
        .map(|generation| generation.get())
        .collect()
}

fn state_of(status: &PlanStatus, generation: u64) -> Option<GenerationState> {
    status
        .installer
        .generations
        .iter()
        .find(|record| record.id.get() == generation)
        .map(|record| record.state)
}

fn reason_of(status: &PlanStatus, generation: u64) -> Option<String> {
    status
        .installer
        .generations
        .iter()
        .find(|record| record.id.get() == generation)
        .and_then(|record| record.reason.clone())
}

/// A source with one metrics output, `metrics`, which sends each list of events it is given as one batch.
struct ScriptedSourceBuilder {
    batches: StdMutex<Option<mpsc::Receiver<Vec<Event>>>>,
    outputs: [OutputDefinition<EventType>; 1],
}

struct ScriptedSource {
    batches: mpsc::Receiver<Vec<Event>>,
}

#[async_trait]
impl SourceBuilder for ScriptedSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        let batches = self
            .batches
            .lock()
            .map_err(|_| generic_error!("poisoned"))?
            .take()
            .ok_or_else(|| generic_error!("the scripted source was already built"))?;
        Ok(Box::new(ScriptedSource { batches }))
    }
}

impl MemoryBounds for ScriptedSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Source for ScriptedSource {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let shutdown = context.take_shutdown_handle();
        tokio::pin!(shutdown);
        let mut open = true;
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                batch = self.batches.recv(), if open => match batch {
                    Some(events) => {
                        let mut buffer = EventsBuffer::default();
                        for event in events {
                            assert!(buffer.try_push(event).is_none(), "a scripted batch fits one buffer");
                        }
                        context.dispatcher().dispatch_named("metrics", buffer).await?;
                    }
                    None => open = false,
                },
            }
        }
        Ok(())
    }
}

/// A timestamped counter, which the no-aggregation pipeline sends past aggregation, so that it reaches the outputs
/// at once.
fn timestamped(name: &'static str) -> Event {
    Event::Metric(Metric::counter(Context::from_static_name(name), (1_700_000_000, 1.0)))
}

/// A transform that forwards every batch unchanged, standing in for aggregation, so that what aggregation receives
/// reaches the outputs at once.
struct ForwardingTransformBuilder {
    outputs: [OutputDefinition<EventType>; 1],
}

impl ForwardingTransformBuilder {
    fn new() -> Self {
        Self {
            outputs: [OutputDefinition::default_output(EventType::Metric)],
        }
    }
}

struct ForwardingTransform;

#[async_trait]
impl TransformBuilder for ForwardingTransformBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(ForwardingTransform))
    }
}

impl MemoryBounds for ForwardingTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Transform for ForwardingTransform {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            tokio::select! {
                _ = health.live() => continue,
                events = context.events().next() => match events {
                    Some(events) => context.dispatcher().dispatch(events).await?,
                    None => break,
                },
            }
        }
        Ok(())
    }
}

/// What stands in for DogStatsD aggregation in a test topology.
#[derive(Clone, Copy)]
enum Aggregation {
    /// The native aggregation component, configured as the configuration says.
    Native,

    /// A transform that forwards what it receives.
    Forwarding,
}

fn add_aggregation(blueprint: &mut TopologyBlueprint, config: &SalukiConfiguration, aggregation: Aggregation) {
    match aggregation {
        Aggregation::Native => {
            let (aggregate, _snapshots) = dogstatsd_aggregate_configuration(config).unwrap();
            blueprint.add_transform("dsd_agg", aggregate).unwrap();
        }
        Aggregation::Forwarding => {
            blueprint
                .add_transform("dsd_agg", ForwardingTransformBuilder::new())
                .unwrap();
        }
    }
}

/// A topology running the plan-driven metrics path of DogStatsD alone, with the installer: the regions, edges, and
/// admission points as the plan-driven topology builds them, the DogStatsD source replaced by a scripted one, and
/// every output by a destination that reports each metric it receives, [described](describe).
struct Running {
    cell: ConfigCell,
    batches: mpsc::Sender<Vec<Event>>,
    main_output: mpsc::UnboundedReceiver<String>,
    telemetry_output: mpsc::UnboundedReceiver<String>,
    /// What the Multi-Region Failover output receives, when the configuration enables it.
    _mirror_output: mpsc::UnboundedReceiver<String>,
    status: Arc<ArcSwap<PlanStatus>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), saluki_core::runtime::SupervisorError>>,
}

impl Running {
    /// Starts the topology, with `aggregation` in place of DogStatsD aggregation.
    async fn start(yaml: &str, aggregation: Aggregation) -> Self {
        let config = configuration(yaml).await;
        let cell = ConfigCell::new(config.clone());
        let registry = ComponentRegistry::default();
        let mut metrics = programmable_metrics(&config, cell.live(), &registry).unwrap();

        let (batches, receiver) = mpsc::channel(16);
        let mut blueprint = TopologyBlueprint::new(PRIMARY_TOPOLOGY, &registry);
        blueprint
            .add_source(
                "dsd_in",
                ScriptedSourceBuilder {
                    batches: StdMutex::new(Some(receiver)),
                    outputs: [OutputDefinition::named_output("metrics", EventType::Metric)],
                },
            )
            .unwrap();
        add_aggregation(&mut blueprint, &config, aggregation);
        let boot = metrics.boot_topology();
        let (main, main_output) = mpsc::unbounded_channel();
        let (telemetry, telemetry_output) = mpsc::unbounded_channel();
        let (mirror, mirror_output) = mpsc::unbounded_channel();
        for output in boot.outputs.iter() {
            let metrics = match output.component.as_str() {
                "dd_metrics_encode" => main.clone(),
                "dsd_client_telemetry_out" => telemetry.clone(),
                "mrf_metrics_encode" => mirror.clone(),
                other => panic!("DogStatsD with Multi-Region Failover has no output `{}`", other),
            };
            blueprint
                .add_destination(output.component.as_str(), DescribingDestinationBuilder { metrics })
                .unwrap();
        }
        add_regions(&mut blueprint, boot).unwrap();
        connect_frame(&mut blueprint, boot).unwrap();
        blueprint
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_ambient_worker_pool();

        let installer = metrics.into_installer();
        let status = installer.status();
        let mut supervisor = Supervisor::new("installer-test").unwrap();
        supervisor.add_worker(blueprint);
        supervisor.add_worker(installer);
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

        Self {
            cell,
            batches,
            main_output,
            telemetry_output,
            _mirror_output: mirror_output,
            status,
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Sends one batch of timestamped counters, and returns the names the main output receives up to the last of
    /// them, which the batch must not drop.
    async fn send(&mut self, names: &[&'static str]) -> Vec<String> {
        self.batches
            .send(names.iter().map(|name| timestamped(name)).collect())
            .await
            .unwrap();
        let last = names.last().expect("a batch has metrics");
        let mut received = Vec::new();
        tokio::time::timeout(PATIENCE, async {
            loop {
                let metric = self.main_output.recv().await.expect("the main output stays open");
                let name = metric.split(' ').next().unwrap_or_default().to_string();
                let done = name == *last;
                received.push(name);
                if done {
                    break;
                }
            }
        })
        .await
        .expect("the main output receives the batch");
        received
    }

    async fn stop(mut self) {
        drop(self.batches);
        self.shutdown.take().unwrap().send(()).unwrap();
        let result = tokio::time::timeout(PATIENCE, self.task).await.unwrap().unwrap();
        assert!(result.is_ok(), "the topology stops cleanly: {:?}", result);
    }
}

fn counter(recorder: &TestRecorder, name: &'static str, labels: &[(&'static str, &'static str)]) -> Option<u64> {
    recorder.counter(Key::from_parts(name, labels))
}

fn gauge(recorder: &TestRecorder, name: &'static str, labels: &[(&'static str, &'static str)]) -> Option<f64> {
    recorder.gauge(Key::from_parts(name, labels))
}

const PREFIX_FILTER: &[(&str, &str)] = &[("component_id", "dsd_prefix_filter"), ("component_type", "transform")];

#[tokio::test]
async fn a_configuration_change_installs_a_new_generation_and_the_old_one_retires() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let mut running = Running::start("", Aggregation::Native).await;
    status_where(&running.status, |status| active(status) == Some(1)).await;
    assert_eq!(gauge(&recorder, "metric_filterlist_size", PREFIX_FILTER), Some(0.0));
    // The installer reports the rewrites applied in compiling the boot generation, by rewrite, from boot: none applies
    // to DogStatsD's default program (see the next test for one that does).
    let rewrites =
        || RewriteId::ALL.map(|rewrite| gauge(&recorder, PLAN_REWRITES_APPLIED, &[("rewrite", rewrite.as_str())]));
    assert_eq!(rewrites(), [Some(0.0); 6]);

    // Generation 1 runs the boot configuration, which blocks nothing.
    assert_eq!(running.send(&["blocked", "kept"]).await, ["blocked", "kept"]);

    // A live change: the listener filterlist now blocks `blocked`. The installer lowers it, prepares it in both
    // regions, and commits; generation 1 retires once nothing stamped with it is left.
    running.cell.set(configuration("metric_filterlist: [blocked]").await);
    let status = status_where(&running.status, |status| live(status) == [2]).await;
    assert_eq!(active(&status), Some(2));
    assert_eq!(state_of(&status, 1), Some(GenerationState::Retired));
    assert_eq!(status.config_changes, 1);
    assert!(status.degraded.is_empty() && status.last_error.is_none());
    assert_eq!(running.send(&["blocked", "kept"]).await, ["kept"]);

    // An unchanged plan creates no generation.
    running
        .cell
        .set(configuration("metric_filterlist: [blocked]\nlog_level: debug").await);
    let status = status_where(&running.status, |status| status.config_changes == 2).await;
    assert_eq!((active(&status), live(&status)), (Some(2), vec![2]));

    assert_eq!(gauge(&recorder, PLAN_ACTIVE_GENERATION, &[]), Some(2.0));
    assert_eq!(gauge(&recorder, PLAN_LIVE_GENERATIONS, &[]), Some(1.0));
    assert_eq!(
        counter(&recorder, PLAN_INSTALLS_TOTAL, &[("outcome", "committed")]),
        Some(1)
    );
    assert_eq!(
        counter(&recorder, PLAN_INSTALLS_TOTAL, &[("outcome", "unchanged")]),
        Some(1)
    );
    // And it sets them at every commit, for the generation committed: still none here.
    assert_eq!(rewrites(), [Some(0.0); 6]);
    // The native filterlist's size and updates, which the installer records from the plan's bindings.
    assert_eq!(gauge(&recorder, "metric_filterlist_size", PREFIX_FILTER), Some(1.0));
    assert_eq!(
        counter(&recorder, "metric_filterlist_updates_total", PREFIX_FILTER),
        Some(1)
    );
    // A tag filterlist change is counted only when the tag filterlist changes; the counter is reported from boot, as
    // the native component's is.
    let tag_filterlist = &[("component_id", "dsd_tag_filterlist"), ("component_type", "transform")];
    assert_eq!(
        counter(&recorder, "tag_filterlist_updates_total", tag_filterlist),
        Some(0)
    );

    // Regions publish what they counted when they stop: the filter dropped one metric under generation 2.
    running.stop().await;
    assert_eq!(
        counter(&recorder, "dogstatsd_listener_filtered_points_total", PREFIX_FILTER),
        Some(1)
    );
    assert_eq!(
        counter(
            &recorder,
            "plan_node_events_total",
            &[("component_id", "dsd_prefix_filter_blocklist"), ("port", "true")]
        ),
        Some(1)
    );
    assert_eq!(
        counter(
            &recorder,
            "plan_loss_total",
            &[
                ("component_id", "dsd_prefix_filter_blocklist_true"),
                ("class", "policy")
            ]
        ),
        Some(1)
    );
}

/// DogStatsD with Multi-Region Failover, mirroring what its allowlist names, or nothing when `mirroring` is false.
fn multi_region_failover(mirroring: bool) -> String {
    format!(
        "multi_region_failover:\n  enabled: true\n  api_key: mrf-key\n  dd_url: https://mrf.example.com\n  \
         failover_metrics: {}\n  metric_allowlist: [mirrored.a]\n",
        mirroring
    )
}

#[tokio::test]
async fn the_installer_reports_the_rewrites_of_the_boot_generation_and_of_every_commit() {
    let recorder = TestRecorder::default();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let rewrites =
        || RewriteId::ALL.map(|rewrite| gauge(&recorder, PLAN_REWRITES_APPLIED, &[("rewrite", rewrite.as_str())]));

    // With mirroring off, the Multi-Region Failover gateway's predicate, `all(flag(enabled), flag(mirroring), ...)`,
    // is false given its resolved values, so decision compilation folds it (R3): one application, from boot. Nothing
    // else applies (gauges in `RewriteId::ALL` order: R2, R3, R4, R5a, R6, R7).
    let running = Running::start(&multi_region_failover(false), Aggregation::Forwarding).await;
    let boot = status_where(&running.status, |status| active(status) == Some(1)).await;
    let folded = [Some(0.0), Some(1.0), Some(0.0), Some(0.0), Some(0.0), Some(0.0)];
    assert_eq!(rewrites(), folded);
    // The active plan's explain document lists the same applications.
    let explained = |status: &PlanStatus| {
        let explain = &status.active.as_ref().expect("a generation is active").explain;
        let rewrites = explain.rewrites.as_ref().expect("the document has a rewrites section");
        rewrites
            .iter()
            .filter(|rewrite| rewrite.outcome == "applied")
            .map(|rewrite| rewrite.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(explained(&boot), ["R3"]);

    // Mirroring on: the predicate now depends on each metric's name (the allowlist is not empty), so the generation
    // committed folds nothing, and the gauges say so at the commit.
    running.cell.set(configuration(&multi_region_failover(true)).await);
    let mirroring = status_where(&running.status, |status| active(status) == Some(2)).await;
    assert_eq!(rewrites(), [Some(0.0); 6]);
    assert!(explained(&mirroring).is_empty());

    // And off again.
    running.cell.set(configuration(&multi_region_failover(false)).await);
    status_where(&running.status, |status| active(status) == Some(3)).await;
    assert_eq!(rewrites(), folded);

    running.stop().await;
}

#[tokio::test]
async fn a_stream_of_configuration_changes_settles_on_the_latest_within_the_overlap_budget() {
    let mut running = Running::start("", Aggregation::Native).await;
    let blocking = configuration("metric_filterlist: [blocked]").await;
    let open = configuration("metric_filterlist: [other]").await;

    // Twenty changes in quick succession, ending with the one that blocks `blocked`: a change that arrives while a
    // preparation is in flight waits behind it, replacing any change already waiting, so intermediate changes may never
    // become generations, but the last one does, and no more generations are alive at once than the overlap budget
    // allows.
    for change in 0..20 {
        let config = if change % 2 == 0 { &open } else { &blocking };
        running.cell.set(config.clone());
        // Each change is observed before the next, while preparations are still in flight.
        let status = status_where(&running.status, |status| status.config_changes == change + 1).await;
        assert!(status.installer.live.len() <= 4, "{}", status);
    }
    let settled = status_where(&running.status, |status| {
        status.config_changes == 20
            && status.installer.preparing.is_none()
            && status.installer.pending.is_none()
            && status.installer.live.len() == 1
    })
    .await;
    assert!(settled.last_error.is_none(), "{}", settled);
    let active = active(&settled).unwrap();
    assert!(active > 1, "{}", settled);
    assert_eq!(live(&settled), [active]);
    assert_eq!(running.send(&["blocked", "kept"]).await, ["kept"]);

    // Every replaced generation retired: regions hold only the active one.
    for record in &settled.installer.generations {
        if record.id.get() != active {
            assert!(record.state.is_final(), "{}: {:?}", record.id, record.state);
        }
    }
    running.stop().await;
}

/// Runs the installer alone, as a supervisor would, until `shutdown` triggers or the task is aborted.
fn run_installer(installer: &Arc<InstallerWorker>, shutdown: ShutdownHandle) -> impl Future<Output = ()> {
    let installer = Arc::clone(installer);
    async move {
        let run = match installer.initialize(shutdown).await {
            Ok(run) => run,
            Err(_) => panic!("the installer initializes"),
        };
        run.await.expect("the installer stops cleanly");
    }
}

/// Takes the regions' mailboxes, and marks them running: they stand for regions that run and never read them.
fn running_unread_mailboxes(metrics: &mut ProgrammableMetrics) -> Vec<Option<ControlMailbox<RegionControl>>> {
    let mailboxes = std::mem::take(&mut metrics.boot_topology().mailboxes);
    for mailbox in mailboxes.iter().flatten() {
        mailbox.mark_running();
    }
    mailboxes
}

#[tokio::test(start_paused = true)]
async fn a_preparation_no_region_acknowledges_is_aborted_and_retried_with_backoff() {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let mut metrics = programmable_metrics(&config, cell.live(), &ComponentRegistry::default()).unwrap();
    // The regions exist and run, but never answer: the test holds their mailboxes and never reads them.
    let _mailboxes = running_unread_mailboxes(&mut metrics);
    let installer = Arc::new(metrics.into_installer());
    let status = installer.status();
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let task = tokio::spawn(run_installer(&installer, shutdown));

    cell.set(configuration("metric_filterlist: [blocked]").await);
    let preparing = status_where(&status, |status| status.installer.preparing.is_some()).await;
    assert_eq!(
        preparing.installer.preparing.as_ref().map(|record| record.id.get()),
        Some(2)
    );

    // After the prepare timeout, the preparation is aborted in every region; the candidate is tried again a second
    // later, as generation 3, and then after two more seconds, as generation 4.
    let retried = status_where(&status, |status| state_of(status, 3) == Some(GenerationState::Rejected)).await;
    assert_eq!(state_of(&retried, 2), Some(GenerationState::Rejected));
    assert_eq!(
        reason_of(&retried, 2).as_deref(),
        Some("the regions `dsd_pre_aggregation`, `metrics_enrich` did not acknowledge within 10s")
    );
    assert_eq!(active(&retried), Some(1));
    status_where(&status, |status| {
        status.installer.preparing.as_ref().map(|record| record.id.get()) == Some(4)
    })
    .await;

    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, task).await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_region_that_stopped_fails_the_preparation_and_the_active_generation_keeps_running() {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    // Without a topology, the regions' mailboxes are dropped with the boot topology: every region is gone.
    let installer = Arc::new(
        programmable_metrics(&config, cell.live(), &ComponentRegistry::default())
            .unwrap()
            .into_installer(),
    );
    let status = installer.status();
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let task = tokio::spawn(run_installer(&installer, shutdown));

    cell.set(configuration("metric_filterlist: [blocked]").await);
    let failed = status_where(&status, |status| state_of(status, 2) == Some(GenerationState::Rejected)).await;
    let reason = reason_of(&failed, 2).unwrap();
    assert!(
        reason.starts_with("region `dsd_pre_aggregation` failed to prepare:"),
        "{}",
        reason
    );
    assert_eq!((active(&failed), live(&failed).first().copied()), (Some(1), Some(1)));
    assert!(
        failed.installer.pending.is_some(),
        "the candidate waits to be tried again"
    );

    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, task).await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_restarted_installer_continues_the_preparation_it_left() {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let mut metrics = programmable_metrics(&config, cell.live(), &ComponentRegistry::default()).unwrap();
    let _mailboxes = running_unread_mailboxes(&mut metrics);
    let installer = Arc::new(metrics.into_installer());
    let status = installer.status();

    // The first run prepares generation 2, and stops before any region answers, as a panicking worker would.
    let first = tokio::spawn(run_installer(&installer, ShutdownHandle::noop()));
    cell.set(configuration("metric_filterlist: [blocked]").await);
    status_where(&status, |status| status.installer.preparing.is_some()).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    // The restarted worker keeps the preparation, its deadline, and the configuration change it observed.
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let second = tokio::spawn(run_installer(&installer, shutdown));
    let retried = status_where(&status, |status| state_of(status, 2) == Some(GenerationState::Rejected)).await;
    assert_eq!(retried.config_changes, 1);
    assert_eq!(
        reason_of(&retried, 2).as_deref(),
        Some("the regions `dsd_pre_aggregation`, `metrics_enrich` did not acknowledge within 10s")
    );
    let generations = retried
        .installer
        .generations
        .iter()
        .map(|record| record.id.get())
        .collect::<BTreeSet<_>>();
    assert_eq!(generations.first(), Some(&1));

    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, second).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_configuration_whose_program_no_operator_accepts_fails_the_boot() {
    // An integer beyond what a plan holds: lowering writes it as configured, and the aggregation operator cannot be
    // configured with it, so the default program has no frame to run in.
    let config = configuration("aggregate_context_limit: 18014398509481984").await;
    let facts = crate::programmable_metrics::test_support::facts(&config);
    let lowering = Lowering::new(Arc::new(config.clone()), facts.clone()).unwrap().unwrap();
    let error = match ProgrammableMetrics::from_lowering(
        lowering,
        facts,
        &config,
        ConfigCell::new(config.clone()).live(),
        &ComponentRegistry::default(),
        OperatorRegistry::native(),
    ) {
        Ok(_) => panic!("the boot fails"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.starts_with("The metrics path cannot be laid out from the default program lowered from the"),
        "{}",
        error
    );
    assert!(error.contains("context_limit"), "{}", error);
}

#[test]
fn plan_status_describes_itself_for_the_logs() {
    let generation = |id: u64| GenerationId::new(id);
    let status = PlanStatus {
        installer: InstallerStatus {
            active: None,
            last_known_good: None,
            preparing: None,
            pending: None,
            live: vec![generation(1), generation(2)],
            revision_high_water: None,
            generations: Vec::new(),
        },
        degraded: vec!["bad".to_string()],
        config_changes: 3,
        last_error: Some("no".to_string()),
        overdue: vec![generation(1)],
        apply_states: ApplyStates::default(),
        exports: BTreeMap::new(),
        default_program: Arc::new(Plan::from_json(saluki_metric_plan::testing::DEFAULT_PROGRAM_JSON).unwrap()),
        active: None,
    };
    assert_eq!(
        status.to_string(),
        "no generation active, 2 live, 3 configuration changes observed, degraded: bad, last change not installed: \
         no, draining past the deadline: 1"
    );

    // An active artifact is named after the generation, and its stale flags follow.
    let mut with_artifact = status;
    with_artifact.degraded.clear();
    with_artifact.last_error = None;
    with_artifact.overdue.clear();
    let summary = &mut with_artifact.apply_states.status;
    summary.active_artifact = Some(saluki_metric_plan::channel::ActiveArtifact {
        id: saluki_metric_plan::channel::ArtifactConfigId::new("demo"),
        revision: 7,
        artifact_hash: saluki_metric_plan::canonical::PlanHash::of_bytes(b"artifact"),
    });
    summary.stale_base = true;
    summary.stale_review = true;
    assert_eq!(
        with_artifact.to_string(),
        "no generation active (artifact `demo` revision 7), 2 live, 3 configuration changes observed, stale base, \
         stale review"
    );
}

/// A metric as an output receives it, for comparing outputs as multisets: its name, its tags, sorted, and its values.
fn describe(metric: &Metric) -> String {
    let mut tags = metric
        .context()
        .tags()
        .into_iter()
        .map(|tag| tag.as_str().to_string())
        .collect::<Vec<_>>();
    tags.sort();
    format!("{} [{}] {:?}", metric.context().name(), tags.join(","), metric.values())
}

/// A destination that reports every metric it receives, described.
struct DescribingDestinationBuilder {
    metrics: mpsc::UnboundedSender<String>,
}

struct DescribingDestination {
    metrics: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl DestinationBuilder for DescribingDestinationBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(DescribingDestination {
            metrics: self.metrics.clone(),
        }))
    }
}

impl MemoryBounds for DescribingDestinationBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Destination for DescribingDestination {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        while let Some(events) = context.events().next().await {
            for metric in events.into_iter().filter_map(Event::try_into_metric) {
                let _ = self.metrics.send(describe(&metric));
            }
        }
        Ok(())
    }
}

/// A DogStatsD configuration that exercises every node of the pre-aggregation and enrichment regions.
const DIFFERENTIAL: &str = r#"
dogstatsd_mapper_profiles:
  - name: checkout
    prefix: raw.
    mappings:
      - match: raw.requests.*
        name: mapped.requests
        tags:
          route: $1
statsd_metric_namespace: tenant
statsd_metric_namespace_blacklist: [internal.]
metric_filterlist: [tenant.raw.blocked, tenant.checkout.latency.max]
metric_tag_filterlist:
  - metric_name: tenant.checkout.latency
    action: exclude
    tags: [user_id]
metric_tag_value_allowlist:
  - metric_prefix: tenant.checkout.
    tag_name: env
    values: [prod, staging]
    on_miss: replace
"#;

/// The metrics the differential test sends, in batches: mapped, prefixed and not, blocked before and after
/// aggregation, tag-filtered, and timestamped (which skip tag filtering and aggregation), of every kind.
fn differential_batches() -> Vec<Vec<Event>> {
    let context = Context::from_static_parts;
    vec![
        vec![
            Event::Metric(Metric::counter(context("raw.requests.checkout", &["keep:a"]), 1.0)),
            Event::Metric(Metric::counter(context("raw.blocked", &[]), 1.0)),
            Event::Metric(Metric::gauge(context("internal.health", &["keep:b"]), 3.0)),
            Event::Metric(Metric::distribution(
                context("checkout.latency", &["user_id:1", "env:dev", "keep:c"]),
                &[1.0, 2.0][..],
            )),
        ],
        vec![
            // Blocked after aggregation: the series of a histogram statistic.
            Event::Metric(Metric::gauge(context("checkout.latency.max", &[]), 9.0)),
            Event::Metric(Metric::gauge(context("checkout.latency.avg", &["env:prod"]), 4.0)),
            Event::Metric(Metric::set(context("visitors", &["env:prod"]), "u1")),
            Event::Metric(Metric::counter(
                context("checkout.latency", &["user_id:2", "env:qa"]),
                (1_700_000_000, 5.0),
            )),
            Event::Metric(Metric::counter(context("raw.requests.ts", &[]), (1_700_000_010, 2.0))),
            Event::Metric(Metric::gauge(context("raw.blocked", &[]), (1_700_000_020, 1.0))),
        ],
        // Sentinels, one per path, after which everything sent before has been delivered.
        vec![
            Event::Metric(Metric::gauge(context("zz.sentinel", &[]), 1.0)),
            Event::Metric(Metric::gauge(context("zz.sentinel_ts", &[]), (1_700_000_030, 1.0))),
        ],
    ]
}

/// Collects what an output receives until both sentinels have arrived, sorted.
async fn collect_until_sentinels(receiver: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut received = Vec::new();
    tokio::time::timeout(PATIENCE, async {
        while !["tenant.zz.sentinel ", "tenant.zz.sentinel_ts "]
            .iter()
            .all(|sentinel| received.iter().any(|metric: &String| metric.starts_with(sentinel)))
        {
            received.push(receiver.recv().await.expect("the output stays open"));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the sentinels never arrived; received {:?}", received));
    received.sort();
    received
}

/// The legacy metrics path of DogStatsD, from native components, as `run.rs` built it, with the same test doubles as
/// [`Running`]: a scripted source, forwarding in place of aggregation, and describing destinations.
async fn legacy_outputs(cell: &ConfigCell) -> (Vec<String>, Vec<String>) {
    use saluki_components::transforms::{ChainedConfiguration, HostEnrichmentConfiguration};

    use crate::{
        cli::run::dogstatsd_mapper_configuration,
        components::{
            dogstatsd_no_agg_split::DogStatsDNoAggSplitConfiguration,
            dogstatsd_post_aggregate_filter::DogStatsDPostAggregateFilterConfiguration,
            dogstatsd_prefix_filter::DogStatsDPrefixFilterConfiguration, tag_filterlist::TagFilterlistConfiguration,
        },
        programmable_metrics::test_support::environment_provider,
    };

    let live = cell.live();
    let config = SalukiConfiguration::clone(&live);
    let dogstatsd = &config.domains.dogstatsd;
    let histogram = &config.shared.metrics_encoding.histogram;
    let registry = ComponentRegistry::default();
    let (batches, receiver) = mpsc::channel(16);
    let (main, mut main_rx) = mpsc::unbounded_channel();
    let (telemetry, mut telemetry_rx) = mpsc::unbounded_channel();
    let env = environment_provider(&config).await;

    let mut blueprint = TopologyBlueprint::new(PRIMARY_TOPOLOGY, &registry);
    blueprint
        .add_source(
            "dsd_in",
            ScriptedSourceBuilder {
                batches: StdMutex::new(Some(receiver)),
                outputs: [OutputDefinition::named_output("metrics", EventType::Metric)],
            },
        )
        .unwrap()
        .add_transform(
            "dsd_enrich",
            ChainedConfiguration::default()
                .with_transform_builder("dogstatsd_mapper", dogstatsd_mapper_configuration(&dogstatsd.mapper)),
        )
        .unwrap()
        .add_transform(
            "dsd_prefix_filter",
            DogStatsDPrefixFilterConfiguration::new(
                dogstatsd.prefix_filter.metric_namespace.clone(),
                dogstatsd.prefix_filter.metric_namespace_blocklist.clone(),
                live.project(|config| &config.domains.dogstatsd.metric_filter),
            ),
        )
        .unwrap()
        .add_transform(
            "dsd_no_agg_split",
            DogStatsDNoAggSplitConfiguration::new(
                dogstatsd.aggregation.passthrough_idle_flush_timeout,
                dogstatsd.aggregation.window_duration_seconds,
            ),
        )
        .unwrap()
        .add_transform(
            "dsd_tag_filterlist",
            TagFilterlistConfiguration::new(
                live.project(|config| &config.domains.dogstatsd.tag_filterlist),
                &dogstatsd.tag_value_allowlist,
                dogstatsd.aggregation.aggregator_tag_filter_cache_capacity,
            )
            .unwrap(),
        )
        .unwrap()
        .add_transform("dsd_agg", ForwardingTransformBuilder::new())
        .unwrap()
        .add_transform(
            "dsd_post_agg_filter",
            DogStatsDPostAggregateFilterConfiguration::new(
                live.project(|config| &config.domains.dogstatsd.metric_filter),
                &histogram.aggregates,
                &histogram.percentiles,
            )
            .unwrap(),
        )
        .unwrap()
        .add_transform(
            "metrics_enrich",
            ChainedConfiguration::default().with_transform_builder(
                "host_enrichment",
                HostEnrichmentConfiguration::from_environment_provider(env.clone()),
            ),
        )
        .unwrap()
        .add_destination("dd_metrics_encode", DescribingDestinationBuilder { metrics: main })
        .unwrap()
        .add_destination(
            "dsd_client_telemetry_out",
            DescribingDestinationBuilder { metrics: telemetry },
        )
        .unwrap()
        .connect_components_in_order([
            "dsd_in.metrics",
            "dsd_enrich",
            "dsd_prefix_filter",
            "dsd_no_agg_split",
            "dsd_tag_filterlist",
            "dsd_agg",
            "dsd_post_agg_filter",
            "metrics_enrich",
            "dd_metrics_encode",
        ])
        .unwrap()
        .connect_components("dsd_no_agg_split.passthrough", "dsd_post_agg_filter")
        .unwrap()
        .connect_components("dsd_post_agg_filter", "dsd_client_telemetry_out")
        .unwrap();
    blueprint
        .with_health_registry(HealthRegistry::new())
        .with_memory_limiter(MemoryLimiter::noop())
        .with_resource_registry(ResourceRegistry::new())
        .with_ambient_worker_pool();
    let mut supervisor = Supervisor::new("legacy-test").unwrap();
    supervisor.add_worker(blueprint);
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

    for batch in differential_batches() {
        batches.send(batch).await.unwrap();
    }
    let outputs = (
        collect_until_sentinels(&mut main_rx).await,
        collect_until_sentinels(&mut telemetry_rx).await,
    );
    drop(batches);
    shutdown.send(()).unwrap();
    tokio::time::timeout(PATIENCE, task).await.unwrap().unwrap().unwrap();
    outputs
}

/// The plan-driven metrics path of the same configuration, as [`Running`] runs it, with forwarding in place of
/// aggregation.
async fn plan_driven_outputs(yaml: &str) -> (Vec<String>, Vec<String>) {
    let mut running = Running::start(yaml, Aggregation::Forwarding).await;
    for batch in differential_batches() {
        running.batches.send(batch).await.unwrap();
    }
    let outputs = (
        collect_until_sentinels(&mut running.main_output).await,
        collect_until_sentinels(&mut running.telemetry_output).await,
    );
    running.stop().await;
    outputs
}

#[tokio::test]
async fn the_plan_driven_metrics_path_delivers_what_the_legacy_one_does() {
    let config = configuration(DIFFERENTIAL).await;
    let (legacy_main, legacy_telemetry) = legacy_outputs(&ConfigCell::new(config)).await;
    let (plan_main, plan_telemetry) = plan_driven_outputs(DIFFERENTIAL).await;
    assert_eq!(plan_main, legacy_main, "the main output");
    assert_eq!(plan_telemetry, legacy_telemetry, "the client telemetry tap");

    // What reaches the outputs, written by hand: everything but the blocked metrics, mapped and prefixed, with the
    // tag filterlist and the value allowlist applied to what is aggregated; the timestamped counters as rates.
    let names = plan_main
        .iter()
        .map(|metric| metric.split(' ').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "internal.health",
            "tenant.checkout.latency",
            "tenant.checkout.latency",
            "tenant.checkout.latency.avg",
            "tenant.mapped.requests",
            "tenant.mapped.requests",
            "tenant.visitors",
            "tenant.zz.sentinel",
            "tenant.zz.sentinel_ts",
        ]
    );
    assert!(plan_main
        .iter()
        .any(|metric| metric.starts_with("tenant.checkout.latency [env:other,keep:c]")));
    assert!(plan_main
        .iter()
        .any(|metric| metric.starts_with("tenant.checkout.latency [env:qa,user_id:2]")));
    assert!(plan_main
        .iter()
        .any(|metric| metric.starts_with("tenant.mapped.requests [keep:a,route:checkout]")));
}
