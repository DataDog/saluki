//! Supervision tree introspection.
//!
//! This module provides point-in-time visibility into a running supervision tree. Supervisors publish a
//! [`ProcessSnapshot`] for themselves and each of their children into the
//! [`DataspaceRegistry`](crate::runtime::state::DataspaceRegistry); consumers can reassemble those snapshots into a
//! [`SupervisionTree`] for inspection or rendering.
//!
//! # Capturing a tree
//!
//! Consumers reconstruct the tree from a flat collection of snapshots via [`SupervisionTree::from_snapshots`]. The
//! materialization of "all currently-asserted snapshots" is the responsibility of an in-process worker that subscribes
//! to the dataspace and maintains the current set; consumers either read directly from that worker's shared state or
//! receive snapshots via an out-of-process channel (such as an HTTP endpoint that the worker also exposes).
//!
//! # Rendering
//!
//! [`SupervisionTree`] exposes two text renderers:
//!
//! - [`render_tree`][SupervisionTree::render_tree]: a Unicode tree diagram that shows the parent/child structure.
//! - [`render_table`][SupervisionTree::render_table]: a `top`-style tabular listing of every process.

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use super::{
    process::Id as ProcessId,
    restart::{RestartMode, RestartStrategy},
    state::Identifier,
    ShutdownStrategy,
};

/// Identifier prefix used by the supervisor when asserting [`ProcessSnapshot`] values into the dataspace.
///
/// The full identifier is `process.<numeric process id>`. Consumers should subscribe with an
/// `IdentifierFilter::All` filter on `ProcessSnapshot` and rely on the snapshot payload itself for identity, rather
/// than parsing the identifier directly.
const SNAPSHOT_IDENTIFIER_PREFIX: &str = "process.";

/// Returns the identifier under which `process_id`'s snapshot is published.
pub(crate) fn snapshot_identifier(process_id: ProcessId) -> Identifier {
    Identifier::named(format!("{}{}", SNAPSHOT_IDENTIFIER_PREFIX, process_id.as_usize()))
}

/// Whether a process is a supervisor or a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    /// A supervisor process. Supervisors manage one or more child processes.
    Supervisor,

    /// A worker process. Workers perform application work and are managed by a supervisor.
    Worker,
}

/// Current lifecycle status of a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    /// The process is currently running.
    Running,

    /// The process has terminated and is in the process of being restarted by its supervisor.
    Restarting,

    /// The process has terminated and will not be restarted.
    Failed,
}

/// Serializable summary of a [`ShutdownStrategy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShutdownStrategyKind {
    /// Graceful shutdown with the given timeout (in milliseconds) before the process is forcefully aborted.
    Graceful {
        /// Maximum time, in milliseconds, the process is allowed to take to exit cleanly.
        timeout_ms: u64,
    },

    /// Immediate, forceful abort without waiting for clean shutdown.
    Brutal,
}

impl From<ShutdownStrategy> for ShutdownStrategyKind {
    fn from(strategy: ShutdownStrategy) -> Self {
        match strategy {
            ShutdownStrategy::Graceful(timeout) => Self::Graceful {
                timeout_ms: timeout_to_ms(timeout),
            },
            ShutdownStrategy::Brutal => Self::Brutal,
        }
    }
}

/// Restart mode for a supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartModeKind {
    /// Restart only the child that failed.
    OneForOne,

    /// Restart all children when any one fails.
    OneForAll,
}

impl From<RestartMode> for RestartModeKind {
    fn from(mode: RestartMode) -> Self {
        match mode {
            RestartMode::OneForOne => Self::OneForOne,
            RestartMode::OneForAll => Self::OneForAll,
        }
    }
}

/// Serializable summary of a [`RestartStrategy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartStrategyKind {
    /// Restart mode.
    pub mode: RestartModeKind,

    /// Maximum number of restarts allowed within `period_ms`.
    pub intensity: u64,

    /// Window over which restarts are counted, in milliseconds.
    pub period_ms: u64,
}

impl From<&RestartStrategy> for RestartStrategyKind {
    fn from(strategy: &RestartStrategy) -> Self {
        Self {
            mode: strategy.mode().into(),
            intensity: strategy.intensity() as u64,
            period_ms: timeout_to_ms(strategy.period()),
        }
    }
}

