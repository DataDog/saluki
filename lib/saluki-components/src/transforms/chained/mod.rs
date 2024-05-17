use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{components::transforms::*, topology::OutputDefinition};
use saluki_error::GenericError;
use saluki_event::DataType;
use tracing::{debug, error};

/// Chained transform.
///
/// Allows chaining multiple transforms together in a single component, which can avoid the overhead of receiving and
/// sending events multiple times when concurrency is not required for processing.
///
/// ## Synchronous tranforms
///
/// This component works with synchronous transforms only. If you need to chain asynchronous transforms, they must be
/// added to the topology normally.
#[derive(Default)]
pub struct ChainedConfiguration {
    transform_builders: Vec<Box<dyn SynchronousTransformBuilder + Send + Sync>>,
}

impl ChainedConfiguration {
    /// Adds a new synchronous transform to the chain.
    pub fn with_transform_builder<TB>(mut self, transform_builder: TB) -> Self
    where
        TB: SynchronousTransformBuilder + Send + Sync + 'static,
    {
        self.transform_builders.push(Box::new(transform_builder));
        self
    }
}

impl MemoryBounds for ChainedConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        for (i, transform_builder) in self.transform_builders.iter().enumerate() {
            let mut subtransform_builder = builder.component(i.to_string());
            transform_builder.specify_bounds(&mut subtransform_builder);
        }
    }
}

#[async_trait]
impl TransformBuilder for ChainedConfiguration {
    async fn build(&self) -> Result<Box<dyn Transform + Send>, GenericError> {
        let mut transforms = Vec::new();
        for transform_builder in &self.transform_builders {
            transforms.push(transform_builder.build().await?);
        }

        Ok(Box::new(Chained { transforms }))
    }

    fn input_data_type(&self) -> DataType {
        DataType::all()
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::all())];

        OUTPUTS
    }
}

pub struct Chained {
    transforms: Vec<Box<dyn SynchronousTransform + Send>>,
}

#[async_trait]
impl Transform for Chained {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), ()> {
        debug!(
            "Chained transform started {} synchronous tranform(s) present.",
            self.transforms.len()
        );

        while let Some(mut event_buffer) = context.event_stream().next().await {
            for transform in &self.transforms {
                transform.transform_buffer(&mut event_buffer);
            }

            if let Err(e) = context.forwarder().forward(event_buffer).await {
                error!(error = %e, "Failed to forward events.");
            }
        }

        debug!("Chained transform stopped.");

        Ok(())
    }
}
