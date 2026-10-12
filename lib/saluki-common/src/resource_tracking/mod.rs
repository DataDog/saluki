//! Process-level resource tracking: per-group memory allocation and CPU usage accounting.
//!
//! This module provides the primitives for attributing memory allocations and CPU time to
//! user-defined "resource groups":
//!
//! - a global allocator wrapper ([`TrackingAllocator`]) that attributes every allocation to the
//!   currently entered resource group,
//! - a registry of resource groups ([`ResourceGroupRegistry`]) and per-group statistics
//!   ([`ResourceStats`]),
//! - a [`ResourceGroupToken`] that can be entered to scope allocations and CPU time to a group,
//!   either directly (via a scope guard) or by wrapping a [`Future`](std::future::Future) with the
//!   [`Track`] extension trait.
//!
//! # Memory usage
//!
//! For memory usage to be tracked, [`TrackingAllocator`] must be installed as the global allocator
//! for the process. When installed, every allocation is attributed to the currently entered
//! resource group, defaulting to a catch-all "root" group when no user-defined group is entered.
//!
//! # CPU usage
//!
//! CPU time is attributed to the currently entered resource group when that group is exited, based
//! on the thread CPU time consumed while the group was entered. CPU usage tracking is only
//! available on Linux; on other platforms it is a no-op.
//!
//! Reading the thread's CPU time is a system call, so entries can be sampled: with
//! [`set_cpu_time_sampling`] set to `N`, one entry in `N`, drawn at random, measures its CPU time
//! and counts it `N` times, which keeps each group's CPU time an unbiased estimate. The default,
//! `1`, measures every entry. [`ResourceGroupToken::enter_measured`] always measures.

mod allocator;
mod groups;
mod stats;

pub use self::allocator::TrackingAllocator;
pub use self::groups::{ResourceGroupRegistry, ResourceGroupToken, ResourceTrackingGuard, Track, Tracked};
#[cfg(any(test, feature = "test-util"))]
pub use self::stats::testing;
pub use self::stats::{cpu_time_sampling, set_cpu_time_sampling, ResourceStats, ResourceStatsSnapshot};
