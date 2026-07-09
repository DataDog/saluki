use async_trait::async_trait;
use saluki_common::strings::lower_alphanumeric;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::data_model::event::EventType;
use saluki_core::{
    components::{transforms::*, ComponentContext},
    topology::OutputDefinition,
};
use saluki_error::GenericError;
use tokio::select;
use tracing::{debug, error};

/// Chained transform.
///
/// Allows chaining multiple transforms together in a single component, which can avoid the overhead of receiving and
/// sending events multiple times when concurrency isn't required for processing.
///
/// ## Synchronous transforms
///
/// This component works with synchronous transforms only. If you need to chain asynchronous transforms, they must be
/// added to the topology normally.
#[derive(Default)]
pub struct ChainedConfiguration {
    subtransform_builders: Vec<(String, Box<dyn SynchronousTransformBuilder + Send + Sync>)>,
}

impl ChainedConfiguration {
    /// Adds a new synchronous transform to the chain.
    pub fn with_transform_builder<TB>(mut self, subtransform_name: &str, subtransform_builder: TB) -> Self
    where
        TB: SynchronousTransformBuilder + Send + Sync + 'static,
    {
        let subtransform_id = format!(
            "{}_{}",
            self.subtransform_builders.len(),
            lower_alphanumeric(subtransform_name)
        );
        self.subtransform_builders
            .push((subtransform_id, Box::new(subtransform_builder)));
        self
    }
}

impl MemoryBounds for ChainedConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<Chained>("component struct");

        for (subtransform_id, subtransform_builder) in self.subtransform_builders.iter() {
            let mut subtransform_bounds_builder = builder.subcomponent(subtransform_id);
            subtransform_builder.specify_bounds(&mut subtransform_bounds_builder);
        }
    }
}

#[async_trait]
impl TransformBuilder for ChainedConfiguration {
    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let mut subtransforms = Vec::new();
        for (subtransform_id, subtransform_builder) in &self.subtransform_builders {
            let subtransform = subtransform_builder.build(context.clone()).await?;
            subtransforms.push((subtransform_id.clone(), subtransform));
        }

        Ok(Box::new(Chained { subtransforms }))
    }

    fn input_event_type(&self) -> EventType {
        EventType::all_bits()
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::all_bits())];
        OUTPUTS
    }
}

pub struct Chained {
    subtransforms: Vec<(String, Box<dyn SynchronousTransform + Send>)>,
}

#[async_trait]
impl Transform for Chained {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();

        debug!(
            "Chained transform started with {} synchronous subtransform(s) present.",
            self.subtransforms.len()
        );

        // We have to re-associate each subtransform with their allocation group token here, as we don't have access to
        // it when the bounds are initially defined.
        let mut subtransforms = self
            .subtransforms
            .into_iter()
            .map(|(subtransform_id, subtransform)| {
                (
                    context.component_registry().get_or_create(subtransform_id).token(),
                    subtransform,
                )
            })
            .collect::<Vec<_>>();

        health.mark_ready();
        debug!("Chained transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(mut event_buffer) => {
                        for (allocation_token, transform) in &mut subtransforms {
                            let _guard = allocation_token.enter();
                            transform.transform_buffer(&mut event_buffer);
                        }

                        if let Err(e) = context.dispatcher().dispatch(event_buffer).await {
                            error!(error = %e, "Failed to dispatch events.");
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("Chained transform stopped.");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use saluki_core::accounting::{ComponentRegistry, MemoryLimiter};
    use saluki_core::components::ComponentContext;
    use saluki_core::data_model::event::{metric::Metric, Event};
    use saluki_core::health::HealthRegistry;
    use saluki_core::runtime::state::DataspaceRegistry;
    use saluki_core::runtime::Supervisor;
    use saluki_core::topology::interconnect::{Consumer, Dispatcher};
    use saluki_core::topology::{EventsBuffer, OutputName, TopologyContext};
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;

    use super::*;

    /// A synchronous subtransform that records the order in which it is invoked and appends a marker metric to the
    /// shared buffer, letting a test observe both subtransform application order and end-to-end event flow.
    struct RecordingTransform {
        marker: &'static str,
        order_log: Arc<Mutex<Vec<String>>>,
    }

    impl SynchronousTransform for RecordingTransform {
        fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
            self.order_log.lock().unwrap().push(self.marker.to_string());
            let _ = buffer.try_push(Event::Metric(Metric::counter(self.marker, 1.0)));
        }
    }

    struct RecordingBuilder {
        marker: &'static str,
        order_log: Arc<Mutex<Vec<String>>>,
    }

    impl MemoryBounds for RecordingBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    #[async_trait]
    impl SynchronousTransformBuilder for RecordingBuilder {
        async fn build(
            &self, _context: ComponentContext,
        ) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
            Ok(Box::new(RecordingTransform {
                marker: self.marker,
                order_log: self.order_log.clone(),
            }))
        }
    }

