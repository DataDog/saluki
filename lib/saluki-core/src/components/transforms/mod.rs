//! Transform component basics.

use std::marker::PhantomData;

use async_trait::async_trait;
use saluki_error::GenericError;

use crate::data_model::event::{Event, EventType};
use crate::topology::{BufferItem, EventsBuffer};

mod builder;
pub use self::builder::{SynchronousTransformBuilder, TransformBuilder};

mod context;
pub use self::context::TransformContext;

/// A transform.
///
/// Transforms sit in the middle of a topology, where events can be manipulated (for example, sampled, filtered, enriched)
/// before being sent to the next components in the topology. Examples of typical transforms include origin
/// enrichment, aggregation, and sampling.
///
/// `I` is the type of item the transform consumes, and `O` is the type of item it dispatches. Both default to [`Event`],
/// so `Transform` on its own is a transform that consumes and dispatches events of any type. A transform that works with
/// a single event type implements, for example, `Transform<Trace, TraceStats>`, and is wrapped in [`Typed`] to run in a
/// topology.
#[async_trait]
pub trait Transform<I: BufferItem = Event, O: BufferItem = Event> {
    /// Runs the transform.
    ///
    /// The transform context provides access primarily to the event stream, used to receive events sent to the
    /// transforms, and the forwarder, used to send events to the next components in the topology, as well as other
    /// information such as the component context.
    ///
    /// Transforms are expected to run indefinitely until their event stream is terminated, or an error occurs.
    ///
    /// # Errors
    ///
    /// If an unrecoverable error occurs while running, an error is returned.
    async fn run(self: Box<Self>, context: TransformContext<I, O>) -> Result<(), GenericError>;

    /// Returns the event types this transform consumes.
    ///
    /// When the topology is built, it checks that each transform can only receive event types it consumes.
    fn consumed_event_types(&self) -> EventType {
        I::EVENT_TYPES
    }
}

/// Runs a transform that works with specific item types as a transform that works with [`Event`].
///
/// A topology runs transforms that consume and dispatch [`EventsBuffer`]s. `Typed` wraps a `Transform<I, O>` so that
/// it can run in a topology: when the transform starts, its context is converted to a `TransformContext<I, O>`, which
/// converts each received buffer to a buffer of `I`, and each dispatched buffer of `O` back to an `EventsBuffer`. These
/// conversions move each buffer's collection of events, without touching the events themselves.
///
/// The topology checks, when it's built, that the transform can only receive events of type `I`.
pub struct Typed<C, I, O> {
    component: Box<C>,
    _types: PhantomData<fn(I) -> O>,
}

impl<C, I, O> Typed<C, I, O>
where
    C: Transform<I, O> + Send + 'static,
    I: BufferItem,
    O: BufferItem,
{
    /// Wraps the given transform.
    pub fn new(component: C) -> Self {
        Self {
            component: Box::new(component),
            _types: PhantomData,
        }
    }
}

#[async_trait]
impl<C, I, O> Transform for Typed<C, I, O>
where
    C: Transform<I, O> + Send + 'static,
    I: BufferItem,
    O: BufferItem,
{
    async fn run(self: Box<Self>, context: TransformContext) -> Result<(), GenericError> {
        self.component.run(context.into_typed()).await
    }

    fn consumed_event_types(&self) -> EventType {
        self.component.consumed_event_types()
    }
}

/// A synchronous transform.
///
/// Synchronous transforms are conceptually similar to transforms, designed to manipulate events. However, in some
/// cases, it can be beneficial to perform multiple transformation operations as a single step, without the overhead of
/// individual transform components. Synchronous transforms allow discrete transformation logic to be combined together
/// within a single transform component for processing efficiency.
pub trait SynchronousTransform {
    /// Transforms the events in the event buffer.
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer);
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use saluki_error::GenericError;

    use super::*;
    use crate::{
        accounting::{MemoryBounds, MemoryBoundsBuilder},
        components::{test_util::TestComponentDriver, BuildContext},
        data_model::event::{log::Log, metric::Metric, Event, EventKind as _, EventType},
        topology::OutputDefinition,
    };

    /// A transform that turns each metric it receives into a log holding the metric's name.
    struct MetricsToLogs;

    #[async_trait]
    impl Transform<Metric, Log> for MetricsToLogs {
        async fn run(self: Box<Self>, mut context: TransformContext<Metric, Log>) -> Result<(), GenericError> {
            context.take_health_handle().mark_ready();

            // `metrics` is a buffer of `Metric`, and the buffered dispatcher only accepts `Log`.
            while let Some(metrics) = context.events().next().await {
                let logs = metrics
                    .into_iter()
                    .map(|metric| Log::new(metric.context().name().to_string()));
                context.dispatcher().buffered()?.send_all(logs).await?;
            }

            Ok(())
        }
    }

    struct MetricsToLogsBuilder;

    #[async_trait]
    impl TransformBuilder for MetricsToLogsBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Log)];
            OUTPUTS
        }

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
            Ok(Box::new(Typed::new(MetricsToLogs)))
        }
    }

    impl MemoryBounds for MetricsToLogsBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    fn log_messages(events: Vec<Event>) -> Vec<String> {
        events
            .into_iter()
            .map(|event| {
                Log::try_from_event(event)
                    .expect("output holds logs")
                    .message()
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_typed_transform_consumes_and_dispatches_its_own_types() {
        let (control, mut outputs) = TestComponentDriver::transform(MetricsToLogsBuilder)
            .await
            .expect("transform should build");

        control
            .send_events([
                Event::Metric(Metric::counter("a", 1.0)),
                Event::Metric(Metric::counter("b", 1.0)),
            ])
            .await;
        control.shutdown().await.expect("transform should stop cleanly");

        assert_eq!(
            log_messages(outputs.default_output().collect_events().await),
            ["a", "b"]
        );
    }

    #[tokio::test]
    async fn a_typed_transform_never_sees_buffers_of_other_types() {
        let (control, mut outputs) = TestComponentDriver::transform(MetricsToLogsBuilder)
            .await
            .expect("transform should build");

        // The log buffer is dropped before it reaches the transform, which only consumes metrics.
        control.send_event(Event::Log(Log::new("ignored"))).await;
        control.send_event(Event::Metric(Metric::counter("a", 1.0))).await;
        control.shutdown().await.expect("transform should stop cleanly");

        assert_eq!(log_messages(outputs.default_output().collect_events().await), ["a"]);
    }

    #[test]
    fn a_typed_transform_reports_the_event_types_it_consumes() {
        let transform: Box<dyn Transform + Send> = Box::new(Typed::new(MetricsToLogs));
        assert_eq!(transform.consumed_event_types(), EventType::Metric);
    }
}
