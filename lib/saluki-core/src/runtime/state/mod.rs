//! Runtime state management utilities.
//!
//! This module provides utilities for managing shared state across processes in the runtime system.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::runtime::process::Id;

mod resources;
pub use self::resources::{
    PendingRequestInfo, PublishError, ResourceGuard, ResourceInfo, ResourceRegistry, ResourceRegistrySnapshot,
};

mod dataspace;
pub use self::dataspace::{AssertionUpdate, DataspaceRegistry, Subscription, WildcardSubscription};

static GLOBAL_HANDLE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A handle used to key resources and pub/sub channels in the runtime state registries.
///
/// Handles come in two flavors:
/// - **Process-scoped**: tied to a specific process, created via [`Handle::current_process`]
///   or [`Handle::for_process`].
/// - **Global**: a globally unique opaque identifier not tied to any process, created via
///   [`Handle::new_global`].
///
/// Handles must always be constructed explicitly — there are no implicit conversions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handle {
    /// Scoped to a specific process.
    Process(Id),

    /// A globally unique opaque identifier.
    Global(u64),
}

impl Handle {
    /// Creates a handle scoped to the currently executing process.
    ///
    /// Uses [`process::Id::current`] to determine the current process. If called outside of a
    /// process context, this returns a handle scoped to the root process ([`process::Id::ROOT`]).
    pub fn current_process() -> Self {
        Self::Process(Id::current())
    }

    /// Creates a handle scoped to the given process.
    pub fn for_process(id: Id) -> Self {
        Self::Process(id)
    }

    /// Creates a new globally unique handle not tied to any process.
    pub fn new_global() -> Self {
        let id = GLOBAL_HANDLE_COUNTER.fetch_add(1, Relaxed);
        Self::Global(id)
    }
}
