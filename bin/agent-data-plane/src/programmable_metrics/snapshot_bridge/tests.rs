//! The snapshot bridge, against a running region whose driver answers kernel control messages, and against a region
//! that has stopped.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_components::transforms::{
    aggregate_context_snapshot_channel, AggregateContextSnapshotEntry, AggregateContextSnapshotHandle,
    AggregateMetricType,
};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        sources::{Source, SourceBuilder, SourceContext},
        BuildContext,
    },
    data_model::event::{metric::context::Context, EventType},
    execution::{
        control_mailbox, ExitSink, PanicOutcome, Progress, RegionDriver, RegionFailure, RegionHostBuilder, RegionPanic,
        RegionTurn,
    },
    health::HealthRegistry,
    runtime::{state::ResourceRegistry, Supervisable, Supervisor, SupervisorError},
    topology::{EventsBuffer, OutputDefinition, TopologyBlueprint},
};
use saluki_metric_plan::runtime::{KernelTarget, RegionControl};
use stringtheory::MetaString;
use tokio::{sync::oneshot, task::JoinHandle};

use super::*;

/// The longest a test waits for the topology or an answer.
const PATIENCE: Duration = Duration::from_secs(30);

fn entry(name: &'static str) -> AggregateContextSnapshotEntry {
    AggregateContextSnapshotEntry::for_test(
        Context::from_static_name(name),
        AggregateMetricType::Counter,
        MetaString::from_static(""),
    )
}

/// A region driver whose state owners hold the contexts `held`, each lineage one of them, and that records the target
/// of every kernel control message it handles.
struct HoldingDriver {
    held: Vec<&'static str>,
    targets: Arc<StdMutex<Vec<KernelTarget>>>,
}

impl RegionDriver for HoldingDriver {
    type Control = RegionControl;

    fn process(&mut self, _turn: &mut RegionTurn<'_>, _batch: EventsBuffer, _exits: &mut ExitSink) -> Progress {
        Progress::Done
    }

    fn control(
        &mut self, _turn: &mut RegionTurn<'_>, message: RegionControl, _exits: &mut ExitSink,
    ) -> Result<(), RegionFailure> {
        let RegionControl::Kernel(control) = message else {
            return Ok(());
        };
        self.targets.lock().unwrap().push(control.target.clone());
        let mut message = control.message;
        // Each lineage adds its contexts in turn, as the plan's driver delivers the message to each instance.
        if let Some(snapshot) = message.downcast_mut::<ContextSnapshot>() {
            for name in &self.held {
                snapshot.entries.push(entry(name));
            }
        }
        (control.reply)(message);
        Ok(())
    }

    fn reset_after_panic(&mut self, _panic: &RegionPanic) -> PanicOutcome {
        PanicOutcome::Continue
    }
}

/// A source that sends nothing and runs until shutdown.
struct IdleSourceBuilder {
    outputs: [OutputDefinition<EventType>; 1],
}

struct IdleSource;

#[async_trait]
impl SourceBuilder for IdleSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(IdleSource))
    }
}

impl MemoryBounds for IdleSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Source for IdleSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        context.take_shutdown_handle().await;
        Ok(())
    }
}

/// A destination that reads everything until its input ends.
struct DrainingSinkBuilder;

struct DrainingSink;

#[async_trait]
impl DestinationBuilder for DrainingSinkBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(DrainingSink))
    }
}

impl MemoryBounds for DrainingSinkBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[async_trait]
impl Destination for DrainingSink {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        while context.events().next().await.is_some() {}
        Ok(())
    }
}

