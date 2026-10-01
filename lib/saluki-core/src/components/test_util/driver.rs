use std::future::{poll_fn, Future};
use std::num::NonZeroUsize;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use saluki_common::resource_tracking::Track as _;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::{generic_error, GenericError};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use super::supervisor::{TestComponentSupervisor, TEST_SHUTDOWN_BUDGET};
use super::{ComponentControl, NoInput, Output, Outputs};
use crate::accounting::{ComponentRegistry, MemoryBoundsBuilder, MemoryLimiter};
use crate::components::{
    decoders::{DecoderBuilder, DecoderContext},
    destinations::{DestinationBuilder, DestinationContext},
    encoders::{EncoderBuilder, EncoderContext},
    forwarders::{ForwarderBuilder, ForwarderContext},
    relays::{RelayBuilder, RelayContext},
    sources::{SourceBuilder, SourceContext},
    transforms::{TransformBuilder, TransformContext},
    BuildContext, ComponentContext, ComponentType,
};
use crate::health::{Health, HealthRegistry};
use crate::runtime::state::{DataspaceRegistry, ResourceRegistry, CURRENT_DATASPACE};
use crate::runtime::{self, SupervisorFuture};
use crate::support::SubsystemIdentifier;
use crate::topology::component_worker::{
    ComponentWorker, DecoderRunnable, DestinationRunnable, EncoderRunnable, ForwarderRunnable, RelayRunnable,
    RunnableComponent, SourceRunnable, TransformRunnable,
};
use crate::topology::interconnect::{Consumer, Dispatchable, Dispatcher};
use crate::topology::{
    topology_identifier, ComponentId, ComponentOutputId, EventsBuffer, OutputDefinition, PayloadsBuffer,
    TopologyContext, DEFAULT_INTERCONNECT_CAPACITY,
};

/// Name of the topology that every component under test belongs to.
///
/// Matches the topology used by the `ComponentContext::test_*` constructors.
const TOPOLOGY_NAME: &str = "test";

/// Component ID used unless [`DriverOptions::with_component_id`] overrides it.
const DEFAULT_COMPONENT_ID: &str = "test";

/// Wait timeout used unless [`DriverOptions::with_wait_timeout`] overrides it.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs a single component in a test.
///
/// Each method takes a component builder and builds the component the way a topology would. It then connects a channel
/// to the component's input and to each of its outputs, and runs the component as the significant child of a
/// [`TestComponentSupervisor`], which is also how a topology runs it. What comes back is a [`ComponentControl`] for the
/// input and the shutdown, along with the component's outputs.
///
/// These methods use the default [`DriverOptions`]. Start from [`options`][Self::options] to change them.
///
/// See the [module documentation][super] for an example, and for what to watch out for.
pub enum TestComponentDriver {}

impl TestComponentDriver {
    /// Returns the default options, for changing before running a component.
    pub fn options() -> DriverOptions {
        DriverOptions::default()
    }

    /// Builds and runs a source with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::source`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::source`].
    pub async fn source<B: SourceBuilder>(
        builder: B,
    ) -> Result<(ComponentControl<NoInput>, Outputs<EventsBuffer>), GenericError> {
        DriverOptions::default().source(builder).await
    }

    /// Builds and runs a relay with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::relay`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::relay`].
    pub async fn relay<B: RelayBuilder>(
        builder: B,
    ) -> Result<(ComponentControl<NoInput>, Outputs<PayloadsBuffer>), GenericError> {
        DriverOptions::default().relay(builder).await
    }

    /// Builds and runs a decoder with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::decoder`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::decoder`].
    pub async fn decoder<B: DecoderBuilder>(
        builder: B,
    ) -> Result<(ComponentControl<PayloadsBuffer>, Output<EventsBuffer>), GenericError> {
        DriverOptions::default().decoder(builder).await
    }

    /// Builds and runs a transform with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::transform`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::transform`].
    pub async fn transform<B: TransformBuilder>(
        builder: B,
    ) -> Result<(ComponentControl<EventsBuffer>, Outputs<EventsBuffer>), GenericError> {
        DriverOptions::default().transform(builder).await
    }