/// Runtime mode for a supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeModeKind {
    /// The supervisor runs on the ambient (parent) Tokio runtime.
    Ambient,

    /// The supervisor runs on a dedicated Tokio runtime with its own OS thread(s).
    Dedicated,
}

/// Point-in-time snapshot of a single process in the supervision tree.
///
/// Supervisors publish one snapshot per process (themselves and each of their children) into the dataspace whenever
/// the tree changes. Snapshots are serialized via `serde` and can be transported across process boundaries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    /// Numeric identifier of the process. Globally unique within the running program.
    pub id: u64,

    /// Numeric identifier of the parent process, if any. Root supervisors have no parent.
    pub parent_id: Option<u64>,

    /// Fully-scoped, dot-separated name (for example, `internal_sup.ctrl_pln.privileged_api`).
    pub name: String,

    /// Whether this process is a supervisor or a worker.
    pub kind: ProcessKind,

    /// Current lifecycle status.
    pub status: ProcessStatus,

    /// Wall-clock time the current incarnation of this process started.
    ///
    /// On restart, this is updated to the time of the most recent (re)spawn.
    #[serde(with = "system_time_millis")]
    pub started_at: SystemTime,

    /// Number of times this child has been restarted by its supervisor.
    ///
    /// Always zero for supervisors themselves (supervisors are restarted by *their* parent, which would account for
    /// the restart against that supervisor's child slot).
    pub restart_count: u64,

    /// Wall-clock time of the most recent restart, if any.
    #[serde(with = "system_time_millis_opt")]
    pub last_restarted_at: Option<SystemTime>,

    /// Human-readable description of the most recent failure, if any.
    pub last_failure: Option<String>,

    /// How this process is shut down by its supervisor.
    pub shutdown_strategy: ShutdownStrategyKind,

    /// Restart strategy (supervisors only).
    pub restart_strategy: Option<RestartStrategyKind>,

    /// Runtime mode (supervisors only).
    pub runtime_mode: Option<RuntimeModeKind>,

    /// Live memory usage of this process, in bytes.
    ///
    /// Sourced from the allocation group registered under this process's [`name`](Self::name) field. `None` when no
    /// matching allocation group is found, or when the tracking allocator is not installed. Populated by the in-process
    /// introspection consumer (such as `SupervisionIntrospectionWorker` in `saluki-app`) at materialization time;
    /// supervisors themselves leave this field at `None` when publishing.
    pub live_bytes: Option<u64>,
}

/// Reassembled supervision tree built from a collection of [`ProcessSnapshot`]s.
///
/// Provides parent/child lookups and renderers for human-readable output.
pub struct SupervisionTree {
    snapshots: HashMap<u64, ProcessSnapshot>,
    children_of: HashMap<u64, Vec<u64>>,
    roots: Vec<u64>,
}

impl SupervisionTree {
    /// Builds a tree from a flat collection of snapshots.
    ///
    /// Snapshots whose `parent_id` does not appear in the input are treated as roots. Children are sorted
    /// deterministically by `name` so that the rendered output is stable.
    pub fn from_snapshots<I>(snapshots: I) -> Self
    where
        I: IntoIterator<Item = ProcessSnapshot>,
    {
        let snapshots: HashMap<u64, ProcessSnapshot> = snapshots.into_iter().map(|s| (s.id, s)).collect();

        let mut children_of: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut roots: BTreeSet<u64> = BTreeSet::new();

        for snapshot in snapshots.values() {
            match snapshot.parent_id {
                Some(parent_id) if snapshots.contains_key(&parent_id) => {
                    children_of.entry(parent_id).or_default().push(snapshot.id);
                }
                _ => {
                    roots.insert(snapshot.id);
                }
            }
        }

        // Sort children of each parent by name to keep rendering deterministic.
        for children in children_of.values_mut() {
            children.sort_by(|a, b| {
                let a_name = snapshots.get(a).map(|s| s.name.as_str()).unwrap_or("");
                let b_name = snapshots.get(b).map(|s| s.name.as_str()).unwrap_or("");
                a_name.cmp(b_name).then(a.cmp(b))
            });
        }

        let mut roots: Vec<u64> = roots.into_iter().collect();
        roots.sort_by(|a, b| {
            let a_name = snapshots.get(a).map(|s| s.name.as_str()).unwrap_or("");
            let b_name = snapshots.get(b).map(|s| s.name.as_str()).unwrap_or("");
            a_name.cmp(b_name).then(a.cmp(b))
        });

        Self {
            snapshots,
            children_of,
            roots,
        }
    }

