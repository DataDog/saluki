//! Tests of the region host, built through its builder and driven by a scripted driver.
//!
//! Most tests run the host on its own task with a hand-built transform context, so that they control its input, its
//! downstream channels, and its control mailbox exactly. Tests that observe what the host does while nothing else
//! happens run with paused time: [`settle`] returns only once every other task is idle, because the runtime advances a
//! paused clock only when no task can make progress.

use std::{
    collections::VecDeque,
    num::{NonZeroU16, NonZeroUsize},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use proptest::prelude::*;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::test::TestRecorder;
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, sleep_until, timeout, Instant},
};

use super::{
    host::{forward_probe, join_acknowledgments, BuiltHost, DeadlineGuard, ExitRoutes, Lane, LaneParts, RegionHost},
    lanes::{lane_link, LaneEnd, LaneFinish, LaneLink},
    *,
};
use crate::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        sources::{Source, SourceBuilder, SourceContext},
        transforms::{TransformBuilder, TransformContext},
        BuildContext, ComponentContext,
    },
    data_model::event::{metric::Metric, Event, EventType},
    execution::{
        control::{
            control_mailbox, ControlAck, ControlError, ControlMailbox, ControlReceiver, ControlReply, ControlSender,
        },
        AdmissionClock, AdmissionMode, AdmissionPointConfig, AdmissionPointId, AdmissionRegistry, AdmissionSchedule,
        AdmissionTime, BatchHeader, Budget, CutBoundary, DrainPolicy, GenerationId, GenerationToken, ManualWallClock,
        OwnedAdmission, Progress, ScheduleHandle, SourceClock, WallClock as _, WorkKind,
    },
    health::HealthRegistry,
    runtime::{
        state::{DataspaceRegistry, ResourceRegistry},
        Supervisor,
    },
    support::SubsystemIdentifier,
    test_support::wait_until,
    topology::{
        interconnect::{Consumer, Dispatcher},
        ComponentId, EventsBuffer, LaneEdge, LaneRoute, OutputDefinition, OutputName, TopologyBlueprint,
        TopologyContext, DEFAULT_EVENTS_BUFFER_CAPACITY,
    },
};

const REGION_ID: &str = "region";
const INPUT_CAPACITY: usize = 16;

/// How long a test waits for something it expects before failing.
const PATIENCE: Duration = Duration::from_secs(5);

/// A call into the scripted driver, as it records it.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    Process(Vec<String>),
    Resume,
    Control(&'static str),
    AdvanceTime,
    Finish(DrainPolicy),
    Reset {
        site: PanicSite,
        lost_events: u64,
        message: String,
    },
    Publish,
}

/// The control messages the scripted driver understands.
#[derive(Debug)]
enum Command {
    /// Does nothing.
    Note(&'static str),

    /// Writes one event with this name, to the exit the name selects.
    Emit(&'static str),

    /// Starts or stops accepting input.
    Accept(bool),

    /// Fails to apply.
    Reject,

    /// Writes one event, then panics.
    Panic,

    /// Asks for `advance_time` this long from now.
    TickIn(Duration),

    /// Splits into a rejection for the given lane and an accepted note for every other lane.
    RejectIn(u16),

    /// Does not split: a note for the given lane alone.
    ForLane(u16, &'static str),
}

impl LaneSplit for Command {
    fn split(self, lanes: u16) -> Result<Vec<Self>, Self> {
        match self {
            Self::Note(label) => Ok((0..lanes).map(|_| Self::Note(label)).collect()),
            Self::Accept(accepting) => Ok((0..lanes).map(|_| Self::Accept(accepting)).collect()),
            Self::RejectIn(rejecting) => Ok((0..lanes)
                .map(|lane| {
                    if lane == rejecting {
                        Self::Reject
                    } else {
                        Self::Note("accept")
                    }
                })
                .collect()),
            other => Err(other),
        }
    }

    fn lane(&self) -> Option<u16> {
        match self {
            Self::ForLane(lane, _) => Some(*lane),
            _ => None,
        }
    }
}

/// The calls a scripted driver received, shared with the test.
///
/// Calls to `start` are recorded apart from the others, as the number of other calls before each and the turn's clock
/// reading, and so are the ends of turns, with whether the host's input was idle, so that tests of everything else see
/// only the calls they are about.
#[derive(Clone, Default)]
struct Log(
    Arc<Mutex<Vec<Call>>>,
    Arc<Mutex<Vec<(usize, std::time::Instant)>>>,
    Arc<Mutex<Vec<(usize, bool)>>>,
);

impl Log {
    fn push(&self, call: Call) {
        self.0.lock().unwrap().push(call);
    }

    fn started(&self, now: std::time::Instant) {
        let before = self.0.lock().unwrap().len();
        self.1.lock().unwrap().push((before, now));
    }

    fn starts(&self) -> Vec<(usize, std::time::Instant)> {
        self.1.lock().unwrap().clone()
    }

    fn ended_turn(&self, input_idle: bool) {
        let before = self.0.lock().unwrap().len();
        self.2.lock().unwrap().push((before, input_idle));
    }

    /// Returns, for every end of a turn, the number of other calls before it, and whether the host's input was idle.
    fn turn_ends(&self) -> Vec<(usize, bool)> {
        self.2.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }

    /// Returns the calls other than telemetry publishing, which the host paces by time.
    fn work(&self) -> Vec<Call> {
        self.calls().into_iter().filter(|call| *call != Call::Publish).collect()
    }

    fn publishes(&self) -> usize {
        self.calls().iter().filter(|call| **call == Call::Publish).count()
    }
}

const FINISH_EVENTS: [&str; 3] = ["finish.0", "finish.1", "finish.2"];
const TICK_EVENTS: [&str; 3] = ["tick", "tick.1", "tick.2"];

/// A driver whose behavior the test scripts.
///
/// Every input event is written to an exit of its own buffer, stamped with the header of the batch: the exit named by
/// a one-digit prefix (`1.x` goes to exit 1), or exit 0, and to the lane of the exit's target that an `@<lane>` suffix
/// names, if it names one (`1.x@2` goes to lane 2). Events are written one per unit of the turn's event budget, so a
/// limited budget makes calls yield. A header-only batch is passed on as a header-only buffer to exit 0. An event
/// named `panic` makes `process` write the events before it and then panic; an event named `poke` makes it send a
/// control message to its own region; an event named `stop` makes the driver stop accepting input. A driver that holds
/// output ([`holding`][Self::holding]) keeps every event whose name starts with `held` until its hold expires, which
/// [`end_turn`][RegionDriver::end_turn] writes.
struct ScriptedDriver {
    log: Log,
    header: BatchHeader,
    queue: VecDeque<Event>,
    accepting: bool,
    deadline: Option<std::time::Instant>,
    tick_period: Option<Duration>,
    tick_in_phase: bool,
    tick_events: usize,
    keeps_its_deadline: bool,
    first_tick_after_start: Option<Duration>,
    finish_events: usize,
    panics: VecDeque<PanicSite>,
    fail_on_panic: bool,
    keeps_its_drain_on_panic: bool,
    orphaned: bool,
    pokes: Option<ControlSender<Command>>,
    spin: Option<Spin>,
    hold: Option<Duration>,
    held: VecDeque<(EventsBuffer, std::time::Instant)>,
    on_process: Option<Arc<dyn Fn() + Send + Sync>>,
    panics_at_start: bool,
}

/// A drain of `turns` turns that writes nothing, recording a shared counter at each turn.
struct Spin {
    turns: usize,
    counter: Arc<AtomicUsize>,
    observed: Arc<Mutex<Vec<usize>>>,
}

impl ScriptedDriver {
    fn new(log: &Log) -> Self {
        Self {
            log: log.clone(),
            header: BatchHeader::default(),
            queue: VecDeque::new(),
            accepting: true,
            deadline: None,
            tick_period: None,
            tick_in_phase: false,
            tick_events: 1,
            keeps_its_deadline: false,
            first_tick_after_start: None,
            finish_events: 0,
            panics: VecDeque::new(),
            fail_on_panic: false,
            keeps_its_drain_on_panic: false,
            orphaned: false,
            pokes: None,
            spin: None,
            hold: None,
            held: VecDeque::new(),
            on_process: None,
            panics_at_start: false,
        }
    }

    /// Makes the driver hold every event whose name starts with `held` for `hold` after the call that received it, as
    /// a keyed exit holds a buffer it fills from several batches.
    fn holding(mut self, hold: Duration) -> Self {
        self.hold = Some(hold);
        self
    }

    /// Calls `hook` at the start of every `process`.
    fn calling_on_process(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_process = Some(Arc::new(hook));
        self
    }

    /// Makes `start` panic, which no host supervises.
    fn panicking_at_start(mut self) -> Self {
        self.panics_at_start = true;
        self
    }

    fn accepting(mut self, accepting: bool) -> Self {
        self.accepting = accepting;
        self
    }

    /// Asks for `advance_time` at `delay` from now, and then every `period`, if given.
    fn ticking(mut self, delay: Duration, period: Option<Duration>) -> Self {
        self.deadline = Some(Instant::now().into_std() + delay);
        self.tick_period = period;
        self
    }

    /// Asks for `advance_time` at `delay` from now, and then every `period` after each deadline, whenever it runs,
    /// as a timer that bursts to catch up does.
    fn ticking_in_phase(mut self, delay: Duration, period: Duration) -> Self {
        self = self.ticking(delay, Some(period));
        self.tick_in_phase = true;
        self
    }

    /// Makes every `advance_time` write `events` events, one per unit of the turn's event budget; with none, it does
    /// no work at all.
    fn ticking_with(mut self, events: usize) -> Self {
        self.tick_events = events;
        self
    }

    /// Makes `advance_time` leave the deadline as it was, which is a bug in a driver that does no work in it.
    fn keeping_its_deadline(mut self) -> Self {
        self.keeps_its_deadline = true;
        self
    }

    /// Asks for `advance_time` at `delay` after the region starts, as `start` measures it.
    fn ticking_after_start(mut self, delay: Duration) -> Self {
        self.first_tick_after_start = Some(delay);
        self
    }

    fn finishing_with(mut self, events: usize) -> Self {
        self.finish_events = events;
        self
    }

    /// Makes the given call write one event named `doomed`, and then panic, until the driver is reset after it.
    ///
    /// Given more than once, the sites take turns: after the reset that follows each panic, the next site panics.
    fn panicking_in(mut self, site: PanicSite) -> Self {
        self.panics.push_back(site);
        self
    }

    fn failing_on_panic(mut self) -> Self {
        self.fail_on_panic = true;
        self
    }

    /// Makes a reset keep the events a panic left unwritten, as a driver that rebuilds only the part that panicked
    /// does, and makes the next `finish` resume writing them rather than start over.
    fn keeping_its_drain_on_panic(mut self) -> Self {
        self.keeps_its_drain_on_panic = true;
        self
    }

    fn poking(mut self, control: &ControlSender<Command>) -> Self {
        self.pokes = Some(control.clone());
        self
    }

    /// Makes `finish` yield for `turns` turns that write nothing, recording `counter` at each.
    fn spinning(mut self, turns: usize, counter: &Arc<AtomicUsize>, observed: &Arc<Mutex<Vec<usize>>>) -> Self {
        self.spin = Some(Spin {
            turns,
            counter: Arc::clone(counter),
            observed: Arc::clone(observed),
        });
        self
    }

    /// Writes queued events while the turn's budget lasts.
    fn write(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        while !self.queue.is_empty() {
            if turn.budget().is_exhausted() {
                return Progress::Yielded;
            }
            let event = self.queue.pop_front().expect("queue is not empty");
            let name = name_of(&event);
            let exit = exit_of(&name);
            let mut buffer = EventsBuffer::with_capacity(1).with_header(self.header.clone());
            assert!(buffer.try_push(event).is_none());
            match (self.hold, lane_of(&name)) {
                (Some(hold), _) if name.starts_with("held") => self.held.push_back((buffer, turn.now() + hold)),
                (_, Some(lane)) => exits.emit_to_lane(exit, lane, buffer),
                (_, None) => exits.emit(exit, buffer),
            }
            turn.budget_mut().charge(WorkKind::Events, 1);
        }
        Progress::Done
    }

    fn panic_if(&self, site: PanicSite, exits: &mut ExitSink) {
        if self.panics.front() == Some(&site) {
            exits.emit(ExitId::new(0), batch(&["doomed"]));
            panic!("boom in {}", site);
        }
    }
}

impl RegionDriver for ScriptedDriver {
    type Control = Command;

    fn start(&mut self, turn: &mut RegionTurn<'_>) {
        self.log.started(turn.now());
        assert!(!self.panics_at_start, "boom in start");
        if let Some(delay) = self.first_tick_after_start {
            self.deadline = Some(turn.now() + delay);
        }
    }

    fn accepts_input(&self) -> bool {
        self.accepting
    }

    fn process(&mut self, turn: &mut RegionTurn<'_>, mut batch: EventsBuffer, exits: &mut ExitSink) -> Progress {
        self.log.push(Call::Process(names(&batch)));
        if let Some(hook) = &self.on_process {
            hook();
        }
        self.header = batch.take_header();
        if batch.is_empty() {
            // Header-only input sends whatever is held first, so that it never overtakes it.
            for (buffer, _) in self.held.drain(..) {
                exits.emit(ExitId::new(0), buffer);
            }
            exits.emit(
                ExitId::new(0),
                EventsBuffer::with_capacity(0).with_header(self.header.clone()),
            );
            return Progress::Done;
        }

        for event in batch {
            match name_of(&event).as_str() {
                "panic" => {
                    self.write(turn, exits);
                    panic!("boom in process");
                }
                "poke" => {
                    if let Some(pokes) = &self.pokes {
                        // The message is queued at once; the driver cannot wait for its own acknowledgment.
                        drop(pokes.send(Command::Note("poke")));
                    }
                }
                "stop" => self.accepting = false,
                _ => {}
            }
            self.queue.push_back(event);
        }
        self.write(turn, exits)
    }

    fn resume(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        if let Some(spin) = &mut self.spin {
            spin.observed.lock().unwrap().push(spin.counter.load(Ordering::Relaxed));
            spin.turns -= 1;
            if spin.turns > 0 {
                return Progress::Yielded;
            }
            self.spin = None;
            return Progress::Done;
        }

        self.log.push(Call::Resume);
        self.panic_if(PanicSite::Resume, exits);
        self.write(turn, exits)
    }

    fn control(
        &mut self, turn: &mut RegionTurn<'_>, message: Command, exits: &mut ExitSink,
    ) -> Result<(), RegionFailure> {
        match message {
            Command::Note(label) => self.log.push(Call::Control(label)),
            Command::Emit(name) => {
                self.log.push(Call::Control("emit"));
                exits.emit(exit_of(name), batch(&[name]));
            }
            Command::Accept(accepting) => {
                self.log.push(Call::Control("accept"));
                self.accepting = accepting;
            }
            Command::Reject => {
                self.log.push(Call::Control("reject"));
                return Err(RegionFailure::new(generic_error!("Generation 7 is not prepared.")));
            }
            Command::Panic => {
                self.log.push(Call::Control("panic"));
                exits.emit(ExitId::new(0), batch(&["doomed"]));
                panic!("boom in control");
            }
            Command::TickIn(delay) => {
                self.log.push(Call::Control("tick in"));
                self.deadline = Some(turn.now() + delay);
            }
            Command::ForLane(_, label) => self.log.push(Call::Control(label)),
            Command::RejectIn(_) => panic!("a rejection for one lane reaches only a region with lanes, split"),
        }
        Ok(())
    }

    fn next_deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }

    fn end_turn(
        &mut self, turn: &mut RegionTurn<'_>, input_idle: bool, exits: &mut ExitSink,
    ) -> Option<std::time::Instant> {
        self.log.ended_turn(input_idle);
        while self.held.front().is_some_and(|(_, due)| *due <= turn.now()) {
            let (buffer, _) = self.held.pop_front().expect("a held buffer is due");
            exits.emit(ExitId::new(0), buffer);
        }
        self.held.front().map(|(_, due)| *due)
    }

    fn advance_time(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        self.log.push(Call::AdvanceTime);
        self.panic_if(PanicSite::AdvanceTime, exits);
        if !self.keeps_its_deadline {
            self.deadline = match self.tick_period {
                Some(period) if self.tick_in_phase => self.deadline.map(|deadline| deadline + period),
                Some(period) => Some(turn.now() + period),
                None => None,
            };
        }
        self.queue
            .extend(TICK_EVENTS[..self.tick_events].iter().map(|name| metric(name)));
        self.write(turn, exits)
    }

    fn finish(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink, policy: DrainPolicy) -> Progress {
        self.log.push(Call::Finish(policy));
        self.panic_if(PanicSite::Finish, exits);
        for (buffer, _) in self.held.drain(..) {
            exits.emit(ExitId::new(0), buffer);
        }
        if !std::mem::take(&mut self.orphaned) {
            self.queue
                .extend(FINISH_EVENTS[..self.finish_events].iter().map(|name| metric(name)));
        }
        if self.spin.is_some() {
            return Progress::Yielded;
        }
        self.write(turn, exits)
    }

    fn reset_after_panic(&mut self, panic: &RegionPanic) -> PanicOutcome {
        self.log.push(Call::Reset {
            site: panic.site(),
            lost_events: panic.lost_events(),
            message: panic.message().to_owned(),
        });

        // A rebuilt driver has no deadline, holds nothing unless it keeps the rest of its drain, and moves on to the
        // next scripted panic.
        if self.keeps_its_drain_on_panic {
            self.orphaned = !self.queue.is_empty();
        } else {
            self.queue.clear();
        }
        self.deadline = None;
        if self.panics.front() == Some(&panic.site()) {
            self.panics.pop_front();
        }
        if self.fail_on_panic {
            PanicOutcome::Fail(generic_error!("The region has no panic budget."))
        } else {
            PanicOutcome::Continue
        }
    }

    fn publish_telemetry(&mut self) {
        self.log.push(Call::Publish);
        if self.panics.front() == Some(&PanicSite::PublishTelemetry) {
            panic!("boom in publish_telemetry");
        }
    }
}

fn metric(name: &'static str) -> Event {
    Event::Metric(Metric::gauge(name, 1.0))
}

fn batch(names: &[&'static str]) -> EventsBuffer {
    let mut buffer = EventsBuffer::with_capacity(names.len());
    for name in names {
        assert!(buffer.try_push(metric(name)).is_none());
    }
    buffer
}

fn name_of(event: &Event) -> String {
    match event {
        Event::Metric(metric) => metric.context().name().to_string(),
        other => panic!("unexpected event: {:?}", other),
    }
}

fn names(buffer: &EventsBuffer) -> Vec<String> {
    buffer.into_iter().map(name_of).collect()
}

/// Returns the lane of the exit's target an event is written to, named by an `@<lane>` suffix, if it names one.
fn lane_of(name: &str) -> Option<u16> {
    name.rsplit_once('@').and_then(|(_, lane)| lane.parse().ok())
}

/// Returns the exit an event goes to: the one named by a one-digit prefix, or exit 0.
fn exit_of(name: &str) -> ExitId {
    match name.split_once('.') {
        Some((prefix, _)) if prefix.len() == 1 => prefix.parse().map(ExitId::new).unwrap_or_else(|_| ExitId::new(0)),
        _ => ExitId::new(0),
    }
}

fn stamped_header(generation: u64) -> BatchHeader {
    BatchHeader {
        admitted_at: Some(AdmissionTime::from_unix_secs(100)),
        admission_point: Some(AdmissionPointId::new(1)),
        epoch: 0,
        generation: Some(Arc::new(GenerationToken::new(GenerationId::new(generation)))),
        entry: None,
    }
}

/// Waits until every other task is idle (see the module documentation). Requires paused time.
async fn settle() {
    sleep(Duration::from_millis(1)).await;
}

/// How a test region is wired.
struct Setup {
    exits: &'static [&'static str],
    work_per_turn: usize,
    turn_budget: Budget,
    wiring: Vec<(usize, Wire)>,
    health: HealthRegistry,
    admission: Vec<AdmissionPointConfig>,
    stats: Vec<Arc<LaneStats>>,
}

/// What one exit of a test region is connected to.
enum Wire {
    /// A downstream channel with one lane.
    One(mpsc::Sender<EventsBuffer>),

    /// The lanes of a downstream component, one channel per lane, which the exit picks among by this edge's routing.
    Lanes(Vec<mpsc::Sender<EventsBuffer>>, LaneEdge),
}

impl Setup {
    fn new(exits: &'static [&'static str]) -> Self {
        Self {
            exits,
            work_per_turn: 1,
            turn_budget: Budget::unlimited(),
            wiring: Vec::new(),
            health: HealthRegistry::new(),
            admission: Vec::new(),
            stats: Vec::new(),
        }
    }

    /// Gives the region's lanes these admission points, one per lane.
    /// Gives the region's lanes these statistics, one per lane.
    fn stats(mut self, stats: &[Arc<LaneStats>]) -> Self {
        self.stats = stats.to_vec();
        self
    }

    fn admission(mut self, points: Vec<AdmissionPointConfig>) -> Self {
        self.admission = points;
        self
    }

    fn work_per_turn(mut self, work_per_turn: usize) -> Self {
        self.work_per_turn = work_per_turn;
        self
    }

    /// Limits every call to `events` events.
    fn events_per_turn(mut self, events: u64) -> Self {
        self.turn_budget = Budget::unlimited().with_limit(WorkKind::Events, events);
        self
    }

    /// Connects `exit` to a downstream channel, after any channels already connected to it.
    fn connect(mut self, exit: usize, downstream: &mpsc::Sender<EventsBuffer>) -> Self {
        self.wiring.push((exit, Wire::One(downstream.clone())));
        self
    }

    /// Connects `exit` to the lanes of a downstream component, one channel per lane, routed by `edge`.
    fn connect_lanes(mut self, exit: usize, lanes: &[mpsc::Sender<EventsBuffer>], edge: LaneEdge) -> Self {
        self.wiring.push((exit, Wire::Lanes(lanes.to_vec(), edge)));
        self
    }

    fn health(mut self, registry: &HealthRegistry) -> Self {
        self.health = registry.clone();
        self
    }
}

/// A region host running on its own task.
struct Running {
    input: Option<mpsc::Sender<EventsBuffer>>,
    task: JoinHandle<Result<(), GenericError>>,
}

impl Running {
    /// Queues a batch on the region's input without waiting.
    fn send(&self, batch: EventsBuffer) {
        self.input
            .as_ref()
            .expect("input is open")
            .try_send(batch)
            .expect("input has room");
    }

