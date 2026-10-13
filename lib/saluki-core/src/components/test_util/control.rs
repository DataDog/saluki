use std::time::Duration;

use saluki_error::GenericError;
use tokio::sync::{mpsc, oneshot};

use super::driver::RunOutcome;
use super::supervisor::{poll_until, TestComponentSupervisor};
use crate::components::ComponentContext;
use crate::data_model::event::Event;
use crate::runtime::SupervisorError;
use crate::topology::interconnect::Dispatchable;
use crate::topology::{EventsBuffer, TopologyContext};

/// The input type of a component that has no input.
///
/// Sources and relays produce items without consuming any. `NoInput` has no values, so a `ComponentControl<NoInput>`
/// has no way to send anything, and none of the sending methods apply to it.
pub enum NoInput {}

/// A handle for feeding and stopping a component under test.
///
/// Returned by [`TestComponentDriver`][super::TestComponentDriver] alongside the component's outputs. `I` is the type
/// of item the component consumes: [`EventsBuffer`], [`PayloadsBuffer`][crate::topology::PayloadsBuffer], or
/// [`NoInput`] for sources and relays.
///
/// The control holds the only sender for the component's input and owns the component's supervisor. Dropping it closes
/// the input and signals the supervisor to shut down, without waiting for either to finish; call
/// [`shutdown`][Self::shutdown] to stop the component and see how it ended.
#[must_use = "dropping a `ComponentControl` closes the component's input and signals its shutdown"]
pub struct ComponentControl<I> {
    pub(super) component_context: ComponentContext,
    pub(super) topology_context: TopologyContext,
    pub(super) supervisor: TestComponentSupervisor,
    pub(super) input: Option<mpsc::Sender<I>>,
    pub(super) outcome: oneshot::Receiver<RunOutcome>,
    pub(super) wait_timeout: Duration,
    pub(super) shutdown_budget: Duration,
}

impl<I> ComponentControl<I> {
    /// Returns the context of the component under test.
    pub fn component_context(&self) -> &ComponentContext {
        &self.component_context
    }

    /// Returns the topology context the component runs in.
    ///
    /// The health registry and dataspace it carries are the ones the component sees.
    pub fn topology_context(&self) -> &TopologyContext {
        &self.topology_context
    }

    /// Returns the component's supervisor.
    ///
    /// Children the component spawns land here. Use it to wait for them with
    /// [`wait_for_children`][TestComponentSupervisor::wait_for_children], to count them, or to reach the dataspace they
    /// share with the component. The component itself runs under the supervisor too, but isn't counted as one of its
    /// children.
    pub fn supervisor(&self) -> &TestComponentSupervisor {
        &self.supervisor
    }

    /// Returns `true` once the component's `run` has returned or panicked.
    ///
    /// A `run` that the supervisor aborts never counts as finished.
    pub fn is_finished(&self) -> bool {
        // The outcome is sent when `run` returns or panics, and never when it's aborted.
        !self.outcome.is_empty()
    }

    /// Waits until the component marks itself ready.
    ///
    /// # Panics
    ///
    /// Panics if the component doesn't mark itself ready within the wait timeout.
    pub async fn wait_until_ready(&self) {
        let identity = self.component_context.identity();
        let ready = self
            .topology_context
            .health_registry()
            .all_ready_matching(|id| *id == identity);

        if tokio::time::timeout(self.wait_timeout, ready).await.is_err() {
            let hint = if self.is_finished() {
                "; its `run` has already returned"
            } else {
                ""
            };
            panic!(
                "timed out after {:?} waiting for component '{}' to mark itself ready{}",
                self.wait_timeout, self.component_context, hint
            );
        }
    }