    fn recording_builder(marker: &'static str, order_log: &Arc<Mutex<Vec<String>>>) -> RecordingBuilder {
        RecordingBuilder {
            marker,
            order_log: order_log.clone(),
        }
    }

    /// Builds and drives the real `Chained` transform to completion over a single input buffer (containing one
    /// `input` metric), returning the metric names dispatched to the default output.
    async fn run_chained(config: ChainedConfiguration) -> Vec<String> {
        let component_context = ComponentContext::test_transform("test");
        let transform = config
            .build(component_context.clone())
            .await
            .expect("chained transform should build");

        // Wire a dispatcher whose default output we can drain.
        let mut dispatcher = Dispatcher::new(component_context.clone());
        dispatcher.add_output(OutputName::Default).expect("add default output");
        let (out_tx, mut out_rx) = mpsc::channel(4);
        dispatcher
            .attach_sender_to_output(&OutputName::Default, out_tx)
            .expect("attach default sender");

        // Feed a single input buffer, then close the input so the run loop terminates deterministically.
        let (in_tx, in_rx) = mpsc::channel(4);
        let consumer = Consumer::new(component_context.clone(), in_rx);
        let mut input = EventsBuffer::default();
        assert!(input.try_push(Event::Metric(Metric::counter("input", 1.0))).is_none());
        in_tx.send(input).await.expect("send input buffer");
        drop(in_tx);

        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = HealthRegistry::new()
            .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("test"))
            .expect("component was not previously registered");
        let supervisor_handle = Supervisor::new("test").expect("valid supervisor name").handle();

        let context = TransformContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            dispatcher,
            consumer,
            supervisor_handle,
        );

        transform.run(context).await.expect("chained run should succeed");

        let mut dispatched = Vec::new();
        while let Ok(buffer) = out_rx.try_recv() {
            for event in buffer {
                if let Event::Metric(metric) = event {
                    dispatched.push(metric.context().name().to_string());
                }
            }
        }
        dispatched
    }

    #[test]
    fn subtransform_ids_are_index_prefixed_and_sanitized() {
        let order_log = Arc::new(Mutex::new(Vec::new()));
        let config = ChainedConfiguration::default()
            .with_transform_builder("First Transform", recording_builder("a", &order_log))
            .with_transform_builder("second-transform", recording_builder("b", &order_log))
            .with_transform_builder("third", recording_builder("c", &order_log));

        // Each subtransform id is `{insertion index}_{lower_alphanumeric(name)}`: the index prefix disambiguates
        // duplicate names, spaces/hyphens become underscores, and letters are lowercased.
        let ids = config
            .subtransform_builders
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["0_first_transform", "1_second_transform", "2_third"]);
    }

    #[tokio::test]
    async fn subtransforms_run_in_insertion_order_and_output_is_dispatched() {
        let order_log = Arc::new(Mutex::new(Vec::new()));
        let config = ChainedConfiguration::default()
            .with_transform_builder("first", recording_builder("first", &order_log))
            .with_transform_builder("second", recording_builder("second", &order_log));

        let dispatched = run_chained(config).await;

        // Subtransforms are applied in the order they were added to the chain.
        assert_eq!(
            *order_log.lock().unwrap(),
            vec!["first".to_string(), "second".to_string()]
        );

        // Every subtransform sees the same buffer, and the accumulated buffer is dispatched to the default output: the
        // original `input` metric survives, followed by each subtransform's marker in application order.
        assert_eq!(
            dispatched,
            vec!["input".to_string(), "first".to_string(), "second".to_string()],
        );
    }
}
