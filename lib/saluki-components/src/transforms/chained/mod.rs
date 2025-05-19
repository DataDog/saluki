use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::strings::lower_alphanumeric;
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
/// sending events multiple times when concurrency is not required for processing.
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
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let mut subtransforms = Vec::new();
        for (subtransform_id, subtransform_builder) in &self.subtransform_builders {
            let subtransform = subtransform_builder.build().await?;
            subtransforms.push((subtransform_id.clone(), subtransform));
        }

        Ok(Box::new(Chained { subtransforms }))
    }

    fn input_event_type(&self) -> EventType {
        EventType::all_bits()
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(EventType::all_bits())];

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
                maybe_events = context.event_stream().next() => match maybe_events {
                    Some(mut event_buffer) => {
                        for (allocation_token, transform) in &mut subtransforms {
                            let _guard = allocation_token.enter();
                            transform.transform_buffer(&mut event_buffer);
                        }

                        if let Err(e) = context.dispatcher().dispatch_buffer(event_buffer).await {
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