/// A running topology with one region, `dsd_agg`, whose driver holds `held`; the bridge between its region and a
/// snapshot channel; and the handle that requests go through.
struct Running {
    handle: AggregateContextSnapshotHandle,
    bridge: Arc<SnapshotBridge>,
    targets: Arc<StdMutex<Vec<KernelTarget>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl Running {
    async fn start(held: Vec<&'static str>) -> Self {
        let targets = Arc::new(StdMutex::new(Vec::new()));
        let (region, mailbox) = control_mailbox();
        let driver = HoldingDriver {
            held,
            targets: Arc::clone(&targets),
        };
        let host = RegionHostBuilder::from_driver(["metrics_enrich"], NonZeroUsize::MIN, mailbox, driver).unwrap();
        let registry = ComponentRegistry::default();
        let mut blueprint = TopologyBlueprint::new("snapshot-bridge-test", &registry);
        blueprint
            .add_source(
                "dsd_in",
                IdleSourceBuilder {
                    outputs: [OutputDefinition::named_output("metrics", EventType::Metric)],
                },
            )
            .unwrap()
            .add_transform("dsd_agg", host)
            .unwrap()
            .add_destination("metrics_enrich", DrainingSinkBuilder)
            .unwrap()
            .connect_components("dsd_in.metrics", "dsd_agg")
            .unwrap()
            .connect_components("dsd_agg.metrics_enrich", "metrics_enrich")
            .unwrap()
            .with_health_registry(HealthRegistry::new())
            .with_memory_limiter(MemoryLimiter::noop())
            .with_resource_registry(ResourceRegistry::new())
            .with_ambient_worker_pool();

        let (handle, receiver) = aggregate_context_snapshot_channel();
        let state = SnapshotBridgeState::new(receiver).unwrap();
        let bridge = Arc::new(SnapshotBridge::new(state, region.clone()));
        let mut supervisor = Supervisor::new("snapshot-bridge-test").unwrap();
        supervisor.add_worker(blueprint);
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });
        assert!(
            tokio::time::timeout(PATIENCE, region.running()).await.unwrap(),
            "the region runs"
        );
        Self {
            handle,
            bridge,
            targets,
            shutdown: Some(shutdown),
            task,
        }
    }

    /// Runs the bridge, as a supervisor would, until the returned task is aborted.
    fn run_bridge(&self) -> JoinHandle<()> {
        let bridge = Arc::clone(&self.bridge);
        tokio::spawn(async move {
            let run = match bridge.initialize(ShutdownHandle::noop()).await {
                Ok(run) => run,
                Err(_) => panic!("the bridge initializes"),
            };
            run.await.expect("the bridge stops cleanly");
        })
    }

    async fn snapshot(&self) -> Result<Vec<AggregateContextSnapshotEntry>, GenericError> {
        tokio::time::timeout(PATIENCE, self.handle.snapshot())
            .await
            .expect("the request is answered")
    }

    async fn stop(mut self) {
        self.shutdown.take().unwrap().send(()).unwrap();
        let result = tokio::time::timeout(PATIENCE, self.task).await.unwrap().unwrap();
        assert!(result.is_ok(), "the topology stops cleanly: {:?}", result);
    }
}

fn names(entries: &[AggregateContextSnapshotEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.context().name().to_string()).collect()
}

#[tokio::test]
async fn a_snapshot_request_is_answered_with_the_contexts_every_state_owner_of_the_region_holds() {
    let running = Running::start(vec!["first", "second"]).await;
    let bridge = running.run_bridge();

    let entries = running.snapshot().await.expect("the region answers");
    assert_eq!(names(&entries), ["first", "second"]);
    assert_eq!(*running.targets.lock().unwrap(), [KernelTarget::Stateful]);

    // Requests are answered one after the other, each from the region's state as it stands.
    let entries = running.snapshot().await.expect("the region answers");
    assert_eq!(names(&entries), ["first", "second"]);
    assert_eq!(running.targets.lock().unwrap().len(), 2);

    bridge.abort();
    running.stop().await;
}

#[tokio::test]
async fn a_restarted_bridge_answers_the_requests_its_previous_run_left() {
    let running = Running::start(vec!["held"]).await;
    let first = running.run_bridge();
    assert_eq!(names(&running.snapshot().await.unwrap()), ["held"]);

    // A run that stops, as a panicking one does, releases the requests; the next run takes them again.
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let second = running.run_bridge();
    assert_eq!(names(&running.snapshot().await.unwrap()), ["held"]);

    second.abort();
    running.stop().await;
}

#[tokio::test]
async fn a_request_to_a_region_that_stopped_fails_as_one_to_an_owner_that_stopped() {
    // The region's mailbox is gone with its host: every control message is dropped unanswered.
    let (region, mailbox) = control_mailbox::<RegionControl>();
    drop(mailbox);
    let (handle, receiver) = aggregate_context_snapshot_channel();
    let bridge = Arc::new(SnapshotBridge::new(SnapshotBridgeState::new(receiver).unwrap(), region));
    let run = {
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            let Ok(run) = bridge.initialize(ShutdownHandle::noop()).await else {
                panic!("the bridge initializes")
            };
            run.await.expect("the bridge stops cleanly");
        })
    };

    let error = tokio::time::timeout(PATIENCE, handle.snapshot())
        .await
        .expect("the request is answered")
        .expect_err("the region stopped");
    assert_eq!(
        error.to_string(),
        "aggregate context snapshot owner stopped before responding"
    );

    // Once every handle is gone, the bridge waits for shutdown, answering nothing more.
    drop(handle);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!run.is_finished(), "the bridge runs until shutdown");
    run.abort();
}

#[tokio::test]
async fn a_bridge_stops_at_shutdown_while_waiting_for_requests() {
    let (region, _mailbox) = control_mailbox::<RegionControl>();
    let (_handle, receiver) = aggregate_context_snapshot_channel();
    let bridge = SnapshotBridge::new(SnapshotBridgeState::new(receiver).unwrap(), region);
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let Ok(run) = bridge.initialize(shutdown).await else {
        panic!("the bridge initializes")
    };
    let run = tokio::spawn(run);
    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, run)
        .await
        .expect("the bridge stops")
        .unwrap()
        .expect("the bridge stops cleanly");
}

#[test]
fn clones_of_the_bridge_state_share_its_requests() {
    let (_handle, receiver) = aggregate_context_snapshot_channel();
    let state = SnapshotBridgeState::new(receiver).unwrap();
    // Clones share the requests: a restarted bridge answers the same ones.
    let clone = state.clone();
    assert!(Arc::ptr_eq(&state.requests, &clone.requests));
}