    /// Returns the number of processes in the tree.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// Returns `true` if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// Returns the snapshot for the given process identifier.
    pub fn get(&self, id: u64) -> Option<&ProcessSnapshot> {
        self.snapshots.get(&id)
    }

    /// Iterates over all snapshots in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = &ProcessSnapshot> {
        self.snapshots.values()
    }

    /// Iterates over the immediate children of `parent_id`.
    pub fn children_of(&self, parent_id: u64) -> impl Iterator<Item = &ProcessSnapshot> {
        self.children_of
            .get(&parent_id)
            .into_iter()
            .flat_map(move |children| children.iter().filter_map(|id| self.snapshots.get(id)))
    }

    /// Walks the tree in pre-order, yielding `(depth, snapshot)` pairs where `depth` is the distance from the nearest
    /// root.
    pub fn walk(&self) -> Vec<(usize, &ProcessSnapshot)> {
        let mut out = Vec::with_capacity(self.snapshots.len());
        for root in &self.roots {
            self.walk_from(*root, 0, &mut out);
        }
        out
    }

    fn walk_from<'a>(&'a self, id: u64, depth: usize, out: &mut Vec<(usize, &'a ProcessSnapshot)>) {
        let Some(snapshot) = self.snapshots.get(&id) else {
            return;
        };
        out.push((depth, snapshot));

        if let Some(children) = self.children_of.get(&id) {
            for child_id in children {
                self.walk_from(*child_id, depth + 1, out);
            }
        }
    }

    /// Renders the tree as a Unicode tree diagram into `w`.
    ///
    /// The rendered output is suitable for fixed-width display. Each line is one process and includes the process
    /// name, its kind, current status, restart count, and uptime relative to `now`.
    pub fn render_tree(&self, w: &mut dyn fmt::Write, now: SystemTime) -> fmt::Result {
        for (root_idx, root) in self.roots.iter().enumerate() {
            let is_last_root = root_idx == self.roots.len() - 1;
            self.render_tree_node(w, *root, "", is_last_root, true, now)?;
        }
        Ok(())
    }

    fn render_tree_node(
        &self, w: &mut dyn fmt::Write, id: u64, prefix: &str, is_last: bool, is_root: bool, now: SystemTime,
    ) -> fmt::Result {
        let Some(snapshot) = self.snapshots.get(&id) else {
            return Ok(());
        };

        // The branch glyph; roots get no leading glyph at all.
        let glyph = if is_root {
            ""
        } else if is_last {
            "└─ "
        } else {
            "├─ "
        };

        let uptime = format_uptime(now.duration_since(snapshot.started_at).unwrap_or_default());
        let kind = match snapshot.kind {
            ProcessKind::Supervisor => "sup",
            ProcessKind::Worker => "worker",
        };
        let status = match snapshot.status {
            ProcessStatus::Running => "running",
            ProcessStatus::Restarting => "restarting",
            ProcessStatus::Failed => "failed",
        };

        // Use the leaf-only name for tree display (the last segment of the dotted name) so deep trees stay readable.
        let leaf = snapshot.name.rsplit('.').next().unwrap_or(snapshot.name.as_str());

        let memory = match snapshot.live_bytes {
            Some(bytes) => format!(" mem={}", format_bytes(bytes)),
            None => String::new(),
        };

        writeln!(
            w,
            "{prefix}{glyph}{leaf} [{kind}, {status}, restarts={} uptime={}{memory}]",
            snapshot.restart_count, uptime
        )?;

        let children = self.children_of.get(&id).cloned().unwrap_or_default();
        let child_prefix = if is_root {
            String::new()
        } else {
            format!("{prefix}{}", if is_last { "   " } else { "│  " })
        };

        for (child_idx, child_id) in children.iter().enumerate() {
            let child_is_last = child_idx == children.len() - 1;
            self.render_tree_node(w, *child_id, &child_prefix, child_is_last, false, now)?;
        }

        Ok(())
    }

    /// Renders the tree as a `top`-style table into `w`.
    ///
    /// Columns: PID, parent PID, name (fully-qualified), kind, status, restart count, uptime, live memory.
    pub fn render_table(&self, w: &mut dyn fmt::Write, now: SystemTime) -> fmt::Result {
        const PID: &str = "PID";
        const PARENT: &str = "PARENT";
        const NAME: &str = "NAME";
        const KIND: &str = "KIND";
        const STATUS: &str = "STATUS";
        const RESTARTS: &str = "RESTARTS";
        const UPTIME: &str = "UPTIME";
        const MEMORY: &str = "MEMORY";

        let rows: Vec<TableRow<'_>> = self
            .walk()
            .into_iter()
            .map(|(_, snapshot)| TableRow {
                pid: snapshot.id.to_string(),
                parent: snapshot
                    .parent_id
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                name: snapshot.name.as_str(),
                kind: match snapshot.kind {
                    ProcessKind::Supervisor => "sup",
                    ProcessKind::Worker => "worker",
                },
                status: match snapshot.status {
                    ProcessStatus::Running => "running",
                    ProcessStatus::Restarting => "restarting",
                    ProcessStatus::Failed => "failed",
                },
                restarts: snapshot.restart_count.to_string(),
                uptime: format_uptime(now.duration_since(snapshot.started_at).unwrap_or_default()),
                memory: snapshot.live_bytes.map(format_bytes).unwrap_or_else(|| "-".to_string()),
            })
            .collect();

        let widths = TableWidths {
            pid: rows.iter().map(|r| r.pid.len()).max().unwrap_or(0).max(PID.len()),
            parent: rows.iter().map(|r| r.parent.len()).max().unwrap_or(0).max(PARENT.len()),
            name: rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max(NAME.len()),
            kind: rows.iter().map(|r| r.kind.len()).max().unwrap_or(0).max(KIND.len()),
            status: rows.iter().map(|r| r.status.len()).max().unwrap_or(0).max(STATUS.len()),
            restarts: rows
                .iter()
                .map(|r| r.restarts.len())
                .max()
                .unwrap_or(0)
                .max(RESTARTS.len()),
            uptime: rows.iter().map(|r| r.uptime.len()).max().unwrap_or(0).max(UPTIME.len()),
            memory: rows.iter().map(|r| r.memory.len()).max().unwrap_or(0).max(MEMORY.len()),
        };

        widths.write_row(w, PID, PARENT, NAME, KIND, STATUS, RESTARTS, UPTIME, MEMORY)?;
        for row in &rows {
            widths.write_row(
                w,
                &row.pid,
                &row.parent,
                row.name,
                row.kind,
                row.status,
                &row.restarts,
                &row.uptime,
                &row.memory,
            )?;
        }

        Ok(())
    }
}

