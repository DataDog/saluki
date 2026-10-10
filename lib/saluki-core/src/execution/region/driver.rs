//! The contract between a region host and the driver that runs a region's programs.

use std::{fmt, time::Instant};

use saluki_error::GenericError;

use crate::{
    execution::{Budget, DrainPolicy, Progress},
    topology::EventsBuffer,
};

/// The synchronous program of one region, driven by a region host.
///
/// A region is one component of a running topology that fuses a chain of synchronous processing steps. The host owns
/// everything asynchronous: the input stream, the outputs (called **exits**), timers, liveness, the control mailbox,
/// telemetry pacing, and supervision. The driver owns everything else: the programs of the generations it runs, their
/// kernels, and the state between calls. Every method is synchronous and does bounded work, so the host stays
/// responsive.
///
/// The host calls the driver in **turns**. Each call that runs kernel code receives a [`RegionTurn`] with the host's
/// clock reading and a fresh work [`Budget`], and an [`ExitSink`] to write output to. The host dispatches what the
/// call wrote, in the order it was written, after the call returns.
///
/// # Yielding
///
/// [`process`][Self::process], [`advance_time`][Self::advance_time], and [`finish`][Self::finish] may return
/// [`Progress::Yielded`] when their budget runs out. The host then dispatches what the call wrote so far and calls
/// [`resume`][Self::resume], in later turns, until it returns anything else. While a call is yielded, the host calls
/// no other method except [`control`][Self::control] and [`publish_telemetry`][Self::publish_telemetry]: no batch is
/// processed, and neither time-driven work nor finishing starts.
///
/// [`Progress::WaitUntil`] means the same as [`Progress::Done`] to the host: the deadline that counts is the one
/// [`next_deadline`][Self::next_deadline] returns, which the host reads after every call.
///
/// # Supervision
///
/// The host runs every method that can run kernel code under [`catch_unwind`][std::panic::catch_unwind]. When one
/// panics, the host discards whatever the call wrote to the exit sink, counts the lost events, and calls
/// [`reset_after_panic`][Self::reset_after_panic], which decides whether the region continues or fails.
/// [`start`][Self::start], [`accepts_input`][Self::accepts_input], and [`next_deadline`][Self::next_deadline] only
/// set or read driver state: they must not run kernel code, and a panic in any of them, or in `reset_after_panic`
/// itself, fails the component.
pub trait RegionDriver: Send + 'static {
    /// The control messages this driver understands, such as preparing or aborting a generation.
    type Control: Send + 'static;

    /// Prepares the driver to run, once the region has started.
    ///
    /// The host calls this once, after it reports the region ready and before it calls anything else, with the time
    /// the region started. A driver that schedules time-driven work measures its first deadline from here rather than
    /// from when it was built, which can be much earlier. It must not run kernel code, and writes nothing.
    ///
    /// The default does nothing.
    fn start(&mut self, _turn: &mut RegionTurn<'_>) {}

    /// Returns `true` if the driver can take another input batch now.
    ///
    /// The host polls its input only while this returns `true` and no call is yielded. A driver returns `false`, for
    /// example, while a drain of accumulated state is pending, so that input waits upstream. The host reads it before
    /// each wait, so it may change only as a result of a call into the driver.
    ///
    /// The default always accepts input.
    fn accepts_input(&self) -> bool {
        true
    }

    /// Processes one input batch.
    ///
    /// The batch's header travels inside `batch`; the driver takes it out before running kernels, and stamps every
    /// buffer it writes to `exits`. The host never changes headers.
    ///
    /// Returns [`Progress::Yielded`] if the driver stopped before finishing the batch because the turn's budget ran
    /// out; it keeps the rest of the batch and continues in [`resume`][Self::resume].
    fn process(&mut self, turn: &mut RegionTurn<'_>, batch: EventsBuffer, exits: &mut ExitSink) -> Progress;

    /// Continues the call that last returned [`Progress::Yielded`].
    ///
    /// Returns [`Progress::Yielded`] again to be resumed in a later turn, and anything else once the yielded call is
    /// complete.
    ///
    /// The default returns [`Progress::Done`]. Drivers that ever yield must implement it.
    fn resume(&mut self, _turn: &mut RegionTurn<'_>, _exits: &mut ExitSink) -> Progress {
        Progress::Done
    }

    /// Applies a control message.
    ///
    /// The host calls this whenever a message arrives, including while an exit send is blocked downstream and while
    /// another call is yielded, so it must only touch state that no yielded call depends on, such as the table of
    /// prepared programs. The host acknowledges the message to its sender with the result, as soon as this returns.
    ///
    /// # Errors
    ///
    /// If the driver cannot apply the message, it returns a [`RegionFailure`] describing why, which the sender receives
    /// as the acknowledgment. The region keeps running.
    fn control(
        &mut self, turn: &mut RegionTurn<'_>, message: Self::Control, exits: &mut ExitSink,
    ) -> Result<(), RegionFailure>;

    /// Returns the instant at which the driver wants [`advance_time`][Self::advance_time] to run, if any.
    ///
    /// The host reads this after every call. The deadline is on the clock of [`RegionTurn::now`]. A deadline that has
    /// passed is due at once, as the next deadline of a timer that bursts to catch up: one that passed while a call
    /// was yielded is due as soon as the call completes, and one that `advance_time` returns has the host call
    /// `advance_time` again immediately.
    ///
    /// After `advance_time` runs, the work that was due at the deadline that made the host call it is done, or is due
    /// later than that deadline. A driver that skips scheduled work therefore moves its deadline forward, for example
    /// by its period, which keeps its phase and, if that deadline has also passed, catches up one period per call. It
    /// never keeps the deadline that fired, and never moves back and forth between past deadlines, either of which
    /// would make the host call it again and again while nothing changes. The next deadline may still be earlier than
    /// the one that fired when other work is due: for example, once a driver has completed work that a panic ended,
    /// which it scheduled for the time of the panic, its next deadline can be that of work that was due before.
    ///
    /// The host checks the likeliest mistake: an `advance_time` that did no work, writing nothing to its exit sink and
    /// charging nothing to its budget, and kept the deadline that fired. It logs an error, does not call `advance_time`
    /// for that deadline again until the driver's deadline changes, and a debug build fails an assertion.
    ///
    /// The default has no deadline.
    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    /// Runs time-driven work once the deadline from [`next_deadline`][Self::next_deadline] has passed.
    ///
    /// The default does nothing.
    fn advance_time(&mut self, _turn: &mut RegionTurn<'_>, _exits: &mut ExitSink) -> Progress {
        Progress::Done
    }

    /// Finishes the region after its input has ended, following `policy`.
    ///
    /// The host calls this with [`DrainPolicy::Shutdown`] after its input stream closes and any yielded call has
    /// completed, resumes it until it returns anything other than [`Progress::Yielded`], and then stops. After the
    /// input ends, the host runs no time-driven work, even for a deadline that has passed: the driver completes
    /// whatever it still owes, such as work a panic ended, as part of finishing.
    ///
    /// If a panic ends the finish before it completes, whether in this call, in [`resume`][Self::resume], or in a call
    /// the host makes while the finish is yielded, and [`reset_after_panic`][Self::reset_after_panic] lets the region
    /// continue, the host calls `finish` again with the same policy, and so on until a call completes it. A call after
    /// such a panic resumes the work the panic ended rather than starting over, so that what the panic did not lose is
    /// still delivered. Since the host calls `finish` again after every panic the driver recovers from, each such
    /// recovery must leave less to finish, for example by dropping the state that panicked, or fail the region.
    ///
    /// The default does nothing.
    fn finish(&mut self, _turn: &mut RegionTurn<'_>, _exits: &mut ExitSink, _policy: DrainPolicy) -> Progress {
        Progress::Done
    }

    /// Recovers from a panic in one of the driver's calls.
    ///
    /// When this is called, whatever the failed call wrote to its exit sink has been discarded, and the host has
    /// counted [`RegionPanic::lost_events`] as lost. The driver rebuilds whatever the panic may have left inconsistent,
    /// typically every kernel of every live generation and all state derived from them, accounts for any state it lost,
    /// and decides what happens next:
    ///
    /// - [`PanicOutcome::Continue`]: the region keeps running. Any yielded call is over, whichever call panicked: the
    ///   host does not resume it. If the region was finishing and the finish had not completed, the host calls
    ///   [`finish`][Self::finish] again, which resumes the work the panic ended.
    /// - [`PanicOutcome::Fail`]: the host stops, and the component fails with the given error, exactly as any other
    ///   component failure does.
    fn reset_after_panic(&mut self, panic: &RegionPanic) -> PanicOutcome;

    /// Publishes the telemetry the driver accumulated since it last published.
    ///
    /// The host calls this at most once per second while the region is busy, after every control message, and once
    /// when the region stops, always off the processing path of any batch. The driver records into plain counters
    /// during its calls and copies them to shared telemetry here.
    ///
    /// The default does nothing.
    fn publish_telemetry(&mut self) {}
}

/// The context of one call from a region host into its driver.
///
/// A turn carries the host's clock reading and the work budget for the call. A driver passes both on to the kernels it
/// runs, through [`Turn::new`][crate::execution::Turn::new], so that every kernel in the call charges one budget.
///
/// Nothing in a turn depends on a running topology, so anything, such as a test or a harness that wires several
/// regions together synchronously, can drive a [`RegionDriver`] by building turns with [`RegionTurn::new`].
pub struct RegionTurn<'a> {
    now: Instant,
    budget: &'a mut Budget,
}