    /// Returns the number of batches waiting on the region's input.
    fn waiting_input(&self) -> usize {
        INPUT_CAPACITY - self.input.as_ref().expect("input is open").capacity()
    }

    fn close_input(&mut self) {
        self.input = None;
    }

    async fn stopped(self) -> Result<(), GenericError> {
        timeout(PATIENCE, self.task)
            .await
            .expect("the region should stop")
            .expect("the region task should not panic")
    }
}

/// Builds the region host through its builder, wires it as a topology would, and runs it on its own task.
///
/// The task does not start running before the caller yields, so batches sent right after this returns are all waiting
/// when the host first looks at its input.
async fn start(setup: Setup, mailbox: ControlMailbox<Command>, driver: ScriptedDriver) -> Running {
    let builder = RegionHostBuilder::from_driver(
        setup.exits.iter().copied(),
        NonZeroUsize::new(setup.work_per_turn).expect("work per turn is not zero"),
        mailbox,
        driver,
    )
    .expect("exits are valid")
    .with_turn_budget(setup.turn_budget)
    .with_lane_admission(setup.admission)
    .with_lane_stats(setup.stats);
    let transform = builder
        .build(BuildContext::test_transform(REGION_ID))
        .await
        .expect("the region should build");

    let component_context = ComponentContext::test_transform(REGION_ID);
    let dispatcher = wired_dispatcher(&builder, setup.exits, setup.wiring);

    let (input, input_rx) = mpsc::channel(INPUT_CAPACITY);
    let health = setup
        .health
        .register_component(&SubsystemIdentifier::from_dotted(REGION_ID))
        .expect("the region is registered once");
    let topology_context = TopologyContext::new(
        Arc::from("test"),
        MemoryLimiter::noop(),
        setup.health.clone(),
        Handle::current(),
        DataspaceRegistry::new(),
    );
    let context = TransformContext::new(
        &topology_context,
        &component_context,
        ComponentRegistry::default(),
        health,
        dispatcher,
        Consumer::new(component_context.clone(), input_rx),
    );

    Running {
        input: Some(input),
        task: tokio::spawn(transform.run(context)),
    }
}

/// Builds the dispatcher of the region `builder` builds, with an output per exit, wired to the given channels.
fn wired_dispatcher<D: RegionDriver>(
    builder: &RegionHostBuilder<D>, exits: &[&str], wiring: Vec<(usize, Wire)>,
) -> Dispatcher<EventsBuffer> {
    let mut dispatcher = Dispatcher::new(ComponentContext::test_transform(REGION_ID));
    for output in builder.outputs() {
        let name = output.output_name().expect("exits are named outputs").to_string();
        dispatcher.add_output(OutputName::Given(name.into())).unwrap();
    }
    for (index, (exit, wire)) in wiring.into_iter().enumerate() {
        let output = OutputName::Given(exits[exit].to_string().into());
        match wire {
            Wire::One(downstream) => dispatcher.attach_sender_to_output(&output, downstream).unwrap(),
            Wire::Lanes(lanes, edge) => {
                let component = ComponentId::try_from(format!("downstream_{}", index).as_str()).unwrap();
                dispatcher
                    .attach_component_lanes_to_output(&output, component, lanes, edge)
                    .unwrap()
            }
        }
    }
    dispatcher
}

/// A region host with lanes running on its own task: its coordinator, which runs a task per lane.
struct LanedRunning {
    inputs: Vec<Option<mpsc::Sender<EventsBuffer>>>,
    task: JoinHandle<Result<(), GenericError>>,
}

impl LanedRunning {
    /// Queues a batch on one lane's input without waiting.
    fn send(&self, lane: usize, batch: EventsBuffer) {
        self.inputs[lane]
            .as_ref()
            .expect("input is open")
            .try_send(batch)
            .expect("input has room");
    }

    fn close_input(&mut self, lane: usize) {
        self.inputs[lane] = None;
    }

    fn close_inputs(&mut self) {
        self.inputs.iter_mut().for_each(|input| *input = None);
    }

    async fn stopped(self) -> Result<(), GenericError> {
        timeout(PATIENCE, self.task)
            .await
            .expect("the region should stop")
            .expect("the region task should not panic")
    }
}

/// Builds a region host with `lanes` lanes through its builder, its lanes' drivers from `driver`, and runs its
/// coordinator on its own task, as a topology would run the transform, with one input channel per lane.
async fn start_laned<F>(setup: Setup, mailbox: ControlMailbox<Command>, lanes: u16, driver: F) -> LanedRunning
where
    F: Fn(LaneContext) -> ScriptedDriver + Send + Sync + 'static,
{
    let builder = RegionHostBuilder::new(
        setup.exits.iter().copied(),
        NonZeroUsize::new(setup.work_per_turn).expect("work per turn is not zero"),
        mailbox,
        |_: &BuildContext| Err(generic_error!("a region with lanes builds its drivers per lane")),
    )
    .expect("exits are valid")
    .with_turn_budget(setup.turn_budget)
    .with_lanes(NonZeroU16::new(lanes).expect("lanes"), move |_, lane| Ok(driver(lane)))
    .with_lane_admission(setup.admission)
    .with_lane_stats(setup.stats);
    let host = match builder.build_host(&BuildContext::test_transform(REGION_ID)) {
        Ok(BuiltHost::Laned(host)) => host,
        Ok(BuiltHost::One(_)) => panic!("a region with {} lanes builds a host with lanes", lanes),
        Err(error) => panic!("the region should build: {:#}", error),
    };

    let dispatcher = wired_dispatcher(&builder, setup.exits, setup.wiring);
    let (inputs, consumers): (Vec<_>, Vec<_>) = (0..lanes)
        .map(|_| {
            let (input, input_rx) = mpsc::channel(INPUT_CAPACITY);
            (
                Some(input),
                Consumer::new(ComponentContext::test_transform(REGION_ID), input_rx),
            )
        })
        .unzip();
    let health = setup
        .health
        .register_component(&SubsystemIdentifier::from_dotted(REGION_ID))
        .expect("the region is registered once");
    let parts = LaneParts {
        inputs: consumers,
        dispatcher: Arc::new(dispatcher),
        health,
        group: SubsystemIdentifier::from_dotted(REGION_ID),
    };

    LanedRunning {
        inputs,
        task: tokio::spawn(host.coordinate(parts, REGION_ID.to_string())),
    }
}

fn downstream(capacity: usize) -> (mpsc::Sender<EventsBuffer>, mpsc::Receiver<EventsBuffer>) {
    mpsc::channel(capacity)
}

/// Waits for the next buffer on a downstream channel.
async fn receive(downstream: &mut mpsc::Receiver<EventsBuffer>) -> EventsBuffer {
    timeout(PATIENCE, downstream.recv())
        .await
        .expect("a buffer should arrive")
        .expect("the channel should stay open")
}

/// Receives `count` buffers, returning the event names of each.
async fn receive_names(downstream: &mut mpsc::Receiver<EventsBuffer>, count: usize) -> Vec<Vec<String>> {
    let mut received = Vec::new();
    for _ in 0..count {
        received.push(names(&receive(downstream).await));
    }
    received
}

/// Returns every buffer already waiting on a downstream channel.
fn waiting(downstream: &mut mpsc::Receiver<EventsBuffer>) -> Vec<EventsBuffer> {
    let mut received = Vec::new();
    while let Ok(buffer) = downstream.try_recv() {
        received.push(buffer);
    }
    received
}

fn names_of_all(buffers: &[EventsBuffer]) -> Vec<Vec<String>> {
    buffers.iter().map(names).collect()
}

/// Spells out expected buffers' event names.
fn expected(buffers: &[&[&str]]) -> Vec<Vec<String>> {
    buffers
        .iter()
        .map(|names| names.iter().map(|name| name.to_string()).collect())
        .collect()
}

fn process(names: &[&str]) -> Call {
    Call::Process(names.iter().map(|name| name.to_string()).collect())
}

async fn acknowledged(ack: ControlAck) -> Result<(), ControlError> {
    timeout(PATIENCE, ack)
        .await
        .expect("the control message should be acknowledged")
}

/// Returns the labels of a metric the host registers for the test region.
fn region_labels(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut labels = vec![("component_id", REGION_ID), ("component_type", "transform")];
    labels.extend_from_slice(extra);
    labels
}

fn lost_events(recorder: &TestRecorder) -> Option<u64> {
    let labels = region_labels(&[
        ("intentional", "false"),
        ("drop_reason", "region_failure"),
        ("loss_class", "invalid"),
    ]);
    recorder.counter(("component_events_dropped_total", labels.as_slice()))
}

fn panics(recorder: &TestRecorder) -> Option<u64> {
    let labels = region_labels(&[("error_type", "panic")]);
    recorder.counter(("component_errors_total", labels.as_slice()))
}

#[tokio::test]
async fn exits_receive_buffers_in_the_order_the_driver_wrote_them() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (shared, mut shared_rx) = downstream(16);
    let (second, mut second_rx) = downstream(16);

    // Exit 0 sends to `shared`; exit 1 to `shared` and `second`; exit 2 is connected to nothing.
    let setup = Setup::new(&["first", "second", "idle"])
        .connect(0, &shared)
        .connect(1, &shared)
        .connect(1, &second);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;
    drop((shared, second));

    region.send(batch(&["1.a", "b", "2.c", "1.d"]).with_header(stamped_header(7)));
    region.send(EventsBuffer::with_capacity(0).with_header(stamped_header(7)));
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");

    // One channel fed by two exits sees their buffers in the order the driver wrote them, and the header-only buffer
    // is delivered like any other.
    let shared_buffers = waiting(&mut shared_rx);
    assert_eq!(
        names_of_all(&shared_buffers),
        expected(&[&["1.a"], &["b"], &["1.d"], &[]])
    );
    for buffer in &shared_buffers {
        assert_eq!(buffer.header().generation_id(), Some(GenerationId::new(7)));
        assert_eq!(buffer.header().admitted_at, Some(AdmissionTime::from_unix_secs(100)));
    }
    assert_eq!(names_of_all(&waiting(&mut second_rx)), expected(&[&["1.a"], &["1.d"]]));

    // The exit with nothing connected discards what it is sent, and counts it.
    let idle_labels = region_labels(&[("output", "idle")]);
    assert_eq!(
        recorder.counter(("component_events_discarded_total", idle_labels.as_slice())),
        Some(1)
    );
    assert_eq!(
        log.work(),
        vec![
            process(&["1.a", "b", "2.c", "1.d"]),
            process(&[]),
            Call::Finish(DrainPolicy::Shutdown)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn control_messages_are_handled_while_an_exit_send_is_blocked() {
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out),
        mailbox,
        ScriptedDriver::new(&log),
    )
    .await;

    // The channel holds one buffer, so the host delivers `a` and then waits to send `b`.
    region.send(batch(&["a", "b", "c"]));
    settle().await;
    assert_eq!(out_rx.len(), 1);
    assert_eq!(log.work(), vec![process(&["a", "b", "c"])]);

    // The control message is applied and acknowledged while the send still waits.
    acknowledged(control.send(Command::Emit("late"))).await.unwrap();
    assert_eq!(out_rx.len(), 1);
    assert_eq!(log.work(), vec![process(&["a", "b", "c"]), Call::Control("emit")]);

    // Its output is delivered after the buffers that were already waiting.
    assert_eq!(
        receive_names(&mut out_rx, 4).await,
        expected(&[&["a"], &["b"], &["c"], &["late"]])
    );

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn input_waits_while_the_driver_does_not_accept_it() {
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log).accepting(false);
    let setup = Setup::new(&["out"]).connect(0, &out).work_per_turn(8);
    let mut region = start(setup, mailbox, driver).await;

    region.send(batch(&["a"]));
    settle().await;
    assert_eq!(
        region.waiting_input(),
        1,
        "the host must not take input the driver does not accept"
    );
    assert!(log.calls().is_empty());

    // Once a call makes the driver accept input again, the host takes the waiting batch.
    acknowledged(control.send(Command::Accept(true))).await.unwrap();
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["a"]]));
    assert_eq!(region.waiting_input(), 0);
    assert_eq!(log.work(), vec![Call::Control("accept"), process(&["a"])]);

    // A batch that makes the driver stop accepting input also stops the batches waiting behind it, although the turn
    // has room for more.
    region.send(batch(&["stop"]));
    region.send(batch(&["b"]));
    settle().await;
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["stop"]]));
    assert_eq!(region.waiting_input(), 1);

    acknowledged(control.send(Command::Accept(true))).await.unwrap();
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["b"]]));
    assert_eq!(
        log.work(),
        vec![
            Call::Control("accept"),
            process(&["a"]),
            process(&["stop"]),
            Call::Control("accept"),
            process(&["b"]),
        ]
    );

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn yielded_calls_are_resumed_before_the_next_batch_and_deliver_their_output_as_they_go() {
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(2);
    let setup = Setup::new(&["out"])
        .connect(0, &out)
        .events_per_turn(2)
        .work_per_turn(8);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;

    region.send(batch(&["a", "b", "c", "d", "e"]));
    region.send(batch(&["f"]));
    settle().await;

    // Each turn writes two events. The first turn's output was delivered (filling the channel) before the batch was
    // finished; the host then resumed the call, and waits to deliver its output. The next batch waits meanwhile.
    assert_eq!(out_rx.len(), 2);
    assert_eq!(log.work(), vec![process(&["a", "b", "c", "d", "e"]), Call::Resume]);
    assert_eq!(region.waiting_input(), 1);

    assert_eq!(
        receive_names(&mut out_rx, 6).await,
        expected(&[&["a"], &["b"], &["c"], &["d"], &["e"], &["f"]])
    );
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    assert_eq!(
        log.work(),
        vec![
            process(&["a", "b", "c", "d", "e"]),
            Call::Resume,
            Call::Resume,
            process(&["f"]),
            Call::Finish(DrainPolicy::Shutdown),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn time_driven_work_runs_when_the_drivers_deadline_passes() {
    let started = Instant::now();
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log).ticking(Duration::from_secs(10), Some(Duration::from_secs(10)));
    let mut region = start(Setup::new(&["out"]).connect(0, &out), mailbox, driver).await;

    sleep_until(started + Duration::from_millis(9_900)).await;
    assert!(log.work().is_empty(), "nothing is due before the deadline");

    sleep_until(started + Duration::from_millis(10_100)).await;
    assert_eq!(log.work(), vec![Call::AdvanceTime]);
    assert_eq!(names_of_all(&waiting(&mut out_rx)), expected(&[&["tick"]]));

    // The driver asked for the next run ten seconds after the first.
    sleep_until(started + Duration::from_millis(19_900)).await;
    assert_eq!(log.work(), vec![Call::AdvanceTime]);
    sleep_until(started + Duration::from_millis(20_100)).await;
    assert_eq!(log.work(), vec![Call::AdvanceTime, Call::AdvanceTime]);

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    assert_eq!(
        log.work(),
        vec![
            Call::AdvanceTime,
            Call::AdvanceTime,
            Call::Finish(DrainPolicy::Shutdown)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_driver_starts_once_before_any_other_call_and_schedules_from_its_start() {
    let built = Instant::now();
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log).ticking_after_start(Duration::from_secs(10));

    // The region starts five seconds after its driver was built, with a control message and a batch already waiting.
    sleep(Duration::from_secs(5)).await;
    let mut region = start(Setup::new(&["out"]).connect(0, &out), mailbox, driver).await;
    let ack = control.send(Command::Note("early"));
    region.send(batch(&["a"]));
    acknowledged(ack).await.unwrap();
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["a"]]));

    // `start` ran once, before either, at the time the region started.
    let starts = log.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].0, 0, "start precedes every other call");
    assert_eq!(starts[0].1, (built + Duration::from_secs(5)).into_std());
    assert_eq!(log.work(), vec![Call::Control("early"), process(&["a"])]);

    // The deadline the driver set in `start` is measured from the start, not from when it was built.
    sleep_until(built + Duration::from_millis(14_900)).await;
    assert!(!log.work().contains(&Call::AdvanceTime));
    sleep_until(built + Duration::from_millis(15_100)).await;
    assert_eq!(log.work().last(), Some(&Call::AdvanceTime));

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    assert_eq!(log.starts().len(), 1, "start runs once in the region's life");
}

