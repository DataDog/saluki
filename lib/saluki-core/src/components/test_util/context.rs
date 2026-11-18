use saluki_common::sync::shutdown::{ShutdownCoordinator, ShutdownHandle};
use saluki_error::GenericError;
use tokio::sync::mpsc;

use super::driver::Harness;
use super::{DriverOptions, Output, Outputs};
use crate::components::{
    decoders::DecoderContext, destinations::DestinationContext, encoders::EncoderContext, forwarders::ForwarderContext,
    relays::RelayContext, sources::SourceContext, transforms::TransformContext, ComponentType,
};
use crate::data_model::event::EventType;
use crate::data_model::payload::PayloadType;
use crate::topology::{EventsBuffer, OutputDefinition, PayloadsBuffer, TopologyContext};

/// A component's context, built the way [`TestComponentDriver`][super::TestComponentDriver] builds it, together with
/// the other ends of its input and outputs.
///
/// Returned by the `*_context` methods of [`DriverOptions`], which build the context without running anything. Use it
/// to drive a piece of a component that takes the context, such as a decoder loop, with a real context.
///
/// The parts also keep the topology context alive, and the health registry with it: once the registry is dropped,
/// [`Health::live`][crate::health::Health::live] resolves immediately, which turns every run loop that polls it in a
/// `select!` into a busy loop. Move only `context` out of the parts, so that the rest stays alive until the parts go out
/// of scope.
#[must_use = "dropping the parts closes the context's input and outputs"]
pub struct ContextParts<C, I, O> {
    /// The context that the driver would pass to the component's `run`.
    ///
    /// The context of a source or relay comes with a shutdown handle installed, which fires on
    /// [`signal_shutdown`][Self::signal_shutdown] or once the parts are dropped.
    pub context: C,

    /// The only sender for the component's input, or `()` for sources and relays.
    ///
    /// Dropping it closes the input.
    pub input: I,

    /// The outputs of the component, or `()` for forwarders and destinations.
    pub outputs: O,

    topology_context: TopologyContext,
    shutdown: Option<ShutdownCoordinator>,
}

impl<C, I, O> ContextParts<C, I, O> {
    /// Returns the topology context that the context belongs to.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Fires the shutdown handle installed in the context of a source or relay.
    ///
    /// Has no effect on the context of any other component type, or once shutdown was already signalled.
    pub fn signal_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.shutdown();
        }
    }
}

impl Harness {
    fn into_parts<C, I, O>(self, context: C, input: I, outputs: O) -> ContextParts<C, I, O> {
        ContextParts {
            context,
            input,
            outputs,
            topology_context: self.topology_context,
            shutdown: None,
        }
    }
}

/// Builds a context without running anything.
///
/// Each method builds the context for its component type the way the matching run method does, from the same
/// options, but takes the output definitions rather than a builder. Nothing is spawned, so a component's children have
/// no supervisor to run under: run the code that spawns them inside
/// [`TestSupervisor::scope`][crate::runtime::test_util::TestSupervisor::scope].
///
/// Every method panics if called outside a Tokio runtime.
impl DriverOptions {
    /// Builds the context of a source with `outputs`.
    ///
    /// # Errors
    ///
    /// If `outputs` holds an invalid or duplicate output, an error is returned.
    pub fn source_context(
        self, outputs: &[OutputDefinition<EventType>],
    ) -> Result<ContextParts<SourceContext, (), Outputs<EventsBuffer>>, GenericError> {
        let harness = Harness::new(self, ComponentType::Source);
        let (dispatcher, outputs) = harness.outputs(outputs)?;

        let (coordinator, shutdown) = ShutdownHandle::paired();
        let mut context = SourceContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
        );
        context.set_shutdown_handle(shutdown);

        let outputs = Outputs::new(harness.identity.clone(), outputs);
        let mut parts = harness.into_parts(context, (), outputs);
        parts.shutdown = Some(coordinator);
        Ok(parts)
    }

    /// Builds the context of a relay with `outputs`.
    ///
    /// # Errors
    ///
    /// If `outputs` holds an invalid or duplicate output, an error is returned.
    pub fn relay_context(
        self, outputs: &[OutputDefinition<PayloadType>],
    ) -> Result<ContextParts<RelayContext, (), Outputs<PayloadsBuffer>>, GenericError> {
        let harness = Harness::new(self, ComponentType::Relay);
        let (dispatcher, outputs) = harness.outputs(outputs)?;

        let (coordinator, shutdown) = ShutdownHandle::paired();
        let mut context = RelayContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
        );
        context.set_shutdown_handle(shutdown);

        let outputs = Outputs::new(harness.identity.clone(), outputs);
        let mut parts = harness.into_parts(context, (), outputs);
        parts.shutdown = Some(coordinator);
        Ok(parts)
    }

    /// Builds the context of a decoder, with its default output.
    pub fn decoder_context(self) -> ContextParts<DecoderContext, mpsc::Sender<PayloadsBuffer>, Output<EventsBuffer>> {
        let harness = Harness::new(self, ComponentType::Decoder);
        let (dispatcher, output) = harness.default_output();
        let (input, consumer) = harness.input();
        let context = DecoderContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
            consumer,
        );

        harness.into_parts(context, input, output)
    }

    /// Builds the context of a transform with `outputs`.
    ///
    /// # Errors
    ///
    /// If `outputs` holds an invalid or duplicate output, an error is returned.
    pub fn transform_context(
        self, outputs: &[OutputDefinition<EventType>],
    ) -> Result<ContextParts<TransformContext, mpsc::Sender<EventsBuffer>, Outputs<EventsBuffer>>, GenericError> {
        let harness = Harness::new(self, ComponentType::Transform);
        let (dispatcher, outputs) = harness.outputs(outputs)?;
        let (input, consumer) = harness.input();
        let context = TransformContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
            consumer,
        );

        let outputs = Outputs::new(harness.identity.clone(), outputs);
        Ok(harness.into_parts(context, input, outputs))
    }

    /// Builds the context of an encoder, with its default output.
    pub fn encoder_context(self) -> ContextParts<EncoderContext, mpsc::Sender<EventsBuffer>, Output<PayloadsBuffer>> {
        let harness = Harness::new(self, ComponentType::Encoder);
        let (dispatcher, output) = harness.default_output();
        let (input, consumer) = harness.input();
        let context = EncoderContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
            consumer,
        );

        harness.into_parts(context, input, output)
    }

    /// Builds the context of a forwarder.
    pub fn forwarder_context(self) -> ContextParts<ForwarderContext, mpsc::Sender<PayloadsBuffer>, ()> {
        let harness = Harness::new(self, ComponentType::Forwarder);
        let (input, consumer) = harness.input();
        let context = ForwarderContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            consumer,
        );

        harness.into_parts(context, input, ())
    }

    /// Builds the context of a destination.
    pub fn destination_context(self) -> ContextParts<DestinationContext, mpsc::Sender<EventsBuffer>, ()> {
        let harness = Harness::new(self, ComponentType::Destination);
        let (input, consumer) = harness.input();
        let context = DestinationContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            consumer,
        );

        harness.into_parts(context, input, ())
    }
}