struct TableRow<'a> {
    pid: String,
    parent: String,
    name: &'a str,
    kind: &'a str,
    status: &'a str,
    restarts: String,
    uptime: String,
    memory: String,
}

struct TableWidths {
    pid: usize,
    parent: usize,
    name: usize,
    kind: usize,
    status: usize,
    restarts: usize,
    uptime: usize,
    memory: usize,
}

impl TableWidths {
    #[allow(clippy::too_many_arguments)]
    fn write_row(
        &self, w: &mut dyn fmt::Write, pid: &str, parent: &str, name: &str, kind: &str, status: &str, restarts: &str,
        uptime: &str, memory: &str,
    ) -> fmt::Result {
        writeln!(
            w,
            "{:<pid$}  {:<parent$}  {:<name$}  {:<kind$}  {:<status$}  {:>restarts$}  {:>uptime$}  {:>memory$}",
            pid,
            parent,
            name,
            kind,
            status,
            restarts,
            uptime,
            memory,
            pid = self.pid,
            parent = self.parent,
            name = self.name,
            kind = self.kind,
            status = self.status,
            restarts = self.restarts,
            uptime = self.uptime,
            memory = self.memory,
        )
    }
}

fn timeout_to_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const TIB: u64 = 1 << 40;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_uptime(d: Duration) -> String {
    let total_secs = d.as_secs();
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if days > 0 {
        format!("{days}d{hours:02}h{minutes:02}m")
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

mod system_time_millis {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = time
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        millis.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + Duration::from_millis(millis))
    }
}