#[tokio::test(start_paused = true)]
async fn a_deadline_the_driver_withdraws_never_runs() {
    let started = Instant::now();
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, _out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log).ticking(Duration::from_secs(10), None);
    let mut region = start(Setup::new(&["out"]).connect(0, &out), mailbox, driver).await;

    sleep_until(started + Duration::from_millis(10_100)).await;
    assert_eq!(log.work(), vec![Call::AdvanceTime]);

    // After the run, the driver has no deadline, so nothing else runs however long the region waits.
    sleep_until(started + Duration::from_secs(100)).await;
    acknowledged(control.send(Command::Note("still here"))).await.unwrap();
    assert_eq!(log.work(), vec![Call::AdvanceTime, Call::Control("still here")]);

    // A deadline the driver sets in any call is armed when that call returns.
    acknowledged(control.send(Command::TickIn(Duration::from_secs(5))))
        .await
        .unwrap();
    sleep_until(started + Duration::from_millis(104_900)).await;
    assert_eq!(log.work().last(), Some(&Call::Control("tick in")));
    sleep_until(started + Duration::from_millis(105_100)).await;
    assert_eq!(log.work().last(), Some(&Call::AdvanceTime));

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn control_messages_are_applied_while_a_call_is_yielded_and_input_waits_until_it_completes() {
    let started = Instant::now();
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let driver = ScriptedDriver::new(&log)
        .ticking(Duration::from_secs(1), None)
        .ticking_with(3);
    let setup = Setup::new(&["out"])
        .connect(0, &out)
        .events_per_turn(1)
        .work_per_turn(8);
    let mut region = start(setup, mailbox, driver).await;

    // One event per turn: at the deadline, `tick` is delivered, `tick.1` waits to be sent, and the call is yielded
    // with `tick.2` left. A batch waits on the input meanwhile.
    sleep_until(started + Duration::from_millis(1_100)).await;
    region.send(batch(&["a"]));
    settle().await;
    assert_eq!(log.work(), vec![Call::AdvanceTime, Call::Resume]);

    // A control message is applied and acknowledged while the call is yielded, and the input is not read.
    acknowledged(control.send(Command::Emit("late"))).await.unwrap();
    assert_eq!(log.work(), vec![Call::AdvanceTime, Call::Resume, Call::Control("emit")]);
    assert_eq!(region.waiting_input(), 1);

    // The message's output follows what was already waiting to be sent; the yielded call then completes, and only
    // then is the batch processed.
    assert_eq!(
        receive_names(&mut out_rx, 5).await,
        expected(&[&["tick"], &["tick.1"], &["late"], &["tick.2"], &["a"]])
    );
    assert_eq!(
        log.work(),
        vec![
            Call::AdvanceTime,
            Call::Resume,
            Call::Control("emit"),
            Call::Resume,
            process(&["a"]),
        ]
    );

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_finish_that_yields_again_and_again_completes_with_control_served_and_no_time_driven_work() {
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let driver = ScriptedDriver::new(&log)
        .ticking(Duration::from_secs(1), Some(Duration::from_secs(1)))
        .finishing_with(3);
    let setup = Setup::new(&["out"]).connect(0, &out).events_per_turn(1);
    let mut region = start(setup, mailbox, driver).await;

    // The input ends at once: `finish.0` is delivered, `finish.1` waits to be sent, and the call is yielded.
    region.close_input();
    settle().await;
    assert_eq!(log.work(), vec![Call::Finish(DrainPolicy::Shutdown), Call::Resume]);

    // Deadlines that pass while the region finishes never run, and control messages are still served.
    sleep(Duration::from_secs(5)).await;
    acknowledged(control.send(Command::Emit("late"))).await.unwrap();
    assert_eq!(
        log.work(),
        vec![Call::Finish(DrainPolicy::Shutdown), Call::Resume, Call::Control("emit")]
    );

    assert_eq!(
        receive_names(&mut out_rx, 4).await,
        expected(&[&["finish.0"], &["finish.1"], &["late"], &["finish.2"]])
    );
    region.stopped().await.expect("the region should stop cleanly");
    assert_eq!(
        log.calls(),
        vec![
            Call::Finish(DrainPolicy::Shutdown),
            Call::Resume,
            Call::Control("emit"),
            Call::Publish,
            Call::Resume,
            Call::Publish,
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn deadlines_that_pass_while_a_call_is_yielded_run_at_once_and_catch_up_before_more_input() {
    let started = Instant::now();
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let driver = ScriptedDriver::new(&log).ticking_in_phase(Duration::from_secs(1), Duration::from_secs(1));
    let setup = Setup::new(&["out"])
        .connect(0, &out)
        .events_per_turn(1)
        .work_per_turn(8);
    let mut region = start(setup, mailbox, driver).await;

    // One event per turn, and room for one buffer downstream: the batch's call is yielded, waiting to send `b`, while
    // the deadlines at 1, 2, and 3 seconds pass. Another batch waits on the input.
    region.send(batch(&["a", "b", "c"]));
    region.send(batch(&["d"]));
    sleep_until(started + Duration::from_millis(3_500)).await;
    assert_eq!(log.work(), vec![process(&["a", "b", "c"]), Call::Resume]);

    // Once the call completes, the passed deadlines run one after another, before the waiting batch.
    assert_eq!(
        receive_names(&mut out_rx, 7).await,
        expected(&[&["a"], &["b"], &["c"], &["tick"], &["tick"], &["tick"], &["d"]])
    );
    assert_eq!(
        log.work(),
        vec![
            process(&["a", "b", "c"]),
            Call::Resume,
            Call::Resume,
            Call::AdvanceTime,
            Call::AdvanceTime,
            Call::AdvanceTime,
            process(&["d"]),
        ]
    );

    // The next deadline keeps the timer's phase.
    sleep_until(started + Duration::from_millis(3_900)).await;
    assert_eq!(log.work().iter().filter(|call| **call == Call::AdvanceTime).count(), 3);
    sleep_until(started + Duration::from_millis(4_100)).await;
    assert_eq!(log.work().iter().filter(|call| **call == Call::AdvanceTime).count(), 4);

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn idle_time_driven_work_that_moves_its_deadline_forward_catches_up_without_spinning() {
    // A driver whose scheduled work is skipped does nothing at its deadlines, but moves each one a period forward,
    // even when the next one has also passed: the host runs it once per passed deadline, and then waits.
    let started = Instant::now();
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let driver = ScriptedDriver::new(&log)
        .ticking_in_phase(Duration::from_secs(1), Duration::from_secs(1))
        .ticking_with(0);
    let setup = Setup::new(&["out"]).connect(0, &out).events_per_turn(1);
    let mut region = start(setup, mailbox, driver).await;

    region.send(batch(&["a", "b", "c"]));
    sleep_until(started + Duration::from_millis(3_500)).await;
    assert_eq!(receive_names(&mut out_rx, 3).await, expected(&[&["a"], &["b"], &["c"]]));
    acknowledged(control.send(Command::Note("caught up"))).await.unwrap();
    assert_eq!(
        log.work(),
        vec![
            process(&["a", "b", "c"]),
            Call::Resume,
            Call::Resume,
            Call::AdvanceTime,
            Call::AdvanceTime,
            Call::AdvanceTime,
            Call::Control("caught up"),
        ]
    );

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[test]
fn the_host_disarms_only_a_past_deadline_that_the_driver_kept_after_doing_nothing() {
    let base = std::time::Instant::now();
    let at = |secs| base + Duration::from_secs(secs);

    // (whether the work did nothing, the driver's next deadline, whether the driver stalled on the fired deadline)
    let cases = [
        (true, Some(at(1)), true),
        // Work that did something may ask to run again at once, as a burst.
        (false, Some(at(1)), false),
        // Idle work that moves its deadline forward, even to one that has also passed, catches up.
        (true, Some(at(2)), false),
        // An earlier deadline is other work that is also due, such as work that was due before the driver completed
        // what a panic ended, at a deadline it had set to the time of the panic.
        (true, Some(at(0)), false),
        (true, None, false),
    ];
    for (idle, next, stalled) in cases {
        let mut guard = DeadlineGuard::default();
        assert_eq!(guard.ran(at(1), idle, next), stalled, "{:?}", (idle, next));
        assert_eq!(guard.arm(next), if stalled { None } else { next }, "{:?}", (idle, next));
    }

    // A stalled deadline stays disarmed for as long as the driver keeps it, and is forgotten once it changes.
    let mut guard = DeadlineGuard::default();
    assert!(guard.ran(at(1), true, Some(at(1))));
    assert_eq!(guard.arm(Some(at(1))), None);
    assert_eq!(guard.arm(Some(at(1))), None);
    assert_eq!(guard.arm(Some(at(5))), Some(at(5)));
    assert_eq!(guard.arm(Some(at(1))), Some(at(1)));
}

// The workspace's test and release profiles both enable debug assertions, so tests catch the driver bug. A build
// without them only stops arming the deadline, which the test above covers.
#[cfg(debug_assertions)]
#[tokio::test(start_paused = true)]
async fn a_driver_that_keeps_a_past_deadline_after_doing_nothing_fails_a_debug_assertion() {
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, _out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log)
        .ticking(Duration::from_secs(1), None)
        .ticking_with(0)
        .keeping_its_deadline();
    let region = start(Setup::new(&["out"]).connect(0, &out), mailbox, driver).await;

    let payload = timeout(PATIENCE, region.task)
        .await
        .expect("the region should stop")
        .expect_err("the host should fail its assertion")
        .into_panic();
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| message.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("kept a past deadline")),
        "unexpected panic: {:?}",
        message
    );
    assert_eq!(log.work(), vec![Call::AdvanceTime]);
}

#[tokio::test]
async fn a_panic_in_process_discards_the_batch_counts_its_events_and_the_region_continues() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out),
        mailbox,
        ScriptedDriver::new(&log),
    )
    .await;

    // The driver writes `a` before it panics; none of the batch is delivered.
    region.send(batch(&["a", "panic", "b"]));
    region.send(batch(&["c"]));
    region.close_input();
    region
        .stopped()
        .await
        .expect("the region should recover and stop cleanly");

    assert_eq!(names_of_all(&waiting(&mut out_rx)), expected(&[&["c"]]));
    assert_eq!(
        log.work(),
        vec![
            process(&["a", "panic", "b"]),
            Call::Reset {
                site: PanicSite::Process,
                lost_events: 3,
                message: "boom in process".to_string(),
            },
            process(&["c"]),
            Call::Finish(DrainPolicy::Shutdown),
        ]
    );
    assert_eq!(lost_events(&recorder), Some(3));
    assert_eq!(panics(&recorder), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_panic_that_fails_the_region_stops_it_with_the_drivers_error() {
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log).failing_on_panic();
    let region = start(Setup::new(&["out"]).connect(0, &out), mailbox, driver).await;

    region.send(batch(&["a", "panic"]));
    region.send(batch(&["never"]));
    let error = region.stopped().await.expect_err("the region should fail");

    assert_eq!(
        format!("{:#}", error),
        "Region panicked in process: boom in process: The region has no panic budget."
    );
    assert!(
        waiting(&mut out_rx).is_empty(),
        "a failed region delivers nothing of the failed batch"
    );
    assert_eq!(
        log.work(),
        vec![
            process(&["a", "panic"]),
            Call::Reset {
                site: PanicSite::Process,
                lost_events: 2,
                message: "boom in process".to_string(),
            },
        ]
    );

    // The region is gone, so control messages are acknowledged as stopped.
    assert!(control.is_closed());
    assert!(matches!(
        acknowledged(control.send(Command::Note("late"))).await,
        Err(ControlError::Stopped { .. })
    ));
}

/// How a test makes a driver call panic.
#[derive(Clone, Copy, Debug)]
enum PanicTrigger {
    /// A control message whose handling panics.
    ControlMessage,
    /// The driver's deadline, one second after start.
    Deadline,
    /// A batch that makes `process` yield, with one event per turn.
    YieldedBatch,
    /// A control message, after which the host publishes telemetry.
    Publishing,
    /// The end of the input.
    EndOfInput,
}

#[tokio::test(start_paused = true)]
async fn panics_in_every_supervised_call_discard_its_output_and_the_region_recovers() {
    struct Case {
        site: PanicSite,
        trigger: PanicTrigger,
        lost_events: u64,
    }

    let cases = [
        Case {
            site: PanicSite::Control,
            trigger: PanicTrigger::ControlMessage,
            lost_events: 1,
        },
        Case {
            site: PanicSite::AdvanceTime,
            trigger: PanicTrigger::Deadline,
            lost_events: 1,
        },
        // The yielded batch has two events, and its first turn delivered one of them already.
        Case {
            site: PanicSite::Resume,
            trigger: PanicTrigger::YieldedBatch,
            lost_events: 2,
        },
        Case {
            site: PanicSite::PublishTelemetry,
            trigger: PanicTrigger::Publishing,
            lost_events: 0,
        },
        Case {
            site: PanicSite::Finish,
            trigger: PanicTrigger::EndOfInput,
            lost_events: 1,
        },
    ];

    for case in cases {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let log = Log::default();
        let (control, mailbox) = control_mailbox();
        let (out, mut out_rx) = downstream(16);
        let mut setup = Setup::new(&["out"]).connect(0, &out);
        let mut driver = ScriptedDriver::new(&log).finishing_with(1);
        // Control-message panics come from the message itself rather than from the driver's script.
        if !matches!(case.trigger, PanicTrigger::ControlMessage) {
            driver = driver.panicking_in(case.site);
        }
        if matches!(case.trigger, PanicTrigger::Deadline) {
            driver = driver.ticking(Duration::from_secs(1), None);
        }
        if matches!(case.trigger, PanicTrigger::YieldedBatch) {
            setup = setup.events_per_turn(1);
        }
        let mut region = start(setup, mailbox, driver).await;

        let mut delivered_before = Vec::new();
        match case.trigger {
            PanicTrigger::ControlMessage => {
                let ack = acknowledged(control.send(Command::Panic)).await;
                assert!(
                    matches!(&ack, Err(ControlError::Panicked { message, .. }) if message == "boom in control"),
                    "{:?}: unexpected acknowledgment {:?}",
                    case.site,
                    ack
                );
            }
            PanicTrigger::Deadline => sleep(Duration::from_secs(2)).await,
            PanicTrigger::YieldedBatch => {
                region.send(batch(&["a", "b"]));
                delivered_before.push(vec!["a".to_string()]);
                settle().await;
            }
            PanicTrigger::Publishing => {
                acknowledged(control.send(Command::Note("publish"))).await.unwrap();
            }
            PanicTrigger::EndOfInput => {}
        }

        // The region keeps running after the panic. A panic in finishing it makes the host finish it again, which the
        // rebuilt driver does from the start, since the panic came before it wrote anything that survived.
        let in_finish = matches!(case.trigger, PanicTrigger::EndOfInput);
        if !in_finish {
            region.send(batch(&["after"]));
        }
        region.close_input();
        region
            .stopped()
            .await
            .expect("the region should recover and stop cleanly");

        let mut delivered = delivered_before;
        if !in_finish {
            delivered.push(vec!["after".to_string()]);
        }
        delivered.push(vec!["finish.0".to_string()]);
        assert_eq!(names_of_all(&waiting(&mut out_rx)), delivered, "{:?}", case.site);

        let resets = log
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Reset { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            resets,
            vec![Call::Reset {
                site: case.site,
                lost_events: case.lost_events,
                message: format!("boom in {}", case.site),
            }],
            "{:?}",
            case.site
        );
        assert_eq!(lost_events(&recorder), Some(case.lost_events), "{:?}", case.site);
        assert_eq!(panics(&recorder), Some(1), "{:?}", case.site);
    }
}

#[tokio::test(start_paused = true)]
async fn a_panic_while_a_batch_is_yielded_ends_the_batch_and_counts_its_events() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let setup = Setup::new(&["out"]).connect(0, &out).events_per_turn(1);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;

    // One event per turn: `a` is delivered, `b` waits to be sent, and the batch's call is yielded with `c` left.
    region.send(batch(&["a", "b", "c"]));
    settle().await;
    assert_eq!(log.work(), vec![process(&["a", "b", "c"]), Call::Resume]);

    // A panic in a control message ends the yielded batch. The panic loses the control message's own event and the
    // yielded batch (an upper bound, since part of it was delivered); what earlier turns wrote is still delivered.
    assert!(matches!(
        acknowledged(control.send(Command::Panic)).await,
        Err(ControlError::Panicked { .. })
    ));
    region.send(batch(&["after"]));
    assert_eq!(
        receive_names(&mut out_rx, 3).await,
        expected(&[&["a"], &["b"], &["after"]])
    );

    region.close_input();
    region
        .stopped()
        .await
        .expect("the region should recover and stop cleanly");
    assert_eq!(
        log.work(),
        vec![
            process(&["a", "b", "c"]),
            Call::Resume,
            Call::Control("panic"),
            Call::Reset {
                site: PanicSite::Control,
                lost_events: 4,
                message: "boom in control".to_string(),
            },
            process(&["after"]),
            Call::Finish(DrainPolicy::Shutdown),
        ]
    );
    assert_eq!(lost_events(&recorder), Some(4));
}

