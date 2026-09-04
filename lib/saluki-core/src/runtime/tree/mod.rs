//! Supervision-tree introspection.
//!
//! A supervisor knows the shape of the subtree below it -- which children it has, whether each is a worker or a
//! nested supervisor, how each is configured, and how each has fared -- but that knowledge lives inside the future
//! that drives the supervisor and is unreachable from anywhere else. This module makes it observable.
//!
//! # How it works
//!
//! Every [`Supervisor`][super::Supervisor] owns an [`Arc<SupervisorNode>`][SupervisorNode]: a small piece of shared
//! bookkeeping that the supervisor's run loop writes to as children start, exit, and restart. When a supervisor is
//! added as a child of another supervisor, the parent records a clone of that child's node alongside its own
//! bookkeeping, so the nodes form a graph that mirrors the supervision tree. Walking that graph from any node yields
//! the subtree rooted there, which is what [`SupervisionTreeHandle::snapshot`] does.
//!
//! Nodes are shared rather than copied, so a handle taken before a supervisor starts observes every subsequent
//! generation of that supervisor, including one running on a dedicated runtime on another OS thread.
//!
//! Note that this module owns more than an observer's view: [`Roster`] holds the supervisor's own live child roster,
//! not a mirror of it. That is deliberate -- it is what makes the two impossible to drift apart -- but it means
//! changes here affect supervision itself and not only what is reported about it.
//!
//! # Consistency
//!
//! A snapshot is assembled by locking one node at a time, so it is not a globally atomic view: a child may start or
//! exit while the walk is in progress, and the result then mixes observations from slightly different instants. This
//! is deliberate. Taking the whole tree's locks at once would put an operator-facing diagnostics endpoint in a
//! position to stall supervision across the entire process, which is a far worse property than a snapshot whose
//! nodes are microseconds apart.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use saluki_common::{
    collections::{FastHashMap, FastHashSet, FastIndexMap},
    resource_tracking::{ResourceGroupRegistry, ResourceStatsSnapshot},
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{
    process::Process,
    restart::{RestartStrategy, RestartType},
    supervisor::AutoShutdown,
    ProcessId,
};

mod api;
pub use self::api::{SupervisionTreeAPIHandler, SupervisionTreeState};

mod worker;
pub use self::worker::SupervisionTreeWorker;

/// API route serving a snapshot of a supervision tree.
///
/// Exported so that a client can address the route without restating the path.
pub const SUPERVISION_TREE_ROUTE: &str = "/runtime/processes";

/// Maximum depth the snapshot walk descends before it stops and reports the subtree as truncated.
///
/// The node graph cannot contain a cycle -- a supervisor can't be its own ancestor, since
/// [`add_worker`][super::Supervisor::add_worker] takes its child by value -- so this is not a termination condition
/// but a stack guard. The walk is recursive and runs wherever the caller asks for a snapshot, which may be a
/// request-serving task with a modest stack, and a diagnostics endpoint should not be able to abort the process even
/// if a future change does manage to build a pathological tree.
const MAX_TREE_DEPTH: usize = 64;

/// A wall-clock and monotonic reading of the same instant.
///
/// Both are needed. Wall clock is what an operator wants to see (and what correlates with logs), but it can step
/// backwards, so ages computed from it can come out negative or absurd. The monotonic reading can't, so every
/// duration is derived from it and every displayed timestamp from the wall clock.
///
/// Uses [`std::time::Instant`] rather than [`tokio::time::Instant`] deliberately: a snapshot may be taken from a
/// thread with no Tokio runtime, and it should report real elapsed time rather than a test's virtual clock.
#[derive(Clone, Copy)]
pub(super) struct Stamp {
    wall: SystemTime,
    mono: Instant,
}

impl Stamp {
    /// Captures the current instant.
    pub(super) fn now() -> Self {
        Self {
            wall: SystemTime::now(),
            mono: Instant::now(),
        }
    }

    /// Returns the wall-clock reading as milliseconds since the Unix epoch.
    fn wall_millis(&self) -> UnixMillis {
        // A pre-epoch clock makes `duration_since` fail rather than return a negative duration; report it as the
        // epoch rather than panicking inside a diagnostics path.
        UnixMillis(duration_millis(
            self.wall.duration_since(UNIX_EPOCH).unwrap_or_default(),
        ))
    }

    /// Returns how long ago this instant was, in milliseconds, per the monotonic clock.
    fn elapsed_millis(&self) -> u64 {
        duration_millis(self.mono.elapsed())
    }
}