mod system_time_millis_opt {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(time: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = time.as_ref().map(|t| {
            t.duration_since(UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0)
        });
        millis.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<SystemTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let maybe_millis = Option::<u64>::deserialize(deserializer)?;
        Ok(maybe_millis.map(|m| UNIX_EPOCH + Duration::from_millis(m)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot(id: u64, parent: Option<u64>, name: &str, kind: ProcessKind) -> ProcessSnapshot {
        ProcessSnapshot {
            id,
            parent_id: parent,
            name: name.to_string(),
            kind,
            status: ProcessStatus::Running,
            started_at: SystemTime::UNIX_EPOCH,
            restart_count: 0,
            last_restarted_at: None,
            last_failure: None,
            shutdown_strategy: ShutdownStrategyKind::Graceful { timeout_ms: 5000 },
            restart_strategy: None,
            runtime_mode: None,
            live_bytes: None,
        }
    }

    #[test]
    fn from_snapshots_groups_by_parent_and_identifies_roots() {
        let snapshots = vec![
            sample_snapshot(1, None, "root", ProcessKind::Supervisor),
            sample_snapshot(2, Some(1), "root.child", ProcessKind::Worker),
            sample_snapshot(3, Some(1), "root.nested", ProcessKind::Supervisor),
            sample_snapshot(4, Some(3), "root.nested.leaf", ProcessKind::Worker),
            // Orphan: parent not present in input, should be treated as root.
            sample_snapshot(5, Some(99), "orphan", ProcessKind::Worker),
        ];

        let tree = SupervisionTree::from_snapshots(snapshots);
        assert_eq!(tree.len(), 5);
        assert!(tree.get(1).is_some());

        let root_children: Vec<u64> = tree.children_of(1).map(|s| s.id).collect();
        assert_eq!(root_children, vec![2, 3]);

        let nested_children: Vec<u64> = tree.children_of(3).map(|s| s.id).collect();
        assert_eq!(nested_children, vec![4]);

        // Both `1` and `5` should be roots since `5`'s parent is absent.
        let walked: Vec<u64> = tree.walk().into_iter().map(|(_, s)| s.id).collect();
        assert!(walked.contains(&1));
        assert!(walked.contains(&5));
    }

    #[test]
    fn walk_produces_preorder_with_depths() {
        let snapshots = vec![
            sample_snapshot(1, None, "root", ProcessKind::Supervisor),
            sample_snapshot(2, Some(1), "root.a", ProcessKind::Supervisor),
            sample_snapshot(3, Some(2), "root.a.x", ProcessKind::Worker),
            sample_snapshot(4, Some(1), "root.b", ProcessKind::Worker),
        ];

        let tree = SupervisionTree::from_snapshots(snapshots);
        let walked: Vec<(usize, u64)> = tree.walk().into_iter().map(|(d, s)| (d, s.id)).collect();
        // Expect: root(0), a(1), a.x(2), b(1).
        assert_eq!(walked, vec![(0, 1), (1, 2), (2, 3), (1, 4)]);
    }

    #[test]
    fn render_tree_writes_a_node_per_process() {
        let snapshots = vec![
            sample_snapshot(1, None, "root", ProcessKind::Supervisor),
            sample_snapshot(2, Some(1), "root.a", ProcessKind::Worker),
            sample_snapshot(3, Some(1), "root.b", ProcessKind::Worker),
        ];
        let tree = SupervisionTree::from_snapshots(snapshots);

        let mut out = String::new();
        tree.render_tree(&mut out, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("root "));
        assert!(out.contains("├─ a"));
        assert!(out.contains("└─ b"));
    }

    #[test]
    fn render_table_includes_header_and_one_row_per_process() {
        let snapshots = vec![
            sample_snapshot(1, None, "root", ProcessKind::Supervisor),
            sample_snapshot(2, Some(1), "root.a", ProcessKind::Worker),
        ];
        let tree = SupervisionTree::from_snapshots(snapshots);

        let mut out = String::new();
        tree.render_table(&mut out, SystemTime::UNIX_EPOCH).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("PID"));
        assert!(lines[0].contains("NAME"));
    }

    #[test]
    fn serde_roundtrip_preserves_payload() {
        let snapshot = ProcessSnapshot {
            id: 42,
            parent_id: Some(1),
            name: "root.worker".to_string(),
            kind: ProcessKind::Worker,
            status: ProcessStatus::Restarting,
            started_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_000_000),
            restart_count: 3,
            last_restarted_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(2_000_000)),
            last_failure: Some("kaboom".to_string()),
            shutdown_strategy: ShutdownStrategyKind::Graceful { timeout_ms: 5000 },
            restart_strategy: None,
            runtime_mode: None,
            live_bytes: Some(1_234_567),
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let back: ProcessSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot, back);
    }
}