#[tokio::test(start_paused = true)]
async fn the_region_stays_live_while_an_exit_send_waits_for_a_long_time() {
    let registry = HealthRegistry::new();
    let _probes = registry.spawn_runner_for_test();
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(1);
    let setup = Setup::new(&["out"]).connect(0, &out).health(&registry);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;

    // The host delivers `a`, then waits for a minute to send `b`. Probes time out after five seconds unanswered.
    region.send(batch(&["a", "b"]));
    sleep(Duration::from_secs(60)).await;
    assert_eq!(out_rx.len(), 1, "the send should still be waiting");
    assert!(registry.is_live_for_test(&SubsystemIdentifier::from_dotted(REGION_ID)));

    assert_eq!(receive_names(&mut out_rx, 2).await, expected(&[&["a"], &["b"]]));
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn at_the_end_of_input_the_region_completes_yielded_work_finishes_and_stops() {
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let setup = Setup::new(&["out"]).connect(0, &out).events_per_turn(1);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log).finishing_with(3)).await;

    region.send(batch(&["a", "b"]));
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");

    assert_eq!(
        names_of_all(&waiting(&mut out_rx)),
        expected(&[&["a"], &["b"], &["finish.0"], &["finish.1"], &["finish.2"]])
    );
    assert_eq!(
        log.calls(),
        vec![
            process(&["a", "b"]),
            Call::Resume,
            Call::Finish(DrainPolicy::Shutdown),
            Call::Resume,
            Call::Resume,
            Call::Publish,
        ]
    );
    assert!(matches!(
        acknowledged(control.send(Command::Note("late"))).await,
        Err(ControlError::Stopped { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_panic_that_ends_the_finish_makes_the_host_finish_again_until_a_call_completes_it() {
    struct Case {
        name: &'static str,
        // The calls that panic, in turn, and whether a control message panics while the finish is yielded.
        panics: &'static [PanicSite],
        control_panics: bool,
        fails: bool,
        work: Vec<Call>,
        delivered: Vec<Vec<String>>,
    }

    let reset = |site: PanicSite| Call::Reset {
        site,
        lost_events: 1,
        message: format!("boom in {}", site),
    };
    let finish = Call::Finish(DrainPolicy::Shutdown);
    let cases = [
        Case {
            name: "a panic in resuming the yielded finish",
            panics: &[PanicSite::Resume],
            control_panics: false,
            fails: false,
            work: vec![
                finish.clone(),
                Call::Resume,
                reset(PanicSite::Resume),
                finish.clone(),
                Call::Resume,
            ],
            delivered: expected(&[&["finish.0"], &["finish.1"], &["finish.2"]]),
        },
        Case {
            name: "a panic in a control message while the finish is yielded",
            panics: &[],
            control_panics: true,
            fails: false,
            work: vec![
                finish.clone(),
                Call::Resume,
                Call::Control("panic"),
                reset(PanicSite::Control),
                finish.clone(),
            ],
            delivered: expected(&[&["finish.0"], &["finish.1"], &["finish.2"]]),
        },
        Case {
            name: "a panic in finishing, and another in resuming the finish called again",
            panics: &[PanicSite::Finish, PanicSite::Resume],
            control_panics: false,
            fails: false,
            work: vec![
                finish.clone(),
                reset(PanicSite::Finish),
                finish.clone(),
                Call::Resume,
                reset(PanicSite::Resume),
                finish.clone(),
                Call::Resume,
            ],
            delivered: expected(&[&["finish.0"], &["finish.1"], &["finish.2"]]),
        },
        Case {
            name: "a panic that fails the region",
            panics: &[PanicSite::Resume],
            control_panics: false,
            fails: true,
            work: vec![finish.clone(), Call::Resume, reset(PanicSite::Resume)],
            delivered: expected(&[&["finish.0"]]),
        },
    ];

    for case in cases {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let log = Log::default();
        let (control, mailbox) = control_mailbox();
        let (out, mut out_rx) = downstream(1);
        let mut driver = ScriptedDriver::new(&log).finishing_with(3).keeping_its_drain_on_panic();
        for site in case.panics {
            driver = driver.panicking_in(*site);
        }
        if case.fails {
            driver = driver.failing_on_panic();
        }
        let setup = Setup::new(&["out"]).connect(0, &out).events_per_turn(1);
        let mut region = start(setup, mailbox, driver).await;

        // One event per turn, and room for one buffer downstream: once the input ends, `finish.0` is delivered,
        // `finish.1` waits to be sent, and the finish is yielded with `finish.2` left.
        region.close_input();
        if case.control_panics {
            settle().await;
            assert_eq!(log.work(), vec![finish.clone(), Call::Resume], "{}", case.name);
            let ack = acknowledged(control.send(Command::Panic)).await;
            assert!(
                matches!(&ack, Err(ControlError::Panicked { .. })),
                "{}: unexpected acknowledgment {:?}",
                case.name,
                ack
            );
        }

        // What the panics did not lose is delivered: the finish called again resumes the drain the panic ended.
        assert_eq!(
            receive_names(&mut out_rx, case.delivered.len()).await,
            case.delivered,
            "{}",
            case.name
        );
        let stopped = region.stopped().await;
        if case.fails {
            let error = stopped.expect_err("the region should fail");
            assert_eq!(
                format!("{:#}", error),
                "Region panicked in resume: boom in resume: The region has no panic budget.",
                "{}",
                case.name
            );
        } else {
            stopped.expect("the region should recover and stop cleanly");
        }
        assert!(waiting(&mut out_rx).is_empty(), "{}", case.name);
        assert_eq!(log.work(), case.work, "{}", case.name);

        let resets = case
            .work
            .iter()
            .filter(|call| matches!(call, Call::Reset { .. }))
            .count() as u64;
        assert_eq!(lost_events(&recorder), Some(resets), "{}", case.name);
        assert_eq!(panics(&recorder), Some(resets), "{}", case.name);
    }
}

#[tokio::test]
async fn a_long_drain_lets_other_tasks_on_its_thread_run_between_its_turns() {
    // The test runtime runs every task on one thread, so the counter only advances while the region yields.
    let counter = Arc::new(AtomicUsize::new(0));
    let ticker = tokio::spawn({
        let counter = Arc::clone(&counter);
        async move {
            loop {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
            }
        }
    });

    let observed = Arc::new(Mutex::new(Vec::new()));
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, _out_rx) = downstream(16);
    let driver = ScriptedDriver::new(&log).spinning(2_000, &counter, &observed);
    let mut region = start(Setup::new(&["out"]).connect(0, &out), mailbox, driver).await;
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    ticker.abort();

    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 2_000);
    assert!(
        observed.last() > observed.first(),
        "other tasks must run during the drain, but the counter stayed at {:?}",
        observed.first()
    );
}

#[tokio::test(start_paused = true)]
async fn telemetry_is_published_at_most_once_a_second_after_every_control_message_and_at_the_end() {
    let started = Instant::now();
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, _out_rx) = downstream(16);
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out),
        mailbox,
        ScriptedDriver::new(&log),
    )
    .await;

    region.send(batch(&["a"]));
    sleep_until(started + Duration::from_millis(900)).await;
    region.send(batch(&["b"]));
    region.send(batch(&["c"]));
    settle().await;
    assert_eq!(
        log.publishes(),
        0,
        "work within the first second is published at the end of it"
    );

    sleep_until(started + Duration::from_millis(1_100)).await;
    assert_eq!(log.publishes(), 1);

    region.send(batch(&["d"]));
    sleep_until(started + Duration::from_millis(1_500)).await;
    assert_eq!(
        log.publishes(),
        1,
        "the next periodic publish is a second after the last one"
    );

    // A control message publishes right away, which leaves nothing to publish periodically.
    acknowledged(control.send(Command::Note("note"))).await.unwrap();
    assert_eq!(log.publishes(), 2);
    sleep_until(started + Duration::from_secs(5)).await;
    assert_eq!(log.publishes(), 2);

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    assert_eq!(
        log.calls(),
        vec![
            process(&["a"]),
            process(&["b"]),
            process(&["c"]),
            Call::Publish,
            process(&["d"]),
            Call::Control("note"),
            Call::Publish,
            Call::Finish(DrainPolicy::Shutdown),
            Call::Publish,
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn at_most_work_per_turn_batches_run_between_checks_for_control_messages() {
    // The first batch makes the driver send a control message to its own region, which is then waiting while the
    // host processes the batches already waiting on its input.
    let cases: [(usize, Vec<Call>); 2] = [
        (
            1,
            vec![
                process(&["poke"]),
                Call::Control("poke"),
                Call::Publish,
                process(&["b"]),
                process(&["c"]),
                process(&["d"]),
            ],
        ),
        (
            3,
            vec![
                process(&["poke"]),
                process(&["b"]),
                process(&["c"]),
                Call::Control("poke"),
                Call::Publish,
                process(&["d"]),
            ],
        ),
    ];

    for (work_per_turn, expected_calls) in cases {
        let log = Log::default();
        let (control, mailbox) = control_mailbox();
        let (out, _out_rx) = downstream(16);
        let setup = Setup::new(&["out"]).connect(0, &out).work_per_turn(work_per_turn);
        let mut region = start(setup, mailbox, ScriptedDriver::new(&log).poking(&control)).await;

        for name in ["poke", "b", "c", "d"] {
            region.send(batch(&[name]));
        }
        settle().await;
        assert_eq!(log.calls(), expected_calls, "work per turn {}", work_per_turn);

        region.close_input();
        region.stopped().await.expect("the region should stop cleanly");
    }
}

#[tokio::test(start_paused = true)]
async fn control_messages_are_acknowledged_with_their_outcome() {
    let log = Log::default();
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out),
        mailbox,
        ScriptedDriver::new(&log),
    )
    .await;

    acknowledged(control.send(Command::Note("applied"))).await.unwrap();
    match acknowledged(control.send(Command::Reject)).await {
        Err(ControlError::Rejected { error, .. }) => {
            assert_eq!(error.to_string(), "Generation 7 is not prepared.");
        }
        other => panic!("expected a rejection, got {:?}", other),
    }

    // A rejected message leaves the region running.
    region.send(batch(&["a"]));
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["a"]]));

    // Messages the region never handles, because it stopped first, are acknowledged as stopped.
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    assert!(control.is_closed());
    let error = acknowledged(control.send(Command::Note("late")))
        .await
        .expect_err("a stopped region applies nothing");
    assert!(matches!(error, ControlError::Stopped { .. }));
    assert_eq!(
        error.to_string(),
        "The region stopped before it handled the control message. The region is shutting down, or it failed; see its \
         logs for the reason."
    );
}

#[test]
fn the_builder_declares_a_named_metrics_output_per_exit() {
    let (_control, mailbox) = control_mailbox();
    let builder = RegionHostBuilder::from_driver(
        ["dsd_agg", "metrics_out", "unused"],
        NonZeroUsize::MIN,
        mailbox,
        ScriptedDriver::new(&Log::default()),
    )
    .unwrap();

    assert_eq!(builder.input_event_type(), EventType::Metric);
    let outputs = builder
        .outputs()
        .iter()
        .map(|output| (output.output_name(), output.data_ty()))
        .collect::<Vec<_>>();
    assert_eq!(
        outputs,
        vec![
            (Some("dsd_agg"), EventType::Metric),
            (Some("metrics_out"), EventType::Metric),
            (Some("unused"), EventType::Metric),
        ]
    );
}

#[test]
fn the_builder_rejects_exit_names_that_are_not_distinct_output_names() {
    let cases: [(&[&str], &str); 4] = [
        (
            &["ok", "not ok"],
            "Region exit name 'not ok' is invalid. Exit names become output names, which follow the rules of \
             component IDs: component IDs may only contain alphanumerics (a-z, A-Z, or 0-9) and underscores, and \
             must start and end with an alphanumeric character.",
        ),
        (
            &["_leading"],
            "Region exit name '_leading' is invalid. Exit names become output names, which follow the rules of \
             component IDs: component IDs may only contain alphanumerics (a-z, A-Z, or 0-9) and underscores, and \
             must start and end with an alphanumeric character.",
        ),
        (
            &["dotted.name"],
            "Region exit name 'dotted.name' is invalid. Exit names become output names, which follow the rules of \
             component IDs: component IDs may only contain alphanumerics (a-z, A-Z, or 0-9) and underscores, and \
             must start and end with an alphanumeric character.",
        ),
        (
            &["a", "b", "a"],
            "Region exit 'a' is listed more than once. Give each exit a distinct name.",
        ),
    ];

    for (exits, message) in cases {
        let (_control, mailbox) = control_mailbox();
        let result = RegionHostBuilder::from_driver(
            exits.iter().copied(),
            NonZeroUsize::MIN,
            mailbox,
            ScriptedDriver::new(&Log::default()),
        );
        match result {
            Ok(_) => panic!("exits {:?} should be rejected", exits),
            Err(error) => assert_eq!(error.to_string(), message),
        }
    }
}

#[test]
fn the_builder_accepts_at_most_65536_exits() {
    let exits = |count: usize| (0..count).map(|exit| format!("e{}", exit)).collect::<Vec<_>>();
    // (exits, the error, if the builder rejects them)
    let cases = [
        (65_536, None),
        (
            65_537,
            Some(
                "A region has 65537 exits, but at most 65536 are supported. Split the region's program across more \
                 regions.",
            ),
        ),
    ];
    for (count, error) in cases {
        let (_control, mailbox) = control_mailbox();
        let result = RegionHostBuilder::from_driver(
            exits(count),
            NonZeroUsize::MIN,
            mailbox,
            ScriptedDriver::new(&Log::default()),
        );
        match (result, error) {
            (Ok(builder), None) => assert_eq!(builder.outputs().len(), count),
            (Err(actual), Some(expected)) => assert_eq!(actual.to_string(), expected),
            (Ok(_), Some(_)) => panic!("{} exits should be rejected", count),
            (Err(error), None) => panic!("{} exits should be accepted: {}", count, error),
        }
    }
}

#[tokio::test]
async fn a_region_reports_running_once_its_host_runs_and_never_when_it_is_only_built() {
    // A region that is built and never run, as when a topology fails before it starts its components: the mailbox's
    // controller is never told it runs.
    let (control, mailbox) = control_mailbox::<Command>();
    let builder = RegionHostBuilder::from_driver(
        ["out"],
        NonZeroUsize::MIN,
        mailbox,
        ScriptedDriver::new(&Log::default()),
    )
    .unwrap();
    let transform = builder.build(BuildContext::test_transform(REGION_ID)).await.unwrap();
    assert!(!control.is_running());
    assert!(
        timeout(Duration::from_millis(50), control.running()).await.is_err(),
        "a built region is not running"
    );
    drop(transform);
    drop(builder);
    assert!(!control.running().await, "a region dropped without running never runs");

    // A region whose host runs.
    let (control, mailbox) = control_mailbox();
    let (out, _out_rx) = downstream(16);
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out),
        mailbox,
        ScriptedDriver::new(&Log::default()),
    )
    .await;
    assert!(timeout(PATIENCE, control.running()).await.expect("the region runs"));
    assert!(control.is_running());
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    // A region that ran and stopped still reports that it ran.
    assert!(control.running().await);
}

