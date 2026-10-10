//! Execution contracts for plan-driven processing.
//!
//! These types define how synchronous processing steps are driven, independently of any particular topology or of the
//! data they process:
//!
//! - [`BatchHeader`]: per-batch metadata stamped where a batch enters plan-driven processing, carrying its admission
//!   time and the generation that processes it.
//! - [`AdmissionPoint`], [`AdmissionClock`], and [`AdmissionSchedule`]: where batches enter, the clock that stamps
//!   their admission time, and the shared schedule that chooses their generation.
//! - [`AdmissionRegistry`]: the admission points of a topology, for an installer that reads or locks several at once.
//! - [`GenerationToken`] and [`GenerationRef`]: generations of the processing program, which retire when nothing
//!   references them any more, and [`CutBoundary`], where a generation takes over at a cut.
//! - [`Kernel`] and [`Turn`]: the synchronous core of one processing step, and the context a host runs it in, with
//!   numbered output ports, a work budget, per-turn counters, and optional per-call parameters.
//! - [`StateBudget`]: a budget of state, such as tracked contexts, that several kernels share.
//! - [`RegionDriver`] and [`RegionHostBuilder`]: a region, which fuses a chain of processing steps into one topology
//!   transform, split into its synchronous driver and the asynchronous host that runs it, with a non-blocking
//!   [control mailbox][control_mailbox] and supervision of every call into the driver.
//!
//! Apart from admission points, which stamp batches inside the dispatchers of a topology, and the region host, which
//! is a topology component, nothing here performs I/O or depends on a running topology, so a kernel or a region driver
//! can be driven directly, for example by a reference interpreter or a test.

mod admission;
pub(crate) use self::admission::OwnerToken;
pub use self::admission::{
    AdmissionMode, AdmissionPoint, AdmissionPointConfig, AdmissionPointStatus, AdmissionRegistry, AdmissionSchedule,
    HeartbeatOutcome, ScheduleHandle, ScheduleTransaction, TransactionError, MAX_HEARTBEAT_PERIOD,
    MIN_HEARTBEAT_PERIOD,
};

mod budget;
pub use self::budget::StateBudget;

mod clock;
#[cfg(any(test, feature = "test-util"))]
pub use self::clock::ManualWallClock;
pub use self::clock::{AdmissionClock, ClockStamp, SystemWallClock, WallClock};

mod generation;
pub use self::generation::{
    CutBoundary, GenerationId, GenerationIdAllocator, GenerationRef, GenerationToken, RetireNotifier,
};

mod header;
pub use self::header::{AdmissionPointId, AdmissionTime, BatchHeader, EntryId};

mod kernel;
pub use self::kernel::{
    Budget, ClockReset, DrainPolicy, Kernel, LossClass, LossCount, PortIndex, PortOutputs, Progress, Turn,
    TurnCounters, WorkKind,
};

mod region;
pub use self::region::{
    control_mailbox, ControlAck, ControlError, ControlMailbox, ControlSender, ExitId, ExitSink, PanicOutcome,
    PanicSite, RegionDriver, RegionFailure, RegionHostBuilder, RegionPanic, RegionTurn,
};