    /// Builds and runs an encoder with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::encoder`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::encoder`].
    pub async fn encoder<B: EncoderBuilder>(
        builder: B,
    ) -> Result<(ComponentControl<EventsBuffer>, Output<PayloadsBuffer>), GenericError> {
        DriverOptions::default().encoder(builder).await
    }

    /// Builds and runs a forwarder with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::forwarder`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::forwarder`].
    pub async fn forwarder<B: ForwarderBuilder>(builder: B) -> Result<ComponentControl<PayloadsBuffer>, GenericError> {
        DriverOptions::default().forwarder(builder).await
    }

    /// Builds and runs a destination with the default options.
    ///
    /// # Errors
    ///
    /// See [`DriverOptions::destination`].
    ///
    /// # Panics
    ///
    /// See [`DriverOptions::destination`].
    pub async fn destination<B: DestinationBuilder>(
        builder: B,
    ) -> Result<ComponentControl<EventsBuffer>, GenericError> {
        DriverOptions::default().destination(builder).await
    }
}

/// Options for running a component with [`TestComponentDriver`].
///
/// Start from [`TestComponentDriver::options`], change what the test needs, and finish with the method for the
/// component's type, such as [`transform`][Self::transform].
#[derive(Clone)]
pub struct DriverOptions {
    component_id: ComponentId,
    interconnect_capacity: NonZeroUsize,
    wait_timeout: Duration,
    shutdown_budget: Duration,
    resource_registry: ResourceRegistry,
}

impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            component_id: ComponentId::try_from(DEFAULT_COMPONENT_ID).expect("default component ID should be valid"),
            interconnect_capacity: DEFAULT_INTERCONNECT_CAPACITY,
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
            shutdown_budget: TEST_SHUTDOWN_BUDGET,
            resource_registry: ResourceRegistry::new(),
        }
    }
}

impl DriverOptions {
    /// Sets the ID of the component under test.
    ///
    /// Defaults to `test`. The component runs in a topology named `test`, so a transform with the ID `mapper` has the
    /// identity `topology.test.transforms.mapper`. Change the ID when the component's telemetry, health, or resource
    /// names are part of what the test checks.
    ///
    /// # Panics
    ///
    /// Panics if `component_id` isn't a valid [`ComponentId`].
    #[track_caller]
    pub fn with_component_id(mut self, component_id: impl AsRef<str>) -> Self {
        let component_id = component_id.as_ref();
        self.component_id = ComponentId::try_from(component_id)
            .unwrap_or_else(|reason| panic!("invalid component ID '{}': {}", component_id, reason));
        self
    }

    /// Sets how many items the component's input and each of its outputs can hold.
    ///
    /// Defaults to 128, the same as a topology. A capacity of one makes backpressure straightforward to arrange: a
    /// single item the test hasn't read yet fills an output, and the component blocks on the next dispatch.
    pub fn with_interconnect_capacity(mut self, capacity: NonZeroUsize) -> Self {
        self.interconnect_capacity = capacity;
        self
    }

    /// Sets how long each wait lasts before it panics.
    ///
    /// Defaults to five seconds. Applies to every wait on a [`ComponentControl`] or an [`Output`]. Waiting for the
    /// component to stop gets the shutdown budget on top.
    ///
    /// Raise it for a component that is slow to start on a loaded machine. Lower it to make a test that is expected to
    /// panic on a deadline finish sooner.
    pub fn with_wait_timeout(mut self, wait_timeout: Duration) -> Self {
        self.wait_timeout = wait_timeout;
        self
    }

    /// Sets the shutdown budget of the component's supervisor.
    ///
    /// Defaults to five seconds. The budget covers the component and every child it spawned: whatever is still running
    /// when it runs out is aborted, and [`ComponentControl::wait`] reports it as an error.
    ///
    /// Shorten it to check that the component, or a child, stops on shutdown without making the test wait out the
    /// default budget when it doesn't.
    pub fn with_shutdown_budget(mut self, shutdown_budget: Duration) -> Self {
        self.shutdown_budget = shutdown_budget;
        self
    }