impl<'a> RegionTurn<'a> {
    /// Creates a turn at `now` with the given budget.
    pub fn new(now: Instant, budget: &'a mut Budget) -> Self {
        Self { now, budget }
    }

    /// Returns the host's monotonic clock reading for this turn.
    ///
    /// This is the clock that [`RegionDriver::next_deadline`] and [`Progress::WaitUntil`] are measured on. It is for
    /// scheduling only and never decides which data belongs where.
    pub fn now(&self) -> Instant {
        self.now
    }

    /// Returns the work budget that remains in this turn.
    pub fn budget(&self) -> &Budget {
        self.budget
    }

    /// Returns the work budget mutably, so the driver, or a kernel it runs, can charge it.
    pub fn budget_mut(&mut self) -> &mut Budget {
        self.budget
    }
}

/// The index of one of a region's exits.
///
/// A region's exits are its outputs, in the order they were given to the region host. Each exit is a named output of
/// the region's component.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExitId(u16);

impl ExitId {
    /// Creates an exit identifier.
    pub const fn new(index: u16) -> Self {
        Self(index)
    }

    /// Returns this identifier as a `usize`, for indexing.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for ExitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The buffers one call into a [`RegionDriver`] writes to the region's exits.
///
/// The sink keeps every buffer in the order it was written, across all exits, and the host dispatches them in that
/// order, so each exit receives its buffers in the order the driver wrote them. Buffers are dispatched as they are:
/// the driver sets their headers, and an empty buffer is dispatched like any other, so that header-only batches, such
/// as progress batches, reach downstream.
///
/// # Panics
///
/// Writing to an exit that does not exist is a bug in the driver, and panics. The host runs drivers under
/// supervision, so such a panic is handled like any other driver panic.
#[derive(Debug)]
pub struct ExitSink {
    exit_count: usize,
    buffers: Vec<(ExitId, EventsBuffer)>,
}

impl ExitSink {
    /// Creates an empty sink for a region with `exit_count` exits.
    pub fn new(exit_count: usize) -> Self {
        Self {
            exit_count,
            buffers: Vec::new(),
        }
    }

