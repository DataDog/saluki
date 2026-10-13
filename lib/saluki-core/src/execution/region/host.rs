//! The region host: the topology transform that drives a [`RegionDriver`], on one lane or on several.

use std::{
    any::Any,
    collections::{HashSet, VecDeque},
    future::Future,
    num::{NonZeroU16, NonZeroUsize},
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    future::{join_all, BoxFuture},
    stream::FuturesUnordered,
    FutureExt as _, StreamExt as _,
};
use saluki_error::{generic_error, GenericError};
use saluki_metrics::{Counter, MetricsBuilder};
use tokio::{
    pin, select,
    sync::{oneshot, watch},
    task::coop::consume_budget,
    time::{sleep_until, Instant},
};
use tracing::{debug, error, warn};

use super::{
    driver::{ExitId, ExitSink, PanicOutcome, PanicSite, RegionDriver, RegionPanic, RegionTurn},
    lanes::{lane_link, LaneContext, LaneEnd, LaneLink, LaneProbes, LaneSplit, LaneStats},
};
use crate::{
    accounting::{MemoryBounds, MemoryBoundsBuilder, UsageExpr},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext, ComponentContext,
    },
    data_model::event::{Event, EventType},
    execution::{
        control::{ControlAck, ControlError, ControlMailbox, ControlReceiver, ControlReply},
        AdmissionPointConfig, Budget, DrainPolicy, OwnedAdmission, Progress,
    },
    health::{Health, Probe},
    observability::ComponentMetricsExt as _,
    runtime,
    support::SubsystemIdentifier,
    topology::{
        ComponentId, EventsBuffer, EventsConsumer, EventsDispatcher, LaneEdge, LaneRoute, OutputDefinition,
        DEFAULT_EVENTS_BUFFER_CAPACITY,
    },
};

/// The shortest interval between two periodic calls to [`RegionDriver::publish_telemetry`].
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

/// The name of every lane task, a category label for task telemetry; each lane's resource group tells lanes apart.
const LANE_TASK_NAME: &str = "region_lane";

type DriverFactory<D> = Box<dyn Fn(&BuildContext, LaneContext) -> Result<D, GenericError> + Send + Sync>;

/// How a coordinator splits a control message per lane, and finds the lane of one that does not split.
struct LaneSplitter<C> {
    split: fn(C, u16) -> Result<Vec<C>, C>,
    lane: fn(&C) -> Option<u16>,
}

impl<C> Clone for LaneSplitter<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for LaneSplitter<C> {}