/// Identity of a child that survives the id churn of a group restart.
///
/// A supervisor keys its live roster by an id drawn from a monotonic counter, and a
/// [`OneForAll`][super::RestartMode::OneForAll] restart discards the whole roster and re-registers every eligible
/// child under a *fresh* id. Facts that must outlive that -- when the child was first created, how many times it has
/// been restarted -- therefore can't be keyed by roster id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum ChildKey {
    /// A child declared before the run, identified by its index in the supervisor's static child list.
    ///
    /// That list is fixed once the supervisor starts, so the index is a stable identity across generations.
    Static(usize),

    /// A dynamically spawned child, identified by its roster id.
    ///
    /// A dynamic child is never restored across generations, so its roster id is as stable an identity as it needs.
    /// Ids come from a monotonic counter, so ordering by one is ordering by spawn.
    Dynamic(u64),
}

impl ChildKey {
    /// Returns whether this key refers to a dynamically spawned child.
    fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dynamic(_))
    }
}

/// The process a child was started under.
///
/// Produced by the supervisor's worker bookkeeping at the moment a child's task is spawned, which is the only place
/// the child's [`Process`] exists and therefore the only place its identity can be captured.
#[derive(Clone)]
pub(super) struct StartedChild {
    process_id: ProcessId,
    process_name: Arc<str>,
    at: Stamp,
}

impl StartedChild {
    /// Records the process a child was just started under.
    pub(super) fn new(process: &Process, process_name: Arc<str>) -> Self {
        Self {
            process_id: *process.id(),
            process_name,
            at: Stamp::now(),
        }
    }
}

/// Facts about a child that its supervisor knows at registration time.
///
/// Assembled by the supervisor (which can see its own child specifications and configuration) and handed to
/// [`Roster::insert`], so that this module needs no visibility into either.
pub(super) struct ChildFacts {
    /// Identity that survives a group restart.
    pub(super) key: ChildKey,

    /// The child's bare, unscoped name.
    pub(super) name: Arc<str>,

    /// The child's own node, if the child is a nested supervisor.
    ///
    /// This is what links a parent's bookkeeping to its child's, and so what makes the tree walkable.
    pub(super) node: Option<Arc<SupervisorNode>>,

    /// The child's restart policy.
    pub(super) restart: RestartType,

    /// Whether the child's termination can drive its supervisor to shut down.
    pub(super) significant: bool,
}

/// Whether a child is currently running or has exited for good.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveState {
    Running,
    Exited,
}

/// What a child is, from its parent's point of view.
enum RecordKind {
    /// A leaf worker.
    Worker,

    /// A nested supervisor, along with its own bookkeeping.
    Supervisor(Arc<SupervisorNode>),
}

/// A supervisor's record of one child in the current generation.
struct ChildRecord {
    key: ChildKey,
    name: Arc<str>,
    kind: RecordKind,
    restart: RestartType,
    significant: bool,
    /// The process the child is currently running under, or last ran under. `None` only for a child that never
    /// successfully started.
    start: Option<StartedChild>,
    state: LiveState,
}

/// Cumulative facts about a child that outlive any single generation of it.
struct ChildHistory {
    /// When the child first entered its supervisor's roster.
    ///
    /// Deliberately not its registration time: registration happens once, during program construction, and never
    /// again on a later generation, so it measures how long ago a builder method was called rather than anything
    /// about the running tree.
    created: Stamp,
    restarts: u32,
    exited: Option<Stamp>,
}

/// Identity of the process a supervisor is currently running under.
struct RunIdentity {
    process_id: ProcessId,
    /// The supervisor's own fully qualified process name.
    ///
    /// Recorded by the supervisor itself rather than by its parent, because the two can differ: a supervisor on a
    /// dedicated runtime re-roots its process name when it starts (a known defect), so only the supervisor knows what
    /// it actually runs as -- and only that name matches the resource group its allocations are attributed to.
    process_name: Arc<str>,
    started: Stamp,
}

/// A supervisor's configuration.
///
/// Owned by the supervisor's node rather than by the [`Supervisor`][super::Supervisor] itself, so that there is one
/// copy rather than two that have to be kept in step, and so that it can be read before the supervisor has ever run.
#[derive(Clone, Copy)]
pub(super) struct NodeConfig {
    pub(super) restart_strategy: RestartStrategy,
    pub(super) auto_shutdown: AutoShutdown,
    pub(super) shutdown_budget: Option<Duration>,
    pub(super) dedicated_threads: Option<usize>,
}

