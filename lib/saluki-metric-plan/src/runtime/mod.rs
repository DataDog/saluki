//! Running region programs: the driver of a slot's region host.
//!
//! A [frame](crate::compile::Frame) has one region host per slot, a topology transform built from `saluki-core`'s
//! `RegionHostBuilder`. The host is generic: it reads batches, sends what its driver writes to the slot's exits, and
//! supervises every call. [`MetricRegionDriver`] is the driver: it runs the [region programs][RegionProgram] that the
//! slot has for every live generation.
//!
//! - **Creating a region.** The region's creator builds the boot program's kernels off the data path with
//!   [`build_kernels`], registers the program's counters through [`PrepareRegion::new`] and the region's through
//!   [`RegionConfig::for_slot`], and creates the driver with [`MetricRegionDriver::new`]. [`RegionBounds`] declares the
//!   memory the driver's kernels and buffers need.
//! - **Installing a generation.** The installer builds the kernels a preparation lists to build, sends each region a
//!   [`RegionControl::Prepare`] through the host's control mailbox, and waits for every acknowledgment before it
//!   commits; [`RegionControl::Abort`] drops a generation that will not commit. Regions never choose a generation: a
//!   batch runs the program of the generation its header carries, and a program is released, off the data path, once
//!   its generation retires; [`RegionControl::Release`] asks a region to release what retired.
//! - **Telemetry.** A driver counts in plain integers and publishes to counters registered beforehand through
//!   [`RegisterTelemetry`]; [`PlanTelemetry`] registers the plan-level counters, [`PLAN_NODE_EVENTS_TOTAL`] and
//!   [`PLAN_LOSS_TOTAL`].
//! - **Supervision.** A generation's [`PanicBudget`] decides whether the region survives a panic in its program; a
//!   [`GenerationFailureReporter`] hears of every generation a region fails.
//! - **Accounting.** Each kernel is built and called inside the resource group its build context names, under the
//!   region's own; [`kernel_accounting_name`] names that group, and [`RegionBounds`] declares each kernel's bounds
//!   under the same name.
//! - **State.** A state owner runs in an isolation slot, whose driver follows the time rules of [`state::rules`]:
//!   it keeps the slot's watermark, ticks each lineage of its state owners' state on its own schedule through the tick
//!   gate, runs clock resets, final retirements at cuts, and shutdown as bounded drains, and stamps their time-driven
//!   output (see [`MetricRegionDriver`]). Its kernels draw their state from the host's [`StateBudgets`], and its
//!   bounds hold one drain step ([`RegionBounds::for_slot`]).
//! - **Testing.** With the `test-util` feature, `harness::FrameHarness` runs a whole frame of region drivers in
//!   process, with admission points modeled on the real ones.
//!
//! [RegionProgram]: crate::compile::RegionProgram

use saluki_core::execution::GenerationId;
use snafu::Snafu;

use crate::{
    compile::{EntryKey, KernelKey, SlotId},
    ir::NodeId,
    operators::{ConfigureError, KernelBuildError},
};

mod bounds;
pub use self::bounds::{kernel_accounting_in, kernel_accounting_name, kernel_accounting_path, RegionBounds};

mod code;

mod driver;
pub use self::driver::{
    GenerationFailureReporter, KernelControl, KernelReply, KernelTarget, LanePreparation, MetricRegionDriver,
    PrepareRegion, RegionConfig, RegionControl, RoutingTelemetry,
};

#[cfg(any(test, feature = "test-util"))]
pub mod harness;

mod kernels;
pub use self::kernels::{build_kernels, PreparedKernel};

pub mod state;
pub use self::state::StateBudgets;

mod supervision;
pub use self::supervision::PanicBudget;

mod telemetry;
pub use self::telemetry::{
    NoTelemetry, PlanTelemetry, RegisterTelemetry, StateLaneTelemetry, StateTelemetry, PLAN_CLOCK_RESETS_TOTAL,
    PLAN_LANE_CONTEXTS, PLAN_LANE_CROSSINGS_TOTAL, PLAN_LANE_CROSSING_OVERLAP_CONTEXTS,
    PLAN_LANE_FLUSH_BLOCKED_SECONDS, PLAN_LANE_FLUSH_SECONDS, PLAN_LANE_MISROUTED_TOTAL,
    PLAN_LANE_RESET_OVERFLOW_TOTAL, PLAN_LANE_TICK_DIVERGENCE_TOTAL, PLAN_LANE_TICK_STRAGGLER_TOTAL,
    PLAN_LANE_WATERMARK_LAG_SECONDS, PLAN_LOSS_TOTAL, PLAN_NODE_EVENTS_TOTAL, PLAN_STATE_LINEAGES,
    PLAN_STATE_TICK_GRACE_EXPIRED_TOTAL, PLAN_STATE_WATERMARK_LAG_SECONDS,
};