#[tokio::test]
async fn a_builder_builds_one_transform_since_it_hands_over_its_driver_and_its_mailbox() {
    // A builder made from a driver instance can hand it over once.
    let (_control, mailbox) = control_mailbox();
    let builder = RegionHostBuilder::from_driver(
        ["out"],
        NonZeroUsize::MIN,
        mailbox,
        ScriptedDriver::new(&Log::default()),
    )
    .unwrap();
    assert!(builder.build(BuildContext::test_transform(REGION_ID)).await.is_ok());
    let error = match builder.build(BuildContext::test_transform(REGION_ID)).await {
        Ok(_) => panic!("a driver instance builds one transform"),
        Err(error) => error,
    };
    assert_eq!(
        format!("{:#}", error),
        "Failed to build the driver of region 'region': The region driver was already used to build a transform. A \
         region host builder created from a driver builds one transform; create it with a driver factory to build \
         several."
    );

    // A factory builds drivers on demand, with the region's build context, but the mailbox can still be used once.
    let (_control, mailbox) = control_mailbox();
    let built_for = Arc::new(Mutex::new(Vec::new()));
    let factory_log = Arc::clone(&built_for);
    let builder = RegionHostBuilder::new(["out"], NonZeroUsize::MIN, mailbox, move |context: &BuildContext| {
        factory_log.lock().unwrap().push(context.component_id().to_string());
        Ok(ScriptedDriver::new(&Log::default()))
    })
    .unwrap();
    assert!(builder.build(BuildContext::test_transform(REGION_ID)).await.is_ok());
    let error = match builder.build(BuildContext::test_transform(REGION_ID)).await {
        Ok(_) => panic!("a mailbox builds one transform"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "The control mailbox of region 'region' was already used to build a transform. A region host builder builds \
         one transform, since each region has its own mailbox."
    );
    assert_eq!(
        *built_for.lock().unwrap(),
        vec!["region".to_string(), "region".to_string()]
    );

    // A factory's error fails the build, naming the region.
    let (_control, mailbox) = control_mailbox();
    let builder = RegionHostBuilder::<ScriptedDriver>::new(["out"], NonZeroUsize::MIN, mailbox, |_: &BuildContext| {
        Err(generic_error!("The boot program is not valid."))
    })
    .unwrap();
    let error = match builder.build(BuildContext::test_transform(REGION_ID)).await {
        Ok(_) => panic!("the factory fails"),
        Err(error) => error,
    };
    assert_eq!(
        format!("{:#}", error),
        "Failed to build the driver of region 'region': The boot program is not valid."
    );
}

/// Memory bounds standing in for the kernels of a driver.
struct KernelBounds;

impl MemoryBounds for KernelBounds {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_fixed_amount("kernel structs", 100);
        builder.firm().with_fixed_amount("kernel caches", 1_000);
    }
}

#[test]
fn memory_bounds_add_a_full_buffer_per_exit_to_the_drivers_bounds() {
    let (_control, mailbox) = control_mailbox();
    let builder = RegionHostBuilder::from_driver(
        ["a", "b", "c"],
        NonZeroUsize::MIN,
        mailbox,
        ScriptedDriver::new(&Log::default()),
    )
    .unwrap()
    .with_bounds(KernelBounds);

    let mut bounds_builder = MemoryBoundsBuilder::for_test();
    builder.specify_bounds(&mut bounds_builder);
    let bounds = bounds_builder.as_bounds();

    let minimum = std::mem::size_of::<RegionHost<ScriptedDriver>>() + 100;
    let full_exit_buffer =
        std::mem::size_of::<(ExitId, Option<u16>, EventsBuffer)>() + 1024 * std::mem::size_of::<Event>();
    assert_eq!(bounds.total_minimum_required_bytes(), minimum);
    assert_eq!(bounds.total_firm_limit_bytes(), minimum + 3 * full_exit_buffer + 1_000);

    // With lanes, every lane holds a full buffer per exit, and the driver's bounds cover every lane.
    let (_control, mailbox) = control_mailbox();
    let builder = RegionHostBuilder::new(["a", "b", "c"], NonZeroUsize::MIN, mailbox, |_: &BuildContext| {
        Ok(ScriptedDriver::new(&Log::default()))
    })
    .unwrap()
    .with_lanes(NonZeroU16::new(4).unwrap(), |_: &BuildContext, _| {
        Ok(ScriptedDriver::new(&Log::default()))
    })
    .with_bounds(KernelBounds);
    let mut bounds_builder = MemoryBoundsBuilder::for_test();
    builder.specify_bounds(&mut bounds_builder);
    let bounds = bounds_builder.as_bounds();
    let minimum = minimum + 4 * std::mem::size_of::<Lane<ScriptedDriver>>();
    assert_eq!(bounds.total_minimum_required_bytes(), minimum);
    assert_eq!(
        bounds.total_firm_limit_bytes(),
        minimum + 4 * 3 * full_exit_buffer + 1_000
    );
}

#[test]
fn exit_sinks_keep_buffers_in_the_order_they_were_written() {
    let mut sink = ExitSink::new(2);
    assert_eq!(sink.exit_count(), 2);
    assert!(sink.is_empty());

    sink.emit(ExitId::new(1), batch(&["x", "y"]));
    sink.emit(ExitId::new(0), EventsBuffer::with_capacity(0));
    sink.emit(ExitId::new(1), batch(&["z"]));
    assert_eq!(sink.len(), 3);
    assert_eq!(sink.event_count(), 3);

    let drained = sink
        .drain()
        .map(|(exit, buffer)| (exit.get(), names(&buffer)))
        .collect::<Vec<_>>();
    assert_eq!(
        drained,
        vec![
            (1, vec!["x".to_string(), "y".to_string()]),
            (0, vec![]),
            (1, vec!["z".to_string()])
        ]
    );
    assert!(sink.is_empty());

    sink.emit(ExitId::new(0), batch(&["w"]));
    sink.clear();
    assert!(sink.is_empty());
}

#[test]
fn exit_sinks_keep_the_lane_a_buffer_was_written_to() {
    let mut sink = ExitSink::new(2);
    sink.emit_to_lane(ExitId::new(1), 3, batch(&["x"]));
    sink.emit(ExitId::new(0), batch(&["y"]));
    sink.emit_to_lane(ExitId::new(1), 0, batch(&["z"]));
    let drained = sink
        .drain_routed()
        .map(|(exit, lane, buffer)| (exit.get(), lane, names(&buffer)))
        .collect::<Vec<_>>();
    assert_eq!(
        drained,
        vec![
            (1, Some(3), vec!["x".to_string()]),
            (0, None, vec!["y".to_string()]),
            (1, Some(0), vec!["z".to_string()])
        ]
    );

    // Draining without lanes keeps the order and drops the lanes.
    sink.emit_to_lane(ExitId::new(1), 1, batch(&["w"]));
    assert_eq!(sink.drain().map(|(exit, _)| exit.get()).collect::<Vec<_>>(), vec![1]);
}

#[tokio::test]
async fn a_region_with_lanes_builds_one_driver_per_lane_and_one_owned_point_per_lane() {
    let lanes_seen = Arc::new(Mutex::new(Vec::new()));
    let builder = |lanes: u16| {
        let (_control, mailbox) = control_mailbox();
        let lanes_seen = Arc::clone(&lanes_seen);
        RegionHostBuilder::new(["out", "spread"], NonZeroUsize::MIN, mailbox, |_: &BuildContext| {
            Ok(ScriptedDriver::new(&Log::default()))
        })
        .unwrap()
        .with_lanes(
            NonZeroU16::new(lanes).unwrap(),
            move |_: &BuildContext, lane: LaneContext| {
                lanes_seen.lock().unwrap().push(lane);
                Ok(ScriptedDriver::new(&Log::default()))
            },
        )
    };
    let context = BuildContext::test_transform(REGION_ID);

    // One lane is the host without lanes, its driver built for the only lane.
    let one = builder(1);
    assert_eq!(one.input_lanes().get(), 1);
    assert!(matches!(one.build_host(&context), Ok(BuiltHost::One(_))));
    assert_eq!(std::mem::take(&mut *lanes_seen.lock().unwrap()), vec![LaneContext::ONE]);

    // Two lanes: a driver for each lane, and an owned point for each, registered for as long as the region lives.
    let schedule = steady(1);
    let registry = AdmissionRegistry::new();
    let two = builder(2)
        .with_lane_admission(vec![
            owned_point(3, &schedule, &registry),
            owned_point(4, &schedule, &registry),
        ])
        .with_lane_stats(vec![Arc::new(LaneStats::new()), Arc::new(LaneStats::new())]);
    assert_eq!((two.lanes().get(), two.input_lanes().get()), (2, 2));
    // Its input holds buffers of an events buffer's capacity, unless it declares smaller ones, as a state owner's lanes
    // fed by routed buffers do.
    assert_eq!(two.input_buffer_events(), DEFAULT_EVENTS_BUFFER_CAPACITY);
    let two = two.with_input_buffer_events(512);
    assert_eq!(two.input_buffer_events(), 512);
    let host = two.build_host(&context);
    assert!(matches!(host, Ok(BuiltHost::Laned(_))));
    assert_eq!(
        std::mem::take(&mut *lanes_seen.lock().unwrap()),
        vec![LaneContext { index: 0, count: 2 }, LaneContext { index: 1, count: 2 }]
    );
    assert_eq!(registry.ids(), vec![AdmissionPointId::new(3), AdmissionPointId::new(4)]);
    drop(host);
    assert!(registry.ids().is_empty());

    let refused = |builder: RegionHostBuilder<ScriptedDriver>| match builder.build_host(&context) {
        Ok(_) => panic!("the build should be refused"),
        Err(error) => format!("{:#}", error),
    };
    let error = refused(builder(1).with_lane_admission(vec![
        owned_point(5, &schedule, &registry),
        owned_point(6, &schedule, &registry),
    ]));
    assert!(
        error.contains("runs on 1 lanes, but was given 2 lane admission points"),
        "{}",
        error
    );
    let shared = AdmissionPointConfig::new(
        AdmissionPointId::new(7),
        AdmissionMode::Admit,
        Arc::clone(&schedule),
        THRESHOLD,
    );
    let error = refused(builder(2).with_lane_admission(vec![shared, owned_point(8, &schedule, &registry)]));
    assert!(
        error.starts_with(
            "Failed to set up the admission point of lane 0 of 2 of region 'region': Admission point 7 is stamped by \
             its dispatcher"
        ),
        "{}",
        error
    );
    let error = refused(builder(2).with_lane_stats(vec![Arc::new(LaneStats::new())]));
    assert_eq!(
        error,
        "Region 'region' runs on 2 lanes, but was given 1 lane statistics. Give every lane one."
    );

    // A lane's driver that fails to build fails the region's build, naming the lane.
    let (_control, mailbox) = control_mailbox();
    let failing = RegionHostBuilder::new(["out"], NonZeroUsize::MIN, mailbox, |_: &BuildContext| {
        Ok(ScriptedDriver::new(&Log::default()))
    })
    .unwrap()
    .with_lanes(NonZeroU16::new(2).unwrap(), |_: &BuildContext, lane: LaneContext| {
        if lane.index == 1 {
            Err(generic_error!("Lane 1 has no program."))
        } else {
            Ok(ScriptedDriver::new(&Log::default()))
        }
    });
    assert_eq!(
        refused(failing),
        "Failed to build the driver of lane 1 of 2 of region 'region': Lane 1 has no program."
    );
}

#[test]
fn a_buffer_goes_where_the_edge_of_its_exits_output_routes_it_and_one_that_does_not_fit_is_refused() {
    let exits = ["keyed", "affine", "spread", "whole", "fixed"];
    let mut dispatcher = Dispatcher::<EventsBuffer>::new(ComponentContext::test_transform(REGION_ID));
    for exit in exits {
        dispatcher.add_output(OutputName::Given(exit.into())).unwrap();
    }
    let lanes = |count| (0..count).map(|_| mpsc::channel(1).0).collect::<Vec<_>>();
    for (exit, count, edge) in [
        ("keyed", 3, LaneEdge::Keyed),
        ("affine", 2, LaneEdge::Affinity),
        ("spread", 4, LaneEdge::Spread),
        ("whole", 1, LaneEdge::Keyed),
        ("fixed", 2, LaneEdge::Fixed(1)),
    ] {
        dispatcher
            .attach_component_lanes_to_output(
                &OutputName::Given(exit.into()),
                ComponentId::try_from(format!("to_{}", exit).as_str()).unwrap(),
                lanes(count),
                edge,
            )
            .unwrap();
    }
    let names: Arc<[String]> = exits.iter().map(|exit| exit.to_string()).collect::<Vec<_>>().into();
    let exit = |name: &str| ExitId::new(exits.iter().position(|exit| *exit == name).unwrap() as u16);
    let mut routes = ExitRoutes::new(Arc::clone(&names), LaneContext { index: 1, count: 3 });
    routes.resolve(&dispatcher);
    let route = |routes: &mut ExitRoutes, name: &str, lane: Option<u16>| {
        routes
            .route(exit(name), lane)
            .map(|(name, route)| (name.to_string(), format!("{:?}", route)))
            .map_err(|error| error.to_string())
    };
    let routed = |name: &str, route: &str| Ok((name.to_string(), route.to_string()));

    // A keyed exit's buffers go to the lane the driver named; an affinity exit's to the sending lane's; a spread exit's
    // from a rotation of the sending lane's own, which starts at its index and keeps where the dispatcher left it.
    assert!(routes.is_keyed(exit("keyed")) && !routes.is_keyed(exit("affine")));
    assert_eq!(route(&mut routes, "keyed", Some(2)), routed("keyed", "Some(Target(2))"));
    assert_eq!(route(&mut routes, "affine", None), routed("affine", "Some(From(1))"));
    match routes.route(exit("spread"), None) {
        Ok(("spread", Some(LaneRoute::Spread(counter)))) => {
            assert_eq!(*counter, 1);
            *counter = 3;
        }
        other => panic!("a spread exit's route should be its counter, got {:?}", other),
    }
    assert_eq!(route(&mut routes, "spread", None), routed("spread", "Some(Spread(3))"));

    // An exit whose output routes to no component's lanes sends as on any output, whatever lane the driver names.
    for name in ["whole", "fixed"] {
        assert_eq!(route(&mut routes, name, None), routed(name, "None"));
        assert_eq!(route(&mut routes, name, Some(5)), routed(name, "None"));
    }

    // A buffer that does not fit its exit's routing is refused.
    assert!(route(&mut routes, "keyed", None)
        .unwrap_err()
        .starts_with("Exit 'keyed' routes to the lanes of its target by key, but the region wrote a buffer to it"));
    assert!(route(&mut routes, "keyed", Some(3))
        .unwrap_err()
        .starts_with("The region wrote a buffer to lane 3 of the target of exit 'keyed', which has 3 lanes."));
    for name in ["affine", "spread"] {
        assert!(route(&mut routes, name, Some(0))
            .unwrap_err()
            .contains("which picks the lane itself"));
    }

    // Before the routes are read from a dispatcher, every exit sends whole.
    let mut unresolved = ExitRoutes::new(names, LaneContext::ONE);
    assert_eq!(route(&mut unresolved, "keyed", None), routed("keyed", "None"));
}

const THRESHOLD: Duration = Duration::from_secs(300);

fn secs(secs: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(secs)
}

/// A schedule that assigns every batch the generation `generation`.
fn steady(generation: u64) -> ScheduleHandle {
    AdmissionSchedule::Steady(Arc::new(GenerationToken::new(GenerationId::new(generation)))).into_handle()
}

/// The configuration of the owned admission point `id`, registered in `registry`.
fn owned_point(id: u32, schedule: &ScheduleHandle, registry: &AdmissionRegistry) -> AdmissionPointConfig {
    AdmissionPointConfig::new(
        AdmissionPointId::new(id),
        AdmissionMode::Admit,
        Arc::clone(schedule),
        THRESHOLD,
    )
    .with_registry(registry.clone())
    .owned()
}

/// Returns the stamp of a buffer's header: its admission point, admission time, epoch, and generation.
fn stamp_of(buffer: &EventsBuffer) -> (Option<u32>, Option<AdmissionTime>, u32, Option<u64>) {
    let header = buffer.header();
    (
        header.admission_point.map(AdmissionPointId::get),
        header.admitted_at,
        header.epoch,
        header.generation_id().map(GenerationId::get),
    )
}