/// The mutable half of a [`SupervisorNode`].
struct NodeInner {
    config: NodeConfig,
    /// The current run, if the supervisor is running. `None` before its first run and after every run ends.
    run: Option<RunIdentity>,
    generation: u64,
    restarts_performed: u64,
    /// Children of the current generation, in declaration order.
    ///
    /// Ordered rather than hashed so the rendered tree and the serialized JSON come out in the order children were
    /// declared, which is how the tree is written and therefore how it reads.
    children: FastIndexMap<u64, ChildRecord>,
    /// Per-child facts that outlive a generation, keyed by an identity that survives a group restart.
    history: FastHashMap<ChildKey, ChildHistory>,
    /// Supervisors that a worker child drives internally, keyed by that worker's roster id.
    ///
    /// Some workers own a supervisor rather than being one -- they build it after initialization and run it inside
    /// their own future -- so the parent has no supervisor value to record at registration time and the subtree would
    /// otherwise be invisible. Such a supervisor registers itself here when it starts.
    adopted: FastHashMap<u64, Arc<SupervisorNode>>,
}

impl NodeInner {
    /// Discards everything specific to a single generation of the supervisor.
    ///
    /// Used when a run begins or ends, which discards the generation wholesale. A group restart within a run keeps
    /// the tombstones it won't bring back, and so prunes the roster itself.
    fn clear_generation(&mut self) {
        self.children.clear();
        self.adopted.clear();
    }

    /// Returns the number of children currently believed to be running.
    fn running_children(&self) -> usize {
        self.children.values().filter(|r| r.state == LiveState::Running).count()
    }
}

/// The supervised child slot a worker is running as.
///
/// Installed around every supervised worker task so that a supervisor a worker builds and runs inside its own future
/// can attach itself to the tree, which it could not otherwise do: the parent has no supervisor value to record at
/// registration time, because the supervisor does not exist until the worker is already running.
#[derive(Clone)]
pub(super) struct TreeParent {
    node: Arc<SupervisorNode>,
    child_id: u64,
}

impl TreeParent {
    /// Identifies the child slot `child_id` of the supervisor tracked by `node`.
    pub(super) fn new(node: Arc<SupervisorNode>, child_id: u64) -> Self {
        Self { node, child_id }
    }
}

tokio::task_local! {
    /// The child slot the currently running worker occupies in its supervisor.
    pub(super) static CURRENT_TREE_PARENT: TreeParent;
}

/// Shared bookkeeping for one supervisor.
///
/// Held by the supervisor itself, by every internal clone of it (so all generations write to the same place), by its
/// parent's record of it, and by any [`SupervisionTreeHandle`] pointed at it.
pub(super) struct SupervisorNode {
    id: Arc<str>,
    created: Stamp,
    state: Mutex<NodeInner>,
}

impl SupervisorNode {
    /// Creates bookkeeping for a supervisor with the given identifier.
    pub(super) fn new(id: Arc<str>) -> Self {
        Self {
            id,
            created: Stamp::now(),
            state: Mutex::new(NodeInner {
                config: NodeConfig {
                    restart_strategy: RestartStrategy::default(),
                    auto_shutdown: AutoShutdown::default(),
                    shutdown_budget: None,
                    dedicated_threads: None,
                },
                run: None,
                generation: 0,
                restarts_performed: 0,
                children: FastIndexMap::default(),
                history: FastHashMap::default(),
                adopted: FastHashMap::default(),
            }),
        }
    }

    /// Locks the node's mutable state.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned, which cannot happen: nothing done while holding it can unwind.
    fn state(&self) -> std::sync::MutexGuard<'_, NodeInner> {
        self.state.lock().expect("supervision-tree node lock poisoned")
    }

    /// Returns the supervisor's configuration.
    pub(super) fn config(&self) -> NodeConfig {
        self.state().config
    }

    /// Updates the supervisor's configuration.
    pub(super) fn update_config(&self, f: impl FnOnce(&mut NodeConfig)) {
        f(&mut self.state().config);
    }

    /// Records that the supervisor has begun a run under `process`.
    ///
    /// Resets everything specific to a generation, so a run that ended abruptly -- a panicked task, or a dedicated
    /// runtime thread that unwound without reaching its cleanup -- can't leave stale children behind for the next
    /// generation to be confused with.
    pub(super) fn begin_run(self: &Arc<Self>, process: &Process) {
        let mut state = self.state();

        if state.run.is_some() {
            // Two overlapping runs share one node, one spawn queue, one id counter and one child gauge, so the second
            // silently clobbers the first. This is unreachable from outside the crate -- `add_worker` takes its child
            // by value and `Supervisor` has no public `Clone` -- so what this really guards is a restart that begins
            // before the previous generation has finished stopping.
            debug_assert!(
                false,
                "supervisor '{}' began a run while another was still active",
                self.id
            );
            warn!(
                supervisor_id = %self.id,
                "Supervisor began a run while another was still active; a supervisor must not be supervised by two \
                 parents."
            );
        }

        state.run = Some(RunIdentity {
            process_id: *process.id(),
            process_name: process.name().into(),
            started: Stamp::now(),
        });
        state.generation += 1;
        state.clear_generation();
        drop(state);

        // If we were started inside a worker's task, attach ourselves to the tree under that worker. This is how a
        // supervisor built and run inside a worker's future -- rather than handed to a parent as a child -- becomes
        // visible. Absent when the supervisor is a root, or is running on a dedicated runtime (whose thread has no
        // task-locals), and in the latter case the parent already recorded us directly.
        let _ = CURRENT_TREE_PARENT.try_with(|parent| {
            parent.node.state().adopted.insert(parent.child_id, Arc::clone(self));
        });
    }

    /// Records that the supervisor's run has ended.
    ///
    /// Keeps everything cumulative -- generation count, restarts performed, per-child history -- and discards
    /// everything about the generation that just ended, so a stopped supervisor reports as stopped rather than
    /// presenting a subtree of processes that no longer exist.
    pub(super) fn end_run(&self) {
        let mut state = self.state();
        state.run = None;
        state.clear_generation();
        drop(state);

        let _ = CURRENT_TREE_PARENT.try_with(|parent| {
            parent.node.state().adopted.remove(&parent.child_id);
        });
    }
}

