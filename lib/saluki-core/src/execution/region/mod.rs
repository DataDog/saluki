//! Regions: fused chains of synchronous processing, hosted as one topology component.
//!
//! A **region** runs the programs of one part of a processing plan inside a single transform. The work is split
//! between two halves:
//!
//! - a [`RegionDriver`], which is synchronous and owns the programs, their kernels, and their state, and
//! - the region host, built by [`RegionHostBuilder`], which is the asynchronous transform around it: it reads input
//!   batches, sends what the driver writes to the region's exits, runs the driver's timers, answers liveness probes,
//!   applies control messages, paces telemetry, and supervises every call into the driver.
//!
//! A region is controlled through its [control mailbox][crate::execution::control]: a controller, such as an installer
//! preparing a new generation of the program, sends messages with a
//! [`ControlSender`][crate::execution::control::ControlSender] without ever blocking on the region, and awaits each
//! message's [`ControlAck`][crate::execution::control::ControlAck]. The host handles control messages even while it
//! waits for a slow downstream component.
//!
//! # Panics and the process panic hook
//!
//! The host catches panics in the driver with [`catch_unwind`][std::panic::catch_unwind] and recovers as the driver's
//! [`reset_after_panic`][RegionDriver::reset_after_panic] decides. Catching a panic does not stop the process's panic
//! hook from running first: the default hook prints every panic, caught or not, and a process that installs a hook to
//! report panics sees caught region panics too.

mod driver;
pub use self::driver::{
    ExitId, ExitSink, PanicOutcome, PanicSite, RegionDriver, RegionFailure, RegionPanic, RegionTurn,
};

mod host;
pub use self::host::RegionHostBuilder;

#[cfg(test)]
mod tests;