/// An error creating a region driver, building its kernels, or preparing a generation in it.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum RegionError {
    /// A program has no kernel with a key that was asked for.
    #[snafu(display(
        "The program of slot {} has no kernel `{}`; build only the kernels of the program being prepared.",
        slot,
        key
    ))]
    UnknownKernel {
        /// The program's slot.
        slot: SlotId,

        /// The key.
        key: KernelKey,
    },

    /// A kernel could not be built.
    #[snafu(display("Failed to build the kernel of node `{}`: {}", node, source))]
    KernelBuild {
        /// The kernel's node.
        node: NodeId,

        /// Why.
        source: KernelBuildError,
    },

    /// A program was sent to the region of another slot.
    #[snafu(display(
        "A program for slot {} was sent to the region of slot {}; send each slot's program to its own region.",
        program,
        region
    ))]
    WrongSlot {
        /// The region's slot.
        region: SlotId,

        /// The program's slot.
        program: SlotId,
    },

    /// A generation was prepared twice.
    #[snafu(display(
        "Generation {} is already prepared in region `{}`; prepare each generation in a region once.",
        generation,
        region
    ))]
    AlreadyPrepared {
        /// The region.
        region: String,

        /// The generation.
        generation: GenerationId,
    },

    /// A program uses a kernel the region neither holds nor was given.
    #[snafu(display(
        "The program of generation {} in region `{}` uses kernel `{}`, which the region does not hold and the \
         preparation did not bring; send every kernel the preparation lists to build with the program.",
        generation,
        region,
        key
    ))]
    MissingKernel {
        /// The region.
        region: String,

        /// The generation.
        generation: GenerationId,

        /// The kernel.
        key: KernelKey,
    },

    /// A program refers to something it does not have, or to more exits than the region has.
    #[snafu(display(
        "The program of slot {} cannot run at {}: {}. Programs must be compiled for the frame the region was built \
         from.",
        slot,
        entry,
        message
    ))]
    MalformedProgram {
        /// The program's slot.
        slot: SlotId,

        /// The entry whose steps are malformed.
        entry: EntryKey,

        /// What is wrong.
        message: String,
    },

    /// A program's kernel runs a node its plan does not have.
    #[snafu(display(
        "The plan has no node `{}`, which the program has a kernel for; compute bounds with the plan the program was \
         compiled from.",
        node
    ))]
    BoundsNode {
        /// The node.
        node: NodeId,
    },

    /// A kernel's operator cannot be configured with its node's parameters.
    #[snafu(display("Cannot configure the operator of node `{}`: {}", node, source))]
    BoundsOperator {
        /// The node.
        node: NodeId,

        /// Why.
        source: ConfigureError,
    },

    /// A preparation's state owners cannot be bound to instances of their state as it says.
    #[snafu(display(
        "Cannot prepare generation {} in region `{}`: {}. Prepare state owners with the bindings the installer \
         computed for the active generations.",
        generation,
        region,
        message
    ))]
    StateBinding {
        /// The region.
        region: String,

        /// The generation.
        generation: GenerationId,

        /// What is wrong.
        message: String,
    },

    /// A region cannot run as the lane of its slot it was given, or its preparation's lane shares do not fit it.
    #[snafu(display("Region `{}` cannot run as configured: {}.", region, message))]
    Lanes {
        /// The region.
        region: String,

        /// What is wrong, and how to fix it.
        message: String,
    },

    /// A region of an isolation slot lacks what it needs to run its state owners.
    #[snafu(display("Cannot create region `{}`: {}.", region, message))]
    StateConfig {
        /// The region.
        region: String,

        /// What is missing.
        message: String,
    },
}

#[cfg(test)]
mod frame_tests;
#[cfg(test)]
mod lane_tests;
#[cfg(test)]
mod partitioned_tests;
#[cfg(test)]
mod r5b_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod telemetry_audit_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transition_tests;