/// A supervisor's children, in both the form the supervisor runs from and the form an observer reads.
///
/// The supervisor's own roster is a plain map it owns exclusively; the observable bookkeeping lives behind a lock and
/// is shared. Both are updated through this one type so that they cannot drift: there is no way to add, restart or
/// remove a child in one without doing so in the other.
pub(super) struct Roster<E> {
    live: FastHashMap<u64, E>,
    node: Arc<SupervisorNode>,
}

impl<E> Roster<E> {
    /// Creates an empty roster that publishes into `node`.
    pub(super) fn new(node: Arc<SupervisorNode>) -> Self {
        Self {
            live: FastHashMap::default(),
            node,
        }
    }

    /// Returns the child registered under `id`, if any.
    pub(super) fn get(&self, id: u64) -> Option<&E> {
        self.live.get(&id)
    }

    /// Returns an iterator over every registered child.
    pub(super) fn values(&self) -> impl Iterator<Item = &E> {
        self.live.values()
    }

    /// Registers a newly started child.
    ///
    /// `facts` describes the child as its supervisor configured it, and `started` identifies the process it was
    /// started under. A child re-registered under a fresh id after a group restart keeps the creation time and
    /// restart count recorded against its [`ChildKey`].
    pub(super) fn insert(&mut self, id: u64, entry: E, facts: ChildFacts, started: StartedChild) {
        self.live.insert(id, entry);

        let mut state = self.node.state();
        let history = state.history.entry(facts.key).or_insert_with(|| ChildHistory {
            created: started.at,
            restarts: 0,
            exited: None,
        });
        history.exited = None;

        let kind = match facts.node {
            Some(node) => RecordKind::Supervisor(node),
            None => RecordKind::Worker,
        };

        state.children.insert(
            id,
            ChildRecord {
                key: facts.key,
                name: facts.name,
                kind,
                restart: facts.restart,
                significant: facts.significant,
                start: Some(started),
                state: LiveState::Running,
            },
        );
        drop(state);

        self.debug_assert_consistent();
    }

    /// Records that the child under `id` has been restarted in place, under a new process.
    pub(super) fn restart_in_place(&mut self, id: u64, started: StartedChild) {
        let mut state = self.node.state();
        state.restarts_performed += 1;

        // Whatever the previous incarnation adopted refers to a supervisor that has since stopped. The new
        // incarnation re-registers if it drives one of its own.
        state.adopted.remove(&id);

        if let Some(record) = state.children.get_mut(&id) {
            let key = record.key;
            record.start = Some(started);
            record.state = LiveState::Running;
            if let Some(history) = state.history.get_mut(&key) {
                history.restarts += 1;
                history.exited = None;
            }
        }
        drop(state);

        self.debug_assert_consistent();
    }

