use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_error::GenericError;

use super::OutputDefinition;
use crate::{
    components::{
        decoders::*, destinations::*, encoders::*, forwarders::*, relays::*, sources::*, transforms::*,
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
    outputs: Vec<OutputDefinition<EventType>>,
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
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(TestSource))
    }
}

impl MemoryBounds for TestSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestRelay;

#[async_trait]
impl Relay for TestRelay {
    async fn run(self: Box<Self>, _context: RelayContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestRelayBuilder {
    outputs: Vec<OutputDefinition<PayloadType>>,
}

impl TestRelayBuilder {
    pub fn default_output(payload_ty: PayloadType) -> Self {
        Self {
            outputs: vec![OutputDefinition::default_output(payload_ty)],
        }
    }

    pub fn multiple_outputs<'a>(outputs: impl Iterator<Item = &'a (Option<&'a str>, PayloadType)>) -> Self {
        Self {
            outputs: outputs
                .map(|(name, payload_ty)| match name {
                    Some(name) => OutputDefinition::named_output(name.to_string(), *payload_ty),
                    None => OutputDefinition::default_output(*payload_ty),
                })
                .collect(),
        }
    }
}

#[async_trait]
impl RelayBuilder for TestRelayBuilder {
    fn outputs(&self) -> &[OutputDefinition<PayloadType>] {
        &self.outputs
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Relay + Send>, GenericError> {
        Ok(Box::new(TestRelay))
    }
}

impl MemoryBounds for TestRelayBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestDecoder;

#[async_trait]
impl Decoder for TestDecoder {
    async fn run(self: Box<Self>, _context: DecoderContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestDecoderBuilder {
    input_payload_ty: PayloadType,
    output_event_ty: EventType,
}

impl TestDecoderBuilder {
    pub fn with_input_and_output_type(input_payload_ty: PayloadType, output_event_ty: EventType) -> Self {
        Self {
            input_payload_ty,
            output_event_ty,
        }
    }
}

#[async_trait]
impl DecoderBuilder for TestDecoderBuilder {
    fn input_payload_type(&self) -> PayloadType {
        self.input_payload_ty
    }

    fn output_event_type(&self) -> EventType {
        self.output_event_ty
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Decoder + Send>, GenericError> {
        Ok(Box::new(TestDecoder))
    }
}

impl MemoryBounds for TestDecoderBuilder {
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
    outputs: Vec<OutputDefinition<EventType>>,
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

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(Box::new(TestTransform))
    }
}

impl MemoryBounds for TestTransformBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

struct TestEncoder;

#[async_trait]
impl Encoder for TestEncoder {
    async fn run(self: Box<Self>, _context: EncoderContext) -> Result<(), GenericError> {
        Ok(())
    }
}

pub struct TestEncoderBuilder {
    input_event_ty: EventType,
    output_payload_ty: PayloadType,
}

impl TestEncoderBuilder {
    pub fn with_input_and_output_type(input_event_ty: EventType, output_payload_ty: PayloadType) -> Self {
        Self {
            input_event_ty,
            output_payload_ty,
        }
    }
}

#[async_trait]
impl EncoderBuilder for TestEncoderBuilder {
    fn input_event_type(&self) -> EventType {
        self.input_event_ty
    }

    fn output_payload_type(&self) -> PayloadType {
        self.output_payload_ty
    }

    async fn build(&self, _: ComponentContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        Ok(Box::new(TestEncoder))
    }
}

impl MemoryBounds for TestEncoderBuilder {
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