    /// Signals the component to shut down, without waiting for it to stop.
    ///
    /// Signals the component's supervisor first, then closes the component's input. This is the order production uses:
    /// stopping a topology signals every component's supervisor at once, while each component's upstream closes as that
    /// upstream stops. Sources and relays stop on the signal; every other kind of component stops once its input
    /// closes.
    ///
    /// Pair this with [`wait`][Self::wait] when the test needs to act between the signal and the component stopping.
    /// Calling this more than once has no further effect.
    pub fn signal_shutdown(&mut self) {
        self.supervisor.signal_shutdown();
        self.input = None;
    }

    /// Waits for the component, and every child it spawned, to stop.
    ///
    /// This doesn't signal shutdown by itself: call [`signal_shutdown`][Self::signal_shutdown] first, or use
    /// [`shutdown`][Self::shutdown], unless the component is expected to stop on its own. A component that stops on its
    /// own stops its supervisor too, which then stops the children the component spawned, the same as in a topology.
    ///
    /// # Errors
    ///
    /// If the component's `run` returns an error, that error is returned unmodified.
    ///
    /// If the supervisor doesn't shut down cleanly, such as when a child ignores shutdown and has to be aborted, the
    /// supervisor's error is returned, and `downcast_ref` recovers it as a [`SupervisorError`]. If both fail, the run's
    /// error is returned with the supervisor's error added as context, and `downcast_ref` recovers either one.
    ///
    /// If the component's `run` itself doesn't stop within the shutdown budget, the supervisor aborts it along with any
    /// children still running, and the supervisor's error is returned with an explanation added as context.
    ///
    /// When the component stops without shutdown being signalled, children aborted while its supervisor stops aren't
    /// reported, the same as in a topology: the supervisor reports that the component exited instead, and the driver
    /// returns the run's own result.
    ///
    /// # Panics
    ///
    /// If the component's `run` panics, the panic is resumed here, once its supervisor has stopped.
    ///
    /// Panics if the component and its supervisor don't stop within the shutdown budget plus the wait timeout. When
    /// shutdown is signalled, the budget bounds both, so the usual cause is a missing shutdown signal. A child that
    /// blocks its thread can also cause it, since aborting the child has no effect until it yields.
    ///
    /// [`SupervisorError`]: crate::runtime::SupervisorError
    pub async fn wait(self) -> Result<(), GenericError> {
        // Keep the input open while waiting: a component that is expected to stop on its own shouldn't be stopped by
        // its input closing instead.
        let Self {
            component_context,
            supervisor,
            input: _input,
            mut outcome,
            wait_timeout,
            shutdown_budget,
            ..
        } = self;

        // The supervisor only stops once the component has, after stopping every child the component spawned, so one
        // deadline covers all of them.
        let deadline = shutdown_budget.saturating_add(wait_timeout);
        let supervisor_result = match tokio::time::timeout(deadline, supervisor.wait()).await {
            Ok(supervisor_result) => supervisor_result,
            Err(_) if !outcome.is_empty() => panic!(
                "timed out after {:?} waiting for the supervisor of component '{}' to stop the children it spawned. \
                 Is a child blocking its thread, so that aborting it has no effect?",
                deadline, component_context
            ),
            Err(_) => panic!(
                "timed out after {:?} waiting for component '{}' to stop. Did you signal shutdown? A component blocked \
                 on an output that the test isn't draining doesn't stop on its own either.",
                deadline, component_context
            ),
        };

        // The supervisor doesn't stop until the component's worker has returned or been aborted, so the outcome is
        // settled by now: it was either sent, or dropped unsent by an abort.
        let run_result = match outcome.try_recv() {
            Ok(Ok(run_result)) => Some(run_result),
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            Err(_) => None,
        };

        match (run_result, supervisor_result) {
            // A component stopping on its own stops its supervisor, which reports that its significant child exited.
            // That is the expected consequence rather than a failure, so the run's own result is what counts.
            (Some(run_result), Ok(()) | Err(SupervisorError::SignificantChildExited)) => run_result,
            (Some(Ok(())), Err(supervisor_error)) => Err(supervisor_error.into()),
            (Some(Err(run_error)), Err(supervisor_error)) => Err(run_error.context(supervisor_error)),
            (None, Err(supervisor_error)) => Err(GenericError::from(supervisor_error).context(format!(
                "component '{}' didn't stop within the shutdown budget of {:?} and was aborted. If it dispatches more \
                 than an output can hold, drain the output concurrently, for example with \
                 `tokio::join!(control.shutdown(), output.collect())`.",
                component_context, shutdown_budget
            ))),
            (None, Ok(())) => panic!(
                "the supervisor of component '{}' stopped cleanly, but the component's `run` never finished",
                component_context
            ),
        }
    }