    /// Removes the child under `id`, which has exited and will not be restarted.
    ///
    /// A statically declared child is kept as a tombstone rather than dropped: it remains part of the tree's declared
    /// shape, and "declared, ran, and then stopped for good" is precisely what an operator needs to see. A
    /// dynamically spawned child is dropped outright -- the canonical dynamic child is one short-lived task per unit
    /// of work, so retaining every one that has ever finished would grow without bound.
    pub(super) fn remove(&mut self, id: u64) -> Option<E> {
        let entry = self.live.remove(&id);

        let mut state = self.node.state();
        state.adopted.remove(&id);
        match state.children.get(&id).map(|record| record.key) {
            // Order is restored by sorting on `ChildKey` when a snapshot is taken, so the roster itself doesn't need
            // to preserve it -- which lets a dynamic child, of which there may be one per unit of work, leave in
            // constant time rather than shifting every entry behind it.
            Some(key) if key.is_dynamic() => {
                state.children.swap_remove(&id);
                state.history.remove(&key);
            }
            Some(key) => {
                let exited = Stamp::now();
                if let Some(record) = state.children.get_mut(&id) {
                    record.state = LiveState::Exited;
                }
                if let Some(history) = state.history.get_mut(&key) {
                    history.exited = Some(exited);
                }
            }
            None => {}
        }
        drop(state);

        self.debug_assert_consistent();

        entry
    }

    /// Clears the roster ahead of a group restart, counting it as a restart of every child it held.
    ///
    /// The children about to be re-registered each get a new process and a new start time, so counting the restart
    /// against every one of them is what keeps their restart counts consistent with the rest of their own record.
    /// Children that a group restart doesn't bring back keep their tombstones.
    pub(super) fn clear_for_group_restart(&mut self) {
        self.live.clear();

        let mut state = self.node.state();
        state.restarts_performed += 1;

        let NodeInner { children, history, .. } = &mut *state;
        for record in children.values().filter(|r| r.state == LiveState::Running) {
            if let Some(history) = history.get_mut(&record.key) {
                history.restarts += 1;
            }
        }

        // Keep the tombstones of statically declared children a group restart won't bring back, since they remain
        // part of the tree's declared shape. Everything else is about to be re-registered under a fresh id, and
        // keeping the old record too would show each of those children twice.
        state.children.retain(|_, record| {
            record.state == LiveState::Exited && !record.key.is_dynamic() && record.restart == RestartType::Temporary
        });
        state.adopted.clear();
        drop(state);

        self.debug_assert_consistent();
    }

    /// Checks that the supervisor's roster and the observable bookkeeping still agree.
    ///
    /// Every mutating method ends here, so the crate's existing supervision tests double as drift tests for this
    /// module without needing to know it exists.
    fn debug_assert_consistent(&self) {
        #[cfg(debug_assertions)]
        {
            let state = self.node.state();
            let running = state.running_children();
            debug_assert_eq!(
                self.live.len(),
                running,
                "supervisor '{}' roster and supervision-tree bookkeeping diverged",
                self.node.id
            );

            // Comparing every key is O(n) per mutation, and therefore O(n^2) across the boot of a supervisor with many
            // children. Worth paying for the small rosters that describe most trees, not for the largest.
            if self.live.len() <= 32 {
                for id in self.live.keys() {
                    debug_assert!(
                        state.children.contains_key(id),
                        "child {} is in supervisor '{}' roster but missing from its supervision-tree bookkeeping",
                        id,
                        self.node.id
                    );
                }
            }
        }

        saluki_antithesis::always_or_unreachable!(
            self.live.len() == self.node.state().running_children(),
            "supervision-tree bookkeeping tracks the supervisor's live roster"
        );
    }
}

/// A read-only handle for taking snapshots of a supervision tree.
///
/// Obtained from [`Supervisor::tree_handle`][super::Supervisor::tree_handle]. Cheap to clone, safe to share across
/// tasks and threads, and deliberately incapable of anything but observation -- it cannot spawn children or
/// otherwise affect the tree it reports on.
///
/// A handle can be taken before the supervisor starts, and remains valid across every restart of it.
#[derive(Clone)]
pub struct SupervisionTreeHandle {
    node: Arc<SupervisorNode>,
}

impl SupervisionTreeHandle {
    /// Creates a handle for the given node.
    pub(super) fn new(node: Arc<SupervisorNode>) -> Self {
        Self { node }
    }

    /// Returns the identifier of the supervisor at the root of this handle's view.
    pub fn name(&self) -> &str {
        &self.node.id
    }