impl<C: LaneSplit> LaneSplitter<C> {
    fn new() -> Self {
        Self {
            split: |message, lanes| message.split(lanes),
            lane: |message| message.lane(),
        }
    }
}

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
/// the driver's deadline, the end of a hold of output ([`RegionDriver::end_turn`]), a heartbeat or a clock reset of
/// the region's admission point (see below), and input batches. It reads input only while no call is yielded and the
/// driver [accepts input][RegionDriver::accepts_input], and processes up to `work_per_turn` batches back to back
/// before it ends the turn and checks for control messages and timers again.
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
/// written, awaiting each send (the buffers of a keyed exit are tried first, see [Routing](#routing)) before the next
/// call: when a downstream component is slow, the region waits for it, as native components do, and the input waits
/// upstream. While a send waits, the host still answers liveness probes and handles control messages, whose output it
/// queues behind the buffers already waiting.
///
/// After each turn of input, once no call is yielded, the host calls [`RegionDriver::end_turn`], which writes the
/// output the driver held across batches and whose hold expired. If the input has nothing more waiting, the host arms
/// a timer for the instant `end_turn` returns, and calls it again then; while input keeps the host busy, it arms no
/// timer, since the next turn ends soon anyway.
///
/// When the input ends, the host completes any yielded call, calls [`RegionDriver::finish`] with
/// [`DrainPolicy::Shutdown`], resumes it until it is done, sends its output, publishes telemetry a last time, and
/// stops. If a panic ends the finish before it is done and the driver continues, the host calls `finish` again, until
/// a call completes it, so that the driver completes the work the panic ended.
///
/// # Admission
///
/// A region given an [owned][crate::execution::Stamper::Owned] admission point
/// ([`with_lane_admission`][Self::with_lane_admission]) stamps every unstamped batch it receives at that point before
/// the driver processes it, and sends what the driver writes in order itself, so that every downstream channel
/// receives the point's batches in stamp order. It drives the point's heartbeats: when the point has stamped nothing
/// for its heartbeat period, the host stamps a header-only batch and processes it as input, which the driver forwards
/// as progress. A point that shares its source's [clock][crate::execution::SourceClock] with other lanes' points also
/// learns of a reset another lane detected: after every batch the host compares the clock's epoch with that of the
/// point's latest stamp, and while its input is idle it waits for the clock's next reset, and either way stamps a
/// header-only batch in the new epoch at once.
///
/// # Lanes
///
/// A region with lanes ([`with_lanes`][Self::with_lanes]) runs one task per lane, each a host as described here, with
/// its own driver, input channel, admission point if it has one, and resource group (`<component>.lane_<i>`), while
/// the component's own task coordinates them: it splits each control message into one per lane
/// ([`LaneSplit`][crate::execution::LaneSplit]) and acknowledges it once every lane accepted its part, or with the
/// first lane's rejection; it routes a message that does not split to the lane it names, and rejects one that names
/// none; and it forwards every liveness probe to every lane, answering it once all answered, so that a stuck lane reads
/// as a stuck component. A lane that fails, or stops before its input ends, fails the component; the other lanes
/// then stop without finishing. A lane that finishes, once its input ended, counts as answering every later probe,
/// and takes no part in later control messages: a message whose every lane finished is left unhandled, as by a region
/// that stopped. The lanes share the component's dispatcher, and each lane sends its buffers in order.
///
/// # Routing
///
/// Where a buffer goes in an exit's target with lanes is the routing of the edge by which the exit's output leads to
/// the target ([`LaneEdge`]), which every lane reads from the dispatcher when it starts: nothing else configures it.
/// On a keyed edge, the driver writes each buffer to the lane it chose ([`ExitSink::emit_to_lane`]); on an affinity
/// edge, every buffer of a lane goes to the target's lane with the sending lane's index (modulo the target's lanes);
/// on a spread edge, to the target's first lane with room, from a rotation of the sending lane's own that starts at
/// its index. A driver writes buffers for affinity and spread edges, and for targets with one lane, without naming a
/// lane ([`ExitSink::emit`]); a lane named for a target with one lane, or on an edge fixed to one lane, is ignored. A
/// buffer that does not fit its exit's edge, written without a lane to a keyed edge, to a lane the target does not
/// have, or to a lane on an affinity or spread edge, is a mismatch between the driver and its topology: it fails the
/// component rather than being dropped.
///
/// The buffers that one call writes to the lanes of a keyed edge's target are sent back to back without waiting, as
/// far as each lane has room, and the ones for full lanes are then awaited in order, before anything the driver wrote
/// after them: one full lane does not hold up what the call routes to the others, and every lane receives its buffers
/// in order.
///
/// # Supervision
///
/// Every call that can run kernel code runs under [`catch_unwind`]. When a call panics, the host discards everything
/// the call wrote, so nothing of it is delivered, and counts the lost events (see [`RegionPanic::lost_events`]) in
/// `component_events_dropped_total`, tagged `intentional:false`, `drop_reason:region_failure`, and
/// `loss_class:invalid`, and the panic in `component_errors_total`, tagged `error_type:panic`. It then calls
/// [`RegionDriver::reset_after_panic`]: on [`PanicOutcome::Continue`] the region keeps running, and on
/// [`PanicOutcome::Fail`] the transform returns the error, which fails the component. In a region with lanes, each
/// lane recovers from its own panics, and the failure of a lane fails the component.
///
/// A caught panic still runs the process's panic hook, before it unwinds: the default hook prints the panic message,
/// and a hook that a process installs to report panics, such as one that reports every panic to a test harness as a
/// failure, sees caught region panics too.
///
/// # Memory bounds
///
/// Besides the component itself and the bounds given with [`with_bounds`][Self::with_bounds], the host accounts for
/// one full events buffer per exit and lane, held while it waits for downstream. A driver that can write more than one
/// full buffer's worth of events to one exit per call accounts for the rest in its own bounds, and the bounds given
/// with `with_bounds` cover every lane's driver.
pub struct RegionHostBuilder<D>
where
    D: RegionDriver,
{
    outputs: Vec<OutputDefinition<EventType>>,
    exit_names: Arc<[String]>,
    work_per_turn: NonZeroUsize,
    turn_budget: Budget,
    factory: DriverFactory<D>,
    lanes: NonZeroU16,
    lane_admission: Vec<AdmissionPointConfig>,
    lane_stats: Vec<Arc<LaneStats>>,
    input_buffer_events: usize,
    splitter: Option<LaneSplitter<D::Control>>,
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
    /// mailbox][crate::execution::control_mailbox]. `factory` builds the driver when the topology builds the
    /// transform, with the build context of the region's component.
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
            factory: Box::new(move |context, _| factory(context)),
            lanes: NonZeroU16::MIN,
            lane_admission: Vec::new(),
            lane_stats: Vec::new(),
            input_buffer_events: DEFAULT_EVENTS_BUFFER_CAPACITY,
            splitter: None,
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
    ///
    /// In a region with lanes, these bounds cover every lane's driver.
    pub fn with_bounds<B>(mut self, bounds: B) -> Self
    where
        B: MemoryBounds + Send + Sync + 'static,
    {
        self.bounds = Some(Box::new(bounds));
        self
    }

    /// Runs the region on `lanes` lanes, building one driver per lane with `factory`, which is told the lane it builds
    /// for, in place of the factory the builder was created with.
    ///
    /// A region with lanes is one component with one input channel per lane, a coordinator that splits its control
    /// messages per lane ([`LaneSplit`]) and forwards its liveness probes, and one task per lane, accounted to a
    /// resource group of its own (see the [module documentation][crate::execution::lanes]). With one lane, the host
    /// runs as it does without lanes.
    pub fn with_lanes<F>(mut self, lanes: NonZeroU16, factory: F) -> Self
    where
        F: Fn(&BuildContext, LaneContext) -> Result<D, GenericError> + Send + Sync + 'static,
        D::Control: LaneSplit,
    {
        self.lanes = lanes;
        self.factory = Box::new(factory);
        self.splitter = Some(LaneSplitter::new());
        self
    }

    /// Gives each lane an [owned][crate::execution::Stamper::Owned] admission point that it stamps every unstamped
    /// batch it receives at, before processing it: lane `i` admits at `points[i]`.
    ///
    /// A region without lanes has one lane, and takes one point. Building a transform fails if the number of points
    /// differs from the number of lanes, or if a point cannot be owned (see [`OwnedAdmission::new`]).
    pub fn with_lane_admission(mut self, points: Vec<AdmissionPointConfig>) -> Self {
        self.lane_admission = points;
        self
    }

    /// Gives each lane statistics that its host updates as the lane works, for the lane's telemetry: lane `i` updates
    /// `stats[i]` (see [`LaneStats`]).
    ///
    /// A region without lanes has one lane, and takes one. Building a transform fails if the number of statistics
    /// differs from the number of lanes. Without them, the host measures nothing of its own.
    pub fn with_lane_stats(mut self, stats: Vec<Arc<LaneStats>>) -> Self {
        self.lane_stats = stats;
        self
    }

    /// Declares that every buffer sent to the region holds storage for at most `events` events, as the senders of a
    /// region fed by buffers routed per key keep to (see [`TransformBuilder::input_buffer_events`]); the capacity of
    /// an events buffer, 1,024 events, by default.
    pub fn with_input_buffer_events(mut self, events: usize) -> Self {
        self.input_buffer_events = events;
        self
    }

    /// Returns the number of lanes the region runs on.
    pub fn lanes(&self) -> NonZeroU16 {
        self.lanes
    }
}

impl<D> MemoryBounds for RegionHostBuilder<D>
where
    D: RegionDriver,
{
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        let lanes = usize::from(self.lanes.get());
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built, which holds the driver.
            .with_single_value::<RegionHost<D>>("component struct");
        if lanes > 1 {
            builder.minimum().with_array::<Lane<D>>("lanes", lanes);
        }
        builder.firm().with_expr(UsageExpr::product(
            "in-flight exit buffers of every lane",
            UsageExpr::constant("lanes", lanes),
            in_flight_exit_buffers(self.exit_names.len()),
        ));

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
            UsageExpr::struct_size::<(ExitId, Option<u16>, EventsBuffer)>("queued exit buffer"),
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

    fn input_lanes(&self) -> NonZeroU16 {
        self.lanes
    }

    fn input_buffer_events(&self) -> usize {
        self.input_buffer_events
    }

    async fn build(&self, context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        Ok(match self.build_host(&context)? {
            BuiltHost::One(host) => host,
            BuiltHost::Laned(host) => Box::new(host),
        })
    }
}

/// The transform a region host builder builds: a region with one lane, or one with lanes.
pub(super) enum BuiltHost<D>
where
    D: RegionDriver,
{
    One(Box<RegionHost<D>>),
    Laned(LanedRegionHost<D>),
}