#[tokio::test(start_paused = true)]
async fn a_region_stamps_every_unstamped_batch_at_its_owned_point_before_its_driver_sees_it() {
    let log = Log::default();
    let schedule = steady(1);
    let registry = AdmissionRegistry::new();
    let wall = ManualWallClock::new(secs(100));
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let setup = Setup::new(&["out"])
        .connect(0, &out)
        .admission(vec![owned_point(3, &schedule, &registry).with_wall_clock(wall.clone())]);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;

    region.send(batch(&["a"]));
    let mut stamped = batch(&["b"]);
    stamped.set_header(stamped_header(9));
    region.send(stamped);
    assert_eq!(
        stamp_of(&receive(&mut out_rx).await),
        (Some(3), Some(secs(100)), 0, Some(1))
    );
    assert_eq!(
        stamp_of(&receive(&mut out_rx).await),
        (Some(1), Some(secs(100)), 0, Some(9)),
        "an admitting point keeps a stamp it finds"
    );
    let status = registry
        .status_within(AdmissionPointId::new(3), Duration::from_secs(1))
        .await
        .expect("the point is registered");
    assert_eq!(status.last_admitted_at, Some(secs(100)));

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn an_idle_region_heartbeats_at_its_owned_point_one_period_after_the_points_latest_stamp() {
    let log = Log::default();
    let schedule = steady(1);
    let wall = ManualWallClock::new(secs(100));
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let point = owned_point(3, &schedule, &AdmissionRegistry::new())
        .with_wall_clock(wall.clone())
        .with_heartbeat(Duration::from_secs(1));
    let started = Instant::now();
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out).admission(vec![point]),
        mailbox,
        ScriptedDriver::new(&log),
    )
    .await;

    // Data at 0.5 s keeps the point busy until 1.5 s: nothing is sent at 1 s, one period after the point was built.
    sleep_until(started + Duration::from_millis(500)).await;
    region.send(batch(&["a"]));
    assert_eq!(
        stamp_of(&receive(&mut out_rx).await),
        (Some(3), Some(secs(100)), 0, Some(1))
    );
    sleep_until(started + Duration::from_millis(1_400)).await;
    assert!(waiting(&mut out_rx).is_empty(), "no heartbeat is due before 1.5 s");

    // At 1.5 s the point has been idle for a period, and again at 2.5 s: each heartbeat is header-only, stamped as
    // data would be, and processed by the driver, which passes it on.
    wall.set(secs(101));
    sleep_until(started + Duration::from_millis(1_600)).await;
    let beats = waiting(&mut out_rx);
    assert_eq!(beats.len(), 1);
    assert!(beats[0].is_empty());
    assert_eq!(stamp_of(&beats[0]), (Some(3), Some(secs(101)), 0, Some(1)));
    wall.set(secs(102));
    sleep_until(started + Duration::from_millis(2_600)).await;
    let beats = waiting(&mut out_rx);
    assert_eq!(beats.len(), 1);
    assert_eq!(stamp_of(&beats[0]), (Some(3), Some(secs(102)), 0, Some(1)));
    assert_eq!(log.work(), vec![process(&["a"]), process(&[]), process(&[])]);

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_reset_another_point_of_the_source_detects_reaches_an_idle_region_at_once_and_a_busy_one_after_its_batch() {
    let log = Log::default();
    let schedule = steady(1);
    let registry = AdmissionRegistry::new();
    let wall = ManualWallClock::new(secs(7_203));
    let clock = SourceClock::new(AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone())));
    let neighbor =
        OwnedAdmission::new(owned_point(2, &schedule, &registry).with_source_clock(Arc::clone(&clock))).unwrap();

    // While the region processes its first batch, another point of the source meets a jump back of an hour.
    let reset_once = {
        let (clock, wall, done) = (Arc::clone(&clock), wall.clone(), Arc::new(AtomicBool::new(false)));
        move || {
            if !done.swap(true, Ordering::SeqCst) {
                wall.set(secs(3_603));
                assert!(clock.stamp().reset);
            }
        }
    };
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let setup = Setup::new(&["out"]).connect(0, &out).admission(vec![
        owned_point(3, &schedule, &registry).with_source_clock(Arc::clone(&clock))
    ]);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log).calling_on_process(reset_once)).await;

    // The batch was stamped before the reset; right after it, the region stamps a heartbeat in the new epoch.
    region.send(batch(&["a"]));
    let data = receive(&mut out_rx).await;
    assert_eq!(stamp_of(&data), (Some(3), Some(secs(7_203)), 0, Some(1)));
    let beat = receive(&mut out_rx).await;
    assert!(beat.is_empty());
    assert_eq!(stamp_of(&beat), (Some(3), Some(secs(3_603)), 1, Some(1)));

    // While the region waits for input, the other point meets another jump: the region stamps a heartbeat at once,
    // not at its next timer.
    settle().await;
    wall.set(secs(10));
    neighbor.stamp(&mut BatchHeader::default()).await;
    assert_eq!(clock.epoch(), 2);
    settle().await;
    let beats = waiting(&mut out_rx);
    assert_eq!(beats.len(), 1, "one heartbeat per reset, at once");
    assert!(beats[0].is_empty());
    assert_eq!(stamp_of(&beats[0]), (Some(3), Some(secs(10)), 2, Some(1)));

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn output_a_driver_holds_is_sent_when_its_hold_expires_while_the_input_is_idle() {
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let mut region = start(
        Setup::new(&["out"]).connect(0, &out),
        mailbox,
        ScriptedDriver::new(&log).holding(Duration::from_millis(5)),
    )
    .await;

    let started = Instant::now();
    region.send(batch(&["held.a", "b"]));
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["b"]]));
    sleep_until(started + Duration::from_millis(4)).await;
    assert!(waiting(&mut out_rx).is_empty(), "the hold has not expired");
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["held.a"]]));
    assert_eq!(started.elapsed(), Duration::from_millis(5));

    // The host ended the turn after the batch, its input idle, and again when the hold expired.
    assert_eq!(log.turn_ends(), vec![(1, true), (1, true)]);
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn the_host_ends_every_turn_once_no_call_is_yielded_and_tells_whether_more_input_waits() {
    let log = Log::default();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let setup = Setup::new(&["out"])
        .connect(0, &out)
        .work_per_turn(2)
        .events_per_turn(1);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;

    // Three batches wait before the host first reads its input. The first batch's call yields, which ends the first
    // turn with two batches waiting; it ends once the call completed. The second turn takes the last two batches.
    region.send(batch(&["a", "b"]));
    region.send(batch(&["c"]));
    region.send(batch(&["d"]));
    assert_eq!(
        receive_names(&mut out_rx, 4).await,
        expected(&[&["a"], &["b"], &["c"], &["d"]])
    );
    settle().await;
    assert_eq!(
        log.work(),
        vec![process(&["a", "b"]), Call::Resume, process(&["c"]), process(&["d"])]
    );
    assert_eq!(log.turn_ends(), vec![(2, false), (4, true)]);

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_region_with_lanes_runs_every_lane_on_its_own_input_and_stops_once_every_lane_finished() {
    let logs = [Log::default(), Log::default()];
    let (control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let driver_logs = logs.clone();
    let mut region = start_laned(Setup::new(&["out"]).connect(0, &out), mailbox, 2, move |lane| {
        ScriptedDriver::new(&driver_logs[usize::from(lane.index)])
    })
    .await;
    assert!(timeout(PATIENCE, control.running()).await.expect("the region runs"));

    region.send(0, batch(&["a"]));
    region.send(1, batch(&["b"]));
    region.send(0, batch(&["c"]));
    let received = receive_names(&mut out_rx, 3).await;
    let position = |name: &str| received.iter().position(|names| names == &[name.to_string()]);
    assert!(position("b").is_some());
    assert!(
        position("a") < position("c"),
        "each lane sends in order: {:?}",
        received
    );

    // Lane 0 finishes when its input ends, and the region runs until every lane finished.
    region.close_input(0);
    settle().await;
    assert_eq!(
        logs[0].work(),
        vec![process(&["a"]), process(&["c"]), Call::Finish(DrainPolicy::Shutdown)]
    );
    assert!(!region.task.is_finished());
    region.send(1, batch(&["d"]));
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["d"]]));
    region.close_input(1);
    region.stopped().await.expect("the region should stop cleanly");
    assert_eq!(
        logs[1].work(),
        vec![process(&["b"]), process(&["d"]), Call::Finish(DrainPolicy::Shutdown)]
    );
}

#[tokio::test(start_paused = true)]
async fn control_messages_split_per_lane_and_are_acknowledged_once_every_lane_accepted_or_with_the_first_rejection() {
    let logs = [Log::default(), Log::default()];
    let (control, mailbox) = control_mailbox();
    let driver_logs = logs.clone();
    let mut region = start_laned(Setup::new(&["out"]), mailbox, 2, move |lane| {
        ScriptedDriver::new(&driver_logs[usize::from(lane.index)])
    })
    .await;
    let message = |ack: Result<(), ControlError>| ack.unwrap_err().to_string();

    // A message every lane accepts is acknowledged once; one that a lane rejects, with that lane's rejection.
    acknowledged(control.send(Command::Note("both"))).await.unwrap();
    assert_eq!(
        message(acknowledged(control.send(Command::RejectIn(1))).await),
        "The region could not apply the control message: Lane 1 of 2 rejected it: Generation 7 is not prepared."
    );

    // A message that does not split goes to the lane it names, and one that names no lane, or a lane the region does
    // not have, is rejected.
    acknowledged(control.send(Command::ForLane(1, "lane one")))
        .await
        .unwrap();
    assert_eq!(
        message(acknowledged(control.send(Command::ForLane(2, "lane two"))).await),
        "The region could not apply the control message: A control message is for lane 2, but the region has only 2 \
         lanes. Address one of its lanes."
    );
    assert_eq!(
        message(acknowledged(control.send(Command::Emit("x"))).await),
        "The region could not apply the control message: A control message neither splits into one part per lane \
         of the region's 2 lanes nor names the lane it is for. Send one part per lane, or one message per lane that \
         names it."
    );

    assert_eq!(logs[0].work(), vec![Call::Control("both"), Call::Control("accept")]);
    assert_eq!(
        logs[1].work(),
        vec![
            Call::Control("both"),
            Call::Control("reject"),
            Call::Control("lane one")
        ]
    );
    region.close_inputs();
    region.stopped().await.expect("the region should stop cleanly");
}

/// Sends a control message through a fresh mailbox of a component calling itself `component`, and returns the
/// message's acknowledgment with the reply the component answers it through.
async fn request(component: &'static str) -> (ControlAck, ControlReply) {
    let (sender, mailbox) = control_mailbox::<()>();
    let mut receiver = mailbox.into_receiver_for(component);
    let ack = sender.send(());
    let ((), reply) = receiver.recv().await.expect("the message was sent");
    (ack, reply)
}

/// Links a coordinator to `count` lanes, as a region with lanes does, and returns the links with each lane's end.
fn linked(count: u16) -> (Arc<[LaneLink<()>]>, Vec<LaneEnd<()>>) {
    let (links, ends): (Vec<_>, Vec<_>) = (0..count).map(|index| lane_link(LaneContext { index, count })).unzip();
    (links.into(), ends)
}

/// Sends one part of a control message to every lane of `links`, and returns each part's acknowledgment, with the
/// control queue of each lane and the reply it answers its part through.
async fn parts(
    links: &[LaneLink<()>], ends: Vec<LaneEnd<()>>,
) -> Vec<((usize, ControlAck), (ControlReceiver<()>, ControlReply, LaneFinish))> {
    let mut sent = Vec::new();
    for (index, (link, end)) in links.iter().zip(ends).enumerate() {
        let ack = link.send(());
        let (mut control, _probes, finish) = end.into_parts();
        let ((), reply) = control.recv().await.expect("the part was sent");
        sent.push(((index, ack), (control, reply, finish)));
    }
    sent
}

#[tokio::test(start_paused = true)]
async fn a_joined_acknowledgment_waits_for_every_lane_and_answers_with_the_first_failure() {
    // Both lanes accept: the acknowledgment completes only once the second of them did.
    let (ack, reply) = request("region").await;
    let (links, ends) = linked(2);
    let (acks, mut lanes): (Vec<_>, Vec<_>) = parts(&links, ends).await.into_iter().unzip();
    let joined = tokio::spawn(join_acknowledgments(acks, Arc::clone(&links), reply));
    let (_, reply_1, _) = lanes.pop().unwrap();
    reply_1.ack();
    settle().await;
    assert!(!joined.is_finished(), "lane 0 has not accepted yet");
    let (_, reply_0, _) = lanes.pop().unwrap();
    reply_0.ack();
    assert!(acknowledged(ack).await.is_ok());

    // The first failure answers at once, naming its lane, while the other lane has not answered yet. A lane that
    // stopped without reporting a finish, as one that failed, rejects the message.
    type Answer = fn(ControlReply);
    let failures: [(Answer, &str); 3] = [
        (
            |reply| reply.reject(generic_error!("lane 1 refuses")),
            "The region could not apply the control message: Lane 1 of 2 rejected it: lane 1 refuses",
        ),
        (
            |reply| reply.panicked("boom".to_string()),
            "The region panicked while applying the control message: Lane 1 of 2 panicked: boom",
        ),
        (
            drop,
            "The region could not apply the control message: Lane 1 of 2 stopped before it handled the control \
             message.",
        ),
    ];
    for (answer, expected) in failures {
        let (ack, reply) = request("region").await;
        let (links, ends) = linked(2);
        let (acks, mut lanes): (Vec<_>, Vec<_>) = parts(&links, ends).await.into_iter().unzip();
        tokio::spawn(join_acknowledgments(acks, links, reply));
        let (_, reply_1, _) = lanes.pop().unwrap();
        answer(reply_1);
        assert_eq!(acknowledged(ack).await.unwrap_err().to_string(), expected);
    }

    // A lane that finished before it handled its part is passed over: the others' answers decide, and a message that
    // every lane finished before handling is left unhandled, as by a region that stopped.
    let (ack, reply) = request("region").await;
    let (links, ends) = linked(2);
    let (acks, mut lanes): (Vec<_>, Vec<_>) = parts(&links, ends).await.into_iter().unzip();
    tokio::spawn(join_acknowledgments(acks, links, reply));
    let (control_1, reply_1, finish_1) = lanes.pop().unwrap();
    finish_1.report();
    drop((control_1, reply_1));
    let (_, reply_0, _) = lanes.pop().unwrap();
    reply_0.ack();
    assert!(acknowledged(ack).await.is_ok());

    let (ack, reply) = request("region").await;
    let (links, ends) = linked(2);
    let (acks, lanes): (Vec<_>, Vec<_>) = parts(&links, ends).await.into_iter().unzip();
    tokio::spawn(join_acknowledgments(acks, links, reply));
    for (control, reply, finish) in lanes {
        finish.report();
        drop((control, reply));
    }
    assert!(matches!(acknowledged(ack).await, Err(ControlError::Stopped { .. })));
}

#[tokio::test(start_paused = true)]
async fn a_region_with_lanes_reads_live_while_every_lane_answers_the_probes_it_forwards() {
    let registry = HealthRegistry::new();
    let _probes = registry.spawn_runner_for_test();
    let (_control, mailbox) = control_mailbox();
    let mut region = start_laned(Setup::new(&["out"]).health(&registry), mailbox, 3, |_| {
        ScriptedDriver::new(&Log::default())
    })
    .await;
    sleep(Duration::from_secs(30)).await;
    assert!(registry.is_live_for_test(&SubsystemIdentifier::from_dotted(REGION_ID)));
    region.close_inputs();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_forwarded_probe_is_answered_once_every_lane_answered_or_finished_and_never_if_a_lane_stopped_otherwise() {
    let registry = HealthRegistry::new();
    let _probes = registry.spawn_runner_for_test();
    let id = SubsystemIdentifier::from_dotted(REGION_ID);
    let mut health = registry.register_component(&id).unwrap();

    let probe = timeout(PATIENCE, health.next_probe())
        .await
        .expect("the registry probes");
    let (forwarded, ends) = linked(2);
    let mut probes = ends.into_iter().map(|end| end.into_parts().1).collect::<Vec<_>>();
    let forwarding = tokio::spawn(async move { forward_probe(probe, &forwarded).await });
    probes[0].next().await.respond();
    settle().await;
    assert!(!forwarding.is_finished(), "lane 1 has not answered");
    assert!(!registry.is_live_for_test(&id));
    probes[1].next().await.respond();
    timeout(PATIENCE, forwarding).await.unwrap().unwrap();
    settle().await;
    assert!(registry.is_live_for_test(&id));

    // A lane that finished counts as answering, whether it finished before the probe or while it was forwarded.
    let probe = timeout(PATIENCE, health.next_probe())
        .await
        .expect("the registry probes again");
    let (forwarded, mut ends) = linked(3);
    let (_, mut finishing, finish) = ends.pop().unwrap().into_parts();
    let (_, _, finished) = ends.pop().unwrap().into_parts();
    finished.report();
    let (_control, mut answering, _finish) = ends.pop().unwrap().into_parts();
    let forwarding = tokio::spawn(async move { forward_probe(probe, &forwarded).await });
    answering.next().await.respond();
    let unanswered = finishing.next().await;
    finish.report();
    drop((unanswered, finishing));
    timeout(PATIENCE, forwarding).await.unwrap().unwrap();
    settle().await;
    assert!(registry.is_live_for_test(&id));

    // A lane that stopped without finishing cannot answer: the probe is dropped, and the component reads as not live
    // once it timed out, five seconds after it was sent, before the registry's next probe, a second after an answer,
    // could time out.
    let probe = timeout(PATIENCE, health.next_probe())
        .await
        .expect("the registry probes again");
    let taken = Instant::now();
    let (forwarded, mut ends) = linked(2);
    drop(ends.pop());
    let (_control, mut answering, _finish) = ends.pop().unwrap().into_parts();
    let forwarding = tokio::spawn(async move { forward_probe(probe, &forwarded).await });
    answering.next().await.respond();
    timeout(PATIENCE, forwarding).await.unwrap().unwrap();
    sleep_until(taken + Duration::from_millis(5_500)).await;
    assert!(!registry.is_live_for_test(&id));
}

#[tokio::test(start_paused = true)]
async fn a_lane_that_fails_fails_the_region_and_the_other_lanes_stop_without_finishing() {
    let logs = [Log::default(), Log::default()];
    let (_control, mailbox) = control_mailbox();
    let (out, _out_rx) = downstream(16);
    let driver_logs = logs.clone();
    let region = start_laned(Setup::new(&["out"]).connect(0, &out), mailbox, 2, move |lane| {
        let driver = ScriptedDriver::new(&driver_logs[usize::from(lane.index)]);
        if lane.index == 1 {
            driver.failing_on_panic()
        } else {
            driver
        }
    })
    .await;
    let lane_0 = region.inputs[0].clone().expect("input is open");

    region.send(1, batch(&["panic"]));
    let error = format!("{:#}", region.stopped().await.unwrap_err());
    assert_eq!(
        error,
        "Lane 1 of 2 of region 'region' failed: Region panicked in process: boom in process: The region has no panic \
         budget."
    );

    // Lane 0 stops once its coordinator is gone: its input closes, and it never finishes.
    wait_until("lane 0 stops", || lane_0.is_closed()).await;
    assert!(!logs[0].work().contains(&Call::Finish(DrainPolicy::Shutdown)));
}

#[tokio::test(start_paused = true)]
async fn a_lane_whose_task_ends_outside_supervision_fails_the_region() {
    let (_control, mailbox) = control_mailbox();
    let region = start_laned(Setup::new(&["out"]), mailbox, 2, |lane| {
        let driver = ScriptedDriver::new(&Log::default());
        if lane.index == 1 {
            driver.panicking_at_start()
        } else {
            driver
        }
    })
    .await;
    assert_eq!(
        format!("{:#}", region.stopped().await.unwrap_err()),
        "Lane 1 of 2 of region 'region' stopped without finishing: its task panicked outside any supervised call, or \
         was aborted. See its logs for the reason."
    );
}

#[tokio::test(start_paused = true)]
async fn lanes_stamp_their_own_points_from_one_source_clock_and_every_lane_follows_a_reset() {
    let schedule = steady(1);
    let registry = AdmissionRegistry::new();
    let wall = ManualWallClock::new(secs(7_203));
    let clock = SourceClock::new(AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone())));
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let setup = Setup::new(&["out"]).connect(0, &out).admission(vec![
        owned_point(10, &schedule, &registry).with_source_clock(Arc::clone(&clock)),
        owned_point(11, &schedule, &registry).with_source_clock(Arc::clone(&clock)),
    ]);
    let mut region = start_laned(setup, mailbox, 2, |_| ScriptedDriver::new(&Log::default())).await;

    region.send(0, batch(&["a"]));
    let a = receive(&mut out_rx).await;
    assert_eq!(
        (names(&a), stamp_of(&a)),
        (vec!["a".to_string()], (Some(10), Some(secs(7_203)), 0, Some(1)))
    );
    region.send(1, batch(&["b"]));
    let b = receive(&mut out_rx).await;
    assert_eq!(
        (names(&b), stamp_of(&b)),
        (vec!["b".to_string()], (Some(11), Some(secs(7_203)), 0, Some(1)))
    );

    // Lane 0's next batch meets a jump back of an hour: its stamp resets the source's clock, and lane 1, idle, stamps a
    // heartbeat in the new epoch at once.
    settle().await;
    wall.set(secs(3_603));
    region.send(0, batch(&["c"]));
    settle().await;
    let sent = waiting(&mut out_rx);
    assert_eq!(
        sent.len(),
        2,
        "lane 1 stamps its heartbeat at once, not at its next timer"
    );
    assert_eq!(
        (names(&sent[0]), stamp_of(&sent[0])),
        (vec!["c".to_string()], (Some(10), Some(secs(3_603)), 1, Some(1)))
    );
    assert!(sent[1].is_empty());
    assert_eq!(stamp_of(&sent[1]), (Some(11), Some(secs(3_603)), 1, Some(1)));
    for point in [10, 11] {
        let status = registry
            .status_within(AdmissionPointId::new(point), Duration::from_secs(1))
            .await
            .expect("the point is registered");
        assert_eq!((status.last_admitted_at, status.epoch), (Some(secs(3_603)), 1));
    }

    region.close_inputs();
    region.stopped().await.expect("the region should stop cleanly");
}

