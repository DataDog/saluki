use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;

use super::OutputDefinition;
use crate::{
    components::{
        destinations::*,
        forwarders::{Forwarder, ForwarderBuilder, ForwarderContext},
        renderers::*,
        sources::*,
        transforms::*,
        ComponentContext,
    },
    data_model::{event::EventType, payload::PayloadType},
};

struct TestSource;

#[async_trait]
impl Source for TestSource {
    async fn run(self: Box<Self>, _context: SourceContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestSourceBuilder {
    outputs: Vec<OutputDefinition>,
}

impl TestSourceBuilder {
    pub fn default_output(event_ty: EventType) -> Self {
        Self {
            outputs: vec![OutputDefinition::default_output(event_ty)],
        }
    }
}

#[async_trait]
impl SourceBuilder for TestSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition] {
        &self.outputs
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(TestSource))
    }
}

impl MemoryBounds for TestSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestTransform;

#[async_trait]
impl Transform for TestTransform {
    async fn run(self: Box<Self>, _context: TransformContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestTransformBuilder {
    input_event_ty: EventType,
    outputs: Vec<OutputDefinition>,
}

impl TestTransformBuilder {
    pub fn default_output(input_event_ty: EventType, output_event_ty: EventType) -> Self {
        Self {
            input_event_ty,
            outputs: vec![OutputDefinition::default_output(output_event_ty)],
        }
    }

    pub fn multiple_outputs<'a>(
        input_event_ty: EventType, outputs: impl Iterator<Item = &'a (Option<&'a str>, EventType)>,
    ) -> Self {
        Self {
            input_event_ty,
            outputs: outputs
                .map(|(name, event_ty)| match name {
                    Some(name) => OutputDefinition::named_output(name.to_string(), *event_ty),
                    None => OutputDefinition::default_output(*event_ty),
                })
                .collect(),
        }
    }
}

#[async_trait]
impl TransformBuilder for TestTransformBuilder {
    fn input_event_type(&self) -> EventType {
        self.input_event_ty
    }

    fn outputs(&self) -> &[OutputDefinition] {
        &self.outputs
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(TestTransform))
    }
}

impl MemoryBounds for TestTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestDestination;

#[async_trait]
impl Destination for TestDestination {
    async fn run(self: Box<Self>, _context: DestinationContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestDestinationBuilder {
    input_event_ty: EventType,
}

impl TestDestinationBuilder {
    pub fn with_input_type(input_event_ty: EventType) -> Self {
        Self { input_event_ty }
    }
}

#[async_trait]
impl DestinationBuilder for TestDestinationBuilder {
    fn input_event_type(&self) -> EventType {
        self.input_event_ty
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(TestDestination))
    }
}

impl MemoryBounds for TestDestinationBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestRenderer;

#[async_trait]
impl Renderer for TestRenderer {
    async fn run(self: Box<Self>, _context: RendererContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestRendererBuilder {
    input_event_ty: EventType,
    output_payload_ty: PayloadType,
}

impl TestRendererBuilder {
    pub fn with_input_and_output_type(input_event_ty: EventType, output_payload_ty: PayloadType) -> Self {
        Self {
            input_event_ty,
            output_payload_ty,
        }
    }
}

#[async_trait]
impl RendererBuilder for TestRendererBuilder {
    fn input_event_type(&self) -> EventType {
        self.input_event_ty
    }

    fn output_payload_type(&self) -> PayloadType {
        self.output_payload_ty
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Renderer + Send>, GenericError> {
        Ok(Box::new(TestRenderer))
    }
}

impl MemoryBounds for TestRendererBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestForwarder;

#[async_trait]
impl Forwarder for TestForwarder {
    async fn run(self: Box<Self>, _context: ForwarderContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestForwarderBuilder {
    input_payload_ty: PayloadType,
}

impl TestForwarderBuilder {
    pub fn with_input_type(input_payload_ty: PayloadType) -> Self {
        Self { input_payload_ty }
    }
}

#[async_trait]
impl ForwarderBuilder for TestForwarderBuilder {
    fn input_payload_type(&self) -> PayloadType {
        self.input_payload_ty
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Forwarder + Send>, GenericError> {
        Ok(Box::new(TestForwarder))
    }
}

impl MemoryBounds for TestForwarderBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}