    /// Signals the component to shut down and waits for it to stop.
    ///
    /// Combines [`signal_shutdown`][Self::signal_shutdown] and [`wait`][Self::wait].
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`wait`][Self::wait].
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`wait`][Self::wait].
    pub async fn shutdown(mut self) -> Result<(), GenericError> {
        self.signal_shutdown();
        self.wait().await
    }
}

impl<I: Dispatchable> ComponentControl<I> {
    /// Sends `item` to the component's input.
    ///
    /// Waits for room if the input is full.
    ///
    /// # Panics
    ///
    /// Panics if shutdown was already signalled, if the component dropped its input, or if the input stays full for
    /// the whole wait timeout.
    pub async fn send(&self, item: I) {
        let sender = self.sender();
        match tokio::time::timeout(self.wait_timeout, sender.send(item)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => panic!(
                "component '{}' dropped its input; call `wait` to see how its `run` ended",
                self.component_context
            ),
            Err(_) => panic!(
                "timed out after {:?} sending to component '{}': its input stayed full. Is the component blocked on an \
                 output that the test isn't draining?",
                self.wait_timeout, self.component_context
            ),
        }
    }

    /// Waits until the component has received every item sent to it so far.
    ///
    /// The component has taken the items off its input, but may still be processing the last one. Returns immediately
    /// if the component has dropped its input, since it can't receive anything after that; [`wait`][Self::wait] then
    /// reports how its `run` ended.
    ///
    /// # Panics
    ///
    /// Panics if shutdown was already signalled, or if items are still queued once the wait timeout elapses.
    pub async fn wait_until_input_drained(&self) {
        let sender = self.sender();
        poll_until(
            self.wait_timeout,
            // Dropping the input discards whatever was still queued on it, so once it's closed there's no telling
            // received items from discarded ones. Either way, nothing more will be received.
            || sender.is_closed() || sender.capacity() == sender.max_capacity(),
            || {
                format!(
                    "component '{}' has drained its input; {} item(s) are still queued",
                    self.component_context,
                    sender.max_capacity() - sender.capacity()
                )
            },
        )
        .await;
    }

    fn sender(&self) -> &mpsc::Sender<I> {
        match self.input.as_ref() {
            Some(sender) => sender,
            None => panic!(
                "the input of component '{}' was closed when its shutdown was signalled",
                self.component_context
            ),
        }
    }
}

impl ComponentControl<EventsBuffer> {
    /// Sends a single event to the component's input.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`send`][Self::send].
    pub async fn send_event(&self, event: Event) {
        self.send_events([event]).await;
    }

    /// Sends `events` to the component's input, in order.
    ///
    /// Packs the events into as few event buffers as possible, filling each one before starting the next. Sends
    /// nothing if `events` is empty.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`send`][Self::send].
    pub async fn send_events<E>(&self, events: E)
    where
        E: IntoIterator<Item = Event>,
    {
        let mut buffer = EventsBuffer::default();
        for event in events {
            if let Some(event) = buffer.try_push(event) {
                self.send(std::mem::take(&mut buffer)).await;
                assert!(buffer.try_push(event).is_none(), "an empty events buffer has room");
            }
        }

        if !buffer.is_empty() {
            self.send(buffer).await;
        }
    }
}