    /// Renders the current state of the supervision tree as pretty-printed JSON.
    ///
    /// Returns a JSON object describing the failure if the tree can't be serialized, so that a caller writing a
    /// diagnostic artifact always has something to write.
    pub fn snapshot_json(&self) -> String {
        match serde_json::to_string_pretty(&self.snapshot()) {
            Ok(json) => json,
            Err(e) => {
                warn!(error = %e, "Failed to serialize supervision tree.");
                String::from(r#"{"error": "failed to serialize supervision tree"}"#)
            }
        }
    }

    /// Creates a [`SupervisionTreeAPIHandler`] serving snapshots of this tree.
    pub fn api_handler(&self) -> SupervisionTreeAPIHandler {
        SupervisionTreeAPIHandler::from_handle(self.clone())
    }

    /// Creates a [`SupervisionTreeWorker`] that publishes this tree over the control plane.
    pub fn worker(&self) -> SupervisionTreeWorker {
        SupervisionTreeWorker::new(self.clone())
    }

    /// Captures the current state of the supervision tree.
    ///
    /// Descends from this handle's supervisor through every child, nesting each child's own children beneath it.
    ///
    /// # Consistency
    ///
    /// The tree is read one supervisor at a time, so a snapshot is not a globally atomic view: a child may start or
    /// exit while the walk is in progress, and the result then mixes observations from slightly different instants.
    /// This is deliberate. Reading the whole tree atomically would mean holding every supervisor's bookkeeping at
    /// once, which would let an operator-facing diagnostics call stall supervision across the entire process -- a far
    /// worse property than a snapshot whose nodes are microseconds apart.
    pub fn snapshot(&self) -> TreeSnapshot {
        // Read every resource group once, before touching any node. Doing it per node would take the global resource
        // registry's lock once per node, each acquisition contending with the group registration that happens
        // whenever any supervisor anywhere in the process starts a child.
        let groups = collect_resource_groups();

        let mut walk = Walk {
            groups,
            totals: TreeTotals::default(),
            counted_groups: FastHashSet::default(),
        };
        let root = walk.supervisor(&self.node, ParentFacts::root(&self.node), 1);

        TreeSnapshot {
            captured_at: Stamp::now().wall_millis(),
            resource_tracking_enabled: ResourceGroupRegistry::allocator_installed(),
            totals: walk.totals,
            root,
        }
    }
}

/// Reads the current usage of every resource group in the process.
fn collect_resource_groups() -> FastHashMap<Arc<str>, ResourceUsage> {
    let mut groups = FastHashMap::default();

    ResourceGroupRegistry::global().visit_resource_groups(|name, stats| {
        let stats = stats.snapshot_delta(&ResourceStatsSnapshot::empty());
        groups.insert(
            Arc::<str>::from(name),
            ResourceUsage {
                allocated_bytes: stats.allocated_bytes as u64,
                allocated_objects: stats.allocated_objects as u64,
                deallocated_bytes: stats.deallocated_bytes as u64,
                deallocated_objects: stats.deallocated_objects as u64,
                live_bytes: stats.live_bytes() as u64,
                live_objects: stats.live_objects() as u64,
                cpu_time_nanos: stats.cpu_time_nanos,
            },
        );
    });

    groups
}

/// What a node's parent contributes to its snapshot.
///
/// A node's own bookkeeping knows what it is doing; its parent knows how it was configured, when it was created, and
/// how many times it has been restarted. Both halves are needed, and only the parent has the second one -- except at
/// the root, which has no parent.
#[derive(Clone)]
struct ParentFacts {
    name: Arc<str>,
    restart: RestartType,
    significant: bool,
    created: Stamp,
    restarts: u32,
    exited: Option<Stamp>,
}

impl ParentFacts {
    /// Facts for a node with no parent: the root of the walk, or a supervisor a worker attached to the tree itself.
    fn root(node: &SupervisorNode) -> Self {
        Self {
            name: Arc::clone(&node.id),
            restart: RestartType::default(),
            significant: false,
            created: node.created,
            restarts: 0,
            exited: None,
        }
    }
}

/// What a node's own run contributes to its snapshot.
struct RunFacts {
    process_id: Option<ProcessId>,
    process_name: Option<Arc<str>>,
    started: Option<Stamp>,
    state: NodeState,
    /// The group this node's allocations are attributed to, which for a worker is its supervisor's rather than its
    /// own.
    resource_group: Option<Arc<str>>,
    resources: Option<ResourceUsage>,
}

/// Derives a node's lifecycle state.
///
/// A node its parent has seen exit is stopped whatever else is true; otherwise it is running exactly when it has a
/// process.
fn node_state(exited: Option<Stamp>, running: bool) -> NodeState {
    match (exited, running) {
        (Some(_), _) => NodeState::Exited,
        (None, true) => NodeState::Running,
        (None, false) => NodeState::Registered,
    }
}

/// State carried through a single snapshot walk.
struct Walk {
    groups: FastHashMap<Arc<str>, ResourceUsage>,
    totals: TreeTotals,
    /// Resource groups already folded into the totals.
    ///
    /// Group registration is idempotent by name, so two supervisors whose fully qualified names coincide share one
    /// group. Totalling per node would then count the same bytes twice.
    counted_groups: FastHashSet<Arc<str>>,
}

impl Walk {
    /// Builds the snapshot of a supervisor and everything beneath it.
    fn supervisor(&mut self, node: &Arc<SupervisorNode>, parent: ParentFacts, depth: usize) -> NodeSnapshot {
        // Copy out everything needed, then release the lock before descending: holding a parent's lock while taking a
        // child's is the only way this walk could ever deadlock against a running supervisor, and not doing it is
        // simpler to guarantee than any ordering rule.
        let (run, config, generation, restarts_performed, children) = {
            let state = node.state();

            let mut children = state
                .children
                .iter()
                .map(|(id, record)| {
                    let history = state.history.get(&record.key);
                    PendingChild {
                        key: record.key,
                        facts: ParentFacts {
                            name: Arc::clone(&record.name),
                            restart: record.restart,
                            significant: record.significant,
                            created: history
                                .map(|h| h.created)
                                .or_else(|| record.start.as_ref().map(|start| start.at))
                                .unwrap_or_else(Stamp::now),
                            restarts: history.map(|h| h.restarts).unwrap_or(0),
                            exited: history.and_then(|h| h.exited),
                        },
                        kind: match &record.kind {
                            RecordKind::Supervisor(node) => PendingKind::Supervisor(Arc::clone(node)),
                            RecordKind::Worker => match state.adopted.get(id) {
                                Some(adopted) => PendingKind::WorkerDriving(Arc::clone(adopted)),
                                None => PendingKind::Worker,
                            },
                        },
                        start: record.start.clone(),
                    }
                })
                .collect::<Vec<_>>();

            // A restart re-registers a child under a fresh id, so insertion order stops matching declaration order as
            // soon as anything restarts. `ChildKey` orders statics by declaration and dynamics by spawn, which keeps
            // successive snapshots diffable.
            children.sort_by_key(|child| child.key);

            (
                run_identity(&state),
                state.config,
                state.generation,
                state.restarts_performed,
                children,
            )
        };

        let (process_id, process_name, started) = split_run(run.as_ref());
        let state = node_state(parent.exited, process_id.is_some());
        let resources = process_name.as_ref().and_then(|name| self.groups.get(name).cloned());

        let mut snapshot = self.node_snapshot(
            NodeKind::Supervisor,
            parent,
            RunFacts {
                process_id,
                // A supervisor owns its resource group, and it is named by the process it actually runs as.
                resource_group: process_name.clone(),
                process_name,
                started,
                state,
                resources,
            },
        );

        snapshot.supervision = Some(SupervisionSettings {
            restart_mode: config.restart_strategy.mode(),
            restart_intensity: config.restart_strategy.intensity(),
            restart_period_ms: duration_millis(config.restart_strategy.period()),
            auto_shutdown: config.auto_shutdown,
            shutdown_budget_ms: config.shutdown_budget.map(duration_millis),
            dedicated_threads: config.dedicated_threads,
            restarts_performed,
            generation,
        });

        if children.is_empty() {
            return snapshot;
        }
        if !self.may_descend(&node.id, depth) {
            return snapshot;
        }

        // A worker inherits its supervisor's resource group rather than owning one, so this is what its children are
        // attributed to.
        let worker_group = snapshot.resource_group.as_deref().map(Arc::<str>::from);

        snapshot.children.reserve(children.len());
        for child in children {
            snapshot
                .children
                .push(self.child(child, worker_group.as_ref(), depth + 1));
        }

        snapshot
    }

