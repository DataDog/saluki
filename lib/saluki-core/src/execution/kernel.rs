//! Kernels and turns.

use std::{any::Any, time::Instant};

use super::header::{AdmissionTime, BatchHeader};
use crate::{
    data_model::event::Event,
    topology::{interconnect::FixedSizeEventBuffer, EventsBuffer},
};

/// The initial capacity, in events, of a port buffer created for individually written events.
///
/// Port buffers grow as needed up to the capacity of [`EventsBuffer`], so this only bounds the first allocation.
const PORT_BUFFER_INITIAL_CAPACITY: usize = 16;

/// The synchronous core of one processing step.
///
/// A kernel consumes batches of events and writes its results to its output ports. Each port is identified by a
/// [`PortIndex`]; port 0 is the **primary** port.
///
/// Kernels never await, perform I/O, read configuration, or schedule their own work. Whatever a kernel depends on is
/// given to it when it is built, or is provided by the [`Turn`] it runs in: the batch header with its admission time,
/// the host's monotonic clock, its output ports, its work budget, and its counters. Whatever hosts a kernel -- a
/// component in a running topology, or a test that calls it directly -- drives it in turns.
///
/// Only [`consume`][Self::consume] is required. The other methods default to doing nothing and returning
/// [`Progress::Done`], which is correct for stateless kernels.
///
/// # Driving a kernel
///
/// A host owns one [`PortOutputs`], [`Budget`], and [`TurnCounters`] per kernel, and for each call builds a turn over
/// them with [`Turn::new`]. After a call to [`Kernel::consume`] returns anything other than [`Progress::Yielded`], the
/// host passes the input buffer to [`PortOutputs::complete`], which moves the events the kernel left in it to their
/// port. The host then drains the ports and delivers their buffers downstream.
///
/// The batch header travels beside the input buffer, never inside it: the host takes the header of the batch it is
/// about to process, passes it to the turn, and stamps every buffer it drains from [`PortOutputs`] with the header that
/// output carries, since buffers created for individually written events start without one. A kernel reads the header
/// only through [`Turn::header`].
///
/// <!-- vale off -->
/// ```
/// use std::time::Instant;
///
/// use saluki_core::data_model::event::{metric::Metric, Event};
/// use saluki_core::execution::{BatchHeader, Budget, Kernel, PortIndex, PortOutputs, Progress, Turn, TurnCounters};
/// use saluki_core::topology::EventsBuffer;
///
/// /// Keeps only gauges.
/// struct GaugesOnly;
///
/// impl Kernel for GaugesOnly {
///     fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
///         input.remove_if(|event| !matches!(event, Event::Metric(m) if m.values().as_str() == "gauge"));
///         Progress::Done
///     }
/// }
///
/// let mut kernel = GaugesOnly;
/// let (mut outputs, mut budget, mut counters) = (PortOutputs::new(1), Budget::unlimited(), TurnCounters::default());
/// let header = BatchHeader::default();
///
/// let mut input = EventsBuffer::default();
/// input.try_push(Event::Metric(Metric::gauge("up", 1.0)));
/// input.try_push(Event::Metric(Metric::counter("hits", 1.0)));
///
/// let mut turn = Turn::new(&header, Instant::now(), &mut budget, &mut outputs, &mut counters);
/// assert_eq!(kernel.consume(&mut turn, &mut input), Progress::Done);
/// outputs.complete(input);
///
/// let delivered: Vec<EventsBuffer> = outputs.drain(PortIndex::PRIMARY).collect();
/// assert_eq!(delivered.iter().map(|buffer| buffer.len()).sum::<usize>(), 1);
/// ```
/// <!-- vale on -->
///
/// # Yielding
///
/// Work that is not bounded by the size of one batch, such as draining accumulated state, checks the turn's budget and
/// returns [`Progress::Yielded`] when the budget is exhausted. The kernel keeps its own cursor, and the host calls the
/// same method again in a later turn, with a fresh budget, to resume. While any call is yielded, the host calls the
/// same method again before calling any other: a yielded `consume` is called again with the same input buffer, and no
/// batch is delivered to a kernel whose [`advance_time`][Self::advance_time], [`control`][Self::control], or
/// [`finish`][Self::finish] is yielded. Events the kernel already wrote to its ports may be delivered in between.
///
/// # Time
///
/// Kernels never decide anything by the time they run at. Per-batch decisions use the batch's admission time,
/// [`Turn::admitted_at`]. Time-driven work, such as closing buckets of accumulated state, uses the turn's
/// [watermark][Turn::watermark]: no input admitted before it can still arrive, so a bucket that ends at or before the
/// watermark can be closed. [`Turn::now`] is a monotonic clock for scheduling only.
pub trait Kernel: Send {
    /// Processes one batch.
    ///
    /// On entry, `input` holds the batch. A kernel with a single output port transforms `input` in place: whatever it
    /// leaves there is its output on the primary port. A kernel with several output ports writes events to a port with
    /// [`Turn::push`], or sends everything it leaves in `input` to a port other than the primary one with
    /// [`Turn::route_all`], which moves the whole buffer without touching its events.
    ///
    /// Events removed from `input` and not written to any port are discarded. A kernel that discards events records
    /// them as loss with [`Turn::counters`].
    ///
    /// Returns [`Progress::Done`] once the whole batch is processed, or [`Progress::WaitUntil`] to also ask for
    /// [`advance_time`][Self::advance_time] to run at a deadline. Stateless kernels always return `Done`: one batch is
    /// their unit of work. A kernel that stops early because its budget is exhausted returns [`Progress::Yielded`] and
    /// is called again later with the same `input`.
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress;

