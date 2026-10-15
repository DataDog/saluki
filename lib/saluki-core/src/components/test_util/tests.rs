use std::time::Duration;

use async_trait::async_trait;
use saluki_error::{generic_error, GenericError};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::TestComponentDriver;
use crate::accounting::{MemoryBounds, MemoryBoundsBuilder};
use crate::components::{
    destinations::{Destination, DestinationBuilder, DestinationContext},
    sources::{Source, SourceBuilder, SourceContext},
    transforms::{Transform, TransformBuilder, TransformContext},
    BuildContext,
};
use crate::data_model::event::{metric::Metric, Event, EventType};
use crate::runtime::state::IdentifierFilter;
use crate::runtime::{self, FnWorker, ShutdownStrategy, SupervisorError};
use crate::topology::{EventsBuffer, OutputDefinition};

/// Returns the names of the metrics in `buffer`, in order.
fn metric_names(buffer: EventsBuffer) -> Vec<String> {
    buffer
        .into_iter()
        .map(|event| match event {
            Event::Metric(metric) => metric.context().name().to_string(),
            other => panic!("expected only metrics, got {:?}", other),
        })
        .collect()
}

/// A source that dispatches one metric to each of its two outputs, marks itself ready, and then runs until shutdown.
struct TwoOutputSource;

#[async_trait]
impl Source for TwoOutputSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        let dispatcher = context.dispatcher();
        dispatcher
            .dispatch_one(Event::Metric(Metric::counter("to_default", 1.0)))
            .await?;
        dispatcher
            .dispatch_one_named("alt", Event::Metric(Metric::counter("to_alt", 1.0)))
            .await?;
        health.mark_ready();

        shutdown.await;
        Ok(())
    }
}

struct TwoOutputSourceBuilder {
    outputs: Vec<OutputDefinition<EventType>>,
}

impl TwoOutputSourceBuilder {
    fn new() -> Self {
        Self {
            outputs: vec![
                OutputDefinition::default_output(EventType::Metric),
                OutputDefinition::named_output("alt", EventType::Metric),
            ],
        }
    }
}

#[async_trait]
impl SourceBuilder for TwoOutputSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(TwoOutputSource))
    }
}

impl MemoryBounds for TwoOutputSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

/// A value that [`AssertingSource`] asserts into the dataspace.
#[derive(Clone, Debug, PartialEq)]
struct SourceState(&'static str);

/// A source that asserts a [`SourceState`] into the dataspace, marks itself ready, and then runs until shutdown.
struct AssertingSource;

#[async_trait]
impl Source for AssertingSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();

        context
            .topology_context()
            .dataspace()
            .assert(SourceState("running"), "state");
        health.mark_ready();

        shutdown.await;
        Ok(())
    }
}

/// A source that ignores its shutdown handle, so that it never stops unless it's aborted.
struct StuckSource;

#[async_trait]
impl Source for StuckSource {
    async fn run(self: Box<Self>, _context: SourceContext) -> Result<(), GenericError> {
        std::future::pending().await
    }
}

/// Builds whichever source it holds.
struct SourceBuilderFor<S> {
    source: fn() -> S,
}

#[async_trait]
impl<S: Source + Send + 'static> SourceBuilder for SourceBuilderFor<S> {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new((self.source)()))
    }
}

impl<S> MemoryBounds for SourceBuilderFor<S> {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

/// How the child spawned by [`SpawningDestination`] behaves, and when the destination stops.
#[derive(Clone, Copy)]
enum ChildBehavior {
    /// Finishes once the destination's `run` returns.
    StopsWithComponent,

    /// Never finishes, so the supervisor has to abort it.
    IgnoresShutdown,

    /// Never finishes, and is aborted as soon as the supervisor stops. The destination returns after its first event,
    /// rather than once its input closes.
    AbortedOnStop,
}

/// A destination that spawns one child on the ambient supervisor, then consumes its input until it closes.
struct SpawningDestination {
    child: ChildBehavior,
}

#[async_trait]
impl Destination for SpawningDestination {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        // Dropped when `run` returns, which is what a well-behaved child waits for.
        let (_component_running, component_stopped) = oneshot::channel::<()>();
        match self.child {
            ChildBehavior::StopsWithComponent => {
                runtime::spawn(FnWorker::new("child", async move {
                    let _ = component_stopped.await;
                }));
            }
            ChildBehavior::IgnoresShutdown => {
                runtime::spawn(FnWorker::new("child", std::future::pending::<()>()));
            }
            ChildBehavior::AbortedOnStop => {
                runtime::worker("child", std::future::pending::<()>())
                    .with_shutdown_strategy(ShutdownStrategy::Brutal)
                    .spawn();
                context.events().next().await;
                return Ok(());
            }
        }