    /// Builds the snapshot of one child of the supervisor currently being walked.
    fn child(&mut self, child: PendingChild, worker_group: Option<&Arc<str>>, depth: usize) -> NodeSnapshot {
        match child.kind {
            // A nested supervisor reports its own process and its own children; its parent only contributes how it
            // was configured and how it has fared.
            PendingKind::Supervisor(node) => self.supervisor(&node, child.facts, depth),

            // A worker that turned out to be driving a supervisor of its own. The worker is what the parent
            // supervises, so it stays the node; the supervisor it drives hangs beneath it.
            PendingKind::WorkerDriving(adopted) => {
                let mut snapshot = self.worker(child.facts, child.start, worker_group, depth);
                if self.may_descend(&adopted.id, depth) {
                    let nested = self.supervisor(&adopted, ParentFacts::root(&adopted), depth + 1);
                    snapshot.children.push(nested);
                }
                snapshot
            }

            PendingKind::Worker => self.worker(child.facts, child.start, worker_group, depth),
        }
    }

    /// Builds the snapshot of a leaf worker.
    fn worker(
        &mut self, facts: ParentFacts, start: Option<StartedChild>, worker_group: Option<&Arc<str>>, depth: usize,
    ) -> NodeSnapshot {
        self.totals.max_depth = self.totals.max_depth.max(depth);

        let (process_id, process_name, started) = split_run(start.as_ref());
        let state = node_state(facts.exited, process_id.is_some());

        self.node_snapshot(
            NodeKind::Worker,
            facts,
            RunFacts {
                process_id,
                process_name,
                started,
                state,
                resource_group: worker_group.cloned(),
                // A worker owns no resource group: its allocations are counted against the supervisor named above.
                // Reporting the supervisor's totals against each of its workers would count them many times over.
                resources: None,
            },
        )
    }

