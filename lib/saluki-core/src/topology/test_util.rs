use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;
use saluki_event::DataType;

use super::OutputDefinition;
use crate::components::{
    destinations::{Destination, DestinationBuilder, DestinationContext},
    sources::{Source, SourceBuilder, SourceContext},
    transforms::{Transform, TransformBuilder, TransformContext},
};

struct TestSource;

#[async_trait]
impl Source for TestSource {
    async fn run(self: Box<Self>, _context: SourceContext) -> Result<(), ()> {
        Ok(())
    }
}

pub struct TestSourceBuilder {
    outputs: Vec<OutputDefinition>,
}

impl TestSourceBuilder {
    pub fn default_output(data_ty: DataType) -> Self {
        Self {
            outputs: vec![OutputDefinition::default_output(data_ty)],
        }
    }
}

#[async_trait]
impl SourceBuilder for TestSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition] {
        &self.outputs
    }

    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(TestSource))
    }
}

impl MemoryBounds for TestSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestTransform;

#[async_trait]
impl Transform for TestTransform {
    async fn run(self: Box<Self>, _context: TransformContext) -> Result<(), ()> {
        Ok(())
    }
}

pub struct TestTransformBuilder {
    input_data_ty: DataType,
    outputs: Vec<OutputDefinition>,
}

impl TestTransformBuilder {
    pub fn default_output(input_data_ty: DataType, output_data_ty: DataType) -> Self {
        Self {
            input_data_ty,
            outputs: vec![OutputDefinition::default_output(output_data_ty)],
        }
    }

    pub fn multiple_outputs<'a>(
        input_data_ty: DataType, outputs: impl Iterator<Item = &'a (Option<&'a str>, DataType)>,
    ) -> Self {
        Self {
            input_data_ty,
            outputs: outputs
                .map(|(name, data_ty)| match name {
                    Some(name) => OutputDefinition::named_output(name.to_string(), *data_ty),
                    None => OutputDefinition::default_output(*data_ty),
                })
                .collect(),
        }
    }
}

#[async_trait]
impl TransformBuilder for TestTransformBuilder {
    fn input_data_type(&self) -> DataType {
        self.input_data_ty
    }

    fn outputs(&self) -> &[OutputDefinition] {
        &self.outputs
    }

    async fn build(&self) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(TestTransform))
    }
}

impl MemoryBounds for TestTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestDestination;

#[async_trait]
impl Destination for TestDestination {
    async fn run(self: Box<Self>, _context: DestinationContext) -> Result<(), ()> {
        Ok(())
    }
}

pub struct TestDestinationBuilder {
    input_data_ty: DataType,
}

impl TestDestinationBuilder {
    pub fn with_input_type(input_data_ty: DataType) -> Self {
        Self { input_data_ty }
    }
}

#[async_trait]
impl DestinationBuilder for TestDestinationBuilder {
    fn input_data_type(&self) -> DataType {
        self.input_data_ty
    }

    async fn build(&self) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(TestDestination))
    }
}

impl MemoryBounds for TestDestinationBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}