/// Creates `count` downstream channels, one per lane of a downstream component, each holding `capacity` buffers.
fn downstream_lanes(
    count: usize, capacity: usize,
) -> (Vec<mpsc::Sender<EventsBuffer>>, Vec<mpsc::Receiver<EventsBuffer>>) {
    (0..count).map(|_| downstream(capacity)).unzip()
}

#[tokio::test(start_paused = true)]
async fn each_exit_sends_to_the_lanes_of_its_target_as_the_edge_its_output_leads_to_routes() {
    let (_control, mailbox) = control_mailbox();
    let (spread, mut spread_rx) = downstream_lanes(2, 4);
    let (affine, mut affine_rx) = downstream_lanes(3, 4);
    let (keyed, mut keyed_rx) = downstream_lanes(2, 4);
    let setup = Setup::new(&["spread", "affine", "keyed"])
        .connect_lanes(0, &spread, LaneEdge::Spread)
        .connect_lanes(1, &affine, LaneEdge::Affinity)
        .connect_lanes(2, &keyed, LaneEdge::Keyed);
    let mut region = start_laned(setup, mailbox, 2, |_| ScriptedDriver::new(&Log::default())).await;

    // Each lane writes one buffer to every exit, naming a lane only for the keyed exit. The region is configured with
    // nothing about its exits' routing: each lane learns it from the edge of the exit's output.
    region.send(0, batch(&["0.a", "1.b", "2.c@1"]));
    region.send(1, batch(&["0.d", "1.e", "2.f@0"]));
    settle().await;
    // A spread exit sends to the first lane with room from a rotation of the sending lane's own, which starts at the
    // lane's index; an affinity exit to the lane with the sending lane's index; a keyed exit to the lane named.
    assert_eq!(names_of_all(&waiting(&mut spread_rx[0])), expected(&[&["0.a"]]));
    assert_eq!(names_of_all(&waiting(&mut spread_rx[1])), expected(&[&["0.d"]]));
    assert_eq!(names_of_all(&waiting(&mut affine_rx[0])), expected(&[&["1.b"]]));
    assert_eq!(names_of_all(&waiting(&mut affine_rx[1])), expected(&[&["1.e"]]));
    assert!(waiting(&mut affine_rx[2]).is_empty());
    assert_eq!(names_of_all(&waiting(&mut keyed_rx[0])), expected(&[&["2.f@0"]]));
    assert_eq!(names_of_all(&waiting(&mut keyed_rx[1])), expected(&[&["2.c@1"]]));

    // Lane 0's rotation moved on to the lane after the one it last sent to.
    region.send(0, batch(&["0.g", "0.h"]));
    settle().await;
    assert_eq!(names_of_all(&waiting(&mut spread_rx[1])), expected(&[&["0.g"]]));
    assert_eq!(names_of_all(&waiting(&mut spread_rx[0])), expected(&[&["0.h"]]));

    region.close_inputs();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_buffer_that_its_exits_routing_cannot_send_fails_the_region_instead_of_being_dropped() {
    let cases: [(&'static str, LaneEdge, &str); 4] = [
        (
            "a",
            LaneEdge::Keyed,
            "Exit 'out' routes to the lanes of its target by key, but the region wrote a buffer to it without naming \
             a lane. Write each buffer of the exit to the lane that owns its keys, or connect the exit to its target \
             with the routing its region's driver uses.",
        ),
        (
            "a@2",
            LaneEdge::Keyed,
            "The region wrote a buffer to lane 2 of the target of exit 'out', which has 2 lanes. Write each buffer to \
             a lane its target has.",
        ),
        (
            "a@1",
            LaneEdge::Spread,
            "The region wrote a buffer to lane 1 of the target of exit 'out', but the exit routes to the lanes of its \
             target by spread routing, which picks the lane itself. Write the exit's buffers without a lane, or \
             connect the exit to its target by key.",
        ),
        (
            "a@0",
            LaneEdge::Affinity,
            "The region wrote a buffer to lane 0 of the target of exit 'out', but the exit routes to the lanes of its \
             target by affinity routing, which picks the lane itself. Write the exit's buffers without a lane, or \
             connect the exit to its target by key.",
        ),
    ];
    for (event, edge, error) in cases {
        let (_control, mailbox) = control_mailbox();
        let (lanes, mut lanes_rx) = downstream_lanes(2, 4);
        let setup = Setup::new(&["out"]).connect_lanes(0, &lanes, edge);
        let region = start(setup, mailbox, ScriptedDriver::new(&Log::default())).await;
        region.send(batch(&[event]));
        assert_eq!(
            format!("{:#}", region.stopped().await.unwrap_err()),
            error,
            "{} on a {} edge",
            event,
            edge
        );
        assert!(lanes_rx.iter_mut().all(|lane| waiting(lane).is_empty()));
    }
}

#[tokio::test(start_paused = true)]
async fn one_full_lane_does_not_hold_up_what_a_call_routes_to_the_other_lanes() {
    let (_control, mailbox) = control_mailbox();
    let (lanes, mut lanes_rx) = downstream_lanes(2, 4);
    let (full, mut full_rx) = downstream(1);
    full.try_send(batch(&["queued"]))
        .expect("lane 0 has room for one buffer");
    let setup = Setup::new(&["out"]).connect_lanes(0, &[full.clone(), lanes[1].clone()], LaneEdge::Keyed);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&Log::default())).await;

    // One call routes two buffers to each lane while lane 0 is full, and the next call waits behind it.
    region.send(batch(&["a@0", "b@1", "c@0", "d@1"]));
    region.send(batch(&["e@1"]));
    settle().await;
    assert_eq!(
        names_of_all(&waiting(&mut lanes_rx[1])),
        expected(&[&["b@1"], &["d@1"]]),
        "lane 1 gets the call's buffers while lane 0 is full, and nothing of the next call"
    );

    // Lane 0 receives its buffers in order once it has room, and only then does the next call run.
    assert_eq!(
        receive_names(&mut full_rx, 3).await,
        expected(&[&["queued"], &["a@0"], &["c@0"]])
    );
    assert_eq!(receive_names(&mut lanes_rx[1], 1).await, expected(&[&["e@1"]]));

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

// Lanes that fill and drain while a call's buffers are tried: a buffer for a lane whose earlier buffer was held must
// wait behind it even when the lane has room again by the time it is tried, or it overtakes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyed_buffers_keep_their_order_in_every_lane_while_the_lanes_fill_and_drain_concurrently() {
    const CALLS: usize = 300;
    const PER_LANE: usize = 4;
    // Each call writes buffers to both lanes, alternating, in an order each lane must keep.
    const NAMES: [&str; PER_LANE * 2] = ["0@0", "0@1", "1@0", "1@1", "2@0", "2@1", "3@0", "3@1"];
    let (_control, mailbox) = control_mailbox();
    let (lanes, lanes_rx) = downstream_lanes(2, 1);
    let mut region = start(
        Setup::new(&["out"]).connect_lanes(0, &lanes, LaneEdge::Keyed),
        mailbox,
        ScriptedDriver::new(&Log::default()),
    )
    .await;
    drop(lanes);

    // Each lane's consumer drains its channel at its own pace, as each buffer arrives.
    let consumers = lanes_rx
        .into_iter()
        .map(|mut lane| {
            tokio::spawn(async move {
                let mut received = Vec::new();
                while let Some(buffer) = lane.recv().await {
                    received.push(names(&buffer).concat());
                    if received.len() % 3 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                received
            })
        })
        .collect::<Vec<_>>();

    let input = region.input.clone().expect("input is open");
    for _ in 0..CALLS {
        input.send(batch(&NAMES)).await.expect("the region runs");
    }
    drop(input);
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");

    for (lane, consumer) in consumers.into_iter().enumerate() {
        let received = consumer.await.unwrap();
        let expected = (0..CALLS)
            .flat_map(|_| (0..PER_LANE).map(move |index| format!("{}@{}", index, lane)))
            .collect::<Vec<_>>();
        assert_eq!(received, expected, "lane {} received its buffers out of order", lane);
    }
}

#[tokio::test(start_paused = true)]
async fn a_lane_that_finished_still_reads_live_and_takes_no_part_in_later_control_messages() {
    let registry = HealthRegistry::new();
    let _probes = registry.spawn_runner_for_test();
    let id = SubsystemIdentifier::from_dotted(REGION_ID);
    let logs = [Log::default(), Log::default()];
    let (control, mailbox) = control_mailbox();
    let driver_logs = logs.clone();
    let mut region = start_laned(Setup::new(&["out"]).health(&registry), mailbox, 2, move |lane| {
        ScriptedDriver::new(&driver_logs[usize::from(lane.index)])
    })
    .await;
    assert!(timeout(PATIENCE, control.running()).await.expect("the region runs"));

    // Lane 0's input ends and it finishes, while lane 1 runs on, as when lanes finish a shutdown at different times.
    region.close_input(0);
    settle().await;
    assert_eq!(logs[0].work(), vec![Call::Finish(DrainPolicy::Shutdown)]);
    sleep(Duration::from_secs(30)).await;
    assert!(
        registry.is_live_for_test(&id),
        "a lane that finished is not a stuck lane"
    );

    // A message that splits goes to the lanes that still run; one for the finished lane alone was handled by no lane,
    // which a controller sees as it sees a region that stopped.
    acknowledged(control.send(Command::Note("later"))).await.unwrap();
    assert!(matches!(
        acknowledged(control.send(Command::ForLane(0, "lane zero"))).await,
        Err(ControlError::Stopped { .. })
    ));
    assert_eq!(logs[1].work(), vec![Call::Control("later")]);
    assert_eq!(logs[0].work(), vec![Call::Finish(DrainPolicy::Shutdown)]);

    region.close_input(1);
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_region_with_lanes_reads_not_live_while_one_lane_is_stuck_and_live_again_once_it_runs() {
    let registry = HealthRegistry::new();
    let _probes = registry.spawn_runner_for_test();
    let id = SubsystemIdentifier::from_dotted(REGION_ID);
    let schedule = steady(1);
    let points = AdmissionRegistry::new();
    let (_control, mailbox) = control_mailbox();
    let setup = Setup::new(&["out"]).health(&registry).admission(vec![
        owned_point(20, &schedule, &points),
        owned_point(21, &schedule, &points),
    ]);
    let mut region = start_laned(setup, mailbox, 2, |_| ScriptedDriver::new(&Log::default())).await;
    sleep(Duration::from_secs(10)).await;
    assert!(registry.is_live_for_test(&id));

    // A transaction holds lane 1's point, so the lane's next batch waits to be stamped, and the lane answers no probe
    // meanwhile, while lane 0 answers every one.
    let transaction = points
        .transaction(&[AdmissionPointId::new(21)], Duration::from_secs(1))
        .await
        .expect("the point is free");
    region.send(1, batch(&["a"]));
    sleep(Duration::from_secs(10)).await;
    assert!(
        !registry.is_live_for_test(&id),
        "a stuck lane reads as a stuck component"
    );

    transaction.abandon();
    sleep(Duration::from_secs(10)).await;
    assert!(registry.is_live_for_test(&id));

    region.close_inputs();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_a_heartbeat_ends_tells_the_driver_whether_input_waits() {
    let log = Log::default();
    let schedule = steady(1);
    let registry = AdmissionRegistry::new();
    let wall = ManualWallClock::new(secs(7_203));
    let clock = SourceClock::new(AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone())));
    let neighbor =
        OwnedAdmission::new(owned_point(2, &schedule, &registry).with_source_clock(Arc::clone(&clock))).unwrap();
    let (_control, mailbox) = control_mailbox();
    let (out, mut out_rx) = downstream(16);
    let setup = Setup::new(&["out"]).connect(0, &out).admission(vec![
        owned_point(3, &schedule, &registry).with_source_clock(Arc::clone(&clock))
    ]);
    let mut region = start(setup, mailbox, ScriptedDriver::new(&log)).await;
    region.send(batch(&["a"]));
    assert_eq!(receive_names(&mut out_rx, 1).await, expected(&[&["a"]]));
    settle().await;

    // Another point of the source resets the clock while two batches wait: the region stamps its heartbeat in the new
    // epoch first, and the turn that heartbeat ends has input waiting.
    wall.set(secs(3_603));
    neighbor.stamp(&mut BatchHeader::default()).await;
    region.send(batch(&["b"]));
    region.send(batch(&["c"]));
    assert_eq!(
        receive_names(&mut out_rx, 3).await,
        vec![Vec::<String>::new(), vec!["b".to_string()], vec!["c".to_string()]]
    );
    settle().await;
    assert_eq!(log.turn_ends(), vec![(1, true), (2, false), (3, false), (4, true)]);

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test(start_paused = true)]
async fn each_lane_counts_what_it_received_its_input_depth_and_its_stamp_wait_in_its_own_lane_stats() {
    let schedule = steady(1);
    let points = AdmissionRegistry::new();
    let stats = [Arc::new(LaneStats::new()), Arc::new(LaneStats::new())];
    let (control, mailbox) = control_mailbox();
    let setup = Setup::new(&["out"]).stats(&stats).admission(vec![
        owned_point(30, &schedule, &points).with_heartbeat(Duration::from_secs(1)),
        owned_point(31, &schedule, &points),
    ]);
    let mut region = start_laned(setup, mailbox, 2, |_| ScriptedDriver::new(&Log::default())).await;

    // Lane 0 stops accepting input after its first batch, so its turn ends with two batches waiting.
    region.send(0, batch(&["a", "stop"]));
    region.send(0, batch(&["c"]));
    region.send(0, EventsBuffer::with_capacity(0));
    region.send(1, batch(&["x"]));
    settle().await;
    let counts = |stats: &LaneStats| (stats.batches(), stats.events(), stats.input_depth());
    assert_eq!(counts(&stats[0]), (1, 2, 2));
    assert_eq!(counts(&stats[1]), (1, 1, 0));

    // Once it accepts input again, it takes the rest, a header-only batch included; the heartbeats its own point
    // stamps while it is idle are not received, and not counted.
    acknowledged(control.send(Command::Accept(true))).await.unwrap();
    settle().await;
    sleep(Duration::from_secs(5)).await;
    assert_eq!(counts(&stats[0]), (3, 3, 0));

    // A stamp of lane 1 waits for a transaction over its point, and the lane's next turn records the wait.
    let transaction = points
        .transaction(&[AdmissionPointId::new(31)], Duration::from_secs(1))
        .await
        .expect("the point is free");
    region.send(1, batch(&["y"]));
    sleep(Duration::from_millis(3)).await;
    transaction.abandon();
    settle().await;
    assert_eq!(counts(&stats[1]), (2, 2, 0));
    assert_eq!(
        (stats[0].stamp_wait(), stats[1].stamp_wait()),
        (Duration::ZERO, Duration::from_millis(3))
    );

    region.close_inputs();
    region.stopped().await.expect("the region should stop cleanly");
}

#[tokio::test]
async fn a_lane_counts_the_time_of_its_calls_into_its_driver_as_busy() {
    let stats = Arc::new(LaneStats::new());
    let (_control, mailbox) = control_mailbox();
    let driver =
        ScriptedDriver::new(&Log::default()).calling_on_process(|| std::thread::sleep(Duration::from_millis(20)));
    let mut region = start(Setup::new(&["out"]).stats(&[Arc::clone(&stats)]), mailbox, driver).await;
    region.send(batch(&["a"]));
    region.send(batch(&["b"]));
    wait_until("both batches are processed", || stats.batches() == 2).await;
    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
    assert!(stats.busy() >= Duration::from_millis(40), "{:?}", stats.busy());
}

#[test]
#[should_panic(expected = "region wrote to exit 2 but has only 2 exits")]
fn writing_to_a_missing_exit_panics() {
    ExitSink::new(2).emit(ExitId::new(2), batch(&["x"]));
}

#[tokio::test]
async fn regions_with_lanes_run_in_a_topology_and_route_by_the_edges_it_connects_them_with() {
    // `region` spreads the source's batches over its two lanes and routes each event by key to a lane of `state`,
    // whose two lanes send everything they receive on to one destination.
    let logs = (0..4).map(|_| Log::default()).collect::<Vec<_>>();
    let laned = |exits: [&'static str; 1], offset: usize| {
        let (control, mailbox) = control_mailbox::<Command>();
        let logs = logs.clone();
        let builder = RegionHostBuilder::new(exits, NonZeroUsize::new(8).unwrap(), mailbox, |_: &BuildContext| {
            Err(generic_error!("a region with lanes builds its drivers per lane"))
        })
        .unwrap()
        .with_lanes(NonZeroU16::new(2).unwrap(), move |_, lane| {
            Ok(ScriptedDriver::new(&logs[offset + usize::from(lane.index)]))
        });
        (control, builder)
    };
    let (control, region) = laned(["keyed"], 0);
    let (state_control, state) = laned(["out"], 2);

    let received = Arc::new(Mutex::new(Vec::new()));
    let component_registry = ComponentRegistry::default();
    let mut blueprint = TopologyBlueprint::new("test", &component_registry);
    blueprint
        .add_source(
            "src",
            ListSourceBuilder {
                batches: vec![vec!["0.a@1", "0.b@0"], vec!["0.c@1"], vec!["0.d@0"]],
            },
        )
        .unwrap()
        .add_transform(REGION_ID, region)
        .unwrap()
        .add_transform("state", state)
        .unwrap()
        .add_destination(
            "out",
            CollectorBuilder {
                received: Arc::clone(&received),
            },
        )
        .unwrap()
        .connect_components_with("src", REGION_ID, LaneEdge::Spread)
        .unwrap()
        .connect_components_with("region.keyed", "state", LaneEdge::Keyed)
        .unwrap()
        .connect_components("state.out", ["out"])
        .unwrap()
        .with_health_registry(HealthRegistry::new())
        .with_memory_limiter(MemoryLimiter::noop())
        .with_resource_registry(ResourceRegistry::new());

    let mut supervisor = Supervisor::new("test-topology").unwrap();
    supervisor.add_worker(blueprint);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let topology = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

    wait_until("the destination received every event", || {
        received.lock().unwrap().len() == 4
    })
    .await;
    acknowledged(control.send(Command::Note("running"))).await.unwrap();
    acknowledged(state_control.send(Command::Note("running")))
        .await
        .unwrap();

    shutdown.send(()).expect("the topology should be running");
    let result = timeout(PATIENCE, topology)
        .await
        .expect("the topology should stop")
        .expect("the topology task should not panic");
    assert!(result.is_ok(), "the topology should stop cleanly, got: {:?}", result);

    let mut delivered = received.lock().unwrap().concat();
    delivered.sort();
    assert_eq!(delivered, ["0.a@1", "0.b@0", "0.c@1", "0.d@0"]);
    // The source's batches spread over `region`'s lanes, each whole; every event reached the lane of `state` it named.
    let processed = |log: &Log| {
        log.work()
            .into_iter()
            .filter_map(|call| match call {
                Call::Process(names) => Some(names),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut batches = [processed(&logs[0]), processed(&logs[1])].concat();
    batches.sort();
    assert_eq!(batches, expected(&[&["0.a@1", "0.b@0"], &["0.c@1"], &["0.d@0"]]));
    assert_eq!(processed(&logs[2]), expected(&[&["0.b@0"], &["0.d@0"]]));
    assert_eq!(processed(&logs[3]), expected(&[&["0.a@1"], &["0.c@1"]]));
    for log in &logs {
        assert!(log.work().contains(&Call::Control("running")));
        assert!(log.work().contains(&Call::Finish(DrainPolicy::Shutdown)));
    }
}

/// A source that sends a list of batches, then waits for shutdown.
struct ListSource {
    batches: Vec<Vec<&'static str>>,
}

#[async_trait]
impl Source for ListSource {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let shutdown = context.take_shutdown_handle();
        for names in &self.batches {
            context.dispatcher().dispatch(batch(names)).await?;
        }
        shutdown.await;
        Ok(())
    }
}

struct ListSourceBuilder {
    batches: Vec<Vec<&'static str>>,
}

#[async_trait]
impl SourceBuilder for ListSourceBuilder {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, _: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(ListSource {
            batches: self.batches.clone(),
        }))
    }
}

impl MemoryBounds for ListSourceBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

/// A destination that records the event names of every buffer it receives.
struct Collector {
    received: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl Destination for Collector {
    async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        while let Some(buffer) = context.events().next().await {
            self.received.lock().unwrap().push(names(&buffer));
        }
        Ok(())
    }
}

struct CollectorBuilder {
    received: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl DestinationBuilder for CollectorBuilder {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        Ok(Box::new(Collector {
            received: Arc::clone(&self.received),
        }))
    }
}

impl MemoryBounds for CollectorBuilder {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

#[tokio::test]
async fn a_region_host_runs_in_a_topology_and_delivers_each_exit_to_its_connections() {
    let log = Log::default();
    let driver_log = log.clone();
    let (control, mailbox) = control_mailbox();
    let region = RegionHostBuilder::new(
        ["evens", "odds", "unused"],
        NonZeroUsize::new(8).unwrap(),
        mailbox,
        move |_: &BuildContext| Ok(ScriptedDriver::new(&driver_log)),
    )
    .unwrap();

    let evens = Arc::new(Mutex::new(Vec::new()));
    let odds = Arc::new(Mutex::new(Vec::new()));
    let component_registry = ComponentRegistry::default();
    let mut blueprint = TopologyBlueprint::new("test", &component_registry);
    blueprint
        .add_source(
            "src",
            ListSourceBuilder {
                batches: vec![vec!["0.a", "1.b"], vec!["1.c"], vec!["0.d"]],
            },
        )
        .unwrap()
        .add_transform(REGION_ID, region)
        .unwrap()
        .add_destination(
            "evens_out",
            CollectorBuilder {
                received: Arc::clone(&evens),
            },
        )
        .unwrap()
        .add_destination(
            "odds_out",
            CollectorBuilder {
                received: Arc::clone(&odds),
            },
        )
        .unwrap()
        .connect_components("src", [REGION_ID])
        .unwrap()
        .connect_components("region.evens", ["evens_out"])
        .unwrap()
        .connect_components("region.odds", ["odds_out"])
        .unwrap()
        .with_health_registry(HealthRegistry::new())
        .with_memory_limiter(MemoryLimiter::noop())
        .with_resource_registry(ResourceRegistry::new());

    let mut supervisor = Supervisor::new("test-topology").unwrap();
    supervisor.add_worker(blueprint);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let topology = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

    wait_until("both destinations received their buffers", || {
        evens.lock().unwrap().len() == 2 && odds.lock().unwrap().len() == 2
    })
    .await;
    acknowledged(control.send(Command::Note("running"))).await.unwrap();

    shutdown.send(()).expect("the topology should be running");
    let result = timeout(PATIENCE, topology)
        .await
        .expect("the topology should stop")
        .expect("the topology task should not panic");
    assert!(result.is_ok(), "the topology should stop cleanly, got: {:?}", result);

    assert_eq!(*evens.lock().unwrap(), expected(&[&["0.a"], &["0.d"]]));
    assert_eq!(*odds.lock().unwrap(), expected(&[&["1.b"], &["1.c"]]));
    assert_eq!(
        log.work(),
        vec![
            process(&["0.a", "1.b"]),
            process(&["1.c"]),
            process(&["0.d"]),
            Call::Control("running"),
            Call::Finish(DrainPolicy::Shutdown),
        ]
    );
}

/// What one buffer a lane sent says about its stamp: its admission point, epoch, admission time, generation, and
/// whether it carried data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Sent {
    point: u32,
    epoch: u32,
    admitted_at: AdmissionTime,
    generation: u64,
    data: bool,
}

impl Sent {
    fn of(buffer: &EventsBuffer) -> Self {
        let header = buffer.header();
        Self {
            point: header.admission_point.expect("stamped").get(),
            epoch: header.epoch,
            admitted_at: header.admitted_at.expect("stamped"),
            generation: header.generation_id().expect("stamped").get(),
            data: !buffer.is_empty(),
        }
    }
}

/// A cut the installer of a stamp-order case armed: its new generation, its time, and the epoch its boundary recorded.
#[derive(Clone, Copy, Debug)]
struct Armed {
    generation: u64,
    at: AdmissionTime,
    epoch: u32,
}

/// What a stamp-order case observed: every buffer each downstream channel received, the cuts it armed, and the number
/// of data events its lanes were fed.
struct StampOrder {
    channels: [Vec<Sent>; 2],
    armed: Vec<Armed>,
    fed: usize,
}

/// Runs a region with `lanes` lanes, each owning a point of one source clock with a heartbeat every millisecond, fed
/// `batches[lane]` one-event batches each, while the wall clock moves forward, jumps back beyond the reset threshold
/// at the steps `resets` names, and an installer arms `cuts` cuts one after another over every point, each in one
/// transaction, finalizing each once every point passed it. Exit 0 feeds two channels of `capacity` buffers.
async fn run_stamp_order_case(
    lanes: u16, batches: Vec<usize>, cuts: usize, resets: Vec<usize>, capacity: usize,
) -> StampOrder {
    const THRESHOLD: Duration = Duration::from_secs(1);
    let wall = ManualWallClock::new(secs(1_000_000));
    let clock = SourceClock::new(AdmissionClock::with_wall_clock(THRESHOLD, Arc::new(wall.clone())));
    let registry = AdmissionRegistry::new();
    let first = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let schedule = AdmissionSchedule::Steady(Arc::clone(&first)).into_handle();
    let points = (0..lanes)
        .map(|lane| {
            AdmissionPointConfig::new(
                AdmissionPointId::new(10 + u32::from(lane)),
                AdmissionMode::Admit,
                Arc::clone(&schedule),
                THRESHOLD,
            )
            .with_source_clock(Arc::clone(&clock))
            .with_heartbeat(Duration::from_millis(1))
            .with_registry(registry.clone())
            .owned()
        })
        .collect();
    let lock_order = (0..lanes)
        .map(|lane| AdmissionPointId::new(10 + u32::from(lane)))
        .collect::<Vec<_>>();

    let (out_a, rx_a) = downstream(capacity);
    let (out_b, rx_b) = downstream(capacity);
    let setup = Setup::new(&["out"])
        .connect(0, &out_a)
        .connect(0, &out_b)
        .work_per_turn(4)
        .admission(points);
    drop((out_a, out_b));
    let (_control, mailbox) = control_mailbox();
    let (inputs, region) = if lanes == 1 {
        let mut running = start(setup, mailbox, ScriptedDriver::new(&Log::default())).await;
        (vec![running.input.take().expect("input is open")], running.task)
    } else {
        let mut running = start_laned(setup, mailbox, lanes, |_| ScriptedDriver::new(&Log::default())).await;
        let inputs = running
            .inputs
            .iter_mut()
            .map(|input| input.take().expect("input is open"))
            .collect();
        (inputs, running.task)
    };

    let collect = |mut rx: mpsc::Receiver<EventsBuffer>| {
        tokio::spawn(async move {
            let mut sent = Vec::new();
            while let Some(buffer) = rx.recv().await {
                sent.push(Sent::of(&buffer));
            }
            sent
        })
    };
    let collectors = [collect(rx_a), collect(rx_b)];

    // Each feeder hands its input back once it fed its lane, so that the lanes, and their points, live until the
    // installer is done.
    // Lane 0's feeder makes the wall clock jump back 2 s, beyond the threshold, after the batches `resets` names, so
    // that whichever lane stamps next resets the source's clock.
    let fed = batches.iter().take(usize::from(lanes)).sum();
    let feeders = inputs
        .into_iter()
        .zip(batches)
        .enumerate()
        .map(|(lane, (input, count))| {
            let (wall, resets) = (wall.clone(), resets.clone());
            tokio::spawn(async move {
                for index in 0..count {
                    input.send(batch(&["e"])).await.expect("the lane reads its input");
                    if lane == 0 && resets.contains(&index) {
                        wall.set(AdmissionTime::from_unix_nanos(
                            wall.now().as_unix_nanos() - 2_000_000_000,
                        ));
                    }
                    if index % 4 == 0 {
                        sleep(Duration::from_micros(300)).await;
                    }
                }
                input
            })
        })
        .collect::<Vec<_>>();

    // Meanwhile, the wall clock moves ahead 1 ms at every tick of the runtime's timer.
    let moving = Arc::new(AtomicBool::new(true));
    let mover = tokio::spawn({
        let (wall, moving) = (wall.clone(), Arc::clone(&moving));
        async move {
            while moving.load(Ordering::SeqCst) {
                wall.advance(Duration::from_millis(1));
                sleep(Duration::from_micros(100)).await;
            }
        }
    });

    let installer = tokio::spawn({
        let (registry, wall) = (registry.clone(), wall.clone());
        async move {
            let mut current = first;
            let mut armed = Vec::new();
            for _ in 0..cuts {
                sleep(Duration::from_millis(1)).await;
                let transaction = registry
                    .transaction(&lock_order, Duration::from_secs(5))
                    .await
                    .expect("stamps hold each lock only for a stamp");
                let statuses = transaction.statuses();
                let now = statuses
                    .iter()
                    .map(|(_, status)| {
                        status
                            .last_admitted_at
                            .map_or(status.wall_now, |last| last.max(status.wall_now))
                    })
                    .max()
                    .unwrap_or_else(|| wall.now());
                let at = now.saturating_add(Duration::from_millis(3));
                let epoch = statuses[0].1.epoch;
                assert!(
                    statuses.iter().all(|(_, status)| status.epoch == epoch),
                    "every point of one clock reports its epoch"
                );
                let next = Arc::new(GenerationToken::new(GenerationId::new(current.id().get() + 1)));
                next.set_cut(CutBoundary {
                    at,
                    replaces: current.id(),
                    epochs: statuses.iter().map(|(id, status)| (*id, status.epoch)).collect(),
                })
                .unwrap();
                transaction.publish(AdmissionSchedule::Cut {
                    old: Arc::clone(&current),
                    new: Arc::clone(&next),
                    at,
                });
                armed.push(Armed {
                    generation: next.id().get(),
                    at,
                    epoch,
                });

                // Finalize once every point passed the cut, as an installer does, and arm the next cut only then.
                let mut finalized = false;
                for _ in 0..2_000 {
                    sleep(Duration::from_micros(200)).await;
                    let transaction = registry.transaction(&lock_order, Duration::from_secs(5)).await.unwrap();
                    if transaction.statuses().iter().all(|(_, status)| status.passed_cut) {
                        transaction.publish(AdmissionSchedule::Steady(Arc::clone(&next)));
                        finalized = true;
                        break;
                    }
                }
                if !finalized {
                    break;
                }
                current = next;
            }
            armed
        }
    });

    let mut inputs = Vec::new();
    for feeder in feeders {
        inputs.push(feeder.await.unwrap());
    }
    let armed = installer.await.unwrap();
    moving.store(false, Ordering::SeqCst);
    mover.await.unwrap();
    // Closing every input stops the region, which closes the downstream channels.
    drop(inputs);
    region.await.unwrap().expect("the region should stop cleanly");
    let [a, b] = collectors;
    StampOrder {
        channels: [a.await.unwrap(), b.await.unwrap()],
        armed,
        fed,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    // One task per lane stamps its own point and sends what it stamped in order, with heartbeats, while transactions
    // arm and finalize cuts over every point and the shared clock resets. Every channel receives each point's batches
    // in stamp order, both channels the same ones, every data event exactly once, and every batch the generation the
    // cut rule gives its stamp: the new generation exactly when its admission time reaches the cut or its epoch differs
    // from the one the cut's boundary recorded.
    #[test]
    fn property_test_owned_admission_keeps_stamp_order(
        lanes in 1u16..=3,
        batches in proptest::collection::vec(1usize..40, 3),
        cuts in 1usize..4,
        resets in proptest::collection::vec(0usize..8, 0..=2),
        capacity in 1usize..4,
    ) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .enable_all()
            .build()
            .unwrap();
        let case = runtime
            .block_on(async {
                let case = run_stamp_order_case(lanes, batches, cuts, resets, capacity);
                timeout(Duration::from_secs(60), case).await
            })
            .expect("the case should finish");

        for sent in &case.channels {
            prop_assert_eq!(sent.iter().filter(|sent| sent.data).count(), case.fed);
            for point in 0..lanes {
                let point = 10 + u32::from(point);
                let stamps = sent.iter().filter(|sent| sent.point == point).collect::<Vec<_>>();
                for pair in stamps.windows(2) {
                    let (earlier, later) = (pair[0], pair[1]);
                    prop_assert!(
                        (later.epoch, later.admitted_at) >= (earlier.epoch, earlier.admitted_at),
                        "point {} sent {:?} after {:?}", point, later, earlier
                    );
                    prop_assert!(
                        later.generation >= earlier.generation,
                        "point {} sent {:?} after {:?}", point, later, earlier
                    );
                }
            }
            for stamp in sent {
                for cut in &case.armed {
                    if stamp.generation == cut.generation {
                        prop_assert!(
                            stamp.epoch != cut.epoch || stamp.admitted_at >= cut.at,
                            "{:?} under {:?}", stamp, cut
                        );
                    } else if stamp.generation + 1 == cut.generation {
                        prop_assert!(
                            stamp.epoch < cut.epoch || (stamp.epoch == cut.epoch && stamp.admitted_at < cut.at),
                            "{:?} under {:?}", stamp, cut
                        );
                    }
                }
            }
        }
        for point in 0..lanes {
            let point = 10 + u32::from(point);
            let of = |sent: &Vec<Sent>| sent.iter().filter(|sent| sent.point == point).copied().collect::<Vec<_>>();
            prop_assert_eq!(of(&case.channels[0]), of(&case.channels[1]));
        }
    }
}