    /// Assembles a node from its two halves, and folds it into the tree totals.
    fn node_snapshot(&mut self, kind: NodeKind, parent: ParentFacts, run: RunFacts) -> NodeSnapshot {
        self.totals.max_depth = self.totals.max_depth.max(1);

        match kind {
            NodeKind::Supervisor => self.totals.supervisors += 1,
            NodeKind::Worker => self.totals.workers += 1,
        }
        match run.state {
            NodeState::Running => self.totals.running += 1,
            NodeState::Exited => self.totals.exited += 1,
            NodeState::Registered => self.totals.registered += 1,
        }
        self.totals.restarts += u64::from(parent.restarts);

        // Several nodes can share one resource group, so fold each group in once rather than once per node.
        if let (Some(usage), Some(group)) = (&run.resources, &run.process_name) {
            if self.counted_groups.insert(Arc::clone(group)) {
                self.totals.live_bytes += usage.live_bytes;
                self.totals.cpu_time_nanos += usage.cpu_time_nanos;
            }
        }

        NodeSnapshot {
            name: parent.name.to_string(),
            kind,
            process_name: run.process_name.as_deref().map(str::to_string),
            process_id: run.process_id.map(|id| id.as_usize() as u64),
            state: run.state,
            restart: parent.restart,
            significant: parent.significant,
            created_at: parent.created.wall_millis(),
            started_at: run.started.map(|started| started.wall_millis()),
            uptime_ms: match run.state {
                NodeState::Running => run.started.map(|started| started.elapsed_millis()),
                _ => None,
            },
            restart_count: parent.restarts,
            exited_at: parent.exited.map(|exited| exited.wall_millis()),
            resource_group: run.resource_group.as_deref().map(str::to_string),
            resources: run.resources,
            supervision: None,
            children: Vec::new(),
        }
    }

    /// Returns whether the walk may descend below `depth`, warning once if it may not.
    fn may_descend(&self, supervisor_id: &str, depth: usize) -> bool {
        if depth < MAX_TREE_DEPTH {
            return true;
        }

        warn!(
            supervisor_id,
            depth, "Supervision tree is deeper than the snapshot walk descends; subtree omitted."
        );
        false
    }
}

/// Reads the identity of a supervisor's current run, if it has one.
fn run_identity(state: &NodeInner) -> Option<StartedChild> {
    state.run.as_ref().map(|run| StartedChild {
        process_id: run.process_id,
        process_name: Arc::clone(&run.process_name),
        at: run.started,
    })
}

/// Splits a started process into the three optional fields a snapshot reports it as.
fn split_run(started: Option<&StartedChild>) -> (Option<ProcessId>, Option<Arc<str>>, Option<Stamp>) {
    match started {
        Some(start) => (
            Some(start.process_id),
            Some(Arc::clone(&start.process_name)),
            Some(start.at),
        ),
        None => (None, None, None),
    }
}

/// A child copied out from under its supervisor's lock, ready to be walked.
struct PendingChild {
    key: ChildKey,
    facts: ParentFacts,
    kind: PendingKind,
    start: Option<StartedChild>,
}

/// What a pending child turned out to be.
enum PendingKind {
    /// A leaf worker.
    Worker,
    /// A worker that drives a supervisor of its own, which hangs beneath it.
    WorkerDriving(Arc<SupervisorNode>),
    /// A nested supervisor.
    Supervisor(Arc<SupervisorNode>),
}

/// Converts a duration to whole milliseconds, saturating rather than overflowing.
///
/// Both a shutdown budget and a restart period come from configuration and can be arbitrarily large, up to and
/// including [`Duration::MAX`], which is used elsewhere to mean `no deadline`.
fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

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

    /// Identifier of the node's current process. `None` if the node has never run.
    ///
    /// A restart gives the node a new process, and so a new identifier.
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

    /// When the node's current process started. `None` if the node has never run.
    pub started_at: Option<UnixMillis>,

    /// How long the node's current process has been running, in milliseconds. `None` unless it is running.
    pub uptime_ms: Option<u64>,

    /// How many times the node has been restarted since it was created.
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

/// How a supervisor supervises its children, and how it has fared.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SupervisionSettings {
    /// Whether a failing child is restarted alone or together with its siblings.
    pub restart_mode: super::RestartMode,

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