    /// Runs time-driven work, such as closing the buckets of accumulated state that the turn's watermark has passed.
    ///
    /// A bucket `[start, end)` closes only when [`Turn::watermark`] is at least `end`. Without a watermark, nothing
    /// closes: some admission point upstream has not admitted anything yet.
    ///
    /// Returns [`Progress::Done`] when nothing more is pending, [`Progress::WaitUntil`] to be called again at a
    /// deadline, or [`Progress::Yielded`] to be called again as soon as possible because the budget ran out.
    fn advance_time(&mut self, _turn: &mut Turn<'_>) -> Progress {
        Progress::Done
    }

    /// Applies a kernel-specific control message.
    ///
    /// Control messages carry requests that are not batches, such as a request for a snapshot of the kernel's state.
    /// The kernel checks whether `control` is one of the message types it understands, and ignores any other. The
    /// return value has the same meaning as for [`advance_time`][Self::advance_time].
    fn control(&mut self, _turn: &mut Turn<'_>, _control: &mut dyn Any) -> Progress {
        Progress::Done
    }

    /// Finishes the kernel after its last input, following `policy`.
    ///
    /// Under [`DrainPolicy::Retire`], the kernel emits everything it holds, whatever the watermark. Under
    /// [`DrainPolicy::Shutdown`], it applies its shutdown behavior, which may discard what it holds.
    ///
    /// Returns [`Progress::Yielded`] to be called again because the budget ran out, and anything else once finished.
    fn finish(&mut self, _turn: &mut Turn<'_>, _policy: DrainPolicy) -> Progress {
        Progress::Done
    }
}

/// What a call into a [`Kernel`] achieved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    /// The call completed all of its work.
    Done,

    /// The call stopped early because the turn's budget was exhausted.
    ///
    /// The kernel kept its cursor. The host calls the same method again, in a later turn, to resume.
    Yielded,

    /// The call completed all of its work, and the kernel has time-driven work due at the given instant.
    ///
    /// The host calls [`Kernel::advance_time`] once its clock reaches the instant.
    WaitUntil(Instant),
}

/// How a [`Kernel`] finishes when its input ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainPolicy {
    /// The process is shutting down.
    ///
    /// The kernel applies its normal shutdown behavior, which may discard state it holds, such as open buckets.
    /// Anything it discards is recorded as [`LossClass::Shutdown`].
    Shutdown,

    /// The kernel's generation is retiring while the process keeps running.
    ///
    /// The kernel emits everything it still holds, and holds nothing afterwards.
    Retire,
}

/// The kinds of work a [`Budget`] limits.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WorkKind {
    /// Events processed or emitted.
    Events,

    /// Data points processed or emitted, where one event may carry several points.
    Points,

    /// Bytes emitted, as estimated by the kernel.
    EmittedBytes,

    /// Units of work on accumulated state, such as entries visited while draining it.
    StateWork,
}

impl WorkKind {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        match self {
            Self::Events => 0,
            Self::Points => 1,
            Self::EmittedBytes => 2,
            Self::StateWork => 3,
        }
    }
}

