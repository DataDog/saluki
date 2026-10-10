//! The region host: the topology transform that drives a [`RegionDriver`].

use std::{
    any::Any,
    collections::{HashSet, VecDeque},
    num::NonZeroUsize,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::FutureExt as _;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::{Counter, MetricsBuilder};
use tokio::{
    pin, select,
    sync::{mpsc, watch},
    task::coop::consume_budget,
    time::{sleep_until, Instant},
};
use tracing::{debug, error, warn};

use super::{
    driver::{ExitId, ExitSink, PanicOutcome, PanicSite, RegionDriver, RegionPanic, RegionTurn},
    mailbox::{ControlError, ControlMailbox, ControlRequest},
};
use crate::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext, ComponentContext,
    },
    data_model::event::{Event, EventType},
    execution::{Budget, DrainPolicy, Progress},
    health::Health,
    observability::ComponentMetricsExt as _,
    topology::{ComponentId, EventsBuffer, EventsDispatcher, OutputDefinition, DEFAULT_EVENTS_BUFFER_CAPACITY},
};

/// The shortest interval between two periodic calls to [`RegionDriver::publish_telemetry`].
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

type DriverFactory<D> = Box<dyn Fn(&BuildContext) -> Result<D, GenericError> + Send + Sync>;
type ControlReceiver<C> = mpsc::UnboundedReceiver<ControlRequest<C>>;

/// Builds a region host: a transform that runs a [`RegionDriver`] in a topology.
///
/// The transform takes metrics as input. Its outputs are the region's **exits**, as named outputs in the order given:
/// exit `i` is [`ExitId::new(i)`][ExitId::new]. Every exit stays declared for the life of the component, whether or not
/// the driver ever writes to it, so a later program can use an exit that an earlier one left idle. An exit that is not
/// connected to any downstream component discards what it is sent, counting it as discarded, like any other output.
///
/// # Running
///
/// Once it reports the region ready, the host calls [`RegionDriver::start`] once. It then waits for, and handles, in
/// this order of priority: liveness probes, control messages, periodic telemetry publishing, resuming a yielded call,
/// the driver's deadline, and input batches. It reads input only while no call is yielded and the driver [accepts
/// input][RegionDriver::accepts_input], and processes up to `work_per_turn` batches back to back before it checks for
/// control messages and timers again.
///
/// Control messages are handled while a call is yielded, between its turns. The driver's deadline is not armed while
/// a call is yielded or the input has ended: a deadline that passed meanwhile is due as soon as the call completes,
/// before any further input, and so is each other deadline the driver returns that has also passed, as a timer bursts
/// to catch up. A driver that keeps the same past deadline after an [`advance_time`][RegionDriver::advance_time] that
/// did no work, writing nothing and charging nothing to its budget, would be called again at once, forever: the host
/// logs an error and does not arm that deadline again until the driver's deadline changes, and a debug build fails an
/// assertion, so that tests catch the driver bug. See [`next_deadline`][RegionDriver::next_deadline] for the rule.
///
/// After every call into the driver, the host sends what the call wrote, buffer by buffer, in the order it was
/// written, awaiting each send: when a downstream component is slow, the region waits for it, as native components do,
/// and the input waits upstream. While a send waits, the host still answers liveness probes and handles control
/// messages, whose output it queues behind the buffers already waiting.
///
/// When the input ends, the host completes any yielded call, calls [`RegionDriver::finish`] with
/// [`DrainPolicy::Shutdown`], resumes it until it is done, sends its output, publishes telemetry a last time, and
/// stops. If a panic ends the finish before it is done and the driver continues, the host calls `finish` again, until
/// a call completes it, so that the driver completes the work the panic ended.
///
/// # Supervision
///
/// Every call that can run kernel code runs under [`catch_unwind`]. When a call panics, the host discards everything
/// the call wrote, so nothing of it is delivered, and counts the lost events (see [`RegionPanic::lost_events`]) in
/// `component_events_dropped_total`, tagged `intentional:false`, `drop_reason:region_failure`, and
/// `loss_class:invalid`, and the panic in `component_errors_total`, tagged `error_type:panic`. It then calls
/// [`RegionDriver::reset_after_panic`]: on [`PanicOutcome::Continue`] the region keeps running, and on
/// [`PanicOutcome::Fail`] the transform returns the error, which fails the component.
///
/// A caught panic still runs the process's panic hook, before it unwinds: the default hook prints the panic message,
/// and a hook that a process installs to report panics, such as one that reports every panic to a test harness as a
/// failure, sees caught region panics too.
///
/// # Memory bounds
///
/// Besides the component itself and the bounds given with [`with_bounds`][Self::with_bounds], the host accounts for
/// one full events buffer per exit, held while it waits for downstream. A driver that can write more than one full
/// buffer's worth of events to one exit per call accounts for the rest in its own bounds.
pub struct RegionHostBuilder<D>
where
    D: RegionDriver,
{
    outputs: Vec<OutputDefinition<EventType>>,
    exit_names: Arc<[String]>,
    work_per_turn: NonZeroUsize,
    turn_budget: Budget,
    factory: DriverFactory<D>,
    mailbox: Mutex<Option<ControlMailbox<D::Control>>>,
    bounds: Option<Box<dyn MemoryBounds + Send + Sync>>,
}