        while context.events().next().await.is_some() {}
        Ok(())
    }
}

struct SpawningDestinationBuilder {
    child: ChildBehavior,
}

#[async_trait]
impl DestinationBuilder for SpawningDestinationBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(SpawningDestination { child: self.child }))
    }
}

impl MemoryBounds for SpawningDestinationBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

/// How [`FailingTransform`] fails.
#[derive(Clone, Copy)]
enum Failure {
    ReturnsError,
    Panics,
}

/// A transform whose `run` fails straight away.
struct FailingTransform {
    failure: Failure,
}

#[async_trait]
impl Transform for FailingTransform {
    async fn run(self: Box<Self>, _context: TransformContext) -> Result<(), GenericError> {
        match self.failure {
            Failure::ReturnsError => Err(generic_error!("transform failed on purpose")),
            Failure::Panics => panic!("transform panicked on purpose"),
        }
    }
}

struct FailingTransformBuilder {
    failure: Failure,
}

#[async_trait]
impl TransformBuilder for FailingTransformBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(FailingTransform { failure: self.failure }))
    }
}

impl MemoryBounds for FailingTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[tokio::test]
async fn source_routes_each_declared_output_and_stops_on_shutdown() {
    let (control, mut outputs) = TestComponentDriver::source(TwoOutputSourceBuilder::new())
        .await
        .expect("source should build");
    control.wait_until_ready().await;

    let default = outputs
        .default_output()
        .next()
        .await
        .expect("default output should receive a buffer");
    assert_eq!(metric_names(default), ["to_default"]);

    let alt = outputs
        .named_output("alt")
        .next()
        .await
        .expect("alt output should receive a buffer");
    assert_eq!(metric_names(alt), ["to_alt"]);

    // The source only returns once its shutdown handle fires, so a clean result here means the signal reached it.
    control
        .shutdown()
        .await
        .expect("source should stop cleanly on shutdown");

    // The source's dispatcher is gone with its context, and it held the only senders.
    assert!(outputs.default_output().next().await.is_none());
    assert!(outputs.named_output("alt").next().await.is_none());
}

#[tokio::test]
async fn children_spawned_from_run_join_the_component_supervisor() {
    let control = TestComponentDriver::destination(SpawningDestinationBuilder {
        child: ChildBehavior::StopsWithComponent,
    })
    .await
    .expect("destination should build");

    control.supervisor().wait_for_children(1).await;

    control
        .shutdown()
        .await
        .expect("the child should stop on its own once the destination does");
}

#[tokio::test]
async fn child_ignoring_shutdown_surfaces_as_shutdown_timed_out() {
    let control = TestComponentDriver::options()
        .with_shutdown_budget(Duration::from_millis(100))
        .destination(SpawningDestinationBuilder {
            child: ChildBehavior::IgnoresShutdown,
        })
        .await
        .expect("destination should build");

    control.supervisor().wait_for_children(1).await;

    let started = Instant::now();
    let error = control
        .shutdown()
        .await
        .expect_err("aborting the child should make the shutdown unclean");
    let elapsed = started.elapsed();

    let supervisor_error = error
        .downcast_ref::<SupervisorError>()
        .expect("the error should come from the supervisor");
    assert!(
        matches!(supervisor_error, SupervisorError::ShutdownTimedOut { aborted: 1 }),
        "expected exactly one aborted child, got {supervisor_error:?}"
    );

    // The default budget is five seconds, so finishing well inside that shows the configured budget was applied.
    assert!(
        elapsed < Duration::from_secs(3),
        "the 100ms budget should have bounded the shutdown; took {elapsed:?}"
    );
}