/// The work a kernel may still do in the current turn.
///
/// A budget holds a remaining amount for each [`WorkKind`]. Kernels charge the budget as they work, and a kernel whose
/// work is not bounded by one batch yields once the budget is exhausted, which happens as soon as any one kind of work
/// has nothing left.
///
/// An unlimited amount is represented as `u64::MAX`, and in practice never runs out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    remaining: [u64; WorkKind::COUNT],
}

impl Budget {
    /// Creates a budget with no limit on any kind of work.
    pub const fn unlimited() -> Self {
        Self {
            remaining: [u64::MAX; WorkKind::COUNT],
        }
    }

    /// Returns this budget with the amount of `kind` set to `amount`.
    pub const fn with_limit(mut self, kind: WorkKind, amount: u64) -> Self {
        self.remaining[kind.index()] = amount;
        self
    }

    /// Returns the amount of `kind` that remains.
    pub const fn remaining(&self, kind: WorkKind) -> u64 {
        self.remaining[kind.index()]
    }

    /// Charges `amount` of `kind` against the budget.
    ///
    /// Charging more than remains leaves nothing of that kind, rather than wrapping around.
    pub fn charge(&mut self, kind: WorkKind, amount: u64) {
        let remaining = &mut self.remaining[kind.index()];
        *remaining = remaining.saturating_sub(amount);
    }

    /// Returns `true` if any kind of work has nothing left.
    pub fn is_exhausted(&self) -> bool {
        self.remaining.contains(&0)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// The index of a kernel's output port.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortIndex(u16);

impl PortIndex {
    /// The primary port, which receives whatever a kernel leaves in its input unless it routes it elsewhere.
    pub const PRIMARY: Self = Self(0);

    /// Creates a port index.
    pub const fn new(index: u16) -> Self {
        Self(index)
    }

    /// Returns this index as a `usize`, for indexing.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

/// The output ports of one kernel, holding the buffers it has written until its host delivers them.
///
/// Each port holds a sequence of buffers, in the order they were written. A whole input buffer moved to a port with
/// [`complete`][Self::complete] keeps its events untouched; individually written events are packed into buffers created
/// on demand.
///
/// # Panics
///
/// Writing to a port that does not exist is a bug in the kernel, and panics. A host runs kernels under supervision, so
/// such a panic fails the processing step rather than silently losing events.
#[derive(Debug)]
pub struct PortOutputs {
    ports: Vec<Vec<EventsBuffer>>,
    remainder: PortIndex,
}

impl PortOutputs {
    /// Creates outputs with `port_count` ports, all empty.
    pub fn new(port_count: usize) -> Self {
        Self {
            ports: (0..port_count).map(|_| Vec::new()).collect(),
            remainder: PortIndex::PRIMARY,
        }
    }

    /// Returns the number of ports.
    pub fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// Writes one event to `port`, after any events already written to it.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn push(&mut self, port: PortIndex, event: Event) {
        let buffers = self.port_mut(port);
        let event = match buffers.last_mut() {
            Some(buffer) => match buffer.try_push(event) {
                None => return,
                Some(event) => event,
            },
            None => event,
        };

        let mut buffer = FixedSizeEventBuffer::with_capacity(PORT_BUFFER_INITIAL_CAPACITY);
        let overflow = buffer.try_push(event);
        debug_assert!(overflow.is_none(), "a new buffer always has room for one event");
        buffers.push(buffer);
    }

    /// Writes a whole buffer to `port`, after any events already written to it, without touching its events.
    ///
    /// An empty buffer is dropped.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn push_buffer(&mut self, port: PortIndex, buffer: EventsBuffer) {
        let buffers = self.port_mut(port);
        if !buffer.is_empty() {
            buffers.push(buffer);
        }
    }

    /// Makes room for `additional` more events on `port`, so that writing them one by one with [`push`][Self::push]
    /// allocates once instead of growing a buffer step by step.
    ///
    /// Room is made in the port's last buffer, or, if it has none with room left, in a new one, up to the capacity of
    /// one buffer. Reserving room for no event does nothing.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn reserve(&mut self, port: PortIndex, additional: usize) {
        let buffers = self.port_mut(port);
        if additional == 0 {
            return;
        }
        match buffers.last_mut() {
            Some(buffer) if !buffer.is_full() => buffer.reserve(additional),
            _ => buffers.push(FixedSizeEventBuffer::with_capacity(additional)),
        }
    }