impl<D> RegionHostBuilder<D>
where
    D: RegionDriver,
{
    /// Creates a builder whose transform gets its driver from `factory`.
    ///
    /// `exits` are the names of the region's exits, which become the names of the transform's outputs, in order.
    /// `work_per_turn` is the largest number of input batches the host processes before it checks for control
    /// messages and timers again. `mailbox` is the receiving side of the region's [control
    /// mailbox][super::control_mailbox]. `factory` builds the driver when the topology builds the transform, with the
    /// build context of the region's component.
    ///
    /// # Errors
    ///
    /// If an exit name is not a valid output name, if two exits have the same name, or if there are more than 65,536
    /// exits, an error is returned.
    pub fn new<I, S, F>(
        exits: I, work_per_turn: NonZeroUsize, mailbox: ControlMailbox<D::Control>, factory: F,
    ) -> Result<Self, GenericError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        F: Fn(&BuildContext) -> Result<D, GenericError> + Send + Sync + 'static,
    {
        let exit_names = exits.into_iter().map(Into::into).collect::<Vec<String>>();
        if exit_names.len() > usize::from(u16::MAX) + 1 {
            return Err(generic_error!(
                "A region has {} exits, but at most 65536 are supported. Split the region's program across more \
                 regions.",
                exit_names.len()
            ));
        }

        let mut seen = HashSet::new();
        for name in &exit_names {
            ComponentId::try_from(name.as_str()).map_err(|reason| {
                generic_error!(
                    "Region exit name '{}' is invalid. Exit names become output names, which follow the rules of \
                     component IDs: {}.",
                    name,
                    reason
                )
            })?;
            if !seen.insert(name.as_str()) {
                return Err(generic_error!(
                    "Region exit '{}' is listed more than once. Give each exit a distinct name.",
                    name
                ));
            }
        }

        let outputs = exit_names
            .iter()
            .map(|name| OutputDefinition::named_output(name.clone(), EventType::Metric))
            .collect();

        Ok(Self {
            outputs,
            exit_names: exit_names.into(),
            work_per_turn,
            turn_budget: Budget::unlimited(),
            factory: Box::new(factory),
            mailbox: Mutex::new(Some(mailbox)),
            bounds: None,
        })
    }

    /// Creates a builder whose transform runs `driver`.
    ///
    /// The arguments are as for [`new`][Self::new]. The builder can build a single transform, since it hands over
    /// `driver` itself.
    ///
    /// # Errors
    ///
    /// If an exit name is not a valid output name, if two exits have the same name, or if there are more than 65,536
    /// exits, an error is returned.
    pub fn from_driver<I, S>(
        exits: I, work_per_turn: NonZeroUsize, mailbox: ControlMailbox<D::Control>, driver: D,
    ) -> Result<Self, GenericError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let driver = Mutex::new(Some(driver));
        Self::new(exits, work_per_turn, mailbox, move |_| {
            driver
                .lock()
                .map_err(|_| generic_error!("The region driver's lock is poisoned."))?
                .take()
                .ok_or_else(|| {
                    generic_error!(
                        "The region driver was already used to build a transform. A region host builder created from \
                         a driver builds one transform; create it with a driver factory to build several."
                    )
                })
        })
    }

    /// Sets the work budget each call into the driver starts with.
    ///
    /// Defaults to an unlimited budget, which suits drivers whose calls are bounded by the size of one batch.
    pub fn with_turn_budget(mut self, budget: Budget) -> Self {
        self.turn_budget = budget;
        self
    }

    /// Sets the memory bounds of the driver, such as those of the kernels it runs, which the host adds to its own.
    pub fn with_bounds<B>(mut self, bounds: B) -> Self
    where
        B: MemoryBounds + Send + Sync + 'static,
    {
        self.bounds = Some(Box::new(bounds));
        self
    }
}