    /// Sets the resource registry that the component acquires resources from while it's built.
    ///
    /// Defaults to a new, empty registry. Pass a registry the test holds on to when it needs to check what the
    /// component acquired, or to hold a resource beforehand so the component can't acquire it.
    pub fn with_resource_registry(mut self, resource_registry: ResourceRegistry) -> Self {
        self.resource_registry = resource_registry;
        self
    }

    /// Builds and runs a source.
    ///
    /// Each output the builder declares gets its own channel. The source's shutdown handle fires when its shutdown is
    /// signalled through the returned [`ComponentControl`].
    ///
    /// # Errors
    ///
    /// If the builder declares an invalid or duplicate output, or fails to build the source, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn source<B: SourceBuilder>(
        self, builder: B,
    ) -> Result<(ComponentControl<NoInput>, Outputs<EventsBuffer>), GenericError> {
        let harness = Harness::new(self, ComponentType::Source);
        let (dispatcher, outputs) = harness.outputs(builder.outputs())?;

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

        let context = SourceContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
        );

        let outputs = Outputs::new(harness.identity.clone(), outputs);
        let control = harness.spawn(SourceRunnable { component, context }, None).await;
        Ok((control, outputs))
    }

    /// Builds and runs a relay.
    ///
    /// Each output the builder declares gets its own channel. The relay's shutdown handle fires when its shutdown is
    /// signalled through the returned [`ComponentControl`].
    ///
    /// # Errors
    ///
    /// If the builder declares an invalid or duplicate output, or fails to build the relay, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn relay<B: RelayBuilder>(
        self, builder: B,
    ) -> Result<(ComponentControl<NoInput>, Outputs<PayloadsBuffer>), GenericError> {
        let harness = Harness::new(self, ComponentType::Relay);
        let (dispatcher, outputs) = harness.outputs(builder.outputs())?;

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

        let context = RelayContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            dispatcher,
        );

        let outputs = Outputs::new(harness.identity.clone(), outputs);
        let control = harness.spawn(RelayRunnable { component, context }, None).await;
        Ok((control, outputs))
    }

    /// Builds and runs a decoder.
    ///
    /// A decoder always has exactly one output, the default output.
    ///
    /// # Errors
    ///
    /// If the builder fails to build the decoder, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn decoder<B: DecoderBuilder>(
        self, builder: B,
    ) -> Result<(ComponentControl<PayloadsBuffer>, Output<EventsBuffer>), GenericError> {
        let harness = Harness::new(self, ComponentType::Decoder);

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

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

        let control = harness.spawn(DecoderRunnable { component, context }, Some(input)).await;
        Ok((control, output))
    }

    /// Builds and runs a transform.
    ///
    /// Each output the builder declares gets its own channel.
    ///
    /// # Errors
    ///
    /// If the builder declares an invalid or duplicate output, or fails to build the transform, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn transform<B: TransformBuilder>(
        self, builder: B,
    ) -> Result<(ComponentControl<EventsBuffer>, Outputs<EventsBuffer>), GenericError> {
        let harness = Harness::new(self, ComponentType::Transform);
        let (dispatcher, outputs) = harness.outputs(builder.outputs())?;

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

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
        let control = harness
            .spawn(TransformRunnable { component, context }, Some(input))
            .await;
        Ok((control, outputs))
    }

    /// Builds and runs an encoder.
    ///
    /// An encoder always has exactly one output, the default output.
    ///
    /// # Errors
    ///
    /// If the builder fails to build the encoder, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn encoder<B: EncoderBuilder>(
        self, builder: B,
    ) -> Result<(ComponentControl<EventsBuffer>, Output<PayloadsBuffer>), GenericError> {
        let harness = Harness::new(self, ComponentType::Encoder);

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

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

        let control = harness.spawn(EncoderRunnable { component, context }, Some(input)).await;
        Ok((control, output))
    }

    /// Builds and runs a forwarder.
    ///
    /// # Errors
    ///
    /// If the builder fails to build the forwarder, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn forwarder<B: ForwarderBuilder>(
        self, builder: B,
    ) -> Result<ComponentControl<PayloadsBuffer>, GenericError> {
        let harness = Harness::new(self, ComponentType::Forwarder);

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

        let (input, consumer) = harness.input();
        let context = ForwarderContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            consumer,
        );

        Ok(harness
            .spawn(ForwarderRunnable { component, context }, Some(input))
            .await)
    }

    /// Builds and runs a destination.
    ///
    /// # Errors
    ///
    /// If the builder fails to build the destination, an error is returned.
    ///
    /// # Panics
    ///
    /// Panics if the component's supervisor fails to start.
    pub async fn destination<B: DestinationBuilder>(
        self, builder: B,
    ) -> Result<ComponentControl<EventsBuffer>, GenericError> {
        let harness = Harness::new(self, ComponentType::Destination);

        builder.specify_bounds(&mut harness.bounds_builder());
        let component = harness.build(builder.build(harness.build_context())).await?;
        drop(builder);

        let (input, consumer) = harness.input();
        let context = DestinationContext::new(
            &harness.topology_context,
            &harness.component_context,
            harness.component_registry.clone(),
            harness.register_health(),
            consumer,
        );

        Ok(harness
            .spawn(DestinationRunnable { component, context }, Some(input))
            .await)
    }
}

