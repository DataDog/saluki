//! Execution contracts for plan-driven processing.
//!
//! These types define how synchronous processing steps are driven, independently of any particular topology or of the
//! data they process:
//!
//! - [`BatchHeader`]: per-batch metadata stamped where a batch enters plan-driven processing, carrying its admission
//!   time and the generation that processes it.
//! - [`GenerationToken`] and [`GenerationRef`]: generations of the processing program, which retire when nothing
//!   references them any more.
//! - [`Kernel`] and [`Turn`]: the synchronous core of one processing step, and the context a host runs it in, with
//!   numbered output ports, a work budget, and per-turn counters.
//!
//! Nothing here performs I/O or depends on a running topology, so a kernel can be driven directly, for example by a
//! reference interpreter or a test.

mod generation;
pub use self::generation::{GenerationId, GenerationIdAllocator, GenerationRef, GenerationToken, RetireNotifier};

mod header;
pub use self::header::{AdmissionPointId, AdmissionTime, BatchHeader, EntryId};

mod kernel;
pub use self::kernel::{
    Budget, DrainPolicy, Kernel, LossClass, LossCount, PortIndex, PortOutputs, Progress, Turn, TurnCounters, WorkKind,
};