impl<D> MemoryBounds for RegionHostBuilder<D>
where
    D: RegionDriver,
{
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built, which holds the driver.
            .with_single_value::<RegionHost<D>>("component struct");
        builder.firm().with_expr(in_flight_exit_buffers(self.exit_names.len()));

        if let Some(bounds) = &self.bounds {
            bounds.specify_bounds(builder);
        }
    }
}

/// Returns the memory of one full events buffer per exit, held while the host waits for downstream.
fn in_flight_exit_buffers(exit_count: usize) -> UsageExpr {
    UsageExpr::product(
        "in-flight exit buffers",
        UsageExpr::constant("exits", exit_count),
        UsageExpr::sum(
            "exit buffer",
            UsageExpr::struct_size::<(ExitId, EventsBuffer)>("queued exit buffer"),
            UsageExpr::product(
                "events",
                UsageExpr::struct_size::<Event>("event"),
                UsageExpr::constant("events buffer capacity", DEFAULT_EVENTS_BUFFER_CAPACITY),
            ),
        ),
    )
}

#[async_trait]
impl<D> TransformBuilder for RegionHostBuilder<D>
where
    D: RegionDriver,
{
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let driver = (self.factory)(&context).map_err(|e| {
            e.context(format!(
                "Failed to build the driver of region '{}'",
                context.component_id()
            ))
        })?;
        let mailbox = self
            .mailbox
            .lock()
            .map_err(|_| generic_error!("The region's control mailbox lock is poisoned."))?
            .take()
            .ok_or_else(|| {
                generic_error!(
                    "The control mailbox of region '{}' was already used to build a transform. A region host builder \
                     builds one transform, since each region has its own mailbox.",
                    context.component_id()
                )
            })?;

        let (control, running) = mailbox.into_parts();
        Ok(Box::new(RegionHost {
            driver,
            exit_names: Arc::clone(&self.exit_names),
            work_per_turn: self.work_per_turn,
            turn_budget: self.turn_budget,
            control,
            running,
            telemetry: HostTelemetry::new(context.component_context()),
        }))
    }
}

/// The telemetry the host records itself, as opposed to the driver's.
struct HostTelemetry {
    events_lost: Counter,
    panics: Counter,
}

impl HostTelemetry {
    fn new(context: &ComponentContext) -> Self {
        let builder = MetricsBuilder::from_component_context(context);
        Self {
            events_lost: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:false", "drop_reason:region_failure", "loss_class:invalid"],
            ),
            panics: builder.register_counter_with_tags("component_errors_total", ["error_type:panic"]),
        }
    }
}

/// The region host transform.
pub(super) struct RegionHost<D>
where
    D: RegionDriver,
{
    driver: D,
    exit_names: Arc<[String]>,
    work_per_turn: NonZeroUsize,
    turn_budget: Budget,
    control: ControlReceiver<D::Control>,
    running: watch::Sender<bool>,
    telemetry: HostTelemetry,
}

