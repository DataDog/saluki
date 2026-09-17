//! Serialized form of a supervision-tree snapshot.
//!
//! These are the wire types: the stable shape that [`SupervisionTreeHandle::snapshot`][super::SupervisionTreeHandle]
//! produces, the API route serves, and the CLI decodes. They are deliberately separate from the live bookkeeping in
//! the parent module -- a rename here changes a payload that shipped binaries parse, which is a very different kind of
//! change from adjusting how the tree is tracked at runtime.

use saluki_common::resource_tracking::ResourceStatsSnapshot;
use serde::{Deserialize, Serialize};

use crate::runtime::{restart::RestartType, supervisor::AutoShutdown, RestartMode};

/// A point in time, as milliseconds since the Unix epoch.
///
/// Serialized as a plain integer. The standard library's own representation of a timestamp serializes as a pair of
/// fields, which is awkward for a consumer that just wants to render a time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct UnixMillis(pub u64);

/// A point-in-time view of a supervision tree.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TreeSnapshot {
    /// When the snapshot was taken.
    pub captured_at: UnixMillis,

    /// Whether allocations are being tracked at all.
    ///
    /// When false, every byte count in the snapshot reads zero because nothing is measuring, rather than because
    /// nothing has been allocated. Distinguishing the two matters: the tracking allocator has to be installed as the
    /// process's global allocator, which not every embedding does.
    pub resource_tracking_enabled: bool,

    /// Aggregate counts across the whole tree.
    pub totals: TreeTotals,

    /// The supervisor the snapshot was taken from, and everything beneath it.
    pub root: NodeSnapshot,
}

/// Aggregate counts across a whole [`TreeSnapshot`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct TreeTotals {
    /// Number of supervisors in the tree.
    pub supervisors: usize,

    /// Number of leaf workers in the tree.
    pub workers: usize,

    /// Number of nodes currently running.
    pub running: usize,

    /// Number of nodes that ran and have since exited without being restarted.
    pub exited: usize,

    /// Number of nodes that are declared but have never run.
    pub registered: usize,

    /// Total restarts across every node in the tree.
    pub restarts: u64,

    /// Total live bytes across every distinct resource group in the tree.
    ///
    /// Summed over groups rather than over nodes: several nodes can share one group, and their usage is one figure
    /// rather than one per node.
    pub live_bytes: u64,

    /// Total CPU time across every distinct resource group in the tree, in nanoseconds.
    pub cpu_time_nanos: u64,

    /// Depth of the deepest node, counting the root as 1.
    pub max_depth: usize,
}

/// Whether a node supervises other nodes or performs work itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A supervisor, which manages other nodes.
    Supervisor,

    /// A worker, which performs work and has no children of its own.
    Worker,
}

/// Where a node is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Declared, but not currently running.
    ///
    /// Either it has never run, or -- for a supervisor being restarted -- it has stopped and its next generation has
    /// not yet started.
    Registered,

    /// Currently running.
    Running,

    /// Ran, exited, and was not restarted.
    Exited,
}