    /// Sends the events the kernel leaves in its input to `port` instead of the primary port.
    ///
    /// The choice applies to the next call to [`complete`][Self::complete], which resets it to the primary port.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn route_all(&mut self, port: PortIndex) {
        let _ = self.port_mut(port);
        self.remainder = port;
    }

    /// Returns the port that the events left in the input go to, as chosen with [`route_all`][Self::route_all].
    pub fn remainder_port(&self) -> PortIndex {
        self.remainder
    }

    /// Completes a batch: moves the events the kernel left in `input` to their port.
    ///
    /// The buffer is moved as a whole, after any events written to the same port individually, so its events are not
    /// touched. An empty buffer is dropped. The port choice made with [`route_all`][Self::route_all] is then reset to
    /// the primary port.
    ///
    /// Hosts call this after every call to [`Kernel::consume`] that does not return [`Progress::Yielded`].
    ///
    /// # Panics
    ///
    /// Panics if `input` still holds events and the chosen port does not exist, which happens when a kernel with no
    /// output ports leaves events in its input.
    pub fn complete(&mut self, input: EventsBuffer) {
        let port = std::mem::replace(&mut self.remainder, PortIndex::PRIMARY);
        if !input.is_empty() {
            self.port_mut(port).push(input);
        }
    }

    /// Returns `true` if no port holds any buffer.
    pub fn is_empty(&self) -> bool {
        self.ports.iter().all(|buffers| buffers.is_empty())
    }

    /// Returns the number of events held on `port`.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn event_count(&self, port: PortIndex) -> usize {
        self.port(port).iter().map(|buffer| buffer.len()).sum()
    }

    /// Removes and returns the buffers held on `port`, in the order they were written.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn drain(&mut self, port: PortIndex) -> std::vec::Drain<'_, EventsBuffer> {
        self.port_mut(port).drain(..)
    }

    /// Discards every buffer held on every port, and resets the port choice made with [`route_all`][Self::route_all].
    pub fn clear(&mut self) {
        for buffers in &mut self.ports {
            buffers.clear();
        }
        self.remainder = PortIndex::PRIMARY;
    }

    fn port(&self, port: PortIndex) -> &Vec<EventsBuffer> {
        let count = self.ports.len();
        self.ports
            .get(port.get())
            .unwrap_or_else(|| panic!("kernel wrote to port {} but has only {} ports", port.get(), count))
    }

    fn port_mut(&mut self, port: PortIndex) -> &mut Vec<EventsBuffer> {
        let count = self.ports.len();
        self.ports
            .get_mut(port.get())
            .unwrap_or_else(|| panic!("kernel wrote to port {} but has only {} ports", port.get(), count))
    }
}

/// Why events failed to reach any output.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LossClass {
    /// Discarded on purpose by a configured or approved rule, such as a filter.
    Policy,

    /// Discarded because a limit was reached, such as a cap on tracked state.
    Capacity,

    /// Discarded because the kernel could not process the event.
    Invalid,

    /// Discarded while shutting down.
    Shutdown,
}

impl LossClass {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        match self {
            Self::Policy => 0,
            Self::Capacity => 1,
            Self::Invalid => 2,
            Self::Shutdown => 3,
        }
    }
}

/// An amount of loss, in events and in the data points those events carried.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LossCount {
    /// The number of events lost.
    pub events: u64,

    /// The number of data points the lost events carried.
    pub points: u64,
}

/// Counters a kernel accumulates during its turns.
///
/// Kernels record into plain fields here instead of into shared telemetry, so recording costs an addition. The host
/// collects the counters with [`take`][Self::take] and publishes them off the data path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnCounters {
    loss: [LossCount; LossClass::COUNT],
}

impl TurnCounters {
    /// Records the loss of `events` events, carrying `points` data points in total, as `class`.
    pub fn record_loss(&mut self, class: LossClass, events: u64, points: u64) {
        let count = &mut self.loss[class.index()];
        count.events = count.events.saturating_add(events);
        count.points = count.points.saturating_add(points);
    }

    /// Returns the loss recorded as `class`.
    pub fn loss(&self, class: LossClass) -> LossCount {
        self.loss[class.index()]
    }