#[tokio::test]
async fn run_error_is_returned_unmodified_from_wait() {
    let (control, _outputs) = TestComponentDriver::transform(FailingTransformBuilder {
        failure: Failure::ReturnsError,
    })
    .await
    .expect("transform should build");

    let error = control.wait().await.expect_err("the transform's run should fail");

    // The alternate format includes every layer of context, so this also shows that none was added.
    assert_eq!(format!("{error:#}"), "transform failed on purpose");
}

#[tokio::test]
#[should_panic(expected = "transform panicked on purpose")]
async fn run_panic_is_resumed_from_wait() {
    let (control, _outputs) = TestComponentDriver::transform(FailingTransformBuilder {
        failure: Failure::Panics,
    })
    .await
    .expect("transform should build");

    let _ = control.wait().await;
}

#[tokio::test]
async fn run_returning_on_its_own_stops_the_children_it_spawned() {
    let control = TestComponentDriver::destination(SpawningDestinationBuilder {
        child: ChildBehavior::AbortedOnStop,
    })
    .await
    .expect("destination should build");
    control.supervisor().wait_for_children(1).await;

    // The destination returns after its first event, without shutdown being signalled. Its supervisor stops with it,
    // and aborts the child on the way.
    control.send_event(Event::Metric(Metric::counter("stop", 1.0))).await;
    control.supervisor().wait_for_children(0).await;

    control
        .wait()
        .await
        .expect("the destination's run should return cleanly");
}

#[tokio::test]
async fn input_drain_wait_returns_once_the_component_has_stopped() {
    let control = TestComponentDriver::destination(SpawningDestinationBuilder {
        child: ChildBehavior::AbortedOnStop,
    })
    .await
    .expect("destination should build");

    control.send_event(Event::Metric(Metric::counter("stop", 1.0))).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !control.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the destination should stop after its first event");

    // The destination took the event and then dropped its input, so there is nothing left to wait for.
    control.wait_until_input_drained().await;
    control
        .wait()
        .await
        .expect("the destination's run should return cleanly");
}

#[tokio::test]
async fn run_ignoring_shutdown_is_aborted_within_the_shutdown_budget() {
    let (control, _outputs) = TestComponentDriver::options()
        .with_shutdown_budget(Duration::from_millis(100))
        .source(SourceBuilderFor { source: || StuckSource })
        .await
        .expect("source should build");

    let started = Instant::now();
    let error = control
        .shutdown()
        .await
        .expect_err("aborting the source should make the shutdown unclean");
    let elapsed = started.elapsed();

    let supervisor_error = error
        .downcast_ref::<SupervisorError>()
        .expect("the error should come from the supervisor");
    assert!(
        matches!(supervisor_error, SupervisorError::ShutdownTimedOut { aborted: 1 }),
        "expected only the source to be aborted, got {supervisor_error:?}"
    );
    assert!(
        error.to_string().contains("didn't stop within the shutdown budget"),
        "the error should say that the component itself was aborted: {error:#}"
    );

    // The deadline for `run` to stop is the budget plus the five-second wait timeout, so finishing well inside that
    // shows that the budget bounded the run.
    assert!(
        elapsed < Duration::from_secs(3),
        "the 100ms budget should have bounded the shutdown; took {elapsed:?}"
    );
}

#[tokio::test]
async fn dataspace_values_asserted_by_run_are_retracted_once_it_returns() {
    let (control, _outputs) = TestComponentDriver::source(SourceBuilderFor {
        source: || AssertingSource,
    })
    .await
    .expect("source should build");
    control.wait_until_ready().await;

    let dataspace = control.supervisor().dataspace().clone();
    assert_eq!(
        dataspace.current_values::<SourceState>(IdentifierFilter::exact("state")),
        [SourceState("running")]
    );

    control
        .shutdown()
        .await
        .expect("source should stop cleanly on shutdown");
    assert_eq!(
        dataspace.current_values::<SourceState>(IdentifierFilter::exact("state")),
        []
    );
}

#[tokio::test]
async fn unbounded_wait_timeout_is_accepted_by_every_wait() {
    let control = TestComponentDriver::options()
        .with_wait_timeout(Duration::MAX)
        .destination(SpawningDestinationBuilder {
            child: ChildBehavior::StopsWithComponent,
        })
        .await
        .expect("destination should build");

    control.send_event(Event::Metric(Metric::counter("drained", 1.0))).await;
    control.wait_until_input_drained().await;
    control.shutdown().await.expect("destination should stop cleanly");
}