#[async_trait]
impl<D> Transform for RegionHost<D>
where
    D: RegionDriver,
{
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let RegionHost {
            driver,
            exit_names,
            work_per_turn,
            turn_budget,
            control,
            running,
            telemetry,
        } = *self;
        let mut health = context.take_health_handle();
        let mut runner = Runner::new(driver, exit_names.len(), turn_budget, telemetry);
        let mut control = Some(control);

        // Timers are armed from the driver's state before every wait, and only reset when what they wait for changes.
        let deadline_timer = sleep_until(Instant::now());
        pin!(deadline_timer);
        let mut armed_deadline = None;
        let publish_timer = sleep_until(Instant::now());
        pin!(publish_timer);
        let mut armed_publish = None;

        let mut input_open = true;
        let mut finishing = false;

        health.mark_ready();
        runner.start();
        // From here on the mailbox is read: whoever controls the region can start timing its messages.
        running.send_replace(true);
        debug!(exits = exit_names.len(), "Region host started.");

        loop {
            if !input_open && runner.pending.is_none() {
                if runner.finished {
                    break;
                }

                if finishing {
                    // A panic ended the finish before it was done, and the driver continues: call `finish` again, which
                    // resumes the work the panic ended. Let other tasks run first, since nothing here waits.
                    debug!("Region host finish was ended by a panic. Finishing the region again...");
                    consume_budget().await;
                } else {
                    debug!("Region host input ended. Finishing the region...");
                    finishing = true;
                }
                runner.finish()?;
                dispatch(
                    &mut runner,
                    context.dispatcher(),
                    &exit_names,
                    &mut health,
                    &mut control,
                )
                .await?;
                continue;
            }

            let wanted_deadline = if input_open && runner.pending.is_none() {
                runner.wanted_deadline().map(Instant::from_std)
            } else {
                None
            };
            if wanted_deadline != armed_deadline {
                if let Some(deadline) = wanted_deadline {
                    deadline_timer.as_mut().reset(deadline);
                }
                armed_deadline = wanted_deadline;
            }

            let wanted_publish = runner.publish_deadline();
            if wanted_publish != armed_publish {
                if let Some(at) = wanted_publish {
                    publish_timer.as_mut().reset(at);
                }
                armed_publish = wanted_publish;
            }

            let poll_input = input_open && runner.pending.is_none() && runner.driver.accepts_input();

            select! {
                biased;

                _ = health.live() => {},

                request = next_control(&mut control) => match request {
                    Some(request) => {
                        runner.control(request)?;
                        dispatch(&mut runner, context.dispatcher(), &exit_names, &mut health, &mut control).await?;
                    }
                    None => control = None,
                },

                _ = &mut publish_timer, if armed_publish.is_some() => {
                    armed_publish = None;
                    runner.publish()?;
                },

                // Resuming is always possible, so consume the task's cooperative budget to let other tasks run
                // between the turns of a long drain, even one that writes nothing.
                _ = consume_budget(), if runner.pending.is_some() => {
                    runner.resume()?;
                    dispatch(&mut runner, context.dispatcher(), &exit_names, &mut health, &mut control).await?;
                },

                _ = &mut deadline_timer, if armed_deadline.is_some() => {
                    let fired = armed_deadline.take().expect("armed per select guard");
                    runner.advance_time(fired.into_std())?;
                    dispatch(&mut runner, context.dispatcher(), &exit_names, &mut health, &mut control).await?;
                },

                maybe_batch = context.events().next(), if poll_input => match maybe_batch {
                    Some(batch) => {
                        runner.process(batch)?;
                        dispatch(&mut runner, context.dispatcher(), &exit_names, &mut health, &mut control).await?;

                        // Keep processing batches that are already waiting, up to the turn's limit, before checking
                        // for control messages and timers again.
                        for _ in 1..work_per_turn.get() {
                            if runner.pending.is_some() || !runner.driver.accepts_input() {
                                break;
                            }
                            match context.events().next().now_or_never() {
                                Some(Some(batch)) => {
                                    runner.process(batch)?;
                                    dispatch(&mut runner, context.dispatcher(), &exit_names, &mut health, &mut control)
                                        .await?;
                                }
                                Some(None) => {
                                    input_open = false;
                                    break;
                                }
                                None => break,
                            }
                        }
                    }
                    None => input_open = false,
                },
            }
        }

        runner.publish()?;
        debug!("Region host stopped.");

        Ok(())
    }
}

