//! Adapts a built topology component into a [`Supervisable`] worker.
//!
//! Each component in a topology runs as the sole, _significant_ child of its own dedicated supervisor
//! (see [`BuiltTopology::spawn_inner`][super::built::BuiltTopology::spawn_inner]). To be supervised, a
//! component must be presented as a [`Supervisable`]: [`ComponentWorker`] wraps an already-built
//! component together with its context and bridges the two.
//!
//! A component is built once (during topology initialization) and run once, so the wrapped component is
//! held behind a take-once [`Mutex`] and consumed by [`Supervisable::initialize`]. Sources and relays
//! drive their own shutdown, so the supervisor-provided shutdown handle is installed into their context;
//! the remaining component kinds stop when their upstream channels close and ignore it.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::generic_error;
use tracing::{error_span, Instrument as _};

use crate::components::{
    decoders::{Decoder, DecoderContext},
    destinations::{Destination, DestinationContext},
    encoders::{Encoder, EncoderContext},
    forwarders::{Forwarder, ForwarderContext},
    relays::{Relay, RelayContext},
    sources::{Source, SourceContext},
    transforms::{Transform, TransformContext},
    ComponentContext,
};
use crate::runtime::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};

/// A built component paired with its context, runnable exactly once.
///
/// Implemented for each of the seven component kinds. Producing the run-future is where the per-kind
/// differences live: which context type is used, and whether the supervisor-provided shutdown handle is
/// installed into the context (sources and relays) or ignored (everything else).
pub(super) trait RunnableComponent: Send + 'static {
    /// Consumes the component and its context, returning the future that runs the component.
    ///
    /// `process_shutdown` is the shutdown signal of the component's dedicated supervisor. The run-future's
    /// allocations are attributed to the component's resource group by the process it runs in: the component's
    /// dedicated supervisor is named for the component, so no additional tracking is needed here.
    fn run_with_shutdown(self, process_shutdown: ShutdownHandle) -> SupervisorFuture;
}

/// A [`Supervisable`] adapter over a single built topology component.
///
/// Holds the component behind a take-once [`Mutex`] -- a component is built and run exactly once -- and
/// exposes it to a supervisor via [`Supervisable`]. The supervisor's shutdown grace period is bounded by
/// [`shutdown_strategy`][Supervisable::shutdown_strategy], which carries the shutdown timeout configured
/// for the topology.
pub(super) struct ComponentWorker<C> {
    component_context: ComponentContext,
    shutdown_timeout: Duration,
    inner: Mutex<Option<C>>,
}

impl<C: RunnableComponent> ComponentWorker<C> {
    /// Creates a new `ComponentWorker` for the given runnable component.
    ///
    /// `component_context` identifies the component (its kind and id); `shutdown_timeout` bounds how long
    /// the supervisor waits for the component to stop gracefully before aborting it.
    pub(super) fn new(component_context: ComponentContext, shutdown_timeout: Duration, runnable: C) -> Self {
        Self {
            component_context,
            shutdown_timeout,
            inner: Mutex::new(Some(runnable)),
        }
    }
}

#[async_trait]
impl<C: RunnableComponent> Supervisable for ComponentWorker<C> {
    fn name(&self) -> &str {
        self.component_context.component_type().as_str()
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(self.shutdown_timeout)
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        // The component is already built; there is no async initialization. Take it out of the take-once
        // slot and hand back its run-future. A second initialization is a bug (a component runs once).
        let runnable = self
            .inner
            .lock()
            .expect("component worker mutex poisoned")
            .take()
            .ok_or_else(|| {
                InitializationError::from(generic_error!(
                    "component worker '{}' was already initialized",
                    self.component_context
                ))
            })?;

        let span = error_span!(
            "component",
            "type" = self.component_context.component_type().as_str(),
            id = %self.component_context.component_id(),
        );
        Ok(Box::pin(runnable.run_with_shutdown(process_shutdown).instrument(span)))
    }
}

/// Generates a [`RunnableComponent`] implementation for a component kind.
///
/// `$inject_shutdown` controls whether the supervisor-provided shutdown handle is installed into the
/// context (sources and relays) or dropped (the channel-draining kinds).
macro_rules! runnable_component {
    ($name:ident, $component:path, $context:ty, inject_shutdown) => {
        /// Pairs a built component with its context for supervised execution.
        pub(super) struct $name {
            pub(super) component: Box<dyn $component + Send>,
            pub(super) context: $context,
        }

        impl RunnableComponent for $name {
            fn run_with_shutdown(self, process_shutdown: ShutdownHandle) -> SupervisorFuture {
                let Self {
                    component,
                    mut context,
                } = self;
                context.set_shutdown_handle(process_shutdown);
                Box::pin(component.run(context))
            }
        }
    };
    ($name:ident, $component:path, $context:ty) => {
        /// Pairs a built component with its context for supervised execution.
        pub(super) struct $name {
            pub(super) component: Box<dyn $component + Send>,
            pub(super) context: $context,
        }

        impl RunnableComponent for $name {
            fn run_with_shutdown(self, _process_shutdown: ShutdownHandle) -> SupervisorFuture {
                // This kind has no shutdown handle of its own: it stops when its upstream channels close.
                let Self { component, context } = self;
                Box::pin(component.run(context))
            }
        }
    };
}

runnable_component!(SourceRunnable, Source, SourceContext, inject_shutdown);
runnable_component!(RelayRunnable, Relay, RelayContext, inject_shutdown);
runnable_component!(DecoderRunnable, Decoder, DecoderContext);
runnable_component!(TransformRunnable, Transform, TransformContext);
runnable_component!(DestinationRunnable, Destination, DestinationContext);
runnable_component!(EncoderRunnable, Encoder, EncoderContext);
runnable_component!(ForwarderRunnable, Forwarder, ForwarderContext);