/// The setup shared by every component type: a topology context, and the wiring around the component.
///
/// Mirrors what a topology does for each component between adding it to the blueprint and running it.
struct Harness {
    options: DriverOptions,
    component_context: ComponentContext,
    identity: SubsystemIdentifier,
    dataspace: DataspaceRegistry,
    topology_context: TopologyContext,
    component_registry: ComponentRegistry,
}

impl Harness {
    fn new(options: DriverOptions, component_type: ComponentType) -> Self {
        let component_context = ComponentContext::new(
            &topology_identifier(TOPOLOGY_NAME),
            options.component_id.clone(),
            component_type,
        );
        let identity = component_context.identity();

        // As in a topology, the component's supervisor only starts once the component is built, but the dataspace the
        // two share exists from the start: building a component can already use it.
        let dataspace = DataspaceRegistry::default();

        // The topology context owns the health registry for as long as the component runs. A dropped registry makes
        // `Health::live` resolve immediately, which turns every run loop that polls it in a `select!` into a busy loop.
        let topology_context = TopologyContext::new(
            Arc::from(TOPOLOGY_NAME),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            dataspace.clone(),
        );

        Self {
            options,
            component_context,
            identity,
            dataspace,
            topology_context,
            component_registry: ComponentRegistry::default(),
        }
    }

