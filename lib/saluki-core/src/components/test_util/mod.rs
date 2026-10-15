//! Test helpers for running components.
//!
//! This module has two levels of helper:
//!
//! - [`TestComponentDriver`] builds one component from its builder, connects its input and outputs, and runs it the way
//!   the topology would. It returns a [`ComponentControl`] to feed and stop the component, and an [`Outputs`] (or a
//!   single [`Output`]) to observe what it dispatches.
//! - [`TestComponentSupervisor`] is the per-component supervisor on its own, for code that spawns supervised children
//!   but isn't a whole component. The driver runs every component under one. Prefer it over
//!   `Supervisor::new(..).handle()`, which looks equivalent but never runs, so every child spawned on it is dropped.
//!
//! # Driving a component
//!
//! ```no_run
//! # use saluki_core::components::{test_util::TestComponentDriver, transforms::TransformBuilder};
//! # use saluki_core::data_model::event::Event;
//! # async fn example(builder: impl TransformBuilder, events: Vec<Event>) -> Result<(), saluki_error::GenericError> {
//! let (control, mut outputs) = TestComponentDriver::transform(builder).await?;
//! control.send_events(events).await;
//! control.shutdown().await?;
//!
//! let dispatched = outputs.default_output().collect_events().await;
//! # Ok(())
//! # }
//! ```
//!
//! Use [`TestComponentDriver::options`] to change the component ID, the channel capacity, the deadlines, or the
//! resource registry the component is built with.
//!
//! # Things to know
//!
//! ## Drain outputs concurrently when they can fill up
//!
//! Each output channel holds as many items as the interconnect capacity allows (128 by default). A component that
//! dispatches more than that before the test reads anything blocks on the full output, so it never gets to see its
//! input close. Its supervisor then aborts it once the shutdown budget runs out, and [`ComponentControl::shutdown`]
//! returns an error saying so. Drain the output at the same time instead.
//! To drain several outputs at once, take them out first with [`Outputs::take_named_output`]:
//!
//! ```no_run
//! # use saluki_core::components::{test_util::TestComponentDriver, transforms::TransformBuilder};
//! # use saluki_core::data_model::event::Event;
//! # async fn example(builder: impl TransformBuilder, events: Vec<Event>) -> Result<(), saluki_error::GenericError> {
//! let (control, mut outputs) = TestComponentDriver::transform(builder).await?;
//! let (result, dispatched) = tokio::join!(
//!     async move {
//!         control.send_events(events).await;
//!         control.shutdown().await
//!     },
//!     outputs.default_output().collect_events(),
//! );
//! result?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Keep the handles alive
//!
//! Bind handles you don't use to `_control` or `_outputs`, not to `_`. A `_` pattern drops the value immediately:
//! dropping the control signals shutdown and closes the component's input, and dropping an output makes every
//! dispatch to it fail.
//!
//! ## Deadlines run on Tokio time
//!
//! Every wait has a deadline (five seconds by default, see [`DriverOptions::with_wait_timeout`]) and panics with a
//! description of what it was waiting for. Deadlines are measured with [`tokio::time`], so under a paused clock the
//! runtime skips ahead to the deadline as soon as every task is idle. A component waiting on real I/O, a blocking
//! thread, or a file system event looks idle to Tokio, and can hit the deadline even though it would have finished.
//!
//! ## What the driver doesn't cover
//!
//! - Every declared output is connected to a channel, so the component never takes the branch where an output has no
//!   downstream component. Build the context by hand to test that branch.
//! - Nothing sends liveness probes, so [`Health::live`][crate::health::Health::live] stays pending forever and a
//!   component's liveness response is never exercised. Readiness does work: see [`ComponentControl::wait_until_ready`].

mod control;
pub use self::control::{ComponentControl, NoInput};

mod driver;
pub use self::driver::{DriverOptions, TestComponentDriver};

mod output;
pub use self::output::{Output, Outputs};

mod supervisor;
pub use self::supervisor::TestComponentSupervisor;

#[cfg(test)]
mod tests;