    /// Returns `true` if nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Returns the counters recorded so far, and resets them.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

/// The context of one call into a [`Kernel`].
///
/// A turn borrows the state its host keeps for the kernel: the output ports, the work budget, and the counters. It also
/// carries the header of the batch being processed and the host's clock reading for the turn. A turn does not depend on
/// any topology, so anything can drive a kernel by building one with [`Turn::new`].
pub struct Turn<'a> {
    header: &'a BatchHeader,
    watermark: Option<AdmissionTime>,
    now: Instant,
    budget: &'a mut Budget,
    outputs: &'a mut PortOutputs,
    counters: &'a mut TurnCounters,
}

impl<'a> Turn<'a> {
    /// Creates a turn.
    ///
    /// `header` is the header of the batch being processed. For calls that are not about one batch, such as
    /// [`Kernel::advance_time`], the host passes the header its time-driven output is stamped with. `now` is the host's
    /// monotonic clock reading for the turn.
    ///
    /// The turn has no [watermark][Self::watermark]; hosts set one with [`with_watermark`][Self::with_watermark] for
    /// time-driven calls.
    pub fn new(
        header: &'a BatchHeader, now: Instant, budget: &'a mut Budget, outputs: &'a mut PortOutputs,
        counters: &'a mut TurnCounters,
    ) -> Self {
        Self {
            header,
            watermark: None,
            now,
            budget,
            outputs,
            counters,
        }
    }

    /// Returns this turn with the given watermark.
    ///
    /// Hosts set the kernel's watermark for the calls that do time-driven work: [`Kernel::advance_time`] and
    /// [`Kernel::finish`]. `None` means some admission point upstream has not admitted anything yet.
    pub fn with_watermark(mut self, watermark: Option<AdmissionTime>) -> Self {
        self.watermark = watermark;
        self
    }

    /// Returns the header of the batch being processed.
    pub fn header(&self) -> &BatchHeader {
        self.header
    }

    /// Returns the admission time of the batch being processed, if it was stamped.
    ///
    /// Kernels that make decisions based on host time use this, never the time at which they run.
    pub fn admitted_at(&self) -> Option<AdmissionTime> {
        self.header.admitted_at
    }

    /// Returns the kernel's watermark, if the host knows one: the earliest of the latest admission times of the
    /// admission points upstream of the kernel.
    ///
    /// No input admitted before the watermark can still arrive, so time-driven work closes exactly the buckets that end
    /// at or before it. `None` means some admission point upstream has not admitted anything yet, so nothing can be
    /// closed. Watermarks only move forward, and hosts set them only for time-driven calls; see
    /// [`with_watermark`][Self::with_watermark].
    pub fn watermark(&self) -> Option<AdmissionTime> {
        self.watermark
    }

    /// Returns the host's monotonic clock reading for this turn.
    ///
    /// This is for scheduling only, such as computing the deadline in [`Progress::WaitUntil`]. It never decides which
    /// data belongs where.
    pub fn now(&self) -> Instant {
        self.now
    }

    /// Returns the work budget that remains in this turn.
    pub fn budget(&self) -> &Budget {
        self.budget
    }

    /// Returns the work budget mutably, so the kernel can charge it.
    pub fn budget_mut(&mut self) -> &mut Budget {
        self.budget
    }

    /// Returns the number of output ports.
    pub fn port_count(&self) -> usize {
        self.outputs.port_count()
    }