    fn bounds_builder(&self) -> MemoryBoundsBuilder<'_> {
        self.component_registry.bounds_builder(&self.identity)
    }

    fn build_context(&self) -> BuildContext {
        BuildContext::new(self.component_context.clone(), self.options.resource_registry.clone())
    }

    /// Drives a component builder's `build`, attributing its allocations to the component and giving it the dataspace.
    async fn build<C, F>(&self, build: F) -> Result<C, GenericError>
    where
        F: Future<Output = Result<C, GenericError>>,
    {
        let token = self.component_registry.get_resource_group_token(&self.identity);
        CURRENT_DATASPACE
            .scope(self.dataspace.clone(), build.track_resources(token))
            .await
    }

    fn register_health(&self) -> Health {
        self.topology_context
            .health_registry()
            .register_component(&self.identity)
            .expect("the component under test should be the only one in its health registry")
    }

    /// Creates a dispatcher with one channel per output definition, returning the receiving ends as [`Output`]s.
    ///
    /// The dispatcher holds the only senders, so each output closes exactly when the component drops its dispatcher.
    fn outputs<T, D>(
        &self, definitions: &[OutputDefinition<D>],
    ) -> Result<(Dispatcher<T>, Vec<Output<T>>), GenericError>
    where
        T: Dispatchable,
        D: Copy,
    {
        let mut dispatcher = Dispatcher::new(self.component_context.clone());
        let mut outputs = Vec::with_capacity(definitions.len());

        for definition in definitions {
            let output_name =
                ComponentOutputId::from_definition(self.component_context.component_id().clone(), definition)
                    .map_err(|(output_id, reason)| {
                        generic_error!("Invalid component output ID '{}': {}", output_id, reason)
                    })?
                    .output();

            let (sender, receiver) = mpsc::channel(self.options.interconnect_capacity.get());
            dispatcher.add_output(output_name.clone())?;
            dispatcher.attach_sender_to_output(&output_name, sender)?;

            outputs.push(Output::new(
                self.identity.clone(),
                output_name,
                receiver,
                self.options.wait_timeout,
            ));
        }

        Ok((dispatcher, outputs))
    }

    /// Creates a dispatcher with only a default output, for decoders and encoders.
    fn default_output<T: Dispatchable>(&self) -> (Dispatcher<T>, Output<T>) {
        let (dispatcher, mut outputs) = self
            .outputs(&[OutputDefinition::default_output(())])
            .expect("a lone default output should always be valid");
        let output = outputs.pop().expect("one output definition should produce one output");
        (dispatcher, output)
    }

    /// Creates the component's input channel.
    ///
    /// The returned sender is the only one, so the input closes exactly when the control drops it.
    fn input<T: Dispatchable>(&self) -> (mpsc::Sender<T>, Consumer<T>) {
        let (sender, receiver) = mpsc::channel(self.options.interconnect_capacity.get());
        (sender, Consumer::new(self.component_context.clone(), receiver))
    }

    /// Starts the component's supervisor with the component running under it, returning the control for it.
    ///
    /// The component runs the way a topology runs it: wrapped in a [`ComponentWorker`], as the one static, temporary,
    /// significant child of a supervisor with [`AutoShutdown::AnySignificant`][crate::runtime::AutoShutdown]. So it
    /// runs in a process of its own, in the component's span, with the supervisor's shutdown budget covering it, and
    /// its `run` returning stops the supervisor along with every child the component spawned.
    async fn spawn<C, I>(self, runnable: C, input: Option<mpsc::Sender<I>>) -> ComponentControl<I>
    where
        C: RunnableComponent,
    {
        let (outcome_tx, outcome) = oneshot::channel();
        let worker = ComponentWorker::new(self.component_context.clone(), Reporting { runnable, outcome_tx });
        let worker = runtime::supervisable(worker).temporary().with_significant(true).build();
        let supervisor = TestComponentSupervisor::start_with_worker(
            &self.identity.to_string(),
            self.options.shutdown_budget,
            self.dataspace,
            worker,
        )
        .await;

        ComponentControl {
            component_context: self.component_context,
            topology_context: self.topology_context,
            supervisor,
            input,
            outcome,
            wait_timeout: self.options.wait_timeout,
            shutdown_budget: self.options.shutdown_budget,
        }
    }
}

/// How a component's `run` ended: its result if it returned, or the panic payload if it panicked.
pub(super) type RunOutcome = std::thread::Result<Result<(), GenericError>>;

/// A component that reports how its `run` ended to its [`ComponentControl`].
///
/// The outcome goes to the control rather than to the supervisor, so that [`ComponentControl::wait`] can return the
/// run's error unmodified, or resume its panic. The supervisor only needs to see the run stop. If the supervisor aborts
/// the run, the outcome is never sent.
struct Reporting<C> {
    runnable: C,
    outcome_tx: oneshot::Sender<RunOutcome>,
}

impl<C: RunnableComponent> RunnableComponent for Reporting<C> {
    const WANTS_SHUTDOWN_SIGNAL: bool = C::WANTS_SHUTDOWN_SIGNAL;

    fn run_with_shutdown(self, process_shutdown: ShutdownHandle) -> SupervisorFuture {
        let Self { runnable, outcome_tx } = self;
        let mut run = runnable.run_with_shutdown(process_shutdown);

        Box::pin(async move {
            // Catch a panic in any poll of `run`, rather than letting it unwind into the supervisor.
            let outcome = poll_fn(|cx| match catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(cx))) {
                Ok(Poll::Ready(run_result)) => Poll::Ready(Ok(run_result)),
                Ok(Poll::Pending) => Poll::Pending,
                Err(panic) => Poll::Ready(Err(panic)),
            })
            .await;

            // The control is gone if the test dropped it, in which case nobody is left to report to.
            let _ = outcome_tx.send(outcome);
            Ok(())
        })
    }
}