    /// Returns the number of exits.
    pub fn exit_count(&self) -> usize {
        self.exit_count
    }

    /// Writes `buffer` to `exit`, after everything already written.
    ///
    /// # Panics
    ///
    /// Panics if `exit` does not exist.
    pub fn emit(&mut self, exit: ExitId, buffer: EventsBuffer) {
        assert!(
            exit.get() < self.exit_count,
            "region wrote to exit {} but has only {} exits",
            exit,
            self.exit_count
        );
        self.buffers.push((exit, buffer));
    }

    /// Returns `true` if nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    /// Returns the number of buffers written.
    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    /// Returns the number of events in all buffers written.
    pub fn event_count(&self) -> usize {
        self.buffers.iter().map(|(_, buffer)| buffer.len()).sum()
    }

    /// Removes and returns the buffers written, in the order they were written.
    pub fn drain(&mut self) -> std::vec::Drain<'_, (ExitId, EventsBuffer)> {
        self.buffers.drain(..)
    }

    /// Discards everything written.
    pub fn clear(&mut self) {
        self.buffers.clear();
    }
}

/// The driver method a panic happened in.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PanicSite {
    /// [`RegionDriver::process`].
    Process,

    /// [`RegionDriver::resume`].
    Resume,

    /// [`RegionDriver::control`].
    Control,

    /// [`RegionDriver::advance_time`].
    AdvanceTime,

    /// [`RegionDriver::finish`].
    Finish,

    /// [`RegionDriver::publish_telemetry`].
    PublishTelemetry,
}