/// One node -- a supervisor or a worker -- in a [`TreeSnapshot`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeSnapshot {
    /// The node's bare name, as registered with its supervisor.
    pub name: String,

    /// Whether the node is a supervisor or a worker.
    pub kind: NodeKind,

    /// The node's fully qualified, dot-scoped process name. `None` if the node has never run.
    pub process_name: Option<String>,

    /// Identifier of the node's most recent process. `None` if the node has never run.
    ///
    /// A restart gives the node a new process, and so a new identifier. A node that has stopped keeps the identifier
    /// it last ran under, which is what `state` is for: this says what the node ran as, and `state` says whether it
    /// still is.
    pub process_id: Option<u64>,

    /// Where the node is in its lifecycle.
    pub state: NodeState,

    /// The node's restart policy.
    pub restart: RestartType,

    /// Whether the node's termination can drive its supervisor to shut down.
    pub significant: bool,

    /// When the node first became part of the tree.
    ///
    /// Constant across restarts, so the difference between this and `started_at` is the time the node has spent not
    /// running since it was created.
    pub created_at: UnixMillis,

    /// When the node's most recent process started. `None` if the node has never run.
    pub started_at: Option<UnixMillis>,

    /// How long the node's current process has been running, in milliseconds. `None` unless it is running.
    pub uptime_ms: Option<u64>,

    /// How many times the node has been restarted since it was created.
    ///
    /// Counts every restart that gave the node a new process, whether it was restarted on its own account or brought
    /// back as part of its supervisor restarting -- either by a group restart or by the supervisor itself being
    /// restarted from above. Only statically declared nodes accumulate this across a supervisor restart, since only
    /// they have an identity that survives one; a dynamically spawned node is never restored.
    pub restart_count: u32,

    /// When the node exited without being restarted. `None` unless it has exited.
    pub exited_at: Option<UnixMillis>,

    /// The resource group the node's allocations are attributed to. `None` if the node has never run.
    ///
    /// For a supervisor this is its own group. For a worker it is its supervisor's, since a worker inherits its
    /// supervisor's group rather than owning one.
    pub resource_group: Option<String>,

    /// Resource usage attributed to this node.
    ///
    /// Populated for a supervisor, which owns a resource group covering itself and its workers. Always absent for a
    /// worker, whose usage is counted against the supervisor named by `resource_group`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceUsage>,

    /// How the node supervises its children. Absent for a worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<SupervisionSettings>,

    /// The node's children. Empty for a worker.
    pub children: Vec<NodeSnapshot>,
}

/// Cumulative resource usage for one resource group.
///
/// Counts are since the process started. Both allocation counts and CPU time depend on process-wide facilities that
/// may not be available: see [`TreeSnapshot::resource_tracking_enabled`].
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ResourceUsage {
    /// Bytes allocated.
    pub allocated_bytes: u64,

    /// Objects allocated.
    pub allocated_objects: u64,

    /// Bytes deallocated.
    pub deallocated_bytes: u64,

    /// Objects deallocated.
    pub deallocated_objects: u64,

    /// Bytes allocated and not yet deallocated.
    pub live_bytes: u64,

    /// Objects allocated and not yet deallocated.
    pub live_objects: u64,

    /// CPU time consumed, in nanoseconds.
    ///
    /// Always zero where per-thread CPU time is unavailable.
    pub cpu_time_nanos: u64,
}

impl From<&ResourceStatsSnapshot> for ResourceUsage {
    fn from(stats: &ResourceStatsSnapshot) -> Self {
        Self {
            allocated_bytes: stats.allocated_bytes as u64,
            allocated_objects: stats.allocated_objects as u64,
            deallocated_bytes: stats.deallocated_bytes as u64,
            deallocated_objects: stats.deallocated_objects as u64,
            live_bytes: stats.live_bytes() as u64,
            live_objects: stats.live_objects() as u64,
            cpu_time_nanos: stats.cpu_time_nanos,
        }
    }
}

/// How a supervisor supervises its children, and how it has fared.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SupervisionSettings {
    /// Whether a failing child is restarted alone or together with its siblings.
    pub restart_mode: RestartMode,

    /// How many restarts the supervisor tolerates within `restart_period_ms` before giving up.
    pub restart_intensity: usize,

    /// The window over which `restart_intensity` is measured, in milliseconds.
    pub restart_period_ms: u64,

    /// Whether the termination of a significant child drives the supervisor to shut down.
    pub auto_shutdown: AutoShutdown,

    /// How long the supervisor allows its children to drain during shutdown, in milliseconds. `None` if unbounded.
    pub shutdown_budget_ms: Option<u64>,

    /// Worker threads on the supervisor's own runtime. `None` if it runs on its parent's runtime.
    pub dedicated_threads: Option<usize>,

    /// How many child restarts the supervisor has performed, across all of its own generations.
    ///
    /// A group restart counts once here however many children it brought back, which is what distinguishes a
    /// supervisor restarting its whole group repeatedly from a single child restarting repeatedly.
    pub restarts_performed: u64,

    /// How many times the supervisor has started running.
    pub generation: u64,
}