impl<D> RegionHostBuilder<D>
where
    D: RegionDriver,
{
    /// Builds the region's transform: its drivers, one per lane, the lanes' admission points, and its mailbox.
    pub(super) fn build_host(&self, context: &BuildContext) -> Result<BuiltHost<D>, GenericError> {
        let count = self.lanes.get();
        if !self.lane_admission.is_empty() && self.lane_admission.len() != usize::from(count) {
            return Err(generic_error!(
                "Region '{}' runs on {} lanes, but was given {} lane admission points. Give every lane one point.",
                context.component_id(),
                self.lanes,
                self.lane_admission.len()
            ));
        }
        if !self.lane_stats.is_empty() && self.lane_stats.len() != usize::from(count) {
            return Err(generic_error!(
                "Region '{}' runs on {} lanes, but was given {} lane statistics. Give every lane one.",
                context.component_id(),
                self.lanes,
                self.lane_stats.len()
            ));
        }

        let mut drivers = Vec::with_capacity(usize::from(count));
        for index in 0..count {
            let lane = LaneContext { index, count };
            let driver = (self.factory)(context, lane).map_err(|e| {
                if lane.is_only() {
                    e.context(format!(
                        "Failed to build the driver of region '{}'",
                        context.component_id()
                    ))
                } else {
                    e.context(format!(
                        "Failed to build the driver of {} of region '{}'",
                        lane,
                        context.component_id()
                    ))
                }
            })?;
            drivers.push((lane, driver));
        }

        let mut admissions = Vec::with_capacity(self.lane_admission.len());
        for (index, config) in self.lane_admission.iter().enumerate() {
            let admission = OwnedAdmission::new(config.clone()).map_err(|e| {
                e.context(format!(
                    "Failed to set up the admission point of {} of region '{}'",
                    LaneContext {
                        index: index as u16,
                        count
                    },
                    context.component_id()
                ))
            })?;
            admissions.push(admission);
        }

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
        let control = mailbox.into_receiver_for("region");

        let mut admissions = admissions.into_iter();
        let mut stats = self.lane_stats.iter().cloned();
        let mut lanes = drivers
            .into_iter()
            .map(|(lane, driver)| {
                let stats = stats.next();
                Lane {
                    context: lane,
                    runner: Runner::new(
                        driver,
                        self.exit_names.len(),
                        self.turn_budget,
                        HostTelemetry::new(context.component_context()),
                        stats.clone(),
                    ),
                    admission: admissions.next(),
                    routes: ExitRoutes::new(Arc::clone(&self.exit_names), lane),
                    stats,
                    work_per_turn: self.work_per_turn,
                }
            })
            .collect::<Vec<_>>();

        if count == 1 {
            let lane = lanes.pop().expect("one lane was built");
            return Ok(BuiltHost::One(Box::new(RegionHost { lane, control })));
        }
        let splitter = self.splitter.ok_or_else(|| {
            generic_error!(
                "Region '{}' runs on {} lanes but has no way to split its control messages. Set its lanes with \
                 `RegionHostBuilder::with_lanes`.",
                context.component_id(),
                count
            )
        })?;
        Ok(BuiltHost::Laned(LanedRegionHost {
            lanes,
            control,
            splitter,
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

/// The region host transform of a region with one lane, which runs the lane on the component's own task.
pub(super) struct RegionHost<D>
where
    D: RegionDriver,
{
    lane: Lane<D>,
    control: ControlReceiver<D::Control>,
}

#[async_trait]
impl<D> Transform for RegionHost<D>
where
    D: RegionDriver,
{
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let RegionHost { lane, control } = *self;
        let mut health = context.take_health_handle();
        let input = only_input(&mut context)?;
        health.mark_ready();
        lane.run(
            input,
            context.dispatcher(),
            &mut Liveness::Component(health),
            &mut ControlSource::component(control),
        )
        .await
    }
}

/// Takes the one input of a region with one lane.
fn only_input(context: &mut TransformContext) -> Result<EventsConsumer, GenericError> {
    let mut inputs = context.take_lane_inputs();
    if inputs.len() != 1 {
        return Err(generic_error!(
            "Region '{}' runs on one lane, but its topology gave it {} input channels. Build the topology from the \
             region's builder, which declares its lanes.",
            context.component_context().component_id(),
            inputs.len()
        ));
    }
    Ok(inputs.pop().expect("one input"))
}

/// The region host transform of a region with lanes: the coordinator, on the component's own task, and a task per
/// lane.
pub(super) struct LanedRegionHost<D>
where
    D: RegionDriver,
{
    lanes: Vec<Lane<D>>,
    control: ControlReceiver<D::Control>,
    splitter: LaneSplitter<D::Control>,
}

/// What the lanes of a region with lanes share from their component.
pub(super) struct LaneParts {
    /// One input per lane, in lane order.
    pub(super) inputs: Vec<EventsConsumer>,

    /// The component's dispatcher, which every lane sends through.
    pub(super) dispatcher: Arc<EventsDispatcher>,

    /// The component's health registration, which the coordinator answers for every lane.
    pub(super) health: Health,

    /// The component's resource group, below which each lane's own is.
    pub(super) group: SubsystemIdentifier,
}

#[async_trait]
impl<D> Transform for LanedRegionHost<D>
where
    D: RegionDriver,
{
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let parts = LaneParts {
            inputs: context.take_lane_inputs(),
            dispatcher: context.shared_dispatcher(),
            health: context.take_health_handle(),
            group: context.component_context().identity(),
        };
        let region = context.component_context().component_id().to_string();
        (*self).coordinate(parts, region).await
    }
}

/// How one lane of a region with lanes ended, as its task reports it to the coordinator.
type LaneOutcome = Result<(), GenericError>;

impl<D> LanedRegionHost<D>
where
    D: RegionDriver,
{
    /// Spawns a task per lane and coordinates them until every lane finished, or one failed.
    pub(super) async fn coordinate(self, parts: LaneParts, region: String) -> Result<(), GenericError> {
        let LanedRegionHost {
            lanes,
            control,
            splitter,
        } = self;
        let LaneParts {
            inputs,
            dispatcher,
            mut health,
            group,
        } = parts;
        let count = lanes.len();
        if inputs.len() != count {
            return Err(generic_error!(
                "Region '{}' runs on {} lanes, but its topology gave it {} input channels. Build the topology from the \
                 region's builder, which declares its lanes.",
                region,
                count,
                inputs.len()
            ));
        }

        let mut links = Vec::with_capacity(count);
        let mut exits = FuturesUnordered::new();
        for (lane, input) in lanes.into_iter().zip(inputs) {
            let context = lane.context;
            let (link, end) = lane_link(context);
            let (outcome, exited) = oneshot::channel::<LaneOutcome>();
            let dispatcher = Arc::clone(&dispatcher);
            let task = async move {
                let _ = outcome.send(lane.run_as_lane(end, input, &dispatcher).await);
            };
            runtime::worker(LANE_TASK_NAME, task)
                .with_resource_group(lane_group(&group, context).to_string())
                .spawn_child_or_detached();
            exits.push(async move { (context, exited.await) });
            links.push(link);
        }
        // The coordinator keeps no sender of its own: once every lane is gone, so is the component's output.
        drop(dispatcher);
        // Shared with the acknowledgments of control messages, which tell a lane that finished from one that failed.
        let links = Arc::<[LaneLink<D::Control>]>::from(links);

        // Controllers time their messages from when the region runs, so wait until every lane reads its control queue.
        // A lane that already finished read it before it did.
        let mut finished = 0;
        let running = join_all(links.iter().map(LaneLink::running));
        pin!(running);
        let started = loop {
            select! {
                // A lane that ends reports how first, which says more than that it never started.
                biased;

                Some((lane, outcome)) = exits.next() => match outcome {
                    Ok(Ok(())) => finished += 1,
                    Ok(Err(e)) => return Err(lane_failed(&region, lane, Some(e))),
                    Err(_) => return Err(lane_failed(&region, lane, None)),
                },

                started = &mut running => break started.into_iter().all(|running| running),
            }
        };
        if !started {
            return Err(generic_error!(
                "A lane of region '{}' stopped before it started. See its logs for the reason.",
                region
            ));
        }

        health.mark_ready();
        control.mark_running();
        debug!(lanes = count, "Region coordinator started.");

        let mut control = Some(control);
        let mut acknowledgments = FuturesUnordered::<BoxFuture<'static, ()>>::new();
        let mut forwarding: Option<Pin<Box<dyn Future<Output = ()> + Send + '_>>> = None;
        while finished < count {
            select! {
                biased;

                probe = health.next_probe(), if forwarding.is_none() => {
                    forwarding = Some(Box::pin(forward_probe(probe, &links)));
                },

                _ = poll_forwarding(&mut forwarding) => forwarding = None,

                Some((lane, outcome)) = exits.next() => match outcome {
                    Ok(Ok(())) => finished += 1,
                    Ok(Err(e)) => return Err(lane_failed(&region, lane, Some(e))),
                    Err(_) => return Err(lane_failed(&region, lane, None)),
                },

                request = next_control(&mut control) => match request {
                    Some((message, reply)) => acknowledgments.push(dispatch_control(message, reply, &links, splitter)),
                    None => control = None,
                },

                Some(()) = acknowledgments.next(), if !acknowledgments.is_empty() => {},
            }
        }

        debug!("Region coordinator stopped: every lane finished.");
        Ok(())
    }
}

/// Returns the resource group of one lane: the component's, with the lane's index.
fn lane_group(component: &SubsystemIdentifier, lane: LaneContext) -> SubsystemIdentifier {
    component.clone().child(format!("lane_{}", lane.index))
}

/// Returns the error with which a lane that failed, with `error`, or stopped without reporting how, fails its
/// component.
fn lane_failed(region: &str, lane: LaneContext, error: Option<GenericError>) -> GenericError {
    match error {
        Some(e) => e.context(format!("{} of region '{}' failed", capitalized(lane), region)),
        None => generic_error!(
            "{} of region '{}' stopped without finishing: its task panicked outside any supervised call, or was \
             aborted. See its logs for the reason.",
            capitalized(lane),
            region
        ),
    }
}

/// Spells a lane for the start of a sentence, as "Lane 1 of 4" begins one.
fn capitalized(lane: LaneContext) -> String {
    format!("Lane {} of {}", lane.index, lane.count)
}

/// Waits for the probe being forwarded to the lanes, or forever if none is.
async fn poll_forwarding(forwarding: &mut Option<Pin<Box<dyn Future<Output = ()> + Send + '_>>>) {
    match forwarding {
        Some(forwarding) => forwarding.as_mut().await,
        None => std::future::pending().await,
    }
}

/// Forwards one liveness probe to every lane, and responds to it once every lane answered.
///
/// A lane that finished, once its input ended, counts as answering: it is done, not stuck. A lane that stopped
/// otherwise cannot answer, so the probe is then dropped unanswered, and the component reads as not live until the
/// coordinator, which sees the lane's end, fails it.
pub(super) async fn forward_probe<C>(probe: Probe, links: &[LaneLink<C>]) {
    let answers = links
        .iter()
        .filter(|link| !link.has_finished())
        .map(|link| async move { link.probe().await || link.has_finished() });
    if join_all(answers).await.into_iter().all(|answered| answered) {
        probe.respond().await;
    }
}

/// Sends a control message to the lanes it is for, and returns the future that acknowledges it once: when every lane
/// it went to accepted its part, or with the first rejection.
///
/// A lane that finished takes no part: it has nothing left to apply a message to. A message for no lane that still
/// runs is left unhandled, which its controller sees as it sees a region that stopped.
fn dispatch_control<C>(
    message: C, reply: ControlReply, links: &Arc<[LaneLink<C>]>, splitter: LaneSplitter<C>,
) -> BoxFuture<'static, ()>
where
    C: Send + 'static,
{
    let lanes = links.len() as u16;
    let acks = match (splitter.split)(message, lanes) {
        Ok(parts) if parts.len() == links.len() => links
            .iter()
            .enumerate()
            .zip(parts)
            .filter(|((_, link), _)| !link.has_finished())
            .map(|((index, link), part)| (index, link.send(part)))
            .collect::<Vec<_>>(),
        Ok(parts) => {
            reply.reject(generic_error!(
                "A control message split into {} parts for a region with {} lanes. Split messages into one part per \
                 lane.",
                parts.len(),
                lanes
            ));
            return std::future::ready(()).boxed();
        }
        Err(message) => match (splitter.lane)(&message) {
            Some(lane) if lane < lanes => {
                let index = usize::from(lane);
                let link = &links[index];
                if link.has_finished() {
                    Vec::new()
                } else {
                    vec![(index, link.send(message))]
                }
            }
            Some(lane) => {
                reply.reject(generic_error!(
                    "A control message is for lane {}, but the region has only {} lanes. Address one of its lanes.",
                    lane,
                    lanes
                ));
                return std::future::ready(()).boxed();
            }
            None => {
                reply.reject(generic_error!(
                    "A control message neither splits into one part per lane of the region's {} lanes nor names the \
                     lane it is for. Send one part per lane, or one message per lane that names it.",
                    lanes
                ));
                return std::future::ready(()).boxed();
            }
        },
    };
    if acks.is_empty() {
        // Dropping the reply acknowledges the message as unhandled.
        drop(reply);
        return std::future::ready(()).boxed();
    }
    join_acknowledgments(acks, Arc::clone(links), reply).boxed()
}

/// Acknowledges a control message once every lane it went to, by index into `links`, accepted its part, or with the
/// first rejection.
///
/// A lane that finished before it handled its part had nothing left to apply it to, and is passed over; if every lane
/// it went to did, the message is left unhandled, as by a region that stopped. A lane that stopped otherwise, without
/// finishing, rejects the message: it failed, which fails the component.
pub(super) async fn join_acknowledgments<C>(
    acks: Vec<(usize, ControlAck)>, links: Arc<[LaneLink<C>]>, reply: ControlReply,
) {
    let mut pending = acks
        .into_iter()
        .map(|(index, ack)| ack.map(move |outcome| (index, outcome)))
        .collect::<FuturesUnordered<_>>();
    let mut accepted = false;
    while let Some((index, outcome)) = pending.next().await {
        let link = &links[index];
        match outcome {
            Ok(()) => accepted = true,
            Err(ControlError::Rejected { error, .. }) => {
                reply.reject(error.context(format!("{} rejected it", capitalized(link.lane()))));
                return;
            }
            Err(ControlError::Panicked { message, .. }) => {
                reply.panicked(format!("{} panicked: {}", capitalized(link.lane()), message));
                return;
            }
            // A lane reports that it finished before its control queue closes, so this sees the report.
            Err(ControlError::Stopped { .. }) if link.has_finished() => {}
            Err(ControlError::Stopped { .. }) => {
                reply.reject(generic_error!(
                    "{} stopped before it handled the control message.",
                    capitalized(link.lane())
                ));
                return;
            }
        }
    }
    if accepted {
        reply.ack();
    } else {
        // Every lane it went to finished first: dropping the reply acknowledges it as unhandled.
        drop(reply);
    }
}

/// Waits for the next control request, or forever once the mailbox is closed.
async fn next_control<C>(control: &mut Option<ControlReceiver<C>>) -> Option<(C, ControlReply)> {
    match control {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Where the liveness probes a lane answers come from.
enum Liveness {
    /// The component's own health registration: a region with one lane answers it itself.
    Component(Health),

    /// The probes the coordinator of a region with lanes forwards to the lane.
    Lane(LaneProbes),
}

impl Liveness {
    /// Waits for the next liveness probe, and answers it.
    async fn answer(&mut self) {
        match self {
            Self::Component(health) => health.live().await,
            Self::Lane(probes) => probes.next().await.respond(),
        }
    }
}

/// Where a lane reads its control messages: the component's mailbox, or, in a region with lanes, the lane's own
/// control queue, which the coordinator feeds.
struct ControlSource<C> {
    receiver: Option<ControlReceiver<C>>,

    /// Whether this is a lane's queue, which closes only when its coordinator is gone: the component has failed, and
    /// the lane stops.
    lane: bool,

    /// Whether the coordinator is gone.
    abandoned: bool,
}

impl<C> ControlSource<C> {
    fn component(receiver: ControlReceiver<C>) -> Self {
        Self {
            receiver: Some(receiver),
            lane: false,
            abandoned: false,
        }
    }

    fn lane(receiver: ControlReceiver<C>) -> Self {
        Self {
            receiver: Some(receiver),
            lane: true,
            abandoned: false,
        }
    }

    fn mark_running(&self) {
        if let Some(receiver) = &self.receiver {
            receiver.mark_running();
        }
    }

    /// Waits for the next control request, or forever once the source is closed.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel-safe.
    async fn next(&mut self) -> Option<(C, ControlReply)> {
        let request = next_control(&mut self.receiver).await;
        if request.is_none() {
            self.receiver = None;
            self.abandoned = self.lane;
        }
        request
    }
}

/// How one lane sends the buffers its driver writes to one exit: by the routing of the edge by which the exit's output
/// sends to the lanes of a component, if it sends to any.
#[derive(Debug)]
enum ExitRoute {
    /// The output sends to no component's lanes: every buffer goes where the output's edges lead, each into a
    /// component with one lane or into the lane its edge is fixed to, and a lane the driver names is ignored.
    Whole,

    /// By key: to the lane the driver names, one of the target's `lanes`.
    Keyed { lanes: u16 },

    /// By affinity: to the target's lane with the sending lane's index, which the dispatcher takes modulo its lanes.
    Affinity,

    /// By spreading: to the target's first lane with room, trying lanes in rotation from this counter of the sending
    /// lane's own, which the dispatcher leaves at the lane after the one it sent to.
    Spread { counter: u16 },
}

impl ExitRoute {
    /// Returns the name of the routing, as "by spread routing" uses it in a sentence.
    fn kind(&self) -> &'static str {
        match self {
            Self::Whole => "no",
            Self::Keyed { .. } => "keyed",
            Self::Affinity => "affinity",
            Self::Spread { .. } => "spread",
        }
    }
}

/// The exits of one lane, and where each buffer the lane's driver writes goes.
pub(super) struct ExitRoutes {
    names: Arc<[String]>,
    lane: LaneContext,
    routes: Vec<ExitRoute>,
}

impl ExitRoutes {
    /// Creates the routes of `lane`'s exits, each sending whole until [`resolve`][Self::resolve] reads its edge.
    pub(super) fn new(names: Arc<[String]>, lane: LaneContext) -> Self {
        let routes = names.iter().map(|_| ExitRoute::Whole).collect();
        Self { names, lane, routes }
    }

    /// Reads the routing of every exit from the dispatcher the lane sends through: the edge, if any, by which the
    /// exit's output sends to the lanes of a component.
    pub(super) fn resolve(&mut self, dispatcher: &EventsDispatcher) {
        for (name, route) in self.names.iter().zip(&mut self.routes) {
            *route = match dispatcher.lane_edge(name) {
                None | Some((LaneEdge::Fixed(_), _)) => ExitRoute::Whole,
                Some((LaneEdge::Keyed, lanes)) => ExitRoute::Keyed { lanes: lanes.get() },
                Some((LaneEdge::Affinity, _)) => ExitRoute::Affinity,
                // A rotation starts at the lane's own index, so that the lanes of one region start on different lanes
                // of the target.
                Some((LaneEdge::Spread, _)) => ExitRoute::Spread {
                    counter: self.lane.index,
                },
            };
        }
    }

    /// Returns the name of `exit`.
    pub(super) fn name(&self, exit: ExitId) -> &str {
        &self.names[exit.get()]
    }

    /// Returns `true` if `exit` sends to the lanes of its target by key.
    pub(super) fn is_keyed(&self, exit: ExitId) -> bool {
        matches!(self.routes[exit.get()], ExitRoute::Keyed { .. })
    }

    /// Returns the name of `exit` and the route of a buffer the driver wrote to it, to `lane` of its target if it named
    /// one: `None` sends the buffer as on any output.
    ///
    /// # Errors
    ///
    /// If the buffer does not fit the exit's routing, an error is returned: written without a lane to a keyed exit, to
    /// a lane its target does not have, or to a lane of an exit that routes by affinity or by spreading, which picks
    /// the lane itself. Each is a mismatch between the region's driver and the topology it runs in.
    pub(super) fn route(
        &mut self, exit: ExitId, lane: Option<u16>,
    ) -> Result<(&str, Option<LaneRoute<'_>>), GenericError> {
        let name = self.names[exit.get()].as_str();
        let route = &mut self.routes[exit.get()];
        let kind = route.kind();
        let routed = match (route, lane) {
            (ExitRoute::Whole, _) => None,
            (ExitRoute::Keyed { lanes }, Some(lane)) if lane < *lanes => Some(LaneRoute::Target(lane)),
            (ExitRoute::Keyed { lanes }, Some(lane)) => {
                return Err(generic_error!(
                    "The region wrote a buffer to lane {} of the target of exit '{}', which has {} lanes. Write each \
                     buffer to a lane its target has.",
                    lane,
                    name,
                    lanes
                ))
            }
            (ExitRoute::Keyed { .. }, None) => {
                return Err(generic_error!(
                    "Exit '{}' routes to the lanes of its target by key, but the region wrote a buffer to it without \
                     naming a lane. Write each buffer of the exit to the lane that owns its keys, or connect the exit \
                     to its target with the routing its region's driver uses.",
                    name
                ))
            }
            (ExitRoute::Affinity, None) => Some(LaneRoute::From(self.lane.index)),
            (ExitRoute::Spread { counter }, None) => Some(LaneRoute::Spread(counter)),
            (ExitRoute::Affinity | ExitRoute::Spread { .. }, Some(lane)) => {
                return Err(generic_error!(
                    "The region wrote a buffer to lane {} of the target of exit '{}', but the exit routes to the lanes \
                     of its target by {} routing, which picks the lane itself. Write the exit's buffers without a \
                     lane, or connect the exit to its target by key.",
                    lane,
                    name,
                    kind
                ))
            }
        };
        Ok((name, routed))
    }
}

/// One lane of a region: its driver, and everything its host keeps for it.
pub(super) struct Lane<D>
where
    D: RegionDriver,
{
    context: LaneContext,
    runner: Runner<D>,
    admission: Option<OwnedAdmission>,
    routes: ExitRoutes,
    stats: Option<Arc<LaneStats>>,
    work_per_turn: NonZeroUsize,
}

/// What a lane waits on for its admission point's source clock to reset: the clock's epoch channel, which keeps the
/// change for a lane that was busy when it happened. It resolves to the channel again, to wait for the next reset, or
/// to `None` once the clock is gone.
type ResetWait = Pin<Box<dyn Future<Output = Option<watch::Receiver<u32>>> + Send>>;

fn wait_for_reset(mut resets: watch::Receiver<u32>) -> ResetWait {
    Box::pin(async move { resets.changed().await.ok().map(|()| resets) })
}

/// Waits for the next reset, or forever if the lane does not wait for any.
async fn next_reset(wait: &mut Option<ResetWait>) -> Option<watch::Receiver<u32>> {
    match wait {
        Some(wait) => wait.as_mut().await,
        None => std::future::pending().await,
    }
}

impl<D> Lane<D>
where
    D: RegionDriver,
{
    /// Runs the lane as one task of a region with lanes, until its input ends, or its coordinator is gone.
    async fn run_as_lane(
        self, end: LaneEnd<D::Control>, input: EventsConsumer, dispatcher: &EventsDispatcher,
    ) -> LaneOutcome {
        let (control, probes, finish) = end.into_parts();
        let mut liveness = Liveness::Lane(probes);
        let mut control = ControlSource::lane(control);
        let outcome = self.run(input, dispatcher, &mut liveness, &mut control).await;
        // Report a finish before the control queue and the probes close, which they do when this returns, so that the
        // coordinator, once it sees them closed, takes the lane for one that finished rather than one that failed.
        if outcome.is_ok() && !control.abandoned {
            finish.report();
        }
        outcome
    }

    /// Runs the lane until its input ends and it has finished, or until its coordinator is gone.
    async fn run(
        mut self, mut input: EventsConsumer, dispatcher: &EventsDispatcher, liveness: &mut Liveness,
        control: &mut ControlSource<D::Control>,
    ) -> Result<(), GenericError> {
        // Timers are armed from the driver's state before every wait, and only reset when what they wait for changes.
        let deadline_timer = sleep_until(Instant::now());
        pin!(deadline_timer);
        let mut armed_deadline = None;
        let publish_timer = sleep_until(Instant::now());
        pin!(publish_timer);
        let mut armed_publish = None;
        // The hold timer is armed when a turn ends with the input idle, and the heartbeat timer when the point's next
        // heartbeat is due; neither is reset per batch.
        let hold_timer = sleep_until(Instant::now());
        pin!(hold_timer);
        let mut armed_hold = None;
        let heartbeat_timer = sleep_until(Instant::now());
        pin!(heartbeat_timer);
        let mut armed_heartbeat = self.admission.as_ref().and_then(OwnedAdmission::heartbeat_due);
        if let Some(due) = armed_heartbeat {
            heartbeat_timer.as_mut().reset(due);
        }
        let mut reset_wait = self
            .admission
            .as_ref()
            .and_then(OwnedAdmission::source_clock)
            .map(|clock| wait_for_reset(clock.subscribe()));

        let mut input_open = true;
        let mut finishing = false;
        // Whether a turn of input ended, by a batch, a heartbeat, or a hold that expired, and the driver has not been
        // told yet: it is told once no call is yielded.
        let mut turn_owed = false;

        // Where each exit's buffers go is the edge its output leads to the lanes of its target by, if any.
        self.routes.resolve(dispatcher);
        self.runner.start();
        // From here on the control messages are read: whoever controls the region can start timing them.
        control.mark_running();
        debug!(exits = self.routes.names.len(), lane = %self.context, "Region host started.");

        loop {
            if control.abandoned {
                debug!(lane = %self.context, "Region lane's coordinator is gone. Stopping the lane.");
                return Ok(());
            }

            // A turn of input ends once no call is yielded, with whether more input waits as of now.
            if turn_owed && self.runner.pending.is_none() {
                turn_owed = false;
                let waiting = input.queued();
                let input_idle = waiting == 0;
                if let Some(stats) = &self.stats {
                    let stamp_wait = self
                        .admission
                        .as_ref()
                        .map_or(Duration::ZERO, OwnedAdmission::lock_wait);
                    stats.record_turn(waiting, stamp_wait);
                }
                let next = self.runner.end_turn(input_idle);
                let wanted = next.filter(|_| input_idle).map(Instant::from_std);
                if wanted != armed_hold {
                    if let Some(at) = wanted {
                        hold_timer.as_mut().reset(at);
                    }
                    armed_hold = wanted;
                }
                self.dispatch(dispatcher, liveness, control).await?;
                continue;
            }

            if !input_open && self.runner.pending.is_none() {
                if self.runner.finished {
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
                self.runner.finish()?;
                self.dispatch(dispatcher, liveness, control).await?;
                continue;
            }

            let can_process = input_open && self.runner.pending.is_none() && self.runner.driver.accepts_input();

            // Another lane's point may have reset the source clock since this lane's point last stamped: the check is
            // level-triggered, after every batch and before every wait, so no reset is missed however busy the lane.
            if can_process && self.admission.as_ref().is_some_and(OwnedAdmission::reset_pending) {
                turn_owed |= self.heartbeat(Heartbeat::Reset, dispatcher, liveness, control).await?;
                continue;
            }

            let wanted_deadline = if input_open && self.runner.pending.is_none() {
                self.runner.wanted_deadline().map(Instant::from_std)
            } else {
                None
            };
            if wanted_deadline != armed_deadline {
                if let Some(deadline) = wanted_deadline {
                    deadline_timer.as_mut().reset(deadline);
                }
                armed_deadline = wanted_deadline;
            }

            let wanted_publish = self.runner.publish_deadline();
            if wanted_publish != armed_publish {
                if let Some(at) = wanted_publish {
                    publish_timer.as_mut().reset(at);
                }
                armed_publish = wanted_publish;
            }

            select! {
                biased;

                _ = liveness.answer() => {},

                request = control.next() => if let Some(request) = request {
                    self.runner.control(request)?;
                    self.dispatch(dispatcher, liveness, control).await?;
                },

                _ = &mut publish_timer, if armed_publish.is_some() => {
                    armed_publish = None;
                    self.runner.publish()?;
                },

                // Resuming is always possible, so consume the task's cooperative budget to let other tasks run
                // between the turns of a long drain, even one that writes nothing.
                _ = consume_budget(), if self.runner.pending.is_some() => {
                    self.runner.resume()?;
                    self.dispatch(dispatcher, liveness, control).await?;
                },

                _ = &mut deadline_timer, if armed_deadline.is_some() => {
                    let fired = armed_deadline.take().expect("armed per select guard");
                    self.runner.advance_time(fired.into_std())?;
                    self.dispatch(dispatcher, liveness, control).await?;
                },

                _ = &mut hold_timer, if armed_hold.is_some() && self.runner.pending.is_none() => {
                    armed_hold = None;
                    turn_owed = true;
                },

                _ = &mut heartbeat_timer, if armed_heartbeat.is_some() && can_process => {
                    turn_owed |= self.heartbeat(Heartbeat::Idle, dispatcher, liveness, control).await?;
                    // A stamp since the timer was armed moves the next heartbeat; either way it is one period after
                    // the point's latest stamp.
                    armed_heartbeat = self.admission.as_ref().and_then(OwnedAdmission::heartbeat_due);
                    if let Some(due) = armed_heartbeat {
                        heartbeat_timer.as_mut().reset(due);
                    }
                },

                // Only waited on while the input is idle; the check before every wait sees a reset that happens
                // while the lane is busy.
                resets = next_reset(&mut reset_wait), if reset_wait.is_some() && can_process => {
                    reset_wait = resets.map(wait_for_reset);
                },

                maybe_batch = input.next(), if can_process => match maybe_batch {
                    Some(batch) => {
                        self.process(batch).await?;
                        self.dispatch(dispatcher, liveness, control).await?;

                        // Keep processing batches that are already waiting, up to the turn's limit, before checking
                        // for control messages and timers again.
                        for _ in 1..self.work_per_turn.get() {
                            let busy = self.runner.pending.is_some() || !self.runner.driver.accepts_input();
                            if busy || control.abandoned {
                                break;
                            }
                            if self.admission.as_ref().is_some_and(OwnedAdmission::reset_pending) {
                                self.heartbeat(Heartbeat::Reset, dispatcher, liveness, control).await?;
                                continue;
                            }
                            match input.next().now_or_never() {
                                Some(Some(batch)) => {
                                    self.process(batch).await?;
                                    self.dispatch(dispatcher, liveness, control).await?;
                                }
                                Some(None) => {
                                    input_open = false;
                                    break;
                                }
                                None => break,
                            }
                        }
                        turn_owed = true;
                    }
                    None => input_open = false,
                },
            }
        }

        self.runner.publish()?;
        debug!(lane = %self.context, "Region host stopped.");

        Ok(())
    }

    /// Stamps a batch the lane received at its admission point, if it has one, and processes it.
    async fn process(&mut self, mut batch: EventsBuffer) -> Result<(), GenericError> {
        if let Some(stats) = &self.stats {
            stats.record_batch(batch.len() as u64);
        }
        if let Some(admission) = &self.admission {
            admission.stamp(batch.header_mut()).await;
        }
        self.runner.process(batch)
    }

    /// Stamps a heartbeat at the lane's admission point, if one is due, and processes it as a header-only batch, which
    /// the driver forwards as progress.
    ///
    /// Returns `true` if it processed a heartbeat, which counts as a turn of input.
    async fn heartbeat(
        &mut self, kind: Heartbeat, dispatcher: &EventsDispatcher, liveness: &mut Liveness,
        control: &mut ControlSource<D::Control>,
    ) -> Result<bool, GenericError> {
        let Some(admission) = &self.admission else {
            return Ok(false);
        };
        let header = match kind {
            Heartbeat::Idle => admission.stamp_heartbeat().await,
            Heartbeat::Reset => admission.stamp_reset().await,
        };
        let Some(header) = header else {
            return Ok(false);
        };
        self.runner
            .process(EventsBuffer::with_capacity(0).with_header(header))?;
        self.dispatch(dispatcher, liveness, control).await?;
        Ok(true)
    }

    async fn dispatch(
        &mut self, dispatcher: &EventsDispatcher, liveness: &mut Liveness, control: &mut ControlSource<D::Control>,
    ) -> Result<(), GenericError> {
        dispatch(&mut self.runner, &mut self.routes, dispatcher, liveness, control).await
    }
}

/// Why a lane stamps a heartbeat.
#[derive(Clone, Copy, Debug)]
enum Heartbeat {
    /// The point stamped nothing for its heartbeat period.
    Idle,

    /// Another point reset the source clock since this point last stamped.
    Reset,
}

/// Sends every buffer waiting in the runner's outbox, in order.
///
/// Each buffer goes where its exit's routing sends it ([`ExitRoutes::route`]); one that does not fit its exit's
/// routing fails the lane. Buffers written back to back to lanes of a keyed exit's target are tried first, without
/// waiting (see [`send_keyed`]); every other send is awaited. While a send waits for downstream, this keeps answering
/// liveness probes and handling control messages. The output of a control message is queued behind the buffers already
/// waiting, and sent before this returns. A lane whose coordinator is gone stops sending.
async fn dispatch<D>(
    runner: &mut Runner<D>, routes: &mut ExitRoutes, dispatcher: &EventsDispatcher, liveness: &mut Liveness,
    control: &mut ControlSource<D::Control>,
) -> Result<(), GenericError>
where
    D: RegionDriver,
{
    while let Some((exit, lane, buffer)) = runner.outbox.pop_front() {
        if control.abandoned {
            return Ok(());
        }
        let sent = match lane {
            Some(lane) if routes.is_keyed(exit) => {
                send_keyed((exit, lane, buffer), runner, routes, dispatcher, liveness, control).await?
            }
            _ => {
                let (exit_name, route) = routes.route(exit, lane)?;
                let send = async {
                    match route {
                        Some(route) => dispatcher.dispatch_named_to_lane(exit_name, route, buffer).await,
                        None => dispatcher.dispatch_named(exit_name, buffer).await,
                    }
                };
                await_send(send, exit_name, runner, liveness, control).await?
            }
        };
        if !sent {
            return Ok(());
        }
    }

    Ok(())
}

/// Sends the buffers the driver wrote back to back to lanes of the target of a keyed exit, from `first`, the exit,
/// lane, and buffer of the first of them, without waiting where a lane has room, and then awaits the rest in order.
///
/// Each buffer is tried first ([`EventsDispatcher::try_dispatch_named_to_lane`]), unless an earlier buffer for its lane
/// is held: a buffer the try returns, because its lane is full, is held, and so is every later one for that lane, so
/// that each lane receives its buffers in the order they were written. The held buffers are then awaited in order,
/// before anything written after the run: one full lane does not hold up what a call routes to the others.
///
/// Returns `false` if the lane's coordinator went away while a send waited.
async fn send_keyed<D>(
    first: (ExitId, u16, EventsBuffer), runner: &mut Runner<D>, routes: &mut ExitRoutes, dispatcher: &EventsDispatcher,
    liveness: &mut Liveness, control: &mut ControlSource<D::Control>,
) -> Result<bool, GenericError>
where
    D: RegionDriver,
{
    let (exit, lane, buffer) = first;
    let mut held: Vec<(u16, EventsBuffer)> = Vec::new();
    let mut next = Some((lane, buffer));
    while let Some((lane, buffer)) = next.take() {
        let (exit_name, route) = routes.route(exit, Some(lane))?;
        if held.iter().any(|(held_lane, _)| *held_lane == lane) {
            held.push((lane, buffer));
        } else if let Some(route) = route {
            match dispatcher.try_dispatch_named_to_lane(exit_name, route, buffer) {
                Ok(None) => {}
                Ok(Some(buffer)) => held.push((lane, buffer)),
                Err(e) => error!(exit = exit_name, error = %e, "Failed to dispatch events."),
            }
        }
        // The run goes on while the driver wrote to lanes of the same exit.
        next = match runner.outbox.front() {
            Some((next_exit, Some(next_lane), _)) if *next_exit == exit => {
                let next_lane = *next_lane;
                runner.outbox.pop_front().map(|(_, _, buffer)| (next_lane, buffer))
            }
            _ => None,
        };
    }

    for (lane, buffer) in held {
        if control.abandoned {
            return Ok(false);
        }
        let exit_name = routes.name(exit);
        let send = dispatcher.dispatch_named_to_lane(exit_name, LaneRoute::Target(lane), buffer);
        if !await_send(send, exit_name, runner, liveness, control).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Awaits one send to the exit `exit_name`, answering liveness probes and handling control messages meanwhile, whose
/// output is queued behind the buffers already waiting.
///
/// A send that fails, as one to a component that stopped does, is logged, and its buffer dropped. Returns `false` if
/// the lane's coordinator went away while the send waited.
async fn await_send<D, F>(
    send: F, exit_name: &str, runner: &mut Runner<D>, liveness: &mut Liveness, control: &mut ControlSource<D::Control>,
) -> Result<bool, GenericError>
where
    D: RegionDriver,
    F: Future<Output = Result<(), GenericError>>,
{
    pin!(send);
    loop {
        select! {
            biased;

            _ = liveness.answer() => {},

            result = &mut send => {
                if let Err(e) = result {
                    error!(exit = exit_name, error = %e, "Failed to dispatch events.");
                }
                return Ok(true);
            },

            request = control.next() => match request {
                Some(request) => runner.control(request)?,
                None if control.abandoned => return Ok(false),
                None => {}
            },
        }
    }
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
    outbox: VecDeque<(ExitId, Option<u16>, EventsBuffer)>,
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

    // Where the time of every call into the driver is counted, if the lane has statistics.
    stats: Option<Arc<LaneStats>>,
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
    fn new(
        driver: D, exit_count: usize, turn_budget: Budget, telemetry: HostTelemetry, stats: Option<Arc<LaneStats>>,
    ) -> Self {
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
            stats,
        }
    }

    /// Counts the time of a call into the driver whose turn started at `started`, if the lane has statistics.
    fn count_busy(&self, started: Instant) {
        if let Some(stats) = &self.stats {
            stats.record_busy(started.elapsed());
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
        let started = Instant::now();
        let result = {
            let mut turn = RegionTurn::new(started.into_std(), &mut budget);
            catch_unwind(AssertUnwindSafe(|| call(driver, &mut turn, sink)))
        };
        self.count_busy(started);

        match result {
            Ok(result) => {
                self.last_call_worked = !self.sink.is_empty() || budget != self.turn_budget;
                self.outbox.extend(self.sink.drain_routed());
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

    /// Ends a turn of input, queueing what the driver writes, and returns when it next wants to end one.
    ///
    /// Ending a turn runs no kernel code, so it is not supervised: a panic in it fails the component.
    fn end_turn(&mut self, input_idle: bool) -> Option<std::time::Instant> {
        let mut budget = self.turn_budget;
        let started = Instant::now();
        let now = started.into_std();
        let next = {
            let mut turn = RegionTurn::new(now, &mut budget);
            self.driver.end_turn(&mut turn, input_idle, &mut self.sink)
        };
        self.count_busy(started);
        self.outbox.extend(self.sink.drain_routed());

        // Everything due by now was just written, so a deadline that is not later is a driver bug, which would have
        // the host end turns again and again while nothing changes.
        debug_assert!(
            next.is_none_or(|next| next > now),
            "the region driver's end of turn returned a deadline that is not later than the turn, which would make \
             the host end turns again at once, forever; write what is due by the turn's instant, and return the next"
        );
        next.filter(|next| *next > now)
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
    fn control(&mut self, (message, reply): (D::Control, ControlReply)) -> Result<(), GenericError> {
        let called = self.call(PanicSite::Control, None, |driver, turn, exits| {
            driver.control(turn, message, exits)
        });

        match called {
            Called::Returned(Ok(())) => reply.ack(),
            Called::Returned(Err(failure)) => {
                warn!(error = %failure, "Region could not apply a control message.");
                reply.reject(failure.into_error());
            }
            Called::Panicked(panic) => {
                let message = panic.message().to_owned();
                let recovered = self.recover(panic);
                reply.panicked(message);
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