impl PanicSite {
    /// Returns the name of the driver method.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Resume => "resume",
            Self::Control => "control",
            Self::AdvanceTime => "advance_time",
            Self::Finish => "finish",
            Self::PublishTelemetry => "publish_telemetry",
        }
    }
}

impl fmt::Display for PanicSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A panic caught in a call into a [`RegionDriver`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionPanic {
    site: PanicSite,
    message: String,
    lost_events: u64,
}

impl RegionPanic {
    /// Creates a description of a caught panic.
    pub fn new(site: PanicSite, message: impl Into<String>, lost_events: u64) -> Self {
        Self {
            site,
            message: message.into(),
            lost_events,
        }
    }

    /// Returns the driver method that panicked.
    pub fn site(&self) -> PanicSite {
        self.site
    }

    /// Returns the panic's message, or a placeholder if the panic carried something other than a string.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the number of events the host counted as lost because of the panic.
    ///
    /// For [`PanicSite::Process`], this is the number of events in the batch passed to the call, counted before the
    /// call. For [`PanicSite::Resume`] of a yielded `process`, it is the number of events of that same batch: an upper
    /// bound, since earlier turns may have delivered part of it. For any other call, it is the number of events the
    /// call had written to its exit sink, which were discarded, plus the number of events of the batch of a yielded
    /// `process` call, which the panic ends. Events held in kernel state are not included; the driver accounts for
    /// them when it rebuilds.
    pub fn lost_events(&self) -> u64 {
        self.lost_events
    }
}

impl fmt::Display for RegionPanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "region panicked in {}: {}", self.site, self.message)
    }
}

/// What a region does after recovering from a panic.
#[derive(Debug)]
pub enum PanicOutcome {
    /// The region keeps running.
    Continue,

    /// The region stops, and its component fails with this error.
    Fail(GenericError),
}

/// Why a driver could not apply a control message.
///
/// A failure is reported to the sender of the message as its acknowledgment; the region keeps running.
#[derive(Debug)]
pub struct RegionFailure {
    error: GenericError,
}

impl RegionFailure {
    /// Creates a failure from the error that caused it.
    pub fn new(error: GenericError) -> Self {
        Self { error }
    }

    /// Returns the error that caused the failure.
    pub fn error(&self) -> &GenericError {
        &self.error
    }

    /// Consumes the failure and returns the error that caused it.
    pub fn into_error(self) -> GenericError {
        self.error
    }
}

impl From<GenericError> for RegionFailure {
    fn from(error: GenericError) -> Self {
        Self::new(error)
    }
}

impl fmt::Display for RegionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The alternate form includes the error's chain of causes.
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for RegionFailure {}