    /// Writes one event to `port`.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn push(&mut self, port: PortIndex, event: Event) {
        self.outputs.push(port, event);
    }

    /// Writes a whole buffer to `port`, without touching its events; see [`PortOutputs::push_buffer`].
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn push_buffer(&mut self, port: PortIndex, buffer: EventsBuffer) {
        self.outputs.push_buffer(port, buffer);
    }

    /// Makes room for `additional` more events on `port`; see [`PortOutputs::reserve`].
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn reserve(&mut self, port: PortIndex, additional: usize) {
        self.outputs.reserve(port, additional);
    }

    /// Sends the events left in the input to `port` instead of the primary port, without touching them.
    ///
    /// # Panics
    ///
    /// Panics if `port` does not exist.
    pub fn route_all(&mut self, port: PortIndex) {
        self.outputs.route_all(port);
    }

    /// Returns the counters for this kernel.
    pub fn counters(&mut self) -> &mut TurnCounters {
        self.counters
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::data_model::event::metric::Metric;

    const SECONDARY: PortIndex = PortIndex::new(1);

    fn metric_event(name: &'static str) -> Event {
        Event::Metric(Metric::gauge(name, 1.0))
    }

    fn buffer_of(names: &[&'static str]) -> EventsBuffer {
        let mut buffer = EventsBuffer::with_capacity(names.len());
        for name in names {
            assert!(buffer.try_push(metric_event(name)).is_none());
        }
        buffer
    }

    /// Returns the metric names held on `port`, in order, draining it.
    fn drain_names(outputs: &mut PortOutputs, port: PortIndex) -> Vec<String> {
        outputs
            .drain(port)
            .flat_map(|buffer| buffer.into_iter())
            .map(|event| match event {
                Event::Metric(metric) => metric.context().name().to_string(),
                other => panic!("unexpected event: {:?}", other),
            })
            .collect()
    }

    /// Host-side state for driving one kernel in tests, mirroring what a real host keeps.
    struct Harness {
        header: BatchHeader,
        now: Instant,
        budget: Budget,
        outputs: PortOutputs,
        counters: TurnCounters,
    }

    impl Harness {
        fn new(port_count: usize) -> Self {
            Self {
                header: BatchHeader::default(),
                now: Instant::now(),
                budget: Budget::unlimited(),
                outputs: PortOutputs::new(port_count),
                counters: TurnCounters::default(),
            }
        }

        /// Runs `consume` once and, unless it yielded, completes the batch as a host does.
        fn consume(&mut self, kernel: &mut dyn Kernel, input: &mut EventsBuffer) -> Progress {
            let mut turn = Turn::new(
                &self.header,
                self.now,
                &mut self.budget,
                &mut self.outputs,
                &mut self.counters,
            );
            let progress = kernel.consume(&mut turn, input);
            if progress != Progress::Yielded {
                self.outputs
                    .complete(std::mem::replace(input, EventsBuffer::with_capacity(0)));
            }
            progress
        }

        fn advance_time(&mut self, kernel: &mut dyn Kernel) -> Progress {
            let mut turn = Turn::new(
                &self.header,
                self.now,
                &mut self.budget,
                &mut self.outputs,
                &mut self.counters,
            );
            kernel.advance_time(&mut turn)
        }
    }

    /// Drops metrics whose name starts with `drop.`, recording them as policy loss.
    struct DropFilter;

    impl Kernel for DropFilter {
        fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
            let before = input.len();
            input.remove_if(
                |event| matches!(event, Event::Metric(metric) if metric.context().name().starts_with("drop.")),
            );
            let dropped = (before - input.len()) as u64;
            turn.counters().record_loss(LossClass::Policy, dropped, dropped);
            Progress::Done
        }
    }

    /// Sends metrics whose name starts with `b.` to the secondary port, and everything else to the primary port. A
    /// batch made only of `b.` metrics takes the whole-buffer fast path.
    struct Splitter;

    impl Kernel for Splitter {
        fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
            let is_b =
                |event: &Event| matches!(event, Event::Metric(metric) if metric.context().name().starts_with("b."));
            if (&*input).into_iter().all(is_b) {
                turn.route_all(SECONDARY);
                return Progress::Done;
            }

            let events = std::mem::replace(input, EventsBuffer::with_capacity(input.len()));
            for event in events {
                if is_b(&event) {
                    turn.push(SECONDARY, event);
                } else {
                    assert!(input.try_push(event).is_none());
                }
            }
            Progress::Done
        }
    }

    /// Holds a number of pending items and emits them while its event budget lasts.
    struct Drainer {
        pending: usize,
    }

    impl Kernel for Drainer {
        fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
            Progress::Done
        }

        fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
            while self.pending > 0 {
                if turn.budget().is_exhausted() {
                    return Progress::Yielded;
                }
                self.pending -= 1;
                turn.push(PortIndex::PRIMARY, metric_event("drained"));
                turn.budget_mut().charge(WorkKind::Events, 1);
            }
            Progress::WaitUntil(turn.now() + Duration::from_secs(10))
        }
    }

    #[test]
    fn in_place_kernels_deliver_their_input_on_the_primary_port() {
        let mut harness = Harness::new(1);
        let mut input = buffer_of(&["keep.a", "drop.b", "keep.c", "drop.d"]);

        assert_eq!(harness.consume(&mut DropFilter, &mut input), Progress::Done);
        let mut kept = drain_names(&mut harness.outputs, PortIndex::PRIMARY);
        kept.sort();
        assert_eq!(kept, vec!["keep.a", "keep.c"]);

        let loss = harness.counters.loss(LossClass::Policy);
        assert_eq!(loss, LossCount { events: 2, points: 2 });
        assert_eq!(harness.counters.loss(LossClass::Invalid), LossCount::default());
    }

    #[test]
    fn route_all_moves_the_whole_input_buffer_to_another_port() {
        let mut harness = Harness::new(2);
        let mut input = buffer_of(&["b.1", "b.2"]);

        assert_eq!(harness.consume(&mut Splitter, &mut input), Progress::Done);
        assert_eq!(harness.outputs.event_count(PortIndex::PRIMARY), 0);
        let buffers = harness.outputs.drain(SECONDARY).collect::<Vec<_>>();
        assert_eq!(buffers.len(), 1, "the input buffer is moved as a whole");
        assert_eq!(buffers[0].capacity(), EventsBuffer::default().capacity());
        assert_eq!(buffers[0].len(), 2);

        // The choice applies to one batch only: the next batch's remainder goes back to the primary port.
        let mut input = buffer_of(&["a.1"]);
        harness.consume(&mut Splitter, &mut input);
        assert_eq!(drain_names(&mut harness.outputs, PortIndex::PRIMARY), vec!["a.1"]);
        assert!(harness.outputs.is_empty());
    }

    #[test]
    fn pushed_events_and_the_remainder_reach_their_ports_in_order() {
        let mut harness = Harness::new(2);
        let mut input = buffer_of(&["a.1", "b.1", "a.2", "b.2", "b.3"]);

        harness.consume(&mut Splitter, &mut input);
        assert_eq!(
            drain_names(&mut harness.outputs, PortIndex::PRIMARY),
            vec!["a.1", "a.2"]
        );
        assert_eq!(drain_names(&mut harness.outputs, SECONDARY), vec!["b.1", "b.2", "b.3"]);
    }

    #[test]
    fn pushed_events_fill_buffers_up_to_their_capacity() {
        let mut outputs = PortOutputs::new(1);
        let capacity = EventsBuffer::default().capacity();
        for _ in 0..capacity + 1 {
            outputs.push(PortIndex::PRIMARY, metric_event("x"));
        }

        let lengths = outputs
            .drain(PortIndex::PRIMARY)
            .map(|buffer| buffer.len())
            .collect::<Vec<_>>();
        assert_eq!(lengths, vec![capacity, 1]);
    }

    #[test]
    fn empty_remainders_are_dropped() {
        let mut outputs = PortOutputs::new(1);
        outputs.complete(EventsBuffer::with_capacity(0));
        assert!(outputs.is_empty());
    }

    #[test]
    fn clear_discards_buffers_and_the_port_choice() {
        let mut outputs = PortOutputs::new(2);
        outputs.push(SECONDARY, metric_event("x"));
        outputs.route_all(SECONDARY);
        outputs.clear();
        assert!(outputs.is_empty());
        assert_eq!(outputs.remainder_port(), PortIndex::PRIMARY);
    }

    #[test]
    #[should_panic(expected = "kernel wrote to port 2 but has only 2 ports")]
    fn writing_to_a_missing_port_panics() {
        let mut outputs = PortOutputs::new(2);
        outputs.push(PortIndex::new(2), metric_event("x"));
    }

    #[test]
    #[should_panic(expected = "kernel wrote to port 0 but has only 0 ports")]
    fn leaving_events_without_an_output_port_panics() {
        let mut outputs = PortOutputs::new(0);
        outputs.complete(buffer_of(&["x"]));
    }

    #[test]
    fn yielded_work_resumes_where_it_stopped() {
        let mut kernel = Drainer { pending: 5 };
        let mut harness = Harness::new(1);

        // Two events fit in each turn's budget.
        harness.budget = Budget::unlimited().with_limit(WorkKind::Events, 2);
        assert_eq!(harness.advance_time(&mut kernel), Progress::Yielded);
        assert_eq!(harness.outputs.event_count(PortIndex::PRIMARY), 2);

        harness.budget = Budget::unlimited().with_limit(WorkKind::Events, 2);
        assert_eq!(harness.advance_time(&mut kernel), Progress::Yielded);
        assert_eq!(harness.outputs.event_count(PortIndex::PRIMARY), 4);

        harness.budget = Budget::unlimited().with_limit(WorkKind::Events, 2);
        assert_eq!(
            harness.advance_time(&mut kernel),
            Progress::WaitUntil(harness.now + Duration::from_secs(10))
        );
        assert_eq!(harness.outputs.event_count(PortIndex::PRIMARY), 5);
        assert_eq!(harness.budget.remaining(WorkKind::Events), 1);
    }

    #[test]
    fn default_kernel_methods_complete_immediately() {
        let mut harness = Harness::new(1);
        let mut turn = Turn::new(
            &harness.header,
            harness.now,
            &mut harness.budget,
            &mut harness.outputs,
            &mut harness.counters,
        );
        let mut kernel = DropFilter;
        assert_eq!(kernel.advance_time(&mut turn), Progress::Done);
        assert_eq!(kernel.control(&mut turn, &mut ()), Progress::Done);
        assert_eq!(kernel.finish(&mut turn, DrainPolicy::Shutdown), Progress::Done);
        assert!(harness.outputs.is_empty());
    }

    #[test]
    fn budget_is_exhausted_when_any_kind_runs_out() {
        let mut budget = Budget::unlimited()
            .with_limit(WorkKind::Points, 10)
            .with_limit(WorkKind::StateWork, 3);
        assert!(!budget.is_exhausted());

        budget.charge(WorkKind::Points, 4);
        budget.charge(WorkKind::StateWork, 2);
        assert_eq!(budget.remaining(WorkKind::Points), 6);
        assert_eq!(budget.remaining(WorkKind::StateWork), 1);
        assert_eq!(budget.remaining(WorkKind::Events), u64::MAX);
        assert!(!budget.is_exhausted());

        // Overcharging saturates at zero, which exhausts the budget.
        budget.charge(WorkKind::StateWork, 100);
        assert_eq!(budget.remaining(WorkKind::StateWork), 0);
        assert!(budget.is_exhausted());

        assert_eq!(Budget::default(), Budget::unlimited());
    }

    #[test]
    fn turn_counters_accumulate_per_class_and_reset_on_take() {
        let mut counters = TurnCounters::default();
        assert!(counters.is_empty());

        counters.record_loss(LossClass::Capacity, 1, 3);
        counters.record_loss(LossClass::Capacity, 2, 2);
        counters.record_loss(LossClass::Shutdown, 1, 1);
        assert_eq!(counters.loss(LossClass::Capacity), LossCount { events: 3, points: 5 });
        assert_eq!(counters.loss(LossClass::Shutdown), LossCount { events: 1, points: 1 });
        assert_eq!(counters.loss(LossClass::Policy), LossCount::default());

        let taken = counters.take();
        assert!(counters.is_empty());
        assert_eq!(taken.loss(LossClass::Capacity), LossCount { events: 3, points: 5 });
    }

    #[test]
    fn turn_exposes_the_header_and_clock_it_was_built_with() {
        let header = BatchHeader {
            admitted_at: Some(AdmissionTime::from_unix_secs(42)),
            ..BatchHeader::default()
        };
        let now = Instant::now();
        let mut budget = Budget::unlimited().with_limit(WorkKind::Events, 7);
        let mut outputs = PortOutputs::new(3);
        let mut counters = TurnCounters::default();
        let turn = Turn::new(&header, now, &mut budget, &mut outputs, &mut counters);

        assert_eq!(turn.admitted_at(), Some(AdmissionTime::from_unix_secs(42)));
        assert_eq!(turn.header().epoch, 0);
        assert_eq!(turn.now(), now);
        assert_eq!(turn.budget().remaining(WorkKind::Events), 7);
        assert_eq!(turn.port_count(), 3);
        // A turn has no watermark unless its host sets one for time-driven work.
        assert_eq!(turn.watermark(), None);

        let watermark = Some(AdmissionTime::from_unix_secs(40));
        let turn = Turn::new(&header, now, &mut budget, &mut outputs, &mut counters).with_watermark(watermark);
        assert_eq!(turn.watermark(), watermark);
        assert_eq!(turn.admitted_at(), Some(AdmissionTime::from_unix_secs(42)));
    }
}