/// Waits for the next control request, or forever once the mailbox is closed.
async fn next_control<C>(control: &mut Option<ControlReceiver<C>>) -> Option<ControlRequest<C>> {
    match control {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Sends every buffer waiting in the runner's outbox, in order, awaiting each send.
///
/// While a send waits for downstream, this keeps answering liveness probes and handling control messages. The output
/// of a control message is queued behind the buffers already waiting, and sent before this returns.
async fn dispatch<D>(
    runner: &mut Runner<D>, dispatcher: &EventsDispatcher, exit_names: &[String], health: &mut Health,
    control: &mut Option<ControlReceiver<D::Control>>,
) -> Result<(), GenericError>
where
    D: RegionDriver,
{
    while let Some((exit, buffer)) = runner.outbox.pop_front() {
        let exit_name = exit_names[exit.get()].as_str();
        let send = dispatcher.dispatch_named(exit_name, buffer);
        pin!(send);

        loop {
            select! {
                biased;

                _ = health.live() => {},

                result = &mut send => {
                    if let Err(e) = result {
                        error!(exit = exit_name, error = %e, "Failed to dispatch events.");
                    }
                    break;
                },

                request = next_control(control) => match request {
                    Some(request) => runner.control(request)?,
                    None => *control = None,
                },
            }
        }
    }

    Ok(())
}

/// A call into the driver that has yielded and is waiting to be resumed.
#[derive(Clone, Copy, Debug)]
enum Pending {
    /// A call to `process`, for a batch of this many events.
    Batch { events: u64 },

    /// A call to `advance_time`.
    Time,

    /// A call to `finish`.
    Finish,
}

/// How a supervised call into the driver ended.
enum Called<R> {
    Returned(R),
    Panicked(RegionPanic),
}

/// The synchronous side of the host: the driver, the output waiting to be sent, and supervision.
struct Runner<D>
where
    D: RegionDriver,
{
    driver: D,
    sink: ExitSink,
    outbox: VecDeque<(ExitId, EventsBuffer)>,
    turn_budget: Budget,
    pending: Option<Pending>,
    telemetry: HostTelemetry,
    last_publish: Instant,
    unpublished: bool,

    // Whether a call to `finish` completed, itself or through `resume`, without a panic ending it.
    finished: bool,

    // Whether the last call that returned wrote anything to its exit sink or charged its budget.
    last_call_worked: bool,
    deadlines: DeadlineGuard,
}

/// Keeps the host from spinning on a past deadline that the driver kept after time-driven work that did nothing.
///
/// Such a deadline would be due again at once, forever, and the host polls deadlines before input. The guard does not
/// arm it again until the driver's deadline changes.
///
/// Any other deadline is armed, including one that has also passed: a driver catching up on work that was due while
/// the host was busy moves its deadline forward one period per call, and one that completes work a panic ended can
/// next return an earlier deadline that is due too (see [`RegionDriver::next_deadline`]). The guard tells neither
/// apart from a driver that moves back and forth between past deadlines, which the driver contract forbids.
#[derive(Debug, Default)]
pub(super) struct DeadlineGuard {
    stalled: Option<std::time::Instant>,
}

impl DeadlineGuard {
    /// Returns the deadline to arm the timer for, given the driver's: none if the driver stalled on it.
    pub(super) fn arm(&mut self, deadline: Option<std::time::Instant>) -> Option<std::time::Instant> {
        if self.stalled.is_some() && deadline == self.stalled {
            return None;
        }
        self.stalled = None;
        deadline
    }

    /// Records time-driven work that ran because the deadline `fired` passed, and returns whether the driver stalled
    /// on it: the work did nothing (`idle`), and the driver's deadline is still `fired`.
    pub(super) fn ran(&mut self, fired: std::time::Instant, idle: bool, next: Option<std::time::Instant>) -> bool {
        let stalled = idle && next == Some(fired);
        if stalled {
            self.stalled = next;
        }
        stalled
    }
}

impl<D> Runner<D>
where
    D: RegionDriver,
{
    fn new(driver: D, exit_count: usize, turn_budget: Budget, telemetry: HostTelemetry) -> Self {
        Self {
            driver,
            sink: ExitSink::new(exit_count),
            outbox: VecDeque::new(),
            turn_budget,
            pending: None,
            telemetry,
            last_publish: Instant::now(),
            unpublished: false,
            finished: false,
            last_call_worked: false,
            deadlines: DeadlineGuard::default(),
        }
    }

    /// Runs `call` under supervision, with a fresh turn, and queues what it wrote for sending if it returns.
    ///
    /// On a panic, what the call wrote is discarded, and any yielded call is over. `batch_events` is the number of
    /// events of the batch the call is processing, which are what the panic loses. A call that is not about a batch
    /// loses the events it wrote, and the rest of a yielded batch, if a `process` call was yielded.
    fn call<R, F>(&mut self, site: PanicSite, batch_events: Option<u64>, call: F) -> Called<R>
    where
        F: FnOnce(&mut D, &mut RegionTurn<'_>, &mut ExitSink) -> R,
    {
        let mut budget = self.turn_budget;
        let driver = &mut self.driver;
        let sink = &mut self.sink;
        let result = {
            let mut turn = RegionTurn::new(Instant::now().into_std(), &mut budget);
            catch_unwind(AssertUnwindSafe(|| call(driver, &mut turn, sink)))
        };

        match result {
            Ok(result) => {
                self.last_call_worked = !self.sink.is_empty() || budget != self.turn_budget;
                self.outbox.extend(self.sink.drain());
                Called::Returned(result)
            }
            Err(payload) => {
                let yielded_batch_events = match self.pending.take() {
                    Some(Pending::Batch { events }) => events,
                    Some(Pending::Time | Pending::Finish) | None => 0,
                };
                let lost_events = batch_events.unwrap_or_else(|| self.sink.event_count() as u64 + yielded_batch_events);
                self.sink.clear();
                Called::Panicked(RegionPanic::new(site, panic_message(payload.as_ref()), lost_events))
            }
        }
    }

    /// Tells the driver the region has started.
    ///
    /// Starting runs no kernel code, so it is not supervised: a panic in it fails the component.
    fn start(&mut self) {
        let mut budget = self.turn_budget;
        let mut turn = RegionTurn::new(Instant::now().into_std(), &mut budget);
        self.driver.start(&mut turn);
    }

    /// Recovers from a panic, returning the error that fails the component if the driver decides so.
    fn recover(&mut self, panic: RegionPanic) -> Result<(), GenericError> {
        error!(
            site = %panic.site(),
            lost_events = panic.lost_events(),
            message = panic.message(),
            "Region panicked. Discarded the output of the failed call and resetting the region."
        );
        self.telemetry.events_lost.increment(panic.lost_events());
        self.telemetry.panics.increment(1);

        match self.driver.reset_after_panic(&panic) {
            PanicOutcome::Continue => {
                warn!(site = %panic.site(), "Region recovered from a panic and continues.");
                Ok(())
            }
            PanicOutcome::Fail(e) => {
                Err(e.context(format!("Region panicked in {}: {}", panic.site(), panic.message())))
            }
        }
    }

    /// Records the outcome of a call that does work, remembering it as `call` if it yielded.
    fn completed(&mut self, called: Called<Progress>, call: Pending) -> Result<(), GenericError> {
        self.unpublished = true;
        match called {
            Called::Returned(Progress::Yielded) => {
                self.pending = Some(call);
                Ok(())
            }
            Called::Returned(_) => {
                self.pending = None;
                self.finished |= matches!(call, Pending::Finish);
                Ok(())
            }
            Called::Panicked(panic) => self.recover(panic),
        }
    }

    fn process(&mut self, batch: EventsBuffer) -> Result<(), GenericError> {
        let events = batch.len() as u64;
        let called = self.call(PanicSite::Process, Some(events), |driver, turn, exits| {
            driver.process(turn, batch, exits)
        });
        self.completed(called, Pending::Batch { events })
    }

    fn resume(&mut self) -> Result<(), GenericError> {
        let Some(pending) = self.pending else {
            return Ok(());
        };
        let batch_events = match pending {
            Pending::Batch { events } => Some(events),
            Pending::Time | Pending::Finish => None,
        };
        let called = self.call(PanicSite::Resume, batch_events, |driver, turn, exits| {
            driver.resume(turn, exits)
        });
        self.completed(called, pending)
    }

    /// Returns the deadline to arm the timer for: the driver's, unless the driver stalled on it.
    fn wanted_deadline(&mut self) -> Option<std::time::Instant> {
        self.deadlines.arm(self.driver.next_deadline())
    }

    /// Runs time-driven work because the deadline `fired` passed.
    ///
    /// A driver whose `advance_time` did no work and kept that same deadline would have the host call it again at
    /// once, forever: the host does not arm the deadline again until the driver changes it, and a debug build fails.
    fn advance_time(&mut self, fired: std::time::Instant) -> Result<(), GenericError> {
        let called = self.call(PanicSite::AdvanceTime, None, |driver, turn, exits| {
            driver.advance_time(turn, exits)
        });
        let idle =
            matches!(called, Called::Returned(Progress::Done | Progress::WaitUntil(_))) && !self.last_call_worked;
        self.completed(called, Pending::Time)?;

        let stalled = self.deadlines.ran(fired, idle, self.driver.next_deadline());
        debug_assert!(
            !stalled,
            "the region driver kept a past deadline after time-driven work that did nothing, which would make the \
             host call it again at once, forever; move the work that was due to a later deadline, or return none"
        );
        if stalled {
            error!(
                "Region driver kept a past deadline after time-driven work that did nothing. Not running time-driven \
                 work again until the driver changes its deadline."
            );
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), GenericError> {
        let called = self.call(PanicSite::Finish, None, |driver, turn, exits| {
            driver.finish(turn, exits, DrainPolicy::Shutdown)
        });
        self.completed(called, Pending::Finish)
    }

    /// Applies a control message, acknowledges it, and publishes telemetry.
    fn control(&mut self, request: ControlRequest<D::Control>) -> Result<(), GenericError> {
        let ControlRequest { message, reply } = request;
        let called = self.call(PanicSite::Control, None, |driver, turn, exits| {
            driver.control(turn, message, exits)
        });

        // The sender may have stopped waiting for the acknowledgment, which is fine.
        match called {
            Called::Returned(Ok(())) => {
                let _ = reply.send(Ok(()));
            }
            Called::Returned(Err(failure)) => {
                warn!(error = %failure, "Region could not apply a control message.");
                let _ = reply.send(Err(ControlError::Rejected { failure }));
            }
            Called::Panicked(panic) => {
                let message = panic.message().to_owned();
                let recovered = self.recover(panic);
                let _ = reply.send(Err(ControlError::Panicked { message }));
                recovered?;
            }
        }

        self.publish()
    }

    /// Returns when the driver's telemetry is next due to be published, if it did work since it last published.
    fn publish_deadline(&self) -> Option<Instant> {
        self.unpublished.then(|| self.last_publish + PUBLISH_INTERVAL)
    }

    fn publish(&mut self) -> Result<(), GenericError> {
        self.last_publish = Instant::now();
        self.unpublished = false;
        match self.call(PanicSite::PublishTelemetry, None, |driver, _, _| {
            driver.publish_telemetry()
        }) {
            Called::Returned(()) => Ok(()),
            Called::Panicked(panic) => self.recover(panic),
        }
    }
}

/// Returns the message a panic carried.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<panic payload that is not a string>".to_owned())
}
