//! Tests of the region host, built through its builder and driven by a scripted driver.
//!
//! Most tests run the host on its own task with a hand-built transform context, so that they control its input, its
//! downstream channels, and its control mailbox exactly. Tests that observe what the host does while nothing else
//! happens run with paused time: [`settle`] returns only once every other task is idle, because the runtime advances a
//! paused clock only when no task can make progress.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::test::TestRecorder;
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, sleep_until, timeout, Instant},
};

use super::{host::RegionHost, *};
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
        AdmissionPointId, AdmissionTime, BatchHeader, Budget, DrainPolicy, GenerationId, GenerationToken, Progress,
        WorkKind,
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
        EventsBuffer, OutputDefinition, OutputName, TopologyBlueprint, TopologyContext,
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
}

/// The calls a scripted driver received, shared with the test.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Call>>>);

impl Log {
    fn push(&self, call: Call) {
        self.0.lock().unwrap().push(call);
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

/// A driver whose behavior the test scripts.
///
/// Every input event is written to an exit of its own buffer, stamped with the header of the batch: the exit named by
/// a one-digit prefix (`1.x` goes to exit 1), or exit 0. Events are written one per unit of the turn's event budget, so
/// a limited budget makes calls yield. A header-only batch is passed on as a header-only buffer to exit 0. An event
/// named `panic` makes `process` write the events before it and then panic; an event named `poke` makes it send a
/// control message to its own region; an event named `stop` makes the driver stop accepting input.
struct ScriptedDriver {
    log: Log,
    header: BatchHeader,
    queue: VecDeque<Event>,
    accepting: bool,
    deadline: Option<std::time::Instant>,
    tick_period: Option<Duration>,
    finish_events: usize,
    panic_in: Option<PanicSite>,
    fail_on_panic: bool,
    pokes: Option<ControlSender<Command>>,
    spin: Option<Spin>,
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
            finish_events: 0,
            panic_in: None,
            fail_on_panic: false,
            pokes: None,
            spin: None,
        }
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

    fn finishing_with(mut self, events: usize) -> Self {
        self.finish_events = events;
        self
    }

    /// Makes the given call write one event named `doomed`, and then panic, until the driver is reset.
    fn panicking_in(mut self, site: PanicSite) -> Self {
        self.panic_in = Some(site);
        self
    }

    fn failing_on_panic(mut self) -> Self {
        self.fail_on_panic = true;
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
            let exit = exit_of(&name_of(&event));
            let mut buffer = EventsBuffer::with_capacity(1).with_header(self.header.clone());
            assert!(buffer.try_push(event).is_none());
            exits.emit(exit, buffer);
            turn.budget_mut().charge(WorkKind::Events, 1);
        }
        Progress::Done
    }

    fn panic_if(&self, site: PanicSite, exits: &mut ExitSink) {
        if self.panic_in == Some(site) {
            exits.emit(ExitId::new(0), batch(&["doomed"]));
            panic!("boom in {}", site);
        }
    }
}

impl RegionDriver for ScriptedDriver {
    type Control = Command;

    fn accepts_input(&self) -> bool {
        self.accepting
    }

    fn process(&mut self, turn: &mut RegionTurn<'_>, mut batch: EventsBuffer, exits: &mut ExitSink) -> Progress {
        self.log.push(Call::Process(names(&batch)));
        self.header = batch.take_header();
        if batch.is_empty() {
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
        &mut self, _turn: &mut RegionTurn<'_>, message: Command, exits: &mut ExitSink,
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
        }
        Ok(())
    }

    fn next_deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }

    fn advance_time(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        self.log.push(Call::AdvanceTime);
        self.panic_if(PanicSite::AdvanceTime, exits);
        self.deadline = self.tick_period.map(|period| turn.now() + period);
        self.queue.push_back(metric("tick"));
        self.write(turn, exits)
    }

    fn finish(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink, policy: DrainPolicy) -> Progress {
        self.log.push(Call::Finish(policy));
        self.panic_if(PanicSite::Finish, exits);
        self.queue
            .extend(FINISH_EVENTS[..self.finish_events].iter().map(|name| metric(name)));
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

        // A rebuilt driver holds nothing, has no deadline, and does not panic again.
        self.queue.clear();
        self.deadline = None;
        self.panic_in = None;
        if self.fail_on_panic {
            PanicOutcome::Fail(generic_error!("The region has no panic budget."))
        } else {
            PanicOutcome::Continue
        }
    }

    fn publish_telemetry(&mut self) {
        self.log.push(Call::Publish);
        if self.panic_in == Some(PanicSite::PublishTelemetry) {
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
    wiring: Vec<(usize, mpsc::Sender<EventsBuffer>)>,
    health: HealthRegistry,
}

impl Setup {
    fn new(exits: &'static [&'static str]) -> Self {
        Self {
            exits,
            work_per_turn: 1,
            turn_budget: Budget::unlimited(),
            wiring: Vec::new(),
            health: HealthRegistry::new(),
        }
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
        self.wiring.push((exit, downstream.clone()));
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
    .with_turn_budget(setup.turn_budget);
    let transform = builder
        .build(BuildContext::test_transform(REGION_ID))
        .await
        .expect("the region should build");

    let component_context = ComponentContext::test_transform(REGION_ID);
    let mut dispatcher = Dispatcher::new(component_context.clone());
    for output in builder.outputs() {
        let name = output.output_name().expect("exits are named outputs").to_string();
        dispatcher.add_output(OutputName::Given(name.into())).unwrap();
    }
    for (exit, downstream) in setup.wiring {
        dispatcher
            .attach_sender_to_output(&OutputName::Given(setup.exits[exit].into()), downstream)
            .unwrap();
    }

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

    region.close_input();
    region.stopped().await.expect("the region should stop cleanly");
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
        Err(ControlError::Stopped)
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
                    matches!(&ack, Err(ControlError::Panicked { message }) if message == "boom in control"),
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

        // The region keeps running after the panic, unless the panic was in finishing it.
        let finished_by_the_panic = matches!(case.trigger, PanicTrigger::EndOfInput);
        if !finished_by_the_panic {
            region.send(batch(&["after"]));
        }
        region.close_input();
        region
            .stopped()
            .await
            .expect("the region should recover and stop cleanly");

        let mut delivered = delivered_before;
        if !finished_by_the_panic {
            delivered.push(vec!["after".to_string()]);
            delivered.push(vec!["finish.0".to_string()]);
        }
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
        Err(ControlError::Stopped)
    ));
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
        Err(ControlError::Rejected { failure }) => {
            assert_eq!(failure.to_string(), "Generation 7 is not prepared.");
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
    assert!(matches!(error, ControlError::Stopped));
    assert_eq!(
        error.to_string(),
        "The region stopped before it handled the control message. The region is shutting down, or it failed; see \
         its logs for the reason."
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
    let full_exit_buffer = std::mem::size_of::<(ExitId, EventsBuffer)>() + 1024 * std::mem::size_of::<Event>();
    assert_eq!(bounds.total_minimum_required_bytes(), minimum);
    assert_eq!(bounds.total_firm_limit_bytes(), minimum + 3 * full_exit_buffer + 1_000);
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
#[should_panic(expected = "region wrote to exit 2 but has only 2 exits")]
fn writing_to_a_missing_exit_panics() {
    ExitSink::new(2).emit(ExitId::new(2), batch(&["x"]));
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
